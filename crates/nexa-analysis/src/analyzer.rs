//! Package-scale name resolution, type/effect checking, and Typed IR construction.
//!
//! Analysis deliberately consumes an immutable [`ResolvedBuildInput`]. It never reads the
//! filesystem, reparses a manifest, resolves a path dependency, or invokes the bytecode compiler.
//! Every ordering decision uses canonical package/module/source identities so the same snapshot
//! produces byte-for-byte equivalent semantic records regardless of filesystem or worker order.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock};

use nexa_core::{
    CanonicalSymbolIdentity, StableId, StableSymbolId, StableSymbolRegistry, SymbolKind,
    deterministic_math::{rem_f32 as deterministic_rem_f32, rem_f64 as deterministic_rem_f64},
};
use nexa_diagnostics::{
    ByteRange, Diagnostic, DiagnosticBatch, DiagnosticBatchLimits, ErrorCode, Label,
    RelatedLocation, Severity, SourceIdentity, SourceSnapshotRegistry, TextEditSuggestion,
};
use nexa_syntax::ast::{
    self, AstErrorKind, Attribute, AttributeArgumentClassification, AttributeArgumentKind,
    BinaryOperatorKind, DeclarationKind, ElseBranch, Expression, ExpressionKind, ForBindings,
    ForIterable, InterpolationPart, LiteralKind, Pattern, PatternKind, Statement, StatementKind,
    TypeDeclarationKind, TypeKind, TypeRef, UnaryOperatorKind, UsePathRootKind, Visibility,
    parse_nexa_ast,
};
use nexa_syntax::{TextRange, TextSize};

use crate::ir::{CollectionIterationKindIr, MigrationIntrinsicIr};
use crate::query::{
    linked_input_query_keys, module_header_query_keys, semantic_input_query_keys,
    typed_module_semantic_context,
};
use crate::{
    ArtifactFileId, ArtifactFileTable, BinaryOperator, BuiltinOperationIr, BuiltinVariantIr,
    CompilationLimits, CompilationProfile, DeclarationVisibility, Definition, DefinitionId,
    DefinitionKind, ExportBindingIr, ExternalSourceRangeIr, ExternalSourceSnapshotIr,
    FieldLayoutIr, HostAsyncResultIr, HostBindingIr, HostFieldBindingIr, HostFunctionBindingIr,
    HostNamespaceBindingIr, HostTypeBindingIr, HostTypeLayoutIr, HostVariantBindingIr,
    IrAbandonPolicy, IrCancelPolicy, IrEffect, IrHostFunctionMode, IrLiteral, IrType,
    LifecycleBindingsIr, ModuleGraph, ModuleGraphError, ModuleKey, ModulePath,
    NormalizedPackagePath, PackageId, PackageKind, PackageSemanticMetadata, PackageSourceSet,
    PublicApiFingerprint, QueryDatabase, QueryExecutionReport, QueryKey, ReplCellInput,
    ReplSessionSnapshot, ResolvedBuildInput, ResolvedReference, ResolvedTestInput,
    SemanticFingerprintRecord, SourceKey, SourceRange, SourceRole, SourceSetFingerprint,
    StableSymbolIdentity, StandardFunctionBindingIr, StateFieldIr, StateMetadataIr,
    StateSchemaFingerprint, StateTypeIr, TestDefinitionIr, TypedBlockIr, TypedDeclarationBody,
    TypedDeclarationIr, TypedExpressionIr, TypedExpressionKind, TypedFunctionIr, TypedMatchArmIr,
    TypedModuleIr, TypedPackageIr, TypedPatternIr, TypedPatternKind, TypedPlaceIr,
    TypedStatementIr, TypedTypeLayoutIr, UnaryOperator, VariantLayoutIr, canonical_state_schema,
    external_source_key, public_api_fingerprint, source_set_fingerprint,
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
    Set(Box<Self>),
    Tuple(Vec<Self>),
    Token(Box<Self>),
    Snapshot(Box<Self>),
    Buffer(Box<Self>),
    StateHandle(Box<Self>),
}

impl std::fmt::Display for SurfaceType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unit => formatter.write_str("unit"),
            Self::Bool => formatter.write_str("bool"),
            Self::I32 => formatter.write_str("i32"),
            Self::I64 => formatter.write_str("i64"),
            Self::F32 => formatter.write_str("f32"),
            Self::F64 => formatter.write_str("f64"),
            Self::String => formatter.write_str("string"),
            Self::Rune => formatter.write_str("rune"),
            Self::TypeParameter(name) | Self::Named { name, .. } => formatter.write_str(name),
            Self::Option(inner) => write!(formatter, "Option<{inner}>"),
            Self::Result(ok, error) => write!(formatter, "Result<{ok}, {error}>"),
            Self::Array(inner) => write!(formatter, "Array<{inner}>"),
            Self::Map(key, value) => write!(formatter, "Map<{key}, {value}>"),
            Self::Set(inner) => write!(formatter, "Set<{inner}>"),
            Self::Tuple(values) => {
                formatter.write_str("(")?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{value}")?;
                }
                formatter.write_str(")")
            }
            Self::Token(inner) => write!(formatter, "Token<{inner}>"),
            Self::Snapshot(inner) => write!(formatter, "Snapshot<{inner}>"),
            Self::Buffer(inner) => write!(formatter, "Buffer<{inner}>"),
            Self::StateHandle(inner) => write!(formatter, "StateHandle<{inner}>"),
        }
    }
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
    /// Canonical declaration fingerprint produced by the validated NIDL model.
    ///
    /// This is authoritative ABI metadata and must be propagated verbatim. Consumers must not
    /// attempt to reconstruct it from this surface's individual fields.
    pub declaration_fingerprint: [u8; 32],
    pub import_index: u32,
    pub fuel_cost: u32,
    pub async_result: Option<HostAsyncResultSurface>,
    /// Canonical sorted, duplicate-free capabilities which must all be declared by the package.
    pub required_capabilities: Vec<String>,
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
    pub contract_name: String,
    pub contract_stable_id: nexa_core::StableId,
    pub types: Vec<ExternalTypeSurface>,
    pub functions: Vec<HostFunctionSurface>,
    /// Every legal `nexa {}` entrypoint declared by the contract.
    ///
    /// Presence in this list makes an implementation discoverable and signature-checked. It does
    /// not make the entrypoint mandatory for every package.
    pub nexa_entrypoints: Vec<NexaEntrypointSurface>,
    /// Engine-selected subset which every enabled package must implement.
    pub required_entrypoints: Vec<RequiredEntrypointSurface>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NexaEntrypointSurface {
    pub name: String,
    pub stable_id: nexa_core::StableId,
    pub parameters: Vec<SurfaceType>,
    pub result: SurfaceType,
    pub effect: Option<IrEffect>,
    pub source: Option<ExternalSourceOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequiredEntrypointSurface {
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
    pub contract: DefinitionId,
    pub contract_stable_id: nexa_core::StableId,
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
    virtual_module_path: Option<ModulePath>,
    source: SourceKey,
    role: SourceRole,
    syntax: Arc<nexa_syntax::SyntaxTree>,
    ast: ParsedAst,
    compiler_provided: bool,
}

/// Source modules remain uniquely owned because script lowering may append a
/// synthetic entrypoint. Immutable compiler modules share their parsed AST;
/// `DerefMut` performs copy-on-write if a future mode ever needs to mutate one.
#[derive(Clone)]
enum ParsedAst {
    Owned(ast::NexaAst),
    Shared(Arc<ast::NexaAst>),
}

impl Deref for ParsedAst {
    type Target = ast::NexaAst;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(ast) => ast,
            Self::Shared(ast) => ast,
        }
    }
}

impl DerefMut for ParsedAst {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Owned(ast) => ast,
            Self::Shared(ast) => Arc::make_mut(ast),
        }
    }
}

struct CachedCompilerModule {
    module: ModulePath,
    source: SourceKey,
    syntax: Arc<nexa_syntax::SyntaxTree>,
    ast: Arc<ast::NexaAst>,
}

static CACHED_STANDARD_LIBRARY: OnceLock<Arc<[CachedCompilerModule]>> = OnceLock::new();

fn cached_standard_library() -> &'static [CachedCompilerModule] {
    CACHED_STANDARD_LIBRARY.get_or_init(|| {
        nexa_stdlib::standard_library()
            .modules()
            .iter()
            .map(|descriptor| {
                let syntax = Arc::new(
                    nexa_syntax::parse_nexa(descriptor.source)
                        .expect("embedded standard-library source fits parser limits"),
                );
                let ast = Arc::new(parse_nexa_ast(&syntax));
                CachedCompilerModule {
                    module: ModulePath::new(descriptor.path)
                        .expect("standard-library module path is valid"),
                    source: standard_library_source_key(descriptor),
                    syntax,
                    ast,
                }
            })
            .collect::<Vec<_>>()
            .into()
    })
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
    field_mutability: BTreeMap<DefinitionId, bool>,
    variants: BTreeMap<String, DefinitionId>,
    variant_order: Vec<DefinitionId>,
    variant_fields: BTreeMap<DefinitionId, BTreeMap<String, DefinitionId>>,
    variant_field_order: BTreeMap<DefinitionId, Vec<DefinitionId>>,
    state: Option<StateClassMetadata>,
}

