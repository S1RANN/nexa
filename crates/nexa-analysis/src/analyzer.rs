//! Package-scale name resolution, type/effect checking, and Typed IR construction.
//!
//! Analysis deliberately consumes an immutable [`ResolvedBuildInput`]. It never reads the
//! filesystem, reparses a manifest, resolves a path dependency, or invokes the bytecode compiler.
//! Every ordering decision uses canonical package/module/source identities so the same snapshot
//! produces byte-for-byte equivalent semantic records regardless of filesystem or worker order.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use nexa_core::{
    CanonicalSymbolIdentity, StableId, StableSymbolId, StableSymbolRegistry, SymbolKind,
};
use nexa_diagnostics::{
    ByteRange, Diagnostic, DiagnosticBatch, DiagnosticBatchLimits, ErrorCode, Label,
    RelatedLocation, Severity, SourceIdentity, SourceSnapshotRegistry,
};
use nexa_syntax::ast::{
    self, Attribute, AttributeArgumentKind, BinaryOperatorKind, DeclarationKind, ElseBranch,
    Expression, ExpressionKind, FunctionEffect, InterpolationPart, LiteralKind, Pattern,
    PatternKind, Statement, StatementKind, TypeDeclarationKind, TypeKind, TypeRef,
    UnaryOperatorKind, Visibility, parse_nexa_ast,
};
use nexa_syntax::{TextRange, TextSize};

use crate::ir::MigrationIntrinsicIr;
use crate::query::{
    linked_input_query_keys, module_header_query_keys, semantic_input_query_keys,
    typed_module_semantic_context,
};
use crate::{
    ArtifactFileId, ArtifactFileTable, BinaryOperator, BuiltinOperationIr, BuiltinVariantIr,
    CompilationLimits, DeclarationVisibility, Definition, DefinitionId, DefinitionKind,
    ExportBindingIr, ExternalSourceRangeIr, ExternalSourceSnapshotIr, FieldLayoutIr,
    HostAsyncResultIr, HostBindingIr, HostFieldBindingIr, HostFunctionBindingIr,
    HostNamespaceBindingIr, HostTypeBindingIr, HostTypeLayoutIr, HostVariantBindingIr,
    IrAbandonPolicy, IrCancelPolicy, IrEffect, IrHostFunctionMode, IrLiteral, IrType,
    LifecycleBindingsIr, ModuleGraph, ModuleGraphError, ModuleKey, ModulePath,
    NormalizedPackagePath, PackageId, PackageKind, PackageSemanticMetadata, PackageSourceSet,
    PublicApiFingerprint, QueryDatabase, QueryExecutionReport, QueryKey, ResolvedBuildInput,
    ResolvedReference, ResolvedTestInput, SemanticFingerprintRecord, SourceKey, SourceRange,
    SourceRole, SourceSetFingerprint, StableSymbolIdentity, StandardFunctionBindingIr,
    StateFieldIr, StateSchemaFingerprint, StateTypeIr, TestDefinitionIr, TypedBlockIr,
    TypedDeclarationBody, TypedDeclarationIr, TypedExpressionIr, TypedExpressionKind,
    TypedFunctionIr, TypedMatchArmIr, TypedModuleIr, TypedPackageIr, TypedPatternIr,
    TypedPatternKind, TypedPlaceIr, TypedStatementIr, TypedTypeLayoutIr, UnaryOperator,
    VariantLayoutIr, canonical_state_schema, external_source_key, public_api_fingerprint,
    source_set_fingerprint,
};

