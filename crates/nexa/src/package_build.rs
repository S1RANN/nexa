//! Canonical package-build artifact and source-integrity boundary.
//!
//! Package discovery and semantic analysis live in `nexa-analysis`; bytecode generation lives in
//! `nexa-compiler`. This module is the only public orchestration boundary which is allowed to turn
//! their immutable outputs into a verified runtime artifact.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexa_analysis::{
    AnalysisOutcome, BuildFingerprint, BuildFingerprintInput, CandidateIdentity,
    FingerprintBuilder, LinkedStateFingerprint, LockFile, ModulePath, NormalizedPackagePath,
    PackageId, PackageKind, PackageManifest, PackageSourceSet, PublicApiFingerprint, QueryDatabase,
    QueryExecutionReport, QueryStats, ResolvedBuildInput, ResolvedDependencyGraph,
    ResolvedTestInput, SourceKey, SourceSetFingerprint, StateSchemaFingerprint, analyze_package,
    analyze_package_tests, source_set_fingerprint,
};
use nexa_compiler::{
    PackageDebugInfo, PackageStateTypeInfo, PackageTestCallGraphNode, PackageTestInfo,
    PackageVisibility,
};
use nexa_core::{CanonicalSymbolIdentity, FileId, SourceSpan, StableSymbolId, SymbolKind};
use nexa_diagnostics::{
    DiagnosticBatch, SourceIdentity, SourceSnapshot, SourceSnapshotRegistry,
    SourceSnapshotRegistryError,
};
use nexa_verifier::VerifiedModule;

pub const NEXA_LANGUAGE_VERSION: &str = nexa_analysis::NEXA_LANGUAGE_VERSION;
pub const NEXA_COMPILER_VERSION: &str = nexa_core::NEXA_COMPILER_VERSION;
pub const COMPILATION_OPTIONS_SCHEMA_VERSION: u32 =
    nexa_analysis::COMPILATION_OPTIONS_SCHEMA_VERSION;

/// Exact source snapshot for the Host contract used by package analysis and debug metadata.
///
/// The identity is deliberately standalone: a Host contract is an external build input, not a
/// source module owned by the Package or any static dependency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostContractSource {
    identity: SourceIdentity,
    text: Arc<str>,
}

impl HostContractSource {
    #[must_use]
    pub fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    #[must_use]
    pub fn text(&self) -> &Arc<str> {
        &self.text
    }
}

/// Parsed Host ABI paired with the exact source bytes and reader-facing source identity.
///
/// CLI and LSP callers should use [`HostContractInput::with_source`]. Generated Rust Hosts which
/// do not retain an original `.nidl` file can use [`HostContractInput::canonical`].
#[derive(Clone, Debug)]
pub struct HostContractInput<'a> {
    idl: &'a nexa_idl::Idl,
    source: HostContractSource,
    required_export_indices: Arc<[usize]>,
}

impl<'a> HostContractInput<'a> {
    #[must_use]
    pub fn canonical(idl: &'a nexa_idl::Idl) -> Self {
        Self {
            idl,
            source: HostContractSource {
                identity: SourceIdentity::standalone(
                    crate::package_environment::CANONICAL_HOST_SOURCE_PATH,
                ),
                text: Arc::from(nexa_idl::canonical_source(idl)),
            },
            required_export_indices: (0..idl.exports.len()).collect::<Vec<_>>().into(),
        }
    }

    pub fn with_source(
        idl: &'a nexa_idl::Idl,
        identity: SourceIdentity,
        text: impl Into<Arc<str>>,
    ) -> Result<Self, HostContractSourceError> {
        if identity.package_id().is_some() || identity.path().is_empty() {
            return Err(HostContractSourceError::InvalidIdentity(identity));
        }
        let text = text.into();
        let parsed = nexa_idl::parse(&text).map_err(HostContractSourceError::Parse)?;
        if parsed != *idl {
            return Err(HostContractSourceError::ParsedContractMismatch);
        }
        Ok(Self {
            idl,
            source: HostContractSource { identity, text },
            required_export_indices: (0..idl.exports.len()).collect::<Vec<_>>().into(),
        })
    }

    /// Selects the exact subset of Host-declared exports which this build must implement.
    ///
    /// The complete IDL, Host hash, functions, types, source identity, and source bytes remain
    /// unchanged. Names are canonicalized into declaration order so equivalent configuration
    /// lists produce one build identity.
    pub fn requiring_exports(&self, names: &[String]) -> Result<Self, HostContractSourceError> {
        let requested = names.iter().map(String::as_str).collect::<BTreeSet<_>>();
        if requested.len() != names.len() {
            let mut seen = BTreeSet::new();
            let duplicate = names
                .iter()
                .find(|name| !seen.insert(name.as_str()))
                .expect("a duplicate exists")
                .clone();
            return Err(HostContractSourceError::DuplicateRequiredExport(duplicate));
        }
        for name in names {
            if !self.idl.exports.iter().any(|export| export.name == *name) {
                return Err(HostContractSourceError::UnknownRequiredExport(name.clone()));
            }
        }
        let required_export_indices = self
            .idl
            .exports
            .iter()
            .enumerate()
            .filter_map(|(index, export)| requested.contains(export.name.as_str()).then_some(index))
            .collect::<Vec<_>>()
            .into();
        Ok(Self {
            idl: self.idl,
            source: self.source.clone(),
            required_export_indices,
        })
    }

    #[must_use]
    pub fn idl(&self) -> &nexa_idl::Idl {
        self.idl
    }

    #[must_use]
    pub fn source(&self) -> &HostContractSource {
        &self.source
    }

    pub(crate) fn required_exports(&self) -> impl ExactSizeIterator<Item = &nexa_idl::Export> + '_ {
        self.required_export_indices
            .iter()
            .map(|index| &self.idl.exports[*index])
    }

    /// Canonical identity of the effective required-export view.
    #[must_use]
    pub fn canonical_required_exports(&self) -> Vec<u8> {
        nexa_idl::canonical_required_exports(
            self.required_exports().map(|export| export.name.as_str()),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostContractSourceError {
    InvalidIdentity(SourceIdentity),
    Parse(nexa_idl::IdlError),
    ParsedContractMismatch,
    UnknownRequiredExport(String),
    DuplicateRequiredExport(String),
}

impl fmt::Display for HostContractSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(identity) => write!(
                formatter,
                "Host contract source identity must be a non-empty standalone path, found {identity}"
            ),
            Self::Parse(error) => write!(formatter, "invalid Host contract source: {error}"),
            Self::ParsedContractMismatch => formatter
                .write_str("Host contract source does not parse to the supplied canonical IDL"),
            Self::UnknownRequiredExport(name) => {
                write!(
                    formatter,
                    "required export `{name}` is not declared by the Host contract"
                )
            }
            Self::DuplicateRequiredExport(name) => {
                write!(formatter, "duplicate required export `{name}`")
            }
        }
    }
}

impl std::error::Error for HostContractSourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::InvalidIdentity(_)
            | Self::ParsedContractMismatch
            | Self::UnknownRequiredExport(_)
            | Self::DuplicateRequiredExport(_) => None,
        }
    }
}

/// Canonical bytes for the one M4 compiler configuration that the façade actually applies.
///
/// M4 does not expose per-caller optimization or language-feature flags. Making this byte
/// sequence internal to the façade prevents CLI and Engine from assigning different build
/// identities to the same artifact.
#[must_use]
pub fn canonical_compilation_options() -> Vec<u8> {
    nexa_analysis::canonical_compilation_options(&nexa_analysis::CompilationOptions::default())
}

/// Canonical, lossless identity of the Host source snapshot that is emitted into debug metadata.
///
/// The semantic ABI has its own canonical field. This record additionally frames the exact
/// standalone URI and raw UTF-8 bytes because both affect source registries and declaration spans.
#[must_use]
pub fn canonical_host_contract_source_identity(contract: &HostContractInput<'_>) -> Vec<u8> {
    let identity = contract.source().identity();
    let text = contract.source().text();
    let mut bytes = b"nexa.host-contract-source\0\x01\0\0\0".to_vec();
    bytes.extend_from_slice(
        &u64::try_from(identity.path().len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    bytes.extend_from_slice(identity.path().as_bytes());
    bytes.extend_from_slice(&u64::try_from(text.len()).unwrap_or(u64::MAX).to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Fingerprints the exact verified output and every façade authority that describes its linked
/// package state.
///
/// [`BuildFingerprint`] binds the complete validated input closure, including the lock graph,
/// Host ABI/source/required exports, compiler and runtime versions, standard-library descriptor,
/// and effective compiler options. The encoded v5 module independently binds the actual
/// code-generation/link result. Exact source and debug registries prevent an internally tampered
/// artifact from retaining a valid linked identity. The exact canonical Stateful surface binds
/// the host lookup registry as well as the runtime schema. The remaining fields bind the artifact
/// summaries and resolved dependency closure exposed to Runtime and Last-Known-Good inspection.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn linked_state_fingerprint(
    build_fingerprint: BuildFingerprint,
    module: &nexa_bytecode::Module,
    source_files: &PackageSourceSnapshot,
    debug_info: &PackageDebugInfo,
    state_surface: &[PackageStateTypeInfo],
    compilation_evidence: PackageCompilationEvidence,
    source_set_fingerprint: SourceSetFingerprint,
    public_api_fingerprint: PublicApiFingerprint,
    state_schema_fingerprint: StateSchemaFingerprint,
    dependency_closure: &ResolvedDependencyGraph,
    dependency_source_fingerprints: &BTreeMap<PackageId, SourceSetFingerprint>,
) -> LinkedStateFingerprint {
    const LINKED_STATE_SCHEMA: u16 = 4;
    let mut builder = FingerprintBuilder::new(LinkedStateFingerprint::DOMAIN, LINKED_STATE_SCHEMA);
    builder.field_bytes("build", build_fingerprint.as_bytes());
    builder.field_bytes("module-v5", &module.encode());
    fingerprint_source_snapshot(&mut builder, source_files);
    fingerprint_debug_info(&mut builder, debug_info);
    fingerprint_state_surface(&mut builder, state_surface);
    builder.field_u64(
        "compiled-module-count",
        u64::try_from(compilation_evidence.modules).unwrap_or(u64::MAX),
    );
    builder.field_u64(
        "compiled-symbol-count",
        u64::try_from(compilation_evidence.symbols).unwrap_or(u64::MAX),
    );
    builder.field_u64(
        "compiled-package-module-count",
        u64::try_from(compilation_evidence.package_modules).unwrap_or(u64::MAX),
    );
    builder.field_u64(
        "compiled-package-symbol-count",
        u64::try_from(compilation_evidence.package_symbols).unwrap_or(u64::MAX),
    );
    builder.field_u64(
        "resolved-import-edge-count",
        u64::try_from(compilation_evidence.import_edges).unwrap_or(u64::MAX),
    );
    builder.field_u64(
        "linked-package-count",
        u64::try_from(compilation_evidence.packages).unwrap_or(u64::MAX),
    );
    builder.field_bytes("source-set", source_set_fingerprint.as_bytes());
    builder.field_bytes("public-api", public_api_fingerprint.as_bytes());
    builder.field_bytes("state-schema", state_schema_fingerprint.as_bytes());
    builder.field_bytes(
        "dependency-closure",
        &dependency_closure.canonical_identity_bytes(),
    );
    builder.field_u64(
        "dependency-source-count",
        u64::try_from(dependency_source_fingerprints.len()).unwrap_or(u64::MAX),
    );
    for (package, fingerprint) in dependency_source_fingerprints {
        builder.field_str("dependency-package", package.as_str());
        builder.field_bytes("dependency-source", fingerprint.as_bytes());
    }
    LinkedStateFingerprint::from_bytes(builder.finish_bytes())
}

fn fingerprint_source_snapshot(
    builder: &mut FingerprintBuilder,
    source_files: &PackageSourceSnapshot,
) {
    builder.field_u64(
        "source-file-count",
        u64::try_from(source_files.files().len()).unwrap_or(u64::MAX),
    );
    for source in source_files.files() {
        builder.field_u32("source-file-id", source.file.0);
        builder.field_u8(
            "source-identity-package-present",
            u8::from(source.identity.package_id().is_some()),
        );
        builder.field_str(
            "source-identity-package",
            source.identity.package_id().unwrap_or_default(),
        );
        builder.field_str("source-identity-path", source.identity.path());
        builder.field_u8("source-key-present", u8::from(source.key.is_some()));
        builder.field_str(
            "source-key-package",
            source
                .key
                .as_ref()
                .map_or("", |key| key.package_id.as_str()),
        );
        builder.field_str(
            "source-key-path",
            source.key.as_ref().map_or("", |key| key.path.as_str()),
        );
        builder.field_u8(
            "source-module-present",
            u8::from(source.module_path.is_some()),
        );
        builder.field_str(
            "source-module",
            source.module_path.as_deref().unwrap_or_default(),
        );
        builder.field_u8(
            "source-compiler-provided",
            u8::from(source.compiler_provided),
        );
        builder.field_bytes("source-text", source.text.as_bytes());
    }
}

fn fingerprint_debug_info(builder: &mut FingerprintBuilder, debug_info: &PackageDebugInfo) {
    builder.field_str("debug-root-package", &debug_info.root_package_id);
    builder.field_str("debug-entry-module", &debug_info.entry_module);
    builder.field_u64(
        "debug-module-count",
        u64::try_from(debug_info.modules.len()).unwrap_or(u64::MAX),
    );
    for module in &debug_info.modules {
        builder.field_str("debug-module-package", &module.package_id);
        builder.field_str("debug-module-path", &module.module_path);
        builder.field_u32("debug-module-file", module.file.0);
        fingerprint_source_span(builder, "debug-module-definition", module.definition_span);
        fingerprint_source_span(builder, "debug-module-source", module.source_span);
        builder.field_u64(
            "debug-module-function-count",
            u64::try_from(module.function_indices.len()).unwrap_or(u64::MAX),
        );
        for function in &module.function_indices {
            builder.field_u32("debug-module-function", *function);
        }
    }
    builder.field_u64(
        "debug-function-count",
        u64::try_from(debug_info.functions.len()).unwrap_or(u64::MAX),
    );
    for function in &debug_info.functions {
        builder.field_u32("debug-function-index", function.function_index);
        builder.field_str("debug-function-package", &function.package_id);
        builder.field_str("debug-function-module", &function.module_path);
        builder.field_str("debug-function-name", &function.name);
        fingerprint_canonical_symbol(
            builder,
            "debug-function-identity",
            &function.canonical_identity,
        );
        builder.field_u64("debug-function-stable-id", function.stable_id.0.0);
        fingerprint_source_span(
            builder,
            "debug-function-definition",
            function.definition_span,
        );
        builder.field_u8(
            "debug-function-effect",
            match function.effect {
                nexa_bytecode::FunctionEffect::Ordinary => 0,
                nexa_bytecode::FunctionEffect::Task => 1,
                nexa_bytecode::FunctionEffect::Immediate => 2,
                nexa_bytecode::FunctionEffect::Migration => 3,
                nexa_bytecode::FunctionEffect::Cleanup => 4,
            },
        );
        builder.field_u8(
            "debug-function-visibility",
            match function.visibility {
                PackageVisibility::Private => 0,
                PackageVisibility::Package => 1,
                PackageVisibility::Public => 2,
            },
        );
    }
    builder.field_u64(
        "debug-host-import-count",
        u64::try_from(debug_info.host_imports.len()).unwrap_or(u64::MAX),
    );
    for host in &debug_info.host_imports {
        builder.field_u32("debug-host-import-index", host.import_index);
        builder.field_u64("debug-host-stable-id", host.stable_id.0);
        builder.field_u64("debug-host-interface-id", host.interface_id.0);
        builder.field_str("debug-host-interface-name", &host.interface_name);
        builder.field_str("debug-host-function-name", &host.function_name);
        fingerprint_source_span(builder, "debug-host-interface", host.interface_span);
        fingerprint_source_span(builder, "debug-host-declaration", host.declaration_span);
    }
}

fn fingerprint_source_span(builder: &mut FingerprintBuilder, name: &str, span: SourceSpan) {
    builder.field_u32(&format!("{name}-file"), span.file.0);
    builder.field_u32(&format!("{name}-start"), span.start);
    builder.field_u32(&format!("{name}-end"), span.end);
}

fn fingerprint_canonical_symbol(
    builder: &mut FingerprintBuilder,
    name: &str,
    identity: &CanonicalSymbolIdentity,
) {
    builder.field_str(&format!("{name}-package"), identity.package_id());
    builder.field_str(&format!("{name}-module"), identity.module_path());
    builder.field_u8(&format!("{name}-kind"), identity.kind() as u8);
    builder.field_str(&format!("{name}-name"), identity.name());
    builder.field_u8(
        &format!("{name}-stable-present"),
        u8::from(identity.explicit_stable_name().is_some()),
    );
    builder.field_str(
        &format!("{name}-stable"),
        identity.explicit_stable_name().unwrap_or_default(),
    );
}

fn fingerprint_state_surface(
    builder: &mut FingerprintBuilder,
    state_surface: &[PackageStateTypeInfo],
) {
    let mut states = state_surface.iter().collect::<Vec<_>>();
    states.sort_by(|left, right| {
        left.canonical_identity
            .cmp(&right.canonical_identity)
            .then_with(|| left.package_id.cmp(&right.package_id))
            .then_with(|| left.module_path.cmp(&right.module_path))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });
    builder.field_u64(
        "state-surface-count",
        u64::try_from(states.len()).unwrap_or(u64::MAX),
    );
    for state in states {
        builder.field_str("state-surface-package", &state.package_id);
        builder.field_str("state-surface-module", &state.module_path);
        builder.field_str("state-surface-name", &state.name);
        builder.field_u32("state-surface-version", state.version);
        fingerprint_canonical_symbol(builder, "state-surface-identity", &state.canonical_identity);
        builder.field_u64("state-surface-stable-id", state.stable_id.0.0);
        fingerprint_source_span(builder, "state-surface-definition", state.definition_span);

        let mut fields = state.fields.iter().collect::<Vec<_>>();
        fields.sort_by(|left, right| {
            left.canonical_identity
                .cmp(&right.canonical_identity)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.stable_id.cmp(&right.stable_id))
        });
        builder.field_u64(
            "state-surface-field-count",
            u64::try_from(fields.len()).unwrap_or(u64::MAX),
        );
        for field in fields {
            builder.field_str("state-surface-field-name", &field.name);
            fingerprint_canonical_symbol(
                builder,
                "state-surface-field-identity",
                &field.canonical_identity,
            );
            builder.field_u64("state-surface-field-stable-id", field.stable_id.0.0);
            fingerprint_source_span(
                builder,
                "state-surface-field-definition",
                field.definition_span,
            );
        }
    }
}

/// Successful semantic check of an application or library root.
#[derive(Clone, Debug)]
pub struct PackageCheckReport {
    pub diagnostics: DiagnosticBatch,
    pub source_set_fingerprint: SourceSetFingerprint,
    pub public_api_fingerprint: PublicApiFingerprint,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub analysis_revision: u64,
    /// Queries that were parsed, analyzed, reused, or invalidated during this exact check.
    pub query_report: QueryExecutionReport,
    /// Cumulative counters for the persistent session after this check completed.
    pub query_stats: QueryStats,
    pub modules: usize,
    pub symbols: usize,
    pub resolved_references: usize,
    pub resolved_module_imports: usize,
    pub resolved_dependency_imports: usize,
    /// Exact semantic/closure cardinalities from this analyzer result.
    pub compilation_evidence: PackageCompilationEvidence,
}

/// Exact cardinalities observed by canonical analysis and retained by the compiled artifact.
///
/// `packages` excludes compiler-provided standard-library modules and is cross-checked against
/// the package identities actually retained in the artifact source/debug registries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackageCompilationEvidence {
    pub modules: usize,
    pub symbols: usize,
    pub package_modules: usize,
    pub package_symbols: usize,
    pub import_edges: usize,
    pub packages: usize,
}

/// Cumulative production-side invocation counters for one canonical build session.
///
/// These counters are incremented at the actual call sites, not inferred by tests from a returned
/// artifact. `checked_delta` lets stress evidence report the exact work performed by one
/// observation while retaining a persistent query database.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackagePipelineStats {
    pub analyzer_runs: u64,
    pub invalid_check_analyzer_runs: u64,
    pub successful_check_analyzer_runs: u64,
    pub compile_analyzer_runs: u64,
    pub typed_compiler_runs: u64,
    pub verifier_runs: u64,
}

