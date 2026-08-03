//! High-level, package-oriented embedding API for trusted Nexa content.

mod artifact;
mod builder;
mod capability;
mod contract;
mod development;
mod diagnostic;
mod diagnostic_evidence;
mod directory_source;
mod dispatch;
mod entitlement;
#[cfg(test)]
mod freshness_tests;
mod inspection;
mod lifecycle;
mod manifest;
mod memory_source;
mod package;
mod persistence;
mod policy;
mod source;
mod source_file;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

// Keep implementation paths descriptive while routing every public runtime/IDL/core type through
// the stable `nexa` facade.
use nexa as nexa_diagnostics;
use nexa as nexa_idl;
use nexa as nexa_verifier;
use nexa::prelude as nexa_core;
use nexa::prelude as nexa_runtime;

pub use artifact::{
    CandidateCompilation, CompiledPackageArtifact, LastKnownGood, PackageDebugInspection,
    PackageFunctionInspection, PackageHostImportInspection, PackageModuleInspection,
    compile_package,
};
pub use builder::NexaEngineBuilder;
pub use capability::CapabilitySet;
pub use contract::{ExportRequirement, HostContract};
pub use development::{
    CandidateCancellation, CandidateTerminal, CandidateTerminalData, CandidateTerminalKind,
    CompileJob, DevelopmentCompileRequest, DevelopmentCompiler, DevelopmentConfig,
    DevelopmentEvent, DevelopmentEventData, DevelopmentState, EnqueueOutcome, WorkerEvent,
    WorkerInspection,
};
pub use diagnostic::{
    DiagnosticRenderer, EngineDiagnostic, EngineDiagnosticContext, EngineDiagnosticStage,
    EngineDiagnosticSummary, EngineSourceSnapshot, EngineTaskId, RelatedDiagnostic,
};
pub use diagnostic_evidence::{
    EngineDiagnosticEvidence, EngineDiagnosticObservation, EngineRenderEvidence,
    run_engine_diagnostic_cases, run_engine_diagnostic_evidence,
};
pub use directory_source::DirectorySource;
pub use entitlement::{EntitlementResolver, NoEntitlements, StaticEntitlements};
pub use inspection::{
    DevelopmentInspection, EngineInspection, EngineTickReport, EntrypointSignature,
    PackageInspection, PackageMetric, ReloadReport, ReloadReportOutcome, ReloadReportSummary,
};
pub use lifecycle::{LifecycleError, PackageLifecycle, PackageStatus};
pub use manifest::{
    CapabilityId, EntitlementId, ManifestError, PackageId, PackageManifest, PackageVersion,
    SourceId,
};
pub use memory_source::{MemoryPackage, MemorySource};
pub use nexa::SourceIdentity;
pub use nexa_analysis::{
    BuildFingerprint, CandidateIdentity, CompilationLimits, DependencyAlias,
    LinkedStateFingerprint, ModulePath, NormalizedPackagePath, PackageKind, PublicApiFingerprint,
    ResolvedBuildInput, ResolvedDependencyGraph, SourceKey, SourceSetFingerprint,
    StateSchemaFingerprint,
};
pub use package::{EngineHealth, PackageInfo, PackageOutput};
pub use policy::{
    ActivationPolicy, ActivationSet, PackagePolicy, PackageRuntimeLimits, TrustLevel,
};
pub use source::{
    CandidateBuildContext, DiscoveredPackage, PackageCandidate, PackageSource, PackageSourceError,
};
pub use source_file::{
    SourceFile, SourceFileRegistry, SourceFileRegistryError, SourcePosition, SourceRange,
};

use development::DevelopmentWorker;
use diagnostic::BoundedDiagnosticLog;
use package::{PackageRecord, PackageRuntime};

#[derive(Clone, Debug)]
pub struct PackageContext {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub trust: TrustLevel,
    pub capabilities: CapabilitySet,
    pub data_namespace: String,
    pub version: PackageVersion,
}

pub trait HostRegistryFactory {
    fn create(&self, context: &PackageContext) -> Box<dyn nexa_runtime::HostRegistry>;
}

impl<F> HostRegistryFactory for F
where
    F: for<'a> Fn(&'a PackageContext) -> Box<dyn nexa_runtime::HostRegistry>,
{
    fn create(&self, context: &PackageContext) -> Box<dyn nexa_runtime::HostRegistry> {
        self(context)
    }
}

/// WP89 effect satisfaction shared by every engine export check: exact
/// match, or a module that strengthens an Ordinary declaration to
/// `@immediate` (same synchronous ABI, strictly fewer rights - the
/// verifier rejects suspension points inside Immediate functions). No
/// other effect pair is substitutable.
pub(crate) fn effect_satisfies_declaration(
    found: nexa_runtime::FunctionEffect,
    declared: nexa_runtime::FunctionEffect,
) -> bool {
    found == declared
        || (declared == nexa_runtime::FunctionEffect::Ordinary
            && found == nexa_runtime::FunctionEffect::Immediate)
}

pub struct NexaEngine {
    contract: HostContract,
    idl: nexa_idl::ValidatedContract,
    host_contract_source_identity: nexa::SourceIdentity,
    host_contract_source: std::sync::Arc<str>,
    host_factory: Box<dyn HostRegistryFactory>,
    sources: Vec<Box<dyn PackageSource>>,
    entitlements: Box<dyn EntitlementResolver>,
    storage_dir: Option<PathBuf>,
    runtime_host: nexa_runtime::RuntimeHost,
    packages: Vec<PackageRecord>,
    /// WP90: the cached deterministic broadcast order - indexes of Enabled
    /// packages, highest priority first, package id as the tie-break.
    /// Never trusted blindly: every dispatch revalidates it against the
    /// live package table in O(n) without allocating, so any lifecycle or
    /// priority change simply rebuilds the plan in place.
    dispatch_plan: Vec<usize>,
    diagnostics: BoundedDiagnosticLog,
    required_exports: Vec<ExportRequirement>,
    declared_entrypoints: Vec<EntrypointSignature>,
    persisted: BTreeMap<PackageId, bool>,
    development: DevelopmentConfig,
    development_coordinator: nexa_analysis::DevelopmentCoordinator,
    build_session: development::SharedPackageBuildSession,
    development_worker: Option<DevelopmentWorker>,
    development_events: VecDeque<DevelopmentEvent>,
    pending_development_events: Vec<DevelopmentEvent>,
    reload_reports: VecDeque<ReloadReport>,
    pending_reload_reports: Vec<ReloadReport>,
    dropped_events: u64,
    last_tick_diagnostic_sequence: u64,
    ticks: u64,
    next_realm_id: u32,
    delivered_release_records: u64,
    discovered: bool,
    shutdown: bool,
}