#[derive(Clone)]
struct StateClassMetadata {
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
    Construct {
        definition: DefinitionId,
        fields: Vec<(DefinitionId, Self)>,
    },
    Variant {
        definition: DefinitionId,
        values: Vec<Self>,
    },
    BuiltinVariant {
        variant: BuiltinVariantIr,
        value: Option<Box<Self>>,
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
    repl_snapshot: Option<&'a ReplSessionSnapshot>,
    repl_cell: Option<&'a ReplCellInput>,
    repl_prior_modules: Vec<TypedModuleIr>,
    repl_prior_external_sources: Vec<ExternalSourceSnapshotIr>,
    repl_environment_definition: Option<DefinitionId>,
    repl_new_state_fields: Vec<DefinitionId>,
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
    repl_entry: Option<crate::ReplEntrypointIr>,
    repl_entry_definition: Option<DefinitionId>,
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
    /// Cause of the first unresolved type/name, used to explain downstream suppressed diagnostics.
    poison_cause: Option<Arc<str>>,
    /// Source ranges of unresolved type names keyed by (source, name), for aggregate notes.
    unknown_type_uses: BTreeMap<(SourceKey, String), Vec<ByteRange>>,
    /// Callees already explained by parser-level suggestions (e.g. `name!(` macro shapes); name
    /// resolution must not re-report them as unknown.
    explained_names: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnalysisMode {
    Product,
    Test,
    Script,
    ReplCell,
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

/// Analyze a single-file script and lower executable top-level statements to a synthetic `main`.
///
/// The input must carry [`CompilationProfile::Script`]; keeping the profile in the resolved build
/// fingerprint prevents script and package semantics from sharing an artifact identity.
#[must_use]
pub fn analyze_script(
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
        AnalysisMode::Script,
    )
    .run()
}

/// Analyze one structured REPL cell through the normal frontend and Typed IR.
///
/// Cross-cell staging is owned by the REPL session layer; this entrypoint guarantees that a cell
/// itself uses the same syntax, name/type analysis, verifier-ready IR, and source spans as scripts.
#[must_use]
pub fn analyze_repl_cell(
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
        AnalysisMode::ReplCell,
    )
    .run()
}

pub(crate) fn analyze_repl_cell_with_session<'a>(
    input: &'a ResolvedBuildInput,
    snapshot: &'a ReplSessionSnapshot,
    cell: &'a ReplCellInput,
    environment: &'a AnalysisEnvironment,
    db: &'a mut QueryDatabase,
) -> AnalysisOutcome {
    Analyzer::new(
        input,
        None,
        &input.artifact_files,
        environment,
        db,
        AnalysisMode::ReplCell,
    )
    .with_repl_session(snapshot, cell)
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
            repl_snapshot: None,
            repl_cell: None,
            repl_prior_modules: Vec::new(),
            repl_prior_external_sources: Vec::new(),
            repl_environment_definition: None,
            repl_new_state_fields: Vec::new(),
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
            repl_entry: None,
            repl_entry_definition: None,
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
            poison_cause: None,
            unknown_type_uses: BTreeMap::new(),
            explained_names: BTreeSet::new(),
        }
    }

    fn with_repl_session(
        mut self,
        snapshot: &'a ReplSessionSnapshot,
        cell: &'a ReplCellInput,
    ) -> Self {
        let mut sources = BTreeMap::<SourceIdentity, Arc<str>>::new();
        for (identity, source) in self.diagnostics.sources().iter() {
            sources.insert(identity.clone(), Arc::from(source.text()));
        }
        if let Some(candidate) = snapshot.candidate_ir() {
            for module in candidate.modules() {
                sources
                    .entry(source_identity(&module.source))
                    .or_insert_with(|| Arc::from(module.syntax.source.as_str()));
            }
            for source in candidate.metadata().external_sources.iter() {
                sources
                    .entry(source.identity.clone())
                    .or_insert_with(|| Arc::clone(&source.text));
            }
        }
        let mut builder = SourceSnapshotRegistry::builder();
        for (identity, text) in sources {
            builder
                .insert(identity, text)
                .expect("deduplicated REPL diagnostic source identity");
        }
        let limits = DiagnosticBatchLimits {
            max_diagnostics: self
                .input
                .compilation_options
                .limits
                .diagnostics_per_revision,
            ..DiagnosticBatchLimits::default()
        };
        let existing = self.diagnostics.diagnostics().to_vec();
        self.diagnostics = DiagnosticBatch::new(builder.build(), limits);
        self.diagnostics.extend(existing);
        self.repl_snapshot = Some(snapshot);
        self.repl_cell = Some(cell);
        self
    }

    #[allow(clippy::too_many_lines)]
    fn seed_repl_snapshot(&mut self) {
        let Some(snapshot) = self.repl_snapshot else {
            return;
        };
        let Some(candidate) = snapshot.candidate_ir() else {
            return;
        };

        self.definitions = candidate.definitions().to_vec();
        self.repl_prior_modules = candidate.modules().to_vec();
        self.repl_prior_external_sources = candidate.metadata().external_sources.to_vec();
        self.repl_environment_definition = candidate
            .metadata()
            .state_types
            .iter()
            .find(|state| state.stable_id == crate::repl_environment_symbol())
            .map(|state| state.definition);

        for definition in &self.definitions {
            if let Some(stable) = &definition.stable_symbol {
                let _ = self.stable_registry.insert(stable.canonical.clone());
                self.stable_ids.insert(definition.id, stable.runtime_id);
            }
            if matches!(
                definition.kind,
                DefinitionKind::Function
                    | DefinitionKind::Task
                    | DefinitionKind::Struct
                    | DefinitionKind::Enum
                    | DefinitionKind::Class
                    | DefinitionKind::Const
                    | DefinitionKind::Variant
                    | DefinitionKind::HostContract
                    | DefinitionKind::HostFunction
                    | DefinitionKind::StandardLibrary
            ) {
                self.symbols.insert(
                    (
                        definition.package_id.clone(),
                        definition.module.clone(),
                        definition.name.clone(),
                    ),
                    definition.id,
                );
            }
        }

        for module in &self.repl_prior_modules {
            for declaration in module.declarations.iter() {
                match &declaration.body {
                    TypedDeclarationBody::Function(function) => {
                        let parameter_types = function
                            .parameters
                            .iter()
                            .map(|parameter| self.definitions[parameter.0 as usize].ty.clone())
                            .collect();
                        self.function_signatures.insert(
                            declaration.definition,
                            FunctionSignature {
                                parameters: function.parameters.clone(),
                                parameter_types,
                                result: function.return_type.clone(),
                                effect: function.effect,
                            },
                        );
                    }
                    TypedDeclarationBody::TypeLayout(layout) => {
                        let mut metadata = TypeMetadata {
                            fields: BTreeMap::new(),
                            field_order: Vec::new(),
                            field_mutability: BTreeMap::new(),
                            variants: BTreeMap::new(),
                            variant_order: Vec::new(),
                            variant_fields: BTreeMap::new(),
                            variant_field_order: BTreeMap::new(),
                            state: None,
                        };
                        match layout {
                            TypedTypeLayoutIr::Struct { fields }
                            | TypedTypeLayoutIr::Class { fields, .. } => {
                                for field in fields {
                                    let definition = &self.definitions[field.definition.0 as usize];
                                    metadata
                                        .fields
                                        .insert(definition.name.clone(), field.definition);
                                    metadata.field_order.push(field.definition);
                                    metadata
                                        .field_mutability
                                        .insert(field.definition, field.mutable);
                                    self.members.insert(
                                        (declaration.definition, definition.name.clone()),
                                        field.definition,
                                    );
                                }
                                if let TypedTypeLayoutIr::Class {
                                    state: Some(state), ..
                                } = layout
                                {
                                    metadata.state = Some(StateClassMetadata {
                                        version: state.version,
                                    });
                                }
                            }
                            TypedTypeLayoutIr::Enum { variants } => {
                                for variant in variants {
                                    let definition =
                                        &self.definitions[variant.definition.0 as usize];
                                    metadata
                                        .variants
                                        .insert(definition.name.clone(), variant.definition);
                                    metadata.variant_order.push(variant.definition);
                                    self.members.insert(
                                        (declaration.definition, definition.name.clone()),
                                        variant.definition,
                                    );
                                    self.variant_payloads.insert(
                                        variant.definition,
                                        variant.payload.clone().into_iter().collect(),
                                    );
                                }
                            }
                        }
                        self.type_metadata.insert(declaration.definition, metadata);
                    }
                    TypedDeclarationBody::Const(_) | TypedDeclarationBody::External => {}
                }
            }
        }

        for definition in &self.definitions {
            if definition.kind == DefinitionKind::Field {
                let Some((_, member)) = definition.canonical_identity.split_once("::Field::")
                else {
                    continue;
                };
                let Some((owner, _)) = member.rsplit_once('.') else {
                    continue;
                };
                for metadata in self.type_metadata.values_mut() {
                    let Some(variant) = metadata.variants.get(owner).copied() else {
                        continue;
                    };
                    metadata
                        .variant_fields
                        .entry(variant)
                        .or_default()
                        .insert(definition.name.clone(), definition.id);
                    metadata
                        .variant_field_order
                        .entry(variant)
                        .or_default()
                        .push(definition.id);
                    self.members
                        .insert((variant, definition.name.clone()), definition.id);
                    break;
                }
            }
        }

        self.state_types = candidate
            .metadata()
            .state_types
            .iter()
            .map(|state| AnalyzedStateType {
                definition: state.definition,
                version: state.version,
                stable_id: state.stable_id,
                fields: state
                    .fields
                    .iter()
                    .map(|field| AnalyzedStateField {
                        definition: field.definition,
                        ty: field.ty.clone(),
                        stable_id: field.stable_id,
                    })
                    .collect(),
            })
            .collect();

        for binding in candidate.metadata().standard_functions.iter() {
            let generic = (
                binding
                    .parameters
                    .iter()
                    .map(|ty| surface_type_from_ir(ty, &self.definitions, &binding.type_parameters))
                    .collect::<Option<Vec<_>>>(),
                surface_type_from_ir(&binding.result, &self.definitions, &binding.type_parameters),
            );
            self.external_functions.insert(
                binding.definition,
                ExternalFunctionMetadata {
                    parameters: binding.parameters.clone(),
                    result: binding.result.clone(),
                    effect: self.definitions[binding.definition.0 as usize].effect,
                    host: None,
                    generic: match generic {
                        (Some(parameters), Some(result)) if !binding.type_parameters.is_empty() => {
                            Some((parameters, result))
                        }
                        _ => None,
                    },
                    type_parameters: binding.type_parameters.clone(),
                    intrinsic: Some(binding.intrinsic),
                },
            );
        }
        for definition in &self.definitions {
            if definition.kind == DefinitionKind::StandardLibrary
                && matches!(definition.ty, IrType::Named(id) if id == definition.id)
                && !self.external_functions.contains_key(&definition.id)
            {
                self.builtin_types
                    .insert(definition.name.clone(), definition.id);
            }
        }

        if let Some(host) = candidate.metadata().host_bindings.first() {
            let external_origin = |range: Option<&ExternalSourceRangeIr>| {
                let range = range?;
                let snapshot = candidate
                    .metadata()
                    .external_sources
                    .iter()
                    .find(|snapshot| snapshot.file_id == range.file_id)?;
                Some(ExternalSourceOrigin {
                    identity: range.identity.clone(),
                    text: Arc::clone(&snapshot.text),
                    range: range.range,
                })
            };
            let current_contract = self
                .environment
                .host
                .as_ref()
                .filter(|surface| surface.contract_stable_id == host.contract_stable_id);
            for binding in &host.types {
                let definition = self.definitions[binding.definition.0 as usize].clone();
                let current = current_contract.and_then(|contract| {
                    contract
                        .types
                        .iter()
                        .find(|candidate| candidate.stable_id == Some(binding.stable_id))
                        .cloned()
                });
                let mut metadata = TypeMetadata {
                    fields: BTreeMap::new(),
                    field_order: Vec::new(),
                    field_mutability: BTreeMap::new(),
                    variants: BTreeMap::new(),
                    variant_order: Vec::new(),
                    variant_fields: BTreeMap::new(),
                    variant_field_order: BTreeMap::new(),
                    state: None,
                };
                let (kind, fields, variants) = match &binding.layout {
                    HostTypeLayoutIr::Opaque => (ExternalTypeKind::Opaque, Vec::new(), Vec::new()),
                    HostTypeLayoutIr::Struct { fields } => {
                        let analyzed = fields
                            .iter()
                            .map(|field| {
                                let field_definition =
                                    &self.definitions[field.definition.0 as usize];
                                metadata
                                    .fields
                                    .insert(field_definition.name.clone(), field.definition);
                                metadata.field_order.push(field.definition);
                                metadata.field_mutability.insert(field.definition, false);
                                self.members.insert(
                                    (binding.definition, field_definition.name.clone()),
                                    field.definition,
                                );
                                let surface = current
                                    .as_ref()
                                    .and_then(|ty| {
                                        ty.fields.iter().find(|candidate| {
                                            candidate.stable_id == Some(field.stable_id)
                                        })
                                    })
                                    .cloned()
                                    .unwrap_or_else(|| ExternalFieldSurface {
                                        name: field_definition.name.clone(),
                                        stable_id: Some(field.stable_id),
                                        ty: surface_type_from_ir(
                                            &field.ty,
                                            &self.definitions,
                                            &[],
                                        )
                                        .expect(
                                            "validated Host field types have a concrete surface form",
                                        ),
                                        source: external_origin(field.source.as_ref()),
                                    });
                                (field.definition, surface)
                            })
                            .collect();
                        (ExternalTypeKind::Struct, analyzed, Vec::new())
                    }
                    HostTypeLayoutIr::Enum { variants } => {
                        let analyzed = variants
                            .iter()
                            .map(|variant| {
                                let variant_definition =
                                    &self.definitions[variant.definition.0 as usize];
                                metadata.variants.insert(
                                    variant_definition.name.clone(),
                                    variant.definition,
                                );
                                metadata.variant_order.push(variant.definition);
                                self.members.insert(
                                    (binding.definition, variant_definition.name.clone()),
                                    variant.definition,
                                );
                                let payload = match &variant.payload {
                                    None => Vec::new(),
                                    Some(IrType::Tuple(values)) => values.clone(),
                                    Some(value) => vec![value.clone()],
                                };
                                self.variant_payloads
                                    .insert(variant.definition, payload.clone());
                                let surface = current
                                    .as_ref()
                                    .and_then(|ty| {
                                        ty.variants.iter().find(|candidate| {
                                            candidate.stable_id == Some(variant.stable_id)
                                        })
                                    })
                                    .cloned()
                                    .unwrap_or_else(|| ExternalVariantSurface {
                                        name: variant_definition.name.clone(),
                                        stable_id: Some(variant.stable_id),
                                        payload: payload
                                            .iter()
                                            .map(|ty| {
                                                surface_type_from_ir(ty, &self.definitions, &[])
                                                    .expect(
                                                        "validated Host variant payloads have a concrete surface form",
                                                    )
                                            })
                                            .collect(),
                                        source: external_origin(variant.source.as_ref()),
                                    });
                                (variant.definition, surface)
                            })
                            .collect();
                        (ExternalTypeKind::Enum, Vec::new(), analyzed)
                    }
                };
                self.type_metadata.insert(binding.definition, metadata);
                self.host_types.push(AnalyzedHostType {
                    definition: binding.definition,
                    stable_id: binding.stable_id,
                    kind,
                    source: current
                        .as_ref()
                        .and_then(|surface| surface.source.clone())
                        .or_else(|| external_origin(binding.source.as_ref())),
                    fields,
                    variants,
                });
                self.symbols.insert(
                    (definition.package_id, definition.module, definition.name),
                    binding.definition,
                );
            }
            let functions = host
                .functions
                .iter()
                .map(|function| {
                    let mode = match function.mode {
                        IrHostFunctionMode::Sync => HostFunctionMode::Sync,
                        IrHostFunctionMode::Request => HostFunctionMode::Request,
                    };
                    let surface = current_contract
                        .and_then(|surface| {
                            surface
                            .functions
                            .iter()
                            .find(|candidate| candidate.stable_id == function.stable_id)
                            .cloned()
                        })
                        .unwrap_or_else(|| HostFunctionSurface {
                            name: self.definitions[function.definition.0 as usize]
                                .name
                                .clone(),
                            parameters: function
                                .parameters
                                .iter()
                                .map(|ty| {
                                    surface_type_from_ir(ty, &self.definitions, &[])
                                        .expect(
                                            "validated Host parameters have a concrete surface form",
                                        )
                                })
                                .collect(),
                            result: surface_type_from_ir(
                                &function.result,
                                &self.definitions,
                                &[],
                            )
                            .expect("validated Host results have a concrete surface form"),
                            mode,
                            stable_id: function.stable_id,
                            declaration_fingerprint: function.declaration_fingerprint,
                            import_index: function.import_index,
                            fuel_cost: function.fuel_cost,
                            async_result: function.async_result.as_ref().map(|result| {
                                HostAsyncResultSurface {
                                    result_type: result.result_type,
                                    success: surface_type_from_ir(
                                        &result.success,
                                        &self.definitions,
                                        &[],
                                    )
                                    .expect(
                                        "validated Host async success types have a concrete surface form",
                                    ),
                                    error: surface_type_from_ir(
                                        &result.error,
                                        &self.definitions,
                                        &[],
                                    )
                                    .expect(
                                        "validated Host async error types have a concrete surface form",
                                    ),
                                    cancel_policy: result.cancel_policy,
                                    abandon_policy: result.abandon_policy,
                                    cancel_error: result.cancel_error,
                                    abandon_error: result.abandon_error,
                                }
                            }),
                            required_capabilities: function.required_capabilities.clone(),
                            source: external_origin(function.source.as_ref()),
                        });
                    self.external_functions.insert(
                        function.definition,
                        ExternalFunctionMetadata {
                            parameters: function.parameters.clone(),
                            result: function.result.clone(),
                            effect: self.definitions[function.definition.0 as usize].effect,
                            host: Some((host.contract, surface.clone())),
                            generic: None,
                            type_parameters: Vec::new(),
                            intrinsic: None,
                        },
                    );
                    AnalyzedHostFunction {
                        definition: function.definition,
                        stable_id: function.stable_id,
                        import_index: function.import_index,
                        mode,
                        source: surface.source.as_ref().map(external_source_range),
                    }
                })
                .collect();
            self.host_binding = Some(AnalyzedHostBinding {
                contract: host.contract,
                contract_stable_id: host.contract_stable_id,
                namespaces: host
                    .namespaces
                    .iter()
                    .map(|namespace| {
                        (
                            namespace.package_id.clone(),
                            namespace.module.clone(),
                            namespace.namespace.clone(),
                        )
                    })
                    .collect(),
                functions,
            });
            self.host_namespaces
                .extend(host.namespaces.iter().map(|namespace| {
                    (
                        namespace.package_id.clone(),
                        namespace.module.clone(),
                        namespace.namespace.clone(),
                    )
                }));
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run(mut self) -> AnalysisOutcome {
        if self.mode != AnalysisMode::Test {
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
        self.seed_repl_snapshot();
        self.validate_compilation_profile();
        self.lower_or_reject_top_level_statements();
        self.validate_dependency_graph();
        self.collect_source_declarations();
        let prior_has_external_surface = self.definitions.iter().any(|definition| {
            matches!(
                definition.kind,
                DefinitionKind::StandardLibrary
                    | DefinitionKind::HostContract
                    | DefinitionKind::HostFunction
            )
        });
        if !prior_has_external_surface {
            self.collect_external_declarations();
        } else if self.mode == AnalysisMode::ReplCell {
            self.collect_incremental_host_declarations();
        }
        self.resolve_imports();
        self.resolve_declaration_signatures();
        self.validate_recursive_value_layouts();
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
                    let poisoned = linked_state_types.iter().any(|state| {
                        state
                            .fields
                            .iter()
                            .any(|field| contains_ir_error(&field.ty))
                    });
                    if !poisoned {
                        self.diagnostics.push(Diagnostic::new(
                            ErrorCode::NX2101,
                            Severity::Error,
                            format!("state schema cannot be lowered: {error}"),
                        ));
                    }
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
        if has_errors && self.mode != AnalysisMode::Test {
            self.db
                .invalidate_keys([QueryKey::LinkedArtifact(root_package.clone())]);
        } else if !has_errors && self.mode != AnalysisMode::Test {
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
                AnalysisMode::Script => TypedPackageIr::new_script(
                    self.input.root_manifest.id.clone(),
                    self.db.revision(),
                    self.definitions,
                    typed_modules,
                    metadata,
                ),
                AnalysisMode::ReplCell => TypedPackageIr::new_repl_cell(
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

        let unknown_type_uses = std::mem::take(&mut self.unknown_type_uses);
        aggregate_unknown_type_notes(&mut self.diagnostics, unknown_type_uses);
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
                contract: binding.contract,
                contract_stable_id: binding.contract_stable_id,
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
                            declaration_fingerprint: metadata
                                .host
                                .as_ref()
                                .expect("Host function metadata retains its NIDL surface")
                                .1
                                .declaration_fingerprint,
                            import_index: function.import_index,
                            mode: match function.mode {
                                HostFunctionMode::Sync => IrHostFunctionMode::Sync,
                                HostFunctionMode::Request => IrHostFunctionMode::Request,
                            },
                            parameters: metadata.parameters.clone(),
                            result: metadata.result.clone(),
                            fuel_cost: metadata.host.as_ref().map_or(0, |(_, host)| host.fuel_cost),
                            required_capabilities: metadata
                                .host
                                .as_ref()
                                .map_or_else(Vec::new, |(_, host)| {
                                    host.required_capabilities.clone()
                                }),
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
            repl_entry: self.repl_entry.clone(),
            standard_functions: standard_functions.into(),
            public_api_fingerprint,
            state_schema_fingerprint,
        }
    }

    #[allow(clippy::too_many_lines)]
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
        let base = if self.mode == AnalysisMode::ReplCell {
            let prior_has_compiler_module = self
                .repl_prior_modules
                .iter()
                .any(|module| module.package_id.as_str() == nexa_stdlib::PACKAGE_ID);
            let current_modules = self
                .modules
                .iter()
                .filter(|module| {
                    module.role == SourceRole::Production
                        && !(prior_has_compiler_module && module.compiler_provided)
                })
                .count();
            self.repl_prior_modules
                .len()
                .checked_add(current_modules)
                .and_then(|value| value.checked_add(1))
                .and_then(|value| u32::try_from(value).ok())
                .expect("external REPL source FileIds fit u32")
        } else {
            self.artifact_files
                .files()
                .len()
                .checked_add(self.compiler_file_ids.len())
                .and_then(|value| value.checked_add(1))
                .and_then(|value| u32::try_from(value).ok())
                .expect("external source FileIds fit u32")
        };
        let mut snapshots_by_identity = self
            .repl_prior_external_sources
            .iter()
            .map(|snapshot| (snapshot.identity.clone(), Arc::clone(&snapshot.text)))
            .collect::<BTreeMap<_, _>>();
        for (identity, text) in sources {
            snapshots_by_identity.insert(identity, text);
        }
        let snapshots_by_identity = snapshots_by_identity
            .into_iter()
            .enumerate()
            .map(|(offset, (identity, text))| {
                let file_id = base
                    .checked_add(u32::try_from(offset).expect("external source count fits u32"))
                    .map(ArtifactFileId)
                    .expect("external FileId fits u32");
                (identity, (file_id, text))
            })
            .collect::<BTreeMap<_, _>>();
        let file_ids = snapshots_by_identity
            .iter()
            .map(|(identity, (file_id, _))| (identity.clone(), *file_id))
            .collect();
        let mut snapshots = snapshots_by_identity
            .into_iter()
            .map(|(identity, (file_id, text))| ExternalSourceSnapshotIr {
                file_id,
                identity,
                text,
            })
            .collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.file_id);
        (snapshots, file_ids)
    }

    fn lifecycle_bindings(&self) -> LifecycleBindingsIr {
        self.lifecycle
    }

    fn validate_compilation_profile(&mut self) {
        let profile = self.input.compilation_options.profile;
        let valid = match self.mode {
            AnalysisMode::Product | AnalysisMode::Test => matches!(
                profile,
                CompilationProfile::Package | CompilationProfile::Standalone
            ),
            AnalysisMode::Script => profile == CompilationProfile::Script,
            AnalysisMode::ReplCell => profile == CompilationProfile::ReplCell,
        };
        if !valid {
            let fallback = self.fallback_source_range();
            self.push_source_error(
                ErrorCode::NX2101,
                &fallback.source,
                ByteRange::default(),
                format!(
                    "analysis entrypoint/profile mismatch: mode {:?}, profile {profile:?}",
                    self.mode
                ),
                "construct the resolved input with the matching CompilationProfile",
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lower_or_reject_top_level_statements(&mut self) {
        for index in 0..self.modules.len() {
            let module = self.modules[index].clone();
            let has_executable =
                !module.ast.top_level_statements.is_empty() || module.ast.top_level_tail.is_some();
            let is_executable_root =
                matches!(self.mode, AnalysisMode::Script | AnalysisMode::ReplCell)
                    && module.key.package == self.input.root_manifest.id
                    && !module.compiler_provided;
            let needs_empty_repl_entry = self.mode == AnalysisMode::ReplCell && is_executable_root;
            if !has_executable && !needs_empty_repl_entry {
                continue;
            }
            let eof = TextRange::at(module.syntax.source.len(), 0);
            let mut range = TextRange::new(
                module.ast.top_level_statements.first().map_or_else(
                    || {
                        module
                            .ast
                            .top_level_tail
                            .as_ref()
                            .map_or(eof.start, |tail| tail.range.start)
                    },
                    |statement| statement.range.start,
                ),
                module.ast.top_level_tail.as_ref().map_or_else(
                    || {
                        module
                            .ast
                            .top_level_statements
                            .last()
                            .map_or(eof.end, |statement| statement.range.end)
                    },
                    |tail| tail.range.end,
                ),
            );
            if self.mode == AnalysisMode::ReplCell
                && range.is_empty()
                && !module.syntax.source.is_empty()
            {
                // A declaration-only Cell still gets a synthetic executable entry. Its bytecode
                // and Task wrapper require real source provenance, so bind them to the exact Cell
                // snapshot instead of the zero-width EOF marker used for synthetic identifiers.
                range = TextRange::new(TextSize::ZERO, module.syntax.source.len());
            }
            if !is_executable_root {
                self.push_source_error(
                    ErrorCode::NX1002,
                    &module.source,
                    byte_range(range),
                    "package modules cannot contain executable top-level statements",
                    "move the statements into a function or compile this source as a Script",
                );
                continue;
            }

            let generated_name = self.repl_cell.map_or_else(
                || "cell_1".to_owned(),
                |cell| format!("cell_{}", cell.ordinal),
            );
            let conflicting_name = if self.mode == AnalysisMode::Script {
                "main"
            } else {
                generated_name.as_str()
            };
            let explicit_conflict = module.ast.declarations.iter().find_map(|declaration| {
                let DeclarationKind::Function(function) = &declaration.kind else {
                    return None;
                };
                (function.name.text == conflicting_name).then_some(function.name.range)
            });
            if let Some(conflict_range) = explicit_conflict {
                self.push_source_error(
                    ErrorCode::NX2101,
                    &module.source,
                    byte_range(conflict_range),
                    format!("top-level statements conflict with generated `{conflicting_name}`"),
                    "rename the explicit function or remove the executable top-level statements",
                );
                continue;
            }

            let mut statements = std::mem::take(&mut self.modules[index].ast.top_level_statements);
            let tail = self.modules[index].ast.top_level_tail.take();
            let is_async = statements.iter().any(statement_contains_await)
                || tail
                    .as_ref()
                    .is_some_and(|tail| expression_contains_await(tail));
            let identifier = |text: &str| ast::Identifier {
                text: text.to_owned(),
                range: eof,
            };
            if self.mode == AnalysisMode::ReplCell {
                self.modules[index].ast.declarations.push(ast::Declaration {
                    docs: Vec::new(),
                    attributes: Vec::new(),
                    visibility: Visibility::Private,
                    kind: DeclarationKind::Function(ast::FunctionDeclaration {
                        is_async,
                        name: identifier(&generated_name),
                        parameters: Vec::new(),
                        result: None,
                        body: ast::Block {
                            statements,
                            tail,
                            range,
                        },
                        range,
                    }),
                    range,
                });
                continue;
            }

            if let Some(tail) = tail {
                let tail_range = tail.range;
                statements.push(ast::Statement {
                    kind: StatementKind::Expression(*tail),
                    range: tail_range,
                });
            }
            statements.push(ast::Statement {
                kind: StatementKind::Return(Some(ast::Expression {
                    kind: ExpressionKind::Literal(ast::Literal {
                        kind: LiteralKind::Integer,
                        raw: "0".into(),
                        cooked: None,
                        range: eof,
                    }),
                    range: eof,
                })),
                range: eof,
            });
            let string_type = TypeRef {
                kind: TypeKind::Named(ast::QualifiedName {
                    segments: vec![identifier("string")],
                    range: eof,
                }),
                range: eof,
            };
            let arguments_type = TypeRef {
                kind: TypeKind::Array(Box::new(string_type)),
                range: eof,
            };
            let result_type = TypeRef {
                kind: TypeKind::Named(ast::QualifiedName {
                    segments: vec![identifier("i32")],
                    range: eof,
                }),
                range: eof,
            };
            self.modules[index].ast.declarations.push(ast::Declaration {
                docs: Vec::new(),
                attributes: Vec::new(),
                visibility: Visibility::Private,
                kind: DeclarationKind::Function(ast::FunctionDeclaration {
                    is_async,
                    name: identifier("main"),
                    parameters: vec![ast::Parameter {
                        name: identifier("args"),
                        ty: arguments_type,
                        range: eof,
                    }],
                    result: Some(result_type),
                    body: ast::Block {
                        statements,
                        tail: None,
                        range,
                    },
                    range,
                }),
                range,
            });
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_sources(&mut self) {
        let mut units = self
            .input
            .all_source_sets()
            .chain(self.test_source_set)
            .flat_map(|set| set.units().values())
            .filter(|unit| match self.mode {
                AnalysisMode::Product | AnalysisMode::Script | AnalysisMode::ReplCell => {
                    unit.role == SourceRole::Production
                }
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
                match error.kind {
                    AstErrorKind::LegacyModuleDeclaration { path }
                        if self.input.compilation_options.profile
                            == CompilationProfile::Package =>
                    {
                        let declared = syntax.source.slice(path).unwrap_or("<invalid>");
                        self.push_source_error(
                            ErrorCode::NX2701,
                            &unit.key,
                            byte_range(path),
                            format!(
                                "legacy module declaration `{declared}` is invalid; package source \
                                 path selects module `{expected}`"
                            ),
                            "module declarations were removed; module identity comes from the package-relative source path",
                        );
                    }
                    AstErrorKind::InvalidSyntax
                    | AstErrorKind::LegacyModuleDeclaration { .. }
                    | AstErrorKind::RustMacroInvocation => {
                        if error.kind == AstErrorKind::RustMacroInvocation
                            && let Some(callee) = syntax.source.slice(error.range)
                        {
                            self.explained_names.insert(callee.to_owned());
                        }
                        let mut diagnostic = Diagnostic::new(
                            ErrorCode::NX1002,
                            Severity::Error,
                            error.message.clone(),
                        )
                        .with_label(Label::primary(
                            source_identity(&unit.key),
                            byte_range(error.range),
                            "invalid Nexa syntax",
                        ));
                        if let Some(fix) = &error.fix {
                            diagnostic =
                                diagnostic.with_fix(TextEditSuggestion::message(fix.clone()));
                        }
                        self.diagnostics.push(diagnostic);
                    }
                }
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
                    ByteRange::default(),
                    "more than one source path maps to this module",
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
                virtual_module_path: unit.virtual_module_path().cloned(),
                source: unit.key.clone(),
                role: unit.role,
                syntax,
                ast: ParsedAst::Owned(ast),
                compiler_provided: false,
            });
        }
        self.parse_standard_library_sources();
    }

    fn parse_standard_library_sources(&mut self) {
        let package =
            PackageId::new(nexa_stdlib::PACKAGE_ID).expect("standard-library package ID is valid");
        for cached in cached_standard_library() {
            let module = cached.module.clone();
            let source = cached.source.clone();
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
                            "compiler-provided module `{module}` collides with package `{}`",
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
            let syntax = Arc::clone(&cached.syntax);
            self.db
                .seed_compiler_syntax(source.clone(), Arc::clone(&syntax));
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
            let ast = Arc::clone(&cached.ast);
            for error in &ast.errors {
                self.push_source_error(
                    ErrorCode::NX1002,
                    &source,
                    byte_range(error.range),
                    error.message.clone(),
                    "invalid embedded standard-library AST",
                );
            }
            let index = self.modules.len();
            self.module_indices.insert(key.clone(), index);
            self.modules.push(ParsedModule {
                key,
                virtual_module_path: None,
                source,
                role: SourceRole::Production,
                syntax,
                ast: ParsedAst::Shared(ast),
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
            let prior_has_compiler_module = self
                .repl_prior_modules
                .iter()
                .any(|prior| prior.package_id.as_str() == nexa_stdlib::PACKAGE_ID);
            if prior_has_compiler_module && module.compiler_provided {
                continue;
            }
            for (declaration_index, declaration) in
                module.ast.declarations.clone().into_iter().enumerate()
            {
                let Some((name, kind, mut symbol_kind, mut effect)) =
                    declaration_surface(&declaration)
                else {
                    continue;
                };
                let is_repl_entry = self.mode == AnalysisMode::ReplCell
                    && matches!(&declaration.kind, DeclarationKind::Function(_))
                    && name == format!("cell_{}", self.repl_cell.map_or(1, |cell| cell.ordinal));
                if is_repl_entry {
                    // A Cell is always dispatched through the reserved Function StableId. Its
                    // Task effect controls scheduling but must not change that public identity.
                    symbol_kind = SymbolKind::Function;
                }
                self.validate_declaration_surface(&module, &declaration);
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
                    declaration_index,
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
                if is_repl_entry {
                    self.repl_entry_definition = Some(definition);
                }
                let key = (
                    module.key.package.clone(),
                    module.key.module.clone(),
                    name.clone(),
                );
                if let Some(prior) = self.symbols.insert(key, definition) {
                    let prior_definition = self.definitions[prior.0 as usize].clone();
                    let is_repl_shadow = self.mode == AnalysisMode::ReplCell
                        && prior_definition.span.source != module.source
                        && (matches!(kind, DefinitionKind::Function | DefinitionKind::Task)
                            || (matches!(
                                kind,
                                DefinitionKind::Struct
                                    | DefinitionKind::Enum
                                    | DefinitionKind::Class
                            ) && self
                                .repl_snapshot
                                .is_some_and(|snapshot| snapshot.resolve_type(&name).is_none())));
                    if !is_repl_shadow {
                        let mut diagnostic = Diagnostic::new(
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
                        ));
                        if self.mode == AnalysisMode::ReplCell
                            && matches!(
                                kind,
                                DefinitionKind::Struct
                                    | DefinitionKind::Enum
                                    | DefinitionKind::Class
                            )
                        {
                            diagnostic =
                                diagnostic.with_note("use `:reset` to begin a new type registry");
                        }
                        self.diagnostics.push(diagnostic);
                    }
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
                        let state = (ty.kind == TypeDeclarationKind::Class)
                            .then(|| state_version(&declaration.attributes))
                            .flatten()
                            .map(|version| StateClassMetadata { version });
                        let mut metadata = TypeMetadata {
                            fields: BTreeMap::new(),
                            field_order: Vec::new(),
                            field_mutability: BTreeMap::new(),
                            variants: BTreeMap::new(),
                            variant_order: Vec::new(),
                            variant_fields: BTreeMap::new(),
                            variant_field_order: BTreeMap::new(),
                            state,
                        };
                        for field in &ty.fields {
                            let identity = self.member_canonical_identity(
                                &module,
                                definition,
                                field.name.text.as_str(),
                                &field.attributes,
                                field.range,
                                SymbolKind::Field,
                                metadata.state.is_some(),
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
                            metadata.field_mutability.insert(id, field.mutable);
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
                            if let ast::VariantPayload::Struct(fields) = &variant.payload {
                                let mut named = BTreeMap::new();
                                let mut order = Vec::new();
                                for field in fields {
                                    let field_identity = self.member_canonical_identity(
                                        &module,
                                        id,
                                        field.name.text.as_str(),
                                        &field.attributes,
                                        field.range,
                                        SymbolKind::Field,
                                        false,
                                    );
                                    let field_id = self.allocate_definition(
                                        module.key.package.clone(),
                                        module.key.module.clone(),
                                        field.name.text.clone(),
                                        DefinitionKind::Field,
                                        visibility,
                                        IrType::Unit,
                                        IrEffect::Immediate,
                                        source_range(&module.source, field.range),
                                        field_identity,
                                    );
                                    if named.insert(field.name.text.clone(), field_id).is_some() {
                                        self.push_source_error(
                                            ErrorCode::NX2704,
                                            &module.source,
                                            byte_range(field.range),
                                            format!(
                                                "duplicate variant field `{}`",
                                                field.name.text
                                            ),
                                            "variant field is declared more than once",
                                        );
                                    }
                                    order.push(field_id);
                                    self.members.insert((id, field.name.text.clone()), field_id);
                                }
                                metadata.variant_fields.insert(id, named);
                                metadata.variant_field_order.insert(id, order);
                            }
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

    #[allow(clippy::too_many_lines)]
    fn validate_declaration_surface(
        &mut self,
        module: &ParsedModule,
        declaration: &ast::Declaration,
    ) {
        let mut seen_attributes = BTreeSet::new();
        for attribute in &declaration.attributes {
            if !seen_attributes.insert(attribute.name.text.as_str()) {
                self.push_source_error(
                    ErrorCode::NX2740,
                    &module.source,
                    byte_range(attribute.range),
                    format!("duplicate `@{}` attribute", attribute.name.text),
                    "an attribute may be applied only once",
                );
            }
            let legal = match &declaration.kind {
                DeclarationKind::Function(_) => matches!(
                    attribute.name.text.as_str(),
                    "stable" | "migration" | "activation" | "cleanup" | "immediate" | "test"
                ),
                DeclarationKind::Type(_) => {
                    matches!(attribute.name.text.as_str(), "stable" | "state")
                }
                DeclarationKind::Const(_) => attribute.name.text == "stable",
                DeclarationKind::Error => true,
            };
            if !legal {
                self.push_source_error(
                    ErrorCode::NX2740,
                    &module.source,
                    byte_range(attribute.name.range),
                    format!("unknown or misplaced `@{}` attribute", attribute.name.text),
                    "the attribute is not valid on this declaration",
                );
            }
            self.validate_attribute_arguments(module, attribute);
        }

        match &declaration.kind {
            DeclarationKind::Function(function) => {
                self.validate_snake_name(
                    module,
                    &function.name,
                    "function names must use snake_case",
                );
                for parameter in &function.parameters {
                    self.validate_snake_name(
                        module,
                        &parameter.name,
                        "parameter names must use snake_case",
                    );
                }
                let special = ["migration", "activation", "cleanup", "immediate"]
                    .into_iter()
                    .filter(|name| has_attribute(&declaration.attributes, name))
                    .collect::<Vec<_>>();
                if special.len() > 1 {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(declaration.range),
                        format!("mutually exclusive attributes: {}", special.join(", ")),
                        "a function may have at most one lifecycle/effect attribute",
                    );
                }
                if has_attribute(&declaration.attributes, "test")
                    && has_attribute(&declaration.attributes, "activation")
                {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(declaration.range),
                        "`@test` and `@activation` are mutually exclusive",
                        "a package test cannot be an activation function",
                    );
                }
                if function.is_async && !special.is_empty() {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(declaration.range),
                        format!("`@{}` does not allow `async fn`", special[0]),
                        "special lifecycle/effect functions must be synchronous",
                    );
                }
            }
            DeclarationKind::Type(ty) => {
                self.validate_pascal_name(module, &ty.name, "type names must use PascalCase");
                if self.mode == AnalysisMode::ReplCell
                    && has_attribute(&declaration.attributes, "state")
                {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(declaration.range),
                        "REPL cells cannot declare additional persistent state classes",
                        "top-level `let` and `let mut` are persisted by the reserved REPL environment",
                    );
                }
                if has_attribute(&declaration.attributes, "state")
                    && ty.kind != TypeDeclarationKind::Class
                {
                    self.push_source_error(
                        ErrorCode::NX2740,
                        &module.source,
                        byte_range(declaration.range),
                        "`@state` is only valid on a class",
                        "persistent state is Class metadata",
                    );
                }
                let state_class = ty.kind == TypeDeclarationKind::Class
                    && has_attribute(&declaration.attributes, "state");
                for field in &ty.fields {
                    self.validate_snake_name(
                        module,
                        &field.name,
                        "field names must use snake_case",
                    );
                    if field.mutable && ty.kind != TypeDeclarationKind::Class {
                        self.push_source_error(
                            ErrorCode::NX2501,
                            &module.source,
                            byte_range(field.range),
                            "`mut` fields are only valid in a class",
                            "Struct mutability is determined by its containing place",
                        );
                    }
                    for attribute in &field.attributes {
                        if attribute.name.text != "stable" || !state_class {
                            self.push_source_error(
                                ErrorCode::NX2740,
                                &module.source,
                                byte_range(attribute.range),
                                format!("`@{}` is not valid on this field", attribute.name.text),
                                "only fields of an `@state` class may use `@stable`",
                            );
                        }
                        self.validate_attribute_arguments(module, attribute);
                    }
                }
                for variant in &ty.variants {
                    self.validate_pascal_name(
                        module,
                        &variant.name,
                        "Enum variants must use PascalCase",
                    );
                    if let ast::VariantPayload::Struct(fields) = &variant.payload {
                        for field in fields {
                            self.validate_snake_name(
                                module,
                                &field.name,
                                "Enum payload fields must use snake_case",
                            );
                            for attribute in &field.attributes {
                                self.push_source_error(
                                    ErrorCode::NX2740,
                                    &module.source,
                                    byte_range(attribute.range),
                                    "Enum payload fields do not accept attributes",
                                    "remove the field attribute",
                                );
                            }
                        }
                    }
                }
            }
            DeclarationKind::Const(constant) => {
                if !is_screaming_snake_case(&constant.name.text) {
                    self.push_source_error(
                        ErrorCode::NX2101,
                        &module.source,
                        byte_range(constant.name.range),
                        format!(
                            "const name `{}` must use SCREAMING_SNAKE_CASE",
                            constant.name.text
                        ),
                        "invalid const name",
                    );
                }
            }
            DeclarationKind::Error => {}
        }
    }

    fn validate_attribute_arguments(&mut self, module: &ParsedModule, attribute: &Attribute) {
        // `@stable` owns the long-standing NX2710 diagnostic family and is validated while its
        // canonical symbol identity is allocated.
        if attribute.name.text == "stable" {
            return;
        }
        let valid = match attribute.name.text.as_str() {
            "state" => matches!(
                attribute.arguments.as_slice(),
                [argument]
                    if argument.classification == AttributeArgumentClassification::Named
                        && argument.name.as_ref().is_some_and(|name| name.text == "version")
                        && argument.kind == AttributeArgumentKind::Integer
                        && argument.text.replace('_', "").parse::<u32>().is_ok_and(|value| value > 0)
            ),
            "migration" | "activation" | "cleanup" | "immediate" | "test" => {
                attribute.arguments.is_empty()
            }
            _ => true,
        };
        if !valid
            || attribute.arguments.iter().any(|argument| {
                matches!(
                    argument.classification,
                    AttributeArgumentClassification::Duplicate
                        | AttributeArgumentClassification::Unknown
                )
            })
        {
            let expected = match attribute.name.text.as_str() {
                "state" => "`@state(version = <positive integer>)`",
                "stable" => "`@stable(\"name\")`",
                _ => "an attribute without arguments",
            };
            self.push_source_error(
                ErrorCode::NX2740,
                &module.source,
                byte_range(attribute.range),
                format!("invalid arguments for `@{}`", attribute.name.text),
                format!("expected {expected}"),
            );
        }
    }

    fn validate_snake_name(
        &mut self,
        module: &ParsedModule,
        identifier: &ast::Identifier,
        rule: &str,
    ) {
        if !identifier.text.starts_with('<') && !is_snake_case(&identifier.text) {
            let mut diagnostic = Diagnostic::new(
                ErrorCode::NX2101,
                Severity::Error,
                format!("invalid name `{}`", identifier.text),
            )
            .with_label(Label::primary(
                source_identity(&module.source),
                byte_range(identifier.range),
                rule,
            ));
            if let Some(trimmed) = identifier.text.strip_prefix('_')
                && !trimmed.is_empty()
            {
                diagnostic = diagnostic.with_fix(TextEditSuggestion::replacement(
                    "remove the leading underscore",
                    source_identity(&module.source),
                    byte_range(identifier.range),
                    trimmed,
                ));
            }
            self.diagnostics.push(diagnostic);
        }
    }

    fn validate_pascal_name(
        &mut self,
        module: &ParsedModule,
        identifier: &ast::Identifier,
        rule: &str,
    ) {
        if !identifier.text.starts_with('<') && !is_pascal_case(&identifier.text) {
            self.push_source_error(
                ErrorCode::NX2101,
                &module.source,
                byte_range(identifier.range),
                format!("invalid name `{}`", identifier.text),
                rule,
            );
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

    fn allocate_repl_state_field(
        &mut self,
        module: &ParsedModule,
        name: &str,
        mutable: bool,
        ty: IrType,
        range: TextRange,
    ) -> Option<DefinitionId> {
        let environment = self.repl_environment_definition?;
        let ordinal = self.repl_cell.map_or(1, |cell| cell.ordinal);
        let identity = crate::repl_binding_identity(
            ordinal,
            self.repl_new_state_fields.len(),
            name,
            &crate::ReplBindingKind::Value,
        );
        let canonical_identity = canonical_identity_text(&identity);
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
                            .expect("inserted REPL binding identity exists")
                            .clone(),
                        runtime_id: stable_id,
                    },
                );
            }
            Err(collision) => {
                self.push_source_error(
                    ErrorCode::NX2711,
                    &module.source,
                    byte_range(range),
                    collision.to_string(),
                    "REPL state slot StableId collision",
                );
                return None;
            }
        }
        let definition = self.allocate_definition(
            module.key.package.clone(),
            module.key.module.clone(),
            name.to_owned(),
            DefinitionKind::Field,
            DeclarationVisibility::Private,
            ty.clone(),
            IrEffect::Immediate,
            source_range(&module.source, range),
            canonical_identity,
        );
        let metadata = self
            .type_metadata
            .get_mut(&environment)
            .expect("the formal REPL seed provides environment type metadata");
        metadata.fields.insert(name.to_owned(), definition);
        metadata.field_order.push(definition);
        metadata.field_mutability.insert(definition, mutable);
        self.members
            .insert((environment, name.to_owned()), definition);
        let stable_id = self
            .stable_ids
            .get(&definition)
            .copied()
            .expect("the REPL field identity was registered");
        let state = self
            .state_types
            .iter_mut()
            .find(|state| state.definition == environment)
            .expect("the formal REPL seed provides environment state metadata");
        state.fields.push(AnalyzedStateField {
            definition,
            ty,
            stable_id,
        });
        self.repl_new_state_fields.push(definition);
        Some(definition)
    }

    fn is_repl_state_field(&self, definition: DefinitionId) -> bool {
        let Some(environment) = self.repl_environment_definition else {
            return false;
        };
        self.type_metadata
            .get(&environment)
            .is_some_and(|metadata| metadata.field_order.contains(&definition))
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_arguments)]
    fn canonical_identity(
        &mut self,
        module: &ParsedModule,
        declaration: &ast::Declaration,
        kind: SymbolKind,
        name: &str,
        range: TextRange,
        allow_stable: bool,
        declaration_index: usize,
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
            Ok(None)
                if self.mode == AnalysisMode::ReplCell
                    && !module.compiler_provided
                    && kind == SymbolKind::Function
                    && name
                        != format!("cell_{}", self.repl_cell.map_or(1, |cell| cell.ordinal)) =>
            {
                crate::repl_binding_identity(
                    self.repl_cell.map_or(1, |cell| cell.ordinal),
                    declaration_index,
                    name,
                    &crate::ReplBindingKind::Function {
                        parameters: Arc::from([]),
                        effect: IrEffect::Ordinary,
                    },
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
                    "@stable is only valid on fields of an @state Class",
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
                        field_mutability: BTreeMap::new(),
                        variants: BTreeMap::new(),
                        variant_order: Vec::new(),
                        variant_fields: BTreeMap::new(),
                        variant_field_order: BTreeMap::new(),
                        state: None,
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
            let contract = self.allocate_definition(
                root.clone(),
                host_module.clone(),
                host.contract_name.clone(),
                DefinitionKind::HostContract,
                DeclarationVisibility::Public,
                IrType::Unit,
                IrEffect::Immediate,
                source,
                format!("host::contract::{}", host.contract_name),
            );
            self.definitions[contract.0 as usize].ty = IrType::Named(contract);
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
                            "Contract nominal types must have a closed ABI",
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
                    DefinitionKind::HostContract,
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
                    field_mutability: BTreeMap::new(),
                    variants: BTreeMap::new(),
                    variant_order: Vec::new(),
                    variant_fields: BTreeMap::new(),
                    variant_field_order: BTreeMap::new(),
                    state: None,
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
                                "Contract fields require a stable ID",
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
                    metadata.field_mutability.insert(field_definition, false);
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
                                "Contract variants require a stable ID",
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
                contract,
                contract_stable_id: host.contract_stable_id,
                namespaces: Vec::new(),
                functions: Vec::new(),
            };
            for mut function in functions {
                function.required_capabilities.sort();
                function.required_capabilities.dedup();
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
                pending.push(Pending::HostFunction(id, contract, Box::new(function)));
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
                Pending::HostFunction(id, contract, function) => {
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
                            host: Some((contract, *function)),
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

    #[allow(clippy::too_many_lines)]
    fn collect_incremental_host_declarations(&mut self) {
        let Some(mut host) = self.environment.host.clone() else {
            return;
        };
        let root = self.input.root_manifest.id.clone();
        let fallback = self.fallback_source_range();
        let host_module = ModulePath::new("host").expect("host is a valid reserved module");

        let contract = if let Some(binding) = self.host_binding.as_ref() {
            if binding.contract_stable_id != host.contract_stable_id {
                self.diagnostics.push(
                    Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        "a cumulative REPL session cannot replace its Host contract",
                    )
                    .with_note(format!(
                        "committed contract: {}, offered contract: {}",
                        binding.contract_stable_id, host.contract_stable_id
                    ))
                    .with_note("use `:reset` before selecting a different Host contract"),
                );
                return;
            }
            binding.contract
        } else {
            let source = host
                .source
                .as_ref()
                .map_or_else(|| fallback.clone(), external_source_range);
            let definition = self.allocate_definition(
                root.clone(),
                host_module.clone(),
                host.contract_name.clone(),
                DefinitionKind::HostContract,
                DeclarationVisibility::Public,
                IrType::Unit,
                IrEffect::Immediate,
                source,
                format!("host::contract::{}", host.contract_name),
            );
            self.definitions[definition.0 as usize].ty = IrType::Named(definition);
            self.host_binding = Some(AnalyzedHostBinding {
                contract: definition,
                contract_stable_id: host.contract_stable_id,
                namespaces: Vec::new(),
                functions: Vec::new(),
            });
            definition
        };

        host.types.sort();
        let mut new_type_definitions = Vec::new();
        for host_type in host.types {
            let Some(stable_id) = host_type.stable_id else {
                self.diagnostics.push(Diagnostic::new(
                    ErrorCode::NX2101,
                    Severity::Error,
                    format!("Host type `{}` is missing a stable ABI ID", host_type.name),
                ));
                continue;
            };
            if let Some(existing_index) = self
                .host_types
                .iter()
                .position(|existing| existing.stable_id == stable_id)
            {
                let existing = &mut self.host_types[existing_index];
                if existing.kind != host_type.kind
                    || self.definitions[existing.definition.0 as usize].name != host_type.name
                {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host type StableId {stable_id} changed name or layout kind in a cumulative REPL session"
                        ),
                    ));
                    continue;
                }
                let prior_fields = existing
                    .fields
                    .iter()
                    .filter_map(|(_, field)| field.stable_id)
                    .collect::<BTreeSet<_>>();
                let current_fields = host_type
                    .fields
                    .iter()
                    .filter_map(|field| field.stable_id)
                    .collect::<BTreeSet<_>>();
                let prior_variants = existing
                    .variants
                    .iter()
                    .filter_map(|(_, variant)| variant.stable_id)
                    .collect::<BTreeSet<_>>();
                let current_variants = host_type
                    .variants
                    .iter()
                    .filter_map(|variant| variant.stable_id)
                    .collect::<BTreeSet<_>>();
                if prior_fields != current_fields || prior_variants != current_variants {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host type StableId {stable_id} changed member ABI in a cumulative REPL session"
                        ),
                    ));
                    continue;
                }
                existing.source = host_type.source.clone().or(existing.source.clone());
                for (definition, prior) in &mut existing.fields {
                    if let Some(current) = host_type
                        .fields
                        .iter()
                        .find(|field| field.stable_id == prior.stable_id)
                    {
                        *prior = current.clone();
                        self.definitions[definition.0 as usize].span = current
                            .source
                            .as_ref()
                            .map_or_else(|| fallback.clone(), external_source_range);
                    }
                }
                for (definition, prior) in &mut existing.variants {
                    if let Some(current) = host_type
                        .variants
                        .iter()
                        .find(|variant| variant.stable_id == prior.stable_id)
                    {
                        *prior = current.clone();
                        self.definitions[definition.0 as usize].span = current
                            .source
                            .as_ref()
                            .map_or_else(|| fallback.clone(), external_source_range);
                    }
                }
                continue;
            }
            if !host_type.type_parameters.is_empty() {
                self.diagnostics.push(Diagnostic::new(
                    ErrorCode::NX2101,
                    Severity::Error,
                    format!(
                        "Host type `{}` cannot declare Nexa type parameters",
                        host_type.name
                    ),
                ));
            }
            let definition = self.allocate_definition(
                root.clone(),
                host_module.clone(),
                host_type.name.clone(),
                DefinitionKind::HostContract,
                DeclarationVisibility::Public,
                IrType::Unit,
                IrEffect::Immediate,
                host_type
                    .source
                    .as_ref()
                    .map_or_else(|| fallback.clone(), external_source_range),
                format!("host::type::{}", host_type.name),
            );
            self.definitions[definition.0 as usize].ty = IrType::Named(definition);
            if self
                .symbols
                .insert(
                    (root.clone(), host_module.clone(), host_type.name.clone()),
                    definition,
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
                field_mutability: BTreeMap::new(),
                variants: BTreeMap::new(),
                variant_order: Vec::new(),
                variant_fields: BTreeMap::new(),
                variant_field_order: BTreeMap::new(),
                state: None,
            };
            let mut analyzed_fields = Vec::new();
            for field in host_type.fields {
                let Some(field_stable_id) = field.stable_id else {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host field `{}.{}` is missing a stable ABI ID",
                            host_type.name, field.name
                        ),
                    ));
                    continue;
                };
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
                    format!(
                        "host::type::{}::field::{}::{field_stable_id}",
                        host_type.name, field.name
                    ),
                );
                metadata.fields.insert(field.name.clone(), field_definition);
                metadata.field_order.push(field_definition);
                metadata.field_mutability.insert(field_definition, false);
                self.members
                    .insert((definition, field.name.clone()), field_definition);
                analyzed_fields.push((field_definition, field));
            }
            let mut analyzed_variants = Vec::new();
            for variant in host_type.variants {
                let Some(variant_stable_id) = variant.stable_id else {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host variant `{}.{}` is missing a stable ABI ID",
                            host_type.name, variant.name
                        ),
                    ));
                    continue;
                };
                let variant_definition = self.allocate_definition(
                    root.clone(),
                    host_module.clone(),
                    variant.name.clone(),
                    DefinitionKind::Variant,
                    DeclarationVisibility::Public,
                    IrType::Named(definition),
                    IrEffect::Immediate,
                    variant
                        .source
                        .as_ref()
                        .map_or_else(|| fallback.clone(), external_source_range),
                    format!(
                        "host::type::{}::variant::{}::{variant_stable_id}",
                        host_type.name, variant.name
                    ),
                );
                metadata
                    .variants
                    .insert(variant.name.clone(), variant_definition);
                metadata.variant_order.push(variant_definition);
                self.members
                    .insert((definition, variant.name.clone()), variant_definition);
                let key = (root.clone(), host_module.clone(), variant.name.clone());
                if let std::collections::btree_map::Entry::Vacant(entry) = self.symbols.entry(key) {
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
            self.type_metadata.insert(definition, metadata);
            self.host_types.push(AnalyzedHostType {
                definition,
                stable_id,
                kind: host_type.kind,
                source: host_type.source,
                fields: analyzed_fields,
                variants: analyzed_variants,
            });
            new_type_definitions.push(definition);
        }

        let new_type_set = new_type_definitions.into_iter().collect::<BTreeSet<_>>();
        for host_type in self.host_types.clone() {
            if !new_type_set.contains(&host_type.definition) {
                continue;
            }
            for (definition, field) in host_type.fields {
                self.definitions[definition.0 as usize].ty = self.resolve_surface_type(&field.ty);
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

        host.functions.sort_by(|left, right| {
            (left.import_index, left.stable_id, &left.name).cmp(&(
                right.import_index,
                right.stable_id,
                &right.name,
            ))
        });
        for mut function in host.functions {
            function.required_capabilities.sort();
            function.required_capabilities.dedup();
            if let Some(existing) = self
                .host_binding
                .as_ref()
                .and_then(|binding| {
                    binding
                        .functions
                        .iter()
                        .find(|existing| existing.stable_id == function.stable_id)
                })
                .cloned()
            {
                let parameters = function
                    .parameters
                    .iter()
                    .map(|ty| self.resolve_surface_type(ty))
                    .collect::<Vec<_>>();
                let result = match function.mode {
                    HostFunctionMode::Sync => self.resolve_surface_type(&function.result),
                    HostFunctionMode::Request => match function.async_result.as_ref() {
                        None => {
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
                        Some(result) => IrType::Result(
                            Box::new(self.resolve_surface_type(&result.success)),
                            Box::new(self.resolve_surface_type(&result.error)),
                        ),
                    },
                };
                let Some(metadata) = self.external_functions.get_mut(&existing.definition) else {
                    continue;
                };
                if metadata.parameters != parameters
                    || metadata.result != result
                    || existing.mode != function.mode
                    || existing.import_index != function.import_index
                    || metadata
                        .host
                        .as_ref()
                        .map(|(_, previous)| previous.declaration_fingerprint)
                        != Some(function.declaration_fingerprint)
                {
                    self.diagnostics.push(Diagnostic::new(
                        ErrorCode::NX2101,
                        Severity::Error,
                        format!(
                            "Host function StableId {} changed ABI in a cumulative REPL session",
                            function.stable_id
                        ),
                    ));
                    continue;
                }
                metadata.host = Some((contract, function));
                continue;
            }
            let definition = self.allocate_definition(
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
                function
                    .source
                    .as_ref()
                    .map_or_else(|| fallback.clone(), external_source_range),
                format!("host::function::{}", function.name),
            );
            if self
                .symbols
                .insert(
                    (root.clone(), host_module.clone(), function.name.clone()),
                    definition,
                )
                .is_some()
            {
                self.diagnostics.push(Diagnostic::new(
                    ErrorCode::NX2704,
                    Severity::Error,
                    format!("duplicate Host function `{}`", function.name),
                ));
            }
            let parameters = function
                .parameters
                .iter()
                .map(|ty| self.resolve_surface_type(ty))
                .collect::<Vec<_>>();
            let result = match function.mode {
                HostFunctionMode::Sync => self.resolve_surface_type(&function.result),
                HostFunctionMode::Request => match function.async_result.as_ref() {
                    None => {
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
                    Some(result) => IrType::Result(
                        Box::new(self.resolve_surface_type(&result.success)),
                        Box::new(self.resolve_surface_type(&result.error)),
                    ),
                },
            };
            self.definitions[definition.0 as usize].ty = result.clone();
            self.external_functions.insert(
                definition,
                ExternalFunctionMetadata {
                    parameters,
                    result,
                    effect: self.definitions[definition.0 as usize].effect,
                    host: Some((contract, function.clone())),
                    generic: None,
                    type_parameters: Vec::new(),
                    intrinsic: None,
                },
            );
            self.host_binding
                .as_mut()
                .expect("incremental Host contract binding exists")
                .functions
                .push(AnalyzedHostFunction {
                    definition,
                    stable_id: function.stable_id,
                    import_index: function.import_index,
                    mode: function.mode,
                    source: function.source.as_ref().map(external_source_range),
                });
        }
        self.host_types.sort_by_key(|host_type| host_type.stable_id);
        if let Some(binding) = &mut self.host_binding {
            binding
                .functions
                .sort_by_key(|function| (function.import_index, function.stable_id));
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
            SurfaceType::Set(inner) => IrType::Set(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            )),
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.resolve_standard_surface_type(value, type_parameters))
                    .collect(),
            ),
            SurfaceType::Token(inner) => IrType::ResourceToken(Some(Box::new(
                self.resolve_standard_surface_type(inner, type_parameters),
            ))),
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
                        "external nominal type `{module}::{name}` is ambiguous across packages"
                    )),
                    (None, _) => self.unresolved_surface_type(format!(
                        "unknown external nominal type `{module}::{name}`"
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
            SurfaceType::Set(inner) => IrType::Set(Box::new(self.resolve_surface_type(inner))),
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.resolve_surface_type(value))
                    .collect(),
            ),
            SurfaceType::Token(inner) => {
                IrType::ResourceToken(Some(Box::new(self.resolve_surface_type(inner))))
            }
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
            SurfaceType::TypeParameter(name) => bindings.get(name).cloned().unwrap_or_else(|| {
                self.unresolved_surface_type(format!(
                    "unbound external type parameter `{name}` during call-site instantiation"
                ))
            }),
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
            SurfaceType::Set(inner) => {
                IrType::Set(Box::new(self.instantiate_surface_type(inner, bindings)))
            }
            SurfaceType::Tuple(values) => IrType::Tuple(
                values
                    .iter()
                    .map(|value| self.instantiate_surface_type(value, bindings))
                    .collect(),
            ),
            SurfaceType::Token(inner) => IrType::ResourceToken(Some(Box::new(
                self.instantiate_surface_type(inner, bindings),
            ))),
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
        let mut source_edge_ranges =
            BTreeMap::<(SourceModuleKey, SourceModuleKey), (SourceKey, ByteRange)>::new();
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
            for usage in &module.ast.uses {
                let text = use_path_text(usage);
                let path_range = use_path_range(usage);
                let local_alias = usage
                    .alias
                    .as_ref()
                    .or_else(|| usage.segments.last())
                    .map(|alias| (alias.text.clone(), alias.range));
                let Some((local_alias, local_alias_range)) = local_alias else {
                    continue;
                };
                let target = self.resolve_use_target(&module, usage);
                let Some(target) = target else {
                    self.push_source_error(
                        ErrorCode::NX2703,
                        &module.source,
                        byte_range(path_range),
                        format!("unknown use path `{text}`"),
                        "the use path must name a source module, dependency alias, Host contract, or standard module",
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
                        byte_range(usage.range),
                        format!("too many use declarations in {}", module.key.module),
                        format!(
                            "a module may resolve at most {} use declarations",
                            self.input.compilation_options.limits.imports_per_module
                        ),
                    );
                }
                total_import_edges = total_import_edges.saturating_add(1);
                if total_import_edges > self.input.compilation_options.limits.module_edges {
                    self.push_source_error(
                        ErrorCode::NX2702,
                        &module.source,
                        byte_range(usage.range),
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
                                byte_range(usage.range),
                                "production modules cannot use test modules",
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
                                    byte_range(usage.range),
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
                        source_edge_ranges.insert(
                            (module.key.clone(), target.clone()),
                            (module.source.clone(), byte_range(path_range)),
                        );
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
                        source_edge_ranges
                            .get(&(
                                SourceModuleKey {
                                    package: package.clone(),
                                    module: from.clone(),
                                },
                                SourceModuleKey {
                                    package: package.clone(),
                                    module: to.clone(),
                                },
                            ))
                            .map(|(source, range)| {
                                (source.clone(), *range, format!("`{from}` uses `{to}`"))
                            })
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

    fn resolve_use_target(
        &mut self,
        module: &ParsedModule,
        usage: &ast::UseDeclaration,
    ) -> Option<ImportTarget> {
        let segments = usage
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>();
        if segments.is_empty() {
            return None;
        }

        if usage.root.kind == UsePathRootKind::Host {
            let host = self.environment.host.as_ref()?;
            let expected = snake_case_name(&host.contract_name);
            return (segments.as_slice() == [expected.as_str()]).then_some(ImportTarget::Host);
        }

        let source_target = |package: PackageId, path: String| {
            let module = ModulePath::new(path).ok()?;
            let target = SourceModuleKey { package, module };
            self.module_indices.contains_key(&target).then_some(target)
        };

        let target = match usage.root.kind {
            UsePathRootKind::Package => {
                source_target(module.key.package.clone(), segments.join("."))
            }
            UsePathRootKind::Self_ => {
                let suffix = segments.join(".");
                source_target(
                    module.key.package.clone(),
                    format!("{}.{}", module.key.module, suffix),
                )
            }
            UsePathRootKind::Super => {
                let (parent, _) = module.key.module.as_str().rsplit_once('.')?;
                source_target(
                    module.key.package.clone(),
                    format!("{parent}.{}", segments.join(".")),
                )
            }
            UsePathRootKind::Std => source_target(
                PackageId::new(nexa_stdlib::PACKAGE_ID)
                    .expect("standard-library package ID is valid"),
                format!("std.{}", segments.join(".")),
            ),
            UsePathRootKind::Dependency => {
                let dependency = self
                    .input
                    .dependency_graph
                    .dependencies_of(&module.key.package)
                    .find(|edge| edge.alias.as_str() == usage.root.name.text);
                dependency.and_then(|edge| source_target(edge.to.clone(), segments.join(".")))
            }
            UsePathRootKind::Host => unreachable!("Host use was resolved above"),
        };
        if let Some(target) = target {
            return Some(ImportTarget::Source(target));
        }

        // Compiler-provided static modules use the same explicit root spelling as their canonical
        // module path. They are considered only when no source/dependency module matched.
        let static_path = std::iter::once(usage.root.name.text.as_str())
            .chain(segments.iter().copied())
            .collect::<Vec<_>>()
            .join(".");
        let static_module = ModulePath::new(static_path).ok()?;
        self.environment
            .static_modules
            .iter()
            .any(|surface| surface.module == static_module)
            .then_some(ImportTarget::Static(static_module))
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
                    let effect = function_semantic_effect(&record.declaration, function);
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
                        let payload_syntax = match &variant.payload {
                            ast::VariantPayload::Unit => Vec::new(),
                            ast::VariantPayload::Tuple(values) => values.iter().collect::<Vec<_>>(),
                            ast::VariantPayload::Struct(fields) => {
                                fields.iter().map(|field| &field.ty).collect::<Vec<_>>()
                            }
                        };
                        let payload = payload_syntax
                            .into_iter()
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
                        if let ast::VariantPayload::Struct(fields) = &variant.payload {
                            for (field, resolved) in fields.iter().zip(payload) {
                                if let Some(field_definition) = metadata
                                    .variant_fields
                                    .get(&definition)
                                    .and_then(|named| named.get(&field.name.text))
                                    .copied()
                                {
                                    self.definitions[field_definition.0 as usize].ty = resolved;
                                }
                            }
                        }
                    }
                    if let Some(state) = metadata.state {
                        let stable_id = self.stable_ids.get(&record.definition).copied();
                        let fields = metadata
                            .field_order
                            .iter()
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
                                version: state.version,
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

    /// Reject nominal value layouts whose size would require implicit recursive boxing.
    ///
    /// `Struct` and `Enum` are inline values. `Option`, `Result`, and tuples preserve that inline
    /// relationship, while `Class` and container/runtime handles terminate an inline-layout path.
    fn validate_recursive_value_layouts(&mut self) {
        let mut graph = BTreeMap::<DefinitionId, Vec<InlineLayoutEdge>>::new();

        for (&owner, metadata) in &self.type_metadata {
            let Some(definition) = self.definitions.get(owner.0 as usize) else {
                continue;
            };
            if !matches!(
                definition.kind,
                DefinitionKind::Struct | DefinitionKind::Enum
            ) {
                continue;
            }

            let mut edges = Vec::new();
            match definition.kind {
                DefinitionKind::Struct => {
                    for field in &metadata.field_order {
                        let Some(field_definition) = self.definitions.get(field.0 as usize) else {
                            continue;
                        };
                        collect_inline_value_targets(
                            &field_definition.ty,
                            &self.definitions,
                            &mut |target| {
                                edges.push(InlineLayoutEdge {
                                    target,
                                    span: field_definition.span.clone(),
                                });
                            },
                        );
                    }
                }
                DefinitionKind::Enum => {
                    for variant in &metadata.variant_order {
                        let Some(variant_definition) = self.definitions.get(variant.0 as usize)
                        else {
                            continue;
                        };
                        for payload in self.variant_payloads.get(variant).into_iter().flatten() {
                            collect_inline_value_targets(
                                payload,
                                &self.definitions,
                                &mut |target| {
                                    edges.push(InlineLayoutEdge {
                                        target,
                                        span: variant_definition.span.clone(),
                                    });
                                },
                            );
                        }
                    }
                }
                _ => unreachable!("only inline nominal values enter the layout graph"),
            }

            edges.sort_by(|left, right| {
                (
                    left.target,
                    left.span.source.clone(),
                    left.span.start,
                    left.span.end,
                )
                    .cmp(&(
                        right.target,
                        right.span.source.clone(),
                        right.span.start,
                        right.span.end,
                    ))
            });
            edges.dedup_by(|left, right| {
                left.target == right.target
                    && left.span.source == right.span.source
                    && left.span.start == right.span.start
                    && left.span.end == right.span.end
            });
            graph.insert(owner, edges);
        }

        let mut state = BTreeMap::<DefinitionId, InlineLayoutVisit>::new();
        let mut stack = Vec::<DefinitionId>::new();
        let mut reported = BTreeSet::<Vec<DefinitionId>>::new();
        let nodes = graph.keys().copied().collect::<Vec<_>>();
        for node in nodes {
            self.visit_inline_layout(node, &graph, &mut state, &mut stack, &mut reported);
        }
    }

    fn visit_inline_layout(
        &mut self,
        node: DefinitionId,
        graph: &BTreeMap<DefinitionId, Vec<InlineLayoutEdge>>,
        state: &mut BTreeMap<DefinitionId, InlineLayoutVisit>,
        stack: &mut Vec<DefinitionId>,
        reported: &mut BTreeSet<Vec<DefinitionId>>,
    ) {
        match state.get(&node) {
            Some(InlineLayoutVisit::Complete | InlineLayoutVisit::Visiting) => return,
            None => {}
        }
        state.insert(node, InlineLayoutVisit::Visiting);
        stack.push(node);

        for edge in graph.get(&node).into_iter().flatten() {
            match state.get(&edge.target).copied() {
                Some(InlineLayoutVisit::Visiting) => {
                    let Some(cycle_start) =
                        stack.iter().position(|candidate| *candidate == edge.target)
                    else {
                        continue;
                    };
                    let mut cycle = stack[cycle_start..].to_vec();
                    cycle.push(edge.target);
                    if reported.insert(canonical_inline_cycle(&cycle)) {
                        let names = cycle
                            .iter()
                            .filter_map(|definition| {
                                self.definitions
                                    .get(definition.0 as usize)
                                    .map(|definition| definition.name.as_str())
                            })
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        self.diagnostics.push(
                            Diagnostic::new(
                                ErrorCode::NX2101,
                                Severity::Error,
                                format!("recursive inline value layout: {names}"),
                            )
                            .with_label(Label::primary(
                                source_identity(&edge.span.source),
                                range_from_source(&edge.span),
                                "this payload closes an inline value-layout cycle",
                            ))
                            .with_note(
                                "use a Class node to break the recursive inline value layout",
                            ),
                        );
                    }
                }
                Some(InlineLayoutVisit::Complete) => {}
                None => {
                    self.visit_inline_layout(edge.target, graph, state, stack, reported);
                }
            }
        }

        let popped = stack.pop();
        debug_assert_eq!(popped, Some(node));
        state.insert(node, InlineLayoutVisit::Complete);
    }

    #[allow(clippy::too_many_lines)]
    fn resolve_type_ref(&mut self, module: &ParsedModule, ty: &TypeRef) -> IrType {
        match &ty.kind {
            TypeKind::Named(name) => match name.text().as_str() {
                "HostRequest" => {
                    self.push_source_error(
                        ErrorCode::NX2101,
                        &module.source,
                        byte_range(ty.range),
                        "`HostRequest` is runtime-only and cannot be named or stored in Nexa source",
                        "call an async Host function and consume its result immediately with postfix `.await`",
                    );
                    IrType::Unit
                }
                "Token" => {
                    self.push_source_error(
                        ErrorCode::NX2101,
                        &module.source,
                        byte_range(ty.range),
                        "`Token` requires exactly one content type",
                        "write `Token<ContentType>`",
                    );
                    IrType::Unit
                }
                "ResourceToken" => {
                    self.push_source_error(
                        ErrorCode::NX2101,
                        &module.source,
                        byte_range(ty.range),
                        "`ResourceToken` is not a Nexa v2 source type",
                        "write `Token<ContentType>`",
                    );
                    IrType::Unit
                }
                name_text => match builtin_type(name_text) {
                    Some(ty) => ty,
                    None => {
                        if let Some(definition) = self.builtin_types.get(name_text).copied() {
                            self.record_reference(module, name.range, definition);
                            IrType::Named(definition)
                        } else {
                            self.resolve_symbol_path(module, name, SymbolUse::Type)
                                .map_or(IrType::Error, IrType::Named)
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
                    ("Set", [inner]) => IrType::Set(Box::new(self.resolve_type_ref(module, inner))),
                    ("HostRequest", [_]) => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &module.source,
                            byte_range(ty.range),
                            "`HostRequest` is runtime-only and cannot be named or stored in Nexa source",
                            "call an async Host function and consume its result immediately with postfix `.await`",
                        );
                        IrType::Error
                    }
                    ("Token", [inner]) => {
                        IrType::ResourceToken(Some(Box::new(self.resolve_type_ref(module, inner))))
                    }
                    ("ResourceToken", _) => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &module.source,
                            byte_range(ty.range),
                            "`ResourceToken` is not a Nexa v2 source type",
                            "write `Token<ContentType>`",
                        );
                        IrType::Error
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
                        "Option" | "Result" | "Array" | "Map" | "Set" | "HostRequest" | "Token"
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
                        IrType::Error
                    }
                    _ => {
                        self.push_source_error(
                            ErrorCode::NX2101,
                            &module.source,
                            byte_range(ty.range),
                            format!("user generic type `{base_name}` is not supported"),
                            "M4 only permits the built-in generic types",
                        );
                        IrType::Error
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
            TypeKind::Set(inner) => IrType::Set(Box::new(self.resolve_type_ref(module, inner))),
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
                | IrType::Set(inner)
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
            | (TypeKind::Set(syntax), IrType::Set(resolved))
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

    #[allow(clippy::too_many_lines)]
    fn resolve_symbol_path(
        &mut self,
        module: &ParsedModule,
        path: &ast::QualifiedName,
        usage: SymbolUse,
    ) -> Option<DefinitionId> {
        let current = self.lookup_symbol_path(module, path);
        let current_is_from_staged_cell = current.is_some_and(|definition| {
            let definition = &self.definitions[definition.0 as usize];
            definition.span.source == module.source && Self::symbol_matches_usage(definition, usage)
        });
        let id = if current_is_from_staged_cell {
            current
        } else {
            self.repl_snapshot_symbol(module, path, usage).or(current)
        };
        let Some(id) = id else {
            // Parser-recovered placeholder names (`<missing>`/`<error>`) are already explained by
            // a syntax diagnostic; resolve them silently to the poison type instead of adding a
            // second name-resolution error.
            if path.text().starts_with('<') {
                return None;
            }
            if self.explained_names.contains(&path.text()) {
                return None;
            }
            let code = match usage {
                SymbolUse::Type => ErrorCode::NX2002,
                SymbolUse::Value | SymbolUse::Callable => ErrorCode::NX2001,
            };
            self.poison_cause.get_or_insert_with(|| {
                format!("caused by unknown {} `{}`", usage.name(), path.text()).into()
            });
            let emit = match usage {
                SymbolUse::Type => {
                    let uses = self
                        .unknown_type_uses
                        .entry((module.source.clone(), path.text()))
                        .or_default();
                    uses.push(byte_range(path.range));
                    uses.len() == 1
                }
                SymbolUse::Value | SymbolUse::Callable => true,
            };
            if emit {
                let mut diagnostic = Diagnostic::new(
                    code,
                    Severity::Error,
                    format!("unknown {} `{}`", usage.name(), path.text()),
                )
                .with_label(Label::primary(
                    source_identity(&module.source),
                    byte_range(path.range),
                    "name is not declared in this module or imported namespace",
                ));
                if let Some(fix) = unknown_symbol_fix(
                    &path.text(),
                    &module.source,
                    byte_range(path.range),
                    usage,
                    &module.key,
                    &self.definitions,
                    &self.builtin_types,
                ) {
                    diagnostic = diagnostic.with_fix(fix);
                }
                self.diagnostics.push(diagnostic);
            }
            return None;
        };
        let definition = self.definitions[id.0 as usize].clone();
        let kind_valid = Self::symbol_matches_usage(&definition, usage);
        if !kind_valid && !path.text().starts_with('<') {
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
                        "declared using {} visibility",
                        visibility_name(definition.visibility)
                    ),
                )),
            );
        }
        self.record_reference(module, path.range, id);
        Some(id)
    }

    fn symbol_matches_usage(definition: &Definition, usage: SymbolUse) -> bool {
        match usage {
            SymbolUse::Type => {
                matches!(
                    definition.kind,
                    DefinitionKind::Struct
                        | DefinitionKind::Enum
                        | DefinitionKind::Class
                        | DefinitionKind::HostContract
                        | DefinitionKind::StandardLibrary
                ) && matches!(definition.ty, IrType::Named(_))
            }
            SymbolUse::Value => !matches!(
                definition.kind,
                DefinitionKind::Struct
                    | DefinitionKind::Enum
                    | DefinitionKind::Class
                    | DefinitionKind::HostContract
            ),
            SymbolUse::Callable => matches!(
                definition.kind,
                DefinitionKind::Function
                    | DefinitionKind::Task
                    | DefinitionKind::HostFunction
                    | DefinitionKind::StandardLibrary
            ),
        }
    }

    fn repl_snapshot_symbol(
        &self,
        module: &ParsedModule,
        path: &ast::QualifiedName,
        usage: SymbolUse,
    ) -> Option<DefinitionId> {
        if self.mode != AnalysisMode::ReplCell
            || module.key.package != self.input.root_manifest.id
            || module.key.module != crate::repl_module_path()
        {
            return None;
        }
        let [name] = path.segments.as_slice() else {
            return None;
        };
        let snapshot = self.repl_snapshot?;
        match usage {
            SymbolUse::Type => snapshot
                .resolve_type(&name.text)
                .map(|slot| slot.definition),
            SymbolUse::Value => snapshot
                .visible_binding(&name.text)
                .filter(|slot| matches!(&slot.kind, crate::ReplBindingKind::Value))
                .map(|slot| slot.definition),
            SymbolUse::Callable => snapshot
                .visible_binding(&name.text)
                .filter(|slot| matches!(&slot.kind, crate::ReplBindingKind::Function { .. }))
                .map(|slot| slot.definition),
        }
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
        self.validate_standalone_main();

        let records = self.declaration_records.clone();
        for record in records {
            let DeclarationKind::Function(function) = &record.declaration.kind else {
                continue;
            };
            let module = self.modules[record.module_index].clone();
            let effect = self.definitions[record.definition.0 as usize].effect;
            let lifecycle = matches!(
                effect,
                IrEffect::Migration | IrEffect::Activation | IrEffect::Cleanup
            );
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
                            "lifecycle attribute `@{}` on `{}` requires a `pub fn` in the root Application entry module",
                            effect_name(effect), function.name.text
                        ),
                        "this lifecycle attribute is not legal at this location",
                    );
                } else {
                    let lifecycle_slot = match effect {
                        IrEffect::Migration => Some(&mut self.lifecycle.migration),
                        IrEffect::Activation => Some(&mut self.lifecycle.activation),
                        IrEffect::Cleanup => Some(&mut self.lifecycle.cleanup),
                        IrEffect::Ordinary | IrEffect::Immediate | IrEffect::Task => None,
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
                                        effect_name(effect),
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
                }
            }
        }

        if let Some(host) = self.environment.host.clone() {
            let Some(entry) = self.input.root_manifest.entry().cloned() else {
                return;
            };
            let required_names = host
                .required_entrypoints
                .iter()
                .map(|entrypoint| entrypoint.name.clone())
                .collect::<BTreeSet<_>>();
            let mut entrypoints = host
                .nexa_entrypoints
                .iter()
                .cloned()
                .map(|entrypoint| (entrypoint.name.clone(), entrypoint))
                .collect::<BTreeMap<_, _>>();
            // A required surface is necessarily legal even while an older adapter is being
            // migrated to populate `nexa_entrypoints`. Keep the semantic union deterministic.
            for required in &host.required_entrypoints {
                entrypoints
                    .entry(required.name.clone())
                    .or_insert_with(|| NexaEntrypointSurface {
                        name: required.name.clone(),
                        stable_id: required.stable_id,
                        parameters: required.parameters.clone(),
                        result: required.result.clone(),
                        effect: required.effect,
                        source: required.source.clone(),
                    });
            }
            for (name, entrypoint) in entrypoints {
                let key = (
                    self.input.root_manifest.id.clone(),
                    entry.clone(),
                    name.clone(),
                );
                let Some(definition) = self.symbols.get(&key).copied() else {
                    if required_names.contains(&name) {
                        let mut diagnostic = Diagnostic::new(
                            ErrorCode::NX7010,
                            Severity::Error,
                            format!("missing required entrypoint `{name}`"),
                        );
                        if let Some(source) = &entrypoint.source {
                            diagnostic = diagnostic.with_label(Label::primary(
                                source.identity.clone(),
                                source.range,
                                "Host configuration requires this Nexa entrypoint",
                            ));
                        }
                        self.diagnostics.push(diagnostic);
                    }
                    continue;
                };
                let valid_visibility = self.definitions[definition.0 as usize].visibility
                    == DeclarationVisibility::Public;
                let required_parameters = entrypoint
                    .parameters
                    .iter()
                    .map(|ty| self.resolve_surface_type(ty))
                    .collect::<Vec<_>>();
                let required_result = self.resolve_surface_type(&entrypoint.result);
                let valid_signature =
                    self.function_signatures
                        .get(&definition)
                        .is_some_and(|signature| {
                            entrypoint
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
                        format!("Nexa entrypoint `{name}` has the wrong signature"),
                    )
                    .with_label(Label::primary(
                        source_identity(&declared.span.source),
                        range_from_source(&declared.span),
                        "entrypoint signature differs from the Host contract",
                    ));
                    if let Some(source) = &entrypoint.source {
                        diagnostic = diagnostic.with_related(RelatedLocation::new(
                            source.identity.clone(),
                            source.range,
                            "declared Nexa entrypoint signature",
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
                        name,
                        function: definition,
                        stable_id: entrypoint.stable_id,
                    });
                } else if let Some(export) = self
                    .exports
                    .iter_mut()
                    .find(|export| export.function == definition)
                {
                    export.stable_id = entrypoint.stable_id;
                }
            }
        }
        self.exports
            .sort_by(|left, right| (&left.name, left.function).cmp(&(&right.name, right.function)));
    }

    fn validate_standalone_main(&mut self) {
        if matches!(
            self.input.compilation_options.profile,
            CompilationProfile::Package | CompilationProfile::ReplCell
        ) {
            return;
        }
        let Some(entry_module) = self.input.root_manifest.entry().cloned() else {
            let fallback = self.fallback_source_range();
            self.push_source_error(
                ErrorCode::NX2101,
                &fallback.source,
                ByteRange::default(),
                "standalone compilation requires an entry module containing `main`",
                "set the Application `entry` module and define `main(args: Array<string>) -> i32`",
            );
            return;
        };
        let key = (
            self.input.root_manifest.id.clone(),
            entry_module.clone(),
            "main".to_owned(),
        );
        let Some(definition) = self.symbols.get(&key).copied() else {
            let source = self
                .modules
                .iter()
                .find(|module| {
                    module.key.package == self.input.root_manifest.id
                        && module.key.module == entry_module
                })
                .map_or_else(
                    || self.fallback_source_range(),
                    |module| source_range(&module.source, TextRange::at(TextSize::new(0), 0)),
                );
            self.push_source_error(
                ErrorCode::NX2101,
                &source.source,
                range_from_source(&source),
                format!(
                    "standalone entry module `{entry_module}` is missing `main(args: Array<string>) -> i32`"
                ),
                "`main` may be `fn` or `async fn`, but no other signature is accepted",
            );
            return;
        };
        let valid = self
            .function_signatures
            .get(&definition)
            .is_some_and(|signature| {
                signature.parameter_types == [IrType::Array(Box::new(IrType::String))]
                    && signature.result == IrType::I32
                    && matches!(signature.effect, IrEffect::Ordinary | IrEffect::Task)
            });
        if valid {
            return;
        }
        let span = self.definition_name_span(definition);
        self.push_source_error(
            ErrorCode::NX2101,
            &span.source,
            range_from_source(&span),
            "standalone `main` has an invalid signature",
            "only `fn main(args: Array<string>) -> i32` or `async fn main(args: Array<string>) -> i32` is accepted",
        );
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
            let declared = self.definitions[definition.0 as usize].clone();
            if !const_safe_type(
                &declared.ty,
                &self.definitions,
                &self.type_metadata,
                &self.variant_payloads,
                &mut BTreeSet::new(),
            ) {
                self.push_source_error(
                    ErrorCode::NX2720,
                    &declared.span.source,
                    range_from_source(&declared.span),
                    format!(
                        "type `{}` is not const-safe",
                        display_ir_type(&declared.ty, &self.definitions)
                    ),
                    "const values may contain primitives, string, rune, Struct, Enum, Tuple, Option, and Result values only",
                );
                continue;
            }
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
                if matches!(
                    path.segments.as_slice(),
                    [namespace, variant]
                        if namespace.text == "Option" && variant.text == "None"
                ) {
                    if !matches!(expected, Some(IrType::Option(_))) {
                        self.invalid_const(
                            module,
                            expression.range,
                            "`Option::None` requires an `Option<T>` const type",
                        );
                        return None;
                    }
                    return Some(ConstValue::BuiltinVariant {
                        variant: BuiltinVariantIr::OptionNone,
                        value: None,
                    });
                }
                let definition = self.resolve_symbol_path(module, path, SymbolUse::Value)?;
                match self.definitions[definition.0 as usize].kind {
                    DefinitionKind::Const => self.evaluate_const(definition, constants, visiting),
                    DefinitionKind::Variant
                        if self
                            .variant_payloads
                            .get(&definition)
                            .is_none_or(Vec::is_empty) =>
                    {
                        Some(ConstValue::Variant {
                            definition,
                            values: Vec::new(),
                        })
                    }
                    DefinitionKind::Variant => {
                        self.invalid_const(
                            module,
                            expression.range,
                            "Enum variant payload is required in this const expression",
                        );
                        None
                    }
                    _ => {
                        self.invalid_const(
                            module,
                            expression.range,
                            "only another const or a unit Enum variant may be read",
                        );
                        None
                    }
                }
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
            ExpressionKind::Construct { ty, fields, update } => {
                if update.is_some() {
                    self.invalid_const(
                        module,
                        expression.range,
                        "constant Struct values cannot use update syntax",
                    );
                    return None;
                }
                let definition = if self
                    .lookup_symbol_path(module, ty)
                    .is_some_and(|definition| {
                        self.definitions[definition.0 as usize].kind == DefinitionKind::Variant
                    }) {
                    self.resolve_symbol_path(module, ty, SymbolUse::Value)?
                } else {
                    self.resolve_symbol_path(module, ty, SymbolUse::Type)?
                };
                let definition_kind = self.definitions[definition.0 as usize].kind;
                if !matches!(
                    definition_kind,
                    DefinitionKind::Struct | DefinitionKind::Variant
                ) {
                    self.invalid_const(
                        module,
                        expression.range,
                        "only Struct values and struct-style Enum variants may be constructed in a const",
                    );
                    return None;
                }
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
                if definition_kind == DefinitionKind::Struct {
                    Some(ConstValue::Construct {
                        definition,
                        fields: values,
                    })
                } else {
                    Some(ConstValue::Variant {
                        definition,
                        values: values.into_iter().map(|(_, value)| value).collect(),
                    })
                }
            }
            ExpressionKind::Call {
                callee, arguments, ..
            } => {
                let ExpressionKind::Name(path) = &callee.kind else {
                    self.invalid_const(
                        module,
                        expression.range,
                        "constant expressions cannot call computed values",
                    );
                    return None;
                };
                let builtin = match (path.segments.as_slice(), expected) {
                    ([namespace, variant], Some(IrType::Option(inner)))
                        if namespace.text == "Option" && variant.text == "Some" =>
                    {
                        Some((BuiltinVariantIr::OptionSome, inner.as_ref()))
                    }
                    ([namespace, variant], Some(IrType::Result(ok, _)))
                        if namespace.text == "Result" && variant.text == "Ok" =>
                    {
                        Some((BuiltinVariantIr::ResultOk, ok.as_ref()))
                    }
                    ([namespace, variant], Some(IrType::Result(_, error)))
                        if namespace.text == "Result" && variant.text == "Err" =>
                    {
                        Some((BuiltinVariantIr::ResultErr, error.as_ref()))
                    }
                    _ => None,
                };
                if let Some((variant, payload_type)) = builtin {
                    let [argument] = arguments.as_slice() else {
                        self.invalid_const(
                            module,
                            expression.range,
                            "builtin const variant requires exactly one payload",
                        );
                        return None;
                    };
                    let value = self.evaluate_const_expression(
                        module,
                        argument,
                        Some(payload_type),
                        constants,
                        visiting,
                    )?;
                    return Some(ConstValue::BuiltinVariant {
                        variant,
                        value: Some(Box::new(value)),
                    });
                }
                if matches!(
                    path.segments.as_slice(),
                    [namespace, variant]
                        if (namespace.text == "Option" && variant.text == "Some")
                            || (namespace.text == "Result"
                                && matches!(variant.text.as_str(), "Ok" | "Err"))
                ) {
                    self.invalid_const(
                        module,
                        expression.range,
                        "builtin variant does not match the declared const type",
                    );
                    return None;
                }
                if let Some(variant) = self.resolve_symbol_path(module, path, SymbolUse::Value)
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
                    let mut signature = self
                        .function_signatures
                        .get(&record.definition)
                        .cloned()
                        .expect("function signature was resolved");
                    let is_repl_entry = self.repl_entry_definition == Some(record.definition);
                    let mut checker =
                        BodyChecker::new(self, module.clone(), Some(record.definition), &signature);
                    let body = if is_repl_entry {
                        checker.check_repl_entry_block(&function.body)
                    } else {
                        checker.check_block(&function.body)
                    };
                    checker.validate_migration_body(&function.body, &body);
                    let locals = checker.locals;
                    if is_repl_entry {
                        signature.result = body
                            .tail
                            .as_ref()
                            .map_or(IrType::Unit, |tail| tail.ty.clone());
                        self.definitions[record.definition.0 as usize].ty =
                            signature.result.clone();
                        self.function_signatures
                            .insert(record.definition, signature.clone());
                        let ordinal = self.repl_cell.map_or(1, |cell| cell.ordinal);
                        self.repl_entry = Some(crate::ReplEntrypointIr {
                            cell_ordinal: ordinal,
                            function: record.definition,
                            stable_id: crate::repl_cell_entry_symbol(ordinal),
                            result: signature.result.clone(),
                            effect: signature.effect,
                        });
                    }
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
                            mutable: ty.kind == TypeDeclarationKind::Class
                                && metadata
                                    .field_mutability
                                    .get(definition)
                                    .copied()
                                    .unwrap_or(false),
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
                        TypeDeclarationKind::Class => TypedTypeLayoutIr::Class {
                            fields,
                            state: metadata.state.as_ref().and_then(|state| {
                                self.stable_ids
                                    .get(&record.definition)
                                    .copied()
                                    .map(|stable_id| StateMetadataIr {
                                        version: state.version,
                                        stable_id,
                                    })
                            }),
                        },
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
                        | DefinitionKind::HostContract
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
                kind: "state-class".into(),
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
            || (self.mode == AnalysisMode::ReplCell
                && self
                    .repl_prior_modules
                    .iter()
                    .any(|module| module.source == *source))
    }

    fn production_state_types(&self) -> Vec<AnalyzedStateType> {
        self.state_types
            .iter()
            .filter(|state| self.definition_is_production(state.definition))
            .cloned()
            .collect()
    }

    #[allow(clippy::too_many_lines)]
    fn typed_modules(&mut self) -> Vec<TypedModuleIr> {
        let mut typed_modules = self.repl_prior_modules.clone();
        let next_repl_file_id = typed_modules
            .iter()
            .map(|module| module.file_id.0)
            .chain(
                self.repl_prior_external_sources
                    .iter()
                    .map(|source| source.file_id.0),
            )
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .expect("REPL artifact FileId space is not exhausted");
        for (index, module) in self.modules.iter().enumerate() {
            if self.mode != AnalysisMode::Test && module.role != SourceRole::Production {
                continue;
            }
            let prior_has_compiler_module = self
                .repl_prior_modules
                .iter()
                .any(|prior| prior.package_id.as_str() == nexa_stdlib::PACKAGE_ID);
            if prior_has_compiler_module && module.compiler_provided {
                continue;
            }
            let module_key = ModuleKey::new(module.key.package.clone(), module.key.module.clone());
            if self.mode != AnalysisMode::ReplCell
                && let Some(cached) = self.db.typed_module(
                    &module_key,
                    &self.definitions,
                    &self.typed_module_semantic_context,
                )
            {
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
                virtual_module_path: module.virtual_module_path.clone(),
                source: module.source.clone(),
                file_id: if self.mode == AnalysisMode::ReplCell {
                    ArtifactFileId(
                        next_repl_file_id
                            .checked_add(
                                u32::try_from(
                                    typed_modules
                                        .len()
                                        .saturating_sub(self.repl_prior_modules.len()),
                                )
                                .expect("REPL module count fits u32"),
                            )
                            .expect("REPL artifact FileId space is not exhausted"),
                    )
                } else if module.compiler_provided {
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
            if self.mode != AnalysisMode::ReplCell {
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
            }
            typed_modules.push(typed);
        }
        self.refresh_repl_environment_layout(&mut typed_modules);
        if self.mode == AnalysisMode::ReplCell {
            // A cumulative Cell inserts a new package source before compiler-provided and external
            // sources in the artifact authority. FileIds are artifact-local, so recanonicalize the
            // complete module catalog on every candidate instead of preserving stale numeric IDs.
            let mut canonical_order = (0..typed_modules.len()).collect::<Vec<_>>();
            canonical_order.sort_by(|left, right| {
                let left = &typed_modules[*left];
                let right = &typed_modules[*right];
                let left_tier = u8::from(left.package_id.as_str() == nexa_stdlib::PACKAGE_ID);
                let right_tier = u8::from(right.package_id.as_str() == nexa_stdlib::PACKAGE_ID);
                (left_tier, source_identity(&left.source).to_string())
                    .cmp(&(right_tier, source_identity(&right.source).to_string()))
            });
            for (offset, module) in canonical_order.into_iter().enumerate() {
                typed_modules[module].file_id = ArtifactFileId(
                    u32::try_from(offset.saturating_add(1))
                        .expect("REPL module count fits artifact FileId"),
                );
            }
        }
        typed_modules
    }

    fn refresh_repl_environment_layout(&self, modules: &mut [TypedModuleIr]) {
        let Some(environment) = self.repl_environment_definition else {
            return;
        };
        let Some(metadata) = self.type_metadata.get(&environment) else {
            return;
        };
        let Some(state) = self
            .state_types
            .iter()
            .find(|state| state.definition == environment)
        else {
            return;
        };
        let fields = metadata
            .field_order
            .iter()
            .enumerate()
            .map(|(order, definition)| FieldLayoutIr {
                definition: *definition,
                ty: self.definitions[definition.0 as usize].ty.clone(),
                order: u32::try_from(order).unwrap_or(u32::MAX),
                mutable: metadata
                    .field_mutability
                    .get(definition)
                    .copied()
                    .unwrap_or(false),
            })
            .collect::<Vec<_>>();
        for module in modules {
            let mut declarations = module.declarations.to_vec();
            let Some(declaration) = declarations
                .iter_mut()
                .find(|declaration| declaration.definition == environment)
            else {
                continue;
            };
            declaration.body = TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Class {
                fields: fields.clone(),
                state: Some(StateMetadataIr {
                    version: state.version,
                    stable_id: state.stable_id,
                }),
            });
            module.declarations = declarations.into();
            break;
        }
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

/// Appends a `N more uses at ...` note to the first diagnostic for each repeatedly
/// unresolved type name.
fn aggregate_unknown_type_notes(
    diagnostics: &mut DiagnosticBatch,
    uses: BTreeMap<(SourceKey, String), Vec<ByteRange>>,
) {
    for ((source, name), positions) in uses {
        if positions.len() <= 1 {
            continue;
        }
        let mut note = format!("{} more uses at ", positions.len() - 1);
        for (index, range) in positions.iter().skip(1).enumerate() {
            if index > 0 {
                note.push_str(", ");
            }
            match diagnostics.sources().get(&source_identity(&source)) {
                Some(snapshot) => {
                    let position = snapshot.human_position(range.start as usize);
                    let _ = write!(note, "{}:{}", position.line, position.column);
                }
                None => note.push_str("<source unavailable>"),
            }
        }
        let expected = format!("unknown type `{name}`");
        diagnostics.push_note_to_first(
            |diagnostic| {
                diagnostic.code == ErrorCode::NX2002 && diagnostic.message.as_ref() == expected
            },
            note,
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
    mutable_bindings: BTreeSet<DefinitionId>,
    readonly_loop_bindings: BTreeSet<DefinitionId>,
    /// Local bindings whose collection value is iterated by an active `for`
    /// loop. Reassignment or a direct mutating method call on them inside the
    /// body is a statically provable mutation hazard and rejected; indirect
    /// mutation is left to the runtime mutation-epoch trap.
    iterated_collections: BTreeSet<DefinitionId>,
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
            mutable_bindings: BTreeSet::new(),
            readonly_loop_bindings: BTreeSet::new(),
            iterated_collections: BTreeSet::new(),
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
    fn check_for_statement(
        &mut self,
        statement: &Statement,
        bindings: &ForBindings,
        iterable: &ForIterable,
        body: &ast::Block,
    ) -> Option<TypedStatementIr> {
        match iterable {
            ForIterable::Range { start, end, .. } => {
                let start = self.check_expression(start, Some(&IrType::I32));
                let end = self.check_expression(end, Some(&IrType::I32));
                self.expect_type(&start.ty, &IrType::I32, &start.span);
                self.expect_type(&end.ty, &IrType::I32, &end.span);
                let ForBindings::Single(binding) = bindings else {
                    let ForBindings::Pair { range, .. } = bindings else {
                        unreachable!("ForBindings is Single or Pair");
                    };
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(*range),
                        "a two-binding `for` loop requires a Map iterable",
                        "a range iterates a single i32 value; use one binding",
                    );
                    return None;
                };
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
                let loop_limit = self
                    .analyzer
                    .input
                    .compilation_options
                    .limits
                    .max_loop_iterations;
                if let (Some(start_value), Some(end_value)) = (
                    constant_i32_expression(&start, &self.analyzer.const_values),
                    constant_i32_expression(&end, &self.analyzer.const_values),
                ) {
                    let iterations = i64::from(end_value)
                        .saturating_sub(i64::from(start_value))
                        .max(0);
                    let max_iterations = u32::try_from(iterations).unwrap_or(u32::MAX);
                    if max_iterations > loop_limit {
                        self.analyzer.push_source_error(
                            ErrorCode::NX2101,
                            &self.module.source,
                            byte_range(statement.range),
                            format!(
                                "static range has {max_iterations} iterations, exceeding the limit of {loop_limit}"
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
                } else {
                    Some(TypedStatementIr::DynamicRangeFor {
                        binding: definition,
                        start,
                        end,
                        body,
                        max_iterations: loop_limit,
                    })
                }
            }
            ForIterable::Expression(expression) => {
                let iterable_expr = self.check_expression(expression, None);
                let iterable_ty = iterable_expr.ty.clone();
                match &iterable_ty {
                    IrType::Array(element) => self.check_collection_for(
                        bindings,
                        iterable_expr,
                        CollectionIterationKindIr::Array,
                        element.as_ref().clone(),
                        None,
                        body,
                    ),
                    IrType::Buffer(element) => self.check_collection_for(
                        bindings,
                        iterable_expr,
                        CollectionIterationKindIr::Buffer,
                        element.as_ref().clone(),
                        None,
                        body,
                    ),
                    IrType::Set(element) => self.check_collection_for(
                        bindings,
                        iterable_expr,
                        CollectionIterationKindIr::Set,
                        element.as_ref().clone(),
                        None,
                        body,
                    ),
                    IrType::Map(key, value) => self.check_collection_for(
                        bindings,
                        iterable_expr,
                        CollectionIterationKindIr::Map,
                        value.as_ref().clone(),
                        Some(key.as_ref().clone()),
                        body,
                    ),
                    _ => {
                        if contains_ir_error(&iterable_expr.ty) {
                            self.record_suppressed();
                            None
                        } else {
                            self.analyzer.push_source_error(
                                ErrorCode::NX2101,
                                &self.module.source,
                                byte_range(expression.range),
                                "expression is not iterable",
                                "`for` iterates a Range, Array, Buffer, Map, or Set",
                            );
                            None
                        }
                    }
                }
            }
        }
    }

    fn check_collection_for(
        &mut self,
        bindings: &ForBindings,
        iterable_expr: TypedExpressionIr,
        collection: CollectionIterationKindIr,
        element_type: IrType,
        key_type: Option<IrType>,
        body: &ast::Block,
    ) -> Option<TypedStatementIr> {
        let binding_shapes = match bindings {
            ForBindings::Single(binding) => {
                if collection == CollectionIterationKindIr::Map {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(binding.range),
                        "iterating a `Map` requires two bindings `(key, value)`",
                        "write `for (key, value) in map { ... }`",
                    );
                    return None;
                }
                vec![(binding.text.clone(), element_type.clone(), binding.range)]
            }
            ForBindings::Pair { key, value, range } => {
                if collection != CollectionIterationKindIr::Map {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2101,
                        &self.module.source,
                        byte_range(*range),
                        "a two-binding `for` loop requires a Map iterable",
                        "Array, Buffer, Set, and Range iterate a single element; use one binding",
                    );
                    return None;
                }
                let key_type = key_type
                    .as_ref()
                    .expect("Map iteration carries a key type")
                    .clone();
                vec![
                    (key.text.clone(), key_type, key.range),
                    (value.text.clone(), element_type.clone(), value.range),
                ]
            }
        };
        let guarded = match &iterable_expr.kind {
            TypedExpressionKind::Reference(definition) => Some(*definition),
            _ => None,
        };
        self.scopes.push(BTreeMap::new());
        let definitions = binding_shapes
            .into_iter()
            .map(|(name, ty, range)| {
                let definition = self.allocate_local(name.clone(), ty, range);
                self.scopes
                    .last_mut()
                    .expect("for scope exists")
                    .insert(name, definition);
                self.readonly_loop_bindings.insert(definition);
                definition
            })
            .collect::<Vec<_>>();
        if let Some(guarded) = guarded {
            self.iterated_collections.insert(guarded);
        }
        self.loop_depth += 1;
        let body = self.check_block(body);
        self.loop_depth = self.loop_depth.saturating_sub(1);
        if let Some(guarded) = guarded {
            self.iterated_collections.remove(&guarded);
        }
        for definition in &definitions {
            self.readonly_loop_bindings.remove(definition);
        }
        self.scopes.pop();
        Some(TypedStatementIr::CollectionFor {
            iterable: iterable_expr,
            bindings: definitions,
            key_type,
            element_type,
            collection,
            body,
            max_iterations: self
                .analyzer
                .input
                .compilation_options
                .limits
                .max_loop_iterations,
        })
    }

    fn check_repl_entry_block(&mut self, block: &ast::Block) -> TypedBlockIr {
        self.scopes.push(BTreeMap::new());
        let statements = block
            .statements
            .iter()
            .filter_map(|statement| self.check_repl_top_level_statement(statement))
            .collect();
        let tail = block
            .tail
            .as_ref()
            .map(|expression| Box::new(self.check_expression(expression, None)));
        self.scopes.pop();
        TypedBlockIr { statements, tail }
    }

    fn check_repl_top_level_statement(
        &mut self,
        statement: &Statement,
    ) -> Option<TypedStatementIr> {
        if matches!(statement.kind, StatementKind::Return(_)) {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(statement.range),
                "REPL cells cannot use a top-level `return` statement",
                "use the cell's tail expression as its result",
            );
            return None;
        }
        let StatementKind::Bind {
            mutable,
            name,
            ty,
            value,
        } = &statement.kind
        else {
            return self.check_statement(statement);
        };
        self.analyzer.validate_snake_name(
            &self.module.clone(),
            name,
            "local variable names must use snake_case",
        );
        let expected = ty
            .as_ref()
            .map(|ty| self.analyzer.resolve_type_ref(&self.module, ty));
        let value = self.check_expression(value, expected.as_ref());
        let resolved = expected.unwrap_or_else(|| value.ty.clone());
        self.expect_type(&value.ty, &resolved, &value.span);
        let Some(definition) = self.analyzer.allocate_repl_state_field(
            &self.module.clone(),
            &name.text,
            *mutable,
            resolved,
            name.range,
        ) else {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(statement.range),
                "REPL persistent environment is unavailable",
                "analyze the cell from the formal revision-zero REPL seed",
            );
            return None;
        };
        self.scopes
            .last_mut()
            .expect("a lexical scope exists")
            .insert(name.text.clone(), definition);
        if *mutable {
            self.mutable_bindings.insert(definition);
        }
        self.mark_restricted(RestrictedOperation::PersistentState);
        Some(TypedStatementIr::Assign {
            target: TypedPlaceIr::StateField {
                base: Box::new(
                    self.repl_environment_expression(source_range(&self.module.source, name.range)),
                ),
                field: definition,
            },
            value,
        })
    }

    fn repl_environment_expression(&self, span: SourceRange) -> TypedExpressionIr {
        let environment = self
            .analyzer
            .repl_environment_definition
            .expect("formal cumulative REPL analysis has an environment definition");
        TypedExpressionIr {
            ty: IrType::Named(environment),
            effect: IrEffect::Immediate,
            span,
            kind: TypedExpressionKind::PersistentStateGet {
                identity: crate::repl_environment_symbol().0,
                state_type: environment,
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn check_statement(&mut self, statement: &Statement) -> Option<TypedStatementIr> {
        match &statement.kind {
            StatementKind::Bind {
                mutable,
                name,
                ty,
                value,
            } => {
                self.analyzer.validate_snake_name(
                    &self.module.clone(),
                    name,
                    "local variable names must use snake_case",
                );
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
                if *mutable {
                    self.mutable_bindings.insert(definition);
                }
                Some(TypedStatementIr::Let {
                    definition,
                    mutable: *mutable,
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
            StatementKind::CompoundAssign {
                target,
                operator,
                value,
            } => {
                let place = self.check_place(target)?;
                let target_type = place_type(&place, &self.analyzer.definitions);
                let value = self.check_expression(value, Some(&target_type));
                self.expect_type(&value.ty, &target_type, &value.span);
                let result = binary_result(
                    operator.kind,
                    &target_type,
                    &self.analyzer.definitions,
                    &self.analyzer.type_metadata,
                    &self.analyzer.variant_payloads,
                    &self.analyzer.host_types,
                )
                .unwrap_or_else(|| {
                    if contains_ir_error(&target_type) || contains_ir_error(&value.ty) {
                        self.record_suppressed();
                        return IrType::Error;
                    }
                    self.type_error(
                        source_range(&self.module.source, statement.range),
                        "invalid compound-assignment operand type",
                    );
                    IrType::Error
                });
                self.expect_type(&result, &target_type, &value.span);
                Some(TypedStatementIr::CompoundAssign {
                    target: place,
                    operator: binary_operator(operator.kind),
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
                bindings,
                iterable,
                body,
            } => self.check_for_statement(statement, bindings, iterable, body),
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
                        "yield is only valid in an async function",
                        "a synchronous function cannot yield",
                    );
                }
                self.mark_restricted(RestrictedOperation::Yield);
                Some(TypedStatementIr::Yield {
                    span: source_range(&self.module.source, statement.range),
                })
            }
            StatementKind::Defer(expression) => {
                let mut expression = self.check_expression(expression, None);
                if expression.effect == IrEffect::Task
                    || typed_expression_contains_await(&expression)
                {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        byte_range(statement.range),
                        "defer cleanup cannot await or call asynchronous code",
                        "defer always runs in a synchronous cleanup context",
                    );
                }
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
                if path
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .eq(["Option", "None"])
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
                    .unwrap_or_else(|| {
                        if expected.is_some_and(contains_ir_error) {
                            IrType::Error
                        } else {
                            self.type_error(
                                span.clone(),
                                "empty array literal requires an expected `Array<T>` type",
                            );
                            IrType::Unit
                        }
                    });
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
                        if contains_ir_error(&operand.ty) {
                            self.record_suppressed();
                            IrType::Error
                        } else {
                            self.type_error(operand.span.clone(), "invalid unary operand type");
                            self.recovery_unit_type(&span)
                        }
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
                let result = binary_result(
                    operator.kind,
                    &left.ty,
                    &self.analyzer.definitions,
                    &self.analyzer.type_metadata,
                    &self.analyzer.variant_payloads,
                    &self.analyzer.host_types,
                )
                .unwrap_or_else(|| {
                    if contains_ir_error(&left.ty) || contains_ir_error(&right.ty) {
                        self.record_suppressed();
                        return IrType::Error;
                    }
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
            ExpressionKind::Construct { ty, fields, update } => {
                self.check_construct(expression, ty, fields, update.as_deref())
            }
            ExpressionKind::New { ty, fields, update } => {
                let definition = match &ty.kind {
                    TypeKind::Named(path) | TypeKind::Generic { base: path, .. } => self
                        .analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Type),
                    _ => None,
                };
                if let Some(definition) = definition {
                    self.check_class_construct(expression, definition, fields, update.as_deref())
                } else {
                    self.error_expression(span)
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
                        if contains_ir_error(&receiver.ty) {
                            self.record_suppressed();
                            (IrType::Error, IrType::Error)
                        } else {
                            self.type_error(span.clone(), "value is not indexable");
                            (IrType::I32, self.recovery_unit_type(&span))
                        }
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
            ExpressionKind::Await { operand } => {
                let await_range = TextRange::at(
                    TextSize::new(expression.range.end.get().saturating_sub(6)),
                    6,
                );
                if self.effect != IrEffect::Task {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        byte_range(await_range),
                        "`.await` is only valid in an async function",
                        "make the enclosing function `async fn` or remove `.await`",
                    );
                }
                self.mark_restricted(RestrictedOperation::Await);
                let value = self.check_expression_inner(operand, expected, true);
                if self.effect == IrEffect::Task && value.effect != IrEffect::Task {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2301,
                        &self.module.source,
                        byte_range(await_range),
                        "`.await` requires an asynchronous call result",
                        "remove `.await` or call an `async fn`/asynchronous Host function",
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
                // `request().await?` is represented as Try(Await(Call)) by the lossless AST.
                // Preserve the enclosing await context through `?` so the async call itself is
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
                            && !contains_ir_error(function_error)
                            && !contains_ir_error(expression_error)
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
                                .with_note(format!(
                                    "function error type: `{}`",
                                    display_ir_type(function_error, &self.analyzer.definitions)
                                ))
                                .with_note(format!(
                                    "expression error type: `{}`",
                                    display_ir_type(expression_error, &self.analyzer.definitions)
                                )),
                            );
                            self.recovery_unit_type(&span)
                        } else {
                            ok.as_ref().clone()
                        }
                    }
                    actual => {
                        if contains_ir_error(actual) {
                            self.record_suppressed();
                            IrType::Error
                        } else {
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
                                .with_note(format!(
                                    "actual type: `{}`",
                                    display_ir_type(actual, &self.analyzer.definitions)
                                )),
                            );
                            self.recovery_unit_type(&span)
                        }
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
                            if is_scalar(&value.ty) {
                                values.push(value);
                            } else if is_interpolatable(&value.ty) {
                                values.push(TypedExpressionIr {
                                    ty: IrType::String,
                                    effect: value.effect,
                                    span: value.span.clone(),
                                    kind: TypedExpressionKind::BuiltinCall {
                                        operation: BuiltinOperationIr::ValueToString,
                                        type_arguments: vec![value.ty.clone()],
                                        arguments: vec![value],
                                    },
                                });
                            } else if contains_ir_error(&value.ty) {
                                self.record_suppressed();
                                values.push(value);
                            } else {
                                self.type_error(
                                    value.span.clone(),
                                    &format!(
                                        "interpolation supports scalar values and Array<T> when \
                                         its elements are formattable; {} is not formattable",
                                        display_ir_type(&value.ty, &self.analyzer.definitions)
                                    ),
                                );
                                values.push(value);
                            }
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
    /// namespace can also contribute the receiver (`use package::app::util as u; u::origin.x`):
    /// after the
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
        if let Some(definition) =
            self.analyzer
                .repl_snapshot_symbol(&self.module, path, SymbolUse::Value)
        {
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

    fn receiver_reference(
        &mut self,
        definition: DefinitionId,
        range: TextRange,
    ) -> TypedExpressionIr {
        let declared = self.analyzer.definitions[definition.0 as usize].clone();
        let span = source_range(&self.module.source, range);
        if self.analyzer.is_repl_state_field(definition) {
            self.mark_restricted(RestrictedOperation::PersistentState);
            return TypedExpressionIr {
                ty: declared.ty,
                effect: IrEffect::Immediate,
                span: span.clone(),
                kind: TypedExpressionKind::StateField {
                    base: Box::new(self.repl_environment_expression(span)),
                    field: definition,
                },
            };
        }
        TypedExpressionIr {
            ty: declared.ty,
            effect: declared.effect,
            span,
            kind: TypedExpressionKind::Reference(definition),
        }
    }

    fn is_receiver_value_definition(&self, definition: DefinitionId) -> bool {
        match self.analyzer.definitions[definition.0 as usize].kind {
            DefinitionKind::Const | DefinitionKind::Parameter | DefinitionKind::Local => true,
            DefinitionKind::Field => self.analyzer.is_repl_state_field(definition),
            DefinitionKind::StandardLibrary => {
                !self.analyzer.external_functions.contains_key(&definition)
                    && !self.analyzer.type_metadata.contains_key(&definition)
            }
            DefinitionKind::Function
            | DefinitionKind::Task
            | DefinitionKind::Struct
            | DefinitionKind::Enum
            | DefinitionKind::Class
            | DefinitionKind::Variant
            | DefinitionKind::HostContract
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
        if self.is_state_definition(owner) {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(whole_range),
                "@state fields cannot be accessed directly",
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

    fn is_state_definition(&self, definition: DefinitionId) -> bool {
        self.analyzer
            .type_metadata
            .get(&definition)
            .is_some_and(|metadata| metadata.state.is_some())
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
        expected: Option<&IrType>,
    ) -> Option<TypedExpressionIr> {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>();
        let (operation, arity) = match segments.as_slice() {
            ["Array", "new"] => (BuiltinOperationIr::ArrayNew, 1),
            ["Map", "new"] => (BuiltinOperationIr::MapNew, 2),
            ["Set", "new"] => (BuiltinOperationIr::SetNew, 1),
            _ => return None,
        };
        let span = source_range(&self.module.source, whole.range);
        if !type_arguments.is_empty() && type_arguments.len() != arity {
            self.type_error(
                span.clone(),
                &format!(
                    "`{}` expects either no explicit type arguments or exactly {arity}, found {}",
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
        let resolved_type_arguments = if type_arguments.len() == arity {
            type_arguments
                .iter()
                .map(|argument| self.analyzer.resolve_type_ref(&self.module, argument))
                .collect::<Vec<_>>()
        } else if type_arguments.is_empty() {
            match (operation, expected) {
                (BuiltinOperationIr::ArrayNew, Some(IrType::Array(element)))
                | (BuiltinOperationIr::SetNew, Some(IrType::Set(element))) => {
                    vec![element.as_ref().clone()]
                }
                (BuiltinOperationIr::MapNew, Some(IrType::Map(key, value))) => {
                    vec![key.as_ref().clone(), value.as_ref().clone()]
                }
                _ => {
                    if expected.is_some_and(contains_ir_error) {
                        vec![IrType::Error; arity]
                    } else {
                        self.type_error(
                            span.clone(),
                            &format!(
                                "cannot infer `{}` element types without an expected type",
                                path.text()
                            ),
                        );
                        vec![IrType::Unit; arity]
                    }
                }
            }
        } else {
            vec![IrType::Unit; arity]
        };
        let result = match operation {
            BuiltinOperationIr::ArrayNew => {
                IrType::Array(Box::new(resolved_type_arguments[0].clone()))
            }
            BuiltinOperationIr::MapNew => IrType::Map(
                Box::new(resolved_type_arguments[0].clone()),
                Box::new(resolved_type_arguments[1].clone()),
            ),
            BuiltinOperationIr::SetNew => IrType::Set(Box::new(resolved_type_arguments[0].clone())),
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
                    "contains" => (
                        BuiltinOperationIr::StringContains,
                        IrType::Bool,
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "starts_with" => (
                        BuiltinOperationIr::StringStartsWith,
                        IrType::Bool,
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "ends_with" => (
                        BuiltinOperationIr::StringEndsWith,
                        IrType::Bool,
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "substring" => (
                        BuiltinOperationIr::StringSubstring,
                        IrType::String,
                        Vec::new(),
                        vec![IrType::I32, IrType::I32],
                        false,
                    ),
                    "trim" => (
                        BuiltinOperationIr::StringTrim,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    "split" => (
                        BuiltinOperationIr::StringSplit,
                        IrType::Array(Box::new(IrType::String)),
                        Vec::new(),
                        vec![IrType::String],
                        false,
                    ),
                    "to_string" => (
                        BuiltinOperationIr::StringToString,
                        IrType::String,
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
                        "is_empty" => (
                            BuiltinOperationIr::ArrayIsEmpty,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "get" => (
                            BuiltinOperationIr::ArrayTryGet,
                            IrType::Option(Box::new(element.clone())),
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
                        "reserve" => (
                            BuiltinOperationIr::ArrayReserve,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![IrType::I32],
                            false,
                        ),
                        "capacity" => (
                            BuiltinOperationIr::ArrayCapacity,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "shrink_to_fit" => (
                            BuiltinOperationIr::ArrayShrinkToFit,
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
                        "insert" => (
                            BuiltinOperationIr::MapInsert,
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
                IrType::Set(element) => {
                    let element = element.as_ref().clone();
                    let operation_type_arguments = vec![element.clone()];
                    match method {
                        "len" => (
                            BuiltinOperationIr::SetLen,
                            IrType::I32,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "contains" => (
                            BuiltinOperationIr::SetContains,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![element.clone()],
                            false,
                        ),
                        "insert" => (
                            BuiltinOperationIr::SetInsert,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![element.clone()],
                            false,
                        ),
                        "remove" => (
                            BuiltinOperationIr::SetRemove,
                            IrType::Bool,
                            operation_type_arguments,
                            vec![element],
                            false,
                        ),
                        "clear" => (
                            BuiltinOperationIr::SetClear,
                            IrType::Unit,
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
                IrType::Option(inner) => {
                    let inner = inner.as_ref().clone();
                    let operation_type_arguments = vec![inner.clone()];
                    match method {
                        "is_some" => (
                            BuiltinOperationIr::OptionIsSome,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "is_none" => (
                            BuiltinOperationIr::OptionIsNone,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "unwrap_or" => (
                            BuiltinOperationIr::OptionUnwrapOr,
                            inner.clone(),
                            operation_type_arguments,
                            vec![inner.clone()],
                            false,
                        ),
                        _ => return None,
                    }
                }
                IrType::Result(ok, error) => {
                    let ok = ok.as_ref().clone();
                    let error = error.as_ref().clone();
                    let operation_type_arguments = vec![ok.clone(), error.clone()];
                    match method {
                        "is_ok" => (
                            BuiltinOperationIr::ResultIsOk,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "is_err" => (
                            BuiltinOperationIr::ResultIsErr,
                            IrType::Bool,
                            operation_type_arguments,
                            Vec::new(),
                            false,
                        ),
                        "unwrap_or" => (
                            BuiltinOperationIr::ResultUnwrapOr,
                            ok.clone(),
                            operation_type_arguments,
                            vec![ok.clone()],
                            false,
                        ),
                        _ => return None,
                    }
                }
                IrType::I32 => match method {
                    "to_string" => (
                        BuiltinOperationIr::I32ToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::I64 => match method {
                    "to_string" => (
                        BuiltinOperationIr::I64ToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::F32 => match method {
                    "to_string" => (
                        BuiltinOperationIr::F32ToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::F64 => match method {
                    "to_string" => (
                        BuiltinOperationIr::F64ToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::Bool => match method {
                    "to_string" => (
                        BuiltinOperationIr::BoolToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                IrType::Rune => match method {
                    "to_string" => (
                        BuiltinOperationIr::RuneToString,
                        IrType::String,
                        Vec::new(),
                        Vec::new(),
                        false,
                    ),
                    _ => return None,
                },
                _ => return None,
            };
        let span = source_range(&self.module.source, whole.range);
        if builtin_operation_mutates(operation)
            && let TypedExpressionKind::Reference(receiver_definition) = &receiver.kind
            && self.iterated_collections.contains(receiver_definition)
        {
            self.type_error(
                span.clone(),
                "the iterated collection is mutated inside its own `for` loop; this mutation is statically provable and rejected (indirect mutation is guarded by the runtime mutation-epoch trap)",
            );
        }
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
            | SurfaceType::Set(_)
            | SurfaceType::Token(_)
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
                (matches!(
                    definition.kind,
                    DefinitionKind::Parameter
                        | DefinitionKind::Local
                        | DefinitionKind::Const
                        | DefinitionKind::Variant
                ) || self.analyzer.is_repl_state_field(definition.id))
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
                binary_result(
                    operator.kind,
                    &left,
                    &self.analyzer.definitions,
                    &self.analyzer.type_metadata,
                    &self.analyzer.variant_payloads,
                    &self.analyzer.host_types,
                )
            }
            ExpressionKind::Call {
                callee,
                type_arguments,
                arguments,
            } => {
                if let ExpressionKind::Name(path) = &callee.kind
                    && let [namespace, name] = path.segments.as_slice()
                {
                    match (namespace.text.as_str(), name.text.as_str()) {
                        ("Option", "Some") => {
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
                        ("Result", "Ok" | "Err") if type_arguments.len() == 2 => {
                            return Some(IrType::Result(
                                Box::new(self.infer_type_ref(&type_arguments[0])?),
                                Box::new(self.infer_type_ref(&type_arguments[1])?),
                            ));
                        }
                        ("Result", "Ok" | "Err") => return None,
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
                    | IrType::Error
                    | IrType::Named(_)
                    | IrType::Option(_)
                    | IrType::Result(_, _)
                    | IrType::Set(_)
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
            ExpressionKind::Await { operand } => {
                self.infer_expression_type(operand, allow_numeric_literals)
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
                    | IrType::Error
                    | IrType::Named(_)
                    | IrType::Option(_)
                    | IrType::Array(_)
                    | IrType::Map(_, _)
                    | IrType::Set(_)
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
                                | DefinitionKind::HostContract
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
                    ("Set", [inner]) => Some(IrType::Set(Box::new(inner.clone()))),
                    ("Token", [inner]) => {
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
            TypeKind::Set(inner) => Some(IrType::Set(Box::new(self.infer_type_ref(inner)?))),
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
        if let ExpressionKind::Name(path) = &callee.kind {
            let name = path.text();
            if let Some(intrinsic) =
                self.check_migration_intrinsic(whole, &name, type_arguments, arguments, expected)
            {
                return intrinsic;
            }
        }
        if let ExpressionKind::Member { receiver, member } = &callee.kind {
            if let ExpressionKind::Name(path) = &receiver.kind
                && let [root] = path.segments.as_slice()
                && matches!(root.text.as_str(), "old" | "new")
            {
                let name = format!("{}.{}", root.text, member.text);
                if let Some(intrinsic) = self.check_migration_intrinsic(
                    whole,
                    &name,
                    type_arguments,
                    arguments,
                    expected,
                ) {
                    return intrinsic;
                }
            }
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
            self.check_builtin_constructor_call(whole, path, type_arguments, arguments, expected)
        {
            return expression;
        }
        if let Some(expression) =
            self.check_builtin_variant_call(whole, path, type_arguments, arguments, expected)
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
                "async call result must be consumed with postfix `.await`",
                "append `.await` to the call inside an `async fn`, for example `call().await`",
            );
        }
        if let Some(caller) = self.current_function {
            self.analyzer
                .call_edges
                .entry(caller)
                .or_default()
                .insert(definition);
        }
        if let Some((contract, host_function)) = host {
            self.mark_restricted(RestrictedOperation::Host);
            for capability in &host_function.required_capabilities {
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
                    contract,
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
        name: &str,
        type_arguments: &[TypeRef],
        arguments: &[Expression],
        expected: Option<&IrType>,
    ) -> Option<TypedExpressionIr> {
        if !matches!(
            name,
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
        let expression = match name {
            "old.get" => {
                if type_arguments.len() > 1 || arguments.len() != 1 {
                    return Some(malformed(
                        self,
                        "one state identity and at most one explicit result type",
                    ));
                }
                let value_type = if let Some(ty) = type_arguments.first() {
                    self.analyzer.resolve_type_ref(&self.module, ty)
                } else if let Some(expected) = expected {
                    expected.clone()
                } else {
                    self.type_error(
                        span.clone(),
                        "`old.get` result type must be explicit or inferred from context",
                    );
                    return Some(self.error_expression(span));
                };
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
                if type_arguments.len() > 1 || arguments.len() != 2 {
                    return Some(malformed(
                        self,
                        "an old object, a @state Class field, and at most one explicit field type",
                    ));
                }
                let object = self.check_expression(&arguments[0], None);
                let Some(field) = self.migration_field(&arguments[1]) else {
                    return Some(self.error_expression(span));
                };
                let field_type = self.analyzer.definitions[field.0 as usize].ty.clone();
                let value_type = type_arguments.first().map_or_else(
                    || field_type.clone(),
                    |ty| self.analyzer.resolve_type_ref(&self.module, ty),
                );
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
                if type_arguments.len() > 1 || arguments.len() != 1 {
                    return Some(malformed(
                        self,
                        "one state identity and at most one explicit @state Class type",
                    ));
                }
                let ty = if let Some(ty) = type_arguments.first() {
                    self.analyzer.resolve_type_ref(&self.module, ty)
                } else if let Some(expected) = expected {
                    expected.clone()
                } else {
                    self.type_error(
                        span.clone(),
                        "`new.create` result type must be explicit or inferred from context",
                    );
                    return Some(self.error_expression(span));
                };
                let IrType::Named(state_type) = ty else {
                    self.type_error(span.clone(), "`new.create` requires a @state Class type");
                    return Some(self.error_expression(span));
                };
                if !self
                    .analyzer
                    .state_types
                    .iter()
                    .any(|state| state.definition == state_type)
                {
                    self.type_error(span.clone(), "`new.create` requires a @state Class type");
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
                        "a new object, a @state Class field, and a field value",
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
                        "`replace` requires a newly created @state Class object",
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

    #[allow(clippy::too_many_lines)]
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
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
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
            TypedStatementIr::DynamicRangeFor {
                binding,
                start,
                end,
                body,
                max_iterations,
            } => {
                self.apply_migration_expression(&mut paths, start);
                self.apply_migration_expression(&mut paths, end);
                let start = constant_i32_expression(start, &self.analyzer.const_values);
                self.analyze_migration_dynamic_loop(
                    Some(*binding),
                    start,
                    body,
                    *max_iterations,
                    paths,
                )
            }
            TypedStatementIr::CollectionFor {
                iterable,
                body,
                max_iterations,
                ..
            } => {
                self.apply_migration_expression(&mut paths, iterable);
                // Collection elements are runtime values with no compile-time constant
                // binding; iterate the body conservatively up to the loop limit.
                self.analyze_migration_dynamic_loop(None, None, body, *max_iterations, paths)
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
            TypedStatementIr::Yield { .. } => MigrationFlow {
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

    /// Conservative migration flow for a loop whose iteration count is a
    /// runtime value: every iteration may exit normally and the loop limit is
    /// the fixed upper bound, deduplicating paths like `analyze_migration_while`.
    fn analyze_migration_dynamic_loop(
        &mut self,
        binding: Option<DefinitionId>,
        start: Option<i32>,
        body: &TypedBlockIr,
        max_iterations: u32,
        input: MigrationPaths,
    ) -> MigrationFlow {
        let mut flow = MigrationFlow::default();
        let mut heads = input;
        let mut seen = MigrationPaths::new();
        for iteration in 0..=max_iterations {
            heads.retain(|path| seen.insert(path.clone()));
            if heads.is_empty() {
                break;
            }
            flow.normal.extend(heads.iter().cloned());
            if iteration == max_iterations {
                break;
            }
            let previous = binding.zip(start).and_then(|(binding, start)| {
                i32::try_from(iteration)
                    .ok()
                    .and_then(|offset| start.checked_add(offset))
                    .map(|value| {
                        self.analyzer
                            .const_values
                            .insert(binding, ConstValue::I32(value))
                    })
            });
            let body_flow = self.analyze_migration_block(body, heads);
            if let (Some(binding), Some(previous)) = (binding, previous) {
                if let Some(previous) = previous {
                    self.analyzer.const_values.insert(binding, previous);
                } else {
                    self.analyzer.const_values.remove(&binding);
                }
            }
            flow.normal.extend(body_flow.breaks);
            flow.returns.extend(body_flow.returns);
            heads = body_flow.normal;
            heads.extend(body_flow.continues);
        }
        flow
    }

    fn apply_migration_place(&mut self, paths: &mut MigrationPaths, place: &TypedPlaceIr) {
        match place {
            TypedPlaceIr::Definition(_) => {}
            TypedPlaceIr::Field { base, .. } => {
                self.apply_migration_place(paths, base);
            }
            TypedPlaceIr::ClassField { object: base, .. }
            | TypedPlaceIr::StateField { base, .. } => {
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
            | TypedExpressionKind::PersistentStateGet { .. }
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
            TypedExpressionKind::ClassConstruct { fields, update, .. } => {
                if let Some(base) = update {
                    self.apply_migration_expression(paths, base);
                }
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
                "migration field must be a qualified @state Class field",
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
                "migration field must belong to a @state Class type",
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
        let (name, variant) = match path.segments.as_slice() {
            [namespace, name] if namespace.text == "Option" && name.text == "Some" => {
                (name, BuiltinVariantIr::OptionSome)
            }
            [namespace, name] if namespace.text == "Result" && name.text == "Ok" => {
                (name, BuiltinVariantIr::ResultOk)
            }
            [namespace, name] if namespace.text == "Result" && name.text == "Err" => {
                (name, BuiltinVariantIr::ResultErr)
            }
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
                    if expected.is_some_and(contains_ir_error) {
                        (IrType::Error, IrType::Error)
                    } else {
                        self.type_error(
                            span.clone(),
                            "`Ok`/`Err` requires an expected Result<T, E> or two type arguments",
                        );
                        if variant == BuiltinVariantIr::ResultOk {
                            (payload_ty.clone(), IrType::Unit)
                        } else {
                            (IrType::Unit, payload_ty.clone())
                        }
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

    #[allow(clippy::too_many_lines)]
    fn check_construct(
        &mut self,
        whole: &Expression,
        ty: &ast::QualifiedName,
        fields: &[ast::FieldInitializer],
        update: Option<&Expression>,
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        if let Some(variant) = self.analyzer.lookup_symbol_path(&self.module, ty)
            && self.analyzer.definitions[variant.0 as usize].kind == DefinitionKind::Variant
        {
            let Some(variant) =
                self.analyzer
                    .resolve_symbol_path(&self.module, ty, SymbolUse::Value)
            else {
                return self.error_expression(span);
            };
            if update.is_some() {
                self.type_error(
                    span.clone(),
                    "Enum variant constructors do not support `..base`",
                );
            }
            let Some(named) = self
                .analyzer
                .type_metadata
                .values()
                .find_map(|metadata| metadata.variant_fields.get(&variant).cloned())
            else {
                self.type_error(
                    span.clone(),
                    "brace construction requires an Enum variant with named payload fields",
                );
                return self.error_expression(span);
            };
            let values = self.check_named_variant_fields(variant, &named, fields);
            let payload = match values.len() {
                0 => None,
                1 => values.into_iter().next().map(|(_, value)| Box::new(value)),
                _ => {
                    let values = values
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>();
                    Some(Box::new(TypedExpressionIr {
                        ty: IrType::Tuple(values.iter().map(|value| value.ty.clone()).collect()),
                        effect: expression_effect(&values),
                        span: span.clone(),
                        kind: TypedExpressionKind::Tuple(values),
                    }))
                }
            };
            let ty = self.analyzer.definitions[variant.0 as usize].ty.clone();
            let IrType::Named(enum_definition) = ty.clone() else {
                return self.error_expression(span);
            };
            return TypedExpressionIr {
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
            };
        }
        let Some(definition) = self
            .analyzer
            .resolve_symbol_path(&self.module, ty, SymbolUse::Type)
        else {
            return self.error_expression(span);
        };
        let definition_kind = self.analyzer.definitions[definition.0 as usize].kind;
        let is_host_struct = definition_kind == DefinitionKind::HostContract
            && self.analyzer.host_types.iter().any(|host_type| {
                host_type.definition == definition && host_type.kind == ExternalTypeKind::Struct
            });
        if definition_kind != DefinitionKind::Struct && !is_host_struct {
            self.type_error(
                span.clone(),
                "Class construction requires `new`; a Struct constructor cannot name this type",
            );
            return self.error_expression(span);
        }
        let checked_fields = self.check_fields(definition, fields);
        self.check_missing_fields(whole, definition, &checked_fields, update.is_some());
        if let Some(update) = update {
            let base = self.check_expression(update, Some(&IrType::Named(definition)));
            self.expect_type(&base.ty, &IrType::Named(definition), &base.span);
            let effect = checked_fields
                .iter()
                .fold(base.effect, |effect, (_, value)| {
                    max_effect(effect, value.effect)
                });
            TypedExpressionIr {
                ty: IrType::Named(definition),
                effect,
                span,
                kind: TypedExpressionKind::Update {
                    base: Box::new(base),
                    fields: checked_fields,
                },
            }
        } else {
            let effect = checked_fields
                .iter()
                .fold(IrEffect::Immediate, |effect, (_, value)| {
                    max_effect(effect, value.effect)
                });
            TypedExpressionIr {
                ty: IrType::Named(definition),
                effect,
                span,
                kind: TypedExpressionKind::Construct {
                    definition,
                    fields: checked_fields,
                },
            }
        }
    }

    fn check_named_variant_fields(
        &mut self,
        variant: DefinitionId,
        named: &BTreeMap<String, DefinitionId>,
        fields: &[ast::FieldInitializer],
    ) -> Vec<(DefinitionId, TypedExpressionIr)> {
        let mut values = Vec::new();
        let mut seen = BTreeSet::new();
        for field in fields {
            let Some(definition) = named.get(&field.name.text).copied() else {
                self.analyzer.push_source_error(
                    ErrorCode::NX2501,
                    &self.module.source,
                    byte_range(field.range),
                    format!("unknown variant field `{}`", field.name.text),
                    "field is not present on this Enum variant",
                );
                continue;
            };
            if !seen.insert(definition) {
                self.type_error(
                    source_range(&self.module.source, field.range),
                    "duplicate Enum variant field initializer",
                );
            }
            let expected = self.analyzer.definitions[definition.0 as usize].ty.clone();
            let value = self.check_expression(&field.value, Some(&expected));
            self.expect_type(&value.ty, &expected, &value.span);
            values.push((definition, value));
        }
        let order = self
            .analyzer
            .type_metadata
            .values()
            .find_map(|metadata| metadata.variant_field_order.get(&variant))
            .cloned()
            .unwrap_or_default();
        let missing = order
            .iter()
            .filter(|definition| !seen.contains(definition))
            .map(|definition| {
                self.analyzer.definitions[definition.0 as usize]
                    .name
                    .clone()
            })
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            self.type_error(
                source_range(
                    &self.module.source,
                    fields
                        .first()
                        .map_or(TextRange::default(), |field| field.range),
                ),
                &format!("missing Enum variant fields: {}", missing.join(", ")),
            );
        }
        values.sort_by_key(|(definition, _)| {
            order
                .iter()
                .position(|candidate| candidate == definition)
                .unwrap_or(usize::MAX)
        });
        values
    }

    fn check_class_construct(
        &mut self,
        whole: &Expression,
        definition: DefinitionId,
        fields: &[ast::FieldInitializer],
        update: Option<&Expression>,
    ) -> TypedExpressionIr {
        let span = source_range(&self.module.source, whole.range);
        if self.analyzer.definitions[definition.0 as usize].kind != DefinitionKind::Class {
            self.type_error(
                span.clone(),
                "`new` requires a Class type; Struct values omit `new`",
            );
            return self.error_expression(span);
        }
        if self.is_state_definition(definition) {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(whole.range),
                "@state Class values cannot be constructed directly",
                "use new.create<T> inside a migration or obtain a value through StateHandle<T>",
            );
            return self.error_expression(span);
        }
        let fields = self.check_fields(definition, fields);
        self.check_missing_fields(whole, definition, &fields, update.is_some());
        let update = update.map(|update| {
            let base = self.check_expression(update, Some(&IrType::Named(definition)));
            self.expect_type(&base.ty, &IrType::Named(definition), &base.span);
            Box::new(base)
        });
        let effect = fields.iter().fold(
            update
                .as_ref()
                .map_or(IrEffect::Immediate, |base| base.effect),
            |effect, (_, value)| max_effect(effect, value.effect),
        );
        TypedExpressionIr {
            ty: IrType::Named(definition),
            effect,
            span,
            kind: TypedExpressionKind::ClassConstruct {
                definition,
                fields,
                update,
            },
        }
    }

    fn check_missing_fields(
        &mut self,
        whole: &Expression,
        definition: DefinitionId,
        fields: &[(DefinitionId, TypedExpressionIr)],
        has_update: bool,
    ) {
        if has_update {
            return;
        }
        let present = fields
            .iter()
            .map(|(definition, _)| *definition)
            .collect::<BTreeSet<_>>();
        let missing = self
            .analyzer
            .type_metadata
            .get(&definition)
            .map(|metadata| {
                metadata
                    .field_order
                    .iter()
                    .filter(|field| !present.contains(field))
                    .map(|field| self.analyzer.definitions[field.0 as usize].name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !missing.is_empty() {
            self.analyzer.push_source_error(
                ErrorCode::NX2101,
                &self.module.source,
                byte_range(whole.range),
                format!("missing constructor fields: {}", missing.join(", ")),
                "initialize every field or provide a `..base` update",
            );
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
        let [namespace, name] = path.segments.as_slice() else {
            return None;
        };
        let family = namespace.text.as_str();
        let (variant, payload_type) = match (family, name.text.as_str(), expected) {
            ("Option", "Some", IrType::Option(inner)) => {
                (BuiltinVariantIr::OptionSome, Some(inner.as_ref().clone()))
            }
            ("Option", "None", IrType::Option(_)) => (BuiltinVariantIr::OptionNone, None),
            ("Result", "Ok", IrType::Result(ok, _)) => {
                (BuiltinVariantIr::ResultOk, Some(ok.as_ref().clone()))
            }
            ("Result", "Err", IrType::Result(_, error)) => {
                (BuiltinVariantIr::ResultErr, Some(error.as_ref().clone()))
            }
            ("Option", "Some" | "None", _) => {
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
            ("Result", "Ok" | "Err", _) => {
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

    #[allow(clippy::too_many_lines)]
    fn check_place(&mut self, expression: &Expression) -> Option<TypedPlaceIr> {
        match &expression.kind {
            ExpressionKind::Name(path) => {
                if path.segments.len() != 1 {
                    self.type_error(
                        source_range(&self.module.source, expression.range),
                        "a namespace-qualified value is not an assignable binding",
                    );
                    return None;
                }
                let definition = self.local(&path.segments[0].text).or_else(|| {
                    self.analyzer
                        .resolve_symbol_path(&self.module, path, SymbolUse::Value)
                })?;
                if self.analyzer.is_repl_state_field(definition) {
                    let mutable = self
                        .analyzer
                        .repl_environment_definition
                        .and_then(|environment| self.analyzer.type_metadata.get(&environment))
                        .and_then(|metadata| metadata.field_mutability.get(&definition))
                        .copied()
                        .unwrap_or(false);
                    if !mutable {
                        self.analyzer.push_source_error(
                            ErrorCode::NX2501,
                            &self.module.source,
                            byte_range(expression.range),
                            format!("binding `{}` is immutable", path.segments[0].text),
                            "declare it with `let mut` to allow assignment",
                        );
                        return None;
                    }
                    self.mark_restricted(RestrictedOperation::PersistentState);
                    return Some(TypedPlaceIr::StateField {
                        base: Box::new(self.repl_environment_expression(source_range(
                            &self.module.source,
                            expression.range,
                        ))),
                        field: definition,
                    });
                }
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
                if self.iterated_collections.contains(&definition) {
                    self.type_error(
                        source_range(&self.module.source, expression.range),
                        &format!(
                            "the iterated collection `{}` cannot be reassigned inside its own `for` loop",
                            path.segments[0].text
                        ),
                    );
                    return None;
                }
                if !self.mutable_bindings.contains(&definition) {
                    self.analyzer.push_source_error(
                        ErrorCode::NX2501,
                        &self.module.source,
                        byte_range(expression.range),
                        format!("binding `{}` is immutable", path.segments[0].text),
                        "declare it with `let mut` to allow rebinding or Struct field updates",
                    );
                    return None;
                }
                Some(TypedPlaceIr::Definition(definition))
            }
            ExpressionKind::Member { receiver, member } => {
                let object = self.check_expression(receiver, None);
                let IrType::Named(owner) = object.ty else {
                    self.type_error(
                        source_range(&self.module.source, expression.range),
                        "field assignment requires a Struct or Class receiver",
                    );
                    return None;
                };
                let value = self.check_field_access(object, member, expression.range)?;
                let TypedExpressionKind::Field {
                    base: object,
                    field,
                } = value.kind
                else {
                    return None;
                };
                if self.analyzer.definitions[owner.0 as usize].kind == DefinitionKind::Class {
                    let mutable = self
                        .analyzer
                        .type_metadata
                        .get(&owner)
                        .and_then(|metadata| metadata.field_mutability.get(&field))
                        .copied()
                        .unwrap_or(false);
                    if !mutable {
                        self.analyzer.push_source_error(
                            ErrorCode::NX2501,
                            &self.module.source,
                            byte_range(member.range),
                            format!("class field `{}` is immutable", member.text),
                            "declare the Class field with `mut` to allow assignment",
                        );
                        return None;
                    }
                    Some(TypedPlaceIr::ClassField { object, field })
                } else {
                    let base = self.check_place(receiver)?;
                    Some(TypedPlaceIr::Field {
                        base: Box::new(base),
                        field,
                    })
                }
            }
            ExpressionKind::Index { receiver, index } => {
                let base = self.check_expression(receiver, None);
                let index_type = match &base.ty {
                    IrType::Array(_) | IrType::Buffer(_) => IrType::I32,
                    IrType::Map(key, _) => key.as_ref().clone(),
                    IrType::Error => return None,
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
                    | IrType::Set(_)
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

    fn record_suppressed(&mut self) {
        self.analyzer.diagnostics.record_suppressed(
            self.analyzer
                .poison_cause
                .as_deref()
                .unwrap_or("caused by a previous error"),
        );
    }

    fn expect_type(&mut self, actual: &IrType, expected: &IrType, span: &SourceRange) {
        if contains_ir_error(actual) || contains_ir_error(expected) {
            self.record_suppressed();
            return;
        }
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
                            display_ir_type(actual, &self.analyzer.definitions),
                            display_ir_type(expected, &self.analyzer.definitions)
                        )
                    } else {
                        format!(
                            "expected {}, found {}",
                            display_ir_type(expected, &self.analyzer.definitions),
                            display_ir_type(actual, &self.analyzer.definitions)
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
                .with_note(format!(
                    "expected type: `{}`",
                    display_ir_type(expected, &self.analyzer.definitions)
                ))
                .with_note(format!(
                    "actual type: `{}`",
                    display_ir_type(actual, &self.analyzer.definitions)
                )),
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
            ty: IrType::Error,
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
                host.required_entrypoints
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

/// A did-you-mean suggestion for an unresolved symbol name, or the unsigned-integer family
/// special case. Candidates are ranked by relevance layer (current module first, then builtins
/// for type positions, then imported definitions) and bounded edit distance; single-character
/// names are too ambiguous to suggest.
fn unknown_symbol_fix(
    name: &str,
    source: &SourceKey,
    range: ByteRange,
    usage: SymbolUse,
    module: &SourceModuleKey,
    definitions: &[Definition],
    builtin_types: &BTreeMap<String, DefinitionId>,
) -> Option<TextEditSuggestion> {
    const PRIMITIVES: &[&str] = &[
        "bool",
        "rune",
        "string",
        "unit",
        "i8",
        "i16",
        "i32",
        "i64",
        "f32",
        "f64",
        "Option",
        "Result",
        "Array",
        "Map",
        "Tuple",
        "Token",
        "Snapshot",
        "Buffer",
        "StateHandle",
    ];
    if name.chars().count() == 1 {
        return None;
    }
    if matches!(name, "u8" | "u16" | "u32" | "u64") {
        return Some(TextEditSuggestion::message(
            "Nexa has no unsigned integer types; use `i8`, `i16`, `i32`, or `i64`",
        ));
    }
    let is_type_kind = |kind: DefinitionKind| {
        matches!(
            kind,
            DefinitionKind::Struct
                | DefinitionKind::Enum
                | DefinitionKind::Class
                | DefinitionKind::HostContract
        )
    };
    let is_value_kind = |kind: DefinitionKind| {
        matches!(
            kind,
            DefinitionKind::Function
                | DefinitionKind::Task
                | DefinitionKind::Const
                | DefinitionKind::Field
                | DefinitionKind::Variant
                | DefinitionKind::Parameter
                | DefinitionKind::Local
                | DefinitionKind::HostFunction
        )
    };
    let mut best: Option<(u8, usize, usize, &str)> = None;
    for definition in definitions {
        let in_module =
            definition.package_id == module.package && definition.module == module.module;
        let relevant = match usage {
            SymbolUse::Type => is_type_kind(definition.kind),
            SymbolUse::Value | SymbolUse::Callable => is_value_kind(definition.kind),
        };
        if relevant {
            let layer = if in_module { 0 } else { 2 };
            let distance = edit_distance(name, &definition.name, 2);
            if distance <= 2 {
                let prefix = common_prefix_length(name, &definition.name);
                if best.is_none_or(|(best_layer, best_distance, best_prefix, _)| {
                    (layer, distance) < (best_layer, best_distance)
                        || (layer == best_layer
                            && distance == best_distance
                            && prefix > best_prefix)
                }) {
                    best = Some((layer, distance, prefix, &definition.name));
                }
            }
        }
    }
    if usage == SymbolUse::Type {
        for candidate in builtin_types
            .keys()
            .map(String::as_str)
            .chain(PRIMITIVES.iter().copied())
        {
            let distance = edit_distance(name, candidate, 2);
            if distance <= 2 {
                let prefix = common_prefix_length(name, candidate);
                if best.is_none_or(|(best_layer, best_distance, best_prefix, _)| {
                    (1, distance) < (best_layer, best_distance)
                        || (best_layer == 1 && distance == best_distance && prefix > best_prefix)
                }) {
                    best = Some((1, distance, prefix, candidate));
                }
            }
        }
    }
    let (_, _, _, candidate) = best?;
    Some(TextEditSuggestion::replacement(
        format!("did you mean `{candidate}`?"),
        source_identity(source),
        range,
        candidate,
    ))
}

/// Length of the shared character prefix of two names, used to break did-you-mean ties.
fn common_prefix_length(left: &str, right: &str) -> usize {
    left.chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .count()
}

/// Classic bounded Levenshtein distance; early-exits beyond `max_distance`.
fn edit_distance(left: &str, right: &str, max_distance: usize) -> usize {
    if left == right {
        return 0;
    }
    if left.is_empty() {
        return right.len().min(max_distance + 1);
    }
    if right.is_empty() {
        return left.len().min(max_distance + 1);
    }
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (row, left_character) in left.chars().enumerate() {
        current[0] = row + 1;
        let mut row_min = current[0];
        for (column, right_character) in right.chars().enumerate() {
            current[column + 1] = if left_character == right_character {
                previous[column]
            } else {
                previous[column]
                    .min(previous[column + 1])
                    .min(current[column])
                    + 1
            };
            row_min = row_min.min(current[column + 1]);
        }
        if row_min > max_distance {
            return max_distance + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn syntax_error_code(kind: nexa_syntax::SyntaxErrorKind) -> ErrorCode {
    match kind {
        nexa_syntax::SyntaxErrorKind::UnexpectedCharacter => ErrorCode::NX1001,
        _ => ErrorCode::NX1002,
    }
}

fn declaration_surface(
    declaration: &ast::Declaration,
) -> Option<(String, DefinitionKind, SymbolKind, IrEffect)> {
    match &declaration.kind {
        DeclarationKind::Function(function) => {
            let effect = function_semantic_effect(declaration, function);
            let kind = if function.is_async {
                DefinitionKind::Task
            } else {
                DefinitionKind::Function
            };
            let symbol = if function.is_async {
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

fn function_semantic_effect(
    declaration: &ast::Declaration,
    function: &ast::FunctionDeclaration,
) -> IrEffect {
    if function.is_async {
        return IrEffect::Task;
    }
    for (attribute, effect) in [
        ("immediate", IrEffect::Immediate),
        ("migration", IrEffect::Migration),
        ("activation", IrEffect::Activation),
        ("cleanup", IrEffect::Cleanup),
        ("test", IrEffect::Immediate),
    ] {
        if has_attribute(&declaration.attributes, attribute) {
            return effect;
        }
    }
    IrEffect::Ordinary
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

fn is_snake_case(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_lowercase)
        && bytes.last().is_some_and(|byte| *byte != b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
        && !bytes.windows(2).any(|pair| pair == b"__")
}

fn is_screaming_snake_case(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_uppercase)
        && bytes.last().is_some_and(|byte| *byte != b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        && !bytes.windows(2).any(|pair| pair == b"__")
}

fn is_pascal_case(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn snake_case_name(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut output = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if character.is_ascii_uppercase() {
            let previous_is_lower_or_digit = index > 0
                && (characters[index - 1].is_ascii_lowercase()
                    || characters[index - 1].is_ascii_digit());
            let acronym_boundary = index > 0
                && characters[index - 1].is_ascii_uppercase()
                && characters
                    .get(index + 1)
                    .is_some_and(char::is_ascii_lowercase);
            if !output.is_empty() && (previous_is_lower_or_digit || acronym_boundary) {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
        } else {
            output.push(character);
        }
    }
    output
}

fn use_path_text(usage: &ast::UseDeclaration) -> String {
    std::iter::once(usage.root.name.text.as_str())
        .chain(usage.segments.iter().map(|segment| segment.text.as_str()))
        .collect::<Vec<_>>()
        .join("::")
}

fn use_path_range(usage: &ast::UseDeclaration) -> TextRange {
    TextRange::new(
        usage.root.name.range.start,
        usage
            .segments
            .last()
            .map_or(usage.root.name.range.end, |segment| segment.range.end),
    )
}

/// Collection-mutating builtin operations. Directly calling one on the local
/// binding iterated by an active `for` loop is a statically provable
/// mutation hazard; the runtime mutation-epoch trap covers every other path.
fn builtin_operation_mutates(operation: BuiltinOperationIr) -> bool {
    matches!(
        operation,
        BuiltinOperationIr::ArraySet
            | BuiltinOperationIr::ArrayPush
            | BuiltinOperationIr::ArrayPop
            | BuiltinOperationIr::ArrayInsert
            | BuiltinOperationIr::ArrayRemove
            | BuiltinOperationIr::ArrayClear
            | BuiltinOperationIr::MapSet
            | BuiltinOperationIr::MapInsert
            | BuiltinOperationIr::MapRemove
            | BuiltinOperationIr::MapClear
            | BuiltinOperationIr::SetInsert
            | BuiltinOperationIr::SetRemove
            | BuiltinOperationIr::SetClear
            | BuiltinOperationIr::BufferSet
    )
}

fn statement_contains_await(statement: &Statement) -> bool {
    match &statement.kind {
        StatementKind::Bind { value, .. }
        | StatementKind::Defer(value)
        | StatementKind::Expression(value) => expression_contains_await(value),
        StatementKind::Assign { target, value }
        | StatementKind::CompoundAssign { target, value, .. } => {
            expression_contains_await(target) || expression_contains_await(value)
        }
        StatementKind::Return(value) => value.as_ref().is_some_and(expression_contains_await),
        StatementKind::If {
            condition,
            then_block,
            else_branch,
        } => {
            expression_contains_await(condition)
                || block_contains_await(then_block)
                || else_branch.as_ref().is_some_and(|branch| match branch {
                    ElseBranch::Block(block) => block_contains_await(block),
                    ElseBranch::If(statement) => statement_contains_await(statement),
                })
        }
        StatementKind::While { condition, body } => {
            expression_contains_await(condition) || block_contains_await(body)
        }
        StatementKind::For { iterable, body, .. } => {
            let iterable = match iterable {
                ForIterable::Range { start, end, .. } => {
                    expression_contains_await(start) || expression_contains_await(end)
                }
                ForIterable::Expression(expression) => expression_contains_await(expression),
            };
            iterable || block_contains_await(body)
        }
        StatementKind::Break
        | StatementKind::Continue
        | StatementKind::Yield
        | StatementKind::Error => false,
    }
}

fn block_contains_await(block: &ast::Block) -> bool {
    block.statements.iter().any(statement_contains_await)
        || block
            .tail
            .as_ref()
            .is_some_and(|expression| expression_contains_await(expression))
}

fn expression_contains_await(expression: &Expression) -> bool {
    match &expression.kind {
        ExpressionKind::Await { .. } => true,
        ExpressionKind::Tuple(values) | ExpressionKind::Array(values) => {
            values.iter().any(expression_contains_await)
        }
        ExpressionKind::Unary { operand, .. } | ExpressionKind::Try(operand) => {
            expression_contains_await(operand)
        }
        ExpressionKind::Binary { left, right, .. } => {
            expression_contains_await(left) || expression_contains_await(right)
        }
        ExpressionKind::Call {
            callee, arguments, ..
        } => expression_contains_await(callee) || arguments.iter().any(expression_contains_await),
        ExpressionKind::Member { receiver, .. } => expression_contains_await(receiver),
        ExpressionKind::Index { receiver, index } => {
            expression_contains_await(receiver) || expression_contains_await(index)
        }
        ExpressionKind::Construct { fields, update, .. }
        | ExpressionKind::New { fields, update, .. } => {
            fields
                .iter()
                .any(|field| expression_contains_await(&field.value))
                || update
                    .as_ref()
                    .is_some_and(|value| expression_contains_await(value))
        }
        ExpressionKind::Match { value, arms } => {
            expression_contains_await(value)
                || arms.iter().any(|arm| expression_contains_await(&arm.value))
        }
        ExpressionKind::Interpolation(parts) => parts.iter().any(|part| match part {
            InterpolationPart::Text { .. } => false,
            InterpolationPart::Expression(expression) => expression_contains_await(expression),
        }),
        ExpressionKind::Literal(_) | ExpressionKind::Name(_) | ExpressionKind::Error => false,
    }
}

fn typed_expression_contains_await(expression: &TypedExpressionIr) -> bool {
    match &expression.kind {
        TypedExpressionKind::Await(_) => true,
        TypedExpressionKind::Unary { operand, .. } | TypedExpressionKind::Try(operand) => {
            typed_expression_contains_await(operand)
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            typed_expression_contains_await(left) || typed_expression_contains_await(right)
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => {
            arguments.iter().any(typed_expression_contains_await)
        }
        TypedExpressionKind::Construct { fields, .. } => fields
            .iter()
            .any(|(_, value)| typed_expression_contains_await(value)),
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            update
                .as_ref()
                .is_some_and(|value| typed_expression_contains_await(value))
                || fields
                    .iter()
                    .any(|(_, value)| typed_expression_contains_await(value))
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => payload
            .as_ref()
            .is_some_and(|value| typed_expression_contains_await(value)),
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            typed_expression_contains_await(base)
        }
        TypedExpressionKind::Index { base, index } => {
            typed_expression_contains_await(base) || typed_expression_contains_await(index)
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            values.iter().any(typed_expression_contains_await)
        }
        TypedExpressionKind::Match { value, arms } => {
            typed_expression_contains_await(value)
                || arms
                    .iter()
                    .any(|arm| typed_expression_contains_await(&arm.value))
        }
        TypedExpressionKind::Update { base, fields } => {
            typed_expression_contains_await(base)
                || fields
                    .iter()
                    .any(|(_, value)| typed_expression_contains_await(value))
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. }
            | MigrationIntrinsicIr::Replace { target: object, .. } => {
                typed_expression_contains_await(object)
            }
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                typed_expression_contains_await(object) || typed_expression_contains_await(value)
            }
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => false,
        },
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => false,
    }
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

fn state_version(attributes: &[Attribute]) -> Option<u32> {
    let attribute = attributes
        .iter()
        .find(|attribute| attribute.name.text == "state")?;
    let argument = attribute.arguments.iter().find(|argument| {
        argument
            .name
            .as_ref()
            .is_some_and(|name| name.text == "version")
    })?;
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
        ("Buffer", [inner]) => Ok(SurfaceType::Buffer(Box::new(inner.clone()))),
        ("Map", [key, value]) => Ok(SurfaceType::Map(
            Box::new(key.clone()),
            Box::new(value.clone()),
        )),
        ("Set", [inner]) => Ok(SurfaceType::Set(Box::new(inner.clone()))),
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
            if matches!(
                path.segments.as_slice(),
                [namespace, variant]
                    if namespace.text == "Option" && variant.text == "None"
            )
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
    let [namespace, name] = path.segments.as_slice() else {
        return None;
    };
    matches!(
        (namespace.text.as_str(), name.text.as_str()),
        ("Option", "Some") | ("Result", "Ok" | "Err")
    )
    .then_some((name.text.as_str(), type_arguments, arguments))
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
        SurfaceType::Set(inner) => Some(IrType::Set(Box::new(instantiate_inferred_surface_type(
            inner, bindings,
        )?))),
        SurfaceType::Tuple(values) => values
            .iter()
            .map(|value| instantiate_inferred_surface_type(value, bindings))
            .collect::<Option<Vec<_>>>()
            .map(IrType::Tuple),
        SurfaceType::Token(inner) => Some(IrType::ResourceToken(Some(Box::new(
            instantiate_inferred_surface_type(inner, bindings)?,
        )))),
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
        | (SurfaceType::Set(left), IrType::Set(right))
        | (SurfaceType::Snapshot(left), IrType::Snapshot(right))
        | (SurfaceType::Buffer(left), IrType::Buffer(right))
        | (SurfaceType::StateHandle(left), IrType::StateHandle(right))
        | (SurfaceType::Token(left), IrType::ResourceToken(Some(right))) => {
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
        _ => false,
    }
}

fn collect_named_types(ty: &IrType, output: &mut Vec<DefinitionId>) {
    match ty {
        IrType::Named(definition) => output.push(*definition),
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Set(inner)
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
        | IrType::Error
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
            Some(ConstValue::F32(canonical_f32_bits(-f32::from_bits(bits))))
        }
        (UnaryOperatorKind::Negate, ConstValue::F64(bits)) => {
            Some(ConstValue::F64(canonical_f64_bits(-f64::from_bits(bits))))
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
                Operator::Remainder => Some(ConstValue::F32(canonical_f32_bits(
                    deterministic_rem_f32(left, right),
                ))),
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
                Operator::Remainder => Some(ConstValue::F64(canonical_f64_bits(
                    deterministic_rem_f64(left, right),
                ))),
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
            Operator::Equal => Some(ConstValue::Bool(const_values_equal(&left, &right))),
            Operator::NotEqual => Some(ConstValue::Bool(!const_values_equal(&left, &right))),
            _ => None,
        },
    }
}

#[allow(clippy::float_cmp)]
fn const_values_equal(left: &ConstValue, right: &ConstValue) -> bool {
    match (left, right) {
        (ConstValue::Unit, ConstValue::Unit) => true,
        (ConstValue::Bool(left), ConstValue::Bool(right)) => left == right,
        (ConstValue::I32(left), ConstValue::I32(right)) => left == right,
        (ConstValue::I64(left), ConstValue::I64(right)) => left == right,
        (ConstValue::F32(left), ConstValue::F32(right)) => {
            f32::from_bits(*left) == f32::from_bits(*right)
        }
        (ConstValue::F64(left), ConstValue::F64(right)) => {
            f64::from_bits(*left) == f64::from_bits(*right)
        }
        (ConstValue::String(left), ConstValue::String(right)) => left == right,
        (ConstValue::Rune(left), ConstValue::Rune(right)) => left == right,
        (ConstValue::Tuple(left), ConstValue::Tuple(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| const_values_equal(left, right))
        }
        (
            ConstValue::Construct {
                definition: left_definition,
                fields: left_fields,
            },
            ConstValue::Construct {
                definition: right_definition,
                fields: right_fields,
            },
        ) => {
            left_definition == right_definition
                && left_fields.len() == right_fields.len()
                && left_fields.iter().zip(right_fields).all(
                    |((left_field, left), (right_field, right))| {
                        left_field == right_field && const_values_equal(left, right)
                    },
                )
        }
        (
            ConstValue::Variant {
                definition: left_definition,
                values: left_values,
            },
            ConstValue::Variant {
                definition: right_definition,
                values: right_values,
            },
        ) => {
            left_definition == right_definition
                && left_values.len() == right_values.len()
                && left_values
                    .iter()
                    .zip(right_values)
                    .all(|(left, right)| const_values_equal(left, right))
        }
        (
            ConstValue::BuiltinVariant {
                variant: left_variant,
                value: left_value,
            },
            ConstValue::BuiltinVariant {
                variant: right_variant,
                value: right_value,
            },
        ) => {
            left_variant == right_variant
                && match (left_value, right_value) {
                    (None, None) => true,
                    (Some(left), Some(right)) => const_values_equal(left, right),
                    (None, Some(_)) | (Some(_), None) => false,
                }
        }
        _ => false,
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

#[derive(Clone)]
struct InlineLayoutEdge {
    target: DefinitionId,
    span: SourceRange,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineLayoutVisit {
    Visiting,
    Complete,
}

fn collect_inline_value_targets(
    ty: &IrType,
    definitions: &[Definition],
    push: &mut impl FnMut(DefinitionId),
) {
    match ty {
        IrType::Named(definition)
            if definitions
                .get(definition.0 as usize)
                .is_some_and(|definition| {
                    matches!(
                        definition.kind,
                        DefinitionKind::Struct | DefinitionKind::Enum
                    )
                }) =>
        {
            push(*definition);
        }
        IrType::Option(inner) => collect_inline_value_targets(inner, definitions, push),
        IrType::Result(ok, error) => {
            collect_inline_value_targets(ok, definitions, push);
            collect_inline_value_targets(error, definitions, push);
        }
        IrType::Tuple(values) => {
            for value in values {
                collect_inline_value_targets(value, definitions, push);
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
        | IrType::Named(_)
        | IrType::Array(_)
        | IrType::Map(_, _)
        | IrType::Set(_)
        | IrType::HostRequest(_)
        | IrType::ResourceToken(_)
        | IrType::Snapshot(_)
        | IrType::Buffer(_)
        | IrType::StateHandle(_)
        | IrType::Error
        | IrType::TypeParameter(_) => {}
    }
}

fn canonical_inline_cycle(cycle: &[DefinitionId]) -> Vec<DefinitionId> {
    let cycle = cycle
        .first()
        .filter(|first| cycle.last() == Some(first))
        .map_or(cycle, |_| &cycle[..cycle.len().saturating_sub(1)]);
    if cycle.is_empty() {
        return Vec::new();
    }
    (0..cycle.len())
        .map(|offset| {
            cycle[offset..]
                .iter()
                .chain(&cycle[..offset])
                .copied()
                .collect::<Vec<_>>()
        })
        .min()
        .unwrap_or_default()
}

include!("analyzer_helpers.rs");