impl PackagePipelineStats {
    #[must_use]
    pub fn checked_delta(self, earlier: Self) -> Option<Self> {
        Some(Self {
            analyzer_runs: self.analyzer_runs.checked_sub(earlier.analyzer_runs)?,
            invalid_check_analyzer_runs: self
                .invalid_check_analyzer_runs
                .checked_sub(earlier.invalid_check_analyzer_runs)?,
            successful_check_analyzer_runs: self
                .successful_check_analyzer_runs
                .checked_sub(earlier.successful_check_analyzer_runs)?,
            compile_analyzer_runs: self
                .compile_analyzer_runs
                .checked_sub(earlier.compile_analyzer_runs)?,
            typed_compiler_runs: self
                .typed_compiler_runs
                .checked_sub(earlier.typed_compiler_runs)?,
            verifier_runs: self.verifier_runs.checked_sub(earlier.verifier_runs)?,
        })
    }

    fn increment(value: &mut u64) {
        *value = value.checked_add(1).expect("pipeline run count fits u64");
    }

    fn start_check_analysis(&mut self) {
        Self::increment(&mut self.analyzer_runs);
    }

    fn finish_check_analysis(&mut self, valid: bool) {
        if valid {
            Self::increment(&mut self.successful_check_analyzer_runs);
        } else {
            Self::increment(&mut self.invalid_check_analyzer_runs);
        }
    }

    fn start_compile_analysis(&mut self) {
        Self::increment(&mut self.analyzer_runs);
        Self::increment(&mut self.compile_analyzer_runs);
    }

    fn start_typed_compiler(&mut self) {
        Self::increment(&mut self.typed_compiler_runs);
    }

    fn start_verifier(&mut self) {
        Self::increment(&mut self.verifier_runs);
    }
}

/// Wall-clock phase durations observed by the canonical package-build façade.
///
/// `compile_duration` covers validation, semantic analysis, typed code generation, artifact
/// assembly, and integrity checks. `verify_duration` covers exactly the one bytecode-verifier
/// invocation performed by the façade. The two durations therefore partition the observed build
/// without re-running verification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackageBuildDurations {
    pub compile_duration: Duration,
    pub verify_duration: Duration,
}

impl PackageBuildDurations {
    #[must_use]
    pub fn total_duration(self) -> Duration {
        self.compile_duration.saturating_add(self.verify_duration)
    }
}

/// Result and phase timings from one canonical Application build.
///
/// Timings are retained on failure so Engine and CLI diagnostics never need to infer a verifier
/// duration or perform a second verification pass.
#[derive(Debug)]
pub struct PackageBuildObservation {
    pub result: Result<CompiledPackageArtifact, PackageBuildError>,
    pub durations: PackageBuildDurations,
    /// Exact façade stages invoked during this observation.
    pub pipeline: PackagePipelineStats,
}

/// Persistent canonical package-build session.
///
/// Reusing this object is the only supported way to retain incremental query state. Free
/// `compile_*` helpers below construct a cold session for one-shot callers.
#[derive(Debug, Default)]
pub struct PackageBuildSession {
    queries: QueryDatabase,
    pipeline: PackagePipelineStats,
}