impl NexaEngine {
    #[must_use]
    pub fn builder(contract: HostContract) -> NexaEngineBuilder {
        NexaEngineBuilder::new(contract)
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn from_builder(builder: NexaEngineBuilder) -> Result<Self, EngineError> {
        let idl = nexa_idl::parse_nidl(builder.contract.source())
            .map_err(|error| EngineError::Contract(error.to_string()))?;
        let descriptor = nexa_idl::abi_descriptor(&idl);
        if idl.name != builder.contract.contract_name()
            || descriptor.as_bytes() != builder.contract.canonical_descriptor()
            || descriptor.fingerprint.into_bytes() != builder.contract.contract_fingerprint()
            || nexa_idl::contract_runtime_id(&idl) != builder.contract.contract_runtime_id()
            || builder.contract.generator_schema_version()
                != nexa_runtime::HOST_CONTRACT_SCHEMA_VERSION
        {
            return Err(EngineError::Contract(
                "generated Host Contract descriptor mismatch".into(),
            ));
        }
        let declared_entrypoints = idl
            .nexa_functions
            .iter()
            .map(|entrypoint| EntrypointSignature {
                name: entrypoint.name.clone(),
                stable_id: nexa_idl::entrypoint_stable_id(entrypoint),
                signature: nexa_idl::entrypoint_signature(entrypoint),
                effect: if entrypoint.is_async {
                    nexa_runtime::FunctionEffect::Task
                } else {
                    nexa_runtime::FunctionEffect::Ordinary
                },
            })
            .collect::<Vec<_>>();
        for required in &builder.required_exports {
            let Some(declared) = declared_entrypoints
                .iter()
                .find(|entrypoint| entrypoint.name == required.name)
            else {
                return Err(EngineError::Contract(format!(
                    "required entrypoint `{}` is not declared by the Host contract",
                    required.name
                )));
            };
            if declared.stable_id != required.stable_id
                || declared.signature != required.signature
                || declared.effect != required.effect
            {
                return Err(EngineError::Contract(format!(
                    "generated descriptor for required entrypoint `{}` does not match the Host contract",
                    required.name
                )));
            }
        }
        let required_exports = declared_entrypoints
            .iter()
            .filter_map(|declared| {
                builder
                    .required_exports
                    .iter()
                    .find(|required| required.stable_id == declared.stable_id)
                    .cloned()
            })
            .collect();
        let (host_contract_source_identity, host_contract_source) =
            if let Some((identity, text)) = builder.host_contract_source {
                nexa::HostContractInput::with_source(&idl, identity.clone(), text.clone())
                    .map_err(|error| EngineError::Contract(error.to_string()))?;
                (identity, text)
            } else {
                let contract = nexa::HostContractInput::canonical(&idl);
                (
                    contract.source().identity().clone(),
                    std::sync::Arc::clone(contract.source().text()),
                )
            };
        let persisted = builder
            .storage_dir
            .as_deref()
            .map(persistence::load)
            .transpose()
            .map_err(|error| EngineError::Persistence(error.to_string()))?
            .unwrap_or_default();
        let build_session =
            std::sync::Arc::new(std::sync::Mutex::new(nexa::PackageBuildSession::new()));
        let development_worker = DevelopmentWorker::start_with_session(
            &builder.development,
            std::sync::Arc::clone(&build_session),
        );
        let development_coordinator = nexa_analysis::DevelopmentCoordinator::new(
            nexa_analysis::DevelopmentCoordinatorConfig {
                stable_scan_count: builder.development.stable_scan_count,
                queue_capacity: builder.development.compile_queue_capacity,
                retain_terminal_generations: 128,
            },
        );
        Ok(Self {
            contract: builder.contract,
            idl,
            host_contract_source_identity,
            host_contract_source,
            host_factory: builder
                .host_factory
                .ok_or(EngineError::MissingHostFactory)?,
            sources: builder.sources,
            entitlements: builder.entitlements,
            storage_dir: builder.storage_dir,
            runtime_host: builder
                .runtime_host
                .unwrap_or_else(|| nexa_runtime::RuntimeHost::new(builder.runtime_host_capacity)),
            packages: Vec::new(),
            dispatch_plan: Vec::new(),
            diagnostics: BoundedDiagnosticLog::default(),
            required_exports,
            declared_entrypoints,
            persisted,
            development: builder.development,
            development_coordinator,
            build_session,
            development_worker,
            development_events: VecDeque::new(),
            pending_development_events: Vec::new(),
            reload_reports: VecDeque::new(),
            pending_reload_reports: Vec::new(),
            dropped_events: 0,
            last_tick_diagnostic_sequence: 0,
            ticks: 0,
            next_realm_id: 1,
            delivered_release_records: 0,
            discovered: false,
            shutdown: false,
        })
    }

    pub fn discover(&mut self) -> Result<Vec<PackageInfo>, EngineError> {
        self.require_open()?;
        if self.discovered {
            return Err(EngineError::DiscoveryAlreadyCompleted);
        }
        let build_context = self.candidate_build_context();
        let mut records = Vec::new();
        for source in &self.sources {
            let source_id = source.id().clone();
            let candidates = match source.discover(&build_context) {
                Ok(candidates) => candidates,
                Err(error) => {
                    let message = error.to_string();
                    let (stage, code) = if error.is_policy() {
                        (EngineDiagnosticStage::Policy, nexa::ErrorCode::NX7003)
                    } else if error.is_manifest() {
                        (EngineDiagnosticStage::Manifest, nexa::ErrorCode::NX7002)
                    } else {
                        (
                            EngineDiagnosticStage::SourceDiscovery,
                            nexa::ErrorCode::NX7001,
                        )
                    };
                    self.diagnostics.push(EngineDiagnostic::without_source(
                        None,
                        Some(source_id.clone()),
                        stage,
                        code,
                        &message,
                    ));
                    return Err(EngineError::Source {
                        source: source_id,
                        message,
                    });
                }
            };
            for discovered in candidates {
                let candidate = discovered.candidate;
                let build_input = discovered.build_input;
                let effective =
                    manifest::apply_package_policy(&candidate.manifest, source.policy()).map_err(
                        |error| EngineError::Source {
                            source: source_id.clone(),
                            message: error.to_string(),
                        },
                    )?;
                let mut lifecycle = PackageLifecycle::discovered();
                let locked = effective
                    .entitlement
                    .as_ref()
                    .is_some_and(|id| !self.entitlements.contains(id));
                lifecycle.transition(if locked {
                    PackageStatus::Locked
                } else {
                    PackageStatus::Disabled
                })?;
                records.push(PackageRecord {
                    source_id: source.id().clone(),
                    policy: source.policy().clone(),
                    effective,
                    candidate,
                    build_input,
                    lifecycle,
                    runtime: None,
                    last_diagnostic: None,
                    last_known_good: None,
                    development: development::PackageDevelopment::default(),
                    awaiting_job: None,
                    handler_calls_this_tick: 0,
                    handler_instructions_this_tick: 0,
                    fuel_used_this_tick: 0,
                    outputs_this_tick: 0,
                    ready_candidate: None,
                    ready_commit_requested: false,
                });
            }
        }
        self.mark_duplicate_package_ids(&mut records)?;
        self.packages = records;
        self.discovered = true;
        Ok(self.packages())
    }

    fn mark_duplicate_package_ids(
        &mut self,
        records: &mut [PackageRecord],
    ) -> Result<(), EngineError> {
        let mut counts = BTreeMap::<PackageId, usize>::new();
        for record in records.iter() {
            *counts
                .entry(record.candidate.manifest.id.clone())
                .or_default() += 1;
        }
        for record in records {
            if counts
                .get(&record.candidate.manifest.id)
                .is_some_and(|count| *count > 1)
            {
                record.lifecycle.transition(PackageStatus::Incompatible)?;
                let diagnostic = self.diagnostics.push(EngineDiagnostic::without_source(
                    Some(record.candidate.manifest.id.clone()),
                    Some(record.source_id.clone()),
                    EngineDiagnosticStage::Manifest,
                    nexa::ErrorCode::NX7002,
                    "duplicate package id",
                ));
                record.last_diagnostic = Some(diagnostic.summary());
            }
        }
        Ok(())
    }

    pub fn enable_defaults(&mut self) -> Result<(), EngineError> {
        let ids = self
            .packages
            .iter()
            .filter(|record| {
                let manifest = &record.candidate.manifest;
                record.effective.activation == ActivationPolicy::Required
                    || (record.effective.activation == ActivationPolicy::DefaultEnabled
                        && self.persisted.get(&manifest.id).copied().unwrap_or(true))
                    || self.persisted.get(&manifest.id).copied() == Some(true)
            })
            .map(|record| record.candidate.manifest.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if self.status(&id) != Some(PackageStatus::Locked) {
                let _ = self.enable(&id);
            }
        }
        Ok(())
    }

    pub fn enable(&mut self, id: &PackageId) -> Result<(), EngineError> {
        self.require_open()?;
        let index = self.unique_index(id)?;
        match self.packages[index].lifecycle.status() {
            PackageStatus::Enabled => return Ok(()),
            PackageStatus::Locked => {
                let error = EngineError::Locked(id.clone());
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                return Err(error);
            }
            PackageStatus::Incompatible => return Err(EngineError::Incompatible(id.clone())),
            PackageStatus::Discovered | PackageStatus::Disabled | PackageStatus::Faulted => {}
            status => return Err(EngineError::InvalidState(id.clone(), status)),
        }
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Enabling)?;
        match self.fresh_candidate(index) {
            Ok(discovered) => {
                let candidate = discovered.candidate;
                let effective = manifest::apply_package_policy(
                    &candidate.manifest,
                    &self.packages[index].policy,
                )
                .map_err(|error| EngineError::Source {
                    source: self.packages[index].source_id.clone(),
                    message: error.to_string(),
                })?;
                self.packages[index].candidate = candidate;
                self.packages[index].build_input = discovered.build_input;
                self.packages[index].effective = effective;
            }
            Err(error) => {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Faulted)?;
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                return Err(error);
            }
        }
        let result = self.build_runtime(index);
        match result {
            Ok(runtime) => {
                let artifact = runtime.artifact.clone();
                self.packages[index].development.active_build_fingerprint =
                    Some(artifact.identity.build_fingerprint);
                self.packages[index].development.desired_build_fingerprint =
                    Some(artifact.identity.build_fingerprint);
                self.packages[index].development.terminal_build_fingerprint = None;
                let epoch = runtime
                    .realm
                    .active_module_epoch(runtime.module)
                    .unwrap_or_default();
                self.commit_last_known_good(index, artifact, epoch);
                self.packages[index].runtime = Some(runtime);
                self.packages[index].last_diagnostic = None;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                self.persisted.insert(id.clone(), true);
                if let Err(error) = self.persist_selections() {
                    let summary = self.record_error(index, &error);
                    self.packages[index].last_diagnostic = Some(summary);
                    return Err(error);
                }
                Ok(())
            }
            Err(error) => {
                self.packages[index].runtime = None;
                let next = if matches!(
                    error,
                    EngineError::MissingExport(_, _)
                        | EngineError::ExportSignature(_, _)
                        | EngineError::UndeclaredEntrypoint(_, _)
                ) {
                    PackageStatus::Incompatible
                } else {
                    PackageStatus::Faulted
                };
                self.packages[index].lifecycle.transition(next)?;
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(error)
            }
        }
    }

    fn fresh_candidate(&self, index: usize) -> Result<DiscoveredPackage, EngineError> {
        let record = &self.packages[index];
        let build_context = self.candidate_build_context();
        let source = self
            .sources
            .iter()
            .find(|source| source.id() == &record.source_id)
            .ok_or_else(|| EngineError::Source {
                source: record.source_id.clone(),
                message: "package source is no longer registered".into(),
            })?;
        source
            .discover(&build_context)
            .map_err(|error| EngineError::Source {
                source: source.id().clone(),
                message: error.to_string(),
            })?
            .into_iter()
            .find(|discovered| discovered.candidate.manifest.id == record.candidate.manifest.id)
            .ok_or_else(|| EngineError::Source {
                source: source.id().clone(),
                message: format!("package {} disappeared", record.candidate.manifest.id),
            })
    }

    fn candidate_build_context(&self) -> CandidateBuildContext {
        CandidateBuildContext::with_source(
            self.host_contract_source_identity.clone(),
            self.host_contract_source.as_bytes().to_vec(),
        )
        .requiring_entrypoints(
            self.required_exports
                .iter()
                .map(|entrypoint| entrypoint.name.clone()),
        )
    }

    fn host_contract_input(&self) -> nexa::HostContractInput<'_> {
        nexa::HostContractInput::with_source(
            &self.idl,
            self.host_contract_source_identity.clone(),
            std::sync::Arc::clone(&self.host_contract_source),
        )
        .expect("the Engine validates its immutable Host source while building")
        .requiring_entrypoints(
            &self
                .required_exports
                .iter()
                .map(|entrypoint| entrypoint.name.clone())
                .collect::<Vec<_>>(),
        )
        .expect("the Engine validates required entrypoints while building")
    }

    fn refresh_desired_build_fingerprint(&mut self, index: usize) -> Option<BuildFingerprint> {
        match self.fresh_candidate(index) {
            Ok(discovered) => {
                let desired = discovered.candidate.build_fingerprint;
                self.packages[index].development.desired_build_fingerprint = Some(desired);
                Some(desired)
            }
            Err(error) => {
                self.packages[index].development.desired_build_fingerprint = None;
                self.mark_source_missing(index, &error.to_string());
                None
            }
        }
    }

    fn candidate_identity_is_current(
        &mut self,
        index: usize,
        data: &CandidateTerminalData,
    ) -> bool {
        data.identity.generation == self.packages[index].development.latest_generation
            && self.refresh_desired_build_fingerprint(index)
                == Some(data.identity.build_fingerprint)
    }

    fn build_runtime(&mut self, index: usize) -> Result<PackageRuntime, EngineError> {
        let generation = self.packages[index]
            .development
            .latest_generation
            .saturating_add(1)
            .max(1);
        let identity = self.packages[index]
            .candidate
            .identity(generation)
            .map_err(|error| EngineError::Candidate(error.to_string()))?;
        let artifact = self.compile_candidate(index, identity.clone())?;
        let fresh = self.fresh_candidate(index)?;
        if fresh.candidate.build_fingerprint != identity.build_fingerprint
            || fresh.build_input.build_fingerprint != artifact.build_fingerprint
        {
            return Err(EngineError::StaleCandidate(identity));
        }
        self.instantiate_runtime(index, artifact)
    }

    fn instantiate_runtime(
        &mut self,
        index: usize,
        artifact: CompiledPackageArtifact,
    ) -> Result<PackageRuntime, EngineError> {
        let record = &self.packages[index];
        let manifest = &record.candidate.manifest;
        artifact.verify_integrity().map_err(|error| {
            EngineError::Load(
                manifest.id.clone(),
                format!("artifact integrity check failed: {error}"),
            )
        })?;
        self.validate_exports(manifest.id.clone(), &artifact.verified)?;
        let context = PackageContext {
            package_id: manifest.id.clone(),
            source_id: record.source_id.clone(),
            trust: record.policy.trust,
            capabilities: record.effective.capabilities.clone(),
            data_namespace: format!("{}.{}", record.source_id, manifest.id),
            version: manifest.version.clone(),
        };
        let registry = self.host_factory.create(&context);
        let config = nexa_runtime::RealmConfig {
            realm_id: self.next_realm_id,
            max_heap_objects: record.effective.runtime_limits.heap_objects,
            max_host_resources: record.effective.runtime_limits.host_resources,
            release_capacity: record.effective.runtime_limits.release_records,
            runtime_limits: nexa_runtime::RuntimeLimits {
                max_tasks: record.effective.runtime_limits.tasks,
                max_scheduler_tokens: record.effective.runtime_limits.tasks,
                ..nexa_runtime::RuntimeLimits::default()
            },
            ..nexa_runtime::RealmConfig::default()
        };
        self.next_realm_id = self
            .next_realm_id
            .checked_add(1)
            .ok_or(EngineError::RealmIdExhausted)?;
        let mut realm =
            nexa_runtime::RealmRuntime::hosted(config, self.runtime_host.clone(), registry)
                .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        let module = realm
            .load_module(
                artifact.verified.clone(),
                self.contract.contract_runtime_id(),
                artifact.state_schema_fingerprint,
            )
            .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        let root_scope = realm
            .create_scope(None)
            .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        Ok(PackageRuntime {
            realm,
            module,
            root_scope,
            artifact,
        })
    }

    fn compile_candidate(
        &mut self,
        index: usize,
        identity: CandidateIdentity,
    ) -> Result<CompiledPackageArtifact, EngineError> {
        let source_id = self.packages[index].source_id.clone();
        let build_input = std::sync::Arc::clone(&self.packages[index].build_input);
        let host_contract = self.host_contract_input();
        let compilation = {
            let mut build_session = self
                .build_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            artifact::compile_package_candidate(
                &mut build_session,
                &host_contract,
                &self.required_exports,
                &source_id,
                identity,
                &build_input,
            )
        };
        match compilation {
            Ok(compilation) => Ok(compilation.artifact),
            Err(failure) => {
                let diagnostic = self.diagnostics.push(failure.diagnostic);
                for additional in failure.additional_diagnostics {
                    self.diagnostics.push(additional);
                }
                Err(EngineError::Diagnostic(Box::new(diagnostic)))
            }
        }
    }

    fn validate_exports(
        &self,
        id: PackageId,
        verified: &nexa_verifier::VerifiedModule,
    ) -> Result<(), EngineError> {
        for found in &verified.module().exports {
            let Some(declared) = self
                .declared_entrypoints
                .iter()
                .find(|entrypoint| entrypoint.stable_id == found.stable_id)
            else {
                return Err(EngineError::UndeclaredEntrypoint(id, found.stable_id));
            };
            let found_effect = usize::try_from(found.function)
                .ok()
                .and_then(|index| verified.module().functions.get(index))
                .map(|function| function.effect);
            if found.signature != declared.signature
                || !found_effect
                    .is_some_and(|found| effect_satisfies_declaration(found, declared.effect))
            {
                return Err(EngineError::ExportSignature(id, declared.name.clone()));
            }
        }
        for requirement in &self.required_exports {
            let Some(found) = verified
                .module()
                .exports
                .iter()
                .find(|export| export.stable_id == requirement.stable_id)
            else {
                return Err(EngineError::MissingExport(id, requirement.name.clone()));
            };
            let found_effect = usize::try_from(found.function)
                .ok()
                .and_then(|index| verified.module().functions.get(index))
                .map(|function| function.effect);
            if found.signature != requirement.signature
                || !found_effect
                    .is_some_and(|found| effect_satisfies_declaration(found, requirement.effect))
            {
                return Err(EngineError::ExportSignature(id, requirement.name.clone()));
            }
        }
        Ok(())
    }

    pub fn disable(&mut self, id: &PackageId) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        if self.packages[index].effective.activation == ActivationPolicy::Required {
            return Err(EngineError::RequiredPackage(id.clone()));
        }
        self.disable_index(index, true)
    }

    pub fn fault(&mut self, id: &PackageId, message: impl Into<String>) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        let message = message.into();
        let status = self.packages[index].lifecycle.status();
        if status == PackageStatus::Enabled {
            if let Some(mut runtime) = self.packages[index].runtime.take() {
                let _ = runtime.realm.cancel_scope(runtime.root_scope);
                drop(runtime);
            }
            self.packages[index]
                .lifecycle
                .transition(PackageStatus::Faulted)?;
        } else if status != PackageStatus::Faulted {
            return Err(EngineError::InvalidState(id.clone(), status));
        }
        let summary = self.record_diagnostic(
            index,
            EngineDiagnosticStage::Handler,
            nexa::ErrorCode::NX7103,
            message,
        );
        self.packages[index].last_diagnostic = Some(summary);
        self.drain_releases();
        Ok(())
    }

    fn disable_index(&mut self, index: usize, persist: bool) -> Result<(), EngineError> {
        if self.packages[index].lifecycle.status() == PackageStatus::Disabled {
            return Ok(());
        }
        if self.packages[index].lifecycle.status() == PackageStatus::Locked {
            return Ok(());
        }
        let package_id = self.packages[index].candidate.manifest.id.clone();
        let _ = self.development_coordinator.invalidate(
            &package_id,
            nexa_analysis::DevelopmentInvalidation::Transient,
        );
        self.cancel_development(index, CandidateCancellation::Disable);
        let id = self.packages[index].candidate.manifest.id.clone();
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Disabling)?;
        if let Some(mut runtime) = self.packages[index].runtime.take() {
            let _ = runtime.realm.cancel_scope(runtime.root_scope);
            let _ = runtime.realm.tick(nexa_runtime::TickBudget {
                max_tasks: usize::MAX,
                frame_fuel_budget: 4_096,
                collect_garbage: true,
            });
            drop(runtime);
        }
        self.drain_releases();
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Disabled)?;
        if persist {
            self.persisted.insert(id, false);
            if let Err(error) = self.persist_selections() {
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn reload(&mut self, id: &PackageId) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        if !matches!(
            self.packages[index].lifecycle.status(),
            PackageStatus::Enabled | PackageStatus::Faulted
        ) {
            return Err(EngineError::InvalidState(
                id.clone(),
                self.packages[index].lifecycle.status(),
            ));
        }
        let discovered = match self.fresh_candidate(index) {
            Ok(discovered) => discovered,
            Err(error) => {
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                return Err(error);
            }
        };
        self.reload_candidate(index, discovered)
    }

    pub fn request_ready_commit(&mut self, id: &PackageId) -> Result<bool, EngineError> {
        let index = self.unique_index(id)?;
        let ready = self.packages[index].ready_candidate.is_some();
        self.packages[index].ready_commit_requested = ready;
        Ok(ready)
    }

    #[allow(clippy::too_many_lines)]
    fn reload_candidate(
        &mut self,
        index: usize,
        discovered: DiscoveredPackage,
    ) -> Result<(), EngineError> {
        let candidate = discovered.candidate;
        let build_input = discovered.build_input;
        self.supersede_development_for_current_source(index, None);
        let (identity, _) = self
            .development_coordinator
            .begin(candidate.manifest.id.clone(), candidate.build_fingerprint);
        self.packages[index].development.latest_generation = identity.generation;
        self.packages[index].development.desired_build_fingerprint =
            Some(candidate.build_fingerprint);
        let source_id = self.packages[index].source_id.clone();
        let host_contract = self.host_contract_input();
        let compilation = {
            let mut build_session = self
                .build_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            artifact::compile_package_candidate(
                &mut build_session,
                &host_contract,
                &self.required_exports,
                &source_id,
                identity.clone(),
                &build_input,
            )
        };
        let compilation = match compilation {
            Ok(compilation) => compilation,
            Err(failure) => {
                let mut diagnostic = failure.diagnostic;
                let additional_diagnostics = failure.additional_diagnostics;
                let requested_terminal = if diagnostic.stage == EngineDiagnosticStage::Verify {
                    CandidateTerminalKind::VerifyFailed
                } else {
                    CandidateTerminalKind::CompileFailed
                };
                let terminal_data = CandidateTerminalData {
                    source_id: self.packages[index].source_id.clone(),
                    identity: identity.clone(),
                    build_input: std::sync::Arc::clone(&build_input),
                    queue_duration: Duration::ZERO,
                    work_duration: failure
                        .compile_duration
                        .saturating_add(failure.verify_duration),
                };
                let terminal_kind =
                    self.record_generation_terminal(index, &terminal_data, requested_terminal);
                if terminal_kind != requested_terminal {
                    self.finish_recorded_candidate_rejection(
                        index,
                        terminal_data,
                        terminal_kind,
                        failure.compile_duration,
                        failure.verify_duration,
                    );
                    return Err(EngineError::StaleCandidate(identity));
                }
                diagnostic.context.candidate_generation = Some(identity.generation);
                let diagnostic = self.diagnostics.push(diagnostic);
                self.packages[index].last_diagnostic = Some(diagnostic.summary());
                for mut additional in additional_diagnostics {
                    additional.context.candidate_generation = Some(identity.generation);
                    self.diagnostics.push(additional);
                }
                let report = reload_report(
                    identity,
                    self.packages[index]
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.realm.active_module_epoch(runtime.module).ok())
                        .unwrap_or_default(),
                    None,
                    failure.compile_duration,
                    failure.verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    if diagnostic.stage == EngineDiagnosticStage::Verify {
                        ReloadReportOutcome::VerifyFailed
                    } else {
                        ReloadReportOutcome::CompileFailed
                    },
                    0,
                    0,
                );
                self.publish_reload(report);
                return Err(EngineError::Diagnostic(Box::new(diagnostic)));
            }
        };
        let terminal_data = CandidateTerminalData {
            source_id: self.packages[index].source_id.clone(),
            identity: identity.clone(),
            build_input: std::sync::Arc::clone(&build_input),
            queue_duration: Duration::ZERO,
            work_duration: compilation
                .compile_duration
                .saturating_add(compilation.verify_duration),
        };
        let terminal_kind =
            self.record_generation_terminal(index, &terminal_data, CandidateTerminalKind::Compiled);
        if terminal_kind != CandidateTerminalKind::Compiled {
            self.finish_recorded_candidate_rejection(
                index,
                terminal_data,
                terminal_kind,
                compilation.compile_duration,
                compilation.verify_duration,
            );
            return Err(EngineError::StaleCandidate(identity));
        }
        match self.commit_compiled_candidate(
            index,
            candidate,
            build_input,
            compilation.artifact,
            compilation.compile_duration,
            compilation.verify_duration,
        ) {
            Ok(report) => {
                self.publish_reload(report);
                Ok(())
            }
            Err(failure) => {
                self.publish_reload(failure.report);
                Err(failure.error)
            }
        }
    }

    #[allow(clippy::result_large_err, clippy::too_many_lines)]
    fn commit_compiled_candidate(
        &mut self,
        index: usize,
        candidate: PackageCandidate,
        build_input: std::sync::Arc<nexa_analysis::ResolvedBuildInput>,
        artifact: CompiledPackageArtifact,
        compile_duration: std::time::Duration,
        verify_duration: std::time::Duration,
    ) -> Result<ReloadReport, ReloadFailure> {
        let id = self.packages[index].candidate.manifest.id.clone();
        let identity = artifact.identity.clone();
        let generation = identity.generation;
        let next_effective =
            manifest::apply_package_policy(&candidate.manifest, &self.packages[index].policy)
                .map_err(|error| {
                    ReloadFailure::new(
                        reload_report(
                            identity.clone(),
                            0,
                            None,
                            compile_duration,
                            verify_duration,
                            nexa_runtime::RestartReloadMetrics::default(),
                            ReloadReportOutcome::RolledBackBeforeCommit,
                            0,
                            0,
                        ),
                        EngineError::Candidate(error.to_string()),
                    )
                })?;
        if next_effective
            .entitlement
            .as_ref()
            .is_some_and(|entitlement| !self.entitlements.contains(entitlement))
        {
            return Err(ReloadFailure::new(
                reload_report(
                    identity.clone(),
                    0,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                EngineError::Locked(id.clone()),
            ));
        }
        let old_epoch = self.packages[index]
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.realm.active_module_epoch(runtime.module).ok())
            .unwrap_or_default();
        if let Err(error) = self.validate_exports(id.clone(), &artifact.verified) {
            return Err(ReloadFailure::new(
                reload_report(
                    identity.clone(),
                    old_epoch,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                error,
            ));
        }
        let commit_identity_matches = artifact.identity.package_id.eq(&candidate.manifest.id)
            && artifact.identity.package_id.eq(build_input.root_package())
            && artifact.identity.build_fingerprint == artifact.build_fingerprint
            && artifact.build_fingerprint == candidate.build_fingerprint
            && artifact.build_fingerprint == build_input.build_fingerprint;
        if !commit_identity_matches {
            return Err(ReloadFailure::new(
                reload_report(
                    identity.clone(),
                    old_epoch,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                EngineError::StaleCandidate(identity),
            ));
        }
        debug_assert_eq!(
            self.packages[index].development.latest_generation, generation,
            "the commit caller terminalizes freshness immediately before entering Runtime"
        );
        if let Err(error) = artifact.verify_integrity() {
            return Err(ReloadFailure::new(
                reload_report(
                    identity.clone(),
                    old_epoch,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                EngineError::Load(
                    id.clone(),
                    format!("artifact integrity check failed: {error}"),
                ),
            ));
        }
        let reload_started = std::time::Instant::now();
        if self.packages[index].lifecycle.status() == PackageStatus::Faulted {
            let old_candidate = std::mem::replace(&mut self.packages[index].candidate, candidate);
            let old_build_input =
                std::mem::replace(&mut self.packages[index].build_input, build_input);
            let old_effective =
                std::mem::replace(&mut self.packages[index].effective, next_effective.clone());
            if let Err(error) = self.packages[index]
                .lifecycle
                .transition(PackageStatus::Enabling)
            {
                self.packages[index].candidate = old_candidate;
                self.packages[index].build_input = old_build_input;
                self.packages[index].effective = old_effective;
                let engine_error = EngineError::Lifecycle(error);
                return Err(ReloadFailure::new(
                    reload_report(
                        identity,
                        old_epoch,
                        None,
                        compile_duration,
                        verify_duration,
                        nexa_runtime::RestartReloadMetrics {
                            activation_duration: reload_started.elapsed(),
                            ..nexa_runtime::RestartReloadMetrics::default()
                        },
                        ReloadReportOutcome::ActivationFaulted,
                        0,
                        0,
                    ),
                    engine_error,
                ));
            }
            return match self.instantiate_runtime(index, artifact.clone()) {
                Ok(runtime) => {
                    let new_epoch = runtime.realm.active_module_epoch(runtime.module).ok();
                    self.packages[index].runtime = Some(runtime);
                    let _ = self.packages[index]
                        .lifecycle
                        .transition(PackageStatus::Enabled);
                    self.commit_last_known_good(index, artifact, new_epoch.unwrap_or(0));
                    self.packages[index].last_diagnostic = None;
                    Ok(reload_report(
                        identity,
                        old_epoch,
                        new_epoch,
                        compile_duration,
                        verify_duration,
                        nexa_runtime::RestartReloadMetrics {
                            activation_duration: reload_started.elapsed(),
                            ..nexa_runtime::RestartReloadMetrics::default()
                        },
                        ReloadReportOutcome::Committed,
                        0,
                        0,
                    ))
                }
                Err(error) => {
                    self.packages[index].candidate = old_candidate;
                    self.packages[index].build_input = old_build_input;
                    self.packages[index].effective = old_effective;
                    let _ = self.packages[index]
                        .lifecycle
                        .transition(PackageStatus::Faulted);
                    let summary = self.record_error(index, &error);
                    self.packages[index].last_diagnostic = Some(summary);
                    Err(ReloadFailure::new(
                        reload_report(
                            identity,
                            old_epoch,
                            None,
                            compile_duration,
                            verify_duration,
                            nexa_runtime::RestartReloadMetrics {
                                activation_duration: reload_started.elapsed(),
                                ..nexa_runtime::RestartReloadMetrics::default()
                            },
                            ReloadReportOutcome::ActivationFaulted,
                            0,
                            0,
                        ),
                        error,
                    ))
                }
            };
        }
        if let Err(error) = self.packages[index]
            .lifecycle
            .transition(PackageStatus::Reloading)
        {
            return Err(ReloadFailure::new(
                reload_report(
                    identity,
                    old_epoch,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                EngineError::Lifecycle(error),
            ));
        }
        let measured = {
            let runtime = self.packages[index].runtime.as_mut().ok_or_else(|| {
                ReloadFailure::new(
                    reload_report(
                        identity.clone(),
                        old_epoch,
                        None,
                        compile_duration,
                        verify_duration,
                        nexa_runtime::RestartReloadMetrics::default(),
                        ReloadReportOutcome::RolledBackBeforeCommit,
                        0,
                        0,
                    ),
                    EngineError::InvalidState(id.clone(), PackageStatus::Reloading),
                )
            })?;
            runtime.realm.restart_reload_measured(
                runtime.module,
                artifact.verified.clone(),
                nexa_runtime::RestartReloadPolicy::default(),
            )
        };
        let (outcome, reload_metrics) = match measured {
            Ok(result) => (Ok(result.outcome), result.metrics),
            Err(error) => (Err(error), nexa_runtime::RestartReloadMetrics::default()),
        };
        let accounting = self.packages[index]
            .runtime
            .as_ref()
            .map(|runtime| runtime.realm.reload_accounting())
            .unwrap_or_default();
        match outcome {
            Ok(nexa_runtime::RestartReloadOutcome::Committed(module)) => {
                let new_epoch = if let Some(runtime) = self.packages[index].runtime.as_mut() {
                    runtime.module = module;
                    runtime.artifact = artifact.clone();
                    runtime.realm.active_module_epoch(module).ok()
                } else {
                    None
                };
                self.packages[index].candidate = candidate;
                self.packages[index].build_input = build_input;
                self.packages[index].effective = next_effective;
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled);
                self.packages[index].last_diagnostic = None;
                self.commit_last_known_good(
                    index,
                    artifact,
                    new_epoch.unwrap_or(old_epoch.saturating_add(1)),
                );
                self.drain_releases();
                Ok(reload_report(
                    identity,
                    old_epoch,
                    new_epoch,
                    compile_duration,
                    verify_duration,
                    reload_metrics,
                    ReloadReportOutcome::Committed,
                    accounting.cancelled_tasks,
                    accounting.detached_requests,
                ))
            }
            Ok(nexa_runtime::RestartReloadOutcome::RolledBackBeforeCommit { reason, .. })
            | Err(reason) => {
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled);
                let error = EngineError::Reload(id.clone(), reason.to_string());
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(ReloadFailure::new(
                    reload_report(
                        identity,
                        old_epoch,
                        None,
                        compile_duration,
                        verify_duration,
                        reload_metrics,
                        ReloadReportOutcome::RolledBackBeforeCommit,
                        accounting.cancelled_tasks,
                        accounting.detached_requests,
                    ),
                    error,
                ))
            }
            Ok(nexa_runtime::RestartReloadOutcome::ActivationFaulted { error, .. }) => {
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled);
                let error = EngineError::Activation(id.clone(), error.to_string());
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(ReloadFailure::new(
                    reload_report(
                        identity,
                        old_epoch,
                        None,
                        compile_duration,
                        verify_duration,
                        reload_metrics,
                        ReloadReportOutcome::ActivationFaulted,
                        accounting.cancelled_tasks,
                        accounting.detached_requests,
                    ),
                    error,
                ))
            }
        }
    }

    fn commit_last_known_good(
        &mut self,
        index: usize,
        artifact: CompiledPackageArtifact,
        epoch: u64,
    ) {
        self.packages[index].last_known_good = Some(LastKnownGood {
            identity: artifact.identity.clone(),
            source_set_fingerprint: artifact.source_set_fingerprint,
            public_api_fingerprint: artifact.public_api_fingerprint,
            state_schema_fingerprint: artifact.state_schema_fingerprint,
            linked_state_fingerprint: artifact.linked_state_fingerprint,
            dependency_closure: std::sync::Arc::clone(&artifact.dependency_closure),
            host_contract_fingerprint: self.contract.contract_fingerprint(),
            host_contract_id: self.contract.contract_runtime_id(),
            artifact,
            epoch,
        });
        self.packages[index].development.active_build_fingerprint = self.packages[index]
            .last_known_good
            .as_ref()
            .map(|known_good| known_good.identity.build_fingerprint);
        if let Some(identity) = self.packages[index]
            .last_known_good
            .as_ref()
            .map(|known_good| known_good.identity.clone())
        {
            self.development_coordinator
                .retain_active(identity.package_id, identity.build_fingerprint);
        }
    }

    pub fn reload_changed(&mut self) -> Result<usize, EngineError> {
        self.require_open()?;
        if !self.discovered {
            return Err(EngineError::DiscoveryRequired);
        }
        let mut changed = Vec::new();
        let mut added = Vec::new();
        let build_context = self.candidate_build_context();
        let discoveries = self
            .sources
            .iter()
            .map(|source| {
                (
                    source.id().clone(),
                    source.policy().clone(),
                    source.discover(&build_context),
                )
            })
            .collect::<Vec<_>>();
        let discoveries = self.validate_reload_discoveries(discoveries)?;
        let mut topology_changes = 0_usize;
        for (source_id, policy, candidates) in discoveries {
            topology_changes = topology_changes.saturating_add(self.reconcile_discovered_source(
                &source_id,
                &policy,
                candidates,
                &mut changed,
                &mut added,
            )?);
        }
        let count = topology_changes.saturating_add(changed.len());
        for (index, discovered) in changed {
            self.reload_candidate(index, discovered)?;
        }
        for package_id in added {
            self.enable(&package_id)?;
        }
        Ok(count)
    }

    fn validate_reload_discoveries(
        &mut self,
        discoveries: Vec<(
            SourceId,
            PackagePolicy,
            Result<Vec<DiscoveredPackage>, PackageSourceError>,
        )>,
    ) -> Result<Vec<(SourceId, PackagePolicy, Vec<DiscoveredPackage>)>, EngineError> {
        let mut validated = Vec::with_capacity(discoveries.len());
        for (source_id, policy, result) in discoveries {
            let candidates = match result {
                Ok(candidates) => candidates,
                Err(error) => {
                    let message = error.to_string();
                    let indexes = self
                        .packages
                        .iter()
                        .enumerate()
                        .filter(|(_, record)| record.source_id == source_id)
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    for index in indexes {
                        self.mark_source_missing(index, &message);
                    }
                    return Err(EngineError::Source {
                        source: source_id,
                        message,
                    });
                }
            };
            for discovered in &candidates {
                let package_id = &discovered.candidate.manifest.id;
                if self.packages.iter().any(|record| {
                    record.source_id != source_id && record.candidate.manifest.id == *package_id
                }) || validated.iter().any(
                    |(validated_source, _, validated_candidates): &(
                        SourceId,
                        PackagePolicy,
                        Vec<DiscoveredPackage>,
                    )| {
                        validated_source != &source_id
                            && validated_candidates
                                .iter()
                                .any(|candidate| candidate.candidate.manifest.id == *package_id)
                    },
                ) {
                    return Err(EngineError::Source {
                        source: source_id,
                        message: format!(
                            "newly discovered Package {package_id} duplicates an existing Package ID"
                        ),
                    });
                }
                manifest::apply_package_policy(&discovered.candidate.manifest, &policy).map_err(
                    |error| EngineError::Source {
                        source: source_id.clone(),
                        message: error.to_string(),
                    },
                )?;
            }
            let mut ids = candidates
                .iter()
                .map(|candidate| candidate.candidate.manifest.id.clone())
                .collect::<Vec<_>>();
            ids.sort();
            if let Some(duplicate) = ids
                .windows(2)
                .find_map(|ids| (ids[0] == ids[1]).then(|| ids[0].clone()))
            {
                return Err(EngineError::Source {
                    source: source_id,
                    message: format!(
                        "newly discovered Package {duplicate} appears more than once in one source"
                    ),
                });
            }
            validated.push((source_id, policy, candidates));
        }
        Ok(validated)
    }

    fn reconcile_discovered_source(
        &mut self,
        source_id: &SourceId,
        policy: &PackagePolicy,
        candidates: Vec<DiscoveredPackage>,
        changed: &mut Vec<(usize, DiscoveredPackage)>,
        added: &mut Vec<PackageId>,
    ) -> Result<usize, EngineError> {
        let mut topology_changes = 0_usize;
        let indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.source_id == *source_id)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in indexes {
            let package_id = self.packages[index].candidate.manifest.id.clone();
            let Some(discovered) = candidates
                .iter()
                .find(|candidate| candidate.candidate.manifest.id == package_id)
                .cloned()
            else {
                if self.packages[index].development.state != DevelopmentState::SourceMissing {
                    topology_changes = topology_changes.saturating_add(1);
                }
                self.mark_source_missing(
                    index,
                    &format!(
                        "Package {package_id} disappeared from source {source_id}; \
                         the Last Known Good version remains loaded when available"
                    ),
                );
                continue;
            };
            let build_fingerprint = discovered.candidate.build_fingerprint;
            self.packages[index].development.desired_build_fingerprint = Some(build_fingerprint);
            if self.packages[index].development.state == DevelopmentState::SourceMissing
                && self.packages[index].development.active_build_fingerprint
                    == Some(build_fingerprint)
            {
                self.development_coordinator
                    .retain_active(package_id.clone(), build_fingerprint);
                self.packages[index].development.state = DevelopmentState::Idle;
                self.packages[index].development.terminal_build_fingerprint = None;
                self.packages[index].last_diagnostic = None;
                self.clear_unqueued_observation(index);
            }
            if self.packages[index].candidate.build_fingerprint != build_fingerprint {
                changed.push((index, discovered));
            }
        }
        for discovered in candidates {
            if let Some((package_id, should_enable)) =
                self.insert_discovered_package(source_id, policy, discovered)?
            {
                topology_changes = topology_changes.saturating_add(1);
                if should_enable {
                    added.push(package_id);
                }
            }
        }
        Ok(topology_changes)
    }

    fn insert_discovered_package(
        &mut self,
        source_id: &SourceId,
        policy: &PackagePolicy,
        discovered: DiscoveredPackage,
    ) -> Result<Option<(PackageId, bool)>, EngineError> {
        let package_id = discovered.candidate.manifest.id.clone();
        if self.packages.iter().any(|record| {
            record.source_id == *source_id && record.candidate.manifest.id == package_id
        }) {
            return Ok(None);
        }
        let effective = manifest::apply_package_policy(&discovered.candidate.manifest, policy)
            .expect("reload discovery policy was validated before reconciliation");
        let mut lifecycle = PackageLifecycle::discovered();
        let locked = effective
            .entitlement
            .as_ref()
            .is_some_and(|id| !self.entitlements.contains(id));
        lifecycle.transition(if locked {
            PackageStatus::Locked
        } else {
            PackageStatus::Disabled
        })?;
        let should_enable = !locked
            && (effective.activation == ActivationPolicy::Required
                || (effective.activation == ActivationPolicy::DefaultEnabled
                    && self.persisted.get(&package_id).copied().unwrap_or(true))
                || self.persisted.get(&package_id).copied() == Some(true));
        self.packages.push(PackageRecord {
            source_id: source_id.clone(),
            policy: policy.clone(),
            effective,
            candidate: discovered.candidate,
            build_input: discovered.build_input,
            lifecycle,
            runtime: None,
            last_diagnostic: None,
            last_known_good: None,
            development: development::PackageDevelopment::default(),
            awaiting_job: None,
            handler_calls_this_tick: 0,
            handler_instructions_this_tick: 0,
            fuel_used_this_tick: 0,
            outputs_this_tick: 0,
            ready_candidate: None,
            ready_commit_requested: false,
        });
        Ok(Some((package_id, should_enable)))
    }

    #[allow(clippy::too_many_lines)]
    fn scan_development_changes(&mut self) {
        let build_context = self.candidate_build_context();
        let discoveries = self
            .sources
            .iter()
            .map(|source| {
                let started = std::time::Instant::now();
                (
                    source.id().clone(),
                    source
                        .discover(&build_context)
                        .map_err(|error| error.to_string()),
                    started.elapsed(),
                )
            })
            .collect::<Vec<_>>();
        for (source_id, discovered, discovery_duration) in discoveries {
            let candidates = match discovered {
                Ok(candidates) => candidates,
                Err(message) => {
                    let indexes = self
                        .packages
                        .iter()
                        .enumerate()
                        .filter(|(_, record)| {
                            record.source_id == source_id
                                && matches!(
                                    record.lifecycle.status(),
                                    PackageStatus::Enabled | PackageStatus::Faulted
                                )
                        })
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    for index in indexes {
                        self.mark_source_missing(index, &message);
                    }
                    continue;
                }
            };
            let indexes = self
                .packages
                .iter()
                .enumerate()
                .filter(|(_, record)| {
                    record.source_id == source_id
                        && matches!(
                            record.lifecycle.status(),
                            PackageStatus::Enabled | PackageStatus::Faulted
                        )
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            for index in indexes {
                self.packages[index].development.last_discovery_duration = discovery_duration;
                let package_id = self.packages[index].candidate.manifest.id.clone();
                let Some(discovered) = candidates
                    .iter()
                    .find(|discovered| discovered.candidate.manifest.id == package_id)
                    .cloned()
                else {
                    self.mark_source_missing(
                        index,
                        "Package source or Manifest disappeared; the active version remains loaded",
                    );
                    continue;
                };
                let candidate = discovered.candidate;
                let build_input = discovered.build_input;

                let fingerprint_started = std::time::Instant::now();
                let build_fingerprint = candidate.build_fingerprint;
                self.packages[index]
                    .development
                    .last_build_fingerprint_duration = fingerprint_started.elapsed();
                self.packages[index].development.desired_build_fingerprint =
                    Some(build_fingerprint);
                if self.packages[index].development.state == DevelopmentState::SourceMissing {
                    if self.packages[index].development.active_build_fingerprint
                        == Some(build_fingerprint)
                    {
                        self.development_coordinator
                            .retain_active(package_id, build_fingerprint);
                        self.packages[index].development.state = DevelopmentState::Idle;
                        self.packages[index].development.terminal_build_fingerprint = None;
                        self.packages[index].last_diagnostic = None;
                        self.clear_unqueued_observation(index);
                        continue;
                    }
                    self.packages[index].development.state = DevelopmentState::Idle;
                    self.packages[index].development.terminal_build_fingerprint = None;
                }
                let observation = self
                    .development_coordinator
                    .observe(package_id, build_fingerprint);
                let matches_active = observation.matched_active;
                let matches_terminal = observation.matched_terminal;
                if matches_active || matches_terminal {
                    self.supersede_development_for_current_source(index, Some(build_fingerprint));
                    self.clear_unqueued_observation(index);
                    if matches_active {
                        self.packages[index].development.state = DevelopmentState::Idle;
                        self.packages[index].development.terminal_build_fingerprint = None;
                    }
                    continue;
                }

                if self.packages[index].development.observed_build_fingerprint
                    == Some(build_fingerprint)
                {
                    self.packages[index].development.stable_scans = observation.stable_scans;
                } else {
                    self.supersede_development_for_current_source(index, Some(build_fingerprint));
                    self.clear_unqueued_observation(index);
                    self.packages[index].development.observed_build_fingerprint =
                        Some(build_fingerprint);
                    self.packages[index].development.stable_build_fingerprint = None;
                    self.packages[index].development.stable_scans = observation.stable_scans;
                    self.packages[index].development.change_observed_at =
                        Some(std::time::Instant::now());
                    self.packages[index].development.state = DevelopmentState::ChangeObserved;
                    let identity = observation
                        .identity
                        .clone()
                        .expect("a changed development observation creates a Generation");
                    self.packages[index].development.latest_generation = identity.generation;
                    self.packages[index].development.unqueued_generation =
                        Some(CandidateTerminalData {
                            source_id: source_id.clone(),
                            identity: identity.clone(),
                            build_input: std::sync::Arc::clone(&build_input),
                            queue_duration: Duration::ZERO,
                            work_duration: Duration::ZERO,
                        });
                    self.publish_event(DevelopmentEvent::ChangeDetected(development_event_data(
                        identity, None,
                    )));
                }

                if self.packages[index].development.stable_scans
                    < self.development.stable_scan_count.max(1)
                {
                    self.packages[index].development.state =
                        DevelopmentState::WaitingForStableWrite;
                    continue;
                }
                let generation = self.packages[index].development.latest_generation;
                if self.packages[index].awaiting_job.is_some()
                    || self.packages[index].ready_candidate.is_some()
                    || (self.packages[index].development.queued_generation == Some(generation)
                        && self.packages[index].development.queued_build_fingerprint
                            == Some(build_fingerprint))
                    || (self.packages[index].development.in_flight_generation == Some(generation)
                        && self.packages[index].development.in_flight_build_fingerprint
                            == Some(build_fingerprint))
                {
                    continue;
                }
                let identity = self.packages[index]
                    .development
                    .unqueued_generation
                    .as_ref()
                    .map(|data| data.identity.clone())
                    .expect("a stable Candidate retains its immutable Generation identity");
                if self.packages[index].development.stable_build_fingerprint
                    != Some(build_fingerprint)
                {
                    self.packages[index].development.stable_build_fingerprint =
                        Some(build_fingerprint);
                    self.packages[index]
                        .development
                        .last_change_to_stable_duration = self.packages[index]
                        .development
                        .change_observed_at
                        .map_or(Duration::ZERO, |observed| observed.elapsed());
                    self.publish_event(DevelopmentEvent::ChangeStabilized(development_event_data(
                        identity.clone(),
                        None,
                    )));
                }
                let unqueued = self.packages[index]
                    .development
                    .unqueued_generation
                    .take()
                    .expect("a stable unqueued Candidate has a Generation ledger entry");
                assert_eq!(unqueued.identity.generation, generation);
                assert_eq!(unqueued.identity, identity);
                self.try_enqueue_job(
                    index,
                    CompileJob::new(
                        source_id.clone(),
                        unqueued.identity,
                        build_input,
                        self.idl.clone(),
                        self.required_exports.clone(),
                        self.host_contract_source_identity.clone(),
                        std::sync::Arc::clone(&self.host_contract_source),
                    ),
                );
            }
        }
    }

    fn mark_source_missing(&mut self, index: usize, message: &str) {
        self.packages[index].development.desired_build_fingerprint = None;
        if self.packages[index].development.state == DevelopmentState::SourceMissing {
            return;
        }
        let package_id = self.packages[index].candidate.manifest.id.clone();
        self.packages[index].development.state = DevelopmentState::SourceMissing;
        self.packages[index].development.observed_build_fingerprint = None;
        self.packages[index].development.stable_build_fingerprint = None;
        let coordinator_terminals = self.development_coordinator.invalidate(
            &package_id,
            nexa_analysis::DevelopmentInvalidation::SourceRemoval,
        );
        let mut event_identity = coordinator_terminals
            .iter()
            .max_by_key(|terminal| terminal.identity.generation)
            .map(|terminal| terminal.identity.clone());
        self.cancel_development(index, CandidateCancellation::SourceRemoval);
        if event_identity.is_none() {
            let active_build_fingerprint = self.packages[index].candidate.build_fingerprint;
            let next_generation = self
                .development_coordinator
                .inspection()
                .packages
                .get(&package_id)
                .map_or(1, |package| package.latest_generation.saturating_add(1));
            let mut fingerprint =
                nexa_analysis::FingerprintBuilder::new("nexa.engine.source-missing", 1);
            fingerprint.field_str("package", package_id.as_str());
            fingerprint.field_u64("generation", next_generation);
            fingerprint.field_bytes("active-build", active_build_fingerprint.as_bytes());
            let source_missing_fingerprint =
                BuildFingerprint::from_bytes(fingerprint.finish_bytes());
            let (identity, superseded) = self
                .development_coordinator
                .begin(package_id.clone(), source_missing_fingerprint);
            debug_assert!(superseded.is_empty());
            self.packages[index].development.latest_generation = identity.generation;
            let terminal = self.development_coordinator.invalidate(
                &package_id,
                nexa_analysis::DevelopmentInvalidation::SourceRemoval,
            );
            debug_assert_eq!(terminal.len(), 1);
            self.process_candidate_terminal(CandidateTerminal::CancelledBySourceRemoval(
                CandidateTerminalData {
                    source_id: self.packages[index].source_id.clone(),
                    identity: identity.clone(),
                    build_input: std::sync::Arc::clone(&self.packages[index].build_input),
                    queue_duration: Duration::ZERO,
                    work_duration: Duration::ZERO,
                },
            ));
            event_identity = Some(identity);
        }
        let source_id = self.packages[index].source_id.clone();
        let diagnostic = self.diagnostics.push(EngineDiagnostic::without_source(
            Some(package_id.clone()),
            Some(source_id),
            EngineDiagnosticStage::SourceDiscovery,
            nexa::ErrorCode::NX7001,
            message,
        ));
        self.packages[index].last_diagnostic = Some(diagnostic.summary());
        self.publish_event(DevelopmentEvent::SourceMissing(development_event_data(
            event_identity.expect("source removal always terminalizes a unique Generation"),
            Some(diagnostic),
        )));
    }

    fn retry_backpressured_jobs(&mut self) {
        let indexes = self
            .packages
            .iter()
            .enumerate()
            .filter_map(|(index, record)| record.awaiting_job.as_ref().map(|_| index))
            .collect::<Vec<_>>();
        for index in indexes {
            if let Some(job) = self.packages[index].awaiting_job.take() {
                self.try_enqueue_job(index, job);
            }
        }
    }

    fn try_enqueue_job(&mut self, index: usize, job: CompileJob) {
        let identity = job.identity.clone();
        match self.development_coordinator.enqueue(identity.clone()) {
            nexa_analysis::DevelopmentQueueOutcome::Accepted
            | nexa_analysis::DevelopmentQueueOutcome::AlreadyQueued => {}
            nexa_analysis::DevelopmentQueueOutcome::Backpressured(_) => {
                self.packages[index].development.state = DevelopmentState::AwaitingQueue;
                self.packages[index].awaiting_job = Some(job);
                return;
            }
            nexa_analysis::DevelopmentQueueOutcome::Stale(_) => {
                self.process_candidate_terminal(job.supersede_before_compile());
                return;
            }
        }
        let Some(worker) = self.development_worker.as_ref() else {
            self.process_candidate_terminal(job.cancel(CandidateCancellation::Shutdown));
            return;
        };
        match worker.enqueue(job) {
            EnqueueOutcome::Accepted => {
                self.mark_job_queued(index, identity);
            }
            EnqueueOutcome::ReplacedPending { terminal, .. } => {
                self.process_candidate_terminal(terminal);
                self.mark_job_queued(index, identity);
            }
            EnqueueOutcome::Backpressured { job } => {
                self.packages[index].development.state = DevelopmentState::AwaitingQueue;
                self.packages[index].awaiting_job = Some(job);
            }
            EnqueueOutcome::Stopping { job } => {
                self.process_candidate_terminal(job.cancel(CandidateCancellation::Shutdown));
            }
        }
    }

    fn mark_job_queued(&mut self, index: usize, identity: CandidateIdentity) {
        self.packages[index].development.queued_build_fingerprint =
            Some(identity.build_fingerprint);
        self.packages[index].development.queued_generation = Some(identity.generation);
        self.packages[index].development.state = DevelopmentState::CompileQueued;
        self.publish_event(DevelopmentEvent::CompileQueued(development_event_data(
            identity, None,
        )));
    }

    fn process_worker_activity(&mut self) {
        let Some(worker) = self.development_worker.as_ref() else {
            return;
        };
        let drain = worker.drain();
        self.process_worker_drain(drain);
    }

    #[cfg(test)]
    fn worker_test_control(&self) -> development::WorkerTestControl {
        self.development_worker
            .as_ref()
            .expect("the development Worker is enabled")
            .test_control()
    }

    fn process_worker_drain(&mut self, drain: development::WorkerDrain) {
        for event in drain.events {
            match event {
                WorkerEvent::CompileStarted {
                    source_id,
                    identity,
                    queue_duration,
                } => {
                    let _ = self.development_coordinator.start(&identity);
                    if let Some(index) = self.packages.iter().position(|record| {
                        record.candidate.manifest.id == identity.package_id
                            && record.source_id == source_id
                    }) {
                        if self.packages[index].development.queued_generation
                            == Some(identity.generation)
                            && self.packages[index].development.queued_build_fingerprint
                                == Some(identity.build_fingerprint)
                        {
                            self.packages[index].development.queued_build_fingerprint = None;
                            self.packages[index].development.queued_generation = None;
                        }
                        self.packages[index].development.in_flight_build_fingerprint =
                            Some(identity.build_fingerprint);
                        self.packages[index].development.in_flight_generation =
                            Some(identity.generation);
                        self.packages[index].development.last_queue_duration = queue_duration;
                        if identity.generation == self.packages[index].development.latest_generation
                            && self.packages[index].development.desired_build_fingerprint
                                == Some(identity.build_fingerprint)
                        {
                            self.packages[index].development.state = DevelopmentState::Compiling;
                        }
                    }
                    self.publish_event(DevelopmentEvent::CompileStarted(DevelopmentEventData {
                        queue_duration: Some(queue_duration),
                        ..development_event_data(identity, None)
                    }));
                }
            }
        }
        for terminal in drain.terminals {
            self.process_candidate_terminal(terminal);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_candidate_terminal(&mut self, terminal: CandidateTerminal) {
        let data = terminal.data().clone();
        let Some(index) = self.packages.iter().position(|record| {
            record.candidate.manifest.id == data.identity.package_id
                && record.source_id == data.source_id
        }) else {
            return;
        };
        let completed_attempt = matches!(
            &terminal,
            CandidateTerminal::Compiled { .. }
                | CandidateTerminal::CompileFailed { .. }
                | CandidateTerminal::VerifyFailed { .. }
        );
        let current_identity =
            !completed_attempt || self.candidate_identity_is_current(index, &data);
        let terminal = if completed_attempt && !current_identity {
            if self.packages[index].development.state == DevelopmentState::SourceMissing {
                CandidateTerminal::CancelledBySourceRemoval(data.clone())
            } else {
                if self.packages[index].development.desired_build_fingerprint
                    != Some(data.identity.build_fingerprint)
                {
                    self.packages[index]
                        .development
                        .desired_build_fingerprint_mismatch_rejection_count = self.packages[index]
                        .development
                        .desired_build_fingerprint_mismatch_rejection_count
                        .saturating_add(1);
                }
                CandidateTerminal::SupersededAfterCompile(data.clone())
            }
        } else if matches!(&terminal, CandidateTerminal::Compiled { .. })
            && !matches!(
                self.packages[index].lifecycle.status(),
                PackageStatus::Enabled | PackageStatus::Faulted
            )
        {
            CandidateTerminal::CancelledByDisable(data.clone())
        } else {
            terminal
        };

        match terminal {
            CandidateTerminal::Compiled {
                data,
                build_input,
                compilation,
            } => self.process_compiled_candidate(index, data, build_input, compilation),
            CandidateTerminal::CompileFailed {
                data,
                mut diagnostic,
                additional_diagnostics,
                compile_duration,
                verify_duration,
            }
            | CandidateTerminal::VerifyFailed {
                data,
                mut diagnostic,
                additional_diagnostics,
                compile_duration,
                verify_duration,
            } => {
                let verify = diagnostic.stage == EngineDiagnosticStage::Verify;
                diagnostic.context.candidate_generation = Some(data.identity.generation);
                let diagnostic = self.diagnostics.push(diagnostic);
                self.packages[index].last_diagnostic = Some(diagnostic.summary());
                for mut additional in additional_diagnostics {
                    additional.context.candidate_generation = Some(data.identity.generation);
                    self.diagnostics.push(additional);
                }
                self.packages[index].development.last_compile_duration = Some(compile_duration);
                self.packages[index].development.last_verify_duration = verify_duration;
                self.packages[index].development.state = if verify {
                    DevelopmentState::VerifyFailed
                } else {
                    DevelopmentState::CompileFailed
                };
                self.record_generation_terminal(
                    index,
                    &data,
                    if verify {
                        CandidateTerminalKind::VerifyFailed
                    } else {
                        CandidateTerminalKind::CompileFailed
                    },
                );
                self.publish_event(if verify {
                    DevelopmentEvent::VerifyFailed(development_event_data(
                        data.identity.clone(),
                        Some(diagnostic),
                    ))
                } else {
                    DevelopmentEvent::CompileFailed(development_event_data(
                        data.identity.clone(),
                        Some(diagnostic),
                    ))
                });
                self.publish_reload(reload_report(
                    data.identity,
                    0,
                    None,
                    compile_duration,
                    verify_duration,
                    nexa_runtime::RestartReloadMetrics::default(),
                    if verify {
                        ReloadReportOutcome::VerifyFailed
                    } else {
                        ReloadReportOutcome::CompileFailed
                    },
                    0,
                    0,
                ));
            }
            CandidateTerminal::SupersededBeforeCompile(data) => {
                self.finish_noncompiled_terminal(
                    index,
                    data,
                    CandidateTerminalKind::SupersededBeforeCompile,
                    ReloadReportOutcome::Superseded,
                    true,
                );
            }
            CandidateTerminal::SupersededAfterCompile(data) => {
                self.finish_noncompiled_terminal(
                    index,
                    data,
                    CandidateTerminalKind::SupersededAfterCompile,
                    ReloadReportOutcome::Superseded,
                    true,
                );
            }
            CandidateTerminal::CancelledByDisable(data) => {
                self.finish_noncompiled_terminal(
                    index,
                    data,
                    CandidateTerminalKind::CancelledByDisable,
                    ReloadReportOutcome::Superseded,
                    false,
                );
            }
            CandidateTerminal::CancelledBySourceRemoval(data) => {
                self.finish_noncompiled_terminal(
                    index,
                    data,
                    CandidateTerminalKind::CancelledBySourceRemoval,
                    ReloadReportOutcome::Superseded,
                    false,
                );
            }
            CandidateTerminal::CancelledByShutdown(data) => {
                self.finish_noncompiled_terminal(
                    index,
                    data,
                    CandidateTerminalKind::CancelledByShutdown,
                    ReloadReportOutcome::Superseded,
                    false,
                );
            }
            CandidateTerminal::RejectedHostContractChange(data) => {
                self.packages[index].development.state = DevelopmentState::HostRebuildRequired;
                self.record_generation_terminal(
                    index,
                    &data,
                    CandidateTerminalKind::RejectedHostContractChange,
                );
                self.publish_event(DevelopmentEvent::HostRebuildRequired(
                    development_event_data(data.identity.clone(), None),
                ));
                self.publish_reload(reload_report(
                    data.identity,
                    0,
                    None,
                    data.work_duration,
                    Duration::ZERO,
                    nexa_runtime::RestartReloadMetrics::default(),
                    ReloadReportOutcome::HostRebuildRequired,
                    0,
                    0,
                ));
            }
        }
    }

    fn finish_noncompiled_terminal(
        &mut self,
        index: usize,
        data: CandidateTerminalData,
        kind: CandidateTerminalKind,
        outcome: ReloadReportOutcome,
        superseded: bool,
    ) {
        let settles_stale_latest = data.identity.generation
            == self.packages[index].development.latest_generation
            && self.packages[index].development.desired_build_fingerprint
                != Some(data.identity.build_fingerprint);
        let source_is_missing =
            self.packages[index].development.state == DevelopmentState::SourceMissing;
        self.record_generation_terminal(index, &data, kind);
        if settles_stale_latest {
            self.clear_unqueued_observation(index);
            if !source_is_missing {
                self.packages[index].development.state = DevelopmentState::Idle;
            }
        }
        self.publish_event(if superseded {
            DevelopmentEvent::CandidateSuperseded(development_event_data(
                data.identity.clone(),
                None,
            ))
        } else {
            DevelopmentEvent::CandidateCancelled(development_event_data(
                data.identity.clone(),
                None,
            ))
        });
        self.publish_reload(reload_report(
            data.identity,
            0,
            None,
            data.work_duration,
            Duration::ZERO,
            nexa_runtime::RestartReloadMetrics::default(),
            outcome,
            0,
            0,
        ));
    }

    fn process_compiled_candidate(
        &mut self,
        index: usize,
        data: CandidateTerminalData,
        build_input: std::sync::Arc<nexa_analysis::ResolvedBuildInput>,
        compilation: CandidateCompilation,
    ) {
        if !self.candidate_identity_is_current(index, &data) {
            self.finish_candidate_rejected_by_freshness(index, data);
            return;
        }
        self.packages[index].development.last_compile_duration = Some(compilation.compile_duration);
        self.packages[index].development.last_verify_duration = compilation.verify_duration;
        self.publish_event(DevelopmentEvent::CompileSucceeded(development_event_data(
            data.identity.clone(),
            None,
        )));
        self.packages[index].development.state = DevelopmentState::CandidateReady;
        self.publish_event(DevelopmentEvent::CandidateReady(development_event_data(
            data.identity.clone(),
            None,
        )));
        if !self.development.auto_reload {
            self.packages[index].ready_candidate = Some(development::ReadyCandidate {
                build_input,
                compilation,
                terminal_data: data,
            });
            return;
        }
        self.commit_compiled_from_tick(index, data, build_input, compilation);
    }

    fn commit_compiled_from_tick(
        &mut self,
        index: usize,
        data: CandidateTerminalData,
        build_input: std::sync::Arc<nexa_analysis::ResolvedBuildInput>,
        compilation: CandidateCompilation,
    ) {
        if !self.candidate_identity_is_current(index, &data) {
            self.finish_candidate_rejected_by_freshness(index, data);
            return;
        }
        let terminal_kind =
            self.record_generation_terminal(index, &data, CandidateTerminalKind::Compiled);
        if terminal_kind != CandidateTerminalKind::Compiled {
            self.finish_recorded_candidate_rejection(
                index,
                data,
                terminal_kind,
                compilation.compile_duration,
                compilation.verify_duration,
            );
            return;
        }
        self.packages[index].development.state = DevelopmentState::ReloadPending;
        self.publish_event(DevelopmentEvent::ReloadStarted(development_event_data(
            data.identity.clone(),
            None,
        )));
        self.packages[index].development.state = DevelopmentState::Reloading;
        let reload_started = std::time::Instant::now();
        let candidate = build_input
            .candidate()
            .expect("a resolved build input always yields its canonical candidate");
        match self.commit_compiled_candidate(
            index,
            candidate,
            build_input,
            compilation.artifact,
            compilation.compile_duration,
            compilation.verify_duration,
        ) {
            Ok(report) => {
                let mut report = report;
                self.finish_reload_metrics(index, &mut report);
                self.packages[index].development.state = DevelopmentState::Reloaded;
                self.packages[index].development.last_reload_duration =
                    Some(reload_started.elapsed());
                self.packages[index].development.last_migration_duration =
                    report.migration_duration;
                self.packages[index].development.last_activation_duration =
                    report.activation_duration;
                self.publish_event(DevelopmentEvent::ReloadCommitted(DevelopmentEventData {
                    reload: Some(report.summary()),
                    ..development_event_data(data.identity, None)
                }));
                self.publish_reload(report);
            }
            Err(failure) => {
                let mut failure = failure;
                self.finish_reload_metrics(index, &mut failure.report);
                self.packages[index].development.last_migration_duration =
                    failure.report.migration_duration;
                self.packages[index].development.last_activation_duration =
                    failure.report.activation_duration;
                let activation = failure.report.outcome == ReloadReportOutcome::ActivationFaulted;
                self.packages[index].development.state = if activation {
                    DevelopmentState::ActivationFaulted
                } else {
                    DevelopmentState::MigrationFailed
                };
                let event_data = DevelopmentEventData {
                    reload: Some(failure.report.summary()),
                    ..development_event_data(data.identity, None)
                };
                self.publish_event(if activation {
                    DevelopmentEvent::ActivationFaulted(event_data)
                } else {
                    DevelopmentEvent::ReloadRolledBack(event_data)
                });
                self.publish_reload(failure.report);
            }
        }
    }

    fn finish_reload_metrics(&mut self, index: usize, report: &mut ReloadReport) {
        report.change_to_stable_duration = self.packages[index]
            .development
            .last_change_to_stable_duration;
        report.queue_duration = self.packages[index].development.last_queue_duration;
        let elapsed = self.packages[index]
            .development
            .change_observed_at
            .map_or_else(
                || {
                    report
                        .compile_duration
                        .saturating_add(report.verify_duration)
                        .saturating_add(report.reload_duration)
                },
                |observed| observed.elapsed(),
            );
        let measured_without_ready = report
            .change_to_stable_duration
            .saturating_add(report.queue_duration)
            .saturating_add(report.compile_duration)
            .saturating_add(report.verify_duration)
            .saturating_add(report.reload_duration);
        report.ready_to_commit_duration = elapsed.saturating_sub(measured_without_ready);
        report.total_change_to_visible_duration =
            measured_without_ready.saturating_add(report.ready_to_commit_duration);
        self.packages[index]
            .development
            .last_ready_to_commit_duration = report.ready_to_commit_duration;
        self.packages[index].development.last_quiesce_duration = report.quiesce_duration;
        self.packages[index].development.last_commit_duration = report.commit_duration;
        self.packages[index]
            .development
            .last_total_change_to_visible_duration = report.total_change_to_visible_duration;
    }

    fn finish_recorded_candidate_rejection(
        &mut self,
        index: usize,
        data: CandidateTerminalData,
        kind: CandidateTerminalKind,
        compile_duration: Duration,
        verify_duration: Duration,
    ) {
        let host_rebuild = kind == CandidateTerminalKind::RejectedHostContractChange;
        let source_missing = kind == CandidateTerminalKind::CancelledBySourceRemoval;
        self.packages[index].development.state = if host_rebuild {
            DevelopmentState::HostRebuildRequired
        } else if source_missing {
            DevelopmentState::SourceMissing
        } else {
            DevelopmentState::Idle
        };
        if source_missing {
            self.packages[index].development.desired_build_fingerprint = None;
            self.packages[index].development.observed_build_fingerprint = None;
            self.packages[index].development.stable_build_fingerprint = None;
            let diagnostic = self.diagnostics.push(EngineDiagnostic::without_source(
                Some(data.identity.package_id.clone()),
                Some(data.source_id.clone()),
                EngineDiagnosticStage::SourceDiscovery,
                nexa::ErrorCode::NX7001,
                "Package source disappeared during the final Candidate freshness check",
            ));
            self.packages[index].last_diagnostic = Some(diagnostic.summary());
            self.publish_event(DevelopmentEvent::SourceMissing(development_event_data(
                data.identity.clone(),
                Some(diagnostic),
            )));
        } else {
            self.publish_event(if host_rebuild {
                DevelopmentEvent::HostRebuildRequired(development_event_data(
                    data.identity.clone(),
                    None,
                ))
            } else if matches!(
                kind,
                CandidateTerminalKind::CancelledByDisable
                    | CandidateTerminalKind::CancelledBySourceRemoval
                    | CandidateTerminalKind::CancelledByShutdown
            ) {
                DevelopmentEvent::CandidateCancelled(development_event_data(
                    data.identity.clone(),
                    None,
                ))
            } else {
                DevelopmentEvent::CandidateSuperseded(development_event_data(
                    data.identity.clone(),
                    None,
                ))
            });
        }
        self.publish_reload(reload_report(
            data.identity,
            0,
            None,
            compile_duration,
            verify_duration,
            nexa_runtime::RestartReloadMetrics::default(),
            if host_rebuild {
                ReloadReportOutcome::HostRebuildRequired
            } else {
                ReloadReportOutcome::Superseded
            },
            0,
            0,
        ));
    }

    fn finish_candidate_rejected_by_freshness(
        &mut self,
        index: usize,
        data: CandidateTerminalData,
    ) {
        if self.packages[index].development.state == DevelopmentState::SourceMissing {
            self.finish_noncompiled_terminal(
                index,
                data,
                CandidateTerminalKind::CancelledBySourceRemoval,
                ReloadReportOutcome::Superseded,
                false,
            );
        } else {
            if self.packages[index].development.desired_build_fingerprint
                != Some(data.identity.build_fingerprint)
            {
                self.packages[index]
                    .development
                    .desired_build_fingerprint_mismatch_rejection_count = self.packages[index]
                    .development
                    .desired_build_fingerprint_mismatch_rejection_count
                    .saturating_add(1);
            }
            self.finish_noncompiled_terminal(
                index,
                data,
                CandidateTerminalKind::SupersededAfterCompile,
                ReloadReportOutcome::Superseded,
                true,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn record_generation_terminal(
        &mut self,
        index: usize,
        data: &CandidateTerminalData,
        kind: CandidateTerminalKind,
    ) -> CandidateTerminalKind {
        self.record_shared_generation_terminal(index, data, kind);
        let kind = if matches!(
            kind,
            CandidateTerminalKind::Compiled
                | CandidateTerminalKind::CompileFailed
                | CandidateTerminalKind::VerifyFailed
                | CandidateTerminalKind::RejectedHostContractChange
        ) {
            self.development_coordinator
                .terminal(&data.identity)
                .map_or(kind, |terminal| match terminal.kind {
                    nexa_analysis::DevelopmentTerminalKind::Compiled => {
                        CandidateTerminalKind::Compiled
                    }
                    nexa_analysis::DevelopmentTerminalKind::CompileFailed => {
                        CandidateTerminalKind::CompileFailed
                    }
                    nexa_analysis::DevelopmentTerminalKind::VerifyFailed => {
                        CandidateTerminalKind::VerifyFailed
                    }
                    nexa_analysis::DevelopmentTerminalKind::SupersededBeforeCompile => {
                        CandidateTerminalKind::SupersededBeforeCompile
                    }
                    nexa_analysis::DevelopmentTerminalKind::SupersededInFlight
                    | nexa_analysis::DevelopmentTerminalKind::CancelledByInvalidation => {
                        CandidateTerminalKind::SupersededAfterCompile
                    }
                    nexa_analysis::DevelopmentTerminalKind::CancelledBySourceRemoval => {
                        CandidateTerminalKind::CancelledBySourceRemoval
                    }
                    nexa_analysis::DevelopmentTerminalKind::CancelledByShutdown => {
                        CandidateTerminalKind::CancelledByShutdown
                    }
                    nexa_analysis::DevelopmentTerminalKind::RejectedHostContractChange => {
                        CandidateTerminalKind::RejectedHostContractChange
                    }
                })
        } else {
            kind
        };
        let previous = self.packages[index]
            .development
            .terminal_generations
            .insert(data.identity.generation, kind);
        if previous.is_some() {
            self.packages[index].development.duplicate_terminal_count = self.packages[index]
                .development
                .duplicate_terminal_count
                .saturating_add(1);
        }
        assert!(
            previous.is_none(),
            "Candidate generation {} for {} received two terminal outcomes",
            data.identity.generation,
            data.identity.package_id
        );
        self.packages[index].development.terminal_count = self.packages[index]
            .development
            .terminal_count
            .saturating_add(1);
        while self.packages[index].development.terminal_generations.len() > 128 {
            let Some(oldest) = self.packages[index]
                .development
                .terminal_generations
                .first_key_value()
                .map(|(generation, _)| *generation)
            else {
                break;
            };
            self.packages[index]
                .development
                .terminal_generations
                .remove(&oldest);
        }
        if self.packages[index].development.queued_generation == Some(data.identity.generation)
            && self.packages[index].development.queued_build_fingerprint
                == Some(data.identity.build_fingerprint)
        {
            self.packages[index].development.queued_build_fingerprint = None;
            self.packages[index].development.queued_generation = None;
        }
        if self.packages[index].development.in_flight_generation == Some(data.identity.generation)
            && self.packages[index].development.in_flight_build_fingerprint
                == Some(data.identity.build_fingerprint)
        {
            self.packages[index].development.in_flight_build_fingerprint = None;
            self.packages[index].development.in_flight_generation = None;
        }
        let records_terminal_build_fingerprint =
            matches!(
                kind,
                CandidateTerminalKind::Compiled
                    | CandidateTerminalKind::CompileFailed
                    | CandidateTerminalKind::VerifyFailed
                    | CandidateTerminalKind::RejectedHostContractChange
            ) && self.packages[index].development.desired_build_fingerprint
                == Some(data.identity.build_fingerprint);
        if data.identity.generation == self.packages[index].development.latest_generation
            && records_terminal_build_fingerprint
        {
            self.packages[index].development.terminal_build_fingerprint =
                Some(data.identity.build_fingerprint);
        }
        kind
    }

    fn record_shared_generation_terminal(
        &mut self,
        index: usize,
        data: &CandidateTerminalData,
        kind: CandidateTerminalKind,
    ) {
        if self
            .development_coordinator
            .terminal(&data.identity)
            .is_some()
        {
            return;
        }
        let (completion, retained_host_contract_changed) = match kind {
            CandidateTerminalKind::Compiled => {
                (nexa_analysis::DevelopmentCompletionKind::Compiled, false)
            }
            CandidateTerminalKind::CompileFailed => (
                nexa_analysis::DevelopmentCompletionKind::CompileFailed,
                false,
            ),
            CandidateTerminalKind::VerifyFailed => (
                nexa_analysis::DevelopmentCompletionKind::VerifyFailed,
                false,
            ),
            CandidateTerminalKind::RejectedHostContractChange => {
                (nexa_analysis::DevelopmentCompletionKind::Compiled, true)
            }
            CandidateTerminalKind::SupersededBeforeCompile
            | CandidateTerminalKind::SupersededAfterCompile
            | CandidateTerminalKind::CancelledByDisable => {
                let package_id = data.identity.package_id.clone();
                let _ = self.development_coordinator.invalidate(
                    &package_id,
                    nexa_analysis::DevelopmentInvalidation::Transient,
                );
                return;
            }
            CandidateTerminalKind::CancelledBySourceRemoval => {
                let package_id = data.identity.package_id.clone();
                let _ = self.development_coordinator.invalidate(
                    &package_id,
                    nexa_analysis::DevelopmentInvalidation::SourceRemoval,
                );
                return;
            }
            CandidateTerminalKind::CancelledByShutdown => {
                let _ = self.development_coordinator.shutdown();
                return;
            }
        };
        let Ok(current) = self.fresh_candidate(index) else {
            let package_id = data.identity.package_id.clone();
            let _ = self.development_coordinator.invalidate(
                &package_id,
                nexa_analysis::DevelopmentInvalidation::SourceRemoval,
            );
            return;
        };
        let _ = self.development_coordinator.complete(
            data.identity.clone(),
            &data.build_input,
            &current.build_input,
            completion,
            retained_host_contract_changed,
        );
    }

    fn terminate_unqueued_generation(&mut self, index: usize, kind: CandidateTerminalKind) -> bool {
        let Some(data) = self.packages[index].development.unqueued_generation.take() else {
            return false;
        };
        let terminal = match kind {
            CandidateTerminalKind::SupersededBeforeCompile => {
                CandidateTerminal::SupersededBeforeCompile(data)
            }
            CandidateTerminalKind::CancelledByDisable => {
                CandidateTerminal::CancelledByDisable(data)
            }
            CandidateTerminalKind::CancelledBySourceRemoval => {
                CandidateTerminal::CancelledBySourceRemoval(data)
            }
            CandidateTerminalKind::CancelledByShutdown => {
                CandidateTerminal::CancelledByShutdown(data)
            }
            _ => unreachable!("an unqueued Candidate can only be superseded or cancelled"),
        };
        self.process_candidate_terminal(terminal);
        true
    }

    fn clear_unqueued_observation(&mut self, index: usize) {
        self.packages[index].development.observed_build_fingerprint = None;
        self.packages[index].development.stable_build_fingerprint = None;
        self.packages[index].development.stable_scans = 0;
        self.packages[index].development.change_observed_at = None;
        if matches!(
            self.packages[index].development.state,
            DevelopmentState::ChangeObserved | DevelopmentState::WaitingForStableWrite
        ) {
            self.packages[index].development.state = DevelopmentState::Idle;
        }
    }

    fn supersede_development_for_current_source(
        &mut self,
        index: usize,
        desired_build_fingerprint: Option<BuildFingerprint>,
    ) {
        if self.packages[index]
            .development
            .unqueued_generation
            .as_ref()
            .is_some_and(|data| Some(data.identity.build_fingerprint) != desired_build_fingerprint)
        {
            self.terminate_unqueued_generation(
                index,
                CandidateTerminalKind::SupersededBeforeCompile,
            );
        }
        let package_id = self.packages[index].candidate.manifest.id.clone();
        let mut terminals = self
            .development_worker
            .as_ref()
            .map_or_else(Vec::new, |worker| {
                worker.supersede_package_except(&package_id, desired_build_fingerprint)
            });
        if self.packages[index]
            .awaiting_job
            .as_ref()
            .is_some_and(|job| Some(job.identity.build_fingerprint) != desired_build_fingerprint)
        {
            let job = self.packages[index]
                .awaiting_job
                .take()
                .expect("the stale awaiting Job was observed");
            terminals.push(job.supersede_before_compile());
        }
        if self.packages[index]
            .ready_candidate
            .as_ref()
            .is_some_and(|ready| {
                Some(ready.terminal_data.identity.build_fingerprint) != desired_build_fingerprint
            })
        {
            let ready = self.packages[index]
                .ready_candidate
                .take()
                .expect("the stale Ready Candidate was observed");
            terminals.push(CandidateTerminal::SupersededAfterCompile(
                ready.terminal_data,
            ));
            self.packages[index].ready_commit_requested = false;
        }
        for terminal in terminals {
            self.process_candidate_terminal(terminal);
        }
    }

    fn cancel_development(&mut self, index: usize, reason: CandidateCancellation) {
        let unqueued_kind = match reason {
            CandidateCancellation::Disable => CandidateTerminalKind::CancelledByDisable,
            CandidateCancellation::SourceRemoval => CandidateTerminalKind::CancelledBySourceRemoval,
            CandidateCancellation::Shutdown => CandidateTerminalKind::CancelledByShutdown,
        };
        self.terminate_unqueued_generation(index, unqueued_kind);
        let package_id = self.packages[index].candidate.manifest.id.clone();
        let mut terminals = self
            .development_worker
            .as_ref()
            .map_or_else(Vec::new, |worker| {
                worker.cancel_package(&package_id, reason)
            });
        if let Some(job) = self.packages[index].awaiting_job.take() {
            terminals.push(job.cancel(reason));
        }
        if let Some(ready) = self.packages[index].ready_candidate.take() {
            terminals.push(match reason {
                CandidateCancellation::Disable => {
                    CandidateTerminal::CancelledByDisable(ready.terminal_data)
                }
                CandidateCancellation::SourceRemoval => {
                    CandidateTerminal::CancelledBySourceRemoval(ready.terminal_data)
                }
                CandidateCancellation::Shutdown => {
                    CandidateTerminal::CancelledByShutdown(ready.terminal_data)
                }
            });
        }
        self.packages[index].ready_commit_requested = false;
        for terminal in terminals {
            self.process_candidate_terminal(terminal);
        }
        self.clear_unqueued_observation(index);
    }

    fn commit_requested_ready_candidates(&mut self) {
        let indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.ready_commit_requested && record.ready_candidate.is_some())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        for index in indexes {
            self.packages[index].ready_commit_requested = false;
            let Some(ready) = self.packages[index].ready_candidate.take() else {
                continue;
            };
            self.commit_compiled_from_tick(
                index,
                ready.terminal_data,
                ready.build_input,
                ready.compilation,
            );
        }
    }

    fn publish_event(&mut self, event: DevelopmentEvent) {
        let retain = self.development.retain_events.max(1);
        while self.development_events.len() >= retain {
            self.development_events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.development_events.push_back(event.clone());
        while self.pending_development_events.len() >= retain {
            self.pending_development_events.remove(0);
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.pending_development_events.push(event);
    }

    pub fn call<E: nexa_runtime::ScriptExport>(
        &mut self,
        id: &PackageId,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EngineError> {
        let index = self.unique_index(id)?;
        self.call_index::<E>(index, args)
    }

    /// Reports whether a package's active or Last-Known-Good artifact implements `E`.
    ///
    /// This is deliberately typed: entrypoint names are never resolved from caller-provided
    /// strings.
    #[must_use]
    pub fn has_export<E: nexa_runtime::ScriptExport>(&self, id: &PackageId) -> bool {
        let Ok(index) = self.unique_index(id) else {
            return false;
        };
        self.package_entrypoint(index, E::STABLE_ID)
            .is_some_and(|(signature, effect)| {
                signature == E::signature() && effect_satisfies_declaration(effect, E::effect())
            })
    }

    /// Calls `E` when the selected package implements it.
    ///
    /// `None` means the entrypoint is absent. A present entrypoint preserves the ordinary call
    /// result, including package state and runtime failures.
    pub fn call_optional<E: nexa_runtime::ScriptExport>(
        &mut self,
        id: &PackageId,
        args: &E::Args,
    ) -> Option<Result<PackageOutput<E::Output>, EngineError>> {
        let index = match self.unique_index(id) {
            Ok(index) => index,
            Err(error) => return Some(Err(error)),
        };
        let (signature, effect) = self.package_entrypoint(index, E::STABLE_ID)?;
        if signature != E::signature() || !effect_satisfies_declaration(effect, E::effect()) {
            return Some(Err(EngineError::ExportSignature(
                id.clone(),
                E::NAME.to_owned(),
            )));
        }
        Some(self.call_index::<E>(index, args))
    }

    /// Deterministically broadcasts `E` to enabled packages which actually implement it.
    pub fn dispatch_optional<E: nexa_runtime::ScriptExport>(
        &mut self,
        args: &E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EngineError>> {
        let mut indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(index, record)| {
                record.lifecycle.status() == PackageStatus::Enabled
                    && self.package_entrypoint(*index, E::STABLE_ID).is_some_and(
                        |(signature, effect)| {
                            signature == E::signature()
                                && effect_satisfies_declaration(effect, E::effect())
                        },
                    )
            })
            .map(|(index, record)| {
                (
                    index,
                    record.effective.priority,
                    record.candidate.manifest.id.clone(),
                )
            })
            .collect::<Vec<_>>();
        indexes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
        indexes
            .into_iter()
            .map(|(index, _, _)| self.call_index::<E>(index, args))
            .collect()
    }

    fn package_entrypoint(
        &self,
        index: usize,
        stable_id: nexa::StableId,
    ) -> Option<(nexa_runtime::Signature, nexa_runtime::FunctionEffect)> {
        let record = &self.packages[index];
        let artifact = record
            .runtime
            .as_ref()
            .map(|runtime| &runtime.artifact)
            .or_else(|| {
                record
                    .last_known_good
                    .as_ref()
                    .map(|known_good| &known_good.artifact)
            })?;
        let entrypoint = artifact
            .module()
            .exports
            .iter()
            .find(|entrypoint| entrypoint.stable_id == stable_id)?;
        let function = artifact
            .module()
            .functions
            .get(usize::try_from(entrypoint.function).ok()?)?;
        Some((entrypoint.signature.clone(), function.effect))
    }

    fn call_index<E: nexa_runtime::ScriptExport>(
        &mut self,
        index: usize,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EngineError> {
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EngineError::InvalidState(
                record.candidate.manifest.id.clone(),
                record.lifecycle.status(),
            ));
        }
        let manifest = &record.candidate.manifest;
        let policy = nexa_runtime::MustCompletePolicy {
            fuel: record.effective.runtime_limits.handler_fuel,
            cumulative_budget: record.effective.runtime_limits.cumulative_budget,
        };
        let runtime = record.runtime.as_mut().ok_or_else(|| {
            EngineError::InvalidState(manifest.id.clone(), PackageStatus::Enabled)
        })?;
        record.handler_calls_this_tick = record.handler_calls_this_tick.saturating_add(1);
        // WP89: exports whose module function is `@immediate` skip the
        // Task/scheduler/tombstone lifecycle entirely (the realm settles
        // them in one predecoded poll). Routing keys off the module's
        // verified effect - not `E::effect()` - because a module may
        // strengthen an Ordinary declaration to `@immediate` and every
        // Ordinary caller must still benefit. All other effects keep the
        // metered Task path.
        let immediate = runtime
            .artifact
            .module()
            .exports
            .iter()
            .find(|export| export.stable_id == E::STABLE_ID)
            .is_some_and(|export| export.effect == nexa_runtime::FunctionEffect::Immediate);
        let called = if immediate {
            runtime
                .realm
                .call_export_immediate::<E>(runtime.module, args, policy)
        } else {
            runtime
                .realm
                .call_export_metered::<E>(runtime.module, runtime.root_scope, args, policy)
        };
        match called {
            Ok((value, charge)) => {
                record.handler_instructions_this_tick = record
                    .handler_instructions_this_tick
                    .saturating_add(charge.instructions);
                record.fuel_used_this_tick =
                    record.fuel_used_this_tick.saturating_add(charge.fuel_used);
                record.outputs_this_tick = record.outputs_this_tick.saturating_add(1);
                Ok(PackageOutput {
                    package_id: manifest.id.clone(),
                    source_id: record.source_id.clone(),
                    trust: record.policy.trust,
                    capabilities: record.effective.capabilities.clone(),
                    value,
                })
            }
            Err(call_error) => {
                let trap_diagnostic = match &call_error {
                    nexa_runtime::ScriptCallError::HandlerTrapped(trap) => Some(
                        runtime_trap_diagnostic(record, &self.contract, &self.idl, trap, E::NAME),
                    ),
                    _ => None,
                };
                let code = match &call_error {
                    nexa_runtime::ScriptCallError::HandlerDidNotComplete => nexa::ErrorCode::NX7101,
                    nexa_runtime::ScriptCallError::HostWaitNotAllowed => nexa::ErrorCode::NX7102,
                    _ => nexa::ErrorCode::NX7103,
                };
                let error = EngineError::Handler(manifest.id.clone(), call_error.to_string());
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
                let summary = if let Some(diagnostic) = trap_diagnostic {
                    self.diagnostics.push(diagnostic).summary()
                } else {
                    self.record_diagnostic(
                        index,
                        EngineDiagnosticStage::Handler,
                        code,
                        error.to_string(),
                    )
                };
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(error)
            }
        }
    }

    pub fn dispatch<E: nexa_runtime::ScriptExport>(
        &mut self,
        args: &E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EngineError>> {
        self.refresh_dispatch_plan();
        // The plan is moved out for the duration of the calls (call_index
        // needs `&mut self`) and restored afterwards; steady-state
        // dispatches therefore allocate nothing beyond the output vector.
        let plan = std::mem::take(&mut self.dispatch_plan);
        let outputs = plan
            .iter()
            .map(|&index| self.call_index::<E>(index, args))
            .collect();
        self.dispatch_plan = plan;
        outputs
    }

    /// Deterministically dispatches while allowing package-specific immutable
    /// arguments, such as a projection of that package's typed state.
    pub fn dispatch_with<E: nexa_runtime::ScriptExport>(
        &mut self,
        mut args: impl FnMut(&PackageInfo) -> E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EngineError>> {
        self.refresh_dispatch_plan();
        let plan = std::mem::take(&mut self.dispatch_plan);
        let outputs = plan
            .iter()
            .map(|&index| {
                let package = self.packages[index].info();
                self.call_index::<E>(index, &args(&package))
            })
            .collect();
        self.dispatch_plan = plan;
        outputs
    }

    /// WP90: rebuilds the broadcast plan only when the revalidation scan
    /// says the cached one no longer matches the live package table.
    fn refresh_dispatch_plan(&mut self) {
        if self.dispatch_plan_is_current() {
            return;
        }
        let packages = &self.packages;
        self.dispatch_plan.clear();
        self.dispatch_plan.extend(
            packages
                .iter()
                .enumerate()
                .filter(|(_, record)| record.lifecycle.status() == PackageStatus::Enabled)
                .map(|(index, _)| index),
        );
        self.dispatch_plan.sort_by(|&left, &right| {
            let left = &packages[left];
            let right = &packages[right];
            right
                .effective
                .priority
                .cmp(&left.effective.priority)
                .then_with(|| left.candidate.manifest.id.cmp(&right.candidate.manifest.id))
        });
    }

    /// The allocation-free O(n) plan check: every cached index must still
    /// be an Enabled package, the walk must strictly follow the broadcast
    /// order (priority descending, package id ascending - duplicates are
    /// conservatively treated as stale), and the Enabled population must
    /// match the plan length so no newly enabled package is missing.
    fn dispatch_plan_is_current(&self) -> bool {
        let enabled = self
            .packages
            .iter()
            .filter(|record| record.lifecycle.status() == PackageStatus::Enabled)
            .count();
        if enabled != self.dispatch_plan.len() {
            return false;
        }
        let mut previous: Option<&PackageRecord> = None;
        for &index in &self.dispatch_plan {
            let Some(record) = self.packages.get(index) else {
                return false;
            };
            if record.lifecycle.status() != PackageStatus::Enabled {
                return false;
            }
            if let Some(previous) = previous {
                let ordered = previous.effective.priority > record.effective.priority
                    || (previous.effective.priority == record.effective.priority
                        && previous.candidate.manifest.id < record.candidate.manifest.id);
                if !ordered {
                    return false;
                }
            }
            previous = Some(record);
        }
        true
    }

    /// Inserts or replaces one scalar field in a package's typed state domain.
    ///
    /// The names use the same stable-ID derivation as `@state(version = N)` state classes.
    pub fn set_state_i32(
        &mut self,
        id: &PackageId,
        state_key: &str,
        type_name: &str,
        version: u32,
        field_name: &str,
        value: i32,
    ) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EngineError::InvalidState(
                id.clone(),
                record.lifecycle.status(),
            ));
        }
        let runtime = record
            .runtime
            .as_mut()
            .ok_or_else(|| EngineError::InvalidState(id.clone(), PackageStatus::Enabled))?;
        let state_type = runtime
            .artifact
            .unique_state_type_named(type_name)
            .filter(|state| state.version == version)
            .ok_or_else(|| {
                EngineError::State(
                    id.clone(),
                    format!("unknown or ambiguous state type {type_name} at version {version}"),
                )
            })?;
        let field = state_type
            .fields
            .iter()
            .find(|field| field.name == field_name)
            .ok_or_else(|| {
                EngineError::State(
                    id.clone(),
                    format!("unknown state field {type_name}.{field_name}"),
                )
            })?;
        runtime
            .realm
            .insert_state(
                runtime.module,
                nexa_core::StableId::from_name(state_key),
                nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
                    type_id: state_type.stable_id.0,
                    version,
                    fields: BTreeMap::from([(
                        field.stable_id.0,
                        nexa_runtime::StateValue::I32(value),
                    )]),
                }),
            )
            .map(|_| ())
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))
    }

    /// Reads one scalar field from a package's typed state domain.
    pub fn state_i32(
        &self,
        id: &PackageId,
        state_key: &str,
        type_name: &str,
        field_name: &str,
    ) -> Result<Option<i32>, EngineError> {
        let index = self.unique_index(id)?;
        let record = &self.packages[index];
        let Some(runtime) = &record.runtime else {
            return Ok(None);
        };
        let Some(state_type) = runtime.artifact.unique_state_type_named(type_name) else {
            return Ok(None);
        };
        let Some(field) = state_type
            .fields
            .iter()
            .find(|field| field.name == field_name)
        else {
            return Ok(None);
        };
        let stable_id = nexa_core::StableId::from_name(state_key);
        let Some(handle) = runtime
            .realm
            .state_handles(runtime.module)
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))?
            .into_iter()
            .find(|handle| handle.stable_id() == stable_id)
        else {
            return Ok(None);
        };
        let value = runtime
            .realm
            .resolve_state(runtime.module, handle)
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))?;
        let nexa_runtime::StateValue::Object(object) = value else {
            return Ok(None);
        };
        if object.type_id != state_type.stable_id.0 {
            return Ok(None);
        }
        Ok(match object.fields.get(&field.stable_id.0) {
            Some(nexa_runtime::StateValue::I32(value)) => Some(*value),
            _ => None,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn tick(&mut self) -> Result<EngineTickReport, EngineError> {
        self.require_open()?;
        self.ticks = self.ticks.saturating_add(1);
        for record in &mut self.packages {
            let ledger = record
                .runtime
                .as_ref()
                .map_or_else(nexa_runtime::RuntimeResourceLedger::default, |runtime| {
                    runtime.realm.resource_ledger()
                });
            while record.development.recent_metrics.len() >= 32 {
                record.development.recent_metrics.pop_front();
            }
            record.development.recent_metrics.push_back(PackageMetric {
                tick: self.ticks,
                discovery_duration: record.development.last_discovery_duration,
                build_fingerprint_duration: record.development.last_build_fingerprint_duration,
                change_to_stable_duration: record.development.last_change_to_stable_duration,
                candidate_queue_duration: record.development.last_queue_duration,
                compile_duration: record.development.last_compile_duration.unwrap_or_default(),
                verify_duration: record.development.last_verify_duration,
                ready_to_commit_duration: record.development.last_ready_to_commit_duration,
                quiesce_duration: record.development.last_quiesce_duration,
                reload_duration: record.development.last_reload_duration.unwrap_or_default(),
                migration_duration: record.development.last_migration_duration,
                commit_duration: record.development.last_commit_duration,
                activation_duration: record.development.last_activation_duration,
                total_change_to_visible_duration: record
                    .development
                    .last_total_change_to_visible_duration,
                handler_calls: record.handler_calls_this_tick,
                handler_instructions: record.handler_instructions_this_tick,
                fuel_used: record.fuel_used_this_tick,
                output_count: record.outputs_this_tick,
                task_peak: ledger.tasks,
                request_peak: ledger.requests,
            });
            record.handler_calls_this_tick = 0;
            record.handler_instructions_this_tick = 0;
            record.fuel_used_this_tick = 0;
            record.outputs_this_tick = 0;
        }
        // Drain only work that was already complete at the tick boundary. Jobs admitted by this
        // tick's scan are deliberately not observed until a later tick, so a fast compiler cannot
        // collapse change detection, admission, compilation, and terminal delivery into one
        // scheduler turn.
        self.process_worker_activity();
        self.commit_requested_ready_candidates();
        if self.development.enabled
            && self.development.scan_interval_ticks != 0
            && self
                .ticks
                .is_multiple_of(self.development.scan_interval_ticks)
        {
            self.scan_development_changes();
        }
        self.retry_backpressured_jobs();
        let mut runtime_failures = Vec::new();
        for (index, record) in self.packages.iter_mut().enumerate() {
            if let Some(runtime) = record.runtime.as_mut()
                && let Err(error) = runtime.realm.tick(nexa_runtime::TickBudget {
                    collect_garbage: true,
                    ..nexa_runtime::TickBudget::default()
                })
            {
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
                runtime_failures.push((index, error.to_string()));
            }
        }
        for (index, message) in runtime_failures {
            let summary = self.record_diagnostic(
                index,
                EngineDiagnosticStage::Runtime,
                nexa::ErrorCode::NX7103,
                message,
            );
            self.packages[index].last_diagnostic = Some(summary);
        }
        let released_before = self.delivered_release_records;
        self.drain_releases();
        let diagnostics = self
            .diagnostics
            .entries()
            .into_iter()
            .filter(|diagnostic| diagnostic.sequence > self.last_tick_diagnostic_sequence)
            .collect::<Vec<_>>();
        if let Some(sequence) = diagnostics.last().map(|diagnostic| diagnostic.sequence) {
            self.last_tick_diagnostic_sequence = sequence;
        }
        let faulted_packages = self
            .packages
            .iter()
            .filter(|record| record.lifecycle.status() == PackageStatus::Faulted)
            .map(|record| record.candidate.manifest.id.clone())
            .collect();
        Ok(EngineTickReport {
            development_events: std::mem::take(&mut self.pending_development_events),
            diagnostics,
            reloads: std::mem::take(&mut self.pending_reload_reports),
            faulted_packages,
            released_resources: usize::try_from(
                self.delivered_release_records
                    .saturating_sub(released_before),
            )
            .unwrap_or(usize::MAX),
        })
    }

    pub fn refresh_entitlements(&mut self) -> Result<(), EngineError> {
        for index in 0..self.packages.len() {
            let locked = self.packages[index]
                .effective
                .entitlement
                .as_ref()
                .is_some_and(|id| !self.entitlements.contains(id));
            let status = self.packages[index].lifecycle.status();
            if locked && status == PackageStatus::Enabled {
                self.disable_index(index, false)?;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Locked)?;
            } else if locked && status == PackageStatus::Disabled {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Locked)?;
            } else if !locked && status == PackageStatus::Locked {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Disabled)?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn packages(&self) -> Vec<PackageInfo> {
        self.packages.iter().map(PackageRecord::info).collect()
    }

    #[must_use]
    pub fn status(&self, id: &PackageId) -> Option<PackageStatus> {
        let mut matches = self
            .packages
            .iter()
            .filter(|record| record.candidate.manifest.id == *id);
        let status = matches.next()?.lifecycle.status();
        matches.next().is_none().then_some(status)
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<EngineDiagnostic> {
        self.diagnostics.entries()
    }

    #[must_use]
    pub fn health(&self) -> EngineHealth {
        let mut health = EngineHealth {
            delivered_release_records: self.delivered_release_records,
            ..EngineHealth::default()
        };
        for record in &self.packages {
            let Some(runtime) = &record.runtime else {
                continue;
            };
            health.enabled_packages += 1;
            let ledger = runtime.realm.resource_ledger();
            health.tasks = health.tasks.saturating_add(ledger.tasks);
            health.scopes = health.scopes.saturating_add(ledger.scopes);
            health.continuations = health.continuations.saturating_add(ledger.continuations);
            health.scheduler_tokens = health
                .scheduler_tokens
                .saturating_add(ledger.scheduler_tokens);
            health.requests = health.requests.saturating_add(ledger.requests);
            health.completion_reservations = health
                .completion_reservations
                .saturating_add(ledger.completion_reservations);
            health.tokens = health.tokens.saturating_add(ledger.tokens);
            health.snapshots = health.snapshots.saturating_add(ledger.snapshots);
            health.release_reservations = health
                .release_reservations
                .saturating_add(ledger.release_reservations);
            health.queued_releases = health
                .queued_releases
                .saturating_add(ledger.queued_releases);
            health.heap_objects = health.heap_objects.saturating_add(ledger.heap_objects);
            health.state_objects = health.state_objects.saturating_add(ledger.state_objects);
            health.retired_modules = health
                .retired_modules
                .saturating_add(ledger.retired_modules);
        }
        let host = self.runtime_host.close_status();
        health.host_pending_completions = host.pending_completions;
        health.host_pending_releases = host.pending_releases;
        health
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn inspection(&self) -> EngineInspection {
        let packages = self
            .packages
            .iter()
            .map(|record| {
                let (active_epoch, ledger) = record.runtime.as_ref().map_or_else(
                    || (None, nexa_runtime::RuntimeResourceLedger::default()),
                    |runtime| {
                        (
                            runtime.realm.active_module_epoch(runtime.module).ok(),
                            runtime.realm.resource_ledger(),
                        )
                    },
                );
                let artifact = record
                    .runtime
                    .as_ref()
                    .map(|runtime| &runtime.artifact)
                    .or_else(|| {
                        record
                            .last_known_good
                            .as_ref()
                            .map(|known_good| &known_good.artifact)
                    });
                let implemented_entrypoints = self
                    .declared_entrypoints
                    .iter()
                    .filter(|declared| {
                        artifact.is_some_and(|artifact| {
                            artifact.module().exports.iter().any(|implemented| {
                                implemented.stable_id == declared.stable_id
                                    && implemented.signature == declared.signature
                                    && usize::try_from(implemented.function)
                                        .ok()
                                        .and_then(|index| artifact.module().functions.get(index))
                                        .is_some_and(|function| function.effect == declared.effect)
                            })
                        })
                    })
                    .map(|entrypoint| entrypoint.name.clone())
                    .collect::<Vec<_>>();
                let required_entrypoints = self
                    .required_exports
                    .iter()
                    .map(|entrypoint| entrypoint.name.clone())
                    .collect::<Vec<_>>();
                let missing_required_entrypoints = required_entrypoints
                    .iter()
                    .filter(|required| !implemented_entrypoints.contains(required))
                    .cloned()
                    .collect::<Vec<_>>();
                let optional_entrypoint_signatures = self
                    .declared_entrypoints
                    .iter()
                    .filter(|entrypoint| {
                        implemented_entrypoints.contains(&entrypoint.name)
                            && !required_entrypoints.contains(&entrypoint.name)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                PackageInspection {
                    package_id: record.candidate.manifest.id.clone(),
                    source_id: record.source_id.clone(),
                    status: record.lifecycle.status(),
                    version: record.candidate.manifest.version.clone(),
                    effective_capabilities: record.effective.capabilities.clone(),
                    active_epoch,
                    active_identity: record
                        .last_known_good
                        .as_ref()
                        .map(|known_good| known_good.identity.clone()),
                    active_linked_state_fingerprint: record
                        .last_known_good
                        .as_ref()
                        .map(|known_good| known_good.linked_state_fingerprint),
                    build_fingerprint: record.candidate.build_fingerprint,
                    desired_build_fingerprint: record.development.desired_build_fingerprint,
                    candidate_generation: record.development.latest_generation,
                    terminal_generations: record.development.terminal_count,
                    duplicate_terminals: record.development.duplicate_terminal_count,
                    generations_without_terminal: record
                        .development
                        .latest_generation
                        .saturating_sub(record.development.terminal_count),
                    desired_build_fingerprint_mismatches_rejected: record
                        .development
                        .desired_build_fingerprint_mismatch_rejection_count,
                    latest_terminal_generation: record
                        .development
                        .terminal_generations
                        .last_key_value()
                        .map(|(generation, _)| *generation),
                    latest_terminal_kind: record
                        .development
                        .terminal_generations
                        .last_key_value()
                        .map(|(_, kind)| *kind),
                    tasks: ledger.tasks,
                    waiting_requests: ledger.requests,
                    host_resources: ledger.tokens.saturating_add(ledger.snapshots),
                    handler_calls_this_tick: record.handler_calls_this_tick,
                    handler_instructions_this_tick: record.handler_instructions_this_tick,
                    fuel_used_this_tick: record.fuel_used_this_tick,
                    last_compile_duration: record.development.last_compile_duration,
                    last_reload_duration: record.development.last_reload_duration,
                    recent_diagnostic: record.last_diagnostic.clone(),
                    recent_metrics: record.development.recent_metrics.iter().cloned().collect(),
                    implemented_entrypoints,
                    required_entrypoints,
                    missing_required_entrypoints,
                    optional_entrypoint_signatures,
                }
            })
            .collect();
        let mut recent_diagnostics = self
            .diagnostics
            .entries()
            .into_iter()
            .rev()
            .take(128)
            .map(|diagnostic| diagnostic.summary())
            .collect::<Vec<_>>();
        recent_diagnostics.reverse();
        if let Some(summary) = self.diagnostics.dropped_summary() {
            recent_diagnostics.push(summary);
        }
        EngineInspection {
            health: self.health(),
            packages,
            development: self.development_inspection(),
            recent_diagnostics,
            recent_reloads: self
                .reload_reports
                .iter()
                .map(ReloadReport::summary)
                .collect(),
            dropped_diagnostics: self.diagnostics.dropped(),
            dropped_events: self.dropped_events,
        }
    }

    fn development_inspection(&self) -> DevelopmentInspection {
        let coordinator = self.development_coordinator.inspection();
        DevelopmentInspection {
            enabled: self.development.enabled,
            worker_running: self.development_worker.is_some(),
            queued_candidates: coordinator.queued_candidates,
            retained_events: self.development_events.len(),
            created_generations: coordinator.created_generations,
            terminal_generations: coordinator.terminal_generations,
            duplicate_terminals: coordinator.duplicate_terminals,
            generations_without_terminal: coordinator.generations_without_terminal,
            desired_build_fingerprint_mismatches_rejected: self.packages.iter().fold(
                0_u64,
                |total, record| {
                    total.saturating_add(
                        record
                            .development
                            .desired_build_fingerprint_mismatch_rejection_count,
                    )
                },
            ),
            worker: self
                .development_worker
                .as_ref()
                .map_or_else(WorkerInspection::default, DevelopmentWorker::inspection),
        }
    }

    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        if self.shutdown {
            return Ok(());
        }
        let _ = self.development_coordinator.shutdown();
        for index in 0..self.packages.len() {
            self.cancel_development(index, CandidateCancellation::Shutdown);
        }
        if let Some(mut worker) = self.development_worker.take() {
            let drain = worker.shutdown();
            self.process_worker_drain(drain);
        }
        for index in 0..self.packages.len() {
            if self.packages[index].lifecycle.status() == PackageStatus::Enabled {
                self.disable_index(index, false)?;
            }
        }
        self.drain_releases();
        let _ = self.runtime_host.begin_close();
        if let Err(error) = self.runtime_host.try_finish_close() {
            let error = EngineError::Shutdown(error.to_string());
            self.diagnostics.push(EngineDiagnostic::without_source(
                None,
                None,
                EngineDiagnosticStage::Shutdown,
                nexa::ErrorCode::NX7303,
                error.to_string(),
            ));
            return Err(error);
        }
        self.shutdown = true;
        Ok(())
    }

    fn unique_index(&self, id: &PackageId) -> Result<usize, EngineError> {
        // Allocation-free uniqueness scan (WP92): `call` sits on the
        // steady-state path, so ambiguity is detected by the second match
        // instead of collecting every match first.
        let mut unique = None;
        for (index, record) in self.packages.iter().enumerate() {
            if record.candidate.manifest.id == *id {
                if unique.is_some() {
                    return Err(EngineError::Incompatible(id.clone()));
                }
                unique = Some(index);
            }
        }
        unique.ok_or_else(|| EngineError::UnknownPackage(id.clone()))
    }

    fn persist_selections(&self) -> Result<(), EngineError> {
        self.storage_dir.as_deref().map_or(Ok(()), |directory| {
            persistence::save(directory, &self.persisted)
                .map_err(|error| EngineError::Persistence(error.to_string()))
        })
    }

    fn drain_releases(&mut self) -> usize {
        let count = self.runtime_host.drain_releases().len();
        self.delivered_release_records = self
            .delivered_release_records
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        count
    }

    fn publish_reload(&mut self, report: ReloadReport) {
        while self.reload_reports.len() >= 64 {
            self.reload_reports.pop_front();
        }
        self.reload_reports.push_back(report.clone());
        while self.pending_reload_reports.len() >= 64 {
            self.pending_reload_reports.remove(0);
        }
        self.pending_reload_reports.push(report);
    }

    fn record_error(&mut self, index: usize, error: &EngineError) -> EngineDiagnosticSummary {
        let record = &self.packages[index];
        let package_id = Some(record.candidate.manifest.id.clone());
        let source_id = Some(record.source_id.clone());
        let diagnostic = if let EngineError::Diagnostic(diagnostic) = error {
            if diagnostic.sequence != 0 {
                return diagnostic.summary();
            }
            diagnostic.as_ref().clone()
        } else {
            let (stage, code) = match error {
                EngineError::Source { .. } => (
                    EngineDiagnosticStage::SourceDiscovery,
                    nexa::ErrorCode::NX7001,
                ),
                EngineError::Persistence(_) => {
                    (EngineDiagnosticStage::Persistence, nexa::ErrorCode::NX7302)
                }
                EngineError::Locked(_) => {
                    (EngineDiagnosticStage::Entitlement, nexa::ErrorCode::NX7004)
                }
                EngineError::Load(_, _) => (EngineDiagnosticStage::Load, nexa::ErrorCode::NX4001),
                EngineError::MissingExport(_, _) => {
                    (EngineDiagnosticStage::Export, nexa::ErrorCode::NX7010)
                }
                EngineError::ExportSignature(_, _) | EngineError::UndeclaredEntrypoint(_, _) => {
                    (EngineDiagnosticStage::Export, nexa::ErrorCode::NX7011)
                }
                EngineError::Handler(_, _) => {
                    (EngineDiagnosticStage::Handler, nexa::ErrorCode::NX7103)
                }
                EngineError::Reload(_, _) => {
                    (EngineDiagnosticStage::Reload, nexa::ErrorCode::NX7201)
                }
                EngineError::Activation(_, _) => {
                    (EngineDiagnosticStage::Activation, nexa::ErrorCode::NX7202)
                }
                EngineError::Shutdown(_) => {
                    (EngineDiagnosticStage::Shutdown, nexa::ErrorCode::NX7303)
                }
                _ => (EngineDiagnosticStage::Policy, nexa::ErrorCode::NX7003),
            };
            EngineDiagnostic::without_source(package_id, source_id, stage, code, error.to_string())
        };
        self.diagnostics.push(diagnostic).summary()
    }

    fn record_diagnostic(
        &mut self,
        index: usize,
        stage: EngineDiagnosticStage,
        code: nexa::ErrorCode,
        message: impl AsRef<str>,
    ) -> EngineDiagnosticSummary {
        let record = &self.packages[index];
        self.diagnostics
            .push(EngineDiagnostic::without_source(
                Some(record.candidate.manifest.id.clone()),
                Some(record.source_id.clone()),
                stage,
                code,
                message,
            ))
            .summary()
    }

    fn require_open(&self) -> Result<(), EngineError> {
        if self.shutdown {
            Err(EngineError::Shutdown("engine is already shut down".into()))
        } else {
            Ok(())
        }
    }
}

impl Drop for NexaEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug)]
pub enum EngineError {
    MissingHostFactory,
    DuplicateSourceId(SourceId),
    Contract(String),
    Source { source: SourceId, message: String },
    Persistence(String),
    Lifecycle(LifecycleError),
    UnknownPackage(PackageId),
    Incompatible(PackageId),
    Locked(PackageId),
    RequiredPackage(PackageId),
    InvalidState(PackageId, PackageStatus),
    Diagnostic(Box<EngineDiagnostic>),
    Load(PackageId, String),
    MissingExport(PackageId, String),
    ExportSignature(PackageId, String),
    UndeclaredEntrypoint(PackageId, nexa::StableId),
    Handler(PackageId, String),
    State(PackageId, String),
    Reload(PackageId, String),
    Activation(PackageId, String),
    Candidate(String),
    StaleCandidate(CandidateIdentity),
    RealmIdExhausted,
    DiscoveryRequired,
    DiscoveryAlreadyCompleted,
    Shutdown(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for EngineError {}

impl From<LifecycleError> for EngineError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

struct ReloadFailure {
    report: ReloadReport,
    error: EngineError,
}

impl ReloadFailure {
    const fn new(report: ReloadReport, error: EngineError) -> Self {
        Self { report, error }
    }
}

#[allow(clippy::too_many_arguments)]
fn reload_report(
    identity: CandidateIdentity,
    old_epoch: u64,
    new_epoch: Option<u64>,
    compile_duration: std::time::Duration,
    verify_duration: std::time::Duration,
    reload_metrics: nexa_runtime::RestartReloadMetrics,
    outcome: ReloadReportOutcome,
    cancelled_tasks: usize,
    detached_requests: usize,
) -> ReloadReport {
    let reload_duration = reload_metrics
        .quiesce_duration
        .saturating_add(reload_metrics.migration_duration)
        .saturating_add(reload_metrics.commit_duration)
        .saturating_add(reload_metrics.activation_duration);
    ReloadReport {
        identity,
        old_epoch,
        new_epoch,
        change_to_stable_duration: Duration::ZERO,
        queue_duration: Duration::ZERO,
        compile_duration,
        verify_duration,
        ready_to_commit_duration: Duration::ZERO,
        quiesce_duration: reload_metrics.quiesce_duration,
        commit_duration: reload_metrics.commit_duration,
        reload_duration,
        migration_duration: reload_metrics.migration_duration,
        activation_duration: reload_metrics.activation_duration,
        total_change_to_visible_duration: compile_duration
            .saturating_add(verify_duration)
            .saturating_add(reload_duration),
        cancelled_tasks,
        detached_requests,
        outcome,
    }
}

fn development_event_data(
    identity: CandidateIdentity,
    diagnostic: Option<EngineDiagnostic>,
) -> DevelopmentEventData {
    DevelopmentEventData {
        identity,
        diagnostic,
        reload: None,
        queue_duration: None,
    }
}

#[allow(clippy::too_many_lines)]
fn runtime_trap_diagnostic(
    record: &PackageRecord,
    contract: &HostContract,
    idl: &nexa_idl::ValidatedContract,
    trap: &nexa_runtime::Trap,
    export: &str,
) -> EngineDiagnostic {
    let Some(runtime) = &record.runtime else {
        return EngineDiagnostic::without_source(
            Some(record.candidate.manifest.id.clone()),
            Some(record.source_id.clone()),
            EngineDiagnosticStage::Runtime,
            nexa::ErrorCode::NX7103,
            trap.message.to_string(),
        );
    };
    let leaf = trap.source_span.map_or_else(
        || {
            nexa::Diagnostic::without_source(
                nexa::ErrorCode::NX7103,
                nexa::Severity::Error,
                nexa_runtime::RuntimeMessage::inline(&trap.message.to_string()),
            )
        },
        |span| {
            nexa::Diagnostic::from_parts(
                nexa::ErrorCode::NX7103,
                nexa::Severity::Error,
                nexa_runtime::RuntimeMessage::inline(&trap.message.to_string()),
                nexa::Label {
                    span,
                    message: nexa_runtime::RuntimeMessage::Static("runtime trap"),
                },
            )
        },
    );
    let mut diagnostic = EngineDiagnostic::from_package_snapshot(
        Some(record.candidate.manifest.id.clone()),
        Some(record.source_id.clone()),
        EngineDiagnosticStage::Runtime,
        leaf,
        &runtime.artifact.source_files,
    );
    diagnostic
        .diagnostic
        .notes
        .push(nexa_runtime::RuntimeMessage::inline(&format!(
            "underlying runtime diagnostic: {}",
            trap.diagnostic_code()
        )));
    diagnostic.context.export = Some(export.to_owned());
    diagnostic.context.module_epoch = trap.epoch;
    diagnostic.context.task = trap.task.map(|task| EngineTaskId(u64::from(task.index)));
    for frame in trap.script_call_stack.as_slice() {
        let name = runtime
            .artifact
            .function_for_script_frame(frame)
            .map_or_else(
                || "<unknown-function>".to_owned(),
                |function| function.name.clone(),
            );
        let source_identity = frame
            .source_span
            .and_then(|span| diagnostic.source_identity(span.file).cloned());
        diagnostic.related.push(RelatedDiagnostic {
            message: format!("at {name}"),
            file: source_identity,
            span: frame.source_span,
        });
    }
    if let Some(boundary) = trap.host_call_boundary {
        let function = idl.host_functions.get(boundary.import as usize);
        let function_name = function.map_or_else(
            || format!("<host import #{}>", boundary.import),
            |function| format!("{}::{}", contract.contract_name(), function.name),
        );
        let source_identity = boundary
            .source_span
            .and_then(|span| diagnostic.source_identity(span.file).cloned());
        diagnostic.related.push(RelatedDiagnostic {
            message: format!("while calling Host function {function_name}"),
            file: source_identity,
            span: boundary.source_span,
        });
        if let Some(function) = function
            && let Ok(registry) = SourceFileRegistry::from_files([(
                format!("{}.nidl", contract.contract_name().to_ascii_lowercase()),
                contract.source().to_owned(),
            )])
            && let Some(file) = registry.files().next().cloned()
        {
            let start = contract.source().find(&function.name).unwrap_or_default();
            let end = start.saturating_add(function.name.len());
            let identity = diagnostic.attach_related_source(
                nexa_diagnostics::SourceIdentity::standalone(std::sync::Arc::<str>::from(
                    file.path.as_str(),
                )),
                contract.source(),
            );
            diagnostic.related.push(RelatedDiagnostic {
                message: format!("Host function {function_name} is declared here"),
                span: Some(nexa_core::SourceSpan::new(
                    file.id,
                    u32::try_from(start).unwrap_or(u32::MAX),
                    u32::try_from(end).unwrap_or(u32::MAX),
                )),
                file: Some(identity),
            });
        }
    }
    diagnostic
}

pub mod prelude {
    pub use crate::{
        DirectorySource, EngineError, EngineHealth, MemorySource, NexaEngine, NexaEngineBuilder,
        PackageId, PackageOutput, PackagePolicy, PackageStatus, TrustLevel,
    };
}