/// A type in a compiler-provided static module or Host contract.
///
/// External surfaces use names instead of analysis-local [`DefinitionId`] values. Analysis
/// resolves these names once while building the same dense definition table as source symbols.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SurfaceType {
    Unit,
    Bool,
    I32,
    I64,
    F32,
    F64,
    String,
    Rune,
    TypeParameter(String),
    Named { module: ModulePath, name: String },
    Option(Box<Self>),
    Result(Box<Self>, Box<Self>),
    Array(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Tuple(Vec<Self>),
    HostRequest(Option<Box<Self>>),
    ResourceToken(Option<Box<Self>>),
    Snapshot(Box<Self>),
    Buffer(Box<Self>),
    StateHandle(Box<Self>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostFunctionMode {
    Sync,
    Request,
}

/// Optional canonical source origin for an external declaration, normally a `.nidl` range.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalSourceOrigin {
    pub identity: SourceIdentity,
    pub text: Arc<str>,
    pub range: ByteRange,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExternalTypeKind {
    Opaque,
    Struct,
    Enum,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalTypeSurface {
    pub name: String,
    pub kind: ExternalTypeKind,
    pub stable_id: Option<nexa_core::StableId>,
    pub type_parameters: Vec<String>,
    pub fields: Vec<ExternalFieldSurface>,
    pub variants: Vec<ExternalVariantSurface>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalFieldSurface {
    pub name: String,
    pub stable_id: Option<nexa_core::StableId>,
    pub ty: SurfaceType,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalVariantSurface {
    pub name: String,
    pub stable_id: Option<nexa_core::StableId>,
    pub payload: Vec<SurfaceType>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExternalConstSurface {
    pub name: String,
    pub ty: SurfaceType,
    /// Canonical, already evaluated value bytes supplied by the standard-library provider.
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalFunctionSurface {
    pub name: String,
    pub type_parameters: Vec<String>,
    pub parameters: Vec<SurfaceType>,
    pub result: SurfaceType,
    pub effect: IrEffect,
    /// Stable lowering identity understood by code generation.
    pub intrinsic: nexa_stdlib::Intrinsic,
}

/// One compiler-provided static module. It has no Realm and receives no ambient capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticModuleSurface {
    pub module: ModulePath,
    pub types: Vec<ExternalTypeSurface>,
    pub constants: Vec<ExternalConstSurface>,
    pub functions: Vec<ExternalFunctionSurface>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFunctionSurface {
    pub name: String,
    pub parameters: Vec<SurfaceType>,
    pub result: SurfaceType,
    pub mode: HostFunctionMode,
    pub stable_id: nexa_core::StableId,
    pub import_index: u32,
    pub fuel_cost: u32,
    pub async_result: Option<HostAsyncResultSurface>,
    pub required_capability: Option<String>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAsyncResultSurface {
    pub result_type: nexa_core::StableId,
    pub success: SurfaceType,
    pub error: SurfaceType,
    pub cancel_policy: IrCancelPolicy,
    pub abandon_policy: IrAbandonPolicy,
    pub cancel_error: Option<u32>,
    pub abandon_error: Option<u32>,
}

/// The structured Host surface paired with `ResolvedBuildInput::canonical_host_contract`.
///
/// Canonical bytes participate in the build fingerprint; this structure is the sole semantic and
/// codegen truth. The analyzer never guesses a Host signature from source spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostContractSurface {
    pub interface_name: String,
    pub interface_stable_id: nexa_core::StableId,
    pub types: Vec<ExternalTypeSurface>,
    pub functions: Vec<HostFunctionSurface>,
    pub required_exports: Vec<RequiredExportSurface>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiredExportSurface {
    pub name: String,
    pub stable_id: nexa_core::StableId,
    pub parameters: Vec<SurfaceType>,
    pub result: SurfaceType,
    /// Optional effect constraint supplied by the Host contract adapter.
    ///
    /// NIDL exports do not currently declare an effect, so their adapter leaves this unset and
    /// analysis validates only the exact parameter/result types.
    pub effect: Option<IrEffect>,
    /// Exact Host declaration which imposed this export requirement.
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, Default)]
pub struct AnalysisEnvironment {
    pub host: Option<HostContractSurface>,
    pub static_modules: Vec<StaticModuleSurface>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzedStateField {
    pub definition: DefinitionId,
    pub ty: IrType,
    pub stable_id: StableSymbolId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzedStateType {
    pub definition: DefinitionId,
    pub version: u32,
    pub stable_id: StableSymbolId,
    pub fields: Vec<AnalyzedStateField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzedHostFunction {
    pub definition: DefinitionId,
    pub stable_id: nexa_core::StableId,
    pub import_index: u32,
    pub mode: HostFunctionMode,
    pub source: Option<SourceRange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzedHostBinding {
    pub interface: DefinitionId,
    pub interface_stable_id: nexa_core::StableId,
    pub namespaces: Vec<(PackageId, ModulePath, String)>,
    pub functions: Vec<AnalyzedHostFunction>,
}

#[derive(Clone, Debug)]
struct AnalyzedHostType {
    definition: DefinitionId,
    stable_id: nexa_core::StableId,
    kind: ExternalTypeKind,
    source: Option<ExternalSourceOrigin>,
    fields: Vec<(DefinitionId, ExternalFieldSurface)>,
    variants: Vec<(DefinitionId, ExternalVariantSurface)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzedExport {
    pub name: String,
    pub function: DefinitionId,
    pub stable_id: nexa_core::StableId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalyzedTest {
    pub package_id: PackageId,
    pub module: ModulePath,
    pub name: String,
    pub function: DefinitionId,
    pub span: SourceRange,
}

/// Complete result for one immutable analysis revision.
///
/// `ir` is present only if the revision has no error diagnostics. Fingerprints and semantic
/// records are still returned for valid portions so tooling can explain invalidation.
#[derive(Debug)]
pub struct AnalysisOutcome {
    pub ir: Option<TypedPackageIr>,
    pub diagnostics: DiagnosticBatch,
    pub source_set_fingerprint: SourceSetFingerprint,
    pub public_api_fingerprint: PublicApiFingerprint,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub public_api_records: Vec<SemanticFingerprintRecord>,
    pub state_schema_records: Vec<SemanticFingerprintRecord>,
    pub state_types: Vec<AnalyzedStateType>,
    pub host_binding: Option<AnalyzedHostBinding>,
    pub exports: Vec<AnalyzedExport>,
    pub tests: Vec<AnalyzedTest>,
    pub analyzed_revision: u64,
    pub query_report: QueryExecutionReport,
    pub resolved_import_edges: Vec<ResolvedImportEdge>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResolvedImportTarget {
    Module(ModuleKey),
    Static(ModulePath),
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedImportEdge {
    pub importer: ModuleKey,
    pub alias: String,
    pub target: ResolvedImportTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SourceModuleKey {
    package: PackageId,
    module: ModulePath,
}

#[derive(Clone)]
struct ParsedModule {
    key: SourceModuleKey,
    source: SourceKey,
    role: SourceRole,
    syntax: Arc<nexa_syntax::SyntaxTree>,
    ast: ast::NexaAst,
    compiler_provided: bool,
}

#[derive(Clone, Debug)]
enum ImportTarget {
    Source(SourceModuleKey),
    Static(ModulePath),
    Host,
}

#[derive(Clone, Debug, Default)]
struct ImportScope {
    aliases: BTreeMap<String, ImportTarget>,
}

#[derive(Clone)]
struct DeclRecord {
    module_index: usize,
    definition: DefinitionId,
    declaration: ast::Declaration,
}

#[derive(Clone)]
struct FunctionSignature {
    parameters: Vec<DefinitionId>,
    parameter_types: Vec<IrType>,
    result: IrType,
    effect: IrEffect,
}

#[derive(Clone)]
struct ExternalFunctionMetadata {
    parameters: Vec<IrType>,
    result: IrType,
    effect: IrEffect,
    host: Option<(DefinitionId, HostFunctionSurface)>,
    generic: Option<(Vec<SurfaceType>, SurfaceType)>,
    type_parameters: Vec<String>,
    intrinsic: Option<nexa_stdlib::Intrinsic>,
}

#[derive(Clone)]
struct TypeMetadata {
    fields: BTreeMap<String, DefinitionId>,
    field_order: Vec<DefinitionId>,
    variants: BTreeMap<String, DefinitionId>,
    variant_order: Vec<DefinitionId>,
    stateful: bool,
    version: u32,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
enum ConstValue {
    Unit,
    Bool(bool),
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    String(String),
    Rune(char),
    Tuple(Vec<Self>),
    Array(Vec<Self>),
    Construct {
        definition: DefinitionId,
        fields: Vec<(DefinitionId, Self)>,
    },
    Variant {
        definition: DefinitionId,
        values: Vec<Self>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RestrictedOperation {
    Host,
    Task,
    Await,
    Yield,
    Activation,
    Migration,
    PersistentState,
}

struct Analyzer<'a> {
    input: &'a ResolvedBuildInput,
    test_source_set: Option<&'a PackageSourceSet>,
    artifact_files: &'a ArtifactFileTable,
    compiler_file_ids: BTreeMap<SourceKey, ArtifactFileId>,
    environment: &'a AnalysisEnvironment,
    db: &'a mut QueryDatabase,
    typed_module_semantic_context: [u8; 32],
    mode: AnalysisMode,
    diagnostics: DiagnosticBatch,
    modules: Vec<ParsedModule>,
    module_indices: BTreeMap<SourceModuleKey, usize>,
    imports: BTreeMap<SourceModuleKey, ImportScope>,
    definitions: Vec<Definition>,
    symbols: BTreeMap<(PackageId, ModulePath, String), DefinitionId>,
    builtin_types: BTreeMap<String, DefinitionId>,
    members: BTreeMap<(DefinitionId, String), DefinitionId>,
    declaration_records: Vec<DeclRecord>,
    function_signatures: BTreeMap<DefinitionId, FunctionSignature>,
    external_functions: BTreeMap<DefinitionId, ExternalFunctionMetadata>,
    type_metadata: BTreeMap<DefinitionId, TypeMetadata>,
    variant_payloads: BTreeMap<DefinitionId, Vec<IrType>>,
    resolved_references: BTreeMap<usize, Vec<ResolvedReference>>,
    typed_declarations: BTreeMap<usize, Vec<TypedDeclarationIr>>,
    stable_registry: StableSymbolRegistry,
    stable_ids: BTreeMap<DefinitionId, StableSymbolId>,
    pending_stable_symbols: BTreeMap<DefinitionId, StableSymbolIdentity>,
    stable_names: BTreeMap<(PackageId, String), (DefinitionId, SourceRange)>,
    const_values: BTreeMap<DefinitionId, ConstValue>,
    call_edges: BTreeMap<DefinitionId, BTreeSet<DefinitionId>>,
    restricted: BTreeMap<DefinitionId, BTreeSet<RestrictedOperation>>,
    state_types: Vec<AnalyzedStateType>,
    host_binding: Option<AnalyzedHostBinding>,
    host_types: Vec<AnalyzedHostType>,
    host_namespaces: BTreeSet<(PackageId, ModulePath, String)>,
    lifecycle: LifecycleBindingsIr,
    exports: Vec<AnalyzedExport>,
    tests: Vec<AnalyzedTest>,
    public_records: Vec<SemanticFingerprintRecord>,
    state_records: Vec<SemanticFingerprintRecord>,
    resolved_import_edges: Vec<ResolvedImportEdge>,
    unresolved_surface_types: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnalysisMode {
    Product,
    Test,
}

/// Analyze one immutable root package and its complete static dependency closure.
#[must_use]
pub fn analyze_package(
    input: &ResolvedBuildInput,
    environment: &AnalysisEnvironment,
    db: &mut QueryDatabase,
) -> AnalysisOutcome {
    Analyzer::new(
        input,
        None,
        &input.artifact_files,
        environment,
        db,
        AnalysisMode::Product,
    )
    .run()
}

/// Analyze the root package's deterministic pure test target.
///
/// Production modules and dependency libraries participate in name resolution, while root
/// `tests/**/*.nexa` modules are added only to this separate test compilation. Test diagnostics
/// therefore never suppress or alter a valid product candidate.
#[must_use]
pub fn analyze_package_tests(
    input: &ResolvedTestInput,
    environment: &AnalysisEnvironment,
    db: &mut QueryDatabase,
) -> AnalysisOutcome {
    Analyzer::new(
        &input.product,
        Some(&input.test_source_set),
        &input.artifact_files,
        environment,
        db,
        AnalysisMode::Test,
    )
    .run()
}

impl<'a> Analyzer<'a> {
    fn new(
        input: &'a ResolvedBuildInput,
        test_source_set: Option<&'a PackageSourceSet>,
        artifact_files: &'a ArtifactFileTable,
        environment: &'a AnalysisEnvironment,
        db: &'a mut QueryDatabase,
        mode: AnalysisMode,
    ) -> Self {
        let (sources, source_conflicts) =
            combined_source_snapshots(input, test_source_set, environment);
        let compiler_file_ids = standard_library_source_keys()
            .into_iter()
            .enumerate()
            .map(|(offset, source)| {
                let raw = artifact_files
                    .files()
                    .len()
                    .checked_add(offset)
                    .and_then(|value| value.checked_add(1))
                    .and_then(|value| u32::try_from(value).ok())
                    .expect("compiler-provided source FileIds fit u32");
                (source, ArtifactFileId(raw))
            })
            .collect();
        let limits = DiagnosticBatchLimits {
            max_diagnostics: input.compilation_options.limits.diagnostics_per_revision,
            ..DiagnosticBatchLimits::default()
        };
        let mut diagnostics = DiagnosticBatch::new(sources, limits);
        for identity in source_conflicts {
            diagnostics.push(
                Diagnostic::new(
                    ErrorCode::NX2704,
                    Severity::Error,
                    format!(
                        "external source identity `{identity}` resolves to conflicting immutable \
                         snapshots"
                    ),
                )
                .with_note(
                    "analysis is blocked because labels cannot be mapped to one exact source",
                ),
            );
        }
        Self {
            input,
            test_source_set,
            artifact_files,
            compiler_file_ids,
            environment,
            db,
            typed_module_semantic_context: typed_module_semantic_context(input),
            mode,
            diagnostics,
            modules: Vec::new(),
            module_indices: BTreeMap::new(),
            imports: BTreeMap::new(),
            definitions: Vec::new(),
            symbols: BTreeMap::new(),
            builtin_types: BTreeMap::new(),
            members: BTreeMap::new(),
            declaration_records: Vec::new(),
            function_signatures: BTreeMap::new(),
            external_functions: BTreeMap::new(),
            type_metadata: BTreeMap::new(),
            variant_payloads: BTreeMap::new(),
            resolved_references: BTreeMap::new(),
            typed_declarations: BTreeMap::new(),
            stable_registry: StableSymbolRegistry::new(),
            stable_ids: BTreeMap::new(),
            pending_stable_symbols: BTreeMap::new(),
            stable_names: BTreeMap::new(),
            const_values: BTreeMap::new(),
            call_edges: BTreeMap::new(),
            restricted: BTreeMap::new(),
            state_types: Vec::new(),
            host_binding: None,
            host_types: Vec::new(),
            host_namespaces: BTreeSet::new(),
            lifecycle: LifecycleBindingsIr::default(),
            exports: Vec::new(),
            tests: Vec::new(),
            public_records: Vec::new(),
            state_records: Vec::new(),
            resolved_import_edges: Vec::new(),
            unresolved_surface_types: BTreeSet::new(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run(mut self) -> AnalysisOutcome {
        if self.mode == AnalysisMode::Product {
            // A persistent session may previously have compiled this root's explicit test target.
            // Clear its transient modules and import edges outside the product execution report.
            self.db
                .discard_test_module_sources(self.input.root_package());
        }
        self.db.begin_analysis();
        self.db.register_resolved_build_input(self.input);
        if let Some(test_sources) = self.test_source_set {
            self.db.register_test_module_sources(test_sources);
        }
        self.parse_sources();
        self.validate_dependency_graph();
        self.collect_source_declarations();
        self.collect_external_declarations();
        self.resolve_imports();
        self.resolve_declaration_signatures();
        self.validate_entry_and_exports();
        self.evaluate_constants();
        self.check_bodies();
        self.validate_tests();
        self.build_semantic_records();

        let public_api_fingerprint = public_api_fingerprint(self.public_records.clone());
        let production_state_types = self.production_state_types();
        let linked_state_types = production_state_types
            .iter()
            .map(|state| StateTypeIr {
                definition: state.definition,
                version: state.version,
                stable_id: state.stable_id,
                fields: state
                    .fields
                    .iter()
                    .map(|field| StateFieldIr {
                        definition: field.definition,
                        ty: field.ty.clone(),
                        stable_id: field.stable_id,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let state_schema_fingerprint =
            match canonical_state_schema(&linked_state_types, &self.definitions) {
                Ok(schema) => schema.fingerprint(),
                Err(error) => {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!("state schema cannot be lowered: {error}"),
                    ));
                    StateSchemaFingerprint::default()
                }
            };
        let source_set_fingerprint = source_set_fingerprint(&self.input.root_source_set);
        let has_errors = self
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error);
        let typed_modules = if has_errors {
            Vec::new()
        } else {
            self.typed_modules()
        };
        let root_package = self.input.root_package().clone();
        if has_errors && self.mode == AnalysisMode::Product {
            self.db
                .invalidate_keys([QueryKey::LinkedArtifact(root_package.clone())]);
        } else if !has_errors && self.mode == AnalysisMode::Product {
            let typed_dependencies = typed_modules
                .iter()
                .map(|module| {
                    QueryKey::TypedModule(ModuleKey::new(
                        module.package_id.clone(),
                        module.module.clone(),
                    ))
                })
                .collect::<BTreeSet<_>>();
            let mut public_dependencies = semantic_input_query_keys(self.input);
            public_dependencies.extend(module_header_query_keys(self.input));
            self.db.record_package_public_api(
                root_package.clone(),
                Arc::<[u8]>::from(public_api_fingerprint.as_bytes().as_slice()),
                public_dependencies,
            );
            let mut state_dependencies = semantic_input_query_keys(self.input);
            state_dependencies.extend(module_header_query_keys(self.input));
            self.db.record_package_state_schema(
                root_package.clone(),
                Arc::<[u8]>::from(state_schema_fingerprint.as_bytes().as_slice()),
                state_dependencies,
            );
            let mut linked_dependencies = linked_input_query_keys(self.input);
            linked_dependencies.extend(typed_dependencies);
            linked_dependencies.extend([
                QueryKey::PackagePublicApi(root_package.clone()),
                QueryKey::PackageStateSchema(root_package.clone()),
            ]);
            self.db.record_linked_artifact(
                root_package.clone(),
                Arc::<[u8]>::from(self.input.build_fingerprint.as_bytes().as_slice()),
                linked_dependencies,
            );
        }
        let ir = if has_errors {
            None
        } else {
            let metadata = self.package_metadata(
                public_api_fingerprint,
                state_schema_fingerprint,
                self.mode == AnalysisMode::Test,
            );
            let built = match self.mode {
                AnalysisMode::Product => TypedPackageIr::new_product(
                    self.input.root_manifest.id.clone(),
                    self.db.revision(),
                    self.definitions,
                    typed_modules,
                    metadata,
                ),
                AnalysisMode::Test => TypedPackageIr::new_test(
                    self.input.root_manifest.id.clone(),
                    self.db.revision(),
                    self.definitions,
                    typed_modules,
                    metadata,
                ),
            };
            match built {
                Ok(ir) => Some(ir),
                Err(error) => {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!("invalid typed package IR: {error}"),
                    ));
                    None
                }
            }
        };

        let query_report = self.db.finish_analysis();
        AnalysisOutcome {
            ir,
            diagnostics: self.diagnostics,
            source_set_fingerprint,
            public_api_fingerprint,
            state_schema_fingerprint,
            public_api_records: self.public_records,
            state_schema_records: self.state_records,
            state_types: production_state_types,
            host_binding: self.host_binding,
            exports: self.exports,
            tests: self.tests,
            analyzed_revision: self.db.revision(),
            query_report,
            resolved_import_edges: self.resolved_import_edges,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn package_metadata(
        &self,
        public_api_fingerprint: PublicApiFingerprint,
        state_schema_fingerprint: StateSchemaFingerprint,
        include_tests: bool,
    ) -> PackageSemanticMetadata {
        let (external_sources, external_file_ids) = self.external_source_catalog();
        let state_types = self
            .production_state_types()
            .iter()
            .map(|state| StateTypeIr {
                definition: state.definition,
                version: state.version,
                stable_id: state.stable_id,
                fields: state
                    .fields
                    .iter()
                    .map(|field| StateFieldIr {
                        definition: field.definition,
                        ty: field.ty.clone(),
                        stable_id: field.stable_id,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let host_bindings = self.host_binding.as_ref().map_or_else(Vec::new, |binding| {
            let host_types = self
                .host_types
                .iter()
                .map(|host_type| {
                    let source = host_type.source.as_ref().and_then(|origin| {
                        external_file_ids
                            .get(&origin.identity)
                            .copied()
                            .map(|file_id| ExternalSourceRangeIr {
                                file_id,
                                identity: origin.identity.clone(),
                                range: origin.range,
                            })
                    });
                    let layout = match host_type.kind {
                        ExternalTypeKind::Opaque => HostTypeLayoutIr::Opaque,
                        ExternalTypeKind::Struct => HostTypeLayoutIr::Struct {
                            fields: host_type
                                .fields
                                .iter()
                                .enumerate()
                                .map(|(order, (definition, field))| HostFieldBindingIr {
                                    definition: *definition,
                                    stable_id: field.stable_id.unwrap_or_default(),
                                    ty: self.definitions[definition.0 as usize].ty.clone(),
                                    order: u32::try_from(order).unwrap_or(u32::MAX),
                                    source: field.source.as_ref().and_then(|origin| {
                                        external_file_ids.get(&origin.identity).copied().map(
                                            |file_id| ExternalSourceRangeIr {
                                                file_id,
                                                identity: origin.identity.clone(),
                                                range: origin.range,
                                            },
                                        )
                                    }),
                                })
                                .collect(),
                        },
                        ExternalTypeKind::Enum => HostTypeLayoutIr::Enum {
                            variants: host_type
                                .variants
                                .iter()
                                .enumerate()
                                .map(|(tag, (definition, variant))| {
                                    let values = self
                                        .variant_payloads
                                        .get(definition)
                                        .cloned()
                                        .unwrap_or_default();
                                    let payload = match values.as_slice() {
                                        [] => None,
                                        [value] => Some(value.clone()),
                                        _ => Some(IrType::Tuple(values)),
                                    };
                                    HostVariantBindingIr {
                                        definition: *definition,
                                        stable_id: variant.stable_id.unwrap_or_default(),
                                        tag: u32::try_from(tag).unwrap_or(u32::MAX),
                                        payload,
                                        source: variant.source.as_ref().and_then(|origin| {
                                            external_file_ids.get(&origin.identity).copied().map(
                                                |file_id| ExternalSourceRangeIr {
                                                    file_id,
                                                    identity: origin.identity.clone(),
                                                    range: origin.range,
                                                },
                                            )
                                        }),
                                    }
                                })
                                .collect(),
                        },
                    };
                    HostTypeBindingIr {
                        definition: host_type.definition,
                        stable_id: host_type.stable_id,
                        layout,
                        source,
                    }
                })
                .collect();
            vec![HostBindingIr {
                interface: binding.interface,
                interface_stable_id: binding.interface_stable_id,
                namespaces: binding
                    .namespaces
                    .iter()
                    .map(|(package_id, module, namespace)| HostNamespaceBindingIr {
                        package_id: package_id.clone(),
                        module: module.clone(),
                        namespace: namespace.clone(),
                    })
                    .collect(),
                types: host_types,
                functions: binding
                    .functions
                    .iter()
                    .map(|function| {
                        let metadata = self
                            .external_functions
                            .get(&function.definition)
                            .expect("Host function metadata exists");
                        HostFunctionBindingIr {
                            definition: function.definition,
                            stable_id: function.stable_id,
                            import_index: function.import_index,
                            mode: match function.mode {
                                HostFunctionMode::Sync => IrHostFunctionMode::Sync,
                                HostFunctionMode::Request => IrHostFunctionMode::Request,
                            },
                            parameters: metadata.parameters.clone(),
                            result: metadata.result.clone(),
                            fuel_cost: metadata.host.as_ref().map_or(0, |(_, host)| host.fuel_cost),
                            async_result: metadata.host.as_ref().and_then(|(_, host)| {
                                host.async_result.as_ref().and_then(|result| {
                                    let IrType::Result(success, error) = &metadata.result else {
                                        return None;
                                    };
                                    Some(HostAsyncResultIr {
                                        result_type: result.result_type,
                                        success: (**success).clone(),
                                        error: (**error).clone(),
                                        cancel_policy: result.cancel_policy,
                                        abandon_policy: result.abandon_policy,
                                        cancel_error: result.cancel_error,
                                        abandon_error: result.abandon_error,
                                    })
                                })
                            }),
                            source: metadata.host.as_ref().and_then(|(_, host)| {
                                host.source.as_ref().and_then(|origin| {
                                    external_file_ids.get(&origin.identity).copied().map(
                                        |file_id| ExternalSourceRangeIr {
                                            file_id,
                                            identity: origin.identity.clone(),
                                            range: origin.range,
                                        },
                                    )
                                })
                            }),
                        }
                    })
                    .collect(),
            }]
        });
        let exports = self
            .exports
            .iter()
            .filter_map(|export| {
                let signature = self.function_signatures.get(&export.function)?;
                Some(ExportBindingIr {
                    name: export.name.clone(),
                    function: export.function,
                    stable_id: export.stable_id,
                    parameters: signature.parameter_types.clone(),
                    result: signature.result.clone(),
                    effect: signature.effect,
                })
            })
            .collect::<Vec<_>>();
        let tests = if include_tests {
            self.tests
                .iter()
                .map(|test| TestDefinitionIr {
                    name: test.name.clone(),
                    function: test.function,
                    module: test.module.clone(),
                    span: test.span.clone(),
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut standard_functions = self
            .external_functions
            .iter()
            .filter_map(|(definition, metadata)| {
                metadata
                    .intrinsic
                    .map(|intrinsic| StandardFunctionBindingIr {
                        definition: *definition,
                        intrinsic,
                        type_parameters: metadata.type_parameters.clone(),
                        parameters: metadata.parameters.clone(),
                        result: metadata.result.clone(),
                    })
            })
            .collect::<Vec<_>>();
        standard_functions.sort_by_key(|binding| binding.definition);
        PackageSemanticMetadata {
            entry_module: self.input.root_manifest.entry().cloned(),
            state_types: state_types.into(),
            host_bindings: host_bindings.into(),
            exports: exports.into(),
            tests: tests.into(),
            external_sources: external_sources.into(),
            lifecycle: self.lifecycle_bindings(),
            standard_functions: standard_functions.into(),
            public_api_fingerprint,
            state_schema_fingerprint,
        }
    }

    fn external_source_catalog(
        &self,
    ) -> (
        Vec<ExternalSourceSnapshotIr>,
        BTreeMap<SourceIdentity, ArtifactFileId>,
    ) {
        let mut sources = BTreeMap::<SourceIdentity, Arc<str>>::new();
        if let Some(host) = self.environment.host.clone() {
            for origin in host
                .source
                .iter()
                .chain(
                    host.functions
                        .iter()
                        .filter_map(|function| function.source.as_ref()),
                )
                .chain(host.types.iter().filter_map(|ty| ty.source.as_ref()))
                .chain(
                    host.types
                        .iter()
                        .flat_map(|ty| ty.fields.iter().filter_map(|field| field.source.as_ref())),
                )
                .chain(host.types.iter().flat_map(|ty| {
                    ty.variants
                        .iter()
                        .filter_map(|variant| variant.source.as_ref())
                }))
            {
                sources
                    .entry(origin.identity.clone())
                    .or_insert_with(|| Arc::clone(&origin.text));
            }
        }
        for module in &self.environment.static_modules {
            for origin in module
                .types
                .iter()
                .filter_map(|ty| ty.source.as_ref())
                .chain(
                    module
                        .types
                        .iter()
                        .flat_map(|ty| ty.fields.iter().filter_map(|field| field.source.as_ref())),
                )
                .chain(module.types.iter().flat_map(|ty| {
                    ty.variants
                        .iter()
                        .filter_map(|variant| variant.source.as_ref())
                }))
            {
                sources
                    .entry(origin.identity.clone())
                    .or_insert_with(|| Arc::clone(&origin.text));
            }
        }
        let base = self
            .artifact_files
            .files()
            .len()
            .checked_add(self.compiler_file_ids.len())
            .and_then(|value| value.checked_add(1))
            .and_then(|value| u32::try_from(value).ok())
            .expect("external source FileIds fit u32");
        let mut file_ids = BTreeMap::new();
        let mut snapshots = Vec::new();
        for (offset, (identity, text)) in sources.into_iter().enumerate() {
            let raw_offset = u32::try_from(offset).expect("external source count fits u32");
            let file_id = ArtifactFileId(
                base.checked_add(raw_offset)
                    .expect("external FileId fits u32"),
            );
            file_ids.insert(identity.clone(), file_id);
            snapshots.push(ExternalSourceSnapshotIr {
                file_id,
                identity,
                text,
            });
        }
        (snapshots, file_ids)
    }

    fn lifecycle_bindings(&self) -> LifecycleBindingsIr {
        self.lifecycle
    }

    #[allow(clippy::too_many_lines)]
    fn parse_sources(&mut self) {
        let mut units = self
            .input
            .all_source_sets()
            .chain(self.test_source_set)
            .flat_map(|set| set.units().values())
            .filter(|unit| match self.mode {
                AnalysisMode::Product => unit.role == SourceRole::Production,
                AnalysisMode::Test => {
                    unit.role == SourceRole::Production
                        || unit.key.package_id == self.input.root_manifest.id
                }
            })
            .collect::<Vec<_>>();
        units.sort_by(|left, right| left.key.cmp(&right.key));

        for unit in units {
            let expected = match unit.expected_module_path() {
                Ok(module) => module,
                Err(error) => {
                    self.push_source_error(
                        ErrorCode::NX2701,
                        &unit.key,
                        ByteRange::default(),
                        format!("invalid source/module mapping: {error}"),
                        "source path cannot name a Nexa module",
                    );
                    continue;
                }
            };
            let syntax = match self.db.parse(unit.key.clone(), &unit.text) {
                Ok(syntax) => syntax,
                Err(error) => {
                    self.push_source_error(
                        ErrorCode::NX1002,
                        &unit.key,
                        ByteRange::default(),
                        error.to_string(),
                        "source exceeds the parser limit",
                    );
                    continue;
                }
            };
            for error in &syntax.errors {
                self.push_source_error(
                    syntax_error_code(error.kind),
                    &unit.key,
                    byte_range(error.range),
                    error.message.clone(),
                    "syntax error",
                );
            }
            let ast = parse_nexa_ast(&syntax);
            if unit.virtual_module_path().is_some() {
                self.db.record_module_source(
                    ModuleKey::new(unit.key.package_id.clone(), expected.clone()),
                    unit.key.clone(),
                );
            } else {
                let _ = self.db.module_header(&unit.key);
            }
            for error in &ast.errors {
                self.push_source_error(
                    ErrorCode::NX1002,
                    &unit.key,
                    byte_range(error.range),
                    error.message.clone(),
                    "invalid Nexa syntax",
                );
            }
            let declared = ast.module.as_ref().map(|module| module.path.text());
            let declaration_matches = if unit.virtual_module_path().is_some() {
                declared
                    .as_deref()
                    .is_none_or(|declared| declared == expected.as_str())
            } else {
                declared.as_deref() == Some(expected.as_str())
            };
            if !declaration_matches {
                let range = ast
                    .module
                    .as_ref()
                    .map_or(ByteRange::default(), |module| byte_range(module.path.range));
                self.push_source_error(
                    ErrorCode::NX2701,
                    &unit.key,
                    range,
                    format!(
                        "module declaration must be `{expected}`, found {}",
                        declared.as_deref().unwrap_or("<missing>")
                    ),
                    "module declaration does not match the package-relative path",
                );
            }
            let module = SourceModuleKey {
                package: unit.key.package_id.clone(),
                module: expected,
            };
            if let Some(prior) = self.module_indices.get(&module).copied() {
                let prior_source = self.modules[prior].source.clone();
                let diagnostic = Diagnostic::new(
                    ErrorCode::NX2701,
                    Severity::Error,
                    format!("duplicate module {}", module.module),
                )
                .with_label(Label::primary(
                    source_identity(&unit.key),
                    ast.module
                        .as_ref()
                        .map_or(ByteRange::default(), |value| byte_range(value.path.range)),
                    "module is declared more than once",
                ))
                .with_related(RelatedLocation::new(
                    source_identity(&prior_source),
                    ByteRange::default(),
                    "first module source",
                ));
                self.diagnostics.push(diagnostic);
                continue;
            }
            let index = self.modules.len();
            self.module_indices.insert(module.clone(), index);
            self.modules.push(ParsedModule {
                key: module,
                source: unit.key.clone(),
                role: unit.role,
                syntax,
                ast,
                compiler_provided: false,
            });
        }
        self.parse_standard_library_sources();
    }

    fn parse_standard_library_sources(&mut self) {
        let package =
            PackageId::new(nexa_stdlib::PACKAGE_ID).expect("standard-library package ID is valid");
        for descriptor in nexa_stdlib::standard_library().modules() {
            let module =
                ModulePath::new(descriptor.path).expect("standard-library module path is valid");
            let source = standard_library_source_key(descriptor);
            let key = SourceModuleKey {
                package: package.clone(),
                module: module.clone(),
            };
            if let Some(prior) = self.module_indices.get(&key).copied() {
                let prior_source = self.modules[prior].source.clone();
                self.diagnostics.push(
                    Diagnostic::new(
                        ErrorCode::NX2704,
                        Severity::Error,
                        format!(
                            "compiler-provided module `{module}` collides with {}",
                            prior_source.package_id
                        ),
                    )
                    .with_label(Label::primary(
                        source_identity(&prior_source),
                        ByteRange::default(),
                        "`nexa.stdlib` and `std.*` are reserved",
                    )),
                );
                continue;
            }
            let syntax = match self.db.parse(source.clone(), descriptor.source) {
                Ok(syntax) => syntax,
                Err(error) => {
                    self.push_source_error(
                        ErrorCode::NX1002,
                        &source,
                        ByteRange::default(),
                        error.to_string(),
                        "embedded standard-library source exceeds parser limits",
                    );
                    continue;
                }
            };
            let _ = self.db.module_header(&source);
            for error in &syntax.errors {
                self.push_source_error(
                    syntax_error_code(error.kind),
                    &source,
                    byte_range(error.range),
                    error.message.clone(),
                    "invalid embedded standard-library syntax",
                );
            }
            let ast = parse_nexa_ast(&syntax);
            for error in &ast.errors {
                self.push_source_error(
                    ErrorCode::NX1002,
                    &source,
                    byte_range(error.range),
                    error.message.clone(),
                    "invalid embedded standard-library AST",
                );
            }
            if ast.module.as_ref().map(|declared| declared.path.text()) != Some(module.to_string())
            {
                self.push_source_error(
                    ErrorCode::NX2701,
                    &source,
                    ast.module
                        .as_ref()
                        .map_or(ByteRange::default(), |declared| {
                            byte_range(declared.path.range)
                        }),
                    format!("embedded standard-library source must declare `module {module};`"),
                    "descriptor module path and source declaration differ",
                );
            }
            let index = self.modules.len();
            self.module_indices.insert(key.clone(), index);
            self.modules.push(ParsedModule {
                key,
                source,
                role: SourceRole::Production,
                syntax,
                ast,
                compiler_provided: true,
            });
        }
    }

    fn validate_dependency_graph(&mut self) {
        if let Err(error) = self.input.dependency_graph.validate_acyclic() {
            let message = error.to_string();
            self.diagnostics
                .push(Diagnostic::new(ErrorCode::NX2702, Severity::Error, message));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn collect_source_declarations(&mut self) {
        for module_index in 0..self.modules.len() {
            let module = self.modules[module_index].clone();
            for declaration in module.ast.declarations.clone() {
                let Some((name, kind, mut symbol_kind, mut effect)) =
                    declaration_surface(&declaration.kind)
                else {
                    continue;
                };
                if has_attribute(&declaration.attributes, "test")
                    && matches!(declaration.kind, DeclarationKind::Function(_))
                {
                    symbol_kind = SymbolKind::Test;
                    // `@test fn` is the deliberately ergonomic spelling from the language
                    // contract. It is analyzed as Immediate even though an unannotated
                    // production `fn` remains Ordinary.
                    effect = IrEffect::Immediate;
                }
                let visibility = declaration_visibility(declaration.visibility);
                let canonical_identity = self.canonical_identity(
                    &module,
                    &declaration,
                    symbol_kind,
                    &name,
                    declaration.range,
                    true,
                );
                let definition = self.allocate_definition(
                    module.key.package.clone(),
                    module.key.module.clone(),
                    name.clone(),
                    kind,
                    visibility,
                    IrType::Unit,
                    effect,
                    source_range(&module.source, declaration.range),
                    canonical_identity,
                );
                let key = (
                    module.key.package.clone(),
                    module.key.module.clone(),
                    name.clone(),
                );
                if let Some(prior) = self.symbols.insert(key, definition) {
                    let prior_definition = self.definitions[prior.0 as usize].clone();
                    self.diagnostics.push(
                        Diagnostic::new(
                            ErrorCode::NX2704,
                            Severity::Error,
                            format!("duplicate declaration `{name}`"),
                        )
                        .with_label(Label::primary(
                            source_identity(&module.source),
                            byte_range(declaration.range),
                            "duplicate declaration",
                        ))
                        .with_related(RelatedLocation::new(
                            source_identity(&prior_definition.span.source),
                            range_from_source(&prior_definition.span),
                            "first declaration",
                        )),
                    );
                }

                match &declaration.kind {
                    DeclarationKind::Function(function) => {
                        let mut parameters = Vec::new();
                        for parameter in &function.parameters {
                            let id = self.allocate_definition(
                                module.key.package.clone(),
                                module.key.module.clone(),
                                parameter.name.text.clone(),
                                DefinitionKind::Parameter,
                                DeclarationVisibility::Private,
                                IrType::Unit,
                                IrEffect::Immediate,
                                source_range(&module.source, parameter.range),
                                format!(
                                    "{}::{}::parameter::{}::{}",
                                    module.key.package,
                                    module.key.module,
                                    name,
                                    parameter.name.text
                                ),
                            );
                            parameters.push(id);
                        }
                        self.function_signatures.insert(
                            definition,
                            FunctionSignature {
                                parameters,
                                parameter_types: Vec::new(),
                                result: IrType::Unit,
                                effect,
                            },
                        );
                    }
                    DeclarationKind::Type(ty) => {
                        let stateful = ty.kind == TypeDeclarationKind::Stateful;
                        let version = if stateful {
                            stateful_version(&declaration.attributes).unwrap_or(1)
                        } else {
                            0
                        };
                        let mut metadata = TypeMetadata {
                            fields: BTreeMap::new(),
                            field_order: Vec::new(),
                            variants: BTreeMap::new(),
                            variant_order: Vec::new(),
                            stateful,
                            version,
                        };
                        for field in &ty.fields {
                            let identity = self.member_canonical_identity(
                                &module,
                                definition,
                                field.name.text.as_str(),
                                &field.attributes,
                                field.range,
                                SymbolKind::Field,
                                stateful,
                            );
                            let id = self.allocate_definition(
                                module.key.package.clone(),
                                module.key.module.clone(),
                                field.name.text.clone(),
                                DefinitionKind::Field,
                                visibility,
                                IrType::Unit,
                                IrEffect::Immediate,
                                source_range(&module.source, field.range),
                                identity,
                            );
                            if metadata
                                .fields
                                .insert(field.name.text.clone(), id)
                                .is_some()
                            {
                                self.push_source_error(
                                    ErrorCode::NX2704,
                                    &module.source,
                                    byte_range(field.range),
                                    format!("duplicate field `{}`", field.name.text),
                                    "field is declared more than once",
                                );
                            }
                            metadata.field_order.push(id);
                            self.members
                                .insert((definition, field.name.text.clone()), id);
                        }
                        for variant in &ty.variants {
                            let identity = self.member_canonical_identity(
                                &module,
                                definition,
                                variant.name.text.as_str(),
                                &[],
                                variant.range,
                                SymbolKind::Variant,
                                false,
                            );
                            let id = self.allocate_definition(
                                module.key.package.clone(),
                                module.key.module.clone(),
                                variant.name.text.clone(),
                                DefinitionKind::Variant,
                                visibility,
                                IrType::Named(definition),
                                IrEffect::Immediate,
                                source_range(&module.source, variant.range),
                                identity,
                            );
                            if metadata
                                .variants
                                .insert(variant.name.text.clone(), id)
                                .is_some()
                            {
                                self.push_source_error(
                                    ErrorCode::NX2704,
                                    &module.source,
                                    byte_range(variant.range),
                                    format!("duplicate variant `{}`", variant.name.text),
                                    "variant is declared more than once",
                                );
                            }
                            metadata.variant_order.push(id);
                            self.members
                                .insert((definition, variant.name.text.clone()), id);
                            let variant_key = (
                                module.key.package.clone(),
                                module.key.module.clone(),
                                variant.name.text.clone(),
                            );
                            if let Some(prior) = self.symbols.get(&variant_key).copied() {
                                let prior = self.definitions[prior.0 as usize].clone();
                                self.diagnostics.push(
                                    Diagnostic::new(
                                        ErrorCode::NX2704,
                                        Severity::Error,
                                        format!("ambiguous module variant `{}`", variant.name.text),
                                    )
                                    .with_label(Label::primary(
                                        source_identity(&module.source),
                                        byte_range(variant.name.range),
                                        "variant conflicts in the module value namespace",
                                    ))
                                    .with_related(
                                        RelatedLocation::new(
                                            source_identity(&prior.span.source),
                                            range_from_source(&prior.span),
                                            "first declaration with this name",
                                        ),
                                    ),
                                );
                            } else {
                                self.symbols.insert(variant_key, id);
                            }
                        }
                        self.type_metadata.insert(definition, metadata);
                        self.definitions[definition.0 as usize].ty = IrType::Named(definition);
                    }
                    DeclarationKind::Const(_) | DeclarationKind::Error => {}
                }
                self.declaration_records.push(DeclRecord {
                    module_index,
                    definition,
                    declaration,
                });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn allocate_definition(
        &mut self,
        package_id: PackageId,
        module: ModulePath,
        name: String,
        kind: DefinitionKind,
        visibility: DeclarationVisibility,
        ty: IrType,
        effect: IrEffect,
        span: SourceRange,
        canonical_identity: String,
    ) -> DefinitionId {
        let id = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
        let stable_symbol = self.pending_stable_symbols.remove(&id);
        self.definitions.push(Definition {
            id,
            package_id,
            module,
            name,
            kind,
            visibility,
            ty,
            effect,
            span,
            canonical_identity,
            stable_symbol,
        });
        id
    }

    #[allow(clippy::too_many_lines)]
    fn canonical_identity(
        &mut self,
        module: &ParsedModule,
        declaration: &ast::Declaration,
        kind: SymbolKind,
        name: &str,
        range: TextRange,
        allow_stable: bool,
    ) -> String {
        let canonical_package = if module.compiler_provided {
            nexa_stdlib::CANONICAL_PACKAGE_ID
        } else {
            module.key.package.as_str()
        };
        let stable_range = stable_diagnostic_range(&declaration.attributes, range);
        let stable = stable_attribute(&declaration.attributes);
        let identity = match stable {
            Ok(Some(stable)) if allow_stable => {
                if declaration.visibility == Visibility::Private {
                    self.push_source_error(
                        ErrorCode::NX2710,
                        &module.source,
                        byte_range(stable_range),
                        "@stable requires `pub` or `pub(package)`",
                        "private declarations cannot carry a stable identity",
                    );
                }
                if !valid_stable_name(&stable) {
                    self.push_source_error(
                        ErrorCode::NX2710,
                        &module.source,
                        byte_range(stable_range),
                        format!("invalid @stable name `{stable}`"),
                        "expected [A-Za-z][A-Za-z0-9._-]{0,127}",
                    );
                }
                self.record_explicit_stable(
                    &module.key.package,
                    &stable,
                    module,
                    stable_range,
                    kind,
                );
                CanonicalSymbolIdentity::explicit(canonical_package, kind, stable)
            }
            Ok(Some(_)) => {
                self.push_source_error(
                    ErrorCode::NX2710,
                    &module.source,
                    byte_range(stable_range),
                    "@stable is not allowed on this declaration",
                    "remove the attribute",
                );
                CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    module.key.module.as_str(),
                    kind,
                    name,
                )
            }
            Ok(None) => CanonicalSymbolIdentity::automatic(
                canonical_package,
                module.key.module.as_str(),
                kind,
                name,
            ),
            Err(message) => {
                self.push_source_error(
                    ErrorCode::NX2710,
                    &module.source,
                    byte_range(stable_range),
                    message,
                    "use exactly one string argument",
                );
                CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    module.key.module.as_str(),
                    kind,
                    name,
                )
            }
        };
        let canonical = canonical_identity_text(&identity);
        match self.stable_registry.insert(identity) {
            Ok(stable_id) => {
                let next = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
                self.stable_ids.insert(next, stable_id);
                self.pending_stable_symbols.insert(
                    next,
                    StableSymbolIdentity {
                        canonical: self
                            .stable_registry
                            .identity(stable_id)
                            .expect("inserted identity exists")
                            .clone(),
                        runtime_id: stable_id,
                    },
                );
            }
            Err(collision) => {
                self.push_source_error(
                    ErrorCode::NX2711,
                    &module.source,
                    byte_range(stable_range),
                    collision.to_string(),
                    "runtime StableId collision",
                );
            }
        }
        canonical
    }

    #[allow(clippy::too_many_arguments)]
    fn member_canonical_identity(
        &mut self,
        module: &ParsedModule,
        owner: DefinitionId,
        name: &str,
        attributes: &[Attribute],
        range: TextRange,
        kind: SymbolKind,
        allow_stable: bool,
    ) -> String {
        let canonical_package = if module.compiler_provided {
            nexa_stdlib::CANONICAL_PACKAGE_ID
        } else {
            module.key.package.as_str()
        };
        let stable_range = stable_diagnostic_range(attributes, range);
        let stable = stable_attribute(attributes);
        let identity = match stable {
            Ok(Some(stable)) if allow_stable => {
                if !valid_stable_name(&stable) {
                    self.push_source_error(
                        ErrorCode::NX2710,
                        &module.source,
                        byte_range(stable_range),
                        format!("invalid @stable name `{stable}`"),
                        "expected [A-Za-z][A-Za-z0-9._-]{0,127}",
                    );
                }
                self.record_explicit_stable(
                    &module.key.package,
                    &stable,
                    module,
                    stable_range,
                    kind,
                );
                CanonicalSymbolIdentity::explicit(canonical_package, kind, stable)
            }
            Ok(Some(_)) => {
                self.push_source_error(
                    ErrorCode::NX2710,
                    &module.source,
                    byte_range(stable_range),
                    "@stable is only valid on Stateful fields",
                    "remove the attribute",
                );
                let owner_name = &self.definitions[owner.0 as usize].name;
                CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    module.key.module.as_str(),
                    kind,
                    format!("{owner_name}.{name}"),
                )
            }
            Ok(None) => {
                let owner_name = &self.definitions[owner.0 as usize].name;
                CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    module.key.module.as_str(),
                    kind,
                    format!("{owner_name}.{name}"),
                )
            }
            Err(message) => {
                self.push_source_error(
                    ErrorCode::NX2710,
                    &module.source,
                    byte_range(stable_range),
                    message,
                    "use exactly one string argument",
                );
                let owner_name = &self.definitions[owner.0 as usize].name;
                CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    module.key.module.as_str(),
                    kind,
                    format!("{owner_name}.{name}"),
                )
            }
        };
        let canonical = canonical_identity_text(&identity);
        let next = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
        match self.stable_registry.insert(identity) {
            Ok(stable_id) => {
                self.stable_ids.insert(next, stable_id);
                self.pending_stable_symbols.insert(
                    next,
                    StableSymbolIdentity {
                        canonical: self
                            .stable_registry
                            .identity(stable_id)
                            .expect("inserted identity exists")
                            .clone(),
                        runtime_id: stable_id,
                    },
                );
            }
            Err(collision) => self.push_source_error(
                ErrorCode::NX2711,
                &module.source,
                byte_range(stable_range),
                collision.to_string(),
                "runtime StableId collision",
            ),
        }
        canonical
    }

    fn record_explicit_stable(
        &mut self,
        package: &PackageId,
        stable: &str,
        module: &ParsedModule,
        range: TextRange,
        _kind: SymbolKind,
    ) {
        let next = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
        let location = source_range(&module.source, range);
        if let Some((_, prior)) = self.stable_names.insert(
            (package.clone(), stable.to_owned()),
            (next, location.clone()),
        ) {
            self.diagnostics.push(
                Diagnostic::new(
                    ErrorCode::NX2711,
                    Severity::Error,
                    format!("duplicate @stable name `{stable}` in package {package}"),
                )
                .with_label(Label::primary(
                    source_identity(&module.source),
                    byte_range(range),
                    "duplicate stable identity",
                ))
                .with_related(RelatedLocation::new(
                    source_identity(&prior.source),
                    range_from_source(&prior),
                    "first use of this stable identity",
                )),
            );
        }
    }

    fn compiler_canonical_identity(
        &mut self,
        module: &ModulePath,
        kind: SymbolKind,
        name: &str,
        source: &SourceKey,
    ) -> String {
        let identity = CanonicalSymbolIdentity::automatic(
            nexa_stdlib::CANONICAL_PACKAGE_ID,
            module.as_str(),
            kind,
            name,
        );
        let canonical = canonical_identity_text(&identity);
        let next = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
        match self.stable_registry.insert(identity) {
            Ok(stable_id) => {
                self.stable_ids.insert(next, stable_id);
                self.pending_stable_symbols.insert(
                    next,
                    StableSymbolIdentity {
                        canonical: self
                            .stable_registry
                            .identity(stable_id)
                            .expect("inserted compiler identity exists")
                            .clone(),
                        runtime_id: stable_id,
                    },
                );
            }
            Err(collision) => self.push_source_error(
                ErrorCode::NX2711,
                source,
                ByteRange::default(),
                collision.to_string(),
                "compiler-provided runtime StableId collision",
            ),
        }
        canonical
    }

    fn generated_canonical_identity(
        &mut self,
        module: &ParsedModule,
        kind: SymbolKind,
        name: &str,
        range: TextRange,
    ) -> String {
        let package = if module.compiler_provided {
            nexa_stdlib::CANONICAL_PACKAGE_ID
        } else {
            module.key.package.as_str()
        };
        let identity =
            CanonicalSymbolIdentity::automatic(package, module.key.module.as_str(), kind, name);
        let canonical = canonical_identity_text(&identity);
        let next = DefinitionId(u32::try_from(self.definitions.len()).unwrap_or(u32::MAX));
        match self.stable_registry.insert(identity) {
            Ok(stable_id) => {
                self.stable_ids.insert(next, stable_id);
                self.pending_stable_symbols.insert(
                    next,
                    StableSymbolIdentity {
                        canonical: self
                            .stable_registry
                            .identity(stable_id)
                            .expect("inserted generated identity exists")
                            .clone(),
                        runtime_id: stable_id,
                    },
                );
            }
            Err(collision) => self.push_source_error(
                ErrorCode::NX2711,
                &module.source,
                byte_range(range),
                collision.to_string(),
                "generated runtime StableId collision",
            ),
        }
        canonical
    }

    #[allow(clippy::too_many_lines)]
    fn collect_external_declarations(&mut self) {
        #[derive(Clone)]
        enum Pending {
            StaticFunction(DefinitionId, ExternalFunctionSurface),
            StaticConst(DefinitionId, ExternalConstSurface),
            HostFunction(DefinitionId, DefinitionId, Box<HostFunctionSurface>),
        }

        let root = self.input.root_manifest.id.clone();
        let fallback = self.fallback_source_range();
        let mut pending = Vec::new();
        let builtin_module =
            ModulePath::new("nexa.builtin").expect("compiler builtin module is valid");
        for name in ["StableId", "StateHandleError"] {
            let definition = self.allocate_definition(
                root.clone(),
                builtin_module.clone(),
                name.to_owned(),
                DefinitionKind::StandardLibrary,
                DeclarationVisibility::Public,
                IrType::Unit,
                IrEffect::Immediate,
                fallback.clone(),
                format!("nexa.builtin::type::{name}"),
            );
            self.definitions[definition.0 as usize].ty = IrType::Named(definition);
            self.builtin_types.insert(name.to_owned(), definition);
        }
        let std_package =
            PackageId::new(nexa_stdlib::PACKAGE_ID).expect("standard-library package ID is valid");
        for descriptor in nexa_stdlib::standard_library().modules() {
            let module =
                ModulePath::new(descriptor.path).expect("standard-library module path is valid");
            let source = standard_library_source_key(descriptor);
            let span = SourceRange {
                source: source.clone(),
                start: 0,
                end: u32::try_from(descriptor.source.len()).unwrap_or(u32::MAX),
            };
            for function in descriptor.functions {
                let nexa_stdlib::Lowering::CompilerIntrinsic(intrinsic) = function.lowering else {
                    continue;
                };
                let type_parameters = function
                    .type_parameters
                    .iter()
                    .map(|parameter| (*parameter).to_owned())
                    .collect::<Vec<_>>();
                let parameter_names = type_parameters.iter().cloned().collect::<BTreeSet<_>>();
                let parameters = function
                    .parameters
                    .iter()
                    .map(|parameter| {
                        descriptor_surface_type(parameter.ty, &module, &parameter_names)
                    })
                    .collect::<Result<Vec<_>, _>>();
                let result = descriptor_surface_type(function.result, &module, &parameter_names);
                let (parameters, result) = match (parameters, result) {
                    (Ok(parameters), Ok(result)) => (parameters, result),
                    (Err(error), _) | (_, Err(error)) => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &source,
                            ByteRange::default(),
                            format!(
                                "invalid standard-library descriptor for {}.{}: {error}",
                                descriptor.path, function.name
                            ),
                            "compiler descriptor type is invalid",
                        );
                        continue;
                    }
                };
                let identity = self.compiler_canonical_identity(
                    &module,
                    SymbolKind::Function,
                    function.name,
                    &source,
                );
                let definition = self.allocate_definition(
                    std_package.clone(),
                    module.clone(),
                    function.name.to_owned(),
                    DefinitionKind::StandardLibrary,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    IrEffect::Immediate,
                    span.clone(),
                    identity,
                );
                if self
                    .symbols
                    .insert(
                        (
                            std_package.clone(),
                            module.clone(),
                            function.name.to_owned(),
                        ),
                        definition,
                    )
                    .is_some()
                {
                    self.push_source_error(
                        ErrorCode::NX2704,
                        &source,
                        ByteRange::default(),
                        format!(
                            "standard-library intrinsic `{}` is also declared in embedded source",
                            function.name
                        ),
                        "intrinsic declarations must not have source bodies",
                    );
                }
                pending.push(Pending::StaticFunction(
                    definition,
                    ExternalFunctionSurface {
                        name: function.name.to_owned(),
                        type_parameters,
                        parameters,
                        result,
                        effect: IrEffect::Immediate,
                        intrinsic,
                    },
                ));
            }
        }
        let mut static_modules = self.environment.static_modules.clone();
        static_modules.sort_by(|left, right| left.module.cmp(&right.module));
        for module in static_modules {
            let mut types = module.types;
            types.sort();
            for ty in types {
                let id = self.allocate_definition(
                    root.clone(),
                    module.module.clone(),
                    ty.name.clone(),
                    DefinitionKind::StandardLibrary,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    IrEffect::Immediate,
                    fallback.clone(),
                    format!("nexa.std::{}::type::{}", module.module, ty.name),
                );
                self.definitions[id.0 as usize].ty = IrType::Named(id);
                self.symbols
                    .insert((root.clone(), module.module.clone(), ty.name), id);
                self.type_metadata.insert(
                    id,
                    TypeMetadata {
                        fields: BTreeMap::new(),
                        field_order: Vec::new(),
                        variants: BTreeMap::new(),
                        variant_order: Vec::new(),
                        stateful: false,
                        version: 0,
                    },
                );
            }
            let mut constants = module.constants;
            constants.sort();
            for constant in constants {
                let id = self.allocate_definition(
                    root.clone(),
                    module.module.clone(),
                    constant.name.clone(),
                    DefinitionKind::StandardLibrary,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    IrEffect::Immediate,
                    fallback.clone(),
                    format!("nexa.std::{}::constant::{}", module.module, constant.name),
                );
                self.symbols.insert(
                    (root.clone(), module.module.clone(), constant.name.clone()),
                    id,
                );
                pending.push(Pending::StaticConst(id, constant));
            }
            let mut functions = module.functions;
            functions.sort_by(|left, right| left.name.cmp(&right.name));
            for function in functions {
                let id = self.allocate_definition(
                    root.clone(),
                    module.module.clone(),
                    function.name.clone(),
                    DefinitionKind::StandardLibrary,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    function.effect,
                    fallback.clone(),
                    format!("nexa.std::{}::function::{}", module.module, function.name),
                );
                self.symbols.insert(
                    (root.clone(), module.module.clone(), function.name.clone()),
                    id,
                );
                pending.push(Pending::StaticFunction(id, function));
            }
        }

        if let Some(host) = self.environment.host.clone() {
            let host_module = ModulePath::new("host").expect("host is a valid reserved module");
            let source = host
                .source
                .as_ref()
                .map_or_else(|| fallback.clone(), external_source_range);
            let interface = self.allocate_definition(
                root.clone(),
                host_module.clone(),
                host.interface_name.clone(),
                DefinitionKind::HostInterface,
                DeclarationVisibility::Public,
                IrType::Unit,
                IrEffect::Immediate,
                source,
                format!("host::interface::{}", host.interface_name),
            );
            self.definitions[interface.0 as usize].ty = IrType::Named(interface);
            let mut host_types = host.types;
            host_types.sort();
            for host_type in host_types {
                if !host_type.type_parameters.is_empty() {
                    self.diagnostics.push(
                        Diagnostic::new(
                            ErrorCode::NX2101,
                            Severity::Error,
                            format!(
                                "Host type `{}` cannot declare Nexa type parameters",
                                host_type.name
                            ),
                        )
                        .with_label(Label::primary(
                            host_type.source.as_ref().map_or_else(
                                || source_identity(&fallback.source),
                                |origin| origin.identity.clone(),
                            ),
                            host_type
                                .source
                                .as_ref()
                                .map_or(ByteRange::default(), |origin| origin.range),
                            "NIDL nominal types must have a closed ABI",
                        )),
                    );
                }
                let layout_matches_kind = match host_type.kind {
                    ExternalTypeKind::Opaque => {
                        host_type.fields.is_empty() && host_type.variants.is_empty()
                    }
                    ExternalTypeKind::Struct => host_type.variants.is_empty(),
                    ExternalTypeKind::Enum => host_type.fields.is_empty(),
                };
                if !layout_matches_kind {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host type `{}` layout does not match {:?}",
                            host_type.name, host_type.kind
                        ),
                    ));
                }
                let span = host_type
                    .source
                    .as_ref()
                    .map_or_else(|| fallback.clone(), external_source_range);
                let type_definition = self.allocate_definition(
                    root.clone(),
                    host_module.clone(),
                    host_type.name.clone(),
                    DefinitionKind::HostInterface,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    IrEffect::Immediate,
                    span,
                    format!("host::type::{}", host_type.name),
                );
                self.definitions[type_definition.0 as usize].ty = IrType::Named(type_definition);
                if self
                    .symbols
                    .insert(
                        (root.clone(), host_module.clone(), host_type.name.clone()),
                        type_definition,
                    )
                    .is_some()
                {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2704,
                        Severity::Error,
                        format!("duplicate Host type `{}`", host_type.name),
                    ));
                }
                let mut metadata = TypeMetadata {
                    fields: BTreeMap::new(),
                    field_order: Vec::new(),
                    variants: BTreeMap::new(),
                    variant_order: Vec::new(),
                    stateful: false,
                    version: 0,
                };
                let mut analyzed_fields = Vec::new();
                for field in host_type.fields {
                    if field.stable_id.is_none() {
                        let mut diagnostic = Diagnostic::new(
                            ErrorCode::NX2101,
                            Severity::Error,
                            format!(
                                "Host field `{}.{}` is missing a stable ABI ID",
                                host_type.name, field.name
                            ),
                        );
                        if let Some(origin) = &field.source {
                            diagnostic = diagnostic.with_label(Label::primary(
                                origin.identity.clone(),
                                origin.range,
                                "NIDL fields require a stable ID",
                            ));
                        }
                        self.diagnostics.push(diagnostic);
                    }
                    let field_definition = self.allocate_definition(
                        root.clone(),
                        host_module.clone(),
                        field.name.clone(),
                        DefinitionKind::Field,
                        DeclarationVisibility::Public,
                        IrType::Unit,
                        IrEffect::Immediate,
                        field
                            .source
                            .as_ref()
                            .map_or_else(|| fallback.clone(), external_source_range),
                        format!("host::type::{}::field::{}", host_type.name, field.name),
                    );
                    metadata.fields.insert(field.name.clone(), field_definition);
                    metadata.field_order.push(field_definition);
                    self.members
                        .insert((type_definition, field.name.clone()), field_definition);
                    analyzed_fields.push((field_definition, field));
                }
                let mut analyzed_variants = Vec::new();
                for variant in host_type.variants {
                    if variant.stable_id.is_none() {
                        let mut diagnostic = Diagnostic::new(
                            ErrorCode::NX2101,
                            Severity::Error,
                            format!(
                                "Host variant `{}.{}` is missing a stable ABI ID",
                                host_type.name, variant.name
                            ),
                        );
                        if let Some(origin) = &variant.source {
                            diagnostic = diagnostic.with_label(Label::primary(
                                origin.identity.clone(),
                                origin.range,
                                "NIDL variants require a stable ID",
                            ));
                        }
                        self.diagnostics.push(diagnostic);
                    }
                    let variant_definition = self.allocate_definition(
                        root.clone(),
                        host_module.clone(),
                        variant.name.clone(),
                        DefinitionKind::Variant,
                        DeclarationVisibility::Public,
                        IrType::Named(type_definition),
                        IrEffect::Immediate,
                        variant
                            .source
                            .as_ref()
                            .map_or_else(|| fallback.clone(), external_source_range),
                        format!("host::type::{}::variant::{}", host_type.name, variant.name),
                    );
                    metadata
                        .variants
                        .insert(variant.name.clone(), variant_definition);
                    metadata.variant_order.push(variant_definition);
                    self.members
                        .insert((type_definition, variant.name.clone()), variant_definition);
                    let variant_key = (root.clone(), host_module.clone(), variant.name.clone());
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        self.symbols.entry(variant_key)
                    {
                        entry.insert(variant_definition);
                    } else {
                        self.diagnostics.push(Diagnostic::new(
                            ErrorCode::NX2704,
                            Severity::Error,
                            format!("ambiguous Host variant `{}`", variant.name),
                        ));
                    }
                    analyzed_variants.push((variant_definition, variant));
                }
                self.type_metadata.insert(type_definition, metadata);
                let stable_id = host_type.stable_id.unwrap_or_else(|| {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!("Host type `{}` is missing a stable ABI ID", host_type.name),
                    ));
                    nexa_core::StableId::default()
                });
                self.host_types.push(AnalyzedHostType {
                    definition: type_definition,
                    stable_id,
                    kind: host_type.kind,
                    source: host_type.source,
                    fields: analyzed_fields,
                    variants: analyzed_variants,
                });
            }
            let mut functions = host.functions;
            functions.sort_by(|left, right| {
                (left.import_index, &left.name).cmp(&(right.import_index, &right.name))
            });
            let mut binding = AnalyzedHostBinding {
                interface,
                interface_stable_id: host.interface_stable_id,
                namespaces: Vec::new(),
                functions: Vec::new(),
            };
            for function in functions {
                let span = function
                    .source
                    .as_ref()
                    .map_or_else(|| fallback.clone(), external_source_range);
                let id = self.allocate_definition(
                    root.clone(),
                    host_module.clone(),
                    function.name.clone(),
                    DefinitionKind::HostFunction,
                    DeclarationVisibility::Public,
                    IrType::Unit,
                    match function.mode {
                        HostFunctionMode::Sync => IrEffect::Immediate,
                        HostFunctionMode::Request => IrEffect::Task,
                    },
                    span,
                    format!("host::function::{}", function.name),
                );
                if self
                    .symbols
                    .insert(
                        (root.clone(), host_module.clone(), function.name.clone()),
                        id,
                    )
                    .is_some()
                {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2704,
                        Severity::Error,
                        format!("duplicate Host function `{}`", function.name),
                    ));
                }
                binding.functions.push(AnalyzedHostFunction {
                    definition: id,
                    stable_id: function.stable_id,
                    import_index: function.import_index,
                    mode: function.mode,
                    source: function.source.as_ref().map(external_source_range),
                });
                pending.push(Pending::HostFunction(id, interface, Box::new(function)));
            }
            self.host_binding = Some(binding);
        }

        for item in pending {
            match item {
                Pending::StaticFunction(id, function) => {
                    let signature_parameters = function
                        .parameters
                        .iter()
                        .map(|ty| self.resolve_standard_surface_type(ty, &function.type_parameters))
                        .collect::<Vec<_>>();
                    let signature_result = self
                        .resolve_standard_surface_type(&function.result, &function.type_parameters);
                    // A generic compiler intrinsic has no concrete definition-level result type.
                    // Its fully generic signature lives in `ExternalFunctionMetadata`, and every
                    // call site is instantiated before typed IR reaches codegen. Keep the dense
                    // definition table free of uninstantiated `TypeParameter` nodes without
                    // routing a valid generic declaration through the fail-closed concrete
                    // surface resolver.
                    self.definitions[id.0 as usize].ty = if function.type_parameters.is_empty() {
                        signature_result.clone()
                    } else {
                        IrType::Unit
                    };
                    self.external_functions.insert(
                        id,
                        ExternalFunctionMetadata {
                            parameters: signature_parameters,
                            result: signature_result,
                            effect: function.effect,
                            host: None,
                            generic: (!function.type_parameters.is_empty())
                                .then(|| (function.parameters.clone(), function.result.clone())),
                            type_parameters: function.type_parameters,
                            intrinsic: Some(function.intrinsic),
                        },
                    );
                }
                Pending::StaticConst(id, constant) => {
                    self.definitions[id.0 as usize].ty = self.resolve_surface_type(&constant.ty);
                }
                Pending::HostFunction(id, interface, function) => {
                    let parameters = function
                        .parameters
                        .iter()
                        .map(|ty| self.resolve_surface_type(ty))
                        .collect::<Vec<_>>();
                    let effect = match function.mode {
                        HostFunctionMode::Sync => IrEffect::Immediate,
                        HostFunctionMode::Request => IrEffect::Task,
                    };
                    let result = match function.mode {
                        HostFunctionMode::Sync => self.resolve_surface_type(&function.result),
                        HostFunctionMode::Request => {
                            if let Some(async_result) = function.async_result.as_ref() {
                                IrType::Result(
                                    Box::new(self.resolve_surface_type(&async_result.success)),
                                    Box::new(self.resolve_surface_type(&async_result.error)),
                                )
                            } else {
                                self.diagnostics.push(Diagnostic::new(
                                    ErrorCode::NX2101,
                                    Severity::Error,
                                    format!(
                                        "Host Request `{}` has no concrete async Result metadata",
                                        function.name
                                    ),
                                ));
                                IrType::Unit
                            }
                        }
                    };
                    self.definitions[id.0 as usize].ty = result.clone();
                    self.external_functions.insert(
                        id,
                        ExternalFunctionMetadata {
                            parameters,
                            result,
                            effect,
                            host: Some((interface, *function)),
                            generic: None,
                            type_parameters: Vec::new(),
                            intrinsic: None,
                        },
                    );
                }
            }
        }
        let host_types = self.host_types.clone();
        for host_type in host_types {
            for (definition, field) in host_type.fields {
                let ty = self.resolve_surface_type(&field.ty);
                self.definitions[definition.0 as usize].ty = ty;
            }
            for (definition, variant) in host_type.variants {
                let payload = variant
                    .payload
                    .iter()
                    .map(|ty| self.resolve_surface_type(ty))
                    .collect();
                self.variant_payloads.insert(definition, payload);
            }
        }
    }

    fn resolve_standard_surface_type(
        &mut self,
        ty: &SurfaceType,
        type_parameters: &[String],
    ) -> IrType {
        match ty {
            SurfaceType::TypeParameter(name) => {
                let index = type_parameters
                    .iter()
                    .position(|parameter| parameter == name)
                    .and_then(|index| u16::try_from(index).ok());
                index.map_or_else(
                    || {
                        self.unresolved_surface_type(format!(
                            "unbound external type parameter `{name}`"
                        ))
                    },
                    IrType::TypeParameter,
                )
            }
            SurfaceType::Option(inner) => IrType::Option(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            SurfaceType::Result(ok, error) => IrType::Result(
                Box::new(self.resolve_standard_surface_type(ok, type_parameters)),
                Box::new(self.resolve_standard_surface_type(error, type_parameters)),
            ),
            SurfaceType::Array(inner) => IrType::Array(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            SurfaceType::Map(key, value) => IrType::Map(
                Box::new(self.resolve_standard_surface_type(key, type_parameters)),
                Box::new(self.resolve_standard_surface_type(value, type_parameters)),
            ),
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.resolve_standard_surface_type(value, type_parameters))
                    .collect(),
            ),
            SurfaceType::HostRequest(inner) => {
                IrType::HostRequest(inner.as_ref().map(|inner| {
                    Box::new(self.resolve_standard_surface_type(inner, type_parameters))
                }))
            }
            SurfaceType::ResourceToken(inner) => {
                IrType::ResourceToken(inner.as_ref().map(|inner| {
                    Box::new(self.resolve_standard_surface_type(inner, type_parameters))
                }))
            }
            SurfaceType::Snapshot(inner) => IrType::Snapshot(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            SurfaceType::Buffer(inner) => IrType::Buffer(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            SurfaceType::StateHandle(inner) => IrType::StateHandle(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            _ => self.resolve_surface_type(ty),
        }
    }

    fn resolve_surface_type(&mut self, ty: &SurfaceType) -> IrType {
        match ty {
            SurfaceType::Unit => IrType::Unit,
            SurfaceType::TypeParameter(name) => self.unresolved_surface_type(format!(
                "external type parameter `{name}` has no generic binding"
            )),
            SurfaceType::Bool => IrType::Bool,
            SurfaceType::I32 => IrType::I32,
            SurfaceType::I64 => IrType::I64,
            SurfaceType::F32 => IrType::F32,
            SurfaceType::F64 => IrType::F64,
            SurfaceType::String => IrType::String,
            SurfaceType::Rune => IrType::Rune,
            SurfaceType::Named { module, name } => {
                let root = self.input.root_manifest.id.clone();
                if let Some(definition) = self
                    .symbols
                    .get(&(root, module.clone(), name.clone()))
                    .copied()
                    .filter(|definition| {
                        let definition = &self.definitions[definition.0 as usize];
                        is_nominal_type_kind(definition.kind)
                            && matches!(definition.ty, IrType::Named(_))
                    })
                {
                    return IrType::Named(definition);
                }
                let mut candidates = self.symbols.iter().filter_map(
                    |((_, candidate_module, candidate_name), definition)| {
                        (candidate_module == module && candidate_name == name && {
                            let definition = &self.definitions[definition.0 as usize];
                            is_nominal_type_kind(definition.kind)
                                && matches!(definition.ty, IrType::Named(_))
                        })
                        .then_some(*definition)
                    },
                );
                let first = candidates.next();
                match (first, candidates.next()) {
                    (Some(definition), None) => IrType::Named(definition),
                    (Some(_), Some(_)) => self.unresolved_surface_type(format!(
                        "external nominal type `{module}.{name}` is ambiguous across packages"
                    )),
                    (None, _) => self.unresolved_surface_type(format!(
                        "unknown external nominal type `{module}.{name}`"
                    )),
                }
            }
            SurfaceType::Option(inner) => {
                IrType::Option(Box::new(self.resolve_surface_type(inner)))
            }
            SurfaceType::Result(ok, error) => IrType::Result(
                Box::new(self.resolve_surface_type(ok)),
                Box::new(self.resolve_surface_type(error)),
            ),
            SurfaceType::Array(inner) => IrType::Array(Box::new(self.resolve_surface_type(inner))),
            SurfaceType::Map(key, value) => IrType::Map(
                Box::new(self.resolve_surface_type(key)),
                Box::new(self.resolve_surface_type(value)),
            ),
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.resolve_surface_type(value))
                    .collect(),
            ),
            SurfaceType::HostRequest(inner) => IrType::HostRequest(
                inner
                    .as_ref()
                    .map(|inner| Box::new(self.resolve_surface_type(inner))),
            ),
            SurfaceType::ResourceToken(inner) => IrType::ResourceToken(
                inner
                    .as_ref()
                    .map(|inner| Box::new(self.resolve_surface_type(inner))),
            ),
            SurfaceType::Snapshot(inner) => {
                IrType::Snapshot(Box::new(self.resolve_surface_type(inner)))
            }
            SurfaceType::Buffer(inner) => {
                IrType::Buffer(Box::new(self.resolve_surface_type(inner)))
            }
            SurfaceType::StateHandle(inner) => {
                IrType::StateHandle(Box::new(self.resolve_surface_type(inner)))
            }
        }
    }

    fn unresolved_surface_type(&mut self, message: String) -> IrType {
        if self.unresolved_surface_types.insert(message.clone()) {
            self.diagnostics
                .push(Diagnostic::new(ErrorCode::NX2101, Severity::Error, message));
        }
        IrType::Unit
    }

    fn instantiate_surface_type(
        &mut self,
        ty: &SurfaceType,
        bindings: &BTreeMap<String, IrType>,
    ) -> IrType {
        match ty {
            SurfaceType::TypeParameter(name) => bindings.get(name).cloned().unwrap_or(IrType::Unit),
            SurfaceType::Option(inner) => {
                IrType::Option(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            SurfaceType::Result(ok, error) => IrType::Result(
                Box::new(self.instantiate_surface_type(ok, bindings)),
                Box::new(self.instantiate_surface_type(error, bindings)),
            ),
            SurfaceType::Array(inner) => {
                IrType::Array(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            SurfaceType::Map(key, value) => IrType::Map(
                Box::new(self.instantiate_surface_type(key, bindings)),
                Box::new(self.instantiate_surface_type(value, bindings)),
            ),
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.instantiate_surface_type(value, bindings))
                    .collect(),
            ),
            SurfaceType::HostRequest(inner) => IrType::HostRequest(
                inner
                    .as_ref()
                    .map(|inner| Box::new(self.instantiate_surface_type(inner, bindings))),
            ),
            SurfaceType::ResourceToken(inner) => IrType::ResourceToken(
                inner
                    .as_ref()
                    .map(|inner| Box::new(self.instantiate_surface_type(inner, bindings))),
            ),
            SurfaceType::Snapshot(inner) => {
                IrType::Snapshot(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            SurfaceType::Buffer(inner) => {
                IrType::Buffer(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            SurfaceType::StateHandle(inner) => {
                IrType::StateHandle(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            _ => self.resolve_surface_type(ty),
        }
    }

    fn fallback_source_range(&self) -> SourceRange {
        self.modules.first().map_or_else(
            || SourceRange {
                source: SourceKey::new(
                    self.input.root_manifest.id.clone(),
                    NormalizedPackagePath::new("src/main.nexa")
                        .expect("fallback path is normalized"),
                ),
                start: 0,
                end: 0,
            },
            |module| SourceRange {
                source: module.source.clone(),
                start: 0,
                end: 0,
            },
        )
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_imports(&mut self) {
        let mut graphs = BTreeMap::<PackageId, ModuleGraph>::new();
        let graph_limits = CompilationLimits {
            imports_per_module: usize::MAX,
            module_edges: usize::MAX,
            ..self.input.compilation_options.limits
        };
        let mut total_import_edges = 0usize;
        for module in &self.modules {
            graphs
                .entry(module.key.package.clone())
                .or_default()
                .add_module(module.key.module.clone());
        }

        for module_index in 0..self.modules.len() {
            let module = self.modules[module_index].clone();
            let mut scope = ImportScope::default();
            self.db.clear_module_imports(&ModuleKey::new(
                module.key.package.clone(),
                module.key.module.clone(),
            ));
            for import in &module.ast.imports {
                let text = import.path.text();
                let local_alias = import
                    .alias
                    .as_ref()
                    .or_else(|| import.path.last())
                    .map(|alias| (alias.text.clone(), alias.range));
                let Some((local_alias, local_alias_range)) = local_alias else {
                    continue;
                };
                let target = if text == "host" {
                    if import.alias.is_none() {
                        self.push_source_error(
                            ErrorCode::NX2703,
                            &module.source,
                            byte_range(import.path.range),
                            "`import host` requires an explicit alias",
                            "write `import host as <name>;`",
                        );
                    }
                    if self.environment.host.is_none() {
                        self.push_source_error(
                            ErrorCode::NX2703,
                            &module.source,
                            byte_range(import.path.range),
                            "Host contract is unavailable",
                            "this build input has no structured Host surface",
                        );
                    }
                    Some(ImportTarget::Host)
                } else {
                    self.resolve_import_target(&module, &import.path)
                };
                let Some(target) = target else {
                    self.push_source_error(
                        ErrorCode::NX2703,
                        &module.source,
                        byte_range(import.path.range),
                        format!("unknown module import `{text}`"),
                        "the import must name a source module, dependency alias, or static module",
                    );
                    continue;
                };
                match scope.aliases.entry(local_alias.clone()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(target.clone());
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {
                        self.push_source_error(
                            ErrorCode::NX2704,
                            &module.source,
                            byte_range(local_alias_range),
                            format!("duplicate namespace alias `{local_alias}`"),
                            "each imported namespace needs a unique local alias",
                        );
                        continue;
                    }
                }
                if scope.aliases.len() > self.input.compilation_options.limits.imports_per_module {
                    self.push_source_error(
                        ErrorCode::NX2702,
                        &module.source,
                        byte_range(import.range),
                        format!("too many imports in {}", module.key.module),
                        format!(
                            "a module may resolve at most {} imports",
                            self.input.compilation_options.limits.imports_per_module
                        ),
                    );
                }
                total_import_edges = total_import_edges.saturating_add(1);
                if total_import_edges > self.input.compilation_options.limits.module_edges {
                    self.push_source_error(
                        ErrorCode::NX2702,
                        &module.source,
                        byte_range(import.range),
                        "module graph edge limit exceeded",
                        format!(
                            "the resolved package closure may contain at most {} import edges",
                            self.input.compilation_options.limits.module_edges
                        ),
                    );
                }
                let importer_module =
                    ModuleKey::new(module.key.package.clone(), module.key.module.clone());
                let resolved_target = match &target {
                    ImportTarget::Source(target) => ResolvedImportTarget::Module(ModuleKey::new(
                        target.package.clone(),
                        target.module.clone(),
                    )),
                    ImportTarget::Static(module) => ResolvedImportTarget::Static(module.clone()),
                    ImportTarget::Host => ResolvedImportTarget::Host,
                };
                self.resolved_import_edges.push(ResolvedImportEdge {
                    importer: importer_module.clone(),
                    alias: local_alias.clone(),
                    target: resolved_target,
                });
                match &target {
                    ImportTarget::Source(target) => {
                        if module.role == SourceRole::Production
                            && self.modules[*self
                                .module_indices
                                .get(target)
                                .expect("resolved source module exists")]
                            .role
                                == SourceRole::Test
                        {
                            self.push_source_error(
                                ErrorCode::NX2705,
                                &module.source,
                                byte_range(import.range),
                                "production modules cannot import test modules",
                                "move shared code under src/ with package visibility",
                            );
                        }
                        let resolved_source_target =
                            ModuleKey::new(target.package.clone(), target.module.clone());
                        if target.package == module.key.package {
                            if let Some(graph) = graphs.get_mut(&module.key.package)
                                && let Err(error) = graph.add_import(
                                    &module.key.module,
                                    &target.module,
                                    graph_limits,
                                )
                            {
                                self.push_source_error(
                                    ErrorCode::NX2702,
                                    &module.source,
                                    byte_range(import.range),
                                    error.to_string(),
                                    "invalid module graph edge",
                                );
                            }
                            self.db
                                .record_module_import(importer_module, resolved_source_target);
                        } else {
                            self.db
                                .record_dependency_import(importer_module, resolved_source_target);
                        }
                    }
                    ImportTarget::Host => {
                        self.host_namespaces.insert((
                            module.key.package.clone(),
                            module.key.module.clone(),
                            local_alias,
                        ));
                    }
                    ImportTarget::Static(_) => {}
                }
            }
            let importer = ModuleKey::new(module.key.package.clone(), module.key.module.clone());
            let mut canonical_imports = Vec::new();
            for (alias, target) in &scope.aliases {
                append_string(&mut canonical_imports, alias);
                match target {
                    ImportTarget::Source(target) => {
                        canonical_imports.push(0);
                        append_string(&mut canonical_imports, target.package.as_str());
                        append_string(&mut canonical_imports, target.module.as_str());
                    }
                    ImportTarget::Static(module) => {
                        canonical_imports.push(1);
                        append_string(&mut canonical_imports, module.as_str());
                    }
                    ImportTarget::Host => canonical_imports.push(2),
                }
            }
            let mut dependencies = semantic_input_query_keys(self.input);
            dependencies.extend([
                QueryKey::Parse(module.source.clone()),
                QueryKey::ModuleHeaders(importer.clone()),
            ]);
            self.db
                .record_resolved_imports(importer, canonical_imports, dependencies);
            self.imports.insert(module.key, scope);
        }

        for (package, graph) in graphs {
            if let Err(ModuleGraphError::Cycle(cycle)) = graph.validate_acyclic() {
                let message = format!("module cycle: {cycle}");
                let edges = cycle
                    .chain
                    .windows(2)
                    .filter_map(|edge| {
                        let from = &edge[0];
                        let to = &edge[1];
                        let index = *self.module_indices.get(&SourceModuleKey {
                            package: package.clone(),
                            module: from.clone(),
                        })?;
                        let module = &self.modules[index];
                        let import = module
                            .ast
                            .imports
                            .iter()
                            .find(|import| import.path.text() == to.as_str())?;
                        Some((
                            module.source.clone(),
                            byte_range(import.path.range),
                            format!("`{from}` imports `{to}`"),
                        ))
                    })
                    .collect::<Vec<_>>();
                if let Some((source, range, edge)) = edges.first() {
                    let mut diagnostic =
                        Diagnostic::new(ErrorCode::NX2702, Severity::Error, message).with_label(
                            Label::primary(source_identity(source), *range, edge.clone()),
                        );
                    for (source, range, edge) in edges.iter().skip(1) {
                        diagnostic = diagnostic.with_related(RelatedLocation::new(
                            source_identity(source),
                            *range,
                            edge.clone(),
                        ));
                    }
                    self.diagnostics.push(diagnostic);
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2702,
                        Severity::Error,
                        message,
                    ));
                }
            }
        }
        if let Some(binding) = &mut self.host_binding {
            binding.namespaces = self.host_namespaces.iter().cloned().collect();
        }
        self.resolved_import_edges.sort();
        self.resolved_import_edges.dedup();
    }

    fn resolve_import_target(
        &mut self,
        module: &ParsedModule,
        path: &ast::QualifiedName,
    ) -> Option<ImportTarget> {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>();
        if segments.is_empty() {
            return None;
        }

        let dependency = self
            .input
            .dependency_graph
            .dependencies_of(&module.key.package)
            .find(|edge| edge.alias.as_str() == segments[0]);
        let dependency_target = dependency.and_then(|edge| {
            (segments.len() > 1)
                .then(|| ModulePath::new(segments[1..].join(".")).ok())
                .flatten()
                .map(|target_module| SourceModuleKey {
                    package: edge.to.clone(),
                    module: target_module,
                })
                .filter(|target| self.module_indices.contains_key(target))
        });

        let module_path = ModulePath::new(path.text()).ok();
        let local_target = module_path.as_ref().and_then(|target_module| {
            let target = SourceModuleKey {
                package: module.key.package.clone(),
                module: target_module.clone(),
            };
            self.module_indices.contains_key(&target).then_some(target)
        });
        let standard_library_target = module_path.as_ref().and_then(|target_module| {
            let target = SourceModuleKey {
                package: PackageId::new(nexa_stdlib::PACKAGE_ID)
                    .expect("standard-library package ID is valid"),
                module: target_module.clone(),
            };
            self.module_indices.contains_key(&target).then_some(target)
        });
        let static_target = module_path.as_ref().and_then(|target_module| {
            // Compiler-provided standard-library source and its intrinsic surface are one
            // namespace. A legacy/static surface with the same canonical module augments that
            // namespace; it is not a second import candidate.
            if standard_library_target.is_some() {
                return None;
            }
            self.environment
                .static_modules
                .iter()
                .any(|surface| &surface.module == target_module)
                .then(|| target_module.clone())
        });

        let count = usize::from(dependency_target.is_some())
            + usize::from(local_target.is_some())
            + usize::from(standard_library_target.is_some())
            + usize::from(static_target.is_some());
        if count > 1 {
            self.push_source_error(
                ErrorCode::NX2704,
                &module.source,
                byte_range(path.range),
                format!("ambiguous module import `{}`", path.text()),
                "dependency aliases and source/static module paths must not overlap",
            );
            return None;
        }
        dependency_target
            .or(local_target)
            .or(standard_library_target)
            .map(ImportTarget::Source)
            .or_else(|| static_target.map(ImportTarget::Static))
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_declaration_signatures(&mut self) {
        let records = self.declaration_records.clone();
        for record in records {
            let module = self.modules[record.module_index].clone();
            match &record.declaration.kind {
                DeclarationKind::Function(function) => {
                    let parameter_ids = self
                        .function_signatures
                        .get(&record.definition)
                        .map_or_else(Vec::new, |signature| signature.parameters.clone());
                    let mut parameter_types = Vec::new();
                    for (parameter, definition) in function
                        .parameters
                        .iter()
                        .zip(parameter_ids.iter().copied())
                    {
                        let ty = self.resolve_type_ref(&module, &parameter.ty);
                        self.definitions[definition.0 as usize].ty = ty.clone();
                        self.validate_public_type_ref(
                            &module,
                            record.definition,
                            &parameter.ty,
                            &ty,
                        );
                        parameter_types.push(ty);
                    }
                    let result = function.result.as_ref().map_or(IrType::Unit, |syntax| {
                        let resolved = self.resolve_type_ref(&module, syntax);
                        self.validate_public_type_ref(
                            &module,
                            record.definition,
                            syntax,
                            &resolved,
                        );
                        resolved
                    });
                    self.definitions[record.definition.0 as usize].ty = result.clone();
                    let effect = if has_attribute(&record.declaration.attributes, "test")
                        && function.effect == FunctionEffect::Ordinary
                    {
                        IrEffect::Immediate
                    } else {
                        function_effect(function.effect)
                    };
                    self.definitions[record.definition.0 as usize].effect = effect;
                    self.function_signatures.insert(
                        record.definition,
                        FunctionSignature {
                            parameters: parameter_ids,
                            parameter_types: parameter_types.clone(),
                            result: result.clone(),
                            effect,
                        },
                    );
                }
                DeclarationKind::Type(ty) => {
                    let metadata = self
                        .type_metadata
                        .get(&record.definition)
                        .cloned()
                        .expect("type metadata was collected");
                    for (field, definition) in ty.fields.iter().filter_map(|field| {
                        metadata
                            .fields
                            .get(&field.name.text)
                            .copied()
                            .map(|id| (field, id))
                    }) {
                        let resolved = self.resolve_type_ref(&module, &field.ty);
                        self.definitions[definition.0 as usize].ty = resolved.clone();
                        self.validate_public_type_ref(
                            &module,
                            record.definition,
                            &field.ty,
                            &resolved,
                        );
                    }
                    for (variant, definition) in ty.variants.iter().filter_map(|variant| {
                        metadata
                            .variants
                            .get(&variant.name.text)
                            .copied()
                            .map(|id| (variant, id))
                    }) {
                        let payload = variant
                            .payload
                            .iter()
                            .map(|syntax| {
                                let resolved = self.resolve_type_ref(&module, syntax);
                                self.validate_public_type_ref(
                                    &module,
                                    record.definition,
                                    syntax,
                                    &resolved,
                                );
                                resolved
                            })
                            .collect::<Vec<_>>();
                        self.variant_payloads.insert(definition, payload.clone());
                    }
                    if metadata.stateful {
                        self.definitions[record.definition.0 as usize].kind =
                            DefinitionKind::Stateful;
                        let stable_id = self.stable_ids.get(&record.definition).copied();
                        let fields = metadata
                            .fields
                            .values()
                            .filter_map(|field| {
                                let stable_id = self.stable_ids.get(field).copied()?;
                                Some(AnalyzedStateField {
                                    definition: *field,
                                    ty: self.definitions[field.0 as usize].ty.clone(),
                                    stable_id,
                                })
                            })
                            .collect::<Vec<_>>();
                        if let Some(stable_id) = stable_id {
                            self.state_types.push(AnalyzedStateType {
                                definition: record.definition,
                                version: metadata.version,
                                stable_id,
                                fields,
                            });
                        }
                    }
                }
                DeclarationKind::Const(constant) => {
                    let ty = self.resolve_type_ref(&module, &constant.ty);
                    self.definitions[record.definition.0 as usize].ty = ty.clone();
                    self.validate_public_type_ref(&module, record.definition, &constant.ty, &ty);
                }
                DeclarationKind::Error => {}
            }
        }
        self.state_types.sort_by_key(|state| state.definition);
    }

    fn resolve_type_ref(&mut self, module: &ParsedModule, ty: &TypeRef) -> IrType {
        match &ty.kind {
            TypeKind::Named(name) => match name.text().as_str() {
                "HostRequest" => IrType::HostRequest(None),
                "ResourceToken" => IrType::ResourceToken(None),
                name_text => match builtin_type(name_text) {
                    Some(ty) => ty,
                    None => {
                        if let Some(definition) = self.builtin_types.get(name_text).copied() {
                            self.record_reference(module, name.range, definition);
                            IrType::Named(definition)
                        } else {
                            self.resolve_symbol_path(module, name, SymbolUse::Type)
                                .map_or(IrType::Unit, IrType::Named)
                        }
                    }
                },
            },
            TypeKind::Generic { base, arguments } => {
                let base_name = base.text();
                match (base_name.as_str(), arguments.as_slice()) {
                    ("Option", [inner]) => {
                        IrType::Option(Box::new(self.resolve_type_ref(module, inner)))
                    }
                    ("Result", [ok, error]) => IrType::Result(
                        Box::new(self.resolve_type_ref(module, ok)),
                        Box::new(self.resolve_type_ref(module, error)),
                    ),
                    ("Array", [inner]) => {
                        IrType::Array(Box::new(self.resolve_type_ref(module, inner)))
                    }
                    ("Map", [key, value]) => IrType::Map(
                        Box::new(self.resolve_type_ref(module, key)),
                        Box::new(self.resolve_type_ref(module, value)),
                    ),
                    ("HostRequest", [inner]) => {
                        IrType::HostRequest(Some(Box::new(self.resolve_type_ref(module, inner))))
                    }
                    ("ResourceToken", [inner]) => {
                        IrType::ResourceToken(Some(Box::new(self.resolve_type_ref(module, inner))))
                    }
                    ("Snapshot", [inner]) => {
                        IrType::Snapshot(Box::new(self.resolve_type_ref(module, inner)))
                    }
                    ("Buffer", [inner]) => {
                        IrType::Buffer(Box::new(self.resolve_type_ref(module, inner)))
                    }
                    ("StateHandle", [inner]) => {
                        IrType::StateHandle(Box::new(self.resolve_type_ref(module, inner)))
                    }
                    (
                        "Option" | "Result" | "Array" | "Map" | "HostRequest" | "ResourceToken"
                        | "Snapshot" | "Buffer" | "StateHandle",
                        _,
                    ) => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &module.source,
                            byte_range(ty.range),
                            format!("invalid type argument count for `{base_name}`"),
                            "container and runtime handle types require their exact declared arity",
                        );
                        IrType::Unit
                    }
                    _ => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &module.source,
                            byte_range(ty.range),
                            format!("user generic type `{base_name}` is not supported"),
                            "M4 only permits the built-in generic types",
                        );
                        IrType::Unit
                    }
                }
            }
            TypeKind::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.resolve_type_ref(module, value))
                    .collect(),
            ),
            TypeKind::Array(inner) => IrType::Array(Box::new(self.resolve_type_ref(module, inner))),
            TypeKind::Map { key, value } => IrType::Map(
                Box::new(self.resolve_type_ref(module, key)),
                Box::new(self.resolve_type_ref(module, value)),
            ),
            TypeKind::Option(inner) => {
                IrType::Option(Box::new(self.resolve_type_ref(module, inner)))
            }
            TypeKind::Result { ok, error } => IrType::Result(
                Box::new(self.resolve_type_ref(module, ok)),
                Box::new(self.resolve_type_ref(module, error)),
            ),
            TypeKind::Error => IrType::Unit,
        }
    }

    fn validate_public_type_ref(
        &mut self,
        module: &ParsedModule,
        owner: DefinitionId,
        syntax: &TypeRef,
        resolved: &IrType,
    ) {
        let visibility = self.definitions[owner.0 as usize].visibility;
        if visibility == DeclarationVisibility::Private {
            return;
        }
        match (&syntax.kind, resolved) {
            (TypeKind::Named(name), IrType::Named(target)) => {
                let range = name.last().map_or(name.range, |segment| segment.range);
                self.validate_public_type_target(module, visibility, *target, range);
            }
            (
                TypeKind::Generic { arguments, .. },
                IrType::Option(inner)
                | IrType::Array(inner)
                | IrType::Snapshot(inner)
                | IrType::Buffer(inner)
                | IrType::StateHandle(inner),
            )
            | (TypeKind::Generic { arguments, .. }, IrType::HostRequest(Some(inner)))
            | (TypeKind::Generic { arguments, .. }, IrType::ResourceToken(Some(inner))) => {
                if let Some(argument) = arguments.first() {
                    self.validate_public_type_ref(module, owner, argument, inner);
                }
            }
            (TypeKind::Generic { arguments, .. }, IrType::Result(ok, error))
            | (TypeKind::Generic { arguments, .. }, IrType::Map(ok, error)) => {
                if let Some(argument) = arguments.first() {
                    self.validate_public_type_ref(module, owner, argument, ok);
                }
                if let Some(argument) = arguments.get(1) {
                    self.validate_public_type_ref(module, owner, argument, error);
                }
            }
            (TypeKind::Tuple(syntax), IrType::Tuple(resolved)) => {
                for (syntax, resolved) in syntax.iter().zip(resolved) {
                    self.validate_public_type_ref(module, owner, syntax, resolved);
                }
            }
            (TypeKind::Array(syntax), IrType::Array(resolved))
            | (TypeKind::Option(syntax), IrType::Option(resolved)) => {
                self.validate_public_type_ref(module, owner, syntax, resolved);
            }
            (TypeKind::Map { key, value }, IrType::Map(resolved_key, resolved_value))
            | (
                TypeKind::Result {
                    ok: key,
                    error: value,
                },
                IrType::Result(resolved_key, resolved_value),
            ) => {
                self.validate_public_type_ref(module, owner, key, resolved_key);
                self.validate_public_type_ref(module, owner, value, resolved_value);
            }
            _ => {
                let mut named = Vec::new();
                collect_named_types(resolved, &mut named);
                for target in named {
                    self.validate_public_type_target(module, visibility, target, syntax.range);
                }
            }
        }
    }

    fn validate_public_type_target(
        &mut self,
        module: &ParsedModule,
        visibility: DeclarationVisibility,
        target: DefinitionId,
        range: TextRange,
    ) {
        let definition = self.definitions[target.0 as usize].clone();
        let valid = match visibility {
            DeclarationVisibility::Public => definition.visibility == DeclarationVisibility::Public,
            DeclarationVisibility::Package => {
                definition.package_id == module.key.package
                    && definition.visibility != DeclarationVisibility::Private
                    || definition.visibility == DeclarationVisibility::Public
            }
            DeclarationVisibility::Private => true,
        };
        if valid {
            return;
        }
        let declaration_span = self.definition_name_span(target);
        self.diagnostics.push(
            Diagnostic::new(
                ErrorCode::NX2706,
                Severity::Error,
                format!(
                    "{} API exposes inaccessible type `{}`",
                    visibility_name(visibility),
                    definition.name
                ),
            )
            .with_label(Label::primary(
                source_identity(&module.source),
                byte_range(range),
                "inaccessible type appears in this API surface",
            ))
            .with_related(RelatedLocation::new(
                source_identity(&declaration_span.source),
                range_from_source(&declaration_span),
                "type is declared here",
            )),
        );
    }

    fn resolve_symbol_path(
        &mut self,
        module: &ParsedModule,
        path: &ast::QualifiedName,
        usage: SymbolUse,
    ) -> Option<DefinitionId> {
        let id = self.lookup_symbol_path(module, path);
        let Some(id) = id else {
            self.push_source_error(
                match usage {
                    SymbolUse::Type => ErrorCode::NX2002,
                    SymbolUse::Value | SymbolUse::Callable => ErrorCode::NX2001,
                },
                &module.source,
                byte_range(path.range),
                format!("unknown {} `{}`", usage.name(), path.text()),
                "name is not declared in this module or imported namespace",
            );
            return None;
        };
        let definition = self.definitions[id.0 as usize].clone();
        let kind_valid = match usage {
            SymbolUse::Type => {
                matches!(
                    definition.kind,
                    DefinitionKind::Struct
                        | DefinitionKind::Enum
                        | DefinitionKind::Class
                        | DefinitionKind::Stateful
                        | DefinitionKind::HostInterface
                        | DefinitionKind::StandardLibrary
                ) && matches!(definition.ty, IrType::Named(_))
            }
            SymbolUse::Value => !matches!(
                definition.kind,
                DefinitionKind::Struct
                    | DefinitionKind::Enum
                    | DefinitionKind::Class
                    | DefinitionKind::Stateful
                    | DefinitionKind::HostInterface
            ),
            SymbolUse::Callable => matches!(
                definition.kind,
                DefinitionKind::Function
                    | DefinitionKind::Task
                    | DefinitionKind::HostFunction
                    | DefinitionKind::StandardLibrary
            ),
        };
        if !kind_valid {
            self.push_source_error(
                if usage == SymbolUse::Type {
                    ErrorCode::NX2002
                } else {
                    ErrorCode::NX2101
                },
                &module.source,
                byte_range(path.range),
                format!("`{}` is not a {}", path.text(), usage.name()),
                "symbol kind does not match this use",
            );
        }
        if !Self::visible_from(&definition, &module.key) {
            let use_range = path.last().map_or(path.range, |name| name.range);
            let declaration_span = self.definition_name_span(id);
            self.diagnostics.push(
                Diagnostic::new(
                    ErrorCode::NX2705,
                    Severity::Error,
                    format!(
                        "`{}` is not visible from {}",
                        definition.name, module.key.module
                    ),
                )
                .with_label(Label::primary(
                    source_identity(&module.source),
                    byte_range(use_range),
                    "inaccessible reference",
                ))
                .with_related(RelatedLocation::new(
                    source_identity(&declaration_span.source),
                    range_from_source(&declaration_span),
                    format!(
                        "declared with {} visibility",
                        visibility_name(definition.visibility)
                    ),
                )),
            );
        }
        self.record_reference(module, path.range, id);
        Some(id)
    }

    fn lookup_symbol_path(
        &self,
        module: &ParsedModule,
        path: &ast::QualifiedName,
    ) -> Option<DefinitionId> {
        let segments = &path.segments;
        let first = segments.first()?;
        let imported = self
            .imports
            .get(&module.key)
            .and_then(|scope| scope.aliases.get(&first.text));
        let (mut definition, consumed) = if let Some(target) = imported {
            let name = segments.get(1)?;
            let definition = match target {
                ImportTarget::Source(target) => self
                    .symbols
                    .get(&(
                        target.package.clone(),
                        target.module.clone(),
                        name.text.clone(),
                    ))
                    .copied(),
                ImportTarget::Static(target) => self
                    .symbols
                    .get(&(
                        self.input.root_manifest.id.clone(),
                        target.clone(),
                        name.text.clone(),
                    ))
                    .copied(),
                ImportTarget::Host => {
                    let host = ModulePath::new("host").expect("reserved host module");
                    self.symbols
                        .get(&(self.input.root_manifest.id.clone(), host, name.text.clone()))
                        .copied()
                }
            }?;
            (definition, 2)
        } else {
            let definition = self
                .symbols
                .get(&(
                    module.key.package.clone(),
                    module.key.module.clone(),
                    first.text.clone(),
                ))
                .copied()?;
            (definition, 1)
        };
        for member in segments.iter().skip(consumed) {
            definition = self
                .members
                .get(&(definition, member.text.clone()))
                .copied()?;
        }
        Some(definition)
    }

    fn visible_from(definition: &Definition, module: &SourceModuleKey) -> bool {
        if definition.package_id == module.package && definition.module == module.module {
            return true;
        }
        if definition.package_id == module.package {
            return definition.visibility != DeclarationVisibility::Private;
        }
        definition.visibility == DeclarationVisibility::Public
    }

    fn definition_name_span(&self, target: DefinitionId) -> SourceRange {
        for record in &self.declaration_records {
            let module = &self.modules[record.module_index];
            if record.definition == target {
                let range = match &record.declaration.kind {
                    DeclarationKind::Function(function) => function.name.range,
                    DeclarationKind::Type(ty) => ty.name.range,
                    DeclarationKind::Const(constant) => constant.name.range,
                    DeclarationKind::Error => record.declaration.range,
                };
                return source_range(&module.source, range);
            }
            if let DeclarationKind::Type(ty) = &record.declaration.kind
                && let Some(metadata) = self.type_metadata.get(&record.definition)
            {
                if let Some(field) = ty
                    .fields
                    .iter()
                    .find(|field| metadata.fields.get(&field.name.text).copied() == Some(target))
                {
                    return source_range(&module.source, field.name.range);
                }
                if let Some(variant) = ty.variants.iter().find(|variant| {
                    metadata.variants.get(&variant.name.text).copied() == Some(target)
                }) {
                    return source_range(&module.source, variant.name.range);
                }
            }
        }
        self.definitions[target.0 as usize].span.clone()
    }

    fn record_reference(&mut self, module: &ParsedModule, range: TextRange, target: DefinitionId) {
        if let Some(index) = self.module_indices.get(&module.key).copied() {
            self.resolved_references
                .entry(index)
                .or_default()
                .push(ResolvedReference {
                    span: source_range(&module.source, range),
                    target,
                });
        }
    }

    #[allow(clippy::too_many_lines)]
    fn validate_entry_and_exports(&mut self) {
        let records = self.declaration_records.clone();
        for record in records {
            let DeclarationKind::Function(function) = &record.declaration.kind else {
                continue;
            };
            let module = self.modules[record.module_index].clone();
            let lifecycle = matches!(
                function.effect,
                FunctionEffect::Migration | FunctionEffect::Activation | FunctionEffect::Cleanup
            ) || has_attribute(&record.declaration.attributes, "export")
                || has_attribute(&record.declaration.attributes, "handler");
            if lifecycle {
                let is_root_entry = self.input.root_manifest.kind == PackageKind::Application
                    && module.key.package == self.input.root_manifest.id
                    && self.input.root_manifest.entry() == Some(&module.key.module);
                if !is_root_entry || record.declaration.visibility != Visibility::Public {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(function.name.range),
                        format!(
                            "lifecycle/export function `{}` must be `pub` in the root Application entry module",
                            function.name.text
                        ),
                        "libraries and non-entry modules cannot define lifecycle exports",
                    );
                } else {
                    let lifecycle_slot = match function.effect {
                        FunctionEffect::Migration => Some(&mut self.lifecycle.migration),
                        FunctionEffect::Activation => Some(&mut self.lifecycle.activation),
                        FunctionEffect::Cleanup => Some(&mut self.lifecycle.cleanup),
                        FunctionEffect::Ordinary
                        | FunctionEffect::Immediate
                        | FunctionEffect::Task => None,
                    };
                    if let Some(slot) = lifecycle_slot {
                        if let Some(prior) = *slot {
                            let prior = self.definitions[prior.0 as usize].clone();
                            let prior_span = self.definition_name_span(prior.id);
                            self.diagnostics.push(
                                Diagnostic::new(
                                    ErrorCode::NX2740,
                                    Severity::Error,
                                    format!(
                                        "duplicate {} lifecycle function `{}`",
                                        effect_name(function_effect(function.effect)),
                                        function.name.text
                                    ),
                                )
                                .with_label(Label::primary(
                                    source_identity(&module.source),
                                    byte_range(function.name.range),
                                    "only one function may implement this lifecycle phase",
                                ))
                                .with_related(
                                    RelatedLocation::new(
                                        source_identity(&prior_span.source),
                                        range_from_source(&prior_span),
                                        "first lifecycle function",
                                    ),
                                ),
                            );
                        } else {
                            *slot = Some(record.definition);
                        }
                    }
                    if has_attribute(&record.declaration.attributes, "export")
                        || has_attribute(&record.declaration.attributes, "handler")
                    {
                        self.exports.push(AnalyzedExport {
                            name: function.name.text.clone(),
                            function: record.definition,
                            stable_id: self.definitions[record.definition.0 as usize]
                                .stable_symbol
                                .as_ref()
                                .map_or(nexa_core::StableId::default(), |stable| {
                                    stable.runtime_id.0
                                }),
                        });
                    }
                }
            }
        }

        if let Some(host) = self.environment.host.clone() {
            let Some(entry) = self.input.root_manifest.entry().cloned() else {
                return;
            };
            for required in &host.required_exports {
                let key = (
                    self.input.root_manifest.id.clone(),
                    entry.clone(),
                    required.name.clone(),
                );
                let Some(definition) = self.symbols.get(&key).copied() else {
                    let mut diagnostic = Diagnostic::new(
                        ErrorCode::NX7010,
                        Severity::Error,
                        format!("missing required export `{}`", required.name),
                    );
                    if let Some(source) = &required.source {
                        diagnostic = diagnostic.with_label(Label::primary(
                            source.identity.clone(),
                            source.range,
                            "Host contract requires this export",
                        ));
                    }
                    self.diagnostics.push(diagnostic);
                    continue;
                };
                let valid_visibility = self.definitions[definition.0 as usize].visibility
                    == DeclarationVisibility::Public;
                let required_parameters = required
                    .parameters
                    .iter()
                    .map(|ty| self.resolve_surface_type(ty))
                    .collect::<Vec<_>>();
                let required_result = self.resolve_surface_type(&required.result);
                let valid_signature =
                    self.function_signatures
                        .get(&definition)
                        .is_some_and(|signature| {
                            required
                                .effect
                                .is_none_or(|required| signature.effect == required)
                                && signature.parameter_types == required_parameters
                                && signature.result == required_result
                        });
                if !valid_visibility || !valid_signature {
                    let declared = self.definitions[definition.0 as usize].clone();
                    let mut diagnostic = Diagnostic::new(
                        ErrorCode::NX7011,
                        Severity::Error,
                        format!(
                            "required export `{}` has the wrong signature",
                            required.name
                        ),
                    )
                    .with_label(Label::primary(
                        source_identity(&declared.span.source),
                        range_from_source(&declared.span),
                        "export signature differs from the Host contract",
                    ));
                    if let Some(source) = &required.source {
                        diagnostic = diagnostic.with_related(RelatedLocation::new(
                            source.identity.clone(),
                            source.range,
                            "required Host export signature",
                        ));
                    }
                    self.diagnostics.push(diagnostic);
                    continue;
                }
                if !self
                    .exports
                    .iter()
                    .any(|export| export.function == definition)
                {
                    self.exports.push(AnalyzedExport {
                        name: required.name.clone(),
                        function: definition,
                        stable_id: required.stable_id,
                    });
                } else if let Some(export) = self
                    .exports
                    .iter_mut()
                    .find(|export| export.function == definition)
                {
                    export.stable_id = required.stable_id;
                }
            }
        }
        self.exports
            .sort_by(|left, right| (&left.name, left.function).cmp(&(&right.name, right.function)));
    }

    fn evaluate_constants(&mut self) {
        let constants = self
            .declaration_records
            .iter()
            .filter_map(|record| match &record.declaration.kind {
                DeclarationKind::Const(constant) => Some((
                    record.definition,
                    (record.module_index, constant.value.clone()),
                )),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let mut visiting = BTreeSet::new();
        for definition in constants.keys().copied().collect::<Vec<_>>() {
            let _ = self.evaluate_const(definition, &constants, &mut visiting);
        }
    }

    fn evaluate_const(
        &mut self,
        definition: DefinitionId,
        constants: &BTreeMap<DefinitionId, (usize, Expression)>,
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> Option<ConstValue> {
        if let Some(value) = self.const_values.get(&definition) {
            return Some(value.clone());
        }
        let (module_index, expression) = constants.get(&definition).cloned()?;
        if !visiting.insert(definition) {
            let module = self.modules[module_index].clone();
            self.push_source_error(
                ErrorCode::NX2720,
                &module.source,
                byte_range(expression.range),
                "constant evaluation cycle",
                "top-level const dependencies must be acyclic",
            );
            return None;
        }
        let module = self.modules[module_index].clone();
        let expected = self.definitions[definition.0 as usize].ty.clone();
        let value = self.evaluate_const_expression(
            &module,
            &expression,
            Some(&expected),
            constants,
            visiting,
        );
        visiting.remove(&definition);
        if let Some(value) = &value {
            self.const_values.insert(definition, value.clone());
        }
        value
    }

    #[allow(clippy::too_many_lines)]
    fn evaluate_const_expression(
        &mut self,
        module: &ParsedModule,
        expression: &Expression,
        expected: Option<&IrType>,
        constants: &BTreeMap<DefinitionId, (usize, Expression)>,
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> Option<ConstValue> {
        match &expression.kind {
            ExpressionKind::Literal(literal) => match literal_const(literal, expected) {
                Ok(value) => Some(value),
                Err(message) => {
                    self.invalid_const(module, expression.range, &message);
                    None
                }
            },
            ExpressionKind::Name(path) => {
                let definition = self.resolve_symbol_path(module, path, SymbolUse::Value)?;
                if self.definitions[definition.0 as usize].kind != DefinitionKind::Const {
                    self.invalid_const(module, expression.range, "only another const may be read");
                    return None;
                }
                self.evaluate_const(definition, constants, visiting)
            }
            ExpressionKind::Tuple(values) => values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let expected = expected.and_then(|expected| match expected {
                        IrType::Tuple(types) => types.get(index),
                        _ => None,
                    });
                    self.evaluate_const_expression(module, value, expected, constants, visiting)
                })
                .collect::<Option<Vec<_>>>()
                .map(ConstValue::Tuple),
            ExpressionKind::Array(values) => values
                .iter()
                .map(|value| {
                    let expected = expected.and_then(|expected| match expected {
                        IrType::Array(inner) => Some(inner.as_ref()),
                        _ => None,
                    });
                    self.evaluate_const_expression(module, value, expected, constants, visiting)
                })
                .collect::<Option<Vec<_>>>()
                .map(ConstValue::Array),
            ExpressionKind::Unary { operator, operand } => {
                let value =
                    self.evaluate_const_expression(module, operand, expected, constants, visiting)?;
                eval_const_unary(operator.kind, value).or_else(|| {
                    self.invalid_const(
                        module,
                        expression.range,
                        "invalid constant unary operation",
                    );
                    None
                })
            }
            ExpressionKind::Binary {
                left,
                operator,
                right,
            } => {
                let operand_expected = if matches!(
                    operator.kind,
                    BinaryOperatorKind::Equal
                        | BinaryOperatorKind::NotEqual
                        | BinaryOperatorKind::Less
                        | BinaryOperatorKind::LessEqual
                        | BinaryOperatorKind::Greater
                        | BinaryOperatorKind::GreaterEqual
                ) {
                    None
                } else {
                    expected
                };
                let left = self.evaluate_const_expression(
                    module,
                    left,
                    operand_expected,
                    constants,
                    visiting,
                )?;
                let right = self.evaluate_const_expression(
                    module,
                    right,
                    operand_expected,
                    constants,
                    visiting,
                )?;
                eval_const_binary(operator.kind, left, right).or_else(|| {
                    self.invalid_const(
                        module,
                        expression.range,
                        "invalid constant binary operation",
                    );
                    None
                })
            }
            ExpressionKind::Construct { ty, fields } => {
                let definition = self.resolve_symbol_path(module, ty, SymbolUse::Type)?;
                let mut values = Vec::new();
                for field in fields {
                    let Some(field_id) = self
                        .members
                        .get(&(definition, field.name.text.clone()))
                        .copied()
                    else {
                        self.invalid_const(module, field.range, "unknown constructor field");
                        return None;
                    };
                    let field_expected = self.definitions[field_id.0 as usize].ty.clone();
                    let value = self.evaluate_const_expression(
                        module,
                        &field.value,
                        Some(&field_expected),
                        constants,
                        visiting,
                    )?;
                    values.push((field_id, value));
                }
                values.sort_by_key(|(field, _)| *field);
                Some(ConstValue::Construct {
                    definition,
                    fields: values,
                })
            }
            ExpressionKind::Call {
                callee, arguments, ..
            } => {
                if let ExpressionKind::Name(path) = &callee.kind
                    && let Some(variant) = self.resolve_symbol_path(module, path, SymbolUse::Value)
                    && self.definitions[variant.0 as usize].kind == DefinitionKind::Variant
                {
                    let payload = self
                        .variant_payloads
                        .get(&variant)
                        .cloned()
                        .unwrap_or_default();
                    let values = arguments
                        .iter()
                        .enumerate()
                        .map(|(index, argument)| {
                            self.evaluate_const_expression(
                                module,
                                argument,
                                payload.get(index),
                                constants,
                                visiting,
                            )
                        })
                        .collect::<Option<Vec<_>>>()?;
                    return Some(ConstValue::Variant {
                        definition: variant,
                        values,
                    });
                }
                self.invalid_const(
                    module,
                    expression.range,
                    "constant expressions cannot call user or Host functions",
                );
                None
            }
            _ => {
                self.invalid_const(
                    module,
                    expression.range,
                    "expression is not permitted in a top-level const",
                );
                None
            }
        }
    }

    fn invalid_const(&mut self, module: &ParsedModule, range: TextRange, message: &str) {
        self.push_source_error(
            ErrorCode::NX2720,
            &module.source,
            byte_range(range),
            message,
            "const supports literals, arithmetic, comparisons, other consts, and pure constructors",
        );
    }

    #[allow(clippy::too_many_lines)]
    fn check_bodies(&mut self) {
        let records = self.declaration_records.clone();
        for record in records {
            let module = self.modules[record.module_index].clone();
            let typed = match &record.declaration.kind {
                DeclarationKind::Function(function) => {
                    let signature = self
                        .function_signatures
                        .get(&record.definition)
                        .cloned()
                        .expect("function signature was resolved");
                    let mut checker =
                        BodyChecker::new(self, module.clone(), Some(record.definition), &signature);
                    let body = checker.check_block(&function.body);
                    checker.validate_migration_body(&function.body, &body);
                    let locals = checker.locals;
                    if signature.effect == IrEffect::Task {
                        self.restricted
                            .entry(record.definition)
                            .or_default()
                            .insert(RestrictedOperation::Task);
                    }
                    match signature.effect {
                        IrEffect::Activation => {
                            self.restricted
                                .entry(record.definition)
                                .or_default()
                                .insert(RestrictedOperation::Activation);
                        }
                        IrEffect::Migration | IrEffect::Cleanup => {
                            self.restricted
                                .entry(record.definition)
                                .or_default()
                                .insert(RestrictedOperation::Migration);
                        }
                        IrEffect::Ordinary | IrEffect::Immediate | IrEffect::Task => {}
                    }
                    TypedDeclarationIr {
                        definition: record.definition,
                        body: TypedDeclarationBody::Function(TypedFunctionIr {
                            parameters: signature.parameters,
                            locals,
                            return_type: signature.result,
                            effect: signature.effect,
                            body,
                        }),
                    }
                }
                DeclarationKind::Const(constant) => {
                    let signature = FunctionSignature {
                        parameters: Vec::new(),
                        parameter_types: Vec::new(),
                        result: self.definitions[record.definition.0 as usize].ty.clone(),
                        effect: IrEffect::Immediate,
                    };
                    let mut checker = BodyChecker::new(self, module, None, &signature);
                    let expression =
                        checker.check_expression(&constant.value, Some(&signature.result));
                    checker.expect_type(&expression.ty, &signature.result, &expression.span);
                    TypedDeclarationIr {
                        definition: record.definition,
                        body: TypedDeclarationBody::Const(expression),
                    }
                }
                DeclarationKind::Type(ty) => {
                    let metadata = self
                        .type_metadata
                        .get(&record.definition)
                        .expect("type metadata exists");
                    let fields = metadata
                        .field_order
                        .iter()
                        .enumerate()
                        .map(|(order, definition)| FieldLayoutIr {
                            definition: *definition,
                            ty: self.definitions[definition.0 as usize].ty.clone(),
                            order: u32::try_from(order).unwrap_or(u32::MAX),
                        })
                        .collect::<Vec<_>>();
                    let variants = metadata
                        .variant_order
                        .iter()
                        .enumerate()
                        .map(|(tag, definition)| {
                            let values = self
                                .variant_payloads
                                .get(definition)
                                .cloned()
                                .unwrap_or_default();
                            let payload = match values.as_slice() {
                                [] => None,
                                [value] => Some(value.clone()),
                                _ => Some(IrType::Tuple(values)),
                            };
                            VariantLayoutIr {
                                definition: *definition,
                                tag: u32::try_from(tag).unwrap_or(u32::MAX),
                                payload,
                            }
                        })
                        .collect::<Vec<_>>();
                    let layout = match ty.kind {
                        TypeDeclarationKind::Struct => TypedTypeLayoutIr::Struct { fields },
                        TypeDeclarationKind::Class => TypedTypeLayoutIr::Class { fields },
                        TypeDeclarationKind::Stateful => TypedTypeLayoutIr::Stateful { fields },
                        TypeDeclarationKind::Enum => TypedTypeLayoutIr::Enum { variants },
                    };
                    TypedDeclarationIr {
                        definition: record.definition,
                        body: TypedDeclarationBody::TypeLayout(layout),
                    }
                }
                DeclarationKind::Error => continue,
            };
            self.typed_declarations
                .entry(record.module_index)
                .or_default()
                .push(typed);
        }
        for declarations in self.typed_declarations.values_mut() {
            declarations.sort_by_key(|declaration| declaration.definition);
        }
    }

    fn validate_tests(&mut self) {
        let records = self.declaration_records.clone();
        for record in records {
            if !has_attribute(&record.declaration.attributes, "test") {
                continue;
            }
            let module = self.modules[record.module_index].clone();
            let DeclarationKind::Function(function) = &record.declaration.kind else {
                self.invalid_test(
                    &module,
                    record.declaration.range,
                    "@test requires a function",
                );
                continue;
            };
            let Some(signature) = self.function_signatures.get(&record.definition).cloned() else {
                continue;
            };
            let declaration_is_valid = module.role == SourceRole::Test
                && module.key.package == self.input.root_manifest.id
                && module.key.module.as_str().starts_with("test.")
                && record.declaration.visibility == Visibility::Private
                && signature.parameters.is_empty()
                && signature.result == IrType::Bool
                && signature.effect == IrEffect::Immediate;
            if !declaration_is_valid {
                self.invalid_test(
                    &module,
                    function.name.range,
                    "@test must be a private zero-argument Immediate function returning bool under tests/",
                );
                continue;
            }
            if let Some((operation, path)) = self.restricted_path(record.definition) {
                let names = path
                    .iter()
                    .map(|definition| self.definitions[definition.0 as usize].name.as_str())
                    .collect::<Vec<_>>()
                    .join(" -> ");
                self.invalid_test(
                    &module,
                    function.name.range,
                    &format!(
                        "@test reaches forbidden {} operation through {names}",
                        restricted_name(operation)
                    ),
                );
                continue;
            }
            self.tests.push(AnalyzedTest {
                package_id: module.key.package,
                module: module.key.module,
                name: function.name.text.clone(),
                function: record.definition,
                span: source_range(&module.source, record.declaration.range),
            });
        }
        self.tests.sort_by(|left, right| {
            (&left.package_id, &left.module, &left.name).cmp(&(
                &right.package_id,
                &right.module,
                &right.name,
            ))
        });
    }

    fn restricted_path(
        &self,
        root: DefinitionId,
    ) -> Option<(RestrictedOperation, Vec<DefinitionId>)> {
        let mut pending = VecDeque::from([(root, vec![root])]);
        let mut visited = BTreeSet::new();
        while let Some((function, path)) = pending.pop_front() {
            if !visited.insert(function) {
                continue;
            }
            if let Some(operations) = self.restricted.get(&function)
                && let Some(operation) = operations.iter().next().copied()
            {
                return Some((operation, path));
            }
            for callee in self.call_edges.get(&function).into_iter().flatten() {
                let mut next = path.clone();
                next.push(*callee);
                pending.push_back((*callee, next));
            }
        }
        None
    }

    fn invalid_test(&mut self, module: &ParsedModule, range: TextRange, message: &str) {
        self.push_source_error(
            ErrorCode::NX2730,
            &module.source,
            byte_range(range),
            message,
            "@test cannot reach Host, Task, await/yield, lifecycle, or persistent State APIs",
        );
    }

    fn build_semantic_records(&mut self) {
        self.public_records.clear();
        for definition in &self.definitions {
            if definition.package_id != self.input.root_manifest.id
                || !self.definition_is_production(definition.id)
                || definition.visibility != DeclarationVisibility::Public
                || matches!(
                    definition.kind,
                    DefinitionKind::Parameter
                        | DefinitionKind::Local
                        | DefinitionKind::Field
                        | DefinitionKind::Variant
                        | DefinitionKind::HostInterface
                        | DefinitionKind::HostFunction
                        | DefinitionKind::StandardLibrary
                )
            {
                continue;
            }
            let payload = definition_fingerprint_payload(
                definition,
                self.function_signatures.get(&definition.id),
                self.const_values.get(&definition.id),
                self.type_metadata.get(&definition.id),
                &self.variant_payloads,
                &self.definitions,
            );
            self.public_records.push(SemanticFingerprintRecord {
                canonical_identity: definition.canonical_identity.clone(),
                kind: definition_kind_name(definition.kind).to_owned(),
                payload,
            });
        }
        self.public_records.sort_by(|left, right| {
            (&left.canonical_identity, &left.kind, &left.payload).cmp(&(
                &right.canonical_identity,
                &right.kind,
                &right.payload,
            ))
        });

        self.state_records.clear();
        for state in &self.state_types {
            let definition = &self.definitions[state.definition.0 as usize];
            if definition.package_id != self.input.root_manifest.id
                || !self.definition_is_production(state.definition)
            {
                continue;
            }
            let mut payload = Vec::new();
            append_u32(&mut payload, state.version);
            append_string(&mut payload, &format!("{}", state.stable_id));
            for field in &state.fields {
                let definition = &self.definitions[field.definition.0 as usize];
                append_string(&mut payload, &definition.canonical_identity);
                append_string(&mut payload, &format!("{}", field.stable_id));
                encode_type(&field.ty, &self.definitions, &mut payload);
            }
            self.state_records.push(SemanticFingerprintRecord {
                canonical_identity: definition.canonical_identity.clone(),
                kind: "stateful".into(),
                payload,
            });
        }
        self.state_records
            .sort_by(|left, right| left.canonical_identity.cmp(&right.canonical_identity));
    }

    fn definition_is_production(&self, definition: DefinitionId) -> bool {
        let source = &self.definitions[definition.0 as usize].span.source;
        self.modules
            .iter()
            .any(|module| module.source == *source && module.role == SourceRole::Production)
    }

    fn production_state_types(&self) -> Vec<AnalyzedStateType> {
        self.state_types
            .iter()
            .filter(|state| self.definition_is_production(state.definition))
            .cloned()
            .collect()
    }

    fn typed_modules(&mut self) -> Vec<TypedModuleIr> {
        let mut typed_modules = Vec::new();
        for (index, module) in self.modules.iter().enumerate() {
            if self.mode != AnalysisMode::Test && module.role != SourceRole::Production {
                continue;
            }
            let module_key = ModuleKey::new(module.key.package.clone(), module.key.module.clone());
            if let Some(cached) = self.db.typed_module(
                &module_key,
                &self.definitions,
                &self.typed_module_semantic_context,
            ) {
                typed_modules.push((*cached).clone());
                continue;
            }
            let mut references = self
                .resolved_references
                .get(&index)
                .cloned()
                .unwrap_or_default();
            references.sort_by(|left, right| {
                (left.span.start, left.span.end, left.target).cmp(&(
                    right.span.start,
                    right.span.end,
                    right.target,
                ))
            });
            references.dedup();
            let typed = TypedModuleIr {
                package_id: module.key.package.clone(),
                module: module.key.module.clone(),
                source: module.source.clone(),
                file_id: if module.compiler_provided {
                    *self
                        .compiler_file_ids
                        .get(&module.source)
                        .expect("compiler-provided sources have deterministic artifact FileIds")
                } else {
                    self.artifact_files
                        .id_for(&module.source)
                        .expect("resolved analysis sources have deterministic artifact FileIds")
                },
                syntax: Arc::clone(&module.syntax),
                resolved_references: references.into(),
                declarations: self
                    .typed_declarations
                    .get(&index)
                    .cloned()
                    .unwrap_or_default()
                    .into(),
            };
            self.db.store_typed_module(
                module_key.clone(),
                Arc::new(typed.clone()),
                &self.definitions,
                self.typed_module_semantic_context,
                {
                    let mut dependencies = semantic_input_query_keys(self.input);
                    dependencies.extend([
                        QueryKey::Parse(module.source.clone()),
                        QueryKey::ResolvedImports(module_key),
                    ]);
                    dependencies
                },
            );
            typed_modules.push(typed);
        }
        typed_modules
    }

    fn push_source_error(
        &mut self,
        code: ErrorCode,
        source: &SourceKey,
        range: ByteRange,
        message: impl Into<Arc<str>>,
        label: impl Into<Arc<str>>,
    ) {
        self.diagnostics.push(
            Diagnostic::new(code, Severity::Error, message).with_label(Label::primary(
                source_identity(source),
                range,
                label,
            )),
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolUse {
    Type,
    Value,
    Callable,
}

impl SymbolUse {
    const fn name(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Value => "value",
            Self::Callable => "function",
        }
    }
}

type MigrationPaths = BTreeSet<MigrationPathState>;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct MigrationPathState {
    reads: BTreeSet<StableId>,
    forwarding: BTreeMap<StableId, u8>,
    duplicate_forwarding: BTreeMap<StableId, (u32, u32)>,
    finish_count: u8,
    duplicate_finish: Option<(u32, u32)>,
    operation_after_finish: Option<(u32, u32)>,
    unforwarded_at_finish: BTreeSet<StableId>,
}

#[derive(Clone, Debug, Default)]
struct MigrationFlow {
    normal: MigrationPaths,
    returns: MigrationPaths,
    breaks: MigrationPaths,
    continues: MigrationPaths,
}

impl MigrationFlow {
    fn merge(&mut self, other: Self) {
        self.normal.extend(other.normal);
        self.returns.extend(other.returns);
        self.breaks.extend(other.breaks);
        self.continues.extend(other.continues);
    }
}

struct BodyChecker<'analyzer, 'input> {
    analyzer: &'analyzer mut Analyzer<'input>,
    module: ParsedModule,
    current_function: Option<DefinitionId>,
    return_type: IrType,
    effect: IrEffect,
    scopes: Vec<BTreeMap<String, DefinitionId>>,
    locals: Vec<DefinitionId>,
    readonly_loop_bindings: BTreeSet<DefinitionId>,
    loop_depth: usize,
    defer_count: usize,
    migration_expression_returns: MigrationPaths,
    reported_duplicate_forwarding: BTreeSet<(StableId, u32, u32)>,
    reported_finish_violations: BTreeSet<(u8, u32, u32)>,
    migration_path_limit_reported: bool,
    recovery_unit_spans: BTreeSet<(SourceKey, u32, u32)>,
}

impl<'analyzer, 'input> BodyChecker<'analyzer, 'input> {
    fn new(
        analyzer: &'analyzer mut Analyzer<'input>,
        module: ParsedModule,
        current_function: Option<DefinitionId>,
        signature: &FunctionSignature,
    ) -> Self {
        let parameters = signature
            .parameters
            .iter()
            .map(|id| (analyzer.definitions[id.0 as usize].name.clone(), *id))
            .collect();
        Self {
            analyzer,
            module,
            current_function,
            return_type: signature.result.clone(),
            effect: signature.effect,
            scopes: vec![parameters],
            locals: Vec::new(),
            readonly_loop_bindings: BTreeSet::new(),
            loop_depth: 0,
            defer_count: 0,
            migration_expression_returns: MigrationPaths::new(),
            reported_duplicate_forwarding: BTreeSet::new(),
            reported_finish_violations: BTreeSet::new(),
            migration_path_limit_reported: false,
            recovery_unit_spans: BTreeSet::new(),
        }
    }

    fn check_block(&mut self, block: &ast::Block) -> TypedBlockIr {
        self.scopes.push(BTreeMap::new());
        let statements = block
            .statements
            .iter()
            .filter_map(|statement| self.check_statement(statement))
            .collect();
        let tail = block.tail.as_ref().map(|expression| {
            Box::new(self.check_expression(expression, Some(&self.return_type.clone())))
        });
        self.scopes.pop();
        TypedBlockIr { statements, tail }
    }

    #[allow(clippy::too_many_lines)]
    fn check_statement(&mut self, statement: &Statement) -> Option<TypedStatementIr> {
        match &statement.kind {
            StatementKind::Bind {
                name, ty, value, ..
            } => {
                let expected = ty
                    .as_ref()
                    .map(|ty| self.analyzer.resolve_type_ref(&self.module, ty));
                let value = self.check_expression(value, expected.as_ref());
                let resolved = expected.unwrap_or_else(|| value.ty.clone());
                self.expect_type(&value.ty, &resolved, &value.span);
                let definition = self.allocate_local(name.text.clone(), resolved, name.range);
                self.scopes
                    .last_mut()
                    .expect("a lexical scope exists")
                    .insert(name.text.clone(), definition);
                Some(TypedStatementIr::Let {
                    definition,
                    value: Some(value),
                })
            }
            StatementKind::Assign { target, value } => {
                let place = self.check_place(target)?;
                let target_type = place_type(&place, &self.analyzer.definitions);
                let value = self.check_expression(value, Some(&target_type));
                self.expect_type(&value.ty, &target_type, &value.span);
                Some(TypedStatementIr::Assign {
                    target: place,
                    value,
                })
            }
            StatementKind::Return(value) => {
                let value = value
                    .as_ref()
                    .map(|value| self.check_expression(value, Some(&self.return_type.clone())));
                let actual = value
                    .as_ref()
                    .map_or(IrType::Unit, |value| value.ty.clone());
                let mismatch_span = value.as_ref().map_or_else(
                    || source_range(&self.module.source, statement.range),
                    |value| value.span.clone(),
                );
                self.expect_type(&actual, &self.return_type.clone(), &mismatch_span);
                Some(TypedStatementIr::Return(value))
            }
            StatementKind::If {
                condition,
                then_block,
                else_branch,
            } => {
                let condition = self.check_expression(condition, Some(&IrType::Bool));
                self.expect_type(&condition.ty, &IrType::Bool, &condition.span);
                let then_block = self.check_block(then_block);
                let else_block = else_branch
                    .as_ref()
                    .map(|branch| self.check_else_branch(branch));
                Some(TypedStatementIr::If {
                    condition,
                    then_block,
                    else_block,
                })
            }
            StatementKind::While { condition, body } => {
                let condition = self.check_expression(condition, Some(&IrType::Bool));
                self.expect_type(&condition.ty, &IrType::Bool, &condition.span);
                self.loop_depth += 1;
                let body = self.check_block(body);
                self.loop_depth = self.loop_depth.saturating_sub(1);
                Some(TypedStatementIr::While {
                    condition,
                    body,
                    max_iterations: self
                        .analyzer
                        .input
                        .compilation_options
                        .max_while_iterations
                        .max(1),
                })
            }
            StatementKind::For {
                binding,
                start,
                end,
                body,
            } => {
                let start = self.check_expression(start, Some(&IrType::I32));
                let end = self.check_expression(end, Some(&IrType::I32));
                self.expect_type(&start.ty, &IrType::I32, &start.span);
                self.expect_type(&end.ty, &IrType::I32, &end.span);
                self.scopes.push(BTreeMap::new());
                let definition =
                    self.allocate_local(binding.text.clone(), IrType::I32, binding.range);
                self.scopes
                    .last_mut()
                    .expect("for scope exists")
                    .insert(binding.text.clone(), definition);
                self.readonly_loop_bindings.insert(definition);
                self.loop_depth += 1;
                let body = self.check_block(body);
                self.loop_depth = self.loop_depth.saturating_sub(1);
                self.readonly_loop_bindings.remove(&definition);
                self.scopes.pop();
                let max_iterations = if let (Some(start), Some(end)) = (
                    constant_i32_expression(&start, &self.analyzer.const_values),
                    constant_i32_expression(&end, &self.analyzer.const_values),
                ) {
                    let iterations = i64::from(end).saturating_sub(i64::from(start)).max(0);
                    u32::try_from(iterations).unwrap_or(u32::MAX)
                } else {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(statement.range),
                        "static range endpoints must be compile-time i32 constants",
                        "use integer literals or top-level i32 const values",
                    );
                    0
                };
                if max_iterations
                    > self
                        .analyzer
                        .input
                        .compilation_options
                        .limits
                        .max_loop_iterations
                {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(statement.range),
                        format!(
                            "static range has {max_iterations} iterations, exceeding the limit of {}",
                            self
                                .analyzer
                                .input
                                .compilation_options
                                .limits
                                .max_loop_iterations
                        ),
                        "reduce the static range",
                    );
                }
                Some(TypedStatementIr::StaticRangeFor {
                    binding: definition,
                    start,
                    end,
                    body,
                    max_iterations,
                })
            }
            StatementKind::Break => {
                self.validate_loop_control(statement.range, "break");
                Some(TypedStatementIr::Break)
            }
            StatementKind::Continue => {
                self.validate_loop_control(statement.range, "continue");
                Some(TypedStatementIr::Continue)
            }
            StatementKind::Yield => {
                if self.effect != IrEffect::Task {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        byte_range(statement.range),
                        "yield is only valid in a Task",
                        "ordinary and Immediate functions cannot yield",
                    );
                }
                self.mark_restricted(RestrictedOperation::Yield);
                Some(TypedStatementIr::Yield)
            }
            StatementKind::Defer(expression) => {
                let mut expression = self.check_expression(expression, None);
                let visible = self
                    .scopes
                    .iter()
                    .flat_map(|scope| scope.values().copied())
                    .collect::<BTreeSet<_>>();
                let mut referenced = BTreeSet::new();
                collect_expression_references(&expression, &mut referenced);
                referenced.retain(|definition| visible.contains(definition));
                let ordinal = self.defer_count;
                self.defer_count = self.defer_count.saturating_add(1);
                let parent = self.current_function.map_or_else(
                    || "const".to_owned(),
                    |definition| {
                        self.analyzer.definitions[definition.0 as usize]
                            .name
                            .clone()
                    },
                );
                let cleanup_name = format!("__defer_{parent}_{ordinal}");
                let canonical_identity = self.analyzer.generated_canonical_identity(
                    &self.module,
                    SymbolKind::Function,
                    &cleanup_name,
                    statement.range,
                );
                let cleanup = self.analyzer.allocate_definition(
                    self.module.key.package.clone(),
                    self.module.key.module.clone(),
                    cleanup_name,
                    DefinitionKind::Function,
                    DeclarationVisibility::Private,
                    IrType::Unit,
                    IrEffect::Cleanup,
                    source_range(&self.module.source, statement.range),
                    canonical_identity,
                );
                let mut parameters = Vec::new();
                let mut captures = Vec::new();
                let mut replacements = BTreeMap::new();
                for (capture_order, original) in referenced.into_iter().enumerate() {
                    let original_definition =
                        self.analyzer.definitions[original.0 as usize].clone();
                    let parameter = self.analyzer.allocate_definition(
                        self.module.key.package.clone(),
                        self.module.key.module.clone(),
                        format!("__capture_{capture_order}_{}", original_definition.name),
                        DefinitionKind::Parameter,
                        DeclarationVisibility::Private,
                        original_definition.ty.clone(),
                        IrEffect::Immediate,
                        source_range(&self.module.source, statement.range),
                        format!(
                            "{}::{}::cleanup::{parent}::{ordinal}::capture::{capture_order}",
                            self.module.key.package, self.module.key.module
                        ),
                    );
                    parameters.push(parameter);
                    replacements.insert(original, parameter);
                    captures.push(TypedExpressionIr {
                        ty: original_definition.ty,
                        effect: IrEffect::Immediate,
                        span: source_range(&self.module.source, statement.range),
                        kind: TypedExpressionKind::Reference(original),
                    });
                }
                rewrite_expression_references(&mut expression, &replacements);
                let module_index = *self
                    .analyzer
                    .module_indices
                    .get(&self.module.key)
                    .expect("body module is registered");
                self.analyzer
                    .typed_declarations
                    .entry(module_index)
                    .or_default()
                    .push(TypedDeclarationIr {
                        definition: cleanup,
                        body: TypedDeclarationBody::Function(TypedFunctionIr {
                            parameters,
                            locals: Vec::new(),
                            return_type: IrType::Unit,
                            effect: IrEffect::Cleanup,
                            body: TypedBlockIr {
                                statements: vec![TypedStatementIr::Expression(expression)],
                                tail: None,
                            },
                        }),
                    });
                Some(TypedStatementIr::Defer { cleanup, captures })
            }
            StatementKind::Expression(expression) => Some(TypedStatementIr::Expression(
                self.check_expression(expression, None),
            )),
            StatementKind::Error => None,
        }
    }

    fn check_else_branch(&mut self, branch: &ElseBranch) -> TypedBlockIr {
        match branch {
            ElseBranch::Block(block) => self.check_block(block),
            ElseBranch::If(statement) => TypedBlockIr {
                statements: self.check_statement(statement).into_iter().collect(),
                tail: None,
            },
        }
    }

    fn check_expression(
        &mut self,
        expression: &Expression,
        expected: Option<&IrType>,
    ) -> TypedExpressionIr {
        self.check_expression_inner(expression, expected, false)
    }

    #[allow(clippy::too_many_lines)]
    fn check_expression_inner(
        &mut self,
        expression: &Expression,
        expected: Option<&IrType>,
        awaited: bool,
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, expression.range);
        match &expression.kind {
            ExpressionKind::Literal(literal) => {
                let (ty, literal) = typed_literal(literal, expected).unwrap_or_else(|message| {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(expression.range),
                        message,
                        "numeric literal must fit its inferred or declared scalar type",
                    );
                    (self.recovery_unit_type(&span), IrLiteral::Unit)
                });
                TypedExpressionIr {
                    ty,
                    effect: IrEffect::Immediate,
                    span,
                    kind: TypedExpressionKind::Literal(literal),
                }
            }
            ExpressionKind::Name(path) => {
                if let Some(value) = self.check_receiver_name(path) {
                    return value;
                }
                if path.segments.len() == 1
                    && path.segments[0].text == "None"
                    && let Some(value) = self.check_empty_builtin_variant(
                        BuiltinVariantIr::OptionNone,
                        expected,
                        path.range,
                    )
                {
                    return value;
                }
                let definition =
                    self.analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Value);
                if let Some(definition) = definition {
                    self.reference_or_unit_variant(definition, span)
                } else {
                    self.error_expression(span)
                }
            }
            ExpressionKind::Tuple(values) => {
                let values = values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let expected = expected.and_then(|ty| match ty {
                            IrType::Tuple(values) => values.get(index),
                            _ => None,
                        });
                        self.check_expression(value, expected)
                    })
                    .collect::<Vec<_>>();
                TypedExpressionIr {
                    ty: IrType::Tuple(values.iter().map(|value| value.ty.clone()).collect()),
                    effect: expression_effect(&values),
                    span,
                    kind: TypedExpressionKind::Tuple(values),
                }
            }
            ExpressionKind::Array(values) => {
                let expected_element = expected.and_then(|ty| match ty {
                    IrType::Array(inner) => Some(inner.as_ref()),
                    _ => None,
                });
                let values = values
                    .iter()
                    .map(|value| self.check_expression(value, expected_element))
                    .collect::<Vec<_>>();
                let element = expected_element
                    .cloned()
                    .or_else(|| values.first().map(|value| value.ty.clone()))
                    .unwrap_or(IrType::Unit);
                for value in &values {
                    self.expect_type(&value.ty, &element, &value.span);
                }
                TypedExpressionIr {
                    ty: IrType::Array(Box::new(element)),
                    effect: expression_effect(&values),
                    span,
                    kind: TypedExpressionKind::Array(values),
                }
            }
            ExpressionKind::Unary { operator, operand } => {
                let operand = self.check_expression(operand, expected);
                let operator = match operator.kind {
                    UnaryOperatorKind::Positive => return operand,
                    UnaryOperatorKind::Negate => UnaryOperator::Negate,
                    UnaryOperatorKind::Not => UnaryOperator::Not,
                };
                let result = match operator {
                    UnaryOperator::Negate if is_numeric(&operand.ty) => operand.ty.clone(),
                    UnaryOperator::Not if operand.ty == IrType::Bool => IrType::Bool,
                    _ => {
                        self.type_error(operand.span.clone(), "invalid unary operand type");
                        self.recovery_unit_type(&span)
                    }
                };
                TypedExpressionIr {
                    ty: result,
                    effect: operand.effect,
                    span,
                    kind: TypedExpressionKind::Unary {
                        operator,
                        operand: Box::new(operand),
                    },
                }
            }
            ExpressionKind::Binary {
                left,
                operator,
                right,
            } => {
                let left = self.check_expression(left, expected);
                let right = self.check_expression(right, Some(&left.ty));
                self.expect_type(&right.ty, &left.ty, &right.span);
                let result = binary_result(operator.kind, &left.ty, &self.analyzer.definitions)
                    .unwrap_or_else(|| {
                        self.type_error(span.clone(), "invalid binary operand type");
                        self.recovery_unit_type(&span)
                    });
                TypedExpressionIr {
                    ty: result,
                    effect: max_effect(left.effect, right.effect),
                    span,
                    kind: TypedExpressionKind::Binary {
                        operator: binary_operator(operator.kind),
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                }
            }
            ExpressionKind::Call {
                callee,
                type_arguments,
                arguments,
            } => self.check_call(
                expression,
                callee,
                type_arguments,
                arguments,
                expected,
                awaited,
            ),
            ExpressionKind::Construct { ty, fields } => {
                self.check_construct(expression, ty, fields)
            }
            ExpressionKind::New { ty, fields } => {
                let definition = match &ty.kind {
                    TypeKind::Named(path) | TypeKind::Generic { base: path, .. } => self
                        .analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Type),
                    _ => None,
                };
                if let Some(definition) = definition {
                    self.check_construct_fields(expression, definition, fields)
                } else {
                    self.error_expression(span)
                }
            }
            ExpressionKind::With { value, fields } => {
                let value = self.check_expression(value, expected);
                let IrType::Named(definition) = value.ty else {
                    self.type_error(span.clone(), "`with` requires a struct value");
                    return self.error_expression(span);
                };
                if self.analyzer.definitions[definition.0 as usize].kind != DefinitionKind::Struct {
                    self.type_error(span.clone(), "`with` requires a struct value");
                    return self.error_expression(span);
                }
                let fields = self.check_fields(definition, fields);
                TypedExpressionIr {
                    ty: IrType::Named(definition),
                    effect: value.effect,
                    span,
                    kind: TypedExpressionKind::Update {
                        base: Box::new(value),
                        fields,
                    },
                }
            }
            ExpressionKind::Member { receiver, member } => {
                let receiver = self.check_expression(receiver, None);
                self.check_field_access(receiver, member, expression.range)
                    .unwrap_or_else(|| self.error_expression(span))
            }
            ExpressionKind::Index { receiver, index } => {
                let receiver = self.check_expression(receiver, None);
                let (index_type, result) = match &receiver.ty {
                    IrType::Array(inner) | IrType::Buffer(inner) => {
                        (IrType::I32, inner.as_ref().clone())
                    }
                    IrType::Map(key, value) => (key.as_ref().clone(), value.as_ref().clone()),
                    IrType::String => (IrType::I32, IrType::Rune),
                    _ => {
                        self.type_error(span.clone(), "value is not indexable");
                        (IrType::I32, self.recovery_unit_type(&span))
                    }
                };
                let index = self.check_expression(index, Some(&index_type));
                self.expect_type(&index.ty, &index_type, &index.span);
                TypedExpressionIr {
                    ty: result,
                    effect: max_effect(receiver.effect, index.effect),
                    span,
                    kind: TypedExpressionKind::Index {
                        base: Box::new(receiver),
                        index: Box::new(index),
                    },
                }
            }
            ExpressionKind::Await(value) => {
                if self.effect != IrEffect::Task {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        byte_range(TextRange::at(expression.range.start, 5)),
                        "await is only valid in a Task",
                        "ordinary and Immediate functions cannot await",
                    );
                }
                self.mark_restricted(RestrictedOperation::Await);
                let operand_range = byte_range(value.range);
                let value = self.check_expression_inner(value, expected, true);
                if self.effect == IrEffect::Task && value.effect != IrEffect::Task {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        operand_range,
                        "await requires a Task or Host Request operand",
                        "remove `await` or call a Task/Request function",
                    );
                }
                if value.ty == IrType::Unit && self.is_recovery_unit(&value.span) {
                    self.mark_recovery_unit(&span);
                }
                TypedExpressionIr {
                    ty: value.ty.clone(),
                    effect: IrEffect::Task,
                    span,
                    kind: TypedExpressionKind::Await(Box::new(value)),
                }
            }
            ExpressionKind::Try(value) => {
                // `await request()?` is represented as Await(Try(Call)) by the lossless AST.
                // Preserve the enclosing await context through `?` so the Request call itself is
                // not incorrectly diagnosed as un-awaited.
                let value = self.check_expression_inner(value, None, awaited);
                let question_range = TextRange::at(
                    TextSize::new(expression.range.end.get().saturating_sub(1)),
                    1,
                );
                let result = match &value.ty {
                    IrType::Result(ok, expression_error) => {
                        if let IrType::Result(_, function_error) = &self.return_type
                            && function_error != expression_error
                        {
                            self.analyzer.diagnostics.push(
                                Diagnostic::new(
                                    ErrorCode::NX2221,
                                    Severity::Error,
                                    "the propagated error type does not match the function error type",
                                )
                                .with_label(Label::primary(
                                    source_identity(&self.module.source),
                                    byte_range(question_range),
                                    "incompatible error propagation",
                                ))
                                .with_note(format!("function error type: {function_error:?}"))
                                .with_note(format!("expression error type: {expression_error:?}")),
                            );
                            self.recovery_unit_type(&span)
                        } else {
                            ok.as_ref().clone()
                        }
                    }
                    actual => {
                        self.analyzer.diagnostics.push(
                            Diagnostic::new(
                                ErrorCode::NX2220,
                                Severity::Error,
                                "the try operator requires a Result value",
                            )
                            .with_label(Label::primary(
                                source_identity(&self.module.source),
                                byte_range(question_range),
                                "this expression does not produce Result",
                            ))
                            .with_note(format!("actual type: {actual:?}")),
                        );
                        self.recovery_unit_type(&span)
                    }
                };
                TypedExpressionIr {
                    ty: result,
                    effect: value.effect,
                    span,
                    kind: TypedExpressionKind::Try(Box::new(value)),
                }
            }
            ExpressionKind::Interpolation(parts) => {
                let mut values = Vec::new();
                for part in parts {
                    match part {
                        InterpolationPart::Text { cooked, range, .. } => {
                            values.push(TypedExpressionIr {
                                ty: IrType::String,
                                effect: IrEffect::Immediate,
                                span: source_range(&self.module.source, *range),
                                kind: TypedExpressionKind::Literal(IrLiteral::String(
                                    cooked.clone(),
                                )),
                            });
                        }
                        InterpolationPart::Expression(expression) => {
                            let value = self.check_expression(expression, None);
                            if !is_scalar(&value.ty) {
                                self.type_error(
                                    value.span.clone(),
                                    "interpolation only accepts scalar values",
                                );
                            }
                            values.push(value);
                        }
                    }
                }
                TypedExpressionIr {
                    ty: IrType::String,
                    effect: expression_effect(&values),
                    span,
                    kind: TypedExpressionKind::StringInterpolation(values),
                }
            }
            ExpressionKind::Match { value, arms } => {
                self.check_match(expression, value, arms, expected)
            }
            ExpressionKind::Error => self.error_expression(span),
        }
    }

    /// Resolves a qualified expression whose first segment is a lexical/local value.
    ///
    /// The lossless parser intentionally keeps `value.field.more` as one qualified name. Semantic
    /// analysis gives a lexical value precedence over an imported namespace. An imported
    /// namespace can also contribute the receiver (`import app.util as u; u.origin.x`): after the
    /// namespace and value segments are consumed, every remaining segment is resolved from the
    /// receiver's nominal [`IrType::Named`] owner and lowered as a real field access.
    fn check_receiver_name(&mut self, path: &ast::QualifiedName) -> Option<TypedExpressionIr> {
        let (mut value, consumed) = self.check_qualified_receiver_base(path)?;
        for member in path.segments.iter().skip(consumed) {
            let Some(field) = self.check_field_access(value, member, path.range) else {
                return Some(self.error_expression(source_range(&self.module.source, path.range)));
            };
            value = field;
        }
        value.span = source_range(&self.module.source, path.range);
        Some(value)
    }

    /// Returns a true runtime value at the start of a qualified path and the number of namespace /
    /// value segments consumed to obtain it.
    ///
    /// This deliberately refuses types, variants and callables. Those symbolic paths must remain
    /// available to the normal namespace/type/constructor resolver instead of being mistaken for
    /// runtime receiver expressions.
    fn check_qualified_receiver_base(
        &mut self,
        path: &ast::QualifiedName,
    ) -> Option<(TypedExpressionIr, usize)> {
        let first = path.segments.first()?;
        if let Some(definition) = self.local(&first.text) {
            self.analyzer
                .record_reference(&self.module, first.range, definition);
            return Some((self.receiver_reference(definition, first.range), 1));
        }

        let imported = self
            .analyzer
            .imports
            .get(&self.module.key)
            .and_then(|scope| scope.aliases.get(&first.text));
        let consumed: usize = if imported.is_some() { 2 } else { 1 };
        let value = path.segments.get(consumed.saturating_sub(1))?;
        let definition = if let Some(target) = imported {
            match target {
                ImportTarget::Source(target) => self
                    .analyzer
                    .symbols
                    .get(&(
                        target.package.clone(),
                        target.module.clone(),
                        value.text.clone(),
                    ))
                    .copied(),
                ImportTarget::Static(target) => self
                    .analyzer
                    .symbols
                    .get(&(
                        self.analyzer.input.root_manifest.id.clone(),
                        target.clone(),
                        value.text.clone(),
                    ))
                    .copied(),
                ImportTarget::Host => {
                    let host = ModulePath::new("host").expect("reserved host module");
                    self.analyzer
                        .symbols
                        .get(&(
                            self.analyzer.input.root_manifest.id.clone(),
                            host,
                            value.text.clone(),
                        ))
                        .copied()
                }
            }
        } else {
            self.analyzer
                .symbols
                .get(&(
                    self.module.key.package.clone(),
                    self.module.key.module.clone(),
                    value.text.clone(),
                ))
                .copied()
        }?;
        if !self.is_receiver_value_definition(definition) {
            return None;
        }

        let receiver_path = ast::QualifiedName {
            segments: path.segments[..consumed].to_vec(),
            range: TextRange::new(first.range.start, value.range.end),
        };
        let definition =
            self.analyzer
                .resolve_symbol_path(&self.module, &receiver_path, SymbolUse::Value)?;
        Some((
            self.receiver_reference(definition, receiver_path.range),
            consumed,
        ))
    }

    fn receiver_reference(&self, definition: DefinitionId, range: TextRange) -> TypedExpressionIr {
        let declared = &self.analyzer.definitions[definition.0 as usize];
        TypedExpressionIr {
            ty: declared.ty.clone(),
            effect: declared.effect,
            span: source_range(&self.module.source, range),
            kind: TypedExpressionKind::Reference(definition),
        }
    }

    fn is_receiver_value_definition(&self, definition: DefinitionId) -> bool {
        match self.analyzer.definitions[definition.0 as usize].kind {
            DefinitionKind::Const | DefinitionKind::Parameter | DefinitionKind::Local => true,
            DefinitionKind::StandardLibrary => {
                !self.analyzer.external_functions.contains_key(&definition)
                    && !self.analyzer.type_metadata.contains_key(&definition)
            }
            DefinitionKind::Function
            | DefinitionKind::Task
            | DefinitionKind::Struct
            | DefinitionKind::Enum
            | DefinitionKind::Class
            | DefinitionKind::Stateful
            | DefinitionKind::Field
            | DefinitionKind::Variant
            | DefinitionKind::HostInterface
            | DefinitionKind::HostFunction => false,
        }
    }

    fn reference_or_unit_variant(
        &mut self,
        definition: DefinitionId,
        span: SourceRange,
    ) -> TypedExpressionIr {
        let declared = self.analyzer.definitions[definition.0 as usize].clone();
        if declared.kind != DefinitionKind::Variant {
            return TypedExpressionIr {
                ty: declared.ty,
                effect: declared.effect,
                span,
                kind: TypedExpressionKind::Reference(definition),
            };
        }
        let payload = self
            .analyzer
            .variant_payloads
            .get(&definition)
            .map_or(&[][..], Vec::as_slice);
        if !payload.is_empty() {
            self.type_error(span.clone(), "enum variant requires a constructor payload");
        }
        let IrType::Named(enum_definition) = declared.ty.clone() else {
            self.type_error(span.clone(), "enum variant has no nominal enum type");
            return self.error_expression(span);
        };
        TypedExpressionIr {
            ty: declared.ty,
            effect: IrEffect::Immediate,
            span,
            kind: TypedExpressionKind::EnumConstruct {
                enum_definition,
                variant_definition: definition,
                payload: None,
            },
        }
    }

    fn check_empty_builtin_variant(
        &mut self,
        variant: BuiltinVariantIr,
        expected: Option<&IrType>,
        range: TextRange,
    ) -> Option<TypedExpressionIr> {
        let span = source_range(&self.module.source, range);
        let ty = match (variant, expected) {
            (BuiltinVariantIr::OptionNone, Some(IrType::Option(inner))) => {
                IrType::Option(inner.clone())
            }
            (BuiltinVariantIr::OptionNone, _) => {
                self.analyzer.push_source_error(
                    ErrorCode::NX2210,
                    &self.module.source,
                    byte_range(range),
                    "`None` requires an expected Option<T> type",
                    "constructor type cannot be inferred here",
                );
                IrType::Option(Box::new(IrType::Unit))
            }
            _ => return None,
        };
        Some(TypedExpressionIr {
            ty,
            effect: IrEffect::Immediate,
            span,
            kind: TypedExpressionKind::BuiltinVariant {
                variant,
                payload: None,
            },
        })
    }

    fn check_field_access(
        &mut self,
        receiver: TypedExpressionIr,
        member: &ast::Identifier,
        whole_range: TextRange,
    ) -> Option<TypedExpressionIr> {
        let IrType::Named(owner) = receiver.ty else {
            self.type_error(
                source_range(&self.module.source, whole_range),
                "field access requires a named type",
            );
            return None;
        };
        if self.is_stateful_definition(owner) {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(whole_range),
                "@stateful fields cannot be accessed directly",
                "use old.field/new.set during migration and StateHandle APIs at runtime",
            );
            return None;
        }
        let Some(field) = self
            .analyzer
            .members
            .get(&(owner, member.text.clone()))
            .copied()
        else {
            self.analyzer.push_source_error(
                ErrorCode::NX2501,
                &self.module.source,
                byte_range(whole_range),
                format!("unknown field `{}`", member.text),
                "field is not present on this type",
            );
            return None;
        };
        if self.analyzer.definitions[field.0 as usize].kind != DefinitionKind::Field {
            self.type_error(
                source_range(&self.module.source, member.range),
                "member is not a field",
            );
            return None;
        }
        self.analyzer
            .record_reference(&self.module, member.range, field);
        Some(TypedExpressionIr {
            ty: self.analyzer.definitions[field.0 as usize].ty.clone(),
            effect: receiver.effect,
            span: source_range(&self.module.source, whole_range),
            kind: TypedExpressionKind::Field {
                base: Box::new(receiver),
                field,
            },
        })
    }

    fn is_stateful_definition(&self, definition: DefinitionId) -> bool {
        self.analyzer
            .type_metadata
            .get(&definition)
            .is_some_and(|metadata| metadata.stateful)
    }

    fn check_receiver_method_path<'path>(
        &mut self,
        path: &'path ast::QualifiedName,
    ) -> Option<(TypedExpressionIr, &'path ast::Identifier)> {
        let (method, receiver_segments) = path.segments.split_last()?;
        let receiver_path = ast::QualifiedName {
            segments: receiver_segments.to_vec(),
            range: TextRange::new(
                receiver_segments.first()?.range.start,
                receiver_segments.last()?.range.end,
            ),
        };
        let (mut receiver, consumed) = self.check_qualified_receiver_base(&receiver_path)?;
        for member in receiver_segments.iter().skip(consumed) {
            receiver = self.check_field_access(receiver, member, path.range)?;
        }
        receiver.span = source_range(&self.module.source, path.range);
        Some((receiver, method))
    }

    fn check_builtin_constructor_call(
        &mut self,
        whole: &Expression,
        path: &ast::QualifiedName,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
    ) -> Option<TypedExpressionIr> {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>();
        let (operation, arity) = match segments.as_slice() {
            ["Array", "new"] => (BuiltinOperationIr::ArrayNew, 1),
            ["Map", "new"] => (BuiltinOperationIr::MapNew, 2),
            _ => return None,
        };
        let span = source_range(&self.module.source, whole.range);
        if type_arguments.len() != arity {
            self.type_error(
                span.clone(),
                &format!(
                    "`{}` expects {arity} type arguments, found {}",
                    path.text(),
                    type_arguments.len()
                ),
            );
        }
        if !arguments.is_empty() {
            self.type_error(
                span.clone(),
                &format!(
                    "`{}` expects no value arguments, found {}",
                    path.text(),
                    arguments.len()
                ),
            );
        }
        let mut resolved_type_arguments = type_arguments
            .iter()
            .take(arity)
            .map(|argument| self.analyzer.resolve_type_ref(&self.module, argument))
            .collect::<Vec<_>>();
        resolved_type_arguments.resize(arity, IrType::Unit);
        let result = match operation {
            BuiltinOperationIr::ArrayNew => {
                IrType::Array(Box::new(resolved_type_arguments[0].clone()))
            }
            BuiltinOperationIr::MapNew => IrType::Map(
                Box::new(resolved_type_arguments[0].clone()),
                Box::new(resolved_type_arguments[1].clone()),
            ),
            _ => unreachable!("constructor operation is fixed above"),
        };
        let arguments = arguments
            .iter()
            .map(|argument| self.check_expression(argument, None))
            .collect::<Vec<_>>();
        Some(TypedExpressionIr {
            ty: result,
            effect: expression_effect(&arguments),
            span,
            kind: TypedExpressionKind::BuiltinCall {
                operation,
                type_arguments: resolved_type_arguments,
                arguments,
            },
        })
    }

    #[allow(clippy::too_many_lines)]
    fn check_builtin_method_call(
        &mut self,
        whole: &Expression,
        receiver: TypedExpressionIr,
        member: &ast::Identifier,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
    ) -> Option<TypedExpressionIr> {
        let receiver_ty = receiver.ty.clone();
        let method = member.text.as_str();
        let stable_id = || {
            IrType::Named(
                *self
                    .analyzer
                    .builtin_types
                    .get("StableId")
                    .expect("compiler StableId type is registered"),
            )
        };
        let state_handle_error = || {
            IrType::Named(
                *self
                    .analyzer
                    .builtin_types
                    .get("StateHandleError")
                    .expect("compiler StateHandleError type is registered"),
            )
        };
        let (operation, result, operation_type_arguments, expected_arguments, persistent) =
            match receiver_ty {
                IrType::String => match method {
                    "len" | "rune_count" => (
                        BuiltinOperationIr::StringLen,
                        IrType::I32,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    "byte_len" => (
                        BuiltinOperationIr::StringByteLen,
                        IrType::I32,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    "equals" => (
                        BuiltinOperationIr::StringEqual,
                        IrType::Bool,
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "concat" => (
                        BuiltinOperationIr::StringConcat,
                        IrType::String,
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "rune_at" => (
                        BuiltinOperationIr::StringRuneAt,
                        IrType::Rune,
                        Vec::new(),
                        vec![IrType::I32],
                        false,
                    ),
                    "hash" => (
                        BuiltinOperationIr::StringHash,
                        IrType::I64,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::Array(element) => {
                    let element = element.as_ref().clone();
                    let operation_type_arguments = vec![element.clone()];
                    match method {
                        "len" => (
                            BuiltinOperationIr::ArrayLen,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "get" => (
                            BuiltinOperationIr::ArrayGet,
                            element.clone(),
                            operation_type_arguments,
                            vec![IrType::I32],
                            false,
                        ),
                        "set" => (
                            BuiltinOperationIr::ArraySet,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![IrType::I32, element.clone()],
                            false,
                        ),
                        "push" => (
                            BuiltinOperationIr::ArrayPush,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![element.clone()],
                            false,
                        ),
                        "pop" => (
                            BuiltinOperationIr::ArrayPop,
                            element.clone(),
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "insert" => (
                            BuiltinOperationIr::ArrayInsert,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![IrType::I32, element.clone()],
                            false,
                        ),
                        "remove" => (
                            BuiltinOperationIr::ArrayRemove,
                            element,
                            operation_type_arguments,
                            vec![IrType::I32],
                            false,
                        ),
                        "clear" => (
                            BuiltinOperationIr::ArrayClear,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        _ => return None,
                    }
                }
                IrType::Map(key, value) => {
                    let key = key.as_ref().clone();
                    let value = value.as_ref().clone();
                    let operation_type_arguments = vec![key.clone(), value.clone()];
                    match method {
                        "len" => (
                            BuiltinOperationIr::MapLen,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "get" => (
                            BuiltinOperationIr::MapGet,
                            IrType::Option(Box::new(value.clone())),
                            operation_type_arguments,
                            vec![key.clone()],
                            false,
                        ),
                        "set" => (
                            BuiltinOperationIr::MapSet,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![key.clone(), value.clone()],
                            false,
                        ),
                        "remove" => (
                            BuiltinOperationIr::MapRemove,
                            IrType::Option(Box::new(value)),
                            operation_type_arguments,
                            vec![key.clone()],
                            false,
                        ),
                        "contains" => (
                            BuiltinOperationIr::MapContains,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![key],
                            false,
                        ),
                        "clear" => (
                            BuiltinOperationIr::MapClear,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        _ => return None,
                    }
                }
                IrType::Buffer(element) => {
                    let element = element.as_ref().clone();
                    let operation_type_arguments = vec![element.clone()];
                    let buffer = IrType::Buffer(Box::new(element.clone()));
                    match method {
                        "len" => (
                            BuiltinOperationIr::BufferLen,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "get" => (
                            BuiltinOperationIr::BufferGet,
                            element.clone(),
                            operation_type_arguments,
                            vec![IrType::I32],
                            false,
                        ),
                        "set" => (
                            BuiltinOperationIr::BufferSet,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![IrType::I32, element],
                            false,
                        ),
                        "slice" => (
                            BuiltinOperationIr::BufferSlice,
                            buffer.clone(),
                            operation_type_arguments,
                            vec![IrType::I32, IrType::I32],
                            false,
                        ),
                        "copy" => (
                            BuiltinOperationIr::BufferCopy,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![buffer, IrType::I32, IrType::I32, IrType::I32],
                            false,
                        ),
                        _ => return None,
                    }
                }
                IrType::StateHandle(target) => {
                    let target = target.as_ref().clone();
                    let operation_type_arguments = vec![target.clone()];
                    let handle = IrType::StateHandle(Box::new(target.clone()));
                    match method {
                        "resolve" => (
                            BuiltinOperationIr::StateHandleResolve,
                            IrType::Result(Box::new(target), Box::new(state_handle_error())),
                            operation_type_arguments,
                            Vec::new(),
                            true,
                        ),
                        "is_alive" => (
                            BuiltinOperationIr::StateHandleIsAlive,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            true,
                        ),
                        "stable_id" => (
                            BuiltinOperationIr::StateHandleStableId,
                            stable_id(),
                            operation_type_arguments,
                            Vec::new(),
                            true,
                        ),
                        "generation" => (
                            BuiltinOperationIr::StateHandleGeneration,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            true,
                        ),
                        "equality" => (
                            BuiltinOperationIr::StateHandleEqual,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![handle],
                            true,
                        ),
                        "hash" => (
                            BuiltinOperationIr::StateHandleHash,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            true,
                        ),
                        _ => return None,
                    }
                }
                _ => return None,
            };
        let span = source_range(&self.module.source, whole.range);
        if !type_arguments.is_empty() {
            self.type_error(
                span.clone(),
                &format!("builtin method `{method}` does not accept explicit type arguments"),
            );
        }
        if arguments.len() != expected_arguments.len() {
            self.type_error(
                span.clone(),
                &format!(
                    "builtin method `{method}` expects {} arguments, found {}",
                    expected_arguments.len(),
                    arguments.len()
                ),
            );
        }
        let mut checked_arguments = Vec::with_capacity(arguments.len() + 1);
        checked_arguments.push(receiver);
        for (index, argument) in arguments.iter().enumerate() {
            let expected = expected_arguments.get(index);
            let value = self.check_expression(argument, expected);
            if let Some(expected) = expected {
                self.expect_type(&value.ty, expected, &value.span);
            }
            checked_arguments.push(value);
        }
        if persistent {
            self.mark_restricted(RestrictedOperation::PersistentState);
        }
        Some(TypedExpressionIr {
            ty: result,
            effect: expression_effect(&checked_arguments),
            span,
            kind: TypedExpressionKind::BuiltinCall {
                operation,
                type_arguments: operation_type_arguments,
                arguments: checked_arguments,
            },
        })
    }

    #[allow(clippy::too_many_lines)]
    fn infer_standard_argument_constraints(
        &self,
        surface: &SurfaceType,
        expression: &Expression,
        bindings: &mut BTreeMap<String, IrType>,
        allow_numeric_literals: bool,
    ) {
        match surface {
            SurfaceType::TypeParameter(name) => {
                if bindings.contains_key(name) {
                    return;
                }
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    bindings.insert(name.clone(), actual);
                }
            }
            SurfaceType::Option(inner) => {
                if is_none_expression(expression) {
                    return;
                }
                if let Some(("Some", explicit, arguments)) = builtin_variant_call(expression) {
                    if let [explicit] = explicit.as_slice()
                        && let Some(actual) = self.infer_type_ref(explicit)
                    {
                        merge_surface_constraint(inner, &actual, bindings);
                    }
                    if let Some(payload) = arguments.first() {
                        self.infer_standard_argument_constraints(
                            inner,
                            payload,
                            bindings,
                            allow_numeric_literals,
                        );
                    }
                    return;
                }
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    merge_surface_constraint(surface, &actual, bindings);
                }
            }
            SurfaceType::Result(success, error) => {
                if let Some((variant @ ("Ok" | "Err"), explicit, arguments)) =
                    builtin_variant_call(expression)
                {
                    if let [explicit_success, explicit_error] = explicit.as_slice()
                        && let (Some(actual_success), Some(actual_error)) = (
                            self.infer_type_ref(explicit_success),
                            self.infer_type_ref(explicit_error),
                        )
                    {
                        merge_surface_constraint(success, &actual_success, bindings);
                        merge_surface_constraint(error, &actual_error, bindings);
                    }
                    if let Some(payload) = arguments.first() {
                        self.infer_standard_argument_constraints(
                            if variant == "Ok" { success } else { error },
                            payload,
                            bindings,
                            allow_numeric_literals,
                        );
                    }
                    return;
                }
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    merge_surface_constraint(surface, &actual, bindings);
                }
            }
            SurfaceType::Array(inner) => {
                if let ExpressionKind::Array(values) = &expression.kind {
                    for value in values {
                        self.infer_standard_argument_constraints(
                            inner,
                            value,
                            bindings,
                            allow_numeric_literals,
                        );
                    }
                    return;
                }
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    merge_surface_constraint(surface, &actual, bindings);
                }
            }
            SurfaceType::Tuple(items) => {
                if let ExpressionKind::Tuple(values) = &expression.kind
                    && items.len() == values.len()
                {
                    for (item, value) in items.iter().zip(values) {
                        self.infer_standard_argument_constraints(
                            item,
                            value,
                            bindings,
                            allow_numeric_literals,
                        );
                    }
                    return;
                }
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    merge_surface_constraint(surface, &actual, bindings);
                }
            }
            SurfaceType::Unit
            | SurfaceType::Bool
            | SurfaceType::I32
            | SurfaceType::I64
            | SurfaceType::F32
            | SurfaceType::F64
            | SurfaceType::String
            | SurfaceType::Rune
            | SurfaceType::Named { .. }
            | SurfaceType::Map(_, _)
            | SurfaceType::HostRequest(_)
            | SurfaceType::ResourceToken(_)
            | SurfaceType::Snapshot(_)
            | SurfaceType::Buffer(_)
            | SurfaceType::StateHandle(_) => {
                if let Some(actual) = self.infer_expression_type(expression, allow_numeric_literals)
                {
                    merge_surface_constraint(surface, &actual, bindings);
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn infer_expression_type(
        &self,
        expression: &Expression,
        allow_numeric_literals: bool,
    ) -> Option<IrType> {
        match &expression.kind {
            ExpressionKind::Literal(literal) => {
                if !allow_numeric_literals
                    && matches!(literal.kind, LiteralKind::Integer | LiteralKind::Float)
                {
                    return None;
                }
                typed_literal(literal, None).ok().map(|(ty, _)| ty)
            }
            ExpressionKind::Name(path) => {
                if is_none_expression(expression) {
                    return None;
                }
                let definition = if path.segments.len() == 1 {
                    self.local(&path.segments[0].text)
                        .or_else(|| self.analyzer.lookup_symbol_path(&self.module, path))
                } else {
                    self.analyzer.lookup_symbol_path(&self.module, path)
                }?;
                let definition = &self.analyzer.definitions[definition.0 as usize];
                matches!(
                    definition.kind,
                    DefinitionKind::Parameter
                        | DefinitionKind::Local
                        | DefinitionKind::Const
                        | DefinitionKind::Variant
                )
                .then(|| definition.ty.clone())
            }
            ExpressionKind::Tuple(values) => values
                .iter()
                .map(|value| self.infer_expression_type(value, allow_numeric_literals))
                .collect::<Option<Vec<_>>>()
                .map(IrType::Tuple),
            ExpressionKind::Array(values) => {
                let mut types = values
                    .iter()
                    .map(|value| self.infer_expression_type(value, allow_numeric_literals));
                let element = types.next()??;
                types
                    .all(|ty| ty.as_ref() == Some(&element))
                    .then(|| IrType::Array(Box::new(element)))
            }
            ExpressionKind::Unary { operator, operand } => {
                let operand = self.infer_expression_type(operand, allow_numeric_literals)?;
                match operator.kind {
                    UnaryOperatorKind::Positive => Some(operand),
                    UnaryOperatorKind::Negate if is_numeric(&operand) => Some(operand),
                    UnaryOperatorKind::Not if operand == IrType::Bool => Some(IrType::Bool),
                    UnaryOperatorKind::Negate | UnaryOperatorKind::Not => None,
                }
            }
            ExpressionKind::Binary { left, operator, .. } => {
                let left = self.infer_expression_type(left, allow_numeric_literals)?;
                binary_result(operator.kind, &left, &self.analyzer.definitions)
            }
            ExpressionKind::Call {
                callee,
                type_arguments,
                arguments,
            } => {
                if let ExpressionKind::Name(path) = &callee.kind
                    && let [name] = path.segments.as_slice()
                {
                    match name.text.as_str() {
                        "Some" => {
                            let inner = type_arguments
                                .first()
                                .and_then(|ty| self.infer_type_ref(ty))
                                .or_else(|| {
                                    arguments.first().and_then(|value| {
                                        self.infer_expression_type(value, allow_numeric_literals)
                                    })
                                })?;
                            return Some(IrType::Option(Box::new(inner)));
                        }
                        "Ok" | "Err" if type_arguments.len() == 2 => {
                            return Some(IrType::Result(
                                Box::new(self.infer_type_ref(&type_arguments[0])?),
                                Box::new(self.infer_type_ref(&type_arguments[1])?),
                            ));
                        }
                        "Ok" | "Err" => return None,
                        _ => {}
                    }
                }
                let ExpressionKind::Name(path) = &callee.kind else {
                    return None;
                };
                let definition = self.analyzer.lookup_symbol_path(&self.module, path)?;
                if let Some(signature) = self.analyzer.function_signatures.get(&definition) {
                    return type_arguments.is_empty().then(|| signature.result.clone());
                }
                let signature = self.analyzer.external_functions.get(&definition)?;
                let Some((surface_parameters, surface_result)) = &signature.generic else {
                    return type_arguments.is_empty().then(|| signature.result.clone());
                };
                if arguments.len() != surface_parameters.len()
                    || (!type_arguments.is_empty()
                        && type_arguments.len() != signature.type_parameters.len())
                {
                    return None;
                }
                let mut bindings = BTreeMap::new();
                if !type_arguments.is_empty() {
                    for (parameter, argument) in
                        signature.type_parameters.iter().zip(type_arguments)
                    {
                        bindings.insert(parameter.clone(), self.infer_type_ref(argument)?);
                    }
                }
                for allow_numeric_literals in [false, true] {
                    for (surface, argument) in surface_parameters.iter().zip(arguments) {
                        self.infer_standard_argument_constraints(
                            surface,
                            argument,
                            &mut bindings,
                            allow_numeric_literals,
                        );
                    }
                }
                signature
                    .type_parameters
                    .iter()
                    .all(|parameter| bindings.contains_key(parameter))
                    .then(|| instantiate_inferred_surface_type(surface_result, &bindings))
                    .flatten()
            }
            ExpressionKind::Member { receiver, member } => {
                let IrType::Named(owner) =
                    self.infer_expression_type(receiver, allow_numeric_literals)?
                else {
                    return None;
                };
                let field = self.analyzer.members.get(&(owner, member.text.clone()))?;
                Some(self.analyzer.definitions[field.0 as usize].ty.clone())
            }
            ExpressionKind::Index { receiver, .. } => {
                match self.infer_expression_type(receiver, allow_numeric_literals)? {
                    IrType::Array(inner) | IrType::Buffer(inner) => Some(*inner),
                    IrType::Map(_, value) => Some(*value),
                    IrType::String => Some(IrType::Rune),
                    IrType::Unit
                    | IrType::Bool
                    | IrType::I32
                    | IrType::I64
                    | IrType::F32
                    | IrType::F64
                    | IrType::Rune
                    | IrType::Named(_)
                    | IrType::Option(_)
                    | IrType::Result(_, _)
                    | IrType::Tuple(_)
                    | IrType::HostRequest(_)
                    | IrType::ResourceToken(_)
                    | IrType::Snapshot(_)
                    | IrType::StateHandle(_)
                    | IrType::TypeParameter(_) => None,
                }
            }
            ExpressionKind::Construct { ty, .. } => self
                .analyzer
                .lookup_symbol_path(&self.module, ty)
                .map(IrType::Named),
            ExpressionKind::New { ty, .. } => self.infer_type_ref(ty),
            ExpressionKind::With { value, .. } | ExpressionKind::Await(value) => {
                self.infer_expression_type(value, allow_numeric_literals)
            }
            ExpressionKind::Try(value) => {
                match self.infer_expression_type(value, allow_numeric_literals)? {
                    IrType::Result(success, _) => Some(*success),
                    IrType::Unit
                    | IrType::Bool
                    | IrType::I32
                    | IrType::I64
                    | IrType::F32
                    | IrType::F64
                    | IrType::String
                    | IrType::Rune
                    | IrType::Named(_)
                    | IrType::Option(_)
                    | IrType::Array(_)
                    | IrType::Map(_, _)
                    | IrType::Tuple(_)
                    | IrType::HostRequest(_)
                    | IrType::ResourceToken(_)
                    | IrType::Snapshot(_)
                    | IrType::Buffer(_)
                    | IrType::StateHandle(_)
                    | IrType::TypeParameter(_) => None,
                }
            }
            ExpressionKind::Match { arms, .. } => {
                let mut types = arms
                    .iter()
                    .map(|arm| self.infer_expression_type(&arm.value, allow_numeric_literals));
                let first = types.next()??;
                types.all(|ty| ty.as_ref() == Some(&first)).then_some(first)
            }
            ExpressionKind::Interpolation(_) => Some(IrType::String),
            ExpressionKind::Error => None,
        }
    }

    fn infer_type_ref(&self, ty: &TypeRef) -> Option<IrType> {
        match &ty.kind {
            TypeKind::Named(path) => builtin_type(&path.text()).or_else(|| {
                self.analyzer
                    .lookup_symbol_path(&self.module, path)
                    .filter(|definition| {
                        matches!(
                            self.analyzer.definitions[definition.0 as usize].kind,
                            DefinitionKind::Struct
                                | DefinitionKind::Enum
                                | DefinitionKind::Class
                                | DefinitionKind::Stateful
                                | DefinitionKind::HostInterface
                                | DefinitionKind::StandardLibrary
                        )
                    })
                    .map(IrType::Named)
            }),
            TypeKind::Generic { base, arguments } => {
                let arguments = arguments
                    .iter()
                    .map(|argument| self.infer_type_ref(argument))
                    .collect::<Option<Vec<_>>>()?;
                match (base.text().as_str(), arguments.as_slice()) {
                    ("Option", [inner]) => Some(IrType::Option(Box::new(inner.clone()))),
                    ("Result", [success, error]) => Some(IrType::Result(
                        Box::new(success.clone()),
                        Box::new(error.clone()),
                    )),
                    ("Array", [inner]) => Some(IrType::Array(Box::new(inner.clone()))),
                    ("Map", [key, value]) => {
                        Some(IrType::Map(Box::new(key.clone()), Box::new(value.clone())))
                    }
                    ("HostRequest", [inner]) => {
                        Some(IrType::HostRequest(Some(Box::new(inner.clone()))))
                    }
                    ("ResourceToken", [inner]) => {
                        Some(IrType::ResourceToken(Some(Box::new(inner.clone()))))
                    }
                    ("Snapshot", [inner]) => Some(IrType::Snapshot(Box::new(inner.clone()))),
                    ("Buffer", [inner]) => Some(IrType::Buffer(Box::new(inner.clone()))),
                    ("StateHandle", [inner]) => Some(IrType::StateHandle(Box::new(inner.clone()))),
                    _ => None,
                }
            }
            TypeKind::Tuple(values) => values
                .iter()
                .map(|value| self.infer_type_ref(value))
                .collect::<Option<Vec<_>>>()
                .map(IrType::Tuple),
            TypeKind::Array(inner) => Some(IrType::Array(Box::new(self.infer_type_ref(inner)?))),
            TypeKind::Map { key, value } => Some(IrType::Map(
                Box::new(self.infer_type_ref(key)?),
                Box::new(self.infer_type_ref(value)?),
            )),
            TypeKind::Option(inner) => Some(IrType::Option(Box::new(self.infer_type_ref(inner)?))),
            TypeKind::Result { ok, error } => Some(IrType::Result(
                Box::new(self.infer_type_ref(ok)?),
                Box::new(self.infer_type_ref(error)?),
            )),
            TypeKind::Error => None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn check_call(
        &mut self,
        whole: &Expression,
        callee: &Expression,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
        expected: Option<&IrType>,
        awaited: bool,
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        if let ExpressionKind::Name(path) = &callee.kind
            && let Some(intrinsic) =
                self.check_migration_intrinsic(whole, path, type_arguments, arguments)
        {
            return intrinsic;
        }
        if let ExpressionKind::Member { receiver, member } = &callee.kind {
            let receiver = self.check_expression(receiver, None);
            return self
                .check_builtin_method_call(whole, receiver, member, type_arguments, arguments)
                .unwrap_or_else(|| {
                    self.type_error(
                        span.clone(),
                        &format!("unknown method `{}` for receiver type", member.text),
                    );
                    self.error_expression(span.clone())
                });
        }
        let ExpressionKind::Name(path) = &callee.kind else {
            self.type_error(
                span.clone(),
                "callee must resolve to a named function or method",
            );
            return self.error_expression(span);
        };
        if let Some(expression) =
            self.check_builtin_constructor_call(whole, path, type_arguments, arguments)
        {
            return expression;
        }
        if let Some((receiver, method)) = self.check_receiver_method_path(path) {
            return self
                .check_builtin_method_call(whole, receiver, method, type_arguments, arguments)
                .unwrap_or_else(|| {
                    self.type_error(
                        span.clone(),
                        &format!("unknown method `{}` for receiver type", method.text),
                    );
                    self.error_expression(span.clone())
                });
        }
        if let Some(expression) =
            self.check_builtin_variant_call(whole, path, type_arguments, arguments, expected)
        {
            return expression;
        }
        if let Some(definition) = self.analyzer.lookup_symbol_path(&self.module, path)
            && self.analyzer.definitions[definition.0 as usize].kind == DefinitionKind::Variant
        {
            let Some(definition) =
                self.analyzer
                    .resolve_symbol_path(&self.module, path, SymbolUse::Value)
            else {
                return self.error_expression(span);
            };
            return self.check_enum_variant_call(whole, definition, arguments);
        }
        let Some(definition) =
            self.analyzer
                .resolve_symbol_path(&self.module, path, SymbolUse::Callable)
        else {
            return self.error_expression(span);
        };
        let signature = self
            .analyzer
            .function_signatures
            .get(&definition)
            .map(|signature| {
                (
                    signature.parameter_types.clone(),
                    signature.result.clone(),
                    signature.effect,
                    None,
                    None,
                    None,
                    Vec::new(),
                )
            })
            .or_else(|| {
                self.analyzer
                    .external_functions
                    .get(&definition)
                    .map(|signature| {
                        (
                            signature.parameters.clone(),
                            signature.result.clone(),
                            signature.effect,
                            signature.host.clone(),
                            signature.generic.clone(),
                            signature.intrinsic,
                            signature.type_parameters.clone(),
                        )
                    })
            });
        let Some((mut parameters, mut result, effect, host, generic, intrinsic, type_parameters)) =
            signature
        else {
            self.type_error(span.clone(), "symbol has no callable signature");
            return self.error_expression(span);
        };
        let explicit_type_arguments = type_arguments
            .iter()
            .map(|argument| self.analyzer.resolve_type_ref(&self.module, argument))
            .collect::<Vec<_>>();
        let mut generic_arguments = None;
        let mut call_type_arguments = Vec::new();
        if let Some((surface_parameters, surface_result)) = generic {
            let mut bindings = BTreeMap::new();
            if !explicit_type_arguments.is_empty()
                && explicit_type_arguments.len() != type_parameters.len()
            {
                self.type_error(
                    span.clone(),
                    &format!(
                        "expected {} standard-library type arguments, found {}",
                        type_parameters.len(),
                        explicit_type_arguments.len()
                    ),
                );
            }
            if explicit_type_arguments.len() == type_parameters.len() {
                for (parameter, argument) in type_parameters.iter().zip(&explicit_type_arguments) {
                    bindings.insert(parameter.clone(), argument.clone());
                }
            }
            if let Some(expected) = expected {
                let mut inferred = bindings.clone();
                if unify_surface_type(&surface_result, expected, &mut inferred) {
                    bindings = inferred;
                }
            }
            for allow_numeric_literals in [false, true] {
                for (surface, argument) in surface_parameters.iter().zip(arguments) {
                    self.infer_standard_argument_constraints(
                        surface,
                        argument,
                        &mut bindings,
                        allow_numeric_literals,
                    );
                }
            }
            for parameter in &type_parameters {
                if !bindings.contains_key(parameter) {
                    self.type_error(
                        span.clone(),
                        &format!("cannot infer standard-library type parameter `{parameter}`"),
                    );
                    bindings.insert(parameter.clone(), IrType::Unit);
                }
            }
            parameters = surface_parameters
                .iter()
                .map(|surface| self.analyzer.instantiate_surface_type(surface, &bindings))
                .collect();
            result = self
                .analyzer
                .instantiate_surface_type(&surface_result, &bindings);
            call_type_arguments = type_parameters
                .iter()
                .map(|parameter| {
                    bindings
                        .get(parameter)
                        .cloned()
                        .expect("every generic parameter has an inferred or recovery binding")
                })
                .collect();
            let values = arguments
                .iter()
                .enumerate()
                .map(|(index, argument)| {
                    let expected = parameters.get(index);
                    let value = self.check_expression(argument, expected);
                    if let Some(expected) = expected {
                        self.expect_type(&value.ty, expected, &value.span);
                    }
                    value
                })
                .collect();
            generic_arguments = Some(values);
        } else if !explicit_type_arguments.is_empty() {
            self.type_error(
                span.clone(),
                "non-generic function does not accept explicit type arguments",
            );
        }
        if parameters.len() != arguments.len() {
            self.type_error(
                span.clone(),
                &format!(
                    "expected {} arguments, found {}",
                    parameters.len(),
                    arguments.len()
                ),
            );
        }
        let arguments = generic_arguments.unwrap_or_else(|| {
            arguments
                .iter()
                .enumerate()
                .map(|(index, argument)| {
                    let expected = parameters.get(index);
                    let value = self.check_expression(argument, expected);
                    if let Some(expected) = expected {
                        self.expect_type(&value.ty, expected, &value.span);
                    }
                    value
                })
                .collect::<Vec<_>>()
        });
        if effect == IrEffect::Task && !awaited {
            self.analyzer.push_source_error(
                ErrorCode::NX2302,
                &self.module.source,
                byte_range(whole.range),
                "Task/Request call must be awaited",
                "prefix the call with `await` inside a Task",
            );
        }
        if let Some(caller) = self.current_function {
            self.analyzer
                .call_edges
                .entry(caller)
                .or_default()
                .insert(definition);
        }
        if let Some((interface, host_function)) = host {
            self.mark_restricted(RestrictedOperation::Host);
            if let Some(capability) = &host_function.required_capability {
                let available = self
                    .analyzer
                    .input
                    .root_manifest
                    .application
                    .as_ref()
                    .is_some_and(|settings| settings.capabilities.contains(capability));
                if !available {
                    self.analyzer.push_source_error(
                        ErrorCode::NX4002,
                        &self.module.source,
                        byte_range(whole.range),
                        format!("Host function requires capability `{capability}`"),
                        "capability is not declared by the root Application",
                    );
                }
            }
            TypedExpressionIr {
                ty: result,
                effect,
                span,
                kind: TypedExpressionKind::HostCall {
                    interface,
                    function: definition,
                    arguments,
                },
            }
        } else if let Some(intrinsic) = intrinsic {
            TypedExpressionIr {
                ty: result,
                effect,
                span,
                kind: TypedExpressionKind::StandardCall {
                    function: definition,
                    intrinsic,
                    type_arguments: call_type_arguments,
                    arguments,
                },
            }
        } else {
            TypedExpressionIr {
                ty: result,
                effect,
                span,
                kind: TypedExpressionKind::Call {
                    callee: definition,
                    arguments,
                },
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn check_migration_intrinsic(
        &mut self,
        whole: &Expression,
        path: &ast::QualifiedName,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
    ) -> Option<TypedExpressionIr> {
        let name = path.text();
        if !matches!(
            name.as_str(),
            "old.get"
                | "old.field"
                | "new.create"
                | "new.set"
                | "replace"
                | "preserve"
                | "delete"
                | "finish_migration"
        ) {
            return None;
        }
        let span = source_range(&self.module.source, whole.range);
        self.mark_restricted(RestrictedOperation::Migration);
        if self.effect != IrEffect::Migration {
            self.analyzer.push_source_error(
                ErrorCode::NX2601,
                &self.module.source,
                byte_range(whole.range),
                format!("migration intrinsic `{name}` is only valid in a Migration"),
                "move the operation into the root migration function",
            );
        }

        let exact_counts = |types: usize, values: usize| {
            type_arguments.len() == types && arguments.len() == values
        };
        let malformed = |this: &mut Self, expected: &str| {
            this.type_error(span.clone(), &format!("`{name}` requires {expected}"));
            this.error_expression(span.clone())
        };
        let expression = match name.as_str() {
            "old.get" => {
                if !exact_counts(1, 1) {
                    return Some(malformed(self, "one type argument and one state identity"));
                }
                let value_type = self
                    .analyzer
                    .resolve_type_ref(&self.module, &type_arguments[0]);
                let Some(identity) = self.migration_identity(&arguments[0]) else {
                    return Some(self.error_expression(span));
                };
                TypedExpressionIr {
                    ty: value_type.clone(),
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::OldGet {
                        identity,
                        value_type,
                    }),
                }
            }
            "old.field" => {
                if !exact_counts(1, 2) {
                    return Some(malformed(
                        self,
                        "one field-value type argument, an old object, and a Stateful field",
                    ));
                }
                let value_type = self
                    .analyzer
                    .resolve_type_ref(&self.module, &type_arguments[0]);
                let object = self.check_expression(&arguments[0], None);
                let Some(field) = self.migration_field(&arguments[1]) else {
                    return Some(self.error_expression(span));
                };
                let field_type = self.analyzer.definitions[field.0 as usize].ty.clone();
                self.expect_type(
                    &field_type,
                    &value_type,
                    &source_range(&self.module.source, arguments[1].range),
                );
                if let Some(owner) = self.migration_field_owner(field) {
                    self.expect_type(&object.ty, &IrType::Named(owner), &object.span);
                }
                TypedExpressionIr {
                    ty: value_type.clone(),
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::OldFieldGet {
                        object: Box::new(object),
                        field,
                        value_type,
                    }),
                }
            }
            "new.create" => {
                if !exact_counts(1, 1) {
                    return Some(malformed(
                        self,
                        "one Stateful type argument and one state identity",
                    ));
                }
                let ty = self
                    .analyzer
                    .resolve_type_ref(&self.module, &type_arguments[0]);
                let IrType::Named(state_type) = ty else {
                    self.type_error(span.clone(), "`new.create` requires a Stateful type");
                    return Some(self.error_expression(span));
                };
                if !self
                    .analyzer
                    .state_types
                    .iter()
                    .any(|state| state.definition == state_type)
                {
                    self.type_error(span.clone(), "`new.create` requires a Stateful type");
                }
                let Some(identity) = self.migration_identity(&arguments[0]) else {
                    return Some(self.error_expression(span));
                };
                TypedExpressionIr {
                    ty: IrType::Named(state_type),
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::NewCreate {
                        identity,
                        state_type,
                    }),
                }
            }
            "new.set" => {
                if !exact_counts(0, 3) {
                    return Some(malformed(
                        self,
                        "a new object, a Stateful field, and a field value",
                    ));
                }
                let object = self.check_expression(&arguments[0], None);
                let Some(field) = self.migration_field(&arguments[1]) else {
                    return Some(self.error_expression(span));
                };
                let field_type = self.analyzer.definitions[field.0 as usize].ty.clone();
                if let Some(owner) = self.migration_field_owner(field) {
                    self.expect_type(&object.ty, &IrType::Named(owner), &object.span);
                }
                let value = self.check_expression(&arguments[2], Some(&field_type));
                self.expect_type(&value.ty, &field_type, &value.span);
                TypedExpressionIr {
                    ty: IrType::Bool,
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::NewSet {
                        object: Box::new(object),
                        field,
                        value: Box::new(value),
                    }),
                }
            }
            "replace" => {
                if !exact_counts(0, 2) {
                    return Some(malformed(
                        self,
                        "a state identity and its replacement object",
                    ));
                }
                let Some(identity) = self.migration_identity(&arguments[0]) else {
                    return Some(self.error_expression(span));
                };
                let target = self.check_expression(&arguments[1], None);
                if !matches!(
                    target.ty,
                    IrType::Named(definition)
                        if self
                            .analyzer
                            .state_types
                            .iter()
                            .any(|state| state.definition == definition)
                ) {
                    self.type_error(
                        target.span.clone(),
                        "`replace` requires a newly created Stateful object",
                    );
                }
                TypedExpressionIr {
                    ty: IrType::Bool,
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::Replace {
                        identity,
                        target: Box::new(target),
                    }),
                }
            }
            "preserve" | "delete" => {
                if !exact_counts(0, 1) {
                    return Some(malformed(self, "one state identity"));
                }
                let Some(identity) = self.migration_identity(&arguments[0]) else {
                    return Some(self.error_expression(span));
                };
                let intrinsic = if name == "preserve" {
                    MigrationIntrinsicIr::Preserve { identity }
                } else {
                    MigrationIntrinsicIr::Delete { identity }
                };
                TypedExpressionIr {
                    ty: IrType::Bool,
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(intrinsic),
                }
            }
            "finish_migration" => {
                if !exact_counts(0, 0) {
                    return Some(malformed(self, "no arguments"));
                }
                TypedExpressionIr {
                    ty: IrType::Bool,
                    effect: IrEffect::Migration,
                    span,
                    kind: TypedExpressionKind::Migration(MigrationIntrinsicIr::Finish),
                }
            }
            _ => unreachable!("migration intrinsic names are exhaustively matched"),
        };
        Some(expression)
    }

    fn validate_migration_body(&mut self, body: &ast::Block, typed: &TypedBlockIr) {
        if self.effect != IrEffect::Migration {
            return;
        }
        let mut initial = MigrationPaths::new();
        initial.insert(MigrationPathState::default());
        let flow = self.analyze_migration_block(typed, initial);
        let mut exits = flow
            .normal
            .union(&flow.returns)
            .cloned()
            .collect::<MigrationPaths>();
        exits.extend(std::mem::take(&mut self.migration_expression_returns));
        if exits.iter().any(|path| path.finish_count == 0) {
            self.analyzer.push_source_error(
                ErrorCode::NX2602,
                &self.module.source,
                byte_range(body.range),
                "Migration must call `finish_migration()`",
                "migration body exits without finalizing the migration",
            );
        }
        let missing = exits
            .iter()
            .flat_map(|path| {
                path.reads.iter().filter(|identity| {
                    path.forwarding.get(identity).copied().unwrap_or_default() == 0
                        || path.unforwarded_at_finish.contains(identity)
                })
            })
            .copied()
            .collect::<BTreeSet<_>>();
        if !missing.is_empty() {
            let mut diagnostic = Diagnostic::new(
                ErrorCode::NX2603,
                Severity::Error,
                "Migration does not forward every old state identity on every reachable path",
            )
            .with_label(Label::primary(
                source_identity(&self.module.source),
                byte_range(body.range),
                "a successful migration path leaves old state unaccounted for",
            ));
            for identity in missing {
                diagnostic = diagnostic.with_note(format!("missing stable ID: {identity}"));
            }
            self.analyzer.diagnostics.push(diagnostic);
        }
    }

    fn analyze_migration_block(
        &mut self,
        block: &TypedBlockIr,
        input: MigrationPaths,
    ) -> MigrationFlow {
        let mut flow = MigrationFlow {
            normal: input,
            ..MigrationFlow::default()
        };
        for statement in &block.statements {
            if flow.normal.is_empty() {
                break;
            }
            let statement_flow =
                self.analyze_migration_statement(statement, std::mem::take(&mut flow.normal));
            flow.normal = statement_flow.normal;
            flow.returns.extend(statement_flow.returns);
            flow.breaks.extend(statement_flow.breaks);
            flow.continues.extend(statement_flow.continues);
            self.enforce_migration_path_limit(&mut flow.normal);
            self.enforce_migration_path_limit(&mut flow.returns);
            self.enforce_migration_path_limit(&mut flow.breaks);
            self.enforce_migration_path_limit(&mut flow.continues);
        }
        if let Some(tail) = &block.tail {
            self.apply_migration_expression(&mut flow.normal, tail);
        }
        flow
    }

    fn enforce_migration_path_limit(&mut self, paths: &mut MigrationPaths) {
        let limit = self
            .analyzer
            .input
            .compilation_options
            .limits
            .diagnostics_per_revision
            .saturating_mul(16)
            .clamp(64, 4_096);
        if paths.len() <= limit {
            return;
        }
        if !self.migration_path_limit_reported {
            self.migration_path_limit_reported = true;
            self.analyzer.push_source_error(
                ErrorCode::NX2602,
                &self.module.source,
                ByteRange::new(
                    0,
                    u32::try_from(self.module.syntax.source.as_str().len()).unwrap_or(u32::MAX),
                ),
                format!("Migration control-flow analysis exceeds its {limit}-path safety limit"),
                "reduce independent migration branches or split the migration into simpler decisions",
            );
        }
        while paths.len() > limit {
            paths.pop_last();
        }
    }

    fn analyze_migration_statement(
        &mut self,
        statement: &TypedStatementIr,
        mut paths: MigrationPaths,
    ) -> MigrationFlow {
        match statement {
            TypedStatementIr::Let { value, .. } => {
                if let Some(value) = value {
                    self.apply_migration_expression(&mut paths, value);
                }
                MigrationFlow {
                    normal: paths,
                    ..MigrationFlow::default()
                }
            }
            TypedStatementIr::Assign { target, value } => {
                self.apply_migration_place(&mut paths, target);
                self.apply_migration_expression(&mut paths, value);
                MigrationFlow {
                    normal: paths,
                    ..MigrationFlow::default()
                }
            }
            TypedStatementIr::Expression(expression) => {
                self.apply_migration_expression(&mut paths, expression);
                MigrationFlow {
                    normal: paths,
                    ..MigrationFlow::default()
                }
            }
            TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    self.apply_migration_expression(&mut paths, value);
                }
                MigrationFlow {
                    returns: paths,
                    ..MigrationFlow::default()
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.apply_migration_expression(&mut paths, condition);
                let constant = constant_bool_expression(condition, &self.analyzer.const_values);
                let mut flow = MigrationFlow::default();
                if constant != Some(false) {
                    flow.merge(self.analyze_migration_block(then_block, paths.clone()));
                }
                if constant != Some(true) {
                    flow.merge(if let Some(else_block) = else_block {
                        self.analyze_migration_block(else_block, paths)
                    } else {
                        MigrationFlow {
                            normal: paths,
                            ..MigrationFlow::default()
                        }
                    });
                }
                flow
            }
            TypedStatementIr::While {
                condition,
                body,
                max_iterations,
            } => self.analyze_migration_while(condition, body, *max_iterations, paths),
            TypedStatementIr::StaticRangeFor {
                binding,
                start,
                end,
                body,
                max_iterations,
            } => {
                self.apply_migration_expression(&mut paths, start);
                self.apply_migration_expression(&mut paths, end);
                let start = constant_i32_expression(start, &self.analyzer.const_values);
                self.analyze_migration_static_loop(*binding, start, body, *max_iterations, paths)
            }
            TypedStatementIr::Break => MigrationFlow {
                breaks: paths,
                ..MigrationFlow::default()
            },
            TypedStatementIr::Continue => MigrationFlow {
                continues: paths,
                ..MigrationFlow::default()
            },
            TypedStatementIr::Defer { captures, .. } => {
                self.apply_migration_expressions(&mut paths, captures);
                MigrationFlow {
                    normal: paths,
                    ..MigrationFlow::default()
                }
            }
            TypedStatementIr::Yield => MigrationFlow {
                normal: paths,
                ..MigrationFlow::default()
            },
        }
    }

    fn analyze_migration_while(
        &mut self,
        condition: &TypedExpressionIr,
        body: &TypedBlockIr,
        max_iterations: u32,
        input: MigrationPaths,
    ) -> MigrationFlow {
        let constant = constant_bool_expression(condition, &self.analyzer.const_values);
        let mut flow = MigrationFlow::default();
        let mut heads = input;
        let mut seen = MigrationPaths::new();
        for iteration in 0..=max_iterations {
            heads.retain(|path| seen.insert(path.clone()));
            if heads.is_empty() {
                break;
            }
            self.apply_migration_expression(&mut heads, condition);
            if constant != Some(true) {
                flow.normal.extend(heads.iter().cloned());
            }
            if constant == Some(false) || iteration == max_iterations {
                break;
            }
            let body_flow = self.analyze_migration_block(body, heads);
            flow.normal.extend(body_flow.breaks);
            flow.returns.extend(body_flow.returns);
            heads = body_flow.normal;
            heads.extend(body_flow.continues);
        }
        flow
    }

    fn analyze_migration_static_loop(
        &mut self,
        binding: DefinitionId,
        start: Option<i32>,
        body: &TypedBlockIr,
        max_iterations: u32,
        mut active: MigrationPaths,
    ) -> MigrationFlow {
        let mut flow = MigrationFlow::default();
        for iteration in 0..max_iterations {
            if active.is_empty() {
                break;
            }
            let iteration_value = start.and_then(|start| {
                i32::try_from(iteration)
                    .ok()
                    .and_then(|offset| start.checked_add(offset))
            });
            let previous = iteration_value.map(|value| {
                self.analyzer
                    .const_values
                    .insert(binding, ConstValue::I32(value))
            });
            let body_flow = self.analyze_migration_block(body, active);
            if let Some(previous) = previous {
                if let Some(previous) = previous {
                    self.analyzer.const_values.insert(binding, previous);
                } else {
                    self.analyzer.const_values.remove(&binding);
                }
            }
            flow.normal.extend(body_flow.breaks);
            flow.returns.extend(body_flow.returns);
            active = body_flow.normal;
            active.extend(body_flow.continues);
        }
        flow.normal.extend(active);
        flow
    }

    fn apply_migration_place(&mut self, paths: &mut MigrationPaths, place: &TypedPlaceIr) {
        match place {
            TypedPlaceIr::Definition(_) => {}
            TypedPlaceIr::Field { base, .. } | TypedPlaceIr::StateField { base, .. } => {
                self.apply_migration_expression(paths, base);
            }
            TypedPlaceIr::Index { base, index } => {
                self.apply_migration_expression(paths, base);
                self.apply_migration_expression(paths, index);
            }
        }
    }

    fn apply_migration_expressions(
        &mut self,
        paths: &mut MigrationPaths,
        expressions: &[TypedExpressionIr],
    ) {
        for expression in expressions {
            self.apply_migration_expression(paths, expression);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_migration_expression(
        &mut self,
        paths: &mut MigrationPaths,
        expression: &TypedExpressionIr,
    ) {
        if paths.is_empty() {
            return;
        }
        match &expression.kind {
            TypedExpressionKind::Literal(_)
            | TypedExpressionKind::Reference(_)
            | TypedExpressionKind::Yield => {}
            TypedExpressionKind::Unary { operand, .. }
            | TypedExpressionKind::Await(operand)
            | TypedExpressionKind::Field { base: operand, .. }
            | TypedExpressionKind::StateField { base: operand, .. } => {
                self.apply_migration_expression(paths, operand);
            }
            TypedExpressionKind::Try(operand) => {
                self.apply_migration_expression(paths, operand);
                self.migration_expression_returns
                    .extend(paths.iter().cloned());
                let mut expression_returns = std::mem::take(&mut self.migration_expression_returns);
                self.enforce_migration_path_limit(&mut expression_returns);
                self.migration_expression_returns = expression_returns;
            }
            TypedExpressionKind::Binary {
                operator,
                left,
                right,
            } => {
                self.apply_migration_expression(paths, left);
                let left_constant = constant_bool_expression(left, &self.analyzer.const_values);
                match operator {
                    BinaryOperator::And if left_constant == Some(false) => {}
                    BinaryOperator::Or if left_constant == Some(true) => {}
                    BinaryOperator::And | BinaryOperator::Or if left_constant.is_none() => {
                        let mut rhs_paths = paths.clone();
                        self.apply_migration_expression(&mut rhs_paths, right);
                        paths.extend(rhs_paths);
                        self.enforce_migration_path_limit(paths);
                    }
                    _ => self.apply_migration_expression(paths, right),
                }
            }
            TypedExpressionKind::Index {
                base: left,
                index: right,
            } => {
                self.apply_migration_expression(paths, left);
                self.apply_migration_expression(paths, right);
            }
            TypedExpressionKind::Call { arguments, .. }
            | TypedExpressionKind::StandardCall { arguments, .. }
            | TypedExpressionKind::BuiltinCall { arguments, .. }
            | TypedExpressionKind::HostCall { arguments, .. }
            | TypedExpressionKind::Array(arguments)
            | TypedExpressionKind::Tuple(arguments)
            | TypedExpressionKind::StringInterpolation(arguments) => {
                self.apply_migration_expressions(paths, arguments);
            }
            TypedExpressionKind::Construct { fields, .. } => {
                for (_, value) in fields {
                    self.apply_migration_expression(paths, value);
                }
            }
            TypedExpressionKind::EnumConstruct { payload, .. }
            | TypedExpressionKind::BuiltinVariant { payload, .. } => {
                if let Some(payload) = payload {
                    self.apply_migration_expression(paths, payload);
                }
            }
            TypedExpressionKind::Match { value, arms } => {
                self.apply_migration_expression(paths, value);
                let arm_input = std::mem::take(paths);
                for arm in arms {
                    let mut arm_paths = arm_input.clone();
                    self.apply_migration_expression(&mut arm_paths, &arm.value);
                    paths.extend(arm_paths);
                }
            }
            TypedExpressionKind::Update { base, fields } => {
                self.apply_migration_expression(paths, base);
                for (_, value) in fields {
                    self.apply_migration_expression(paths, value);
                }
            }
            TypedExpressionKind::Migration(intrinsic) => match intrinsic {
                MigrationIntrinsicIr::OldGet { identity, .. } => {
                    self.record_migration_operation(paths, &expression.span);
                    map_migration_paths(paths, |path| {
                        path.reads.insert(*identity);
                    });
                }
                MigrationIntrinsicIr::OldFieldGet { object, .. } => {
                    self.apply_migration_expression(paths, object);
                    self.record_migration_operation(paths, &expression.span);
                }
                MigrationIntrinsicIr::NewCreate { .. } => {
                    self.record_migration_operation(paths, &expression.span);
                }
                MigrationIntrinsicIr::NewSet { object, value, .. } => {
                    self.apply_migration_expression(paths, object);
                    self.apply_migration_expression(paths, value);
                    self.record_migration_operation(paths, &expression.span);
                }
                MigrationIntrinsicIr::Replace {
                    identity, target, ..
                } => {
                    self.apply_migration_expression(paths, target);
                    self.record_migration_operation(paths, &expression.span);
                    self.record_migration_forwarding(paths, *identity, &expression.span);
                }
                MigrationIntrinsicIr::Preserve { identity }
                | MigrationIntrinsicIr::Delete { identity } => {
                    self.record_migration_operation(paths, &expression.span);
                    self.record_migration_forwarding(paths, *identity, &expression.span);
                }
                MigrationIntrinsicIr::Finish => {
                    self.record_migration_finish(paths, &expression.span);
                }
            },
        }
    }

    fn record_migration_operation(&mut self, paths: &mut MigrationPaths, span: &SourceRange) {
        if paths.iter().any(|path| path.finish_count >= 1)
            && self
                .reported_finish_violations
                .insert((1, span.start, span.end))
        {
            self.analyzer.push_source_error(
                ErrorCode::NX2602,
                &self.module.source,
                range_from_source(span),
                "Migration performs an operation after `finish_migration()`",
                "finalization must be the last migration operation on every reachable path",
            );
        }
        record_migration_operation(paths, span);
    }

    fn record_migration_forwarding(
        &mut self,
        paths: &mut MigrationPaths,
        identity: StableId,
        span: &SourceRange,
    ) {
        if paths
            .iter()
            .any(|path| path.forwarding.get(&identity).copied().unwrap_or_default() >= 1)
            && self
                .reported_duplicate_forwarding
                .insert((identity, span.start, span.end))
        {
            self.analyzer.push_source_error(
                ErrorCode::NX2604,
                &self.module.source,
                range_from_source(span),
                format!("state identity {identity} is forwarded more than once"),
                "each reachable path must preserve, replace, or delete an old identity exactly once",
            );
        }
        record_migration_forwarding(paths, identity, span);
    }

    fn record_migration_finish(&mut self, paths: &mut MigrationPaths, span: &SourceRange) {
        if paths.iter().any(|path| path.finish_count >= 1)
            && self
                .reported_finish_violations
                .insert((2, span.start, span.end))
        {
            self.analyzer.push_source_error(
                ErrorCode::NX2602,
                &self.module.source,
                range_from_source(span),
                "Migration calls `finish_migration()` more than once on one reachable path",
                "every reachable successful path must finalize exactly once",
            );
        }
        record_migration_finish(paths, span);
    }

    fn migration_field_owner(&self, field: DefinitionId) -> Option<DefinitionId> {
        self.analyzer.state_types.iter().find_map(|state| {
            state
                .fields
                .iter()
                .any(|candidate| candidate.definition == field)
                .then_some(state.definition)
        })
    }

    fn migration_identity(&mut self, expression: &Expression) -> Option<StableId> {
        let ExpressionKind::Name(path) = &expression.kind else {
            self.type_error(
                source_range(&self.module.source, expression.range),
                "migration state identity must be one bare identifier",
            );
            return None;
        };
        let [identifier] = path.segments.as_slice() else {
            self.type_error(
                source_range(&self.module.source, expression.range),
                "migration state identity must be one bare identifier",
            );
            return None;
        };
        Some(StableId::from_name(&identifier.text))
    }

    fn migration_field(&mut self, expression: &Expression) -> Option<DefinitionId> {
        let ExpressionKind::Name(path) = &expression.kind else {
            self.type_error(
                source_range(&self.module.source, expression.range),
                "migration field must be a qualified Stateful field",
            );
            return None;
        };
        let field = self
            .analyzer
            .resolve_symbol_path(&self.module, path, SymbolUse::Value)?;
        if self.analyzer.definitions[field.0 as usize].kind != DefinitionKind::Field
            || !self.analyzer.state_types.iter().any(|state| {
                state
                    .fields
                    .iter()
                    .any(|candidate| candidate.definition == field)
            })
        {
            self.type_error(
                source_range(&self.module.source, expression.range),
                "migration field must belong to a Stateful type",
            );
            return None;
        }
        Some(field)
    }

    #[allow(clippy::too_many_lines)]
    fn check_builtin_variant_call(
        &mut self,
        whole: &Expression,
        path: &ast::QualifiedName,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
        expected: Option<&IrType>,
    ) -> Option<TypedExpressionIr> {
        let [name] = path.segments.as_slice() else {
            return None;
        };
        let variant = match name.text.as_str() {
            "Some" => BuiltinVariantIr::OptionSome,
            "Ok" => BuiltinVariantIr::ResultOk,
            "Err" => BuiltinVariantIr::ResultErr,
            _ => return None,
        };
        let span = source_range(&self.module.source, whole.range);
        if arguments.len() != 1 {
            self.type_error(
                span.clone(),
                &format!("`{}` expects exactly one payload", name.text),
            );
        }
        let explicit = type_arguments
            .iter()
            .map(|ty| self.analyzer.resolve_type_ref(&self.module, ty))
            .collect::<Vec<_>>();
        let (payload_expected, result_type) = match variant {
            BuiltinVariantIr::OptionSome => {
                if !matches!(explicit.len(), 0 | 1) {
                    self.type_error(
                        span.clone(),
                        "`Some` accepts at most one explicit type argument",
                    );
                }
                let inner = explicit.first().cloned().or_else(|| match expected {
                    Some(IrType::Option(inner)) => Some(inner.as_ref().clone()),
                    _ => None,
                });
                let payload = arguments
                    .first()
                    .map(|argument| self.check_expression(argument, inner.as_ref()));
                let inner = inner
                    .or_else(|| payload.as_ref().map(|value| value.ty.clone()))
                    .unwrap_or_else(|| {
                        self.type_error(span.clone(), "cannot infer `Some` payload type");
                        IrType::Unit
                    });
                return Some(TypedExpressionIr {
                    ty: IrType::Option(Box::new(inner)),
                    effect: payload
                        .as_ref()
                        .map_or(IrEffect::Immediate, |value| value.effect),
                    span,
                    kind: TypedExpressionKind::BuiltinVariant {
                        variant,
                        payload: payload.map(Box::new),
                    },
                });
            }
            BuiltinVariantIr::ResultOk | BuiltinVariantIr::ResultErr => {
                if !matches!(explicit.len(), 0 | 2) {
                    self.type_error(
                        span.clone(),
                        "`Ok` and `Err` require either zero or two explicit type arguments",
                    );
                }
                let expected_pair = if explicit.len() == 2 {
                    Some((explicit[0].clone(), explicit[1].clone()))
                } else if let Some(IrType::Result(ok, error)) = expected {
                    Some((ok.as_ref().clone(), error.as_ref().clone()))
                } else {
                    None
                };
                let payload_expected = expected_pair.as_ref().map(|(ok, error)| {
                    if variant == BuiltinVariantIr::ResultOk {
                        ok.clone()
                    } else {
                        error.clone()
                    }
                });
                let payload = arguments
                    .first()
                    .map(|argument| self.check_expression(argument, payload_expected.as_ref()));
                let payload_ty = payload_expected
                    .clone()
                    .or_else(|| payload.as_ref().map(|value| value.ty.clone()))
                    .unwrap_or(IrType::Unit);
                let (ok, error) = expected_pair.unwrap_or_else(|| {
                    self.type_error(
                        span.clone(),
                        "`Ok`/`Err` requires an expected Result<T, E> or two type arguments",
                    );
                    if variant == BuiltinVariantIr::ResultOk {
                        (payload_ty.clone(), IrType::Unit)
                    } else {
                        (IrType::Unit, payload_ty.clone())
                    }
                });
                (payload, IrType::Result(Box::new(ok), Box::new(error)))
            }
            BuiltinVariantIr::OptionNone => unreachable!("None is a value, not a call"),
        };
        Some(TypedExpressionIr {
            ty: result_type,
            effect: payload_expected
                .as_ref()
                .map_or(IrEffect::Immediate, |value| value.effect),
            span,
            kind: TypedExpressionKind::BuiltinVariant {
                variant,
                payload: payload_expected.map(Box::new),
            },
        })
    }

    fn check_enum_variant_call(
        &mut self,
        whole: &Expression,
        variant: DefinitionId,
        arguments: &[Expression],
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        let expected = self
            .analyzer
            .variant_payloads
            .get(&variant)
            .cloned()
            .unwrap_or_default();
        if expected.len() != arguments.len() {
            self.type_error(
                span.clone(),
                &format!(
                    "enum variant expects {} payload values, found {}",
                    expected.len(),
                    arguments.len()
                ),
            );
        }
        let values = arguments
            .iter()
            .enumerate()
            .map(|(index, argument)| {
                let expected = expected.get(index);
                let value = self.check_expression(argument, expected);
                if let Some(expected) = expected {
                    self.expect_type(&value.ty, expected, &value.span);
                }
                value
            })
            .collect::<Vec<_>>();
        let payload = match values.len() {
            0 => None,
            1 => values.into_iter().next().map(Box::new),
            _ => Some(Box::new(TypedExpressionIr {
                ty: IrType::Tuple(values.iter().map(|value| value.ty.clone()).collect()),
                effect: expression_effect(&values),
                span: span.clone(),
                kind: TypedExpressionKind::Tuple(values),
            })),
        };
        let ty = self.analyzer.definitions[variant.0 as usize].ty.clone();
        let IrType::Named(enum_definition) = ty.clone() else {
            self.type_error(span.clone(), "enum variant has no nominal enum type");
            return self.error_expression(span);
        };
        TypedExpressionIr {
            ty,
            effect: payload
                .as_ref()
                .map_or(IrEffect::Immediate, |payload| payload.effect),
            span,
            kind: TypedExpressionKind::EnumConstruct {
                enum_definition,
                variant_definition: variant,
                payload,
            },
        }
    }

    fn check_construct(
        &mut self,
        whole: &Expression,
        ty: &ast::QualifiedName,
        fields: &[ast::FieldInitializer],
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        let Some(definition) = self
            .analyzer
            .resolve_symbol_path(&self.module, ty, SymbolUse::Type)
        else {
            return self.error_expression(span);
        };
        self.check_construct_fields(whole, definition, fields)
    }

    fn check_construct_fields(
        &mut self,
        whole: &Expression,
        definition: DefinitionId,
        fields: &[ast::FieldInitializer],
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        if self.is_stateful_definition(definition) {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(whole.range),
                "@stateful values cannot be constructed directly",
                "use new.create<T> inside a migration or obtain a value through StateHandle<T>",
            );
            return self.error_expression(span);
        }
        let fields = self.check_fields(definition, fields);
        TypedExpressionIr {
            ty: IrType::Named(definition),
            effect: fields
                .iter()
                .fold(IrEffect::Immediate, |effect, (_, value)| {
                    max_effect(effect, value.effect)
                }),
            span,
            kind: TypedExpressionKind::Construct { definition, fields },
        }
    }

    fn check_fields(
        &mut self,
        definition: DefinitionId,
        fields: &[ast::FieldInitializer],
    ) -> Vec<(DefinitionId, TypedExpressionIr)> {
        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        for field in fields {
            let Some(field_id) = self
                .analyzer
                .members
                .get(&(definition, field.name.text.clone()))
                .copied()
            else {
                self.analyzer.push_source_error(
                    ErrorCode::NX2501,
                    &self.module.source,
                    byte_range(field.range),
                    format!("unknown field `{}`", field.name.text),
                    "constructor field does not exist",
                );
                continue;
            };
            if !seen.insert(field_id) {
                self.analyzer.push_source_error(
                    ErrorCode::NX2101,
                    &self.module.source,
                    byte_range(field.range),
                    format!("duplicate field initializer `{}`", field.name.text),
                    "field can only be initialized once",
                );
            }
            let expected = self.analyzer.definitions[field_id.0 as usize].ty.clone();
            let value = self.check_expression(&field.value, Some(&expected));
            self.expect_type(&value.ty, &expected, &value.span);
            self.analyzer
                .record_reference(&self.module, field.name.range, field_id);
            result.push((field_id, value));
        }
        result.sort_by_key(|(field, _)| *field);
        result
    }

    fn check_match(
        &mut self,
        whole: &Expression,
        value: &Expression,
        arms: &[ast::MatchArm],
        expected: Option<&IrType>,
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        let value = self.check_expression(value, None);
        let mut typed_arms = Vec::new();
        let mut result = expected.cloned();
        for arm in arms {
            self.scopes.push(BTreeMap::new());
            let pattern = self.check_pattern(&arm.pattern, &value.ty);
            let arm_value = self.check_expression(&arm.value, result.as_ref());
            if let Some(result) = &result {
                self.expect_type(&arm_value.ty, result, &arm_value.span);
            } else {
                result = Some(arm_value.ty.clone());
            }
            self.scopes.pop();
            typed_arms.push(TypedMatchArmIr {
                pattern,
                value: arm_value,
            });
        }
        self.check_nominal_match_coverage(whole, &value, arms, &typed_arms);
        TypedExpressionIr {
            ty: result.unwrap_or(IrType::Unit),
            effect: typed_arms.iter().fold(value.effect, |effect, arm| {
                max_effect(effect, arm.value.effect)
            }),
            span,
            kind: TypedExpressionKind::Match {
                value: Box::new(value),
                arms: typed_arms,
            },
        }
    }

    fn check_nominal_match_coverage(
        &mut self,
        whole: &Expression,
        value: &TypedExpressionIr,
        arms: &[ast::MatchArm],
        typed_arms: &[TypedMatchArmIr],
    ) {
        let IrType::Named(type_definition) = value.ty else {
            return;
        };
        let Some(metadata) = self.analyzer.type_metadata.get(&type_definition) else {
            return;
        };
        let variants = metadata.variant_order.clone();
        if variants.is_empty() {
            return;
        }
        let variant_set = variants.iter().copied().collect::<BTreeSet<_>>();
        let mut seen = BTreeMap::<DefinitionId, TextRange>::new();
        let mut catch_all = false;
        for (arm, typed_arm) in arms.iter().zip(typed_arms) {
            match &typed_arm.pattern.kind {
                TypedPatternKind::Wildcard | TypedPatternKind::Binding(_) => catch_all = true,
                TypedPatternKind::Variant { definition, .. }
                    if variant_set.contains(definition) =>
                {
                    let arm_range = TextRange::new(arm.pattern.range.start, arm.value.range.end);
                    if let Some(first_range) = seen.insert(*definition, arm_range) {
                        let variant_name = self.analyzer.definitions[definition.0 as usize]
                            .name
                            .clone();
                        self.analyzer.diagnostics.push(
                            Diagnostic::new(
                                ErrorCode::NX2202,
                                Severity::Error,
                                format!("variant `{variant_name}` is matched more than once"),
                            )
                            .with_label(Label::primary(
                                source_identity(&self.module.source),
                                byte_range(arm_range),
                                "duplicate match arm",
                            ))
                            .with_label(Label::secondary(
                                source_identity(&self.module.source),
                                byte_range(first_range),
                                "first match arm for this variant",
                            ))
                            .with_note(format!("duplicate variant: {variant_name}")),
                        );
                    }
                }
                _ => {}
            }
        }
        if catch_all {
            return;
        }
        let missing = variants
            .into_iter()
            .filter(|variant| !seen.contains_key(variant))
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return;
        }
        let mut diagnostic = Diagnostic::new(
            ErrorCode::NX2201,
            Severity::Error,
            "match does not cover every enum variant",
        )
        .with_label(Label::primary(
            source_identity(&self.module.source),
            byte_range(whole.range),
            "non-exhaustive match expression",
        ));
        for variant in missing {
            diagnostic = diagnostic.with_note(format!(
                "missing variant: {}",
                self.analyzer.definitions[variant.0 as usize].name
            ));
        }
        self.analyzer.diagnostics.push(diagnostic);
    }

    #[allow(clippy::too_many_lines)]
    fn check_pattern(&mut self, pattern: &Pattern, expected: &IrType) -> TypedPatternIr {
        let span = source_range(&self.module.source, pattern.range);
        let kind = match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Error => TypedPatternKind::Wildcard,
            PatternKind::Binding(binding) => {
                let definition =
                    self.allocate_local(binding.text.clone(), expected.clone(), binding.range);
                self.scopes
                    .last_mut()
                    .expect("pattern scope exists")
                    .insert(binding.text.clone(), definition);
                TypedPatternKind::Binding(definition)
            }
            PatternKind::Literal(literal) => {
                let (actual, literal) =
                    typed_literal(literal, Some(expected)).unwrap_or_else(|message| {
                        self.analyzer.push_source_error(
                            ErrorCode::NX2101,
                            &self.module.source,
                            byte_range(pattern.range),
                            message,
                            "pattern literal must fit the scrutinee scalar type",
                        );
                        (IrType::Unit, IrLiteral::Unit)
                    });
                self.expect_type(&actual, expected, &span);
                TypedPatternKind::Literal(literal)
            }
            PatternKind::Variant { path, payload } => {
                if let Some(kind) = self.check_builtin_variant_pattern(path, payload, expected) {
                    return TypedPatternIr {
                        ty: expected.clone(),
                        span,
                        kind,
                    };
                }
                let Some(definition) =
                    self.analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Value)
                else {
                    return TypedPatternIr {
                        ty: expected.clone(),
                        span,
                        kind: TypedPatternKind::Wildcard,
                    };
                };
                let expected_payload = self
                    .analyzer
                    .variant_payloads
                    .get(&definition)
                    .cloned()
                    .unwrap_or_default();
                let variant_type = self.analyzer.definitions[definition.0 as usize].ty.clone();
                self.expect_type(&variant_type, expected, &span);
                if expected_payload.len() != payload.len() {
                    self.type_error(
                        span.clone(),
                        &format!(
                            "variant pattern expects {} payload values, found {}",
                            expected_payload.len(),
                            payload.len()
                        ),
                    );
                }
                let payload = payload
                    .iter()
                    .enumerate()
                    .map(|(index, pattern)| {
                        let ty = expected_payload.get(index).cloned().unwrap_or(IrType::Unit);
                        self.check_pattern(pattern, &ty)
                    })
                    .collect();
                TypedPatternKind::Variant {
                    definition,
                    payload,
                }
            }
            PatternKind::Struct { path, fields } => {
                let Some(definition) =
                    self.analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Type)
                else {
                    return TypedPatternIr {
                        ty: expected.clone(),
                        span,
                        kind: TypedPatternKind::Wildcard,
                    };
                };
                let mut typed = Vec::new();
                for field in fields {
                    let Some(field_id) = self
                        .analyzer
                        .members
                        .get(&(definition, field.name.text.clone()))
                        .copied()
                    else {
                        continue;
                    };
                    let ty = self.analyzer.definitions[field_id.0 as usize].ty.clone();
                    typed.push((field_id, self.check_pattern(&field.pattern, &ty)));
                }
                TypedPatternKind::Struct {
                    definition,
                    fields: typed,
                }
            }
        };
        TypedPatternIr {
            ty: expected.clone(),
            span,
            kind,
        }
    }

    fn check_builtin_variant_pattern(
        &mut self,
        path: &ast::QualifiedName,
        payload: &[Pattern],
        expected: &IrType,
    ) -> Option<TypedPatternKind> {
        let [name] = path.segments.as_slice() else {
            return None;
        };
        let (variant, payload_type) = match (name.text.as_str(), expected) {
            ("Some", IrType::Option(inner)) => {
                (BuiltinVariantIr::OptionSome, Some(inner.as_ref().clone()))
            }
            ("None", IrType::Option(_)) => (BuiltinVariantIr::OptionNone, None),
            ("Ok", IrType::Result(ok, _)) => {
                (BuiltinVariantIr::ResultOk, Some(ok.as_ref().clone()))
            }
            ("Err", IrType::Result(_, error)) => {
                (BuiltinVariantIr::ResultErr, Some(error.as_ref().clone()))
            }
            ("Some" | "None", _) => {
                self.type_error(
                    source_range(&self.module.source, path.range),
                    "Option pattern requires an Option<T> scrutinee",
                );
                (
                    if name.text == "Some" {
                        BuiltinVariantIr::OptionSome
                    } else {
                        BuiltinVariantIr::OptionNone
                    },
                    None,
                )
            }
            ("Ok" | "Err", _) => {
                self.type_error(
                    source_range(&self.module.source, path.range),
                    "Result pattern requires a Result<T, E> scrutinee",
                );
                (
                    if name.text == "Ok" {
                        BuiltinVariantIr::ResultOk
                    } else {
                        BuiltinVariantIr::ResultErr
                    },
                    None,
                )
            }
            _ => return None,
        };
        let expected_arity = usize::from(payload_type.is_some());
        if payload.len() != expected_arity {
            self.type_error(
                source_range(&self.module.source, path.range),
                &format!(
                    "builtin variant pattern expects {expected_arity} payload values, found {}",
                    payload.len()
                ),
            );
        }
        let payload = payload.first().map(|pattern| {
            Box::new(self.check_pattern(pattern, payload_type.as_ref().unwrap_or(&IrType::Unit)))
        });
        Some(TypedPatternKind::BuiltinVariant { variant, payload })
    }

    fn check_place(&mut self, expression: &Expression) -> Option<TypedPlaceIr> {
        match &expression.kind {
            ExpressionKind::Name(path) => {
                if path.segments.len() == 1 {
                    let definition = self.local(&path.segments[0].text).or_else(|| {
                        self.analyzer
                            .resolve_symbol_path(&self.module, path, SymbolUse::Value)
                    })?;
                    if self.readonly_loop_bindings.contains(&definition) {
                        self.type_error(
                            source_range(&self.module.source, expression.range),
                            &format!(
                                "static-range binding `{}` is read-only",
                                path.segments[0].text
                            ),
                        );
                        return None;
                    }
                    return Some(TypedPlaceIr::Definition(definition));
                }
                let value = self.check_receiver_name(path)?;
                match value.kind {
                    TypedExpressionKind::Reference(definition) => {
                        Some(TypedPlaceIr::Definition(definition))
                    }
                    TypedExpressionKind::Field { base, field } => {
                        self.mutable_field_place(base, field, expression.range)
                    }
                    TypedExpressionKind::StateField { base, field } => {
                        Some(TypedPlaceIr::StateField { base, field })
                    }
                    _ => None,
                }
            }
            ExpressionKind::Member { receiver, member } => {
                let base = self.check_expression(receiver, None);
                let value = self.check_field_access(base, member, expression.range)?;
                match value.kind {
                    TypedExpressionKind::Field { base, field } => {
                        self.mutable_field_place(base, field, expression.range)
                    }
                    TypedExpressionKind::StateField { base, field } => {
                        Some(TypedPlaceIr::StateField { base, field })
                    }
                    _ => None,
                }
            }
            ExpressionKind::Index { receiver, index } => {
                let base = self.check_expression(receiver, None);
                let index_type = match &base.ty {
                    IrType::Array(_) | IrType::Buffer(_) => IrType::I32,
                    IrType::Map(key, _) => key.as_ref().clone(),
                    IrType::Unit
                    | IrType::Bool
                    | IrType::I32
                    | IrType::I64
                    | IrType::F32
                    | IrType::F64
                    | IrType::String
                    | IrType::Rune
                    | IrType::Named(_)
                    | IrType::Option(_)
                    | IrType::Result(_, _)
                    | IrType::Tuple(_)
                    | IrType::HostRequest(_)
                    | IrType::ResourceToken(_)
                    | IrType::Snapshot(_)
                    | IrType::StateHandle(_)
                    | IrType::TypeParameter(_) => {
                        self.type_error(
                            source_range(&self.module.source, expression.range),
                            "assignment index requires Array, Map, or Buffer",
                        );
                        return None;
                    }
                };
                let index = self.check_expression(index, Some(&index_type));
                self.expect_type(&index.ty, &index_type, &index.span);
                Some(TypedPlaceIr::Index {
                    base: Box::new(base),
                    index: Box::new(index),
                })
            }
            _ => {
                self.type_error(
                    source_range(&self.module.source, expression.range),
                    "assignment target is not a mutable place",
                );
                None
            }
        }
    }

    fn mutable_field_place(
        &mut self,
        base: Box<TypedExpressionIr>,
        field: DefinitionId,
        range: TextRange,
    ) -> Option<TypedPlaceIr> {
        let mutable = match &base.ty {
            IrType::Named(owner) => {
                self.analyzer.definitions[owner.0 as usize].kind == DefinitionKind::Class
            }
            _ => false,
        };
        if !mutable {
            self.type_error(
                source_range(&self.module.source, range),
                "only class fields are assignable",
            );
            return None;
        }
        Some(TypedPlaceIr::Field { base, field })
    }

    #[allow(clippy::needless_pass_by_value)]
    fn allocate_local(&mut self, name: String, ty: IrType, range: TextRange) -> DefinitionId {
        let function = self.current_function.map_or("const", |definition| {
            self.analyzer.definitions[definition.0 as usize]
                .name
                .as_str()
        });
        let ordinal = self.locals.len();
        let definition = self.analyzer.allocate_definition(
            self.module.key.package.clone(),
            self.module.key.module.clone(),
            name.clone(),
            DefinitionKind::Local,
            DeclarationVisibility::Private,
            ty,
            IrEffect::Immediate,
            source_range(&self.module.source, range),
            format!(
                "{}::{}::local::{function}::{ordinal}::{name}",
                self.module.key.package, self.module.key.module
            ),
        );
        self.locals.push(definition);
        definition
    }

    fn local(&self, name: &str) -> Option<DefinitionId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn expect_type(&mut self, actual: &IrType, expected: &IrType, span: &SourceRange) {
        let recovery_unit = (actual == &IrType::Unit || expected == &IrType::Unit)
            && self
                .recovery_unit_spans
                .contains(&(span.source.clone(), span.start, span.end));
        if actual != expected && !recovery_unit {
            let numeric_conversion = is_numeric(actual) && is_numeric(expected);
            self.analyzer.diagnostics.push(
                Diagnostic::new(
                    if numeric_conversion {
                        ErrorCode::NX2401
                    } else {
                        ErrorCode::NX2101
                    },
                    Severity::Error,
                    if numeric_conversion {
                        format!(
                            "cannot implicitly convert {} to {}",
                            display_type(actual, &self.analyzer.definitions),
                            display_type(expected, &self.analyzer.definitions)
                        )
                    } else {
                        format!(
                            "expected {}, found {}",
                            display_type(expected, &self.analyzer.definitions),
                            display_type(actual, &self.analyzer.definitions)
                        )
                    },
                )
                .with_label(Label::primary(
                    source_identity(&span.source),
                    range_from_source(span),
                    if numeric_conversion {
                        "numeric conversion is not implicit"
                    } else {
                        "expression has an incompatible type"
                    },
                ))
                .with_note(format!("expected type: {expected:?}"))
                .with_note(format!("actual type: {actual:?}")),
            );
        }
    }

    fn mark_recovery_unit(&mut self, span: &SourceRange) {
        self.recovery_unit_spans
            .insert((span.source.clone(), span.start, span.end));
    }

    fn is_recovery_unit(&self, span: &SourceRange) -> bool {
        self.recovery_unit_spans
            .contains(&(span.source.clone(), span.start, span.end))
    }

    fn recovery_unit_type(&mut self, span: &SourceRange) -> IrType {
        self.mark_recovery_unit(span);
        IrType::Unit
    }

    fn error_expression(&mut self, span: SourceRange) -> TypedExpressionIr {
        self.mark_recovery_unit(&span);
        TypedExpressionIr {
            ty: IrType::Unit,
            effect: IrEffect::Immediate,
            span,
            kind: TypedExpressionKind::Literal(IrLiteral::Unit),
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn type_error(&mut self, span: SourceRange, message: &str) {
        self.analyzer.push_source_error(
            ErrorCode::NX2101,
            &span.source,
            range_from_source(&span),
            message,
            "expression has an incompatible type",
        );
    }

    fn validate_loop_control(&mut self, range: TextRange, keyword: &str) {
        if self.loop_depth == 0 {
            self.analyzer.push_source_error(
                ErrorCode::NX1002,
                &self.module.source,
                byte_range(range),
                format!("{keyword} is only valid inside a loop"),
                "no enclosing while or static-range for",
            );
        }
    }

    fn mark_restricted(&mut self, operation: RestrictedOperation) {
        if let Some(function) = self.current_function {
            self.analyzer
                .restricted
                .entry(function)
                .or_default()
                .insert(operation);
        }
    }
}

fn combined_source_snapshots(
    input: &ResolvedBuildInput,
    test_source_set: Option<&PackageSourceSet>,
    environment: &AnalysisEnvironment,
) -> (Arc<SourceSnapshotRegistry>, BTreeSet<SourceIdentity>) {
    let mut sources = BTreeMap::<SourceIdentity, Arc<str>>::new();
    let mut conflicts = BTreeSet::new();
    for source_set in input.all_source_sets() {
        for unit in source_set.units().values() {
            retain_source_snapshot(
                &mut sources,
                &mut conflicts,
                source_identity(&unit.key),
                Arc::clone(&unit.text),
            );
        }
    }
    if let Some(test_source_set) = test_source_set {
        for unit in test_source_set.units().values() {
            retain_source_snapshot(
                &mut sources,
                &mut conflicts,
                source_identity(&unit.key),
                Arc::clone(&unit.text),
            );
        }
    }
    for descriptor in nexa_stdlib::standard_library().modules() {
        let source = standard_library_source_key(descriptor);
        retain_source_snapshot(
            &mut sources,
            &mut conflicts,
            source_identity(&source),
            Arc::<str>::from(descriptor.source),
        );
    }
    if let Some(host) = &environment.host {
        for origin in host
            .source
            .iter()
            .chain(
                host.functions
                    .iter()
                    .filter_map(|function| function.source.as_ref()),
            )
            .chain(
                host.required_exports
                    .iter()
                    .filter_map(|export| export.source.as_ref()),
            )
            .chain(host.types.iter().filter_map(|ty| ty.source.as_ref()))
            .chain(
                host.types
                    .iter()
                    .flat_map(|ty| ty.fields.iter().filter_map(|field| field.source.as_ref())),
            )
            .chain(host.types.iter().flat_map(|ty| {
                ty.variants
                    .iter()
                    .filter_map(|variant| variant.source.as_ref())
            }))
        {
            retain_source_snapshot(
                &mut sources,
                &mut conflicts,
                origin.identity.clone(),
                Arc::clone(&origin.text),
            );
        }
    }
    for module in &environment.static_modules {
        for origin in module
            .types
            .iter()
            .filter_map(|ty| ty.source.as_ref())
            .chain(
                module
                    .types
                    .iter()
                    .flat_map(|ty| ty.fields.iter().filter_map(|field| field.source.as_ref())),
            )
            .chain(module.types.iter().flat_map(|ty| {
                ty.variants
                    .iter()
                    .filter_map(|variant| variant.source.as_ref())
            }))
        {
            retain_source_snapshot(
                &mut sources,
                &mut conflicts,
                origin.identity.clone(),
                Arc::clone(&origin.text),
            );
        }
    }
    let mut builder = SourceSnapshotRegistry::builder();
    for (identity, text) in sources {
        builder
            .insert(identity, text)
            .expect("source snapshots were deduplicated by identity");
    }
    (builder.build(), conflicts)
}

fn retain_source_snapshot(
    sources: &mut BTreeMap<SourceIdentity, Arc<str>>,
    conflicts: &mut BTreeSet<SourceIdentity>,
    identity: SourceIdentity,
    text: Arc<str>,
) {
    if let Some(existing) = sources.get(&identity) {
        if existing.as_ref() != text.as_ref() {
            conflicts.insert(identity);
        }
    } else {
        sources.insert(identity, text);
    }
}

fn standard_library_source_keys() -> Vec<SourceKey> {
    let mut sources = nexa_stdlib::standard_library()
        .modules()
        .iter()
        .map(standard_library_source_key)
        .collect::<Vec<_>>();
    sources.sort();
    sources
}

fn standard_library_source_key(descriptor: &nexa_stdlib::ModuleDescriptor) -> SourceKey {
    let path = format!("stdlib/{}.nexa", descriptor.path.replace('.', "/"));
    SourceKey::new(
        PackageId::new(nexa_stdlib::PACKAGE_ID).expect("standard-library package ID is valid"),
        NormalizedPackagePath::new(path)
            .expect("standard-library module path produces a normalized source path"),
    )
}

fn source_identity(source: &SourceKey) -> SourceIdentity {
    SourceIdentity::package(source.package_id.as_str(), source.path.as_str())
}

fn source_range(source: &SourceKey, range: TextRange) -> SourceRange {
    SourceRange {
        source: source.clone(),
        start: range.start.get(),
        end: range.end.get(),
    }
}

fn external_source_range(origin: &ExternalSourceOrigin) -> SourceRange {
    SourceRange {
        source: external_source_key(&origin.identity),
        start: origin.range.start,
        end: origin.range.end,
    }
}

fn range_from_source(range: &SourceRange) -> ByteRange {
    ByteRange::new(range.start, range.end)
}

fn byte_range(range: TextRange) -> ByteRange {
    ByteRange::new(range.start.get(), range.end.get())
}

fn syntax_error_code(kind: nexa_syntax::SyntaxErrorKind) -> ErrorCode {
    match kind {
        nexa_syntax::SyntaxErrorKind::UnexpectedCharacter => ErrorCode::NX1001,
        _ => ErrorCode::NX1002,
    }
}

fn declaration_surface(
    declaration: &DeclarationKind,
) -> Option<(String, DefinitionKind, SymbolKind, IrEffect)> {
    match declaration {
        DeclarationKind::Function(function) => {
            let effect = function_effect(function.effect);
            let kind = if function.effect == FunctionEffect::Task {
                DefinitionKind::Task
            } else {
                DefinitionKind::Function
            };
            let symbol = if function.effect == FunctionEffect::Task {
                SymbolKind::Task
            } else {
                SymbolKind::Function
            };
            Some((function.name.text.clone(), kind, symbol, effect))
        }
        DeclarationKind::Type(ty) => Some((
            ty.name.text.clone(),
            match ty.kind {
                TypeDeclarationKind::Struct => DefinitionKind::Struct,
                TypeDeclarationKind::Enum => DefinitionKind::Enum,
                TypeDeclarationKind::Class => DefinitionKind::Class,
                TypeDeclarationKind::Stateful => DefinitionKind::Stateful,
            },
            SymbolKind::Type,
            IrEffect::Immediate,
        )),
        DeclarationKind::Const(constant) => Some((
            constant.name.text.clone(),
            DefinitionKind::Const,
            SymbolKind::Constant,
            IrEffect::Immediate,
        )),
        DeclarationKind::Error => None,
    }
}

const fn declaration_visibility(visibility: Visibility) -> DeclarationVisibility {
    match visibility {
        Visibility::Private => DeclarationVisibility::Private,
        Visibility::Package => DeclarationVisibility::Package,
        Visibility::Public => DeclarationVisibility::Public,
    }
}

const fn function_effect(effect: FunctionEffect) -> IrEffect {
    match effect {
        FunctionEffect::Ordinary => IrEffect::Ordinary,
        FunctionEffect::Immediate => IrEffect::Immediate,
        FunctionEffect::Task => IrEffect::Task,
        FunctionEffect::Migration => IrEffect::Migration,
        FunctionEffect::Activation => IrEffect::Activation,
        FunctionEffect::Cleanup => IrEffect::Cleanup,
    }
}

fn has_attribute(attributes: &[Attribute], name: &str) -> bool {
    attributes
        .iter()
        .any(|attribute| attribute.name.text == name)
}

fn stable_diagnostic_range(attributes: &[Attribute], fallback: TextRange) -> TextRange {
    let Some(attribute) = attributes
        .iter()
        .find(|attribute| attribute.name.text == "stable")
    else {
        return fallback;
    };
    let [argument] = attribute.arguments.as_slice() else {
        return attribute.range;
    };
    if argument.kind == AttributeArgumentKind::String
        && argument.text.starts_with('"')
        && argument.text.ends_with('"')
        && argument.range.len() >= 2
    {
        return TextRange::new(
            TextSize::new(argument.range.start.get().saturating_add(1)),
            TextSize::new(argument.range.end.get().saturating_sub(1)),
        );
    }
    argument.range
}

fn stable_attribute(attributes: &[Attribute]) -> Result<Option<String>, String> {
    let values = attributes
        .iter()
        .filter(|attribute| attribute.name.text == "stable")
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] => Ok(None),
        [attribute]
            if matches!(
                attribute.arguments.as_slice(),
                [argument] if argument.kind == AttributeArgumentKind::String
            ) =>
        {
            let argument = &attribute.arguments[0].text;
            Ok(Some(decode_quoted(argument).ok_or_else(|| {
                "@stable argument must be a valid string literal".to_owned()
            })?))
        }
        [_] => Err("@stable requires exactly one string argument".into()),
        _ => Err("@stable may only be declared once".into()),
    }
}

fn decode_quoted(value: &str) -> Option<String> {
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        output.push(match chars.next()? {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '"' => '"',
            '\\' => '\\',
            other => other,
        });
    }
    Some(output)
}

fn valid_stable_name(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn canonical_identity_text(identity: &CanonicalSymbolIdentity) -> String {
    if let Some(stable) = identity.explicit_stable_name() {
        format!(
            "{}::stable::{:?}::{stable}",
            identity.package_id(),
            identity.kind()
        )
    } else {
        format!(
            "{}::{}::{:?}::{}",
            identity.package_id(),
            identity.module_path(),
            identity.kind(),
            identity.name()
        )
    }
}

fn stateful_version(attributes: &[Attribute]) -> Option<u32> {
    let attribute = attributes
        .iter()
        .find(|attribute| attribute.name.text == "stateful")?;
    let argument = attribute.arguments.first()?;
    (argument.kind == AttributeArgumentKind::Integer)
        .then(|| argument.text.replace('_', "").parse().ok())
        .flatten()
}

fn builtin_type(name: &str) -> Option<IrType> {
    match name {
        "unit" => Some(IrType::Unit),
        "bool" => Some(IrType::Bool),
        "i32" => Some(IrType::I32),
        "i64" => Some(IrType::I64),
        "f32" => Some(IrType::F32),
        "f64" => Some(IrType::F64),
        "string" => Some(IrType::String),
        "rune" => Some(IrType::Rune),
        _ => None,
    }
}

fn descriptor_surface_type(
    text: &str,
    module: &ModulePath,
    type_parameters: &BTreeSet<String>,
) -> Result<SurfaceType, String> {
    let text = text.trim();
    if type_parameters.contains(text) {
        return Ok(SurfaceType::TypeParameter(text.to_owned()));
    }
    let primitive = match text {
        "unit" => Some(SurfaceType::Unit),
        "bool" => Some(SurfaceType::Bool),
        "i32" => Some(SurfaceType::I32),
        "i64" => Some(SurfaceType::I64),
        "f32" => Some(SurfaceType::F32),
        "f64" => Some(SurfaceType::F64),
        "string" => Some(SurfaceType::String),
        "rune" => Some(SurfaceType::Rune),
        _ => None,
    };
    if let Some(primitive) = primitive {
        return Ok(primitive);
    }
    let Some(open) = text.find('<') else {
        return Ok(SurfaceType::Named {
            module: module.clone(),
            name: text.to_owned(),
        });
    };
    if !text.ends_with('>') {
        return Err(format!("unclosed generic type `{text}`"));
    }
    let base = &text[..open];
    let arguments = split_descriptor_arguments(&text[open + 1..text.len() - 1])?
        .into_iter()
        .map(|argument| descriptor_surface_type(argument, module, type_parameters))
        .collect::<Result<Vec<_>, _>>()?;
    match (base, arguments.as_slice()) {
        ("Option", [inner]) => Ok(SurfaceType::Option(Box::new(inner.clone()))),
        ("Result", [ok, error]) => Ok(SurfaceType::Result(
            Box::new(ok.clone()),
            Box::new(error.clone()),
        )),
        ("Array", [inner]) => Ok(SurfaceType::Array(Box::new(inner.clone()))),
        ("Map", [key, value]) => Ok(SurfaceType::Map(
            Box::new(key.clone()),
            Box::new(value.clone()),
        )),
        _ => Err(format!("unsupported descriptor type `{text}`")),
    }
}

fn split_descriptor_arguments(text: &str) -> Result<Vec<&str>, String> {
    let mut depth = 0_u32;
    let mut start = 0;
    let mut values = Vec::new();
    for (offset, character) in text.char_indices() {
        match character {
            '<' => depth = depth.saturating_add(1),
            '>' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced descriptor type `{text}`"))?;
            }
            ',' if depth == 0 => {
                values.push(text[start..offset].trim());
                start = offset + 1;
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(format!("unbalanced descriptor type `{text}`"));
    }
    values.push(text[start..].trim());
    if values.iter().any(|value| value.is_empty()) {
        return Err(format!("empty descriptor type argument in `{text}`"));
    }
    Ok(values)
}

fn is_none_expression(expression: &Expression) -> bool {
    matches!(
        &expression.kind,
        ExpressionKind::Name(path)
            if path.segments.len() == 1 && path.segments[0].text == "None"
    )
}

fn builtin_variant_call(
    expression: &Expression,
) -> Option<(&str, &Vec<TypeRef>, &Vec<Expression>)> {
    let ExpressionKind::Call {
        callee,
        type_arguments,
        arguments,
    } = &expression.kind
    else {
        return None;
    };
    let ExpressionKind::Name(path) = &callee.kind else {
        return None;
    };
    let [name] = path.segments.as_slice() else {
        return None;
    };
    matches!(name.text.as_str(), "Some" | "Ok" | "Err").then_some((
        name.text.as_str(),
        type_arguments,
        arguments,
    ))
}

fn merge_surface_constraint(
    surface: &SurfaceType,
    actual: &IrType,
    bindings: &mut BTreeMap<String, IrType>,
) {
    let mut inferred = bindings.clone();
    if unify_surface_type(surface, actual, &mut inferred) {
        *bindings = inferred;
    }
}

fn instantiate_inferred_surface_type(
    surface: &SurfaceType,
    bindings: &BTreeMap<String, IrType>,
) -> Option<IrType> {
    match surface {
        SurfaceType::Unit => Some(IrType::Unit),
        SurfaceType::Bool => Some(IrType::Bool),
        SurfaceType::I32 => Some(IrType::I32),
        SurfaceType::I64 => Some(IrType::I64),
        SurfaceType::F32 => Some(IrType::F32),
        SurfaceType::F64 => Some(IrType::F64),
        SurfaceType::String => Some(IrType::String),
        SurfaceType::Rune => Some(IrType::Rune),
        SurfaceType::TypeParameter(name) => bindings.get(name).cloned(),
        SurfaceType::Named { .. } => None,
        SurfaceType::Option(inner) => Some(IrType::Option(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        ))),
        SurfaceType::Result(success, error) => Some(IrType::Result(
            Box::new(instantiate_inferred_surface_type(success, bindings)?),
            Box::new(instantiate_inferred_surface_type(error, bindings)?),
        )),
        SurfaceType::Array(inner) => Some(IrType::Array(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        ))),
        SurfaceType::Map(key, value) => Some(IrType::Map(
            Box::new(instantiate_inferred_surface_type(key, bindings)?),
            Box::new(instantiate_inferred_surface_type(value, bindings)?),
        )),
        SurfaceType::Tuple(values) => values
            .iter()
            .map(|value| instantiate_inferred_surface_type(value, bindings))
            .collect::<Option<Vec<_>>>()
            .map(IrType::Tuple),
        SurfaceType::HostRequest(inner) => Some(IrType::HostRequest(match inner.as_deref() {
            Some(inner) => Some(Box::new(instantiate_inferred_surface_type(
                inner, bindings,
            )?)),
            None => None,
        })),
        SurfaceType::ResourceToken(inner) => Some(IrType::ResourceToken(match inner.as_deref() {
            Some(inner) => Some(Box::new(instantiate_inferred_surface_type(
                inner, bindings,
            )?)),
            None => None,
        })),
        SurfaceType::Snapshot(inner) => Some(IrType::Snapshot(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        ))),
        SurfaceType::Buffer(inner) => Some(IrType::Buffer(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        ))),
        SurfaceType::StateHandle(inner) => Some(IrType::StateHandle(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        ))),
    }
}

fn unify_surface_type(
    surface: &SurfaceType,
    actual: &IrType,
    bindings: &mut BTreeMap<String, IrType>,
) -> bool {
    match (surface, actual) {
        (SurfaceType::TypeParameter(name), actual) => {
            if let Some(bound) = bindings.get(name) {
                bound == actual
            } else {
                bindings.insert(name.clone(), actual.clone());
                true
            }
        }
        (SurfaceType::Unit, IrType::Unit)
        | (SurfaceType::Bool, IrType::Bool)
        | (SurfaceType::I32, IrType::I32)
        | (SurfaceType::I64, IrType::I64)
        | (SurfaceType::F32, IrType::F32)
        | (SurfaceType::F64, IrType::F64)
        | (SurfaceType::String, IrType::String)
        | (SurfaceType::Rune, IrType::Rune)
        | (SurfaceType::Named { .. }, IrType::Named(_)) => true,
        (SurfaceType::Option(left), IrType::Option(right))
        | (SurfaceType::Array(left), IrType::Array(right))
        | (SurfaceType::Snapshot(left), IrType::Snapshot(right))
        | (SurfaceType::Buffer(left), IrType::Buffer(right))
        | (SurfaceType::StateHandle(left), IrType::StateHandle(right)) => {
            unify_surface_type(left, right, bindings)
        }
        (SurfaceType::Result(left_ok, left_error), IrType::Result(right_ok, right_error))
        | (SurfaceType::Map(left_ok, left_error), IrType::Map(right_ok, right_error)) => {
            unify_surface_type(left_ok, right_ok, bindings)
                && unify_surface_type(left_error, right_error, bindings)
        }
        (SurfaceType::Tuple(left), IrType::Tuple(right)) if left.len() == right.len() => left
            .iter()
            .zip(right)
            .all(|(left, right)| unify_surface_type(left, right, bindings)),
        (SurfaceType::HostRequest(left), IrType::HostRequest(right))
        | (SurfaceType::ResourceToken(left), IrType::ResourceToken(right)) => match (left, right) {
            (Some(left), Some(right)) => unify_surface_type(left, right, bindings),
            (None, None) => true,
            _ => false,
        },
        _ => false,
    }
}

fn collect_named_types(ty: &IrType, output: &mut Vec<DefinitionId>) {
    match ty {
        IrType::Named(definition) => output.push(*definition),
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => collect_named_types(inner, output),
        IrType::HostRequest(inner) | IrType::ResourceToken(inner) => {
            if let Some(inner) = inner {
                collect_named_types(inner, output);
            }
        }
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            collect_named_types(ok, output);
            collect_named_types(error, output);
        }
        IrType::Tuple(values) => {
            for value in values {
                collect_named_types(value, output);
            }
        }
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::TypeParameter(_) => {}
    }
}

const fn visibility_name(visibility: DeclarationVisibility) -> &'static str {
    match visibility {
        DeclarationVisibility::Private => "private",
        DeclarationVisibility::Package => "pub(package)",
        DeclarationVisibility::Public => "pub",
    }
}

fn literal_const(literal: &ast::Literal, expected: Option<&IrType>) -> Result<ConstValue, String> {
    match literal.kind {
        LiteralKind::Bool => Ok(ConstValue::Bool(literal.raw == "true")),
        LiteralKind::Integer if expected == Some(&IrType::I64) => parse_integer_i64(&literal.raw)
            .map(ConstValue::I64)
            .ok_or_else(|| format!("integer literal `{}` does not fit i64", literal.raw)),
        LiteralKind::Integer => parse_integer_i64(&literal.raw)
            .and_then(|value| i32::try_from(value).ok())
            .map(ConstValue::I32)
            .ok_or_else(|| format!("integer literal `{}` does not fit i32", literal.raw)),
        LiteralKind::Float if expected == Some(&IrType::F32) => {
            parse_float_f32(&literal.raw).map(|value| ConstValue::F32(value.to_bits()))
        }
        LiteralKind::Float => {
            parse_float_f64(&literal.raw).map(|value| ConstValue::F64(value.to_bits()))
        }
        LiteralKind::String => Ok(ConstValue::String(
            literal.cooked.clone().unwrap_or_default(),
        )),
        LiteralKind::Rune => literal
            .cooked
            .as_deref()
            .and_then(|value| value.chars().next())
            .map(ConstValue::Rune)
            .ok_or_else(|| "rune literal has no Unicode scalar value".to_owned()),
    }
}

fn eval_const_unary(operator: UnaryOperatorKind, value: ConstValue) -> Option<ConstValue> {
    match (operator, value) {
        (UnaryOperatorKind::Positive, value) => Some(value),
        (UnaryOperatorKind::Negate, ConstValue::I32(value)) => {
            value.checked_neg().map(ConstValue::I32)
        }
        (UnaryOperatorKind::Negate, ConstValue::I64(value)) => {
            value.checked_neg().map(ConstValue::I64)
        }
        (UnaryOperatorKind::Negate, ConstValue::F32(bits)) => {
            Some(ConstValue::F32((-f32::from_bits(bits)).to_bits()))
        }
        (UnaryOperatorKind::Negate, ConstValue::F64(bits)) => {
            Some(ConstValue::F64((-f64::from_bits(bits)).to_bits()))
        }
        (UnaryOperatorKind::Not, ConstValue::Bool(value)) => Some(ConstValue::Bool(!value)),
        _ => None,
    }
}

#[allow(clippy::float_cmp)] // Nexa constant folding implements exact IEEE equality semantics.
fn eval_const_binary(
    operator: BinaryOperatorKind,
    left: ConstValue,
    right: ConstValue,
) -> Option<ConstValue> {
    use BinaryOperatorKind as Operator;
    match (left, right) {
        (ConstValue::I32(left), ConstValue::I32(right)) => match operator {
            Operator::Add => left.checked_add(right).map(ConstValue::I32),
            Operator::Subtract => left.checked_sub(right).map(ConstValue::I32),
            Operator::Multiply => left.checked_mul(right).map(ConstValue::I32),
            Operator::Divide => left.checked_div(right).map(ConstValue::I32),
            Operator::Remainder => left.checked_rem(right).map(ConstValue::I32),
            Operator::Equal => Some(ConstValue::Bool(left == right)),
            Operator::NotEqual => Some(ConstValue::Bool(left != right)),
            Operator::Less => Some(ConstValue::Bool(left < right)),
            Operator::LessEqual => Some(ConstValue::Bool(left <= right)),
            Operator::Greater => Some(ConstValue::Bool(left > right)),
            Operator::GreaterEqual => Some(ConstValue::Bool(left >= right)),
            Operator::And | Operator::Or => None,
        },
        (ConstValue::I64(left), ConstValue::I64(right)) => match operator {
            Operator::Add => left.checked_add(right).map(ConstValue::I64),
            Operator::Subtract => left.checked_sub(right).map(ConstValue::I64),
            Operator::Multiply => left.checked_mul(right).map(ConstValue::I64),
            Operator::Divide => left.checked_div(right).map(ConstValue::I64),
            Operator::Remainder => left.checked_rem(right).map(ConstValue::I64),
            Operator::Equal => Some(ConstValue::Bool(left == right)),
            Operator::NotEqual => Some(ConstValue::Bool(left != right)),
            Operator::Less => Some(ConstValue::Bool(left < right)),
            Operator::LessEqual => Some(ConstValue::Bool(left <= right)),
            Operator::Greater => Some(ConstValue::Bool(left > right)),
            Operator::GreaterEqual => Some(ConstValue::Bool(left >= right)),
            Operator::And | Operator::Or => None,
        },
        (ConstValue::F32(left), ConstValue::F32(right)) => {
            let left = f32::from_bits(left);
            let right = f32::from_bits(right);
            match operator {
                Operator::Add => Some(ConstValue::F32(canonical_f32_bits(left + right))),
                Operator::Subtract => Some(ConstValue::F32(canonical_f32_bits(left - right))),
                Operator::Multiply => Some(ConstValue::F32(canonical_f32_bits(left * right))),
                Operator::Divide => Some(ConstValue::F32(canonical_f32_bits(left / right))),
                Operator::Remainder => Some(ConstValue::F32(canonical_f32_bits(left % right))),
                Operator::Equal => Some(ConstValue::Bool(left == right)),
                Operator::NotEqual => Some(ConstValue::Bool(left != right)),
                Operator::Less => Some(ConstValue::Bool(left < right)),
                Operator::LessEqual => Some(ConstValue::Bool(left <= right)),
                Operator::Greater => Some(ConstValue::Bool(left > right)),
                Operator::GreaterEqual => Some(ConstValue::Bool(left >= right)),
                Operator::And | Operator::Or => None,
            }
        }
        (ConstValue::F64(left), ConstValue::F64(right)) => {
            let left = f64::from_bits(left);
            let right = f64::from_bits(right);
            match operator {
                Operator::Add => Some(ConstValue::F64(canonical_f64_bits(left + right))),
                Operator::Subtract => Some(ConstValue::F64(canonical_f64_bits(left - right))),
                Operator::Multiply => Some(ConstValue::F64(canonical_f64_bits(left * right))),
                Operator::Divide => Some(ConstValue::F64(canonical_f64_bits(left / right))),
                Operator::Remainder => Some(ConstValue::F64(canonical_f64_bits(left % right))),
                Operator::Equal => Some(ConstValue::Bool(left == right)),
                Operator::NotEqual => Some(ConstValue::Bool(left != right)),
                Operator::Less => Some(ConstValue::Bool(left < right)),
                Operator::LessEqual => Some(ConstValue::Bool(left <= right)),
                Operator::Greater => Some(ConstValue::Bool(left > right)),
                Operator::GreaterEqual => Some(ConstValue::Bool(left >= right)),
                Operator::And | Operator::Or => None,
            }
        }
        (ConstValue::Bool(left), ConstValue::Bool(right)) => match operator {
            Operator::And => Some(ConstValue::Bool(left && right)),
            Operator::Or => Some(ConstValue::Bool(left || right)),
            Operator::Equal => Some(ConstValue::Bool(left == right)),
            Operator::NotEqual => Some(ConstValue::Bool(left != right)),
            _ => None,
        },
        (ConstValue::String(left), ConstValue::String(right)) => match operator {
            Operator::Add => Some(ConstValue::String(left + &right)),
            Operator::Equal => Some(ConstValue::Bool(left == right)),
            Operator::NotEqual => Some(ConstValue::Bool(left != right)),
            _ => None,
        },
        (left, right) => match operator {
            Operator::Equal => Some(ConstValue::Bool(left == right)),
            Operator::NotEqual => Some(ConstValue::Bool(left != right)),
            _ => None,
        },
    }
}

fn canonical_f32_bits(value: f32) -> u32 {
    if value.is_nan() {
        f32::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

fn typed_literal(
    literal: &ast::Literal,
    expected: Option<&IrType>,
) -> Result<(IrType, IrLiteral), String> {
    match literal.kind {
        LiteralKind::Bool => Ok((IrType::Bool, IrLiteral::Bool(literal.raw == "true"))),
        LiteralKind::String => Ok((
            IrType::String,
            IrLiteral::String(literal.cooked.clone().unwrap_or_default()),
        )),
        LiteralKind::Rune => Ok((
            IrType::Rune,
            IrLiteral::Rune(
                literal
                    .cooked
                    .as_deref()
                    .and_then(|value| value.chars().next())
                    .unwrap_or('\0'),
            ),
        )),
        LiteralKind::Integer if expected == Some(&IrType::I64) => {
            let value = parse_integer_i64(&literal.raw)
                .ok_or_else(|| format!("integer literal `{}` does not fit i64", literal.raw))?;
            Ok((IrType::I64, IrLiteral::I64(value)))
        }
        LiteralKind::Integer => {
            let value = parse_integer_i64(&literal.raw)
                .and_then(|value| i32::try_from(value).ok())
                .ok_or_else(|| format!("integer literal `{}` does not fit i32", literal.raw))?;
            Ok((IrType::I32, IrLiteral::I32(value)))
        }
        LiteralKind::Float if expected == Some(&IrType::F32) => {
            parse_float_f32(&literal.raw).map(|value| (IrType::F32, IrLiteral::F32(value)))
        }
        LiteralKind::Float => {
            parse_float_f64(&literal.raw).map(|value| (IrType::F64, IrLiteral::F64(value)))
        }
    }
}

fn parse_integer_i64(value: &str) -> Option<i64> {
    let value = value.replace('_', "");
    if let Some(value) = value.strip_prefix("0x") {
        i64::from_str_radix(value, 16).ok()
    } else if let Some(value) = value.strip_prefix("0b") {
        i64::from_str_radix(value, 2).ok()
    } else {
        value.parse().ok()
    }
}

fn parse_float_f32(value: &str) -> Result<f32, String> {
    let normalized = value.replace('_', "");
    let parsed = normalized
        .parse::<f32>()
        .map_err(|_| format!("float literal `{value}` is invalid for f32"))?;
    parsed
        .is_finite()
        .then_some(parsed)
        .ok_or_else(|| format!("float literal `{value}` does not fit finite f32"))
}

fn parse_float_f64(value: &str) -> Result<f64, String> {
    let normalized = value.replace('_', "");
    let parsed = normalized
        .parse::<f64>()
        .map_err(|_| format!("float literal `{value}` is invalid for f64"))?;
    parsed
        .is_finite()
        .then_some(parsed)
        .ok_or_else(|| format!("float literal `{value}` does not fit finite f64"))
}

fn binary_operator(operator: BinaryOperatorKind) -> BinaryOperator {
    match operator {
        BinaryOperatorKind::Add => BinaryOperator::Add,
        BinaryOperatorKind::Subtract => BinaryOperator::Subtract,
        BinaryOperatorKind::Multiply => BinaryOperator::Multiply,
        BinaryOperatorKind::Divide => BinaryOperator::Divide,
        BinaryOperatorKind::Remainder => BinaryOperator::Remainder,
        BinaryOperatorKind::Equal => BinaryOperator::Equal,
        BinaryOperatorKind::NotEqual => BinaryOperator::NotEqual,
        BinaryOperatorKind::Less => BinaryOperator::Less,
        BinaryOperatorKind::LessEqual => BinaryOperator::LessEqual,
        BinaryOperatorKind::Greater => BinaryOperator::Greater,
        BinaryOperatorKind::GreaterEqual => BinaryOperator::GreaterEqual,
        BinaryOperatorKind::And => BinaryOperator::And,
        BinaryOperatorKind::Or => BinaryOperator::Or,
    }
}

fn binary_result(
    operator: BinaryOperatorKind,
    operand: &IrType,
    definitions: &[Definition],
) -> Option<IrType> {
    match operator {
        BinaryOperatorKind::Equal | BinaryOperatorKind::NotEqual
            if equality_supported(operand, definitions) =>
        {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::Less
        | BinaryOperatorKind::LessEqual
        | BinaryOperatorKind::Greater
        | BinaryOperatorKind::GreaterEqual
            if is_numeric(operand) =>
        {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::And | BinaryOperatorKind::Or if operand == &IrType::Bool => {
            Some(IrType::Bool)
        }
        BinaryOperatorKind::Add if operand == &IrType::String => Some(IrType::String),
        BinaryOperatorKind::Add
        | BinaryOperatorKind::Subtract
        | BinaryOperatorKind::Multiply
        | BinaryOperatorKind::Divide
        | BinaryOperatorKind::Remainder
            if is_numeric(operand) =>
        {
            Some(operand.clone())
        }
        _ => None,
    }
}

fn equality_supported(operand: &IrType, definitions: &[Definition]) -> bool {
    match operand {
        IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune => true,
        IrType::Named(definition) => {
            definitions
                .get(definition.0 as usize)
                .is_some_and(|definition| {
                    matches!(
                        definition.kind,
                        DefinitionKind::Struct | DefinitionKind::Class
                    )
                })
        }
        IrType::Unit
        | IrType::Option(_)
        | IrType::Result(_, _)
        | IrType::Array(_)
        | IrType::Map(_, _)
        | IrType::Tuple(_)
        | IrType::HostRequest(_)
        | IrType::ResourceToken(_)
        | IrType::Snapshot(_)
        | IrType::Buffer(_)
        | IrType::StateHandle(_)
        | IrType::TypeParameter(_) => false,
    }
}

fn constant_i32_expression(
    expression: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, ConstValue>,
) -> Option<i32> {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::I32(value)) => Some(*value),
        TypedExpressionKind::Reference(definition) => match constants.get(definition) {
            Some(ConstValue::I32(value)) => Some(*value),
            _ => None,
        },
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Negate,
            operand,
        } => constant_i32_expression(operand, constants)?.checked_neg(),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            let left = constant_i32_expression(left, constants)?;
            let right = constant_i32_expression(right, constants)?;
            match operator {
                BinaryOperator::Add => left.checked_add(right),
                BinaryOperator::Subtract => left.checked_sub(right),
                BinaryOperator::Multiply => left.checked_mul(right),
                BinaryOperator::Divide => left.checked_div(right),
                BinaryOperator::Remainder => left.checked_rem(right),
                BinaryOperator::Equal
                | BinaryOperator::NotEqual
                | BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual
                | BinaryOperator::And
                | BinaryOperator::Or => None,
            }
        }
        _ => None,
    }
}

fn constant_bool_expression(
    expression: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, ConstValue>,
) -> Option<bool> {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::Bool(value)) => Some(*value),
        TypedExpressionKind::Reference(definition) => match constants.get(definition) {
            Some(ConstValue::Bool(value)) => Some(*value),
            _ => None,
        },
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Not,
            operand,
        } => constant_bool_expression(operand, constants).map(|value| !value),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::And,
            left,
            right,
        } => Some(
            constant_bool_expression(left, constants)?
                && constant_bool_expression(right, constants)?,
        ),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::Or,
            left,
            right,
        } => Some(
            constant_bool_expression(left, constants)?
                || constant_bool_expression(right, constants)?,
        ),
        TypedExpressionKind::Binary {
            operator: BinaryOperator::Equal,
            left,
            right,
        } => match (
            constant_bool_expression(left, constants),
            constant_bool_expression(right, constants),
        ) {
            (Some(left), Some(right)) => Some(left == right),
            _ => Some(
                constant_i32_expression(left, constants)?
                    == constant_i32_expression(right, constants)?,
            ),
        },
        TypedExpressionKind::Binary {
            operator: BinaryOperator::NotEqual,
            left,
            right,
        } => match (
            constant_bool_expression(left, constants),
            constant_bool_expression(right, constants),
        ) {
            (Some(left), Some(right)) => Some(left != right),
            _ => Some(
                constant_i32_expression(left, constants)?
                    != constant_i32_expression(right, constants)?,
            ),
        },
        TypedExpressionKind::Binary {
            operator:
                operator @ (BinaryOperator::Less
                | BinaryOperator::LessEqual
                | BinaryOperator::Greater
                | BinaryOperator::GreaterEqual),
            left,
            right,
        } => {
            let left = constant_i32_expression(left, constants)?;
            let right = constant_i32_expression(right, constants)?;
            Some(match operator {
                BinaryOperator::Less => left < right,
                BinaryOperator::LessEqual => left <= right,
                BinaryOperator::Greater => left > right,
                BinaryOperator::GreaterEqual => left >= right,
                _ => unreachable!("comparison pattern is exhaustive"),
            })
        }
        _ => None,
    }
}

fn map_migration_paths(
    paths: &mut MigrationPaths,
    mut update: impl FnMut(&mut MigrationPathState),
) {
    let mut updated = MigrationPaths::new();
    for mut path in std::mem::take(paths) {
        update(&mut path);
        updated.insert(path);
    }
    *paths = updated;
}

fn record_migration_operation(paths: &mut MigrationPaths, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        if path.finish_count >= 1 {
            path.operation_after_finish
                .get_or_insert((span.start, span.end));
        }
    });
}

fn record_migration_forwarding(paths: &mut MigrationPaths, identity: StableId, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        let count = path.forwarding.entry(identity).or_default();
        if *count >= 1 {
            path.duplicate_forwarding
                .entry(identity)
                .or_insert((span.start, span.end));
        }
        *count = count.saturating_add(1).min(2);
    });
}

fn record_migration_finish(paths: &mut MigrationPaths, span: &SourceRange) {
    map_migration_paths(paths, |path| {
        let unforwarded = path
            .reads
            .iter()
            .filter(|identity| path.forwarding.get(identity).copied().unwrap_or_default() == 0)
            .copied()
            .collect::<Vec<_>>();
        path.unforwarded_at_finish.extend(unforwarded);
        if path.finish_count >= 1 {
            path.duplicate_finish.get_or_insert((span.start, span.end));
        }
        path.finish_count = path.finish_count.saturating_add(1).min(2);
    });
}

fn is_numeric(ty: &IrType) -> bool {
    matches!(ty, IrType::I32 | IrType::I64 | IrType::F32 | IrType::F64)
}

fn is_scalar(ty: &IrType) -> bool {
    matches!(
        ty,
        IrType::Bool
            | IrType::I32
            | IrType::I64
            | IrType::F32
            | IrType::F64
            | IrType::String
            | IrType::Rune
    )
}

fn max_effect(left: IrEffect, right: IrEffect) -> IrEffect {
    if left == IrEffect::Task || right == IrEffect::Task {
        IrEffect::Task
    } else if left == IrEffect::Ordinary || right == IrEffect::Ordinary {
        IrEffect::Ordinary
    } else {
        left
    }
}

fn expression_effect(values: &[TypedExpressionIr]) -> IrEffect {
    values.iter().fold(IrEffect::Immediate, |effect, value| {
        max_effect(effect, value.effect)
    })
}

fn collect_expression_references(
    expression: &TypedExpressionIr,
    output: &mut BTreeSet<DefinitionId>,
) {
    match &expression.kind {
        TypedExpressionKind::Reference(definition) => {
            output.insert(*definition);
        }
        TypedExpressionKind::Unary { operand, .. } => {
            collect_expression_references(operand, output);
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            collect_expression_references(left, output);
            collect_expression_references(right, output);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                collect_expression_references(argument, output);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, value) in fields {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            collect_expression_references(base, output);
            for (_, value) in fields {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_references(payload, output);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            collect_expression_references(base, output);
        }
        TypedExpressionKind::Index { base, index } => {
            collect_expression_references(base, output);
            collect_expression_references(index, output);
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                collect_expression_references(value, output);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            collect_expression_references(value, output);
            for arm in arms {
                collect_expression_references(&arm.value, output);
            }
        }
        TypedExpressionKind::Try(value) | TypedExpressionKind::Await(value) => {
            collect_expression_references(value, output);
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. }
            | MigrationIntrinsicIr::Replace { target: object, .. } => {
                collect_expression_references(object, output);
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                collect_expression_references(object, output);
                collect_expression_references(value, output);
            }
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Yield => {}
    }
}

fn rewrite_expression_references(
    expression: &mut TypedExpressionIr,
    replacements: &BTreeMap<DefinitionId, DefinitionId>,
) {
    match &mut expression.kind {
        TypedExpressionKind::Reference(definition) => {
            if let Some(replacement) = replacements.get(definition) {
                *definition = *replacement;
            }
        }
        TypedExpressionKind::Unary { operand, .. } => {
            rewrite_expression_references(operand, replacements);
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            rewrite_expression_references(left, replacements);
            rewrite_expression_references(right, replacements);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            for argument in arguments {
                rewrite_expression_references(argument, replacements);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, value) in fields {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            rewrite_expression_references(base, replacements);
            for (_, value) in fields {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                rewrite_expression_references(payload, replacements);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            rewrite_expression_references(base, replacements);
        }
        TypedExpressionKind::Index { base, index } => {
            rewrite_expression_references(base, replacements);
            rewrite_expression_references(index, replacements);
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                rewrite_expression_references(value, replacements);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            rewrite_expression_references(value, replacements);
            for arm in arms {
                rewrite_expression_references(&mut arm.value, replacements);
            }
        }
        TypedExpressionKind::Try(value) | TypedExpressionKind::Await(value) => {
            rewrite_expression_references(value, replacements);
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. }
            | MigrationIntrinsicIr::Replace { target: object, .. } => {
                rewrite_expression_references(object, replacements);
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                rewrite_expression_references(object, replacements);
                rewrite_expression_references(value, replacements);
            }
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Yield => {}
    }
}

fn place_type(place: &TypedPlaceIr, definitions: &[Definition]) -> IrType {
    match place {
        TypedPlaceIr::Definition(definition) => definitions[definition.0 as usize].ty.clone(),
        TypedPlaceIr::Field { field, .. } | TypedPlaceIr::StateField { field, .. } => {
            definitions[field.0 as usize].ty.clone()
        }
        TypedPlaceIr::Index { base, .. } => match &base.ty {
            IrType::Array(inner) | IrType::Buffer(inner) => inner.as_ref().clone(),
            IrType::Map(_, value) => value.as_ref().clone(),
            IrType::String => IrType::Rune,
            _ => IrType::Unit,
        },
    }
}

fn restricted_name(operation: RestrictedOperation) -> &'static str {
    match operation {
        RestrictedOperation::Host => "Host",
        RestrictedOperation::Task => "Task",
        RestrictedOperation::Await => "await",
        RestrictedOperation::Yield => "yield",
        RestrictedOperation::Activation => "Activation",
        RestrictedOperation::Migration => "Migration",
        RestrictedOperation::PersistentState => "persistent State",
    }
}

fn definition_fingerprint_payload(
    definition: &Definition,
    signature: Option<&FunctionSignature>,
    constant: Option<&ConstValue>,
    metadata: Option<&TypeMetadata>,
    variant_payloads: &BTreeMap<DefinitionId, Vec<IrType>>,
    definitions: &[Definition],
) -> Vec<u8> {
    let mut payload = Vec::new();
    append_string(&mut payload, definition_kind_name(definition.kind));
    append_string(&mut payload, visibility_name(definition.visibility));
    append_string(&mut payload, effect_name(definition.effect));
    encode_type(&definition.ty, definitions, &mut payload);
    if let Some(signature) = signature {
        append_u32(
            &mut payload,
            u32::try_from(signature.parameter_types.len()).unwrap_or(u32::MAX),
        );
        for parameter in &signature.parameter_types {
            encode_type(parameter, definitions, &mut payload);
        }
        encode_type(&signature.result, definitions, &mut payload);
    }
    if let Some(constant) = constant {
        encode_const(constant, definitions, &mut payload);
    }
    if let Some(metadata) = metadata {
        append_u32(
            &mut payload,
            u32::try_from(metadata.field_order.len()).unwrap_or(u32::MAX),
        );
        for (source_order, field) in metadata.field_order.iter().enumerate() {
            append_u32(
                &mut payload,
                u32::try_from(source_order).unwrap_or(u32::MAX),
            );
            let field = &definitions[field.0 as usize];
            append_string(&mut payload, &field.canonical_identity);
            encode_type(&field.ty, definitions, &mut payload);
        }
        append_u32(
            &mut payload,
            u32::try_from(metadata.variant_order.len()).unwrap_or(u32::MAX),
        );
        for (tag, variant) in metadata.variant_order.iter().enumerate() {
            append_u32(&mut payload, u32::try_from(tag).unwrap_or(u32::MAX));
            append_string(
                &mut payload,
                &definitions[variant.0 as usize].canonical_identity,
            );
            let values = variant_payloads.get(variant).map_or(&[][..], Vec::as_slice);
            append_u32(
                &mut payload,
                u32::try_from(values.len()).unwrap_or(u32::MAX),
            );
            for value in values {
                encode_type(value, definitions, &mut payload);
            }
        }
    }
    payload
}

fn encode_type(ty: &IrType, definitions: &[Definition], output: &mut Vec<u8>) {
    match ty {
        IrType::Unit => output.push(0),
        IrType::Bool => output.push(1),
        IrType::I32 => output.push(2),
        IrType::I64 => output.push(3),
        IrType::F32 => output.push(4),
        IrType::F64 => output.push(5),
        IrType::String => output.push(6),
        IrType::Rune => output.push(7),
        IrType::Named(definition) => {
            output.push(8);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
        }
        IrType::Option(inner) => {
            output.push(9);
            encode_type(inner, definitions, output);
        }
        IrType::Result(ok, error) => {
            output.push(10);
            encode_type(ok, definitions, output);
            encode_type(error, definitions, output);
        }
        IrType::Array(inner) => {
            output.push(11);
            encode_type(inner, definitions, output);
        }
        IrType::Map(key, value) => {
            output.push(12);
            encode_type(key, definitions, output);
            encode_type(value, definitions, output);
        }
        IrType::Tuple(values) => {
            output.push(13);
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_type(value, definitions, output);
            }
        }
        IrType::HostRequest(inner) => {
            output.push(14);
            if let Some(inner) = inner {
                output.push(1);
                encode_type(inner, definitions, output);
            } else {
                output.push(0);
            }
        }
        IrType::ResourceToken(inner) => {
            output.push(15);
            if let Some(inner) = inner {
                output.push(1);
                encode_type(inner, definitions, output);
            } else {
                output.push(0);
            }
        }
        IrType::Snapshot(inner) => {
            output.push(16);
            encode_type(inner, definitions, output);
        }
        IrType::Buffer(inner) => {
            output.push(17);
            encode_type(inner, definitions, output);
        }
        IrType::StateHandle(inner) => {
            output.push(18);
            encode_type(inner, definitions, output);
        }
        IrType::TypeParameter(index) => {
            output.push(19);
            output.extend_from_slice(&index.to_le_bytes());
        }
    }
}

fn encode_const(value: &ConstValue, definitions: &[Definition], output: &mut Vec<u8>) {
    match value {
        ConstValue::Unit => output.push(0),
        ConstValue::Bool(value) => {
            output.push(1);
            output.push(u8::from(*value));
        }
        ConstValue::I32(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::I64(value) => {
            output.push(3);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::F32(value) => {
            output.push(4);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::F64(value) => {
            output.push(5);
            output.extend_from_slice(&value.to_le_bytes());
        }
        ConstValue::String(value) => {
            output.push(6);
            append_string(output, value);
        }
        ConstValue::Rune(value) => {
            output.push(7);
            output.extend_from_slice(&u32::from(*value).to_le_bytes());
        }
        ConstValue::Tuple(values) | ConstValue::Array(values) => {
            output.push(if matches!(value, ConstValue::Tuple(_)) {
                8
            } else {
                9
            });
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_const(value, definitions, output);
            }
        }
        ConstValue::Construct { definition, fields } => {
            output.push(10);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
            append_u32(output, u32::try_from(fields.len()).unwrap_or(u32::MAX));
            for (field, value) in fields {
                append_string(output, &definitions[field.0 as usize].canonical_identity);
                encode_const(value, definitions, output);
            }
        }
        ConstValue::Variant { definition, values } => {
            output.push(11);
            append_string(
                output,
                &definitions[definition.0 as usize].canonical_identity,
            );
            append_u32(output, u32::try_from(values.len()).unwrap_or(u32::MAX));
            for value in values {
                encode_const(value, definitions, output);
            }
        }
    }
}

fn append_string(output: &mut Vec<u8>, value: &str) {
    append_u32(output, u32::try_from(value.len()).unwrap_or(u32::MAX));
    output.extend_from_slice(value.as_bytes());
}

fn append_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

const fn definition_kind_name(kind: DefinitionKind) -> &'static str {
    match kind {
        DefinitionKind::Function => "function",
        DefinitionKind::Task => "task",
        DefinitionKind::Struct => "struct",
        DefinitionKind::Enum => "enum",
        DefinitionKind::Class => "class",
        DefinitionKind::Stateful => "stateful",
        DefinitionKind::Const => "const",
        DefinitionKind::Field => "field",
        DefinitionKind::Variant => "variant",
        DefinitionKind::Parameter => "parameter",
        DefinitionKind::Local => "local",
        DefinitionKind::HostInterface => "host-interface",
        DefinitionKind::HostFunction => "host-function",
        DefinitionKind::StandardLibrary => "standard-library",
    }
}

const fn is_nominal_type_kind(kind: DefinitionKind) -> bool {
    matches!(
        kind,
        DefinitionKind::Struct
            | DefinitionKind::Enum
            | DefinitionKind::Class
            | DefinitionKind::Stateful
            | DefinitionKind::HostInterface
            | DefinitionKind::StandardLibrary
    )
}

const fn effect_name(effect: IrEffect) -> &'static str {
    match effect {
        IrEffect::Ordinary => "ordinary",
        IrEffect::Immediate => "immediate",
        IrEffect::Task => "task",
        IrEffect::Migration => "migration",
        IrEffect::Activation => "activation",
        IrEffect::Cleanup => "cleanup",
    }
}

fn display_type(ty: &IrType, definitions: &[Definition]) -> String {
    match ty {
        IrType::Unit => "unit".into(),
        IrType::Bool => "bool".into(),
        IrType::I32 => "i32".into(),
        IrType::I64 => "i64".into(),
        IrType::F32 => "f32".into(),
        IrType::F64 => "f64".into(),
        IrType::String => "string".into(),
        IrType::Rune => "rune".into(),
        IrType::Named(definition) => definitions.get(definition.0 as usize).map_or_else(
            || format!("<definition {}>", definition.0),
            |value| value.name.clone(),
        ),
        IrType::Option(inner) => format!("Option<{}>", display_type(inner, definitions)),
        IrType::Result(ok, error) => format!(
            "Result<{}, {}>",
            display_type(ok, definitions),
            display_type(error, definitions)
        ),
        IrType::Array(inner) => format!("Array<{}>", display_type(inner, definitions)),
        IrType::Map(key, value) => format!(
            "Map<{}, {}>",
            display_type(key, definitions),
            display_type(value, definitions)
        ),
        IrType::Tuple(values) => format!(
            "({})",
            values
                .iter()
                .map(|value| display_type(value, definitions))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IrType::HostRequest(inner) => inner.as_ref().map_or_else(
            || "HostRequest".into(),
            |inner| format!("HostRequest<{}>", display_type(inner, definitions)),
        ),
        IrType::ResourceToken(inner) => inner.as_ref().map_or_else(
            || "ResourceToken".into(),
            |inner| format!("ResourceToken<{}>", display_type(inner, definitions)),
        ),
        IrType::Snapshot(inner) => format!("Snapshot<{}>", display_type(inner, definitions)),
        IrType::Buffer(inner) => format!("Buffer<{}>", display_type(inner, definitions)),
        IrType::StateHandle(inner) => {
            format!("StateHandle<{}>", display_type(inner, definitions))
        }
        IrType::TypeParameter(index) => format!("<type-parameter {index}>"),
    }
}