impl PackageBuildSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queries: QueryDatabase::new(),
            pipeline: PackagePipelineStats::default(),
        }
    }

    /// Cumulative query-cache counters for this persistent session.
    #[must_use]
    pub const fn query_stats(&self) -> QueryStats {
        self.queries.stats()
    }

    /// Cumulative canonical pipeline invocations for this persistent session.
    #[must_use]
    pub const fn pipeline_stats(&self) -> PackagePipelineStats {
        self.pipeline
    }

    pub fn check_package(
        &mut self,
        input: &ResolvedBuildInput,
        interface: &nexa_idl::Idl,
    ) -> Result<PackageCheckReport, PackageBuildError> {
        self.check_package_with_contract(input, &HostContractInput::canonical(interface))
    }

    pub fn check_package_with_contract(
        &mut self,
        input: &ResolvedBuildInput,
        contract: &HostContractInput<'_>,
    ) -> Result<PackageCheckReport, PackageBuildError> {
        input
            .validate_integrity()
            .map_err(PackageBuildError::InvalidResolvedInput)?;
        validate_host_contract(input, contract)?;
        let environment = crate::package_environment::canonical_analysis_environment(contract)?;
        self.pipeline.start_check_analysis();
        let mut outcome = analyze_package(input, &environment, &mut self.queries);
        let Some(ir) = outcome.ir.take() else {
            self.pipeline.finish_check_analysis(false);
            return Err(PackageBuildError::AnalysisFailed(
                outcome.diagnostics.clone(),
            ));
        };
        self.pipeline.finish_check_analysis(true);
        let compilation_evidence =
            package_compilation_evidence(&ir, &outcome.resolved_import_edges);
        let resolved_module_imports = outcome
            .resolved_import_edges
            .iter()
            .filter(|edge| {
                matches!(
                    &edge.target,
                    nexa_analysis::ResolvedImportTarget::Module(target)
                        if target.package_id == edge.importer.package_id
                )
            })
            .count();
        let resolved_dependency_imports = outcome
            .resolved_import_edges
            .iter()
            .filter(|edge| {
                matches!(
                    &edge.target,
                    nexa_analysis::ResolvedImportTarget::Module(target)
                        if target.package_id != edge.importer.package_id
                )
            })
            .count();
        Ok(PackageCheckReport {
            diagnostics: outcome.diagnostics,
            source_set_fingerprint: outcome.source_set_fingerprint,
            public_api_fingerprint: outcome.public_api_fingerprint,
            state_schema_fingerprint: outcome.state_schema_fingerprint,
            analysis_revision: outcome.analyzed_revision,
            query_report: outcome.query_report,
            query_stats: self.queries.stats(),
            modules: ir.modules().len(),
            symbols: ir.definitions().len(),
            resolved_references: ir
                .modules()
                .iter()
                .map(|module| module.resolved_references.len())
                .sum(),
            resolved_module_imports,
            resolved_dependency_imports,
            compilation_evidence,
        })
    }

    pub fn compile_package(
        &mut self,
        input: &ResolvedBuildInput,
        interface: &nexa_idl::Idl,
        identity: CandidateIdentity,
    ) -> Result<CompiledPackageArtifact, PackageBuildError> {
        self.compile_package_with_limits(
            input,
            interface,
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    pub fn compile_package_with_limits(
        &mut self,
        input: &ResolvedBuildInput,
        interface: &nexa_idl::Idl,
        identity: CandidateIdentity,
        verifier_limits: nexa_verifier::VerifierLimits,
    ) -> Result<CompiledPackageArtifact, PackageBuildError> {
        self.compile_package_with_contract_and_limits(
            input,
            &HostContractInput::canonical(interface),
            identity,
            verifier_limits,
        )
    }

    /// Builds one Application and reports the real compile/verifier phase split.
    pub fn compile_package_observed(
        &mut self,
        input: &ResolvedBuildInput,
        interface: &nexa_idl::Idl,
        identity: CandidateIdentity,
    ) -> PackageBuildObservation {
        self.compile_package_with_contract_and_limits_observed(
            input,
            &HostContractInput::canonical(interface),
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    pub fn compile_package_with_contract(
        &mut self,
        input: &ResolvedBuildInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
    ) -> Result<CompiledPackageArtifact, PackageBuildError> {
        self.compile_package_with_contract_and_limits(
            input,
            contract,
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    pub fn compile_package_with_contract_and_limits(
        &mut self,
        input: &ResolvedBuildInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
        verifier_limits: nexa_verifier::VerifierLimits,
    ) -> Result<CompiledPackageArtifact, PackageBuildError> {
        self.compile_package_with_contract_and_limits_observed(
            input,
            contract,
            identity,
            verifier_limits,
        )
        .result
    }

    /// Builds one Application with an exact Host source and reports the real phase split.
    pub fn compile_package_with_contract_observed(
        &mut self,
        input: &ResolvedBuildInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
    ) -> PackageBuildObservation {
        self.compile_package_with_contract_and_limits_observed(
            input,
            contract,
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    fn compile_package_with_contract_and_limits_observed(
        &mut self,
        input: &ResolvedBuildInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
        verifier_limits: nexa_verifier::VerifierLimits,
    ) -> PackageBuildObservation {
        let build_started = Instant::now();
        let pipeline_before = self.pipeline;
        let mut verify_duration = Duration::ZERO;
        let result = (|| {
            input
                .validate_integrity()
                .map_err(PackageBuildError::InvalidResolvedInput)?;
            validate_candidate(input, &identity)?;
            validate_host_contract(input, contract)?;
            if input.root_manifest.kind != PackageKind::Application {
                return Err(PackageBuildError::ApplicationArtifactRequired(
                    input.root_manifest.id.clone(),
                ));
            }
            let environment = crate::package_environment::canonical_analysis_environment(contract)?;
            self.pipeline.start_compile_analysis();
            let mut outcome = analyze_package(input, &environment, &mut self.queries);
            let ir = outcome
                .ir
                .take()
                .ok_or_else(|| PackageBuildError::AnalysisFailed(outcome.diagnostics.clone()))?;
            let compilation_evidence =
                package_compilation_evidence(&ir, &outcome.resolved_import_edges);
            self.pipeline.start_typed_compiler();
            let compiled = nexa_compiler::compile_typed_package(&ir)?;
            validate_product_compiler_output(&compiled, &outcome)?;
            let verify_started = Instant::now();
            self.pipeline.start_verifier();
            let verification = nexa_verifier::verify(compiled.module, verifier_limits);
            verify_duration = verify_started.elapsed();
            let verified = verification?;
            validate_host_exports(verified.module(), contract)?;
            let source_files = PackageSourceSnapshot::from_compiler_sources(compiled.sources)?;
            validate_compiled_closure(
                input,
                &source_files,
                &compiled.debug_info,
                compilation_evidence,
            )?;
            let state_surface = Arc::from(compiled.state_surface);
            let dependency_source_fingerprints = input
                .dependency_source_sets
                .iter()
                .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
                .collect::<BTreeMap<_, _>>();
            let linked_state_fingerprint = linked_state_fingerprint(
                input.build_fingerprint,
                verified.module(),
                &source_files,
                &compiled.debug_info,
                &state_surface,
                compilation_evidence,
                outcome.source_set_fingerprint,
                outcome.public_api_fingerprint,
                outcome.state_schema_fingerprint,
                &input.dependency_graph,
                &dependency_source_fingerprints,
            );
            let artifact = CompiledPackageArtifact {
                identity,
                verified,
                source_files,
                debug_info: compiled.debug_info,
                state_surface,
                compilation_evidence,
                source_set_fingerprint: outcome.source_set_fingerprint,
                public_api_fingerprint: outcome.public_api_fingerprint,
                state_schema_fingerprint: outcome.state_schema_fingerprint,
                build_fingerprint: input.build_fingerprint,
                linked_state_fingerprint,
                dependency_closure: Arc::clone(&input.dependency_graph),
                dependency_source_fingerprints: Arc::new(dependency_source_fingerprints),
                analysis_revision: outcome.analyzed_revision,
            };
            artifact.verify_integrity()?;
            Ok(artifact)
        })();
        let total_duration = build_started.elapsed();
        PackageBuildObservation {
            result,
            durations: PackageBuildDurations {
                compile_duration: total_duration.saturating_sub(verify_duration),
                verify_duration,
            },
            pipeline: self
                .pipeline
                .checked_delta(pipeline_before)
                .expect("session pipeline counters are monotonic"),
        }
    }

    pub fn compile_package_tests(
        &mut self,
        input: &ResolvedTestInput,
        interface: &nexa_idl::Idl,
        identity: CandidateIdentity,
    ) -> Result<CompiledPackageTests, PackageBuildError> {
        self.compile_package_tests_with_limits(
            input,
            interface,
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    pub fn compile_package_tests_with_limits(
        &mut self,
        input: &ResolvedTestInput,
        interface: &nexa_idl::Idl,
        identity: CandidateIdentity,
        verifier_limits: nexa_verifier::VerifierLimits,
    ) -> Result<CompiledPackageTests, PackageBuildError> {
        self.compile_package_tests_with_contract_and_limits(
            input,
            &HostContractInput::canonical(interface),
            identity,
            verifier_limits,
        )
    }

    pub fn compile_package_tests_with_contract(
        &mut self,
        input: &ResolvedTestInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
    ) -> Result<CompiledPackageTests, PackageBuildError> {
        self.compile_package_tests_with_contract_and_limits(
            input,
            contract,
            identity,
            nexa_verifier::VerifierLimits::default(),
        )
    }

    pub fn compile_package_tests_with_contract_and_limits(
        &mut self,
        input: &ResolvedTestInput,
        contract: &HostContractInput<'_>,
        identity: CandidateIdentity,
        verifier_limits: nexa_verifier::VerifierLimits,
    ) -> Result<CompiledPackageTests, PackageBuildError> {
        input
            .validate_integrity()
            .map_err(PackageBuildError::InvalidResolvedInput)?;
        validate_candidate(&input.product, &identity)?;
        validate_host_contract(&input.product, contract)?;
        let environment = crate::package_environment::canonical_analysis_environment(contract)?;
        self.pipeline.start_compile_analysis();
        let mut outcome = analyze_package_tests(input, &environment, &mut self.queries);
        let ir = outcome
            .ir
            .take()
            .ok_or_else(|| PackageBuildError::AnalysisFailed(outcome.diagnostics.clone()))?;
        self.pipeline.start_typed_compiler();
        let compiled = nexa_compiler::compile_typed_package(&ir)?;
        validate_test_compiler_output(&compiled, &outcome)?;
        let module = compiled
            .test_module
            .ok_or(PackageBuildError::MissingTestModule)?;
        let debug_info = compiled
            .test_debug_info
            .ok_or(PackageBuildError::MissingTestDebugInfo)?;
        self.pipeline.start_verifier();
        let verified = nexa_verifier::verify(module, verifier_limits)?;
        if verified.module().host_interface_hash != Some(nexa_idl::exact_hash(contract.idl())) {
            return Err(PackageBuildError::HostInterfaceHashMismatch);
        }
        let source_files = PackageSourceSnapshot::from_compiler_sources(compiled.test_sources)?;
        CompiledPackageTests::new(
            identity.package_id,
            verified,
            source_files,
            debug_info,
            compiled.tests,
            compiled.test_call_graph,
        )
    }
}

/// One-shot cold semantic check for an application or library package.
pub fn check_package(
    input: &ResolvedBuildInput,
    interface: &nexa_idl::Idl,
) -> Result<PackageCheckReport, PackageBuildError> {
    PackageBuildSession::new().check_package(input, interface)
}

/// One-shot cold semantic check which retains the caller's exact `.nidl` source snapshot.
pub fn check_package_with_contract(
    input: &ResolvedBuildInput,
    contract: &HostContractInput<'_>,
) -> Result<PackageCheckReport, PackageBuildError> {
    PackageBuildSession::new().check_package_with_contract(input, contract)
}

/// One-shot canonical Application build.
pub fn compile_package(
    input: &ResolvedBuildInput,
    interface: &nexa_idl::Idl,
    identity: CandidateIdentity,
) -> Result<CompiledPackageArtifact, PackageBuildError> {
    PackageBuildSession::new().compile_package(input, interface, identity)
}

/// One-shot canonical Application build with exact Host source/debug identity.
pub fn compile_package_with_contract(
    input: &ResolvedBuildInput,
    contract: &HostContractInput<'_>,
    identity: CandidateIdentity,
) -> Result<CompiledPackageArtifact, PackageBuildError> {
    PackageBuildSession::new().compile_package_with_contract(input, contract, identity)
}

/// One-shot canonical pure package-test build for an Application or Library root.
pub fn compile_package_tests(
    input: &ResolvedTestInput,
    interface: &nexa_idl::Idl,
    identity: CandidateIdentity,
) -> Result<CompiledPackageTests, PackageBuildError> {
    PackageBuildSession::new().compile_package_tests(input, interface, identity)
}

/// One-shot pure Package Test build with exact Host source/debug identity.
pub fn compile_package_tests_with_contract(
    input: &ResolvedTestInput,
    contract: &HostContractInput<'_>,
    identity: CandidateIdentity,
) -> Result<CompiledPackageTests, PackageBuildError> {
    PackageBuildSession::new().compile_package_tests_with_contract(input, contract, identity)
}

/// Constructs the complete versioned identity input used by
/// [`nexa_analysis::ResolvedBuildInput`].
///
/// The standard-library descriptor carries both its canonical bytes and independently computed
/// descriptor hash. Human version strings are never treated as sufficient content identity.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn canonical_package_build_fingerprint_input(
    root_manifest: &PackageManifest,
    root_source_set: &PackageSourceSet,
    dependency_manifests: &BTreeMap<PackageId, Arc<PackageManifest>>,
    dependency_source_sets: &BTreeMap<PackageId, Arc<PackageSourceSet>>,
    host_contract: &nexa_idl::Idl,
    lock: Option<&LockFile>,
) -> BuildFingerprintInput {
    let contract = HostContractInput::canonical(host_contract);
    canonical_package_build_fingerprint_input_with_contract(
        root_manifest,
        root_source_set,
        dependency_manifests,
        dependency_source_sets,
        &contract,
        lock,
    )
}

/// Exact-source variant used by CLI, LSP, and any caller retaining a real `.nidl` snapshot.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn canonical_package_build_fingerprint_input_with_contract(
    root_manifest: &PackageManifest,
    root_source_set: &PackageSourceSet,
    dependency_manifests: &BTreeMap<PackageId, Arc<PackageManifest>>,
    dependency_source_sets: &BTreeMap<PackageId, Arc<PackageSourceSet>>,
    host_contract: &HostContractInput<'_>,
    lock: Option<&LockFile>,
) -> BuildFingerprintInput {
    let standard_library = nexa_stdlib::standard_library();

    BuildFingerprintInput {
        root_package: root_manifest.id.clone(),
        root_manifest: root_manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(root_source_set),
        dependency_manifests: dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect(),
        dependency_source_sets: dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect(),
        host_contract: nexa_idl::canonical(host_contract.idl()).into_bytes(),
        host_contract_source: canonical_host_contract_source_identity(host_contract),
        host_required_exports: host_contract.canonical_required_exports(),
        language_version: NEXA_LANGUAGE_VERSION.into(),
        standard_library_version: standard_library.version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: canonical_compilation_options(),
        canonical_lock_graph: lock.map_or_else(Vec::new, LockFile::canonical_bytes),
    }
}

/// One source captured by a compiler output.
///
/// `compiler_provided` distinguishes versioned standard-library sources from package and
/// dependency inputs. It does not relax span validation: compiler-provided sources must be just as
/// precisely addressable as ordinary package sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledSource {
    pub file: FileId,
    /// Present for package, dependency, and compiler-provided static-module sources.
    /// Standalone external sources such as the exact NIDL snapshot have no package `SourceKey`.
    pub key: Option<SourceKey>,
    pub identity: SourceIdentity,
    pub module_path: Option<String>,
    pub text: Arc<str>,
    pub compiler_provided: bool,
}

/// Complete immutable source snapshot for one package artifact.
///
/// Numeric [`FileId`] values are artifact-local. Stable identity is always [`SourceKey`].
#[derive(Clone, Debug)]
pub struct PackageSourceSnapshot {
    files: Arc<[CompiledSource]>,
    by_file: Arc<BTreeMap<FileId, usize>>,
    by_key: Arc<BTreeMap<SourceKey, usize>>,
    diagnostics: Arc<SourceSnapshotRegistry>,
}

impl PackageSourceSnapshot {
    pub fn from_compiler_sources(
        sources: impl IntoIterator<Item = nexa_compiler::PackageCompiledSource>,
    ) -> Result<Self, PackageArtifactIntegrityError> {
        let sources = sources
            .into_iter()
            .map(|source| {
                if let Some(key) = &source.source_key {
                    if source.package_id.as_deref() != Some(key.package_id.as_str())
                        || source.path != key.path.as_str()
                        || source.identity
                            != SourceIdentity::package(key.package_id.as_str(), key.path.as_str())
                        || source.module_path.is_none()
                    {
                        return Err(
                            PackageArtifactIntegrityError::CompilerSourceIdentityMismatch {
                                key: key.clone(),
                                package: source.package_id,
                                path: source.path,
                            },
                        );
                    }
                } else if source.package_id.is_some() || source.module_path.is_some() {
                    return Err(
                        PackageArtifactIntegrityError::InvalidExternalSourceIdentity {
                            identity: source.identity,
                            package: source.package_id,
                            module: source.module_path,
                        },
                    );
                }
                Ok(CompiledSource {
                    file: source.file,
                    key: source.source_key,
                    identity: source.identity,
                    module_path: source.module_path,
                    text: source.source,
                    compiler_provided: source.compiler_provided,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(sources)
    }

    /// Builds a deterministic snapshot from the exact sources returned by code generation.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        sources: impl IntoIterator<Item = CompiledSource>,
    ) -> Result<Self, PackageArtifactIntegrityError> {
        let mut files = sources.into_iter().collect::<Vec<_>>();
        files.sort_by(|left, right| {
            (left.file, &left.identity, &left.module_path).cmp(&(
                right.file,
                &right.identity,
                &right.module_path,
            ))
        });

        let mut by_file = BTreeMap::new();
        let mut by_key = BTreeMap::new();
        let mut diagnostics = SourceSnapshotRegistry::builder();
        for (index, source) in files.iter().enumerate() {
            if source.file == FileId::default() {
                return Err(PackageArtifactIntegrityError::ReservedFileId);
            }
            let module = source
                .module_path
                .as_ref()
                .map(|module| {
                    ModulePath::new(module.clone()).map_err(|_| {
                        PackageArtifactIntegrityError::InvalidModulePath(module.clone())
                    })
                })
                .transpose()?;
            if let (Some(key), Some(module)) = (&source.key, &module)
                && !source.compiler_provided
            {
                let expected = if let Some(test_module) = module.as_str().strip_prefix("test.") {
                    NormalizedPackagePath::new(format!(
                        "tests/{}.nexa",
                        test_module.replace('.', "/")
                    ))
                    .expect("a validated test module maps to a normalized path")
                } else {
                    module.source_path()
                };
                if key.path != expected {
                    return Err(PackageArtifactIntegrityError::ModulePathMismatch {
                        module: module.clone(),
                        expected,
                        actual: key.path.clone(),
                    });
                }
            } else if source.key.is_some() != source.module_path.is_some() {
                return Err(
                    PackageArtifactIntegrityError::IncompletePackageSourceIdentity {
                        identity: source.identity.clone(),
                    },
                );
            }
            if let Some(first) = by_file.insert(source.file, index) {
                return Err(PackageArtifactIntegrityError::DuplicateFileId {
                    file: source.file,
                    first: files[first].identity.clone(),
                    second: source.identity.clone(),
                });
            }
            let expected_file = FileId(
                u32::try_from(index + 1)
                    .map_err(|_| PackageArtifactIntegrityError::TooManySourceFiles)?,
            );
            if source.file != expected_file {
                return Err(PackageArtifactIntegrityError::NonDenseFileId {
                    expected: expected_file,
                    actual: source.file,
                });
            }
            if let Some(key) = &source.key
                && let Some(first) = by_key.insert(key.clone(), index)
            {
                return Err(PackageArtifactIntegrityError::DuplicateSourceKey {
                    key: key.clone(),
                    first_file: files[first].file,
                    second_file: source.file,
                });
            }
            diagnostics
                .insert(source.identity.clone(), Arc::clone(&source.text))
                .map_err(PackageArtifactIntegrityError::DiagnosticSnapshot)?;
        }
        let source_order = files
            .iter()
            .map(|source| {
                let tier = match (&source.key, source.compiler_provided) {
                    (Some(_), false) => 0_u8,
                    (Some(_), true) => 1_u8,
                    (None, _) => 2_u8,
                };
                (tier, source.identity.to_string())
            })
            .collect::<Vec<_>>();
        let mut canonical_source_order = source_order.clone();
        canonical_source_order.sort();
        if source_order != canonical_source_order {
            return Err(PackageArtifactIntegrityError::NonDeterministicSourceOrder);
        }

        Ok(Self {
            files: files.into(),
            by_file: Arc::new(by_file),
            by_key: Arc::new(by_key),
            diagnostics: diagnostics.build(),
        })
    }

    #[must_use]
    pub fn files(&self) -> &[CompiledSource] {
        &self.files
    }

    #[must_use]
    pub fn source(&self, file: FileId) -> Option<&CompiledSource> {
        self.by_file
            .get(&file)
            .and_then(|index| self.files.get(*index))
    }

    #[must_use]
    pub fn source_by_key(&self, key: &SourceKey) -> Option<&CompiledSource> {
        self.by_key
            .get(key)
            .and_then(|index| self.files.get(*index))
    }

    #[must_use]
    pub fn diagnostic_sources(&self) -> &Arc<SourceSnapshotRegistry> {
        &self.diagnostics
    }

    #[must_use]
    pub fn diagnostic_source(&self, key: &SourceKey) -> Option<&Arc<SourceSnapshot>> {
        self.diagnostics.get(&SourceIdentity::package(
            key.package_id.as_str(),
            key.path.as_str(),
        ))
    }

    /// Returns reader-facing stable paths for package-test reports and stack frames.
    #[must_use]
    pub fn source_paths(&self) -> BTreeMap<FileId, String> {
        self.files
            .iter()
            .map(|source| (source.file, source.identity.to_string()))
            .collect()
    }
}

/// Verified, statically linked output for one application Package.
#[derive(Clone, Debug)]
pub struct CompiledPackageArtifact {
    pub identity: CandidateIdentity,
    pub verified: VerifiedModule,
    pub source_files: PackageSourceSnapshot,
    pub debug_info: PackageDebugInfo,
    pub state_surface: Arc<[PackageStateTypeInfo]>,
    pub compilation_evidence: PackageCompilationEvidence,
    pub source_set_fingerprint: SourceSetFingerprint,
    pub public_api_fingerprint: PublicApiFingerprint,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub build_fingerprint: BuildFingerprint,
    pub linked_state_fingerprint: LinkedStateFingerprint,
    pub dependency_closure: Arc<ResolvedDependencyGraph>,
    pub dependency_source_fingerprints: Arc<BTreeMap<PackageId, SourceSetFingerprint>>,
    pub analysis_revision: u64,
}

fn package_compilation_evidence(
    ir: &nexa_analysis::TypedPackageIr,
    import_edges: &[nexa_analysis::ResolvedImportEdge],
) -> PackageCompilationEvidence {
    let package_modules = ir
        .modules()
        .iter()
        .filter(|module| module.package_id.as_str() != nexa_stdlib::PACKAGE_ID)
        .count();
    let packages = ir
        .modules()
        .iter()
        .filter(|module| module.package_id.as_str() != nexa_stdlib::PACKAGE_ID)
        .map(|module| module.package_id.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let package_symbols = ir
        .definitions()
        .iter()
        .filter(|definition| {
            definition.package_id.as_str() != nexa_stdlib::PACKAGE_ID
                && definition.module.as_str() != "nexa.builtin"
                && definition.module.as_str() != "host"
        })
        .count();
    let import_edges = import_edges
        .iter()
        .filter(|edge| matches!(edge.target, nexa_analysis::ResolvedImportTarget::Module(_)))
        .count();
    PackageCompilationEvidence {
        modules: ir.modules().len(),
        symbols: ir.definitions().len(),
        package_modules,
        package_symbols,
        import_edges,
        packages,
    }
}

fn validate_compiled_closure(
    input: &ResolvedBuildInput,
    sources: &PackageSourceSnapshot,
    debug_info: &PackageDebugInfo,
    evidence: PackageCompilationEvidence,
) -> Result<(), PackageBuildError> {
    let expected_sources = input
        .all_source_sets()
        .flat_map(PackageSourceSet::production_units)
        .map(|unit| unit.key.clone())
        .collect::<BTreeSet<_>>();
    let actual_sources = sources
        .files()
        .iter()
        .filter(|source| !source.compiler_provided)
        .filter_map(|source| source.key.clone())
        .collect::<BTreeSet<_>>();
    if actual_sources != expected_sources {
        return Err(PackageBuildError::CompilerSourceClosureMismatch);
    }

    let expected_modules = input
        .all_source_sets()
        .flat_map(PackageSourceSet::production_units)
        .map(|unit| {
            (
                unit.key.package_id.as_str().to_owned(),
                unit.expected_module_path()
                    .expect("resolved source module identity was validated")
                    .as_str()
                    .to_owned(),
            )
        })
        .collect::<BTreeSet<_>>();
    let actual_modules = debug_info
        .modules
        .iter()
        .filter(|module| module.package_id != nexa_stdlib::PACKAGE_ID)
        .map(|module| (module.package_id.clone(), module.module_path.clone()))
        .collect::<BTreeSet<_>>();
    if actual_modules != expected_modules {
        return Err(PackageBuildError::CompilerModuleClosureMismatch);
    }

    let actual_packages = actual_sources
        .iter()
        .map(|source| source.package_id.clone())
        .collect::<BTreeSet<_>>();
    let resolved_packages = input
        .dependency_graph
        .packages
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if actual_packages != resolved_packages
        || evidence.packages != actual_packages.len()
        || evidence.modules != debug_info.modules.len()
        || evidence.package_modules != expected_modules.len()
    {
        return Err(PackageBuildError::CompilerCompilationEvidenceMismatch);
    }
    Ok(())
}

fn validate_candidate(
    input: &ResolvedBuildInput,
    identity: &CandidateIdentity,
) -> Result<(), PackageBuildError> {
    if identity.package_id != input.root_manifest.id {
        return Err(PackageBuildError::CandidatePackageMismatch {
            candidate: identity.package_id.clone(),
            input: input.root_manifest.id.clone(),
        });
    }
    if identity.build_fingerprint != input.build_fingerprint {
        return Err(PackageBuildError::CandidateFingerprintMismatch {
            candidate: identity.build_fingerprint,
            input: input.build_fingerprint,
        });
    }
    Ok(())
}

fn validate_host_contract(
    input: &ResolvedBuildInput,
    contract: &HostContractInput<'_>,
) -> Result<(), PackageBuildError> {
    if input.canonical_host_contract.as_ref() != nexa_idl::canonical(contract.idl()).as_bytes() {
        return Err(PackageBuildError::HostContractMismatch);
    }
    if input.host_contract_source_identity.as_ref()
        != canonical_host_contract_source_identity(contract).as_slice()
    {
        return Err(PackageBuildError::HostContractSourceMismatch);
    }
    if input.host_required_exports_identity.as_ref()
        != contract.canonical_required_exports().as_slice()
    {
        return Err(PackageBuildError::HostRequiredExportsMismatch);
    }
    if input.compilation_options != nexa_analysis::CompilationOptions::default()
        || input.fingerprint_input.compiler_options != canonical_compilation_options()
    {
        return Err(PackageBuildError::CompilationOptionsMismatch);
    }
    Ok(())
}

fn validate_product_compiler_output(
    compiled: &nexa_compiler::PackageCompileOutput,
    outcome: &AnalysisOutcome,
) -> Result<(), PackageBuildError> {
    if compiled.test_module.is_some()
        || !compiled.test_sources.is_empty()
        || compiled.test_debug_info.is_some()
        || !compiled.tests.is_empty()
        || !compiled.test_call_graph.is_empty()
    {
        return Err(PackageBuildError::InvalidProductCompilerOutput(
            "product codegen retained test-only output",
        ));
    }
    validate_compiler_fingerprints(compiled, outcome)
}

fn validate_test_compiler_output(
    compiled: &nexa_compiler::PackageCompileOutput,
    outcome: &AnalysisOutcome,
) -> Result<(), PackageBuildError> {
    if !compiled.sources.is_empty()
        || !compiled.debug_info.modules.is_empty()
        || !compiled.debug_info.functions.is_empty()
        || !compiled.public_symbols.is_empty()
        || !compiled.state_surface.is_empty()
    {
        return Err(PackageBuildError::InvalidTestCompilerOutput(
            "test codegen retained product-only output",
        ));
    }
    if compiled.test_module.is_none() || compiled.test_debug_info.is_none() {
        return Err(PackageBuildError::InvalidTestCompilerOutput(
            "test codegen omitted its explicit test module or debug metadata",
        ));
    }
    validate_compiler_fingerprints(compiled, outcome)
}

fn validate_compiler_fingerprints(
    compiled: &nexa_compiler::PackageCompileOutput,
    outcome: &AnalysisOutcome,
) -> Result<(), PackageBuildError> {
    let standard_library = nexa_stdlib::standard_library();
    if compiled.standard_library.package_id != standard_library.package_id
        || compiled.standard_library.canonical_package_id != standard_library.canonical_package_id
        || compiled.standard_library.version != standard_library.version.to_string()
        || compiled.standard_library.descriptor_schema != standard_library.descriptor_schema
        || compiled.standard_library.descriptor_hash != standard_library.descriptor_hash().0
    {
        return Err(PackageBuildError::CompilerStandardLibraryMismatch);
    }
    if compiled.public_api_fingerprint != Some(outcome.public_api_fingerprint) {
        return Err(PackageBuildError::CompilerPublicApiFingerprintMismatch);
    }
    if compiled.state_schema_fingerprint != Some(outcome.state_schema_fingerprint)
        || compiled.module.state_schema_fingerprint != outcome.state_schema_fingerprint
        || compiled.module.reload_metadata.state_schema_fingerprint
            != outcome.state_schema_fingerprint
    {
        return Err(PackageBuildError::CompilerStateSchemaFingerprintMismatch);
    }
    Ok(())
}

fn validate_host_exports(
    module: &nexa_bytecode::Module,
    contract: &HostContractInput<'_>,
) -> Result<(), PackageBuildError> {
    let interface = contract.idl();
    if module.host_interface_hash != Some(nexa_idl::exact_hash(interface)) {
        return Err(PackageBuildError::HostInterfaceHashMismatch);
    }
    for required in contract.required_exports() {
        let stable_id = nexa_idl::export_stable_id(interface, required);
        let Some(actual) = module
            .exports
            .iter()
            .find(|candidate| candidate.stable_id == stable_id)
        else {
            return Err(PackageBuildError::MissingRequiredExport(
                required.name.clone(),
            ));
        };
        let expected = nexa_idl::export_signature(interface, required);
        if actual.signature != expected {
            return Err(PackageBuildError::ExportSignatureMismatch {
                name: required.name.clone(),
                expected,
                actual: actual.signature.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug)]
pub enum PackageBuildError {
    InvalidResolvedInput(nexa_analysis::ResolvedBuildInputError),
    CandidatePackageMismatch {
        candidate: PackageId,
        input: PackageId,
    },
    CandidateFingerprintMismatch {
        candidate: BuildFingerprint,
        input: BuildFingerprint,
    },
    ApplicationArtifactRequired(PackageId),
    HostContractMismatch,
    HostContractSourceMismatch,
    HostRequiredExportsMismatch,
    CompilationOptionsMismatch,
    Environment(crate::package_environment::PackageEnvironmentError),
    AnalysisFailed(DiagnosticBatch),
    MissingTypedPackageIr,
    Compile(nexa_compiler::CompileError),
    Verify(nexa_verifier::VerifyError),
    InvalidProductCompilerOutput(&'static str),
    InvalidTestCompilerOutput(&'static str),
    CompilerSourceClosureMismatch,
    CompilerModuleClosureMismatch,
    CompilerCompilationEvidenceMismatch,
    CompilerPublicApiFingerprintMismatch,
    CompilerStateSchemaFingerprintMismatch,
    CompilerStandardLibraryMismatch,
    MissingRequiredExport(String),
    ExportSignatureMismatch {
        name: String,
        expected: nexa_bytecode::Signature,
        actual: nexa_bytecode::Signature,
    },
    HostInterfaceHashMismatch,
    MissingTestModule,
    MissingTestDebugInfo,
    InvalidTestArtifact(crate::PackageTestRunError),
    Integrity(Box<PackageArtifactIntegrityError>),
}

impl fmt::Display for PackageBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResolvedInput(error) => {
                write!(formatter, "invalid resolved build input: {error}")
            }
            Self::CandidatePackageMismatch { candidate, input } => write!(
                formatter,
                "candidate package {candidate} does not match resolved input {input}"
            ),
            Self::CandidateFingerprintMismatch { .. } => {
                formatter.write_str("candidate BuildFingerprint does not match resolved input")
            }
            Self::ApplicationArtifactRequired(package) => {
                write!(
                    formatter,
                    "package {package} is a library and has no runtime artifact"
                )
            }
            Self::HostContractMismatch => {
                formatter.write_str("Host contract does not match resolved build input")
            }
            Self::HostContractSourceMismatch => formatter.write_str(
                "Host contract source URI or raw text does not match resolved build input",
            ),
            Self::HostRequiredExportsMismatch => {
                formatter.write_str("Host required-export view does not match resolved build input")
            }
            Self::CompilationOptionsMismatch => formatter.write_str(
                "resolved build input does not use the canonical M4 compilation options",
            ),
            Self::Environment(error) => error.fmt(formatter),
            Self::AnalysisFailed(diagnostics) => write!(
                formatter,
                "package analysis failed with {} diagnostic(s)",
                diagnostics.len()
            ),
            Self::MissingTypedPackageIr => {
                formatter.write_str("successful analysis did not produce TypedPackageIr")
            }
            Self::Compile(error) => error.fmt(formatter),
            Self::Verify(error) => error.fmt(formatter),
            Self::InvalidProductCompilerOutput(message)
            | Self::InvalidTestCompilerOutput(message) => formatter.write_str(message),
            Self::CompilerSourceClosureMismatch => {
                formatter.write_str("compiler source registry does not match the resolved closure")
            }
            Self::CompilerModuleClosureMismatch => {
                formatter.write_str("compiler debug modules do not match the resolved closure")
            }
            Self::CompilerCompilationEvidenceMismatch => formatter.write_str(
                "compiler artifact cardinalities do not match its linked source/debug closure",
            ),
            Self::CompilerPublicApiFingerprintMismatch => {
                formatter.write_str("compiler changed the analyzed PublicApiFingerprint")
            }
            Self::CompilerStateSchemaFingerprintMismatch => {
                formatter.write_str("compiler changed the analyzed StateSchemaFingerprint")
            }
            Self::CompilerStandardLibraryMismatch => formatter.write_str(
                "compiler standard-library identity does not match the canonical M4 descriptor",
            ),
            Self::MissingRequiredExport(name) => {
                write!(formatter, "missing required export {name}")
            }
            Self::ExportSignatureMismatch { name, .. } => {
                write!(formatter, "export {name} has an incompatible signature")
            }
            Self::HostInterfaceHashMismatch => {
                formatter.write_str("compiled Host interface hash does not match the exact IDL")
            }
            Self::MissingTestModule => formatter.write_str("test build produced no test module"),
            Self::MissingTestDebugInfo => {
                formatter.write_str("test build produced no test debug metadata")
            }
            Self::InvalidTestArtifact(error) => error.fmt(formatter),
            Self::Integrity(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PackageBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidResolvedInput(error) => Some(error),
            Self::Environment(error) => Some(error),
            Self::Compile(error) => Some(error),
            Self::Verify(error) => Some(error),
            Self::InvalidTestArtifact(error) => Some(error),
            Self::Integrity(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<crate::package_environment::PackageEnvironmentError> for PackageBuildError {
    fn from(value: crate::package_environment::PackageEnvironmentError) -> Self {
        Self::Environment(value)
    }
}

impl From<nexa_compiler::CompileError> for PackageBuildError {
    fn from(value: nexa_compiler::CompileError) -> Self {
        Self::Compile(value)
    }
}

impl From<nexa_verifier::VerifyError> for PackageBuildError {
    fn from(value: nexa_verifier::VerifyError) -> Self {
        Self::Verify(value)
    }
}

impl From<PackageArtifactIntegrityError> for PackageBuildError {
    fn from(value: PackageArtifactIntegrityError) -> Self {
        Self::Integrity(Box::new(value))
    }
}

/// Explicit test-only artifact. It is never loadable as a product Package candidate.
#[derive(Clone, Debug)]
pub struct CompiledPackageTests {
    package_id: PackageId,
    verified: VerifiedModule,
    source_files: PackageSourceSnapshot,
    debug_info: PackageDebugInfo,
    tests: Arc<[PackageTestInfo]>,
    call_graph: Arc<[PackageTestCallGraphNode]>,
    source_paths: Arc<BTreeMap<FileId, String>>,
}

impl CompiledPackageTests {
    pub(crate) fn new(
        package_id: PackageId,
        verified: VerifiedModule,
        source_files: PackageSourceSnapshot,
        debug_info: PackageDebugInfo,
        tests: Vec<PackageTestInfo>,
        call_graph: Vec<PackageTestCallGraphNode>,
    ) -> Result<Self, PackageBuildError> {
        if debug_info.root_package_id != package_id.as_str() {
            return Err(PackageArtifactIntegrityError::DebugRootMismatch {
                identity: package_id,
                debug_root: debug_info.root_package_id.clone(),
            }
            .into());
        }
        verify_package_artifact_integrity(verified.module(), &source_files, &debug_info)?;
        if !tests.is_empty()
            && !source_files.files().iter().any(|source| {
                source
                    .key
                    .as_ref()
                    .is_some_and(|key| key.path.as_str().starts_with("tests/"))
                    && source
                        .module_path
                        .as_deref()
                        .is_some_and(|module| module.starts_with("test."))
            })
        {
            return Err(PackageArtifactIntegrityError::TestArtifactMissingTestSource.into());
        }
        if !tests.is_empty()
            && !debug_info
                .modules
                .iter()
                .any(|module| module.module_path.starts_with("test."))
        {
            return Err(PackageArtifactIntegrityError::TestArtifactMissingTestModule.into());
        }
        let source_paths = Arc::new(source_files.source_paths());
        crate::package_test::validate_package_test_artifact(
            crate::package_test::PackageTestArtifactRef {
                verified: &verified,
                tests: &tests,
                call_graph: &call_graph,
                debug_info: &debug_info,
                source_paths: &source_paths,
            },
        )
        .map_err(PackageBuildError::InvalidTestArtifact)?;
        Ok(Self {
            package_id,
            verified,
            source_files,
            debug_info,
            tests: tests.into(),
            call_graph: call_graph.into(),
            source_paths,
        })
    }

    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    #[must_use]
    pub fn test_count(&self) -> usize {
        self.tests.len()
    }

    #[must_use]
    pub fn source_files(&self) -> &PackageSourceSnapshot {
        &self.source_files
    }

    #[must_use]
    pub(crate) fn artifact_ref(&self) -> crate::package_test::PackageTestArtifactRef<'_> {
        crate::package_test::PackageTestArtifactRef {
            verified: &self.verified,
            tests: &self.tests,
            call_graph: &self.call_graph,
            debug_info: &self.debug_info,
            source_paths: &self.source_paths,
        }
    }

    pub fn run(
        &self,
        options: crate::PackageTestOptions,
    ) -> Result<crate::TestRun, crate::PackageTestRunError> {
        crate::package_test::run_package_tests(self.artifact_ref(), options)
    }
}

impl CompiledPackageArtifact {
    /// Encodes the already-verified product module.
    ///
    /// Callers never need to reach through the façade into compiler output or run a second
    /// verifier pass. Test modules deliberately have no corresponding product-byte accessor.
    #[must_use]
    pub fn encode_module(&self) -> Vec<u8> {
        self.verified.module().encode()
    }

    /// Borrows the verified product module for runtime loading and legacy inspection commands.
    #[must_use]
    pub fn module(&self) -> &nexa_bytecode::Module {
        self.verified.module()
    }

    /// Resolves one linked Stateful type by its exact Package/Module/name coordinates.
    #[must_use]
    pub fn state_type(
        &self,
        package_id: &str,
        module_path: &str,
        name: &str,
    ) -> Option<&PackageStateTypeInfo> {
        self.state_surface.iter().find(|state| {
            state.package_id == package_id && state.module_path == module_path && state.name == name
        })
    }

    /// Resolves a Stateful type by name only when that name is unique across the linked closure.
    ///
    /// This supports high-level embedding helpers without reintroducing legacy name hashing. A
    /// missing or ambiguous name returns `None`; hosts which need exact control should use
    /// [`Self::state_type`].
    #[must_use]
    pub fn unique_state_type_named(&self, name: &str) -> Option<&PackageStateTypeInfo> {
        let mut matches = self.state_surface.iter().filter(|state| state.name == name);
        let state = matches.next()?;
        matches.next().is_none().then_some(state)
    }

    fn verify_fingerprint_integrity(&self) -> Result<(), PackageArtifactIntegrityError> {
        if self.identity.build_fingerprint != self.build_fingerprint {
            return Err(PackageArtifactIntegrityError::BuildFingerprintMismatch {
                identity: self.identity.build_fingerprint,
                artifact: self.build_fingerprint,
            });
        }
        let computed_linked_state = linked_state_fingerprint(
            self.build_fingerprint,
            self.verified.module(),
            &self.source_files,
            &self.debug_info,
            &self.state_surface,
            self.compilation_evidence,
            self.source_set_fingerprint,
            self.public_api_fingerprint,
            self.state_schema_fingerprint,
            &self.dependency_closure,
            &self.dependency_source_fingerprints,
        );
        if self.linked_state_fingerprint != computed_linked_state {
            return Err(
                PackageArtifactIntegrityError::LinkedStateFingerprintMismatch {
                    artifact: self.linked_state_fingerprint,
                    computed: computed_linked_state,
                },
            );
        }
        let module = self.verified.module();
        let computed_state_schema = module.state_schema.fingerprint();
        for (location, observed) in [
            ("module metadata", module.state_schema_fingerprint),
            (
                "reload metadata",
                module.reload_metadata.state_schema_fingerprint,
            ),
            ("encoded state schema", computed_state_schema),
        ] {
            if self.state_schema_fingerprint == observed {
                continue;
            }
            return Err(
                PackageArtifactIntegrityError::StateSchemaFingerprintMismatch {
                    location,
                    artifact: self.state_schema_fingerprint,
                    observed,
                },
            );
        }
        Ok(())
    }

    /// Re-validates all cross-crate identities before an artifact crosses into Runtime or LKG.
    pub fn verify_integrity(&self) -> Result<(), PackageArtifactIntegrityError> {
        self.verify_fingerprint_integrity()?;
        if self.identity.package_id != self.dependency_closure.root {
            return Err(PackageArtifactIntegrityError::DependencyRootMismatch {
                identity: self.identity.package_id.clone(),
                dependency_root: self.dependency_closure.root.clone(),
            });
        }
        if self.debug_info.root_package_id != self.identity.package_id.as_str() {
            return Err(PackageArtifactIntegrityError::DebugRootMismatch {
                identity: self.identity.package_id.clone(),
                debug_root: self.debug_info.root_package_id.clone(),
            });
        }
        let expected_dependencies = self
            .dependency_closure
            .packages
            .keys()
            .filter(|package| **package != self.dependency_closure.root)
            .cloned()
            .collect::<BTreeSet<_>>();
        let actual_dependencies = self
            .dependency_source_fingerprints
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if expected_dependencies != actual_dependencies {
            return Err(
                PackageArtifactIntegrityError::DependencyFingerprintSetMismatch {
                    expected: expected_dependencies,
                    actual: actual_dependencies,
                },
            );
        }
        for source in self.source_files.files() {
            if source
                .key
                .as_ref()
                .is_some_and(|key| key.path.as_str().starts_with("tests/"))
                || source
                    .module_path
                    .as_deref()
                    .is_some_and(|module| module.starts_with("test."))
            {
                return Err(PackageArtifactIntegrityError::ProductContainsTestSource {
                    identity: source.identity.clone(),
                });
            }
            if let Some(key) = &source.key
                && !source.compiler_provided
                && !self
                    .dependency_closure
                    .packages
                    .contains_key(&key.package_id)
            {
                return Err(
                    PackageArtifactIntegrityError::SourceOutsideDependencyClosure(key.clone()),
                );
            }
        }
        if let Some(module) = self
            .debug_info
            .modules
            .iter()
            .find(|module| module.module_path.starts_with("test."))
        {
            return Err(PackageArtifactIntegrityError::ProductContainsTestModule {
                package: module.package_id.clone(),
                module: module.module_path.clone(),
            });
        }
        let mut stable_symbols = BTreeMap::new();
        validate_debug_symbol_identities(
            &self.debug_info,
            &self.source_files,
            &mut stable_symbols,
        )?;
        validate_state_surface(
            self.verified.module(),
            &self.state_surface,
            &self.source_files,
            &mut stable_symbols,
        )?;
        verify_package_artifact_integrity(
            self.verified.module(),
            &self.source_files,
            &self.debug_info,
        )
    }
}

/// Verifies that every source-map and debug location resolves inside the immutable source
/// snapshot, and that every debug function refers to the emitted bytecode function it describes.
#[allow(clippy::too_many_lines)]
pub fn verify_package_artifact_integrity(
    module: &nexa_bytecode::Module,
    sources: &PackageSourceSnapshot,
    debug_info: &PackageDebugInfo,
) -> Result<(), PackageArtifactIntegrityError> {
    validate_debug_symbol_identities(debug_info, sources, &mut BTreeMap::new())?;
    let function_count = module.functions.len();
    let mut source_coverage = module
        .functions
        .iter()
        .map(|function| vec![false; function.code.len()])
        .collect::<Vec<_>>();
    for entry in &module.source_map {
        let function = usize::try_from(entry.function).unwrap_or(usize::MAX);
        if function >= function_count {
            return Err(PackageArtifactIntegrityError::UnknownFunction {
                function: entry.function,
                location: "source-map",
            });
        }
        let code_len = u32::try_from(module.functions[function].code.len()).unwrap_or(u32::MAX);
        if entry.pc_start >= entry.pc_end || entry.pc_end > code_len {
            return Err(PackageArtifactIntegrityError::InvalidProgramCounterRange {
                function: entry.function,
                start: entry.pc_start,
                end: entry.pc_end,
                code_len,
            });
        }
        for pc in entry.pc_start..entry.pc_end {
            let pc_index = usize::try_from(pc).expect("validated PC fits the function code vector");
            let covered = &mut source_coverage[function][pc_index];
            if *covered {
                return Err(PackageArtifactIntegrityError::OverlappingSourceMap {
                    function: entry.function,
                    pc,
                });
            }
            *covered = true;
        }
        validate_span(entry.span, sources, "source-map")?;
    }
    for (function, coverage) in source_coverage.iter().enumerate() {
        if let Some(pc) = coverage.iter().position(|covered| !covered) {
            return Err(PackageArtifactIntegrityError::MissingSourceMap {
                function: u32::try_from(function).unwrap_or(u32::MAX),
                pc: u32::try_from(pc).unwrap_or(u32::MAX),
            });
        }
    }

    let mut debug_functions = BTreeSet::new();
    for function in &debug_info.functions {
        if !debug_functions.insert(function.function_index) {
            return Err(PackageArtifactIntegrityError::DuplicateDebugFunction(
                function.function_index,
            ));
        }
        if usize::try_from(function.function_index)
            .ok()
            .is_none_or(|index| index >= function_count)
        {
            return Err(PackageArtifactIntegrityError::UnknownFunction {
                function: function.function_index,
                location: "debug-info",
            });
        }
        validate_span(function.definition_span, sources, "function-definition")?;
        let source = sources.source(function.definition_span.file).ok_or(
            PackageArtifactIntegrityError::UnknownSourceFile {
                file: function.definition_span.file,
                location: "function-definition",
            },
        )?;
        if source.key.as_ref().map(|key| key.package_id.as_str()) != Some(&function.package_id)
            || source.module_path.as_deref() != Some(&function.module_path)
        {
            return Err(PackageArtifactIntegrityError::DebugSourceMismatch {
                file: function.definition_span.file,
                expected_identity: source.identity.clone(),
                expected_module: source.module_path.clone(),
                actual_package: function.package_id.clone(),
                actual_module: function.module_path.clone(),
            });
        }
    }
    for function in 0..function_count {
        let function = u32::try_from(function).unwrap_or(u32::MAX);
        if !debug_functions.contains(&function) {
            return Err(PackageArtifactIntegrityError::MissingDebugFunction(
                function,
            ));
        }
    }

    let mut module_function_owners = BTreeMap::<u32, (String, String)>::new();
    for source_module in &debug_info.modules {
        let source = sources.source(source_module.file).ok_or(
            PackageArtifactIntegrityError::UnknownSourceFile {
                file: source_module.file,
                location: "module-debug-info",
            },
        )?;
        if source_module.definition_span.file != source_module.file
            || source_module.source_span.file != source_module.file
            || source.key.as_ref().map(|key| key.package_id.as_str())
                != Some(&source_module.package_id)
            || source.module_path.as_deref() != Some(&source_module.module_path)
        {
            return Err(PackageArtifactIntegrityError::DebugSourceMismatch {
                file: source_module.file,
                expected_identity: source.identity.clone(),
                expected_module: source.module_path.clone(),
                actual_package: source_module.package_id.clone(),
                actual_module: source_module.module_path.clone(),
            });
        }
        validate_span(source_module.definition_span, sources, "module-definition")?;
        validate_span(source_module.source_span, sources, "module-source")?;
        for function in &source_module.function_indices {
            if usize::try_from(*function)
                .ok()
                .is_none_or(|index| index >= function_count)
            {
                return Err(PackageArtifactIntegrityError::UnknownFunction {
                    function: *function,
                    location: "module-debug-info",
                });
            }
            if let Some((first_package, first_module)) = module_function_owners.insert(
                *function,
                (
                    source_module.package_id.clone(),
                    source_module.module_path.clone(),
                ),
            ) {
                return Err(
                    PackageArtifactIntegrityError::DuplicateModuleFunctionOwnership {
                        function: *function,
                        first_package,
                        first_module,
                        second_package: source_module.package_id.clone(),
                        second_module: source_module.module_path.clone(),
                    },
                );
            }
            let debug = debug_info
                .functions
                .iter()
                .find(|debug| debug.function_index == *function)
                .expect("complete debug-function coverage was checked");
            if debug.package_id != source_module.package_id
                || debug.module_path != source_module.module_path
            {
                return Err(PackageArtifactIntegrityError::ModuleFunctionMismatch {
                    function: *function,
                    owner_package: source_module.package_id.clone(),
                    owner_module: source_module.module_path.clone(),
                    debug_package: debug.package_id.clone(),
                    debug_module: debug.module_path.clone(),
                });
            }
        }
    }
    for function in &debug_functions {
        if !module_function_owners.contains_key(function) {
            return Err(PackageArtifactIntegrityError::MissingModuleFunctionOwnership(*function));
        }
    }

    if debug_info.host_imports.len() != module.host_imports.len() {
        return Err(
            PackageArtifactIntegrityError::HostDebugImportCountMismatch {
                expected: module.host_imports.len(),
                actual: debug_info.host_imports.len(),
            },
        );
    }
    let mut host_debug_imports = BTreeSet::new();
    for host in &debug_info.host_imports {
        if !host_debug_imports.insert(host.import_index) {
            return Err(PackageArtifactIntegrityError::DuplicateHostDebugImport(
                host.import_index,
            ));
        }
        let Some(module_import) = usize::try_from(host.import_index)
            .ok()
            .and_then(|index| module.host_imports.get(index))
        else {
            return Err(PackageArtifactIntegrityError::UnknownHostImport(
                host.import_index,
            ));
        };
        if module_import.stable_id != host.stable_id {
            return Err(PackageArtifactIntegrityError::HostImportStableIdMismatch {
                import: host.import_index,
                module: module_import.stable_id,
                debug: host.stable_id,
            });
        }
        if module.host_interface_hash != Some(host.interface_id) {
            return Err(
                PackageArtifactIntegrityError::HostInterfaceDebugIdMismatch {
                    import: host.import_index,
                    module: module.host_interface_hash,
                    debug: host.interface_id,
                },
            );
        }
        if host.interface_name.is_empty() || host.function_name.is_empty() {
            return Err(PackageArtifactIntegrityError::EmptyHostDebugName {
                import: host.import_index,
            });
        }
        validate_nonempty_span(host.interface_span, sources, "host-interface")?;
        validate_nonempty_span(host.declaration_span, sources, "host-declaration")?;
        if host.interface_span.file != host.declaration_span.file {
            return Err(PackageArtifactIntegrityError::HostDebugSourceMismatch {
                import: host.import_index,
                interface_file: host.interface_span.file,
                declaration_file: host.declaration_span.file,
            });
        }
        let source = sources.source(host.declaration_span.file).ok_or(
            PackageArtifactIntegrityError::UnknownSourceFile {
                file: host.declaration_span.file,
                location: "host-declaration",
            },
        )?;
        if source.key.is_some() || source.module_path.is_some() {
            return Err(PackageArtifactIntegrityError::HostDebugSourceIsPackage {
                import: host.import_index,
                identity: source.identity.clone(),
            });
        }
        let (_, source_map) = nexa_idl::parse_with_source_map(&source.text).map_err(|_| {
            PackageArtifactIntegrityError::InvalidHostDebugSource {
                import: host.import_index,
                identity: source.identity.clone(),
            }
        })?;
        let Some(function_source) = source_map.functions.get(&host.function_name) else {
            return Err(
                PackageArtifactIntegrityError::HostDebugDeclarationMismatch {
                    import: host.import_index,
                },
            );
        };
        let expected_interface = SourceSpan::new(
            host.interface_span.file,
            u32::try_from(source_map.interface.declaration_start).unwrap_or(u32::MAX),
            u32::try_from(source_map.interface.declaration_end).unwrap_or(u32::MAX),
        );
        let expected_declaration = SourceSpan::new(
            host.declaration_span.file,
            u32::try_from(function_source.declaration_start).unwrap_or(u32::MAX),
            u32::try_from(function_source.declaration_end).unwrap_or(u32::MAX),
        );
        if source_map
            .interface
            .name_start
            .checked_add(host.interface_name.len())
            != Some(source_map.interface.name_end)
            || source
                .text
                .get(source_map.interface.name_start..source_map.interface.name_end)
                != Some(host.interface_name.as_str())
            || host.interface_span != expected_interface
            || host.declaration_span != expected_declaration
        {
            return Err(
                PackageArtifactIntegrityError::HostDebugDeclarationMismatch {
                    import: host.import_index,
                },
            );
        }
    }
    for expected in 0..module.host_imports.len() {
        let expected = u32::try_from(expected).unwrap_or(u32::MAX);
        if !host_debug_imports.contains(&expected) {
            return Err(PackageArtifactIntegrityError::MissingHostDebugImport(
                expected,
            ));
        }
    }
    Ok(())
}

fn validate_debug_symbol_identities(
    debug_info: &PackageDebugInfo,
    sources: &PackageSourceSnapshot,
    registry: &mut BTreeMap<StableSymbolId, CanonicalSymbolIdentity>,
) -> Result<(), PackageArtifactIntegrityError> {
    for function in &debug_info.functions {
        validate_span(function.definition_span, sources, "function-symbol")?;
        let source = sources.source(function.definition_span.file).ok_or(
            PackageArtifactIntegrityError::UnknownSourceFile {
                file: function.definition_span.file,
                location: "function-symbol",
            },
        )?;
        let canonical_package_id =
            if source.compiler_provided && function.package_id == nexa_stdlib::PACKAGE_ID {
                nexa_stdlib::CANONICAL_PACKAGE_ID
            } else {
                &function.package_id
            };
        validate_canonical_symbol(
            function.stable_id,
            &function.canonical_identity,
            canonical_package_id,
            &function.package_id,
            &function.module_path,
            &function.name,
            &[SymbolKind::Function, SymbolKind::Task, SymbolKind::Test],
            registry,
        )?;
    }
    Ok(())
}

fn validate_state_surface(
    module: &nexa_bytecode::Module,
    states: &[PackageStateTypeInfo],
    sources: &PackageSourceSnapshot,
    registry: &mut BTreeMap<StableSymbolId, CanonicalSymbolIdentity>,
) -> Result<(), PackageArtifactIntegrityError> {
    if module.state_schema.types.len() != states.len() {
        return Err(PackageArtifactIntegrityError::StateSurfaceCountMismatch {
            module: module.state_schema.types.len(),
            debug: states.len(),
        });
    }
    for state in states {
        validate_canonical_symbol(
            state.stable_id,
            &state.canonical_identity,
            &state.package_id,
            &state.package_id,
            &state.module_path,
            &state.name,
            &[SymbolKind::Type],
            registry,
        )?;
        validate_span(state.definition_span, sources, "state-type")?;
        let source = sources.source(state.definition_span.file).ok_or(
            PackageArtifactIntegrityError::UnknownSourceFile {
                file: state.definition_span.file,
                location: "state-type",
            },
        )?;
        if source.key.as_ref().map(|key| key.package_id.as_str()) != Some(&state.package_id)
            || source.module_path.as_deref() != Some(&state.module_path)
        {
            return Err(PackageArtifactIntegrityError::DebugSourceMismatch {
                file: state.definition_span.file,
                expected_identity: source.identity.clone(),
                expected_module: source.module_path.clone(),
                actual_package: state.package_id.clone(),
                actual_module: state.module_path.clone(),
            });
        }
        let Some(bytecode_state) = module
            .state_schema
            .types
            .iter()
            .find(|candidate| candidate.stable_id == state.stable_id.0)
        else {
            return Err(PackageArtifactIntegrityError::MissingBytecodeStateType(
                state.stable_id,
            ));
        };
        if bytecode_state.version != state.version {
            return Err(PackageArtifactIntegrityError::StateVersionMismatch {
                state: state.stable_id,
                module: bytecode_state.version,
                debug: state.version,
            });
        }
        let mut expected_fields = BTreeSet::new();
        for field in &state.fields {
            let canonical_field_name = format!("{}.{}", state.name, field.name);
            validate_canonical_symbol(
                field.stable_id,
                &field.canonical_identity,
                &state.package_id,
                &state.package_id,
                &state.module_path,
                &canonical_field_name,
                &[SymbolKind::Field],
                registry,
            )?;
            validate_span(field.definition_span, sources, "state-field")?;
            let field_source = sources.source(field.definition_span.file).ok_or(
                PackageArtifactIntegrityError::UnknownSourceFile {
                    file: field.definition_span.file,
                    location: "state-field",
                },
            )?;
            if field_source.key.as_ref().map(|key| key.package_id.as_str())
                != Some(&state.package_id)
                || field_source.module_path.as_deref() != Some(&state.module_path)
            {
                return Err(PackageArtifactIntegrityError::DebugSourceMismatch {
                    file: field.definition_span.file,
                    expected_identity: field_source.identity.clone(),
                    expected_module: field_source.module_path.clone(),
                    actual_package: state.package_id.clone(),
                    actual_module: state.module_path.clone(),
                });
            }
            expected_fields.insert(field.stable_id.0);
        }
        let actual_fields = bytecode_state
            .fields
            .iter()
            .map(|field| field.stable_id)
            .collect::<BTreeSet<_>>();
        if expected_fields != actual_fields {
            return Err(PackageArtifactIntegrityError::StateFieldSetMismatch {
                state: state.stable_id,
                expected: expected_fields,
                actual: actual_fields,
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_canonical_symbol(
    stable_id: StableSymbolId,
    canonical: &CanonicalSymbolIdentity,
    canonical_package_id: &str,
    package_id: &str,
    module_path: &str,
    name: &str,
    allowed_kinds: &[SymbolKind],
    registry: &mut BTreeMap<StableSymbolId, CanonicalSymbolIdentity>,
) -> Result<(), PackageArtifactIntegrityError> {
    if stable_id != canonical.runtime_id() {
        return Err(PackageArtifactIntegrityError::StableSymbolIdMismatch {
            canonical: Box::new(canonical.clone()),
            expected: canonical.runtime_id(),
            actual: stable_id,
        });
    }
    if canonical.package_id() != canonical_package_id || !allowed_kinds.contains(&canonical.kind())
    {
        return Err(
            PackageArtifactIntegrityError::CanonicalSymbolMetadataMismatch {
                canonical: Box::new(canonical.clone()),
                package_id: package_id.to_owned(),
                module_path: module_path.to_owned(),
                name: name.to_owned(),
            },
        );
    }
    if canonical.explicit_stable_name().is_none()
        && (canonical.module_path() != module_path || canonical.name() != name)
    {
        return Err(
            PackageArtifactIntegrityError::CanonicalSymbolMetadataMismatch {
                canonical: Box::new(canonical.clone()),
                package_id: package_id.to_owned(),
                module_path: module_path.to_owned(),
                name: name.to_owned(),
            },
        );
    }
    if let Some(first) = registry.insert(stable_id, canonical.clone())
        && first != *canonical
    {
        return Err(PackageArtifactIntegrityError::StableSymbolCollision {
            stable_id,
            first: Box::new(first),
            second: Box::new(canonical.clone()),
        });
    }
    Ok(())
}

fn validate_nonempty_span(
    span: SourceSpan,
    sources: &PackageSourceSnapshot,
    location: &'static str,
) -> Result<(), PackageArtifactIntegrityError> {
    validate_span(span, sources, location)?;
    if span.start == span.end {
        return Err(PackageArtifactIntegrityError::EmptySourceRange {
            file: span.file,
            start: span.start,
            location,
        });
    }
    Ok(())
}

fn validate_span(
    span: SourceSpan,
    sources: &PackageSourceSnapshot,
    location: &'static str,
) -> Result<(), PackageArtifactIntegrityError> {
    let source =
        sources
            .source(span.file)
            .ok_or(PackageArtifactIntegrityError::UnknownSourceFile {
                file: span.file,
                location,
            })?;
    let source_len = u32::try_from(source.text.len()).unwrap_or(u32::MAX);
    if span.start > span.end || span.end > source_len {
        return Err(PackageArtifactIntegrityError::InvalidSourceRange {
            file: span.file,
            start: span.start,
            end: span.end,
            source_len,
            location,
        });
    }
    let start = usize::try_from(span.start).unwrap_or(usize::MAX);
    let end = usize::try_from(span.end).unwrap_or(usize::MAX);
    if !source.text.is_char_boundary(start) || !source.text.is_char_boundary(end) {
        return Err(PackageArtifactIntegrityError::NonCharacterBoundary {
            file: span.file,
            start: span.start,
            end: span.end,
            location,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageArtifactIntegrityError {
    ReservedFileId,
    TooManySourceFiles,
    NonDenseFileId {
        expected: FileId,
        actual: FileId,
    },
    NonDeterministicSourceOrder,
    DuplicateFileId {
        file: FileId,
        first: SourceIdentity,
        second: SourceIdentity,
    },
    DuplicateSourceKey {
        key: SourceKey,
        first_file: FileId,
        second_file: FileId,
    },
    CompilerSourceIdentityMismatch {
        key: SourceKey,
        package: Option<String>,
        path: String,
    },
    InvalidExternalSourceIdentity {
        identity: SourceIdentity,
        package: Option<String>,
        module: Option<String>,
    },
    IncompletePackageSourceIdentity {
        identity: SourceIdentity,
    },
    DiagnosticSnapshot(SourceSnapshotRegistryError),
    InvalidModulePath(String),
    ModulePathMismatch {
        module: ModulePath,
        expected: NormalizedPackagePath,
        actual: NormalizedPackagePath,
    },
    BuildFingerprintMismatch {
        identity: BuildFingerprint,
        artifact: BuildFingerprint,
    },
    LinkedStateFingerprintMismatch {
        artifact: LinkedStateFingerprint,
        computed: LinkedStateFingerprint,
    },
    StateSchemaFingerprintMismatch {
        location: &'static str,
        artifact: StateSchemaFingerprint,
        observed: StateSchemaFingerprint,
    },
    DependencyRootMismatch {
        identity: PackageId,
        dependency_root: PackageId,
    },
    DebugRootMismatch {
        identity: PackageId,
        debug_root: String,
    },
    DependencyFingerprintSetMismatch {
        expected: BTreeSet<PackageId>,
        actual: BTreeSet<PackageId>,
    },
    SourceOutsideDependencyClosure(SourceKey),
    ProductContainsTestSource {
        identity: SourceIdentity,
    },
    ProductContainsTestModule {
        package: String,
        module: String,
    },
    TestArtifactMissingTestSource,
    TestArtifactMissingTestModule,
    DebugSourceMismatch {
        file: FileId,
        expected_identity: SourceIdentity,
        expected_module: Option<String>,
        actual_package: String,
        actual_module: String,
    },
    UnknownSourceFile {
        file: FileId,
        location: &'static str,
    },
    InvalidSourceRange {
        file: FileId,
        start: u32,
        end: u32,
        source_len: u32,
        location: &'static str,
    },
    NonCharacterBoundary {
        file: FileId,
        start: u32,
        end: u32,
        location: &'static str,
    },
    UnknownFunction {
        function: u32,
        location: &'static str,
    },
    InvalidProgramCounterRange {
        function: u32,
        start: u32,
        end: u32,
        code_len: u32,
    },
    MissingSourceMap {
        function: u32,
        pc: u32,
    },
    OverlappingSourceMap {
        function: u32,
        pc: u32,
    },
    DuplicateDebugFunction(u32),
    MissingDebugFunction(u32),
    DuplicateModuleFunctionOwnership {
        function: u32,
        first_package: String,
        first_module: String,
        second_package: String,
        second_module: String,
    },
    MissingModuleFunctionOwnership(u32),
    ModuleFunctionMismatch {
        function: u32,
        owner_package: String,
        owner_module: String,
        debug_package: String,
        debug_module: String,
    },
    HostDebugImportCountMismatch {
        expected: usize,
        actual: usize,
    },
    DuplicateHostDebugImport(u32),
    UnknownHostImport(u32),
    MissingHostDebugImport(u32),
    HostImportStableIdMismatch {
        import: u32,
        module: nexa_core::StableId,
        debug: nexa_core::StableId,
    },
    HostInterfaceDebugIdMismatch {
        import: u32,
        module: Option<nexa_core::StableId>,
        debug: nexa_core::StableId,
    },
    EmptyHostDebugName {
        import: u32,
    },
    HostDebugSourceMismatch {
        import: u32,
        interface_file: FileId,
        declaration_file: FileId,
    },
    HostDebugSourceIsPackage {
        import: u32,
        identity: SourceIdentity,
    },
    InvalidHostDebugSource {
        import: u32,
        identity: SourceIdentity,
    },
    HostDebugDeclarationMismatch {
        import: u32,
    },
    EmptySourceRange {
        file: FileId,
        start: u32,
        location: &'static str,
    },
    StableSymbolIdMismatch {
        canonical: Box<CanonicalSymbolIdentity>,
        expected: StableSymbolId,
        actual: StableSymbolId,
    },
    CanonicalSymbolMetadataMismatch {
        canonical: Box<CanonicalSymbolIdentity>,
        package_id: String,
        module_path: String,
        name: String,
    },
    StableSymbolCollision {
        stable_id: StableSymbolId,
        first: Box<CanonicalSymbolIdentity>,
        second: Box<CanonicalSymbolIdentity>,
    },
    StateSurfaceCountMismatch {
        module: usize,
        debug: usize,
    },
    MissingBytecodeStateType(StableSymbolId),
    StateVersionMismatch {
        state: StableSymbolId,
        module: u32,
        debug: u32,
    },
    StateFieldSetMismatch {
        state: StableSymbolId,
        expected: BTreeSet<nexa_core::StableId>,
        actual: BTreeSet<nexa_core::StableId>,
    },
}

impl fmt::Display for PackageArtifactIntegrityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PackageArtifactIntegrityError {}

#[cfg(test)]
mod tests {
    use nexa_analysis::{
        ModulePath, NormalizedPackagePath, ResolvedBuildInput, ResolvedPackage, ResolvedTestInput,
        SourceId,
    };
    use nexa_bytecode::{
        FunctionBuilder, HostCallMode, HostImport, Instruction, ModuleBuilder, Signature,
        SourceMapEntry,
    };
    use nexa_compiler::{
        PackageFunctionDebugInfo, PackageHostImportDebugInfo, PackageModuleDebugInfo,
        PackageVisibility,
    };
    use nexa_core::{CanonicalSymbolIdentity, SymbolKind};
    use nexa_diagnostics::ErrorCode;

    use super::*;

    fn key(path: &str) -> SourceKey {
        SourceKey::new(
            PackageId::new("example.app").unwrap(),
            NormalizedPackagePath::new(path).unwrap(),
        )
    }

    fn snapshot() -> PackageSourceSnapshot {
        PackageSourceSnapshot::new([CompiledSource {
            file: FileId(1),
            key: Some(key("src/main.nexa")),
            identity: SourceIdentity::package("example.app", "src/main.nexa"),
            module_path: Some(ModulePath::new("main").unwrap().to_string()),
            text: Arc::from("module main;\nfn main() {}\n"),
            compiler_provided: false,
        }])
        .unwrap()
    }

    fn snapshot_with_host() -> PackageSourceSnapshot {
        let host: Arc<str> = Arc::from("interface Host { sync fn ping() -> i32; }\n");
        PackageSourceSnapshot::new([
            CompiledSource {
                file: FileId(1),
                key: Some(key("src/main.nexa")),
                identity: SourceIdentity::package("example.app", "src/main.nexa"),
                module_path: Some(ModulePath::new("main").unwrap().to_string()),
                text: Arc::from("module main;\nfn main() {}\n"),
                compiler_provided: false,
            },
            CompiledSource {
                file: FileId(2),
                key: None,
                identity: SourceIdentity::standalone("contracts/app api.nidl"),
                module_path: None,
                text: host,
                compiler_provided: false,
            },
        ])
        .unwrap()
    }

    fn manifest_and_sources() -> (PackageManifest, PackageSourceSet) {
        let manifest = PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.app"
name = "Example"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "default-enabled"
"#,
        )
        .unwrap();
        let mut sources = nexa_analysis::SourceSetBuilder::new(
            manifest.id.clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        sources
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "module main;\nfn main() {}\n",
                nexa_analysis::SourceRole::Production,
            )
            .unwrap();
        (manifest, sources.build().unwrap())
    }

    fn library_manifest_and_sources(source: &str) -> (PackageManifest, PackageSourceSet) {
        let manifest = PackageManifest::parse(
            r#"
schema = 2
kind = "library"
id = "example.app"
name = "Example"
version = "1.0.0"
source_root = "src"
"#,
        )
        .unwrap();
        let mut sources = nexa_analysis::SourceSetBuilder::new(
            manifest.id.clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        sources
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                source,
                nexa_analysis::SourceRole::Production,
            )
            .unwrap();
        (manifest, sources.build().unwrap())
    }

    fn resolved_input(
        manifest: PackageManifest,
        sources: PackageSourceSet,
        contract: &nexa_idl::Idl,
    ) -> ResolvedBuildInput {
        let source_id = SourceId::new("package-build-test").unwrap();
        let directory = NormalizedPackagePath::new("packages/example").unwrap();
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id,
                    directory,
                    kind: manifest.kind,
                },
            )]),
            edges: BTreeSet::new(),
        });
        let fingerprint_input = canonical_package_build_fingerprint_input(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            contract,
            None,
        );
        ResolvedBuildInput::new(
            Arc::new(manifest),
            Arc::new(sources),
            BTreeMap::new(),
            BTreeMap::new(),
            graph,
            None,
            Arc::<[u8]>::from(nexa_idl::canonical(contract).into_bytes()),
            canonical_host_contract_source_identity(&HostContractInput::canonical(contract)),
            fingerprint_input.host_required_exports.clone(),
            nexa_analysis::CompilationOptions::default(),
            fingerprint_input,
        )
        .unwrap()
    }

    fn canonical_artifact() -> (CompiledPackageArtifact, ResolvedBuildInput, nexa_idl::Idl) {
        let manifest = PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.app"
name = "Example"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
        )
        .unwrap();
        let mut builder = nexa_analysis::SourceSetBuilder::new(
            manifest.id.clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                concat!(
                    "module main;\n",
                    "@stable(\"root-state\")\n",
                    "@stateful(1)\n",
                    "pub class Root { value: i32; }\n",
                    "pub fn value() -> i32 { return 7; }\n",
                ),
                nexa_analysis::SourceRole::Production,
            )
            .unwrap();
        let contract = nexa_idl::parse("interface Empty {}").unwrap();
        let input = resolved_input(manifest, builder.build().unwrap(), &contract);
        let identity =
            CandidateIdentity::new(input.root_manifest.id.clone(), 1, input.build_fingerprint)
                .unwrap();
        let artifact = compile_package(&input, &contract, identity).unwrap();
        (artifact, input, contract)
    }

    #[test]
    fn library_check_rejects_both_directions_of_unit_return_mismatch() {
        let contract = nexa_idl::parse("interface Empty {}").unwrap();
        for (source, expected_message) in [
            (
                "module main;\nfn bad() -> i32 { return; }\n",
                "expected i32, found unit",
            ),
            (
                "module main;\nfn bad() -> unit { return 1; }\n",
                "expected unit, found i32",
            ),
        ] {
            let (manifest, sources) = library_manifest_and_sources(source);
            let input = resolved_input(manifest, sources, &contract);
            let error = PackageBuildSession::new()
                .check_package(&input, &contract)
                .unwrap_err();
            let PackageBuildError::AnalysisFailed(diagnostics) = error else {
                panic!("library mismatch must fail semantic analysis: {error}");
            };
            let matching = diagnostics
                .diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.code == ErrorCode::NX2101)
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1, "{matching:#?}");
            assert_eq!(matching[0].message.as_ref(), expected_message);
        }
    }

    fn linked_for(
        artifact: &CompiledPackageArtifact,
        build: BuildFingerprint,
        module: &nexa_bytecode::Module,
        closure: &ResolvedDependencyGraph,
    ) -> LinkedStateFingerprint {
        linked_state_fingerprint(
            build,
            module,
            &artifact.source_files,
            &artifact.debug_info,
            &artifact.state_surface,
            artifact.compilation_evidence,
            artifact.source_set_fingerprint,
            artifact.public_api_fingerprint,
            artifact.state_schema_fingerprint,
            closure,
            &artifact.dependency_source_fingerprints,
        )
    }

    fn same_length_ascii_mutation(value: &str) -> String {
        let mut bytes = value.as_bytes().to_vec();
        let byte = bytes
            .iter_mut()
            .find(|byte| byte.is_ascii_alphanumeric())
            .expect("fixture contains an ASCII identity byte");
        *byte = if *byte == b'x' { b'y' } else { b'x' };
        String::from_utf8(bytes).expect("an ASCII byte mutation remains UTF-8")
    }

    fn exact_host_build_fingerprint(
        input: &ResolvedBuildInput,
        contract: &nexa_idl::Idl,
    ) -> BuildFingerprint {
        let exact_host = HostContractInput::with_source(
            contract,
            SourceIdentity::standalone("contracts/relocated-empty.nidl"),
            nexa_idl::canonical_source(contract),
        )
        .unwrap();
        canonical_package_build_fingerprint_input_with_contract(
            &input.root_manifest,
            &input.root_source_set,
            &input.dependency_manifests,
            &input.dependency_source_sets,
            &exact_host,
            input.lock.as_deref(),
        )
        .fingerprint()
    }

    fn changed_options_input(input: &ResolvedBuildInput) -> ResolvedBuildInput {
        let changed_options = nexa_analysis::CompilationOptions {
            max_while_iterations: 17,
            ..input.compilation_options
        };
        let mut fingerprint = input.fingerprint_input.as_ref().clone();
        fingerprint.compiler_options =
            nexa_analysis::canonical_compilation_options(&changed_options);
        ResolvedBuildInput::new(
            Arc::clone(&input.root_manifest),
            Arc::clone(&input.root_source_set),
            input.dependency_manifests.as_ref().clone(),
            input.dependency_source_sets.as_ref().clone(),
            Arc::clone(&input.dependency_graph),
            input.lock.clone(),
            Arc::clone(&input.canonical_host_contract),
            Arc::clone(&input.host_contract_source_identity),
            Arc::clone(&input.host_required_exports_identity),
            changed_options,
            fingerprint,
        )
        .unwrap()
    }

    fn changed_options_build_fingerprint(input: &ResolvedBuildInput) -> BuildFingerprint {
        changed_options_input(input).build_fingerprint
    }

    fn debug_info(span: SourceSpan) -> PackageDebugInfo {
        let identity =
            CanonicalSymbolIdentity::automatic("example.app", "main", SymbolKind::Function, "main");
        PackageDebugInfo {
            root_package_id: "example.app".into(),
            entry_module: "main".into(),
            modules: vec![PackageModuleDebugInfo {
                package_id: "example.app".into(),
                module_path: "main".into(),
                file: span.file,
                definition_span: span,
                source_span: SourceSpan::new(span.file, 0, 26),
                function_indices: vec![0],
            }],
            functions: vec![PackageFunctionDebugInfo {
                function_index: 0,
                package_id: "example.app".into(),
                module_path: "main".into(),
                name: "main".into(),
                canonical_identity: identity.clone(),
                stable_id: identity.runtime_id(),
                definition_span: span,
                effect: nexa_bytecode::FunctionEffect::Ordinary,
                visibility: PackageVisibility::Private,
            }],
            host_imports: Vec::new(),
        }
    }

    #[test]
    fn snapshot_rejects_duplicate_numeric_and_stable_source_identity() {
        let base = CompiledSource {
            file: FileId(1),
            key: Some(key("src/main.nexa")),
            identity: SourceIdentity::package("example.app", "src/main.nexa"),
            module_path: Some("main".into()),
            text: Arc::from("module main;"),
            compiler_provided: false,
        };
        let mut duplicate_file = base.clone();
        duplicate_file.key = Some(key("src/other.nexa"));
        duplicate_file.identity = SourceIdentity::package("example.app", "src/other.nexa");
        duplicate_file.module_path = Some("other".into());
        assert!(matches!(
            PackageSourceSnapshot::new([base.clone(), duplicate_file]),
            Err(PackageArtifactIntegrityError::DuplicateFileId { .. })
        ));
        let mut duplicate_key = base.clone();
        duplicate_key.file = FileId(2);
        assert!(matches!(
            PackageSourceSnapshot::new([base, duplicate_key]),
            Err(PackageArtifactIntegrityError::DuplicateSourceKey { .. })
        ));
    }

    #[test]
    fn source_map_and_debug_spans_must_resolve_to_exact_utf8_bytes() {
        let mut builder = ModuleBuilder::new();
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        function.emit(Instruction::ReturnVoid);
        let function = function.finish().unwrap();
        builder.function(function);
        builder.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: SourceSpan::new(FileId(1), 13, 26),
        }]);
        let module = builder.finish();
        let sources = snapshot();
        verify_package_artifact_integrity(
            &module,
            &sources,
            &debug_info(SourceSpan::new(FileId(1), 13, 26)),
        )
        .unwrap();

        let mut bad = module;
        bad.source_map[0].span = SourceSpan::new(FileId(9), 0, 1);
        assert!(matches!(
            verify_package_artifact_integrity(
                &bad,
                &sources,
                &debug_info(SourceSpan::new(FileId(1), 13, 26))
            ),
            Err(PackageArtifactIntegrityError::UnknownSourceFile { .. })
        ));
    }

    #[test]
    fn package_source_map_requires_exactly_one_mapping_for_every_pc() {
        let sources = snapshot();
        let span = SourceSpan::new(FileId(1), 13, 26);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        function
            .emit(Instruction::Safepoint)
            .emit(Instruction::ReturnVoid);
        let function = function.finish().unwrap();

        let mut gap_builder = ModuleBuilder::new();
        gap_builder.function(function.clone());
        gap_builder.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span,
        }]);
        assert!(matches!(
            verify_package_artifact_integrity(&gap_builder.finish(), &sources, &debug_info(span)),
            Err(PackageArtifactIntegrityError::MissingSourceMap { function: 0, pc: 1 })
        ));

        let mut overlap_builder = ModuleBuilder::new();
        overlap_builder.function(function);
        overlap_builder.source_map([
            SourceMapEntry {
                function: 0,
                pc_start: 0,
                pc_end: 2,
                span,
            },
            SourceMapEntry {
                function: 0,
                pc_start: 1,
                pc_end: 2,
                span,
            },
        ]);
        assert!(matches!(
            verify_package_artifact_integrity(
                &overlap_builder.finish(),
                &sources,
                &debug_info(span)
            ),
            Err(PackageArtifactIntegrityError::OverlappingSourceMap { function: 0, pc: 1 })
        ));
    }

    #[test]
    fn host_debug_table_must_exactly_cover_bytecode_imports_and_external_source() {
        let mut builder = ModuleBuilder::new();
        let interface_id = nexa_core::StableId::from_name("Host");
        let import_id = nexa_core::StableId::from_parts(&["Host", "::", "ping"]);
        builder
            .metadata(
                interface_id,
                nexa_analysis::StateSchemaFingerprint::from_bytes([0; 32]),
            )
            .host_import(HostImport {
                stable_id: import_id,
                parameters: Vec::new(),
                result: Some(nexa_bytecode::ValueType::I32),
                mode: HostCallMode::Immediate,
                fuel_cost: 1,
                async_result: None,
            });
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        function.emit(Instruction::ReturnVoid);
        builder.function(function.finish().unwrap());
        builder.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: SourceSpan::new(FileId(1), 13, 26),
        }]);
        let module = builder.finish();
        let sources = snapshot_with_host();
        let mut debug = debug_info(SourceSpan::new(FileId(1), 13, 26));
        let host_source = sources.source(FileId(2)).unwrap();
        let (_, host_map) = nexa_idl::parse_with_source_map(&host_source.text).unwrap();
        let function_source = host_map.functions.get("ping").unwrap();
        debug.host_imports.push(PackageHostImportDebugInfo {
            import_index: 0,
            stable_id: import_id,
            interface_id,
            interface_name: "Host".into(),
            function_name: "ping".into(),
            interface_span: SourceSpan::new(
                FileId(2),
                u32::try_from(host_map.interface.declaration_start).unwrap(),
                u32::try_from(host_map.interface.declaration_end).unwrap(),
            ),
            declaration_span: SourceSpan::new(
                FileId(2),
                u32::try_from(function_source.declaration_start).unwrap(),
                u32::try_from(function_source.declaration_end).unwrap(),
            ),
        });
        verify_package_artifact_integrity(&module, &sources, &debug).unwrap();

        debug.host_imports[0].declaration_span.start += 1;
        assert!(matches!(
            verify_package_artifact_integrity(&module, &sources, &debug),
            Err(PackageArtifactIntegrityError::HostDebugDeclarationMismatch { .. })
        ));
        debug.host_imports[0].declaration_span.start -= 1;
        debug.host_imports[0].stable_id = nexa_core::StableId::from_name("wrong");
        assert!(matches!(
            verify_package_artifact_integrity(&module, &sources, &debug),
            Err(PackageArtifactIntegrityError::HostImportStableIdMismatch { .. })
        ));
    }

    #[test]
    fn build_identity_includes_canonical_standard_library_descriptor() {
        let (manifest, sources) = manifest_and_sources();
        let contract = nexa_idl::parse("interface Empty {}").unwrap();
        let first = canonical_package_build_fingerprint_input(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &contract,
            None,
        );
        let second = canonical_package_build_fingerprint_input(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &contract,
            None,
        );
        assert!(!first.standard_library_descriptor.is_empty());
        assert_eq!(
            first.standard_library_descriptor,
            second.standard_library_descriptor
        );
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn required_export_view_changes_identity_without_changing_the_complete_contract() {
        let (manifest, sources) = manifest_and_sources();
        let source: Arc<str> = Arc::from(
            "interface Host {\n\
             export Run() -> i32;\n\
             export Reset() -> void;\n\
             }\n",
        );
        let idl = nexa_idl::parse(&source).unwrap();
        let base = HostContractInput::with_source(
            &idl,
            SourceIdentity::standalone("contracts/host.nidl"),
            Arc::clone(&source),
        )
        .unwrap();
        let run_then_reset = base
            .requiring_exports(&["Run".to_owned(), "Reset".to_owned()])
            .unwrap();
        let reset_then_run = base
            .requiring_exports(&["Reset".to_owned(), "Run".to_owned()])
            .unwrap();
        let run_only = base.requiring_exports(&["Run".to_owned()]).unwrap();
        let full = canonical_package_build_fingerprint_input_with_contract(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &run_then_reset,
            None,
        );
        let reordered = canonical_package_build_fingerprint_input_with_contract(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &reset_then_run,
            None,
        );
        let subset = canonical_package_build_fingerprint_input_with_contract(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &run_only,
            None,
        );

        assert_eq!(full.host_contract, subset.host_contract);
        assert_eq!(full.host_contract_source, subset.host_contract_source);
        assert_eq!(full.host_required_exports, reordered.host_required_exports);
        assert_eq!(full.fingerprint(), reordered.fingerprint());
        assert_ne!(full.host_required_exports, subset.host_required_exports);
        assert_ne!(full.fingerprint(), subset.fingerprint());
        assert_eq!(
            nexa_idl::exact_hash(base.idl()),
            nexa_idl::exact_hash(run_only.idl())
        );
    }

    #[test]
    fn package_fingerprint_uses_canonical_build_authorities() {
        let (manifest, sources) = manifest_and_sources();
        let contract = nexa_idl::parse("interface Empty {}").unwrap();
        let fingerprint = canonical_package_build_fingerprint_input(
            &manifest,
            &sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &contract,
            None,
        );
        let options = nexa_analysis::CompilationOptions::default();

        assert_eq!(
            fingerprint.runtime_semantics_version,
            u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION)
        );
        assert_eq!(
            fingerprint.opcode_cost_table_version,
            nexa_core::OPCODE_COST_TABLE_VERSION
        );
        assert_eq!(
            fingerprint.deterministic_math_backend,
            nexa_core::RUNTIME_MATH_BACKEND_ID
        );
        assert_eq!(
            fingerprint.compiler_options,
            nexa_analysis::canonical_compilation_options(&options)
        );
        assert_eq!(
            fingerprint.compiler_options,
            canonical_compilation_options()
        );
        assert_eq!(
            fingerprint.standard_library_descriptor,
            nexa_stdlib::canonical_descriptor_identity()
        );
        assert_eq!(
            fingerprint.language_version,
            nexa_analysis::NEXA_LANGUAGE_VERSION
        );
        assert_eq!(
            fingerprint.standard_library_version,
            nexa_stdlib::standard_library().version.to_string()
        );
        assert_eq!(
            fingerprint.compiler_version,
            nexa_core::NEXA_COMPILER_VERSION
        );
        assert_eq!(
            fingerprint.bytecode_version,
            u32::from(nexa_core::BYTECODE_VERSION)
        );
        assert_eq!(
            COMPILATION_OPTIONS_SCHEMA_VERSION,
            nexa_analysis::COMPILATION_OPTIONS_SCHEMA_VERSION
        );
    }

    #[test]
    fn facade_rejects_noncanonical_options_and_post_new_source_replacement() {
        let (_artifact, input, contract) = canonical_artifact();
        let changed_options = changed_options_input(&input);
        let changed_identity = CandidateIdentity::new(
            changed_options.root_package().clone(),
            1,
            changed_options.build_fingerprint,
        )
        .unwrap();
        assert!(matches!(
            compile_package(&changed_options, &contract, changed_identity),
            Err(PackageBuildError::CompilationOptionsMismatch)
        ));

        let mut changed_source_builder = nexa_analysis::SourceSetBuilder::new(
            input.root_package().clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        changed_source_builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "module main;\npub fn changed() -> i32 { return 1; }\n",
                nexa_analysis::SourceRole::Production,
            )
            .unwrap();
        let mut tampered = input.clone();
        tampered.root_source_set = Arc::new(changed_source_builder.build().unwrap());
        assert!(matches!(
            check_package(&tampered, &contract),
            Err(PackageBuildError::InvalidResolvedInput(
                nexa_analysis::ResolvedBuildInputError::FingerprintRootSourceMismatch
            ))
        ));
    }

    #[test]
    fn canonical_facade_runs_analysis_typed_codegen_and_verifier() {
        let (artifact, _input, contract) = canonical_artifact();
        assert!(!artifact.encode_module().is_empty());
        assert_eq!(
            artifact.module().host_interface_hash,
            Some(nexa_idl::exact_hash(&contract))
        );
        assert!(artifact.source_files.files().iter().any(|source| {
            source.key.is_none()
                && source.identity.path() == crate::package_environment::CANONICAL_HOST_SOURCE_PATH
        }));
    }

    #[test]
    fn test_artifact_runtime_schema_is_exactly_the_production_schema() {
        let manifest = PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.app"
name = "Example"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
        )
        .unwrap();
        let mut production = nexa_analysis::SourceSetBuilder::new(
            manifest.id.clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        production
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "module main;\n@stateful(1) pub class ProductState { value: i32; }\npub fn value() -> i32 { return 7; }\n",
                nexa_analysis::SourceRole::Production,
            )
            .unwrap();
        let contract = nexa_idl::parse("interface Empty {}").unwrap();
        let input = Arc::new(resolved_input(
            manifest,
            production.build().unwrap(),
            &contract,
        ));
        let mut tests = nexa_analysis::SourceSetBuilder::new(
            input.root_manifest.id.clone(),
            nexa_analysis::CompilationLimits::default(),
        );
        tests
            .add(
                NormalizedPackagePath::new("tests/checks.nexa").unwrap(),
                "module test.checks;\n@stateful(99) pub class TestOnlyState { ignored: string; }\n@test fn succeeds() -> bool { return true; }\n",
                nexa_analysis::SourceRole::Test,
            )
            .unwrap();
        let test_input =
            ResolvedTestInput::new(Arc::clone(&input), Arc::new(tests.build().unwrap())).unwrap();
        let identity =
            CandidateIdentity::new(input.root_package().clone(), 1, input.build_fingerprint)
                .unwrap();
        let mut session = PackageBuildSession::new();
        let product = session
            .compile_package(&input, &contract, identity.clone())
            .expect("production artifact");
        let test = session
            .compile_package_tests(&test_input, &contract, identity)
            .expect("test artifact");
        let test_module = test.verified.module();

        assert_eq!(test_module.state_schema, product.module().state_schema);
        assert_eq!(
            test_module.state_schema_fingerprint,
            product.module().state_schema_fingerprint
        );
        assert_eq!(
            test_module.reload_metadata.state_schema_fingerprint,
            product.module().reload_metadata.state_schema_fingerprint
        );
        assert_eq!(test_module.state_schema.types.len(), 1);
    }

    #[test]
    fn linked_state_fingerprint_binds_codegen_closure_host_and_options() {
        let (artifact, input, contract) = canonical_artifact();
        let baseline = artifact.linked_state_fingerprint;
        assert_eq!(
            baseline,
            linked_for(
                &artifact,
                artifact.build_fingerprint,
                artifact.module(),
                &artifact.dependency_closure,
            )
        );

        let mut changed_module = artifact.module().clone();
        changed_module
            .strings
            .push("compiler-output-mutation".into());
        assert_ne!(
            baseline,
            linked_for(
                &artifact,
                artifact.build_fingerprint,
                &changed_module,
                &artifact.dependency_closure,
            ),
            "linked identity must catch output drift under an unchanged BuildFingerprint"
        );

        assert_ne!(
            baseline,
            linked_for(
                &artifact,
                exact_host_build_fingerprint(&input, &contract),
                artifact.module(),
                &artifact.dependency_closure,
            ),
            "a real Host URI mutation must change linked identity"
        );

        assert_ne!(
            baseline,
            linked_for(
                &artifact,
                changed_options_build_fingerprint(&input),
                artifact.module(),
                &artifact.dependency_closure,
            ),
            "a validated effective compiler-option mutation must change linked identity"
        );

        let mut changed_closure = artifact.dependency_closure.as_ref().clone();
        let changed_root = changed_closure.root.clone();
        changed_closure
            .packages
            .get_mut(&changed_root)
            .unwrap()
            .directory = NormalizedPackagePath::new("packages/relocated").unwrap();
        assert_ne!(
            baseline,
            linked_for(
                &artifact,
                artifact.build_fingerprint,
                artifact.module(),
                &changed_closure,
            )
        );

        let mut evidence_tampered = artifact.clone();
        evidence_tampered.compilation_evidence.package_symbols = evidence_tampered
            .compilation_evidence
            .package_symbols
            .saturating_add(1);
        assert_ne!(
            baseline,
            linked_for(
                &evidence_tampered,
                evidence_tampered.build_fingerprint,
                evidence_tampered.module(),
                &evidence_tampered.dependency_closure,
            ),
            "linked identity must bind the actual compiled closure cardinalities"
        );
        assert!(matches!(
            evidence_tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));

        let mut tampered = artifact;
        tampered.linked_state_fingerprint = LinkedStateFingerprint::default();
        assert!(matches!(
            tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));
    }

    #[test]
    fn linked_state_fingerprint_binds_exact_source_and_debug_registries() {
        let (artifact, _, _) = canonical_artifact();
        let baseline = artifact.linked_state_fingerprint;

        let mut uri_tampered = artifact.clone();
        let mut sources = uri_tampered.source_files.files().to_vec();
        let host = sources
            .iter_mut()
            .find(|source| source.key.is_none())
            .expect("canonical artifact retains the exact Host source");
        let changed_uri = same_length_ascii_mutation(host.identity.path());
        assert_eq!(changed_uri.len(), host.identity.path().len());
        host.identity = SourceIdentity::standalone(changed_uri);
        uri_tampered.source_files = PackageSourceSnapshot::new(sources).unwrap();
        assert_ne!(
            baseline,
            linked_for(
                &uri_tampered,
                uri_tampered.build_fingerprint,
                uri_tampered.module(),
                &uri_tampered.dependency_closure,
            )
        );
        assert!(matches!(
            uri_tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));

        let mut text_tampered = artifact.clone();
        let mut sources = text_tampered.source_files.files().to_vec();
        let host = sources
            .iter_mut()
            .find(|source| source.key.is_none())
            .expect("canonical artifact retains the exact Host source");
        let changed_text = same_length_ascii_mutation(&host.text);
        assert_eq!(changed_text.len(), host.text.len());
        host.text = Arc::from(changed_text);
        text_tampered.source_files = PackageSourceSnapshot::new(sources).unwrap();
        assert_ne!(
            baseline,
            linked_for(
                &text_tampered,
                text_tampered.build_fingerprint,
                text_tampered.module(),
                &text_tampered.dependency_closure,
            )
        );
        assert!(matches!(
            text_tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));

        let mut debug_tampered = artifact.clone();
        let changed_entry = same_length_ascii_mutation(&debug_tampered.debug_info.entry_module);
        assert_eq!(
            changed_entry.len(),
            debug_tampered.debug_info.entry_module.len()
        );
        debug_tampered.debug_info.entry_module = changed_entry;
        assert_ne!(
            baseline,
            linked_for(
                &debug_tampered,
                debug_tampered.build_fingerprint,
                debug_tampered.module(),
                &debug_tampered.dependency_closure,
            )
        );
        assert!(matches!(
            debug_tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));

        let mut state_surface_tampered = artifact;
        let mut state_surface = state_surface_tampered.state_surface.to_vec();
        let state = state_surface
            .first_mut()
            .expect("canonical artifact exposes its Stateful type");
        assert_eq!(
            state.canonical_identity.explicit_stable_name(),
            Some("root-state")
        );
        let changed_name = same_length_ascii_mutation(&state.name);
        assert_eq!(changed_name.len(), state.name.len());
        state.name = changed_name;
        state_surface_tampered.state_surface = Arc::from(state_surface);
        assert_ne!(
            baseline,
            linked_for(
                &state_surface_tampered,
                state_surface_tampered.build_fingerprint,
                state_surface_tampered.module(),
                &state_surface_tampered.dependency_closure,
            )
        );
        assert!(matches!(
            state_surface_tampered.verify_integrity(),
            Err(PackageArtifactIntegrityError::LinkedStateFingerprintMismatch { .. })
        ));
    }
}
