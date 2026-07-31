use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use crate::{ArtifactFileId, DeclarationVisibility, ModulePath, PackageId, SourceKey};
use nexa_core::{CanonicalSymbolIdentity, StableId, StableSymbolId};
use nexa_diagnostics::{ByteRange, SourceIdentity};

use crate::{PublicApiFingerprint, StateSchemaFingerprint};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRange {
    pub source: SourceKey,
    pub start: u32,
    pub end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefinitionKind {
    Function,
    Task,
    Struct,
    Enum,
    Class,
    Stateful,
    Const,
    Field,
    Variant,
    Parameter,
    Local,
    HostInterface,
    HostFunction,
    StandardLibrary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IrEffect {
    Ordinary,
    Immediate,
    Task,
    Migration,
    Activation,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrHostFunctionMode {
    Sync,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrCancelPolicy {
    ReturnError,
    CancelTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrAbandonPolicy {
    ReturnError,
    Trap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrCompilationKind {
    Product,
    Test,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldLayoutIr {
    pub definition: DefinitionId,
    pub ty: IrType,
    pub order: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VariantLayoutIr {
    pub definition: DefinitionId,
    pub tag: u32,
    pub payload: Option<IrType>,
}

/// Fully analyzed nominal layout. Source field order and enum tags are ABI data and therefore
/// travel in Typed IR instead of being reconstructed from map iteration or Definition allocation.
#[derive(Clone, Debug, PartialEq)]
pub enum TypedTypeLayoutIr {
    Struct { fields: Vec<FieldLayoutIr> },
    Class { fields: Vec<FieldLayoutIr> },
    Stateful { fields: Vec<FieldLayoutIr> },
    Enum { variants: Vec<VariantLayoutIr> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinVariantIr {
    OptionSome,
    OptionNone,
    ResultOk,
    ResultErr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinOperationIr {
    ArrayNew,
    MapNew,
    StringLen,
    StringByteLen,
    StringEqual,
    StringConcat,
    StringRuneAt,
    StringHash,
    ArrayLen,
    ArrayGet,
    ArraySet,
    ArrayPush,
    ArrayPop,
    ArrayInsert,
    ArrayRemove,
    ArrayClear,
    MapLen,
    MapGet,
    MapSet,
    MapRemove,
    MapContains,
    MapClear,
    BufferLen,
    BufferGet,
    BufferSet,
    BufferSlice,
    BufferCopy,
    StateHandleResolve,
    StateHandleIsAlive,
    StateHandleStableId,
    StateHandleGeneration,
    StateHandleEqual,
    StateHandleHash,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IrType {
    Unit,
    Bool,
    I32,
    I64,
    F32,
    F64,
    String,
    Rune,
    Named(DefinitionId),
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
    /// A generic parameter in compiler-provided standard-library signature metadata.
    ///
    /// Executable source expressions and call-site substitutions must never retain this form.
    TypeParameter(u16),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    pub id: DefinitionId,
    pub package_id: PackageId,
    pub module: ModulePath,
    pub name: String,
    pub kind: DefinitionKind,
    pub visibility: DeclarationVisibility,
    pub ty: IrType,
    pub effect: IrEffect,
    pub span: SourceRange,
    pub canonical_identity: String,
    /// Analysis-assigned persistent/runtime identity for source-visible symbols. Parameters and
    /// lexical locals intentionally have no stable symbol identity.
    pub stable_symbol: Option<StableSymbolIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StableSymbolIdentity {
    pub canonical: CanonicalSymbolIdentity,
    pub runtime_id: StableSymbolId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedReference {
    pub span: SourceRange,
    pub target: DefinitionId,
}

#[derive(Clone, Debug)]
pub struct TypedModuleIr {
    pub package_id: PackageId,
    pub module: ModulePath,
    pub source: SourceKey,
    pub file_id: ArtifactFileId,
    pub syntax: Arc<nexa_syntax::SyntaxTree>,
    pub resolved_references: Arc<[ResolvedReference]>,
    pub declarations: Arc<[TypedDeclarationIr]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedDeclarationIr {
    pub definition: DefinitionId,
    pub body: TypedDeclarationBody,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedDeclarationBody {
    Function(TypedFunctionIr),
    Const(TypedExpressionIr),
    TypeLayout(TypedTypeLayoutIr),
    External,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedFunctionIr {
    pub parameters: Vec<DefinitionId>,
    pub locals: Vec<DefinitionId>,
    pub return_type: IrType,
    pub effect: IrEffect,
    pub body: TypedBlockIr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateFieldIr {
    pub definition: DefinitionId,
    pub ty: IrType,
    pub stable_id: StableSymbolId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StateTypeIr {
    pub definition: DefinitionId,
    pub version: u32,
    pub stable_id: StableSymbolId,
    pub fields: Vec<StateFieldIr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostAsyncResultIr {
    pub result_type: StableId,
    pub success: IrType,
    pub error: IrType,
    pub cancel_policy: IrCancelPolicy,
    pub abandon_policy: IrAbandonPolicy,
    pub cancel_error: Option<u32>,
    pub abandon_error: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostFunctionBindingIr {
    pub definition: DefinitionId,
    pub stable_id: StableId,
    pub import_index: u32,
    pub mode: IrHostFunctionMode,
    pub parameters: Vec<IrType>,
    pub result: IrType,
    pub fuel_cost: u32,
    pub async_result: Option<HostAsyncResultIr>,
    pub source: Option<ExternalSourceRangeIr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostFieldBindingIr {
    pub definition: DefinitionId,
    pub stable_id: StableId,
    pub ty: IrType,
    pub order: u32,
    pub source: Option<ExternalSourceRangeIr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostVariantBindingIr {
    pub definition: DefinitionId,
    pub stable_id: StableId,
    pub tag: u32,
    pub payload: Option<IrType>,
    pub source: Option<ExternalSourceRangeIr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostTypeLayoutIr {
    Opaque,
    Struct { fields: Vec<HostFieldBindingIr> },
    Enum { variants: Vec<HostVariantBindingIr> },
}

/// Exact nominal ABI supplied by the NIDL contract.
///
/// These stable IDs are intentionally distinct from source-symbol identities: Host type ABI is
/// owned by the contract, and code generation must not derive it from a Nexa name.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTypeBindingIr {
    pub definition: DefinitionId,
    pub stable_id: StableId,
    pub layout: HostTypeLayoutIr,
    pub source: Option<ExternalSourceRangeIr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostBindingIr {
    pub interface: DefinitionId,
    pub interface_stable_id: StableId,
    pub namespaces: Vec<HostNamespaceBindingIr>,
    pub types: Vec<HostTypeBindingIr>,
    pub functions: Vec<HostFunctionBindingIr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostNamespaceBindingIr {
    pub package_id: PackageId,
    pub module: ModulePath,
    pub namespace: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportBindingIr {
    pub name: String,
    pub function: DefinitionId,
    pub stable_id: StableId,
    pub parameters: Vec<IrType>,
    pub result: IrType,
    pub effect: IrEffect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestDefinitionIr {
    pub name: String,
    pub function: DefinitionId,
    pub module: ModulePath,
    pub span: SourceRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSourceSnapshotIr {
    pub file_id: ArtifactFileId,
    pub identity: SourceIdentity,
    pub text: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalSourceRangeIr {
    pub file_id: ArtifactFileId,
    pub identity: SourceIdentity,
    pub range: ByteRange,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifecycleBindingsIr {
    pub migration: Option<DefinitionId>,
    pub activation: Option<DefinitionId>,
    pub cleanup: Option<DefinitionId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StandardFunctionBindingIr {
    pub definition: DefinitionId,
    pub intrinsic: nexa_stdlib::Intrinsic,
    pub type_parameters: Vec<String>,
    pub parameters: Vec<IrType>,
    pub result: IrType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackageSemanticMetadata {
    pub entry_module: Option<ModulePath>,
    pub state_types: Arc<[StateTypeIr]>,
    pub host_bindings: Arc<[HostBindingIr]>,
    pub exports: Arc<[ExportBindingIr]>,
    pub tests: Arc<[TestDefinitionIr]>,
    pub external_sources: Arc<[ExternalSourceSnapshotIr]>,
    pub lifecycle: LifecycleBindingsIr,
    pub standard_functions: Arc<[StandardFunctionBindingIr]>,
    pub public_api_fingerprint: PublicApiFingerprint,
    pub state_schema_fingerprint: StateSchemaFingerprint,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TypedBlockIr {
    pub statements: Vec<TypedStatementIr>,
    pub tail: Option<Box<TypedExpressionIr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedStatementIr {
    Let {
        definition: DefinitionId,
        value: Option<TypedExpressionIr>,
    },
    Assign {
        target: TypedPlaceIr,
        value: TypedExpressionIr,
    },
    Expression(TypedExpressionIr),
    Return(Option<TypedExpressionIr>),
    If {
        condition: TypedExpressionIr,
        then_block: TypedBlockIr,
        else_block: Option<TypedBlockIr>,
    },
    While {
        condition: TypedExpressionIr,
        body: TypedBlockIr,
        max_iterations: u32,
    },
    StaticRangeFor {
        binding: DefinitionId,
        start: TypedExpressionIr,
        end: TypedExpressionIr,
        body: TypedBlockIr,
        /// Analyzer-proven upper bound for the number of loop-body executions.
        max_iterations: u32,
    },
    Break,
    Continue,
    /// Schedules an analyzer-generated cleanup function. The cleanup definition has
    /// [`IrEffect::Cleanup`], and `captures` are evaluated when the defer is registered and passed
    /// to that function's parameters.
    Defer {
        cleanup: DefinitionId,
        captures: Vec<TypedExpressionIr>,
    },
    Yield,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedPlaceIr {
    Definition(DefinitionId),
    Field {
        base: Box<TypedExpressionIr>,
        field: DefinitionId,
    },
    Index {
        base: Box<TypedExpressionIr>,
        index: Box<TypedExpressionIr>,
    },
    StateField {
        base: Box<TypedExpressionIr>,
        field: DefinitionId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedExpressionIr {
    pub ty: IrType,
    pub effect: IrEffect,
    pub span: SourceRange,
    pub kind: TypedExpressionKind,
}

/// A semantically resolved state-migration operation.
///
/// State instance identities are source-level migration tokens and intentionally use the
/// Runtime's 64-bit [`StableId`]. They are not declaration identities. Nominal state types and
/// fields, on the other hand, are represented by [`DefinitionId`] and are lowered through
/// [`StateTypeIr`] / [`StateFieldIr`] so the compiler never reconstructs a state ABI from names.
#[derive(Clone, Debug, PartialEq)]
pub enum MigrationIntrinsicIr {
    OldGet {
        identity: StableId,
        value_type: IrType,
    },
    OldFieldGet {
        object: Box<TypedExpressionIr>,
        field: DefinitionId,
        value_type: IrType,
    },
    NewCreate {
        identity: StableId,
        state_type: DefinitionId,
    },
    NewSet {
        object: Box<TypedExpressionIr>,
        field: DefinitionId,
        value: Box<TypedExpressionIr>,
    },
    Preserve {
        identity: StableId,
    },
    Replace {
        identity: StableId,
        target: Box<TypedExpressionIr>,
    },
    Delete {
        identity: StableId,
    },
    Finish,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedExpressionKind {
    Literal(IrLiteral),
    Reference(DefinitionId),
    Unary {
        operator: UnaryOperator,
        operand: Box<TypedExpressionIr>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<TypedExpressionIr>,
        right: Box<TypedExpressionIr>,
    },
    Call {
        callee: DefinitionId,
        arguments: Vec<TypedExpressionIr>,
    },
    /// A compiler-provided standard-library intrinsic instantiated at this call site.
    ///
    /// `type_arguments`, every argument type, and the enclosing expression result type are fully
    /// concrete. The generic signature remains in [`StandardFunctionBindingIr`] for diagnostics
    /// and signature validation, but code generation never reconstructs substitutions from it.
    StandardCall {
        function: DefinitionId,
        intrinsic: nexa_stdlib::Intrinsic,
        type_arguments: Vec<IrType>,
        arguments: Vec<TypedExpressionIr>,
    },
    /// A language-level collection, string, buffer, or state-handle operation.
    ///
    /// Instance receivers are always `arguments[0]`; constructor operations have no receiver.
    /// `type_arguments` contains the fully concrete element/key/value/target types required for
    /// bytecode metadata and is never reconstructed from names by the compiler.
    BuiltinCall {
        operation: BuiltinOperationIr,
        type_arguments: Vec<IrType>,
        arguments: Vec<TypedExpressionIr>,
    },
    HostCall {
        interface: DefinitionId,
        function: DefinitionId,
        arguments: Vec<TypedExpressionIr>,
    },
    Construct {
        definition: DefinitionId,
        fields: Vec<(DefinitionId, TypedExpressionIr)>,
    },
    EnumConstruct {
        enum_definition: DefinitionId,
        variant_definition: DefinitionId,
        payload: Option<Box<TypedExpressionIr>>,
    },
    BuiltinVariant {
        variant: BuiltinVariantIr,
        payload: Option<Box<TypedExpressionIr>>,
    },
    Field {
        base: Box<TypedExpressionIr>,
        field: DefinitionId,
    },
    StateField {
        base: Box<TypedExpressionIr>,
        field: DefinitionId,
    },
    Index {
        base: Box<TypedExpressionIr>,
        index: Box<TypedExpressionIr>,
    },
    Array(Vec<TypedExpressionIr>),
    Tuple(Vec<TypedExpressionIr>),
    StringInterpolation(Vec<TypedExpressionIr>),
    Match {
        value: Box<TypedExpressionIr>,
        arms: Vec<TypedMatchArmIr>,
    },
    Try(Box<TypedExpressionIr>),
    Update {
        base: Box<TypedExpressionIr>,
        fields: Vec<(DefinitionId, TypedExpressionIr)>,
    },
    Migration(MigrationIntrinsicIr),
    Await(Box<TypedExpressionIr>),
    Yield,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedMatchArmIr {
    pub pattern: TypedPatternIr,
    pub value: TypedExpressionIr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedPatternIr {
    pub ty: IrType,
    pub span: SourceRange,
    pub kind: TypedPatternKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedPatternKind {
    Wildcard,
    Binding(DefinitionId),
    Literal(IrLiteral),
    Variant {
        definition: DefinitionId,
        payload: Vec<TypedPatternIr>,
    },
    BuiltinVariant {
        variant: BuiltinVariantIr,
        payload: Option<Box<TypedPatternIr>>,
    },
    Struct {
        definition: DefinitionId,
        fields: Vec<(DefinitionId, TypedPatternIr)>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum IrLiteral {
    Unit,
    Bool(bool),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    String(String),
    Rune(char),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOperator {
    Negate,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
}

/// Codegen-ready package IR. Every source reference targets a compact [`DefinitionId`].
#[derive(Clone, Debug)]
pub struct TypedPackageIr {
    package_id: PackageId,
    analysis_revision: u64,
    compilation_kind: IrCompilationKind,
    definitions: Arc<[Definition]>,
    modules: Arc<[TypedModuleIr]>,
    metadata: Arc<PackageSemanticMetadata>,
}

impl TypedPackageIr {
    pub fn new_product(
        package_id: PackageId,
        analysis_revision: u64,
        definitions: Vec<Definition>,
        modules: Vec<TypedModuleIr>,
        metadata: PackageSemanticMetadata,
    ) -> Result<Self, TypedIrError> {
        Self::construct(
            package_id,
            analysis_revision,
            IrCompilationKind::Product,
            definitions,
            modules,
            metadata,
        )
    }

    pub fn new_test(
        package_id: PackageId,
        analysis_revision: u64,
        definitions: Vec<Definition>,
        modules: Vec<TypedModuleIr>,
        metadata: PackageSemanticMetadata,
    ) -> Result<Self, TypedIrError> {
        Self::construct(
            package_id,
            analysis_revision,
            IrCompilationKind::Test,
            definitions,
            modules,
            metadata,
        )
    }

    fn construct(
        package_id: PackageId,
        analysis_revision: u64,
        compilation_kind: IrCompilationKind,
        definitions: Vec<Definition>,
        modules: Vec<TypedModuleIr>,
        metadata: PackageSemanticMetadata,
    ) -> Result<Self, TypedIrError> {
        for (index, definition) in definitions.iter().enumerate() {
            let expected =
                DefinitionId(u32::try_from(index).map_err(|_| TypedIrError::TooManyDefinitions)?);
            if definition.id != expected {
                return Err(TypedIrError::NonDenseDefinition {
                    expected,
                    actual: definition.id,
                });
            }
        }
        let limit = definitions.len();
        for definition in &definitions {
            if definition.kind == DefinitionKind::StandardLibrary {
                validate_type(&definition.ty, limit)?;
            } else {
                validate_concrete_type(&definition.ty, limit)?;
            }
        }
        if !modules.iter().any(|module| module.package_id == package_id) {
            return Err(TypedIrError::MissingRootPackageModule(package_id));
        }
        let constants = modules
            .iter()
            .flat_map(|module| module.declarations.iter())
            .filter_map(|declaration| match &declaration.body {
                TypedDeclarationBody::Const(expression) => {
                    Some((declaration.definition, expression))
                }
                TypedDeclarationBody::Function(_)
                | TypedDeclarationBody::TypeLayout(_)
                | TypedDeclarationBody::External => None,
            })
            .collect::<BTreeMap<_, _>>();
        let module_packages = modules
            .iter()
            .map(|module| module.package_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for definition in &definitions {
            if !module_packages.contains(&definition.package_id)
                && !matches!(
                    definition.kind,
                    DefinitionKind::HostInterface
                        | DefinitionKind::HostFunction
                        | DefinitionKind::StandardLibrary
                )
            {
                return Err(TypedIrError::DefinitionPackageWithoutModule(
                    definition.package_id.clone(),
                ));
            }
        }
        for module in &modules {
            for reference in module.resolved_references.iter() {
                validate_id(reference.target, limit)?;
            }
            for declaration in module.declarations.iter() {
                validate_id(declaration.definition, limit)?;
                validate_declaration(declaration, limit, &constants)?;
            }
        }
        let standard_functions = validate_metadata(&metadata, &definitions)?;
        let migration_types = migration_type_context(&metadata, &definitions, &modules)?;
        for module in &modules {
            for declaration in module.declarations.iter() {
                validate_standard_calls_in_declaration(
                    declaration,
                    &standard_functions,
                    &migration_types,
                )?;
            }
        }
        Ok(Self {
            package_id,
            analysis_revision,
            compilation_kind,
            definitions: definitions.into(),
            modules: modules.into(),
            metadata: Arc::new(metadata),
        })
    }

    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    #[must_use]
    pub const fn analysis_revision(&self) -> u64 {
        self.analysis_revision
    }

    #[must_use]
    pub const fn compilation_kind(&self) -> IrCompilationKind {
        self.compilation_kind
    }

    #[must_use]
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    #[must_use]
    pub fn modules(&self) -> &[TypedModuleIr] {
        &self.modules
    }

    #[must_use]
    pub fn metadata(&self) -> &PackageSemanticMetadata {
        &self.metadata
    }

    #[must_use]
    pub fn definition(&self, id: DefinitionId) -> Option<&Definition> {
        self.definitions.get(id.0 as usize)
    }
}

#[allow(clippy::too_many_lines)]
fn validate_metadata<'a>(
    metadata: &'a PackageSemanticMetadata,
    definitions: &[Definition],
) -> Result<BTreeMap<DefinitionId, &'a StandardFunctionBindingIr>, TypedIrError> {
    let limit = definitions.len();
    let external_file_ids = metadata
        .external_sources
        .iter()
        .map(|source| source.file_id)
        .collect::<std::collections::BTreeSet<_>>();
    if external_file_ids.len() != metadata.external_sources.len() {
        return Err(TypedIrError::DuplicateExternalFileId);
    }
    for state in metadata.state_types.iter() {
        validate_id(state.definition, limit)?;
        for field in &state.fields {
            validate_id(field.definition, limit)?;
            validate_type(&field.ty, limit)?;
        }
    }
    for host in metadata.host_bindings.iter() {
        validate_id(host.interface, limit)?;
        for ty in &host.types {
            validate_id(ty.definition, limit)?;
            match &ty.layout {
                HostTypeLayoutIr::Opaque => {}
                HostTypeLayoutIr::Struct { fields } => {
                    let mut orders = std::collections::BTreeSet::new();
                    for field in fields {
                        validate_id(field.definition, limit)?;
                        validate_type(&field.ty, limit)?;
                        if !orders.insert(field.order) {
                            return Err(TypedIrError::DuplicateFieldOrder(field.order));
                        }
                        validate_external_range(field.source.as_ref(), &external_file_ids)?;
                    }
                }
                HostTypeLayoutIr::Enum { variants } => {
                    let mut tags = std::collections::BTreeSet::new();
                    for variant in variants {
                        validate_id(variant.definition, limit)?;
                        if !tags.insert(variant.tag) {
                            return Err(TypedIrError::DuplicateVariantTag(variant.tag));
                        }
                        if let Some(payload) = &variant.payload {
                            validate_type(payload, limit)?;
                        }
                        validate_external_range(variant.source.as_ref(), &external_file_ids)?;
                    }
                }
            }
            validate_external_range(ty.source.as_ref(), &external_file_ids)?;
        }
        for function in &host.functions {
            validate_id(function.definition, limit)?;
            for parameter in &function.parameters {
                validate_type(parameter, limit)?;
            }
            validate_type(&function.result, limit)?;
            if let Some(async_result) = &function.async_result {
                validate_type(&async_result.success, limit)?;
                validate_type(&async_result.error, limit)?;
            }
            validate_host_async_result(function, host, definitions)?;
            validate_external_range(function.source.as_ref(), &external_file_ids)?;
        }
    }
    for export in metadata.exports.iter() {
        validate_id(export.function, limit)?;
        for parameter in &export.parameters {
            validate_type(parameter, limit)?;
        }
        validate_type(&export.result, limit)?;
    }
    for test in metadata.tests.iter() {
        validate_id(test.function, limit)?;
    }
    for lifecycle in [
        metadata.lifecycle.migration,
        metadata.lifecycle.activation,
        metadata.lifecycle.cleanup,
    ]
    .into_iter()
    .flatten()
    {
        validate_id(lifecycle, limit)?;
    }
    let mut standard_functions = BTreeMap::new();
    for function in metadata.standard_functions.iter() {
        validate_id(function.definition, limit)?;
        if definitions[function.definition.0 as usize].kind != DefinitionKind::StandardLibrary {
            return Err(TypedIrError::InvalidStandardFunction(function.definition));
        }
        if standard_functions
            .insert(function.definition, function)
            .is_some()
        {
            return Err(TypedIrError::DuplicateStandardFunction(function.definition));
        }
        for parameter in &function.parameters {
            validate_standard_signature_type(parameter, function.type_parameters.len(), limit)?;
        }
        validate_standard_signature_type(&function.result, function.type_parameters.len(), limit)?;
    }
    Ok(standard_functions)
}

fn validate_host_async_result(
    function: &HostFunctionBindingIr,
    host: &HostBindingIr,
    definitions: &[Definition],
) -> Result<(), TypedIrError> {
    let valid = match (function.mode, function.async_result.as_ref()) {
        (IrHostFunctionMode::Sync, None) => true,
        (IrHostFunctionMode::Request, Some(async_result)) => {
            let IrType::Result(success, error) = &function.result else {
                return Err(TypedIrError::InvalidHostAsyncResult(function.definition));
            };
            if success.as_ref() != &async_result.success || error.as_ref() != &async_result.error {
                return Err(TypedIrError::InvalidHostAsyncResult(function.definition));
            }
            let success = canonical_host_value_type(&async_result.success, host, definitions)?;
            let error = canonical_host_value_type(&async_result.error, host, definitions)?;
            async_result.result_type == nexa_core::canonical_result_type_id(success, error)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(TypedIrError::InvalidHostAsyncResult(function.definition))
    }
}

fn canonical_host_value_type(
    ty: &IrType,
    host: &HostBindingIr,
    definitions: &[Definition],
) -> Result<nexa_core::CanonicalValueType, TypedIrError> {
    use nexa_core::CanonicalValueType;
    let named = |stable_id| Ok(CanonicalValueType::Named(stable_id));
    match ty {
        IrType::I32 => Ok(CanonicalValueType::I32),
        IrType::I64 => Ok(CanonicalValueType::I64),
        IrType::F32 => Ok(CanonicalValueType::F32),
        IrType::F64 => Ok(CanonicalValueType::F64),
        IrType::Bool => Ok(CanonicalValueType::Bool),
        IrType::Rune => Ok(CanonicalValueType::Rune),
        IrType::String => Ok(CanonicalValueType::String),
        IrType::Named(definition) => {
            if let Some(ty) = host.types.iter().find(|ty| ty.definition == *definition) {
                return named(ty.stable_id);
            }
            let stable = definitions
                .get(definition.0 as usize)
                .and_then(|definition| definition.stable_symbol.as_ref())
                .ok_or(TypedIrError::MissingStableSymbol(*definition))?;
            named(stable.runtime_id.0)
        }
        IrType::Option(inner) => named(nexa_core::canonical_option_type_id(
            canonical_host_value_type(inner, host, definitions)?,
        )),
        IrType::Result(success, error) => named(nexa_core::canonical_result_type_id(
            canonical_host_value_type(success, host, definitions)?,
            canonical_host_value_type(error, host, definitions)?,
        )),
        IrType::Array(inner) => named(nexa_core::canonical_array_type_id(
            canonical_host_value_type(inner, host, definitions)?,
        )),
        IrType::Map(key, value) => named(nexa_core::canonical_map_type_id(
            canonical_host_value_type(key, host, definitions)?,
            canonical_host_value_type(value, host, definitions)?,
        )),
        IrType::Tuple(items) => {
            let items = items
                .iter()
                .map(|item| canonical_host_value_type(item, host, definitions))
                .collect::<Result<Vec<_>, _>>()?;
            named(nexa_core::canonical_tuple_type_id(&items))
        }
        IrType::HostRequest(_) => named(StableId::from_name("HostRequest")),
        IrType::ResourceToken(_) => named(StableId::from_name("ResourceToken")),
        IrType::Snapshot(content) => {
            let CanonicalValueType::Named(content) =
                canonical_host_value_type(content, host, definitions)?
            else {
                return Err(TypedIrError::InvalidSnapshotContentType);
            };
            named(nexa_core::canonical_snapshot_type_id(content))
        }
        IrType::Buffer(inner) => named(nexa_core::canonical_buffer_type_id(
            canonical_host_value_type(inner, host, definitions)?,
        )),
        IrType::StateHandle(inner) => named(nexa_core::canonical_state_handle_type_id(
            canonical_host_value_type(inner, host, definitions)?,
        )),
        IrType::Unit | IrType::TypeParameter(_) => Err(TypedIrError::NonRuntimeStateType),
    }
}

fn validate_standard_signature_type(
    ty: &IrType,
    type_parameter_count: usize,
    limit: usize,
) -> Result<(), TypedIrError> {
    validate_type(ty, limit)?;
    match ty {
        IrType::TypeParameter(index) => {
            if usize::from(*index) < type_parameter_count {
                Ok(())
            } else {
                Err(TypedIrError::InvalidTypeParameter(*index))
            }
        }
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => {
            validate_standard_signature_type(inner, type_parameter_count, limit)
        }
        IrType::HostRequest(inner) | IrType::ResourceToken(inner) => {
            inner.as_deref().map_or(Ok(()), |inner| {
                validate_standard_signature_type(inner, type_parameter_count, limit)
            })
        }
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            validate_standard_signature_type(ok, type_parameter_count, limit)?;
            validate_standard_signature_type(error, type_parameter_count, limit)
        }
        IrType::Tuple(items) => {
            for item in items {
                validate_standard_signature_type(item, type_parameter_count, limit)?;
            }
            Ok(())
        }
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_) => Ok(()),
    }
}

fn validate_standard_calls_in_declaration(
    declaration: &TypedDeclarationIr,
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    match &declaration.body {
        TypedDeclarationBody::Function(function) => {
            validate_standard_calls_in_block(&function.body, bindings, migration_types)
        }
        TypedDeclarationBody::Const(expression) => {
            validate_standard_calls_in_expression(expression, bindings, migration_types)
        }
        TypedDeclarationBody::TypeLayout(_) | TypedDeclarationBody::External => Ok(()),
    }
}

fn validate_standard_calls_in_block(
    block: &TypedBlockIr,
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } => {
                if let Some(value) = value {
                    validate_standard_calls_in_expression(value, bindings, migration_types)?;
                }
            }
            TypedStatementIr::Assign { target, value } => {
                validate_standard_calls_in_place(target, bindings, migration_types)?;
                validate_standard_calls_in_expression(value, bindings, migration_types)?;
            }
            TypedStatementIr::Expression(expression) => {
                validate_standard_calls_in_expression(expression, bindings, migration_types)?;
            }
            TypedStatementIr::Return(expression) => {
                if let Some(expression) = expression {
                    validate_standard_calls_in_expression(expression, bindings, migration_types)?;
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                validate_standard_calls_in_expression(condition, bindings, migration_types)?;
                validate_standard_calls_in_block(then_block, bindings, migration_types)?;
                if let Some(else_block) = else_block {
                    validate_standard_calls_in_block(else_block, bindings, migration_types)?;
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                validate_standard_calls_in_expression(condition, bindings, migration_types)?;
                validate_standard_calls_in_block(body, bindings, migration_types)?;
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            } => {
                validate_standard_calls_in_expression(start, bindings, migration_types)?;
                validate_standard_calls_in_expression(end, bindings, migration_types)?;
                validate_standard_calls_in_block(body, bindings, migration_types)?;
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    validate_standard_calls_in_expression(capture, bindings, migration_types)?;
                }
            }
            TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield => {}
        }
    }
    if let Some(tail) = &block.tail {
        validate_standard_calls_in_expression(tail, bindings, migration_types)?;
    }
    Ok(())
}

fn validate_standard_calls_in_place(
    place: &TypedPlaceIr,
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    match place {
        TypedPlaceIr::Definition(_) => Ok(()),
        TypedPlaceIr::Field { base, .. } | TypedPlaceIr::StateField { base, .. } => {
            validate_standard_calls_in_expression(base, bindings, migration_types)
        }
        TypedPlaceIr::Index { base, index } => {
            validate_standard_calls_in_expression(base, bindings, migration_types)?;
            validate_standard_calls_in_expression(index, bindings, migration_types)
        }
    }
}

fn validate_standard_calls_in_expression(
    expression: &TypedExpressionIr,
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::Yield => Ok(()),
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Try(operand)
        | TypedExpressionKind::Await(operand)
        | TypedExpressionKind::Field { base: operand, .. }
        | TypedExpressionKind::StateField { base: operand, .. } => {
            validate_standard_calls_in_expression(operand, bindings, migration_types)
        }
        TypedExpressionKind::Binary { left, right, .. }
        | TypedExpressionKind::Index {
            base: left,
            index: right,
        } => {
            validate_standard_calls_in_expression(left, bindings, migration_types)?;
            validate_standard_calls_in_expression(right, bindings, migration_types)
        }
        TypedExpressionKind::Call { callee, arguments } => {
            if bindings.contains_key(callee) {
                return Err(TypedIrError::StandardCallRequired(*callee));
            }
            validate_standard_call_arguments(arguments, bindings, migration_types)
        }
        TypedExpressionKind::StandardCall {
            function,
            intrinsic,
            type_arguments,
            arguments,
        } => {
            let binding = bindings
                .get(function)
                .ok_or(TypedIrError::InvalidStandardFunction(*function))?;
            if binding.intrinsic != *intrinsic {
                return Err(TypedIrError::StandardIntrinsicMismatch(*function));
            }
            if binding.type_parameters.len() != type_arguments.len() {
                return Err(TypedIrError::StandardTypeArgumentArity {
                    function: *function,
                    expected: binding.type_parameters.len(),
                    actual: type_arguments.len(),
                });
            }
            let parameters = binding
                .parameters
                .iter()
                .map(|ty| substitute_standard_type(ty, type_arguments))
                .collect::<Result<Vec<_>, _>>()?;
            if parameters.len() != arguments.len()
                || parameters
                    .iter()
                    .zip(arguments)
                    .any(|(expected, actual)| expected != &actual.ty)
            {
                return Err(TypedIrError::StandardCallSignatureMismatch(*function));
            }
            let result = substitute_standard_type(&binding.result, type_arguments)?;
            if result != expression.ty {
                return Err(TypedIrError::StandardCallSignatureMismatch(*function));
            }
            validate_standard_call_arguments(arguments, bindings, migration_types)
        }
        TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. }
        | TypedExpressionKind::Array(arguments)
        | TypedExpressionKind::Tuple(arguments)
        | TypedExpressionKind::StringInterpolation(arguments) => {
            validate_standard_call_arguments(arguments, bindings, migration_types)
        }
        TypedExpressionKind::Construct { fields, .. }
        | TypedExpressionKind::Update { fields, .. } => {
            if let TypedExpressionKind::Update { base, .. } = &expression.kind {
                validate_standard_calls_in_expression(base, bindings, migration_types)?;
            }
            for (_, value) in fields {
                validate_standard_calls_in_expression(value, bindings, migration_types)?;
            }
            Ok(())
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                validate_standard_calls_in_expression(payload, bindings, migration_types)?;
            }
            Ok(())
        }
        TypedExpressionKind::Match { value, arms } => {
            validate_standard_calls_in_expression(value, bindings, migration_types)?;
            for arm in arms {
                validate_standard_calls_in_expression(&arm.value, bindings, migration_types)?;
            }
            Ok(())
        }
        TypedExpressionKind::Migration(intrinsic) => {
            validate_migration_type_contract(intrinsic, migration_types)?;
            validate_standard_calls_in_migration(intrinsic, bindings, migration_types)
        }
    }
}

fn validate_standard_call_arguments(
    arguments: &[TypedExpressionIr],
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    for argument in arguments {
        validate_standard_calls_in_expression(argument, bindings, migration_types)?;
    }
    Ok(())
}

fn validate_standard_calls_in_migration(
    intrinsic: &MigrationIntrinsicIr,
    bindings: &BTreeMap<DefinitionId, &StandardFunctionBindingIr>,
    migration_types: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    match intrinsic {
        MigrationIntrinsicIr::OldFieldGet { object, .. } => {
            validate_standard_calls_in_expression(object, bindings, migration_types)
        }
        MigrationIntrinsicIr::NewSet { object, value, .. } => {
            validate_standard_calls_in_expression(object, bindings, migration_types)?;
            validate_standard_calls_in_expression(value, bindings, migration_types)
        }
        MigrationIntrinsicIr::Replace { target, .. } => {
            validate_standard_calls_in_expression(target, bindings, migration_types)
        }
        MigrationIntrinsicIr::OldGet { .. }
        | MigrationIntrinsicIr::NewCreate { .. }
        | MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => Ok(()),
    }
}

#[derive(Default)]
struct MigrationTypeContext {
    state_types: BTreeSet<DefinitionId>,
    state_fields: BTreeMap<DefinitionId, (DefinitionId, IrType)>,
}

fn migration_type_context(
    metadata: &PackageSemanticMetadata,
    definitions: &[Definition],
    modules: &[TypedModuleIr],
) -> Result<MigrationTypeContext, TypedIrError> {
    let mut layout_fields = BTreeMap::new();
    let mut layout_state_types = BTreeSet::new();
    for declaration in modules.iter().flat_map(|module| module.declarations.iter()) {
        let TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Stateful { fields }) =
            &declaration.body
        else {
            continue;
        };
        if definitions[declaration.definition.0 as usize].kind != DefinitionKind::Stateful {
            return Err(TypedIrError::InvalidMigrationStateType(
                declaration.definition,
            ));
        }
        if !layout_state_types.insert(declaration.definition) {
            return Err(TypedIrError::InvalidMigrationStateType(
                declaration.definition,
            ));
        }
        for field in fields {
            if layout_fields
                .insert(field.definition, (declaration.definition, field.ty.clone()))
                .is_some()
            {
                return Err(TypedIrError::InvalidMigrationFieldOwner(field.definition));
            }
        }
    }

    let mut context = MigrationTypeContext::default();
    for state in metadata.state_types.iter() {
        if definitions[state.definition.0 as usize].kind != DefinitionKind::Stateful
            || !layout_state_types.contains(&state.definition)
            || !context.state_types.insert(state.definition)
        {
            return Err(TypedIrError::InvalidMigrationStateType(state.definition));
        }
        for field in &state.fields {
            let definition = &definitions[field.definition.0 as usize];
            if definition.kind != DefinitionKind::Field {
                return Err(TypedIrError::InvalidMigrationStateField(field.definition));
            }
            if definition.ty != field.ty {
                return Err(TypedIrError::InvalidMigrationFieldType(field.definition));
            }
            let Some((layout_owner, layout_type)) = layout_fields.get(&field.definition) else {
                return Err(TypedIrError::InvalidMigrationStateField(field.definition));
            };
            if *layout_owner != state.definition {
                return Err(TypedIrError::InvalidMigrationFieldOwner(field.definition));
            }
            if layout_type != &field.ty {
                return Err(TypedIrError::InvalidMigrationFieldType(field.definition));
            }
            if context
                .state_fields
                .insert(field.definition, (state.definition, field.ty.clone()))
                .is_some()
            {
                return Err(TypedIrError::InvalidMigrationFieldOwner(field.definition));
            }
        }
    }
    Ok(context)
}

fn validate_migration_type_contract(
    intrinsic: &MigrationIntrinsicIr,
    context: &MigrationTypeContext,
) -> Result<(), TypedIrError> {
    let validate_field = |object: &TypedExpressionIr,
                          field: DefinitionId,
                          value_type: &IrType|
     -> Result<(), TypedIrError> {
        let Some((owner, expected_type)) = context.state_fields.get(&field) else {
            return Err(TypedIrError::InvalidMigrationStateField(field));
        };
        if object.ty != IrType::Named(*owner) {
            return Err(TypedIrError::InvalidMigrationFieldOwner(field));
        }
        if value_type != expected_type {
            return Err(TypedIrError::InvalidMigrationFieldType(field));
        }
        Ok(())
    };

    match intrinsic {
        MigrationIntrinsicIr::OldFieldGet {
            object,
            field,
            value_type,
        } => validate_field(object, *field, value_type),
        MigrationIntrinsicIr::NewCreate { state_type, .. } => {
            if context.state_types.contains(state_type) {
                Ok(())
            } else {
                Err(TypedIrError::InvalidMigrationStateType(*state_type))
            }
        }
        MigrationIntrinsicIr::NewSet {
            object,
            field,
            value,
        } => validate_field(object, *field, &value.ty),
        MigrationIntrinsicIr::Replace { target, .. } => {
            if matches!(target.ty, IrType::Named(definition) if context.state_types.contains(&definition))
            {
                Ok(())
            } else {
                Err(TypedIrError::InvalidMigrationTargetType)
            }
        }
        MigrationIntrinsicIr::OldGet { .. }
        | MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => Ok(()),
    }
}

fn substitute_standard_type(ty: &IrType, arguments: &[IrType]) -> Result<IrType, TypedIrError> {
    Ok(match ty {
        IrType::TypeParameter(index) => arguments
            .get(usize::from(*index))
            .cloned()
            .ok_or(TypedIrError::InvalidTypeParameter(*index))?,
        IrType::Option(inner) => {
            IrType::Option(Box::new(substitute_standard_type(inner, arguments)?))
        }
        IrType::Result(ok, error) => IrType::Result(
            Box::new(substitute_standard_type(ok, arguments)?),
            Box::new(substitute_standard_type(error, arguments)?),
        ),
        IrType::Array(inner) => {
            IrType::Array(Box::new(substitute_standard_type(inner, arguments)?))
        }
        IrType::Map(key, value) => IrType::Map(
            Box::new(substitute_standard_type(key, arguments)?),
            Box::new(substitute_standard_type(value, arguments)?),
        ),
        IrType::Tuple(items) => IrType::Tuple(
            items
                .iter()
                .map(|item| substitute_standard_type(item, arguments))
                .collect::<Result<_, _>>()?,
        ),
        IrType::HostRequest(inner) => IrType::HostRequest(
            inner
                .as_deref()
                .map(|inner| substitute_standard_type(inner, arguments).map(Box::new))
                .transpose()?,
        ),
        IrType::ResourceToken(inner) => IrType::ResourceToken(
            inner
                .as_deref()
                .map(|inner| substitute_standard_type(inner, arguments).map(Box::new))
                .transpose()?,
        ),
        IrType::Snapshot(inner) => {
            IrType::Snapshot(Box::new(substitute_standard_type(inner, arguments)?))
        }
        IrType::Buffer(inner) => {
            IrType::Buffer(Box::new(substitute_standard_type(inner, arguments)?))
        }
        IrType::StateHandle(inner) => {
            IrType::StateHandle(Box::new(substitute_standard_type(inner, arguments)?))
        }
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_) => ty.clone(),
    })
}

fn validate_external_range(
    range: Option<&ExternalSourceRangeIr>,
    external_file_ids: &std::collections::BTreeSet<ArtifactFileId>,
) -> Result<(), TypedIrError> {
    if let Some(range) = range
        && !external_file_ids.contains(&range.file_id)
    {
        return Err(TypedIrError::UnknownExternalFileId(range.file_id));
    }
    Ok(())
}

fn validate_declaration(
    declaration: &TypedDeclarationIr,
    limit: usize,
    constants: &BTreeMap<DefinitionId, &TypedExpressionIr>,
) -> Result<(), TypedIrError> {
    match &declaration.body {
        TypedDeclarationBody::Function(function) => {
            for id in function.parameters.iter().chain(&function.locals) {
                validate_id(*id, limit)?;
            }
            validate_type(&function.return_type, limit)?;
            validate_block(&function.body, limit, constants)
        }
        TypedDeclarationBody::Const(expression) => validate_expression(expression, limit),
        TypedDeclarationBody::TypeLayout(layout) => {
            match layout {
                TypedTypeLayoutIr::Struct { fields }
                | TypedTypeLayoutIr::Class { fields }
                | TypedTypeLayoutIr::Stateful { fields } => {
                    let mut orders = std::collections::BTreeSet::new();
                    for field in fields {
                        validate_id(field.definition, limit)?;
                        validate_type(&field.ty, limit)?;
                        if !orders.insert(field.order) {
                            return Err(TypedIrError::DuplicateFieldOrder(field.order));
                        }
                    }
                }
                TypedTypeLayoutIr::Enum { variants } => {
                    let mut tags = std::collections::BTreeSet::new();
                    for variant in variants {
                        validate_id(variant.definition, limit)?;
                        if !tags.insert(variant.tag) {
                            return Err(TypedIrError::DuplicateVariantTag(variant.tag));
                        }
                        if let Some(payload) = &variant.payload {
                            validate_type(payload, limit)?;
                        }
                    }
                }
            }
            Ok(())
        }
        TypedDeclarationBody::External => Ok(()),
    }
}

fn validate_block(
    block: &TypedBlockIr,
    limit: usize,
    constants: &BTreeMap<DefinitionId, &TypedExpressionIr>,
) -> Result<(), TypedIrError> {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { definition, value } => {
                validate_id(*definition, limit)?;
                if let Some(value) = value {
                    validate_expression(value, limit)?;
                }
            }
            TypedStatementIr::Assign { target, value } => {
                validate_place(target, limit)?;
                validate_expression(value, limit)?;
            }
            TypedStatementIr::Expression(expression) => validate_expression(expression, limit)?,
            TypedStatementIr::Return(expression) => {
                if let Some(expression) = expression {
                    validate_expression(expression, limit)?;
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                validate_expression(condition, limit)?;
                validate_block(then_block, limit, constants)?;
                if let Some(else_block) = else_block {
                    validate_block(else_block, limit, constants)?;
                }
            }
            TypedStatementIr::While {
                condition,
                body,
                max_iterations,
            } => {
                if *max_iterations == 0 {
                    return Err(TypedIrError::ZeroLoopBound);
                }
                validate_expression(condition, limit)?;
                validate_block(body, limit, constants)?;
            }
            TypedStatementIr::StaticRangeFor {
                binding,
                start,
                end,
                body,
                max_iterations,
            } => {
                validate_id(*binding, limit)?;
                validate_expression(start, limit)?;
                validate_expression(end, limit)?;
                let exact = exact_static_range_iterations(start, end, constants)
                    .ok_or(TypedIrError::NonConstantStaticRange)?;
                if *max_iterations != exact {
                    return Err(TypedIrError::InvalidStaticRangeBound {
                        declared: *max_iterations,
                        exact,
                    });
                }
                validate_block(body, limit, constants)?;
            }
            TypedStatementIr::Defer { cleanup, captures } => {
                validate_id(*cleanup, limit)?;
                validate_expressions(captures, limit)?;
            }
            TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield => {}
        }
    }
    if let Some(tail) = &block.tail {
        validate_expression(tail, limit)?;
    }
    Ok(())
}

fn exact_static_range_iterations(
    start: &TypedExpressionIr,
    end: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, &TypedExpressionIr>,
) -> Option<u32> {
    let start = constant_i32_expression(start, constants, &mut BTreeSet::new())?;
    let end = constant_i32_expression(end, constants, &mut BTreeSet::new())?;
    u32::try_from(i64::from(end).saturating_sub(i64::from(start)).max(0)).ok()
}

fn constant_i32_expression(
    expression: &TypedExpressionIr,
    constants: &BTreeMap<DefinitionId, &TypedExpressionIr>,
    visiting: &mut BTreeSet<DefinitionId>,
) -> Option<i32> {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::I32(value)) => Some(*value),
        TypedExpressionKind::Reference(definition) => {
            let expression = constants.get(definition)?;
            if !visiting.insert(*definition) {
                return None;
            }
            let value = constant_i32_expression(expression, constants, visiting);
            visiting.remove(definition);
            value
        }
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Negate,
            operand,
        } => constant_i32_expression(operand, constants, visiting)?.checked_neg(),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            let left = constant_i32_expression(left, constants, visiting)?;
            let right = constant_i32_expression(right, constants, visiting)?;
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

fn validate_place(place: &TypedPlaceIr, limit: usize) -> Result<(), TypedIrError> {
    match place {
        TypedPlaceIr::Definition(id) => validate_id(*id, limit),
        TypedPlaceIr::Field { base, field } | TypedPlaceIr::StateField { base, field } => {
            validate_expression(base, limit)?;
            validate_id(*field, limit)
        }
        TypedPlaceIr::Index { base, index } => {
            validate_expression(base, limit)?;
            validate_expression(index, limit)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn validate_expression(expression: &TypedExpressionIr, limit: usize) -> Result<(), TypedIrError> {
    validate_concrete_type(&expression.ty, limit)?;
    validate_unit_expression_shape(expression)?;
    match &expression.kind {
        TypedExpressionKind::Literal(_) => Ok(()),
        TypedExpressionKind::Yield => {
            if expression.effect == IrEffect::Task {
                Ok(())
            } else {
                Err(TypedIrError::InvalidUnitExpression)
            }
        }
        TypedExpressionKind::Reference(id) => validate_id(*id, limit),
        TypedExpressionKind::Unary { operand, .. } => validate_expression(operand, limit),
        TypedExpressionKind::Binary { left, right, .. } => {
            validate_expression(left, limit)?;
            validate_expression(right, limit)
        }
        TypedExpressionKind::Call { callee, arguments } => {
            validate_id(*callee, limit)?;
            validate_expressions(arguments, limit)
        }
        TypedExpressionKind::StandardCall {
            function,
            type_arguments,
            arguments,
            ..
        } => {
            validate_id(*function, limit)?;
            for argument in type_arguments {
                validate_concrete_type(argument, limit)?;
            }
            validate_expressions(arguments, limit)
        }
        TypedExpressionKind::BuiltinCall {
            type_arguments,
            arguments,
            ..
        } => {
            for argument in type_arguments {
                validate_concrete_type(argument, limit)?;
            }
            validate_expressions(arguments, limit)
        }
        TypedExpressionKind::HostCall {
            interface,
            function,
            arguments,
        } => {
            validate_id(*interface, limit)?;
            validate_id(*function, limit)?;
            validate_expressions(arguments, limit)
        }
        TypedExpressionKind::Construct { definition, fields } => {
            validate_id(*definition, limit)?;
            for (field, value) in fields {
                validate_id(*field, limit)?;
                validate_expression(value, limit)?;
            }
            Ok(())
        }
        TypedExpressionKind::EnumConstruct {
            enum_definition,
            variant_definition,
            payload,
        } => {
            validate_id(*enum_definition, limit)?;
            validate_id(*variant_definition, limit)?;
            if let Some(payload) = payload {
                validate_expression(payload, limit)?;
            }
            Ok(())
        }
        TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                validate_expression(payload, limit)?;
            }
            Ok(())
        }
        TypedExpressionKind::Field { base, field }
        | TypedExpressionKind::StateField { base, field } => {
            validate_expression(base, limit)?;
            validate_id(*field, limit)
        }
        TypedExpressionKind::Index { base, index } => {
            validate_expression(base, limit)?;
            validate_expression(index, limit)
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => validate_expressions(values, limit),
        TypedExpressionKind::Match { value, arms } => {
            validate_expression(value, limit)?;
            for arm in arms {
                validate_pattern(&arm.pattern, limit)?;
                validate_expression(&arm.value, limit)?;
            }
            Ok(())
        }
        TypedExpressionKind::Try(value) => validate_expression(value, limit),
        TypedExpressionKind::Await(value) => {
            validate_expression(value, limit)?;
            if value.effect == IrEffect::Task
                && expression.effect == IrEffect::Task
                && value.ty == expression.ty
            {
                Ok(())
            } else {
                Err(TypedIrError::InvalidAwaitOperand)
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            validate_expression(base, limit)?;
            for (field, value) in fields {
                validate_id(*field, limit)?;
                validate_expression(value, limit)?;
            }
            Ok(())
        }
        TypedExpressionKind::Migration(intrinsic) => {
            validate_migration(&expression.ty, expression.effect, intrinsic, limit)
        }
    }
}

fn validate_unit_expression_shape(expression: &TypedExpressionIr) -> Result<(), TypedIrError> {
    if matches!(
        &expression.kind,
        TypedExpressionKind::Literal(IrLiteral::Unit)
    ) {
        return (expression.ty == IrType::Unit)
            .then_some(())
            .ok_or(TypedIrError::InvalidUnitExpression);
    }
    if expression.ty != IrType::Unit {
        return Ok(());
    }
    match &expression.kind {
        TypedExpressionKind::Reference(_)
        | TypedExpressionKind::Call { .. }
        | TypedExpressionKind::StandardCall { .. }
        | TypedExpressionKind::BuiltinCall { .. }
        | TypedExpressionKind::HostCall { .. }
        | TypedExpressionKind::Field { .. }
        | TypedExpressionKind::StateField { .. }
        | TypedExpressionKind::Index { .. }
        | TypedExpressionKind::Match { .. }
        | TypedExpressionKind::Try(_)
        | TypedExpressionKind::Migration(_)
        | TypedExpressionKind::Await(_)
        | TypedExpressionKind::Yield => Ok(()),
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Unary { .. }
        | TypedExpressionKind::Binary { .. }
        | TypedExpressionKind::StringInterpolation(_)
        | TypedExpressionKind::Construct { .. }
        | TypedExpressionKind::EnumConstruct { .. }
        | TypedExpressionKind::BuiltinVariant { .. }
        | TypedExpressionKind::Array(_)
        | TypedExpressionKind::Tuple(_)
        | TypedExpressionKind::Update { .. } => Err(TypedIrError::InvalidUnitExpression),
    }
}

fn validate_migration(
    expression_type: &IrType,
    effect: IrEffect,
    intrinsic: &MigrationIntrinsicIr,
    limit: usize,
) -> Result<(), TypedIrError> {
    if effect != IrEffect::Migration {
        return Err(TypedIrError::InvalidMigrationEffect);
    }
    let expected_result = match intrinsic {
        MigrationIntrinsicIr::OldGet { value_type, .. } => {
            validate_type(value_type, limit)?;
            value_type.clone()
        }
        MigrationIntrinsicIr::OldFieldGet {
            object,
            field,
            value_type,
        } => {
            validate_expression(object, limit)?;
            validate_id(*field, limit)?;
            validate_type(value_type, limit)?;
            value_type.clone()
        }
        MigrationIntrinsicIr::NewCreate { state_type, .. } => {
            validate_id(*state_type, limit)?;
            IrType::Named(*state_type)
        }
        MigrationIntrinsicIr::NewSet {
            object,
            field,
            value,
        } => {
            validate_expression(object, limit)?;
            validate_id(*field, limit)?;
            validate_expression(value, limit)?;
            IrType::Bool
        }
        MigrationIntrinsicIr::Replace { target, .. } => {
            validate_expression(target, limit)?;
            IrType::Bool
        }
        MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => IrType::Bool,
    };
    if expression_type != &expected_result {
        return Err(TypedIrError::InvalidMigrationResultType);
    }
    Ok(())
}

fn validate_pattern(pattern: &TypedPatternIr, limit: usize) -> Result<(), TypedIrError> {
    validate_concrete_type(&pattern.ty, limit)?;
    match &pattern.kind {
        TypedPatternKind::Wildcard | TypedPatternKind::Literal(_) => Ok(()),
        TypedPatternKind::Binding(definition) => validate_id(*definition, limit),
        TypedPatternKind::Variant {
            definition,
            payload,
        } => {
            validate_id(*definition, limit)?;
            for pattern in payload {
                validate_pattern(pattern, limit)?;
            }
            Ok(())
        }
        TypedPatternKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                validate_pattern(payload, limit)?;
            }
            Ok(())
        }
        TypedPatternKind::Struct { definition, fields } => {
            validate_id(*definition, limit)?;
            for (field, pattern) in fields {
                validate_id(*field, limit)?;
                validate_pattern(pattern, limit)?;
            }
            Ok(())
        }
    }
}

fn validate_expressions(values: &[TypedExpressionIr], limit: usize) -> Result<(), TypedIrError> {
    for value in values {
        validate_expression(value, limit)?;
    }
    Ok(())
}

fn validate_type(ty: &IrType, limit: usize) -> Result<(), TypedIrError> {
    match ty {
        IrType::Named(id) => validate_id(*id, limit),
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => validate_type(inner, limit),
        IrType::HostRequest(inner) | IrType::ResourceToken(inner) => inner
            .as_deref()
            .map_or(Ok(()), |inner| validate_type(inner, limit)),
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            validate_type(ok, limit)?;
            validate_type(error, limit)
        }
        IrType::Tuple(items) => {
            for item in items {
                validate_type(item, limit)?;
            }
            Ok(())
        }
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::TypeParameter(_) => Ok(()),
    }
}

fn validate_concrete_type(ty: &IrType, limit: usize) -> Result<(), TypedIrError> {
    validate_type(ty, limit)?;
    if contains_type_parameter(ty) {
        Err(TypedIrError::UnresolvedTypeParameter)
    } else {
        Ok(())
    }
}

fn contains_type_parameter(ty: &IrType) -> bool {
    match ty {
        IrType::TypeParameter(_) => true,
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => contains_type_parameter(inner),
        IrType::HostRequest(inner) | IrType::ResourceToken(inner) => {
            inner.as_deref().is_some_and(contains_type_parameter)
        }
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            contains_type_parameter(ok) || contains_type_parameter(error)
        }
        IrType::Tuple(items) => items.iter().any(contains_type_parameter),
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_) => false,
    }
}

fn validate_id(id: DefinitionId, limit: usize) -> Result<(), TypedIrError> {
    if id.0 as usize >= limit {
        Err(TypedIrError::UnknownDefinition(id))
    } else {
        Ok(())
    }
}

pub(crate) fn remap_definition(
    mut definition: Definition,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<Definition, TypedIrError> {
    definition.id = remapped_id(definition.id, mapping)?;
    remap_type(&mut definition.ty, mapping)?;
    Ok(definition)
}

pub(crate) fn remap_typed_module(
    module: &TypedModuleIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<TypedModuleIr, TypedIrError> {
    let mut references = module.resolved_references.to_vec();
    for reference in &mut references {
        reference.target = remapped_id(reference.target, mapping)?;
    }
    let mut declarations = module.declarations.to_vec();
    for declaration in &mut declarations {
        remap_declaration(declaration, mapping)?;
    }
    Ok(TypedModuleIr {
        package_id: module.package_id.clone(),
        module: module.module.clone(),
        source: module.source.clone(),
        file_id: module.file_id,
        syntax: Arc::clone(&module.syntax),
        resolved_references: references.into(),
        declarations: declarations.into(),
    })
}

fn remap_declaration(
    declaration: &mut TypedDeclarationIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    declaration.definition = remapped_id(declaration.definition, mapping)?;
    match &mut declaration.body {
        TypedDeclarationBody::Function(function) => {
            remap_ids(&mut function.parameters, mapping)?;
            remap_ids(&mut function.locals, mapping)?;
            remap_type(&mut function.return_type, mapping)?;
            remap_block(&mut function.body, mapping)
        }
        TypedDeclarationBody::Const(expression) => remap_expression(expression, mapping),
        TypedDeclarationBody::TypeLayout(layout) => remap_type_layout(layout, mapping),
        TypedDeclarationBody::External => Ok(()),
    }
}

fn remap_type_layout(
    layout: &mut TypedTypeLayoutIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    match layout {
        TypedTypeLayoutIr::Struct { fields }
        | TypedTypeLayoutIr::Class { fields }
        | TypedTypeLayoutIr::Stateful { fields } => {
            for field in fields {
                field.definition = remapped_id(field.definition, mapping)?;
                remap_type(&mut field.ty, mapping)?;
            }
        }
        TypedTypeLayoutIr::Enum { variants } => {
            for variant in variants {
                variant.definition = remapped_id(variant.definition, mapping)?;
                if let Some(payload) = &mut variant.payload {
                    remap_type(payload, mapping)?;
                }
            }
        }
    }
    Ok(())
}

fn remap_block(
    block: &mut TypedBlockIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    for statement in &mut block.statements {
        match statement {
            TypedStatementIr::Let { definition, value } => {
                *definition = remapped_id(*definition, mapping)?;
                if let Some(value) = value {
                    remap_expression(value, mapping)?;
                }
            }
            TypedStatementIr::Assign { target, value } => {
                remap_place(target, mapping)?;
                remap_expression(value, mapping)?;
            }
            TypedStatementIr::Expression(expression) => remap_expression(expression, mapping)?,
            TypedStatementIr::Return(expression) => {
                if let Some(expression) = expression {
                    remap_expression(expression, mapping)?;
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                remap_expression(condition, mapping)?;
                remap_block(then_block, mapping)?;
                if let Some(else_block) = else_block {
                    remap_block(else_block, mapping)?;
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                remap_expression(condition, mapping)?;
                remap_block(body, mapping)?;
            }
            TypedStatementIr::StaticRangeFor {
                binding,
                start,
                end,
                body,
                ..
            } => {
                *binding = remapped_id(*binding, mapping)?;
                remap_expression(start, mapping)?;
                remap_expression(end, mapping)?;
                remap_block(body, mapping)?;
            }
            TypedStatementIr::Defer { cleanup, captures } => {
                *cleanup = remapped_id(*cleanup, mapping)?;
                for capture in captures {
                    remap_expression(capture, mapping)?;
                }
            }
            TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield => {}
        }
    }
    if let Some(tail) = &mut block.tail {
        remap_expression(tail, mapping)?;
    }
    Ok(())
}

fn remap_place(
    place: &mut TypedPlaceIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    match place {
        TypedPlaceIr::Definition(definition) => {
            *definition = remapped_id(*definition, mapping)?;
        }
        TypedPlaceIr::Field { base, field } | TypedPlaceIr::StateField { base, field } => {
            remap_expression(base, mapping)?;
            *field = remapped_id(*field, mapping)?;
        }
        TypedPlaceIr::Index { base, index } => {
            remap_expression(base, mapping)?;
            remap_expression(index, mapping)?;
        }
    }
    Ok(())
}

fn remap_expression(
    expression: &mut TypedExpressionIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    remap_type(&mut expression.ty, mapping)?;
    match &mut expression.kind {
        TypedExpressionKind::Literal(_) | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Reference(definition) => {
            *definition = remapped_id(*definition, mapping)?;
        }
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Try(operand)
        | TypedExpressionKind::Await(operand) => remap_expression(operand, mapping)?,
        TypedExpressionKind::Binary { left, right, .. } => {
            remap_expression(left, mapping)?;
            remap_expression(right, mapping)?;
        }
        TypedExpressionKind::Call { callee, arguments } => {
            *callee = remapped_id(*callee, mapping)?;
            remap_expressions(arguments, mapping)?;
        }
        TypedExpressionKind::StandardCall {
            function,
            type_arguments,
            arguments,
            ..
        } => {
            *function = remapped_id(*function, mapping)?;
            for argument in type_arguments {
                remap_type(argument, mapping)?;
            }
            remap_expressions(arguments, mapping)?;
        }
        TypedExpressionKind::BuiltinCall {
            type_arguments,
            arguments,
            ..
        } => {
            for argument in type_arguments {
                remap_type(argument, mapping)?;
            }
            remap_expressions(arguments, mapping)?;
        }
        TypedExpressionKind::HostCall {
            interface,
            function,
            arguments,
        } => {
            *interface = remapped_id(*interface, mapping)?;
            *function = remapped_id(*function, mapping)?;
            remap_expressions(arguments, mapping)?;
        }
        TypedExpressionKind::Construct { definition, fields } => {
            *definition = remapped_id(*definition, mapping)?;
            remap_fields(fields, mapping)?;
        }
        TypedExpressionKind::EnumConstruct {
            enum_definition,
            variant_definition,
            payload,
        } => {
            *enum_definition = remapped_id(*enum_definition, mapping)?;
            *variant_definition = remapped_id(*variant_definition, mapping)?;
            if let Some(payload) = payload {
                remap_expression(payload, mapping)?;
            }
        }
        TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                remap_expression(payload, mapping)?;
            }
        }
        TypedExpressionKind::Field { base, field }
        | TypedExpressionKind::StateField { base, field } => {
            remap_expression(base, mapping)?;
            *field = remapped_id(*field, mapping)?;
        }
        TypedExpressionKind::Index { base, index } => {
            remap_expression(base, mapping)?;
            remap_expression(index, mapping)?;
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            remap_expressions(values, mapping)?;
        }
        TypedExpressionKind::Match { value, arms } => {
            remap_expression(value, mapping)?;
            for arm in arms {
                remap_pattern(&mut arm.pattern, mapping)?;
                remap_expression(&mut arm.value, mapping)?;
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            remap_expression(base, mapping)?;
            remap_fields(fields, mapping)?;
        }
        TypedExpressionKind::Migration(intrinsic) => remap_migration(intrinsic, mapping)?,
    }
    Ok(())
}

fn remap_migration(
    intrinsic: &mut MigrationIntrinsicIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    match intrinsic {
        MigrationIntrinsicIr::OldGet { value_type, .. } => remap_type(value_type, mapping)?,
        MigrationIntrinsicIr::OldFieldGet {
            object,
            field,
            value_type,
        } => {
            remap_expression(object, mapping)?;
            *field = remapped_id(*field, mapping)?;
            remap_type(value_type, mapping)?;
        }
        MigrationIntrinsicIr::NewCreate { state_type, .. } => {
            *state_type = remapped_id(*state_type, mapping)?;
        }
        MigrationIntrinsicIr::NewSet {
            object,
            field,
            value,
        } => {
            remap_expression(object, mapping)?;
            *field = remapped_id(*field, mapping)?;
            remap_expression(value, mapping)?;
        }
        MigrationIntrinsicIr::Replace { target, .. } => {
            remap_expression(target, mapping)?;
        }
        MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => {}
    }
    Ok(())
}

fn remap_fields(
    fields: &mut [(DefinitionId, TypedExpressionIr)],
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    for (field, value) in fields {
        *field = remapped_id(*field, mapping)?;
        remap_expression(value, mapping)?;
    }
    Ok(())
}

fn remap_expressions(
    expressions: &mut [TypedExpressionIr],
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    for expression in expressions {
        remap_expression(expression, mapping)?;
    }
    Ok(())
}

fn remap_pattern(
    pattern: &mut TypedPatternIr,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    remap_type(&mut pattern.ty, mapping)?;
    match &mut pattern.kind {
        TypedPatternKind::Wildcard | TypedPatternKind::Literal(_) => {}
        TypedPatternKind::Binding(definition) => {
            *definition = remapped_id(*definition, mapping)?;
        }
        TypedPatternKind::Variant {
            definition,
            payload,
        } => {
            *definition = remapped_id(*definition, mapping)?;
            for pattern in payload {
                remap_pattern(pattern, mapping)?;
            }
        }
        TypedPatternKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                remap_pattern(payload, mapping)?;
            }
        }
        TypedPatternKind::Struct { definition, fields } => {
            *definition = remapped_id(*definition, mapping)?;
            for (field, pattern) in fields {
                *field = remapped_id(*field, mapping)?;
                remap_pattern(pattern, mapping)?;
            }
        }
    }
    Ok(())
}

fn remap_type(
    ty: &mut IrType,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    match ty {
        IrType::Named(definition) => {
            *definition = remapped_id(*definition, mapping)?;
        }
        IrType::Option(inner)
        | IrType::Array(inner)
        | IrType::HostRequest(Some(inner))
        | IrType::ResourceToken(Some(inner))
        | IrType::Snapshot(inner)
        | IrType::Buffer(inner)
        | IrType::StateHandle(inner) => remap_type(inner, mapping)?,
        IrType::Result(ok, error) | IrType::Map(ok, error) => {
            remap_type(ok, mapping)?;
            remap_type(error, mapping)?;
        }
        IrType::Tuple(items) => {
            for item in items {
                remap_type(item, mapping)?;
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
        | IrType::TypeParameter(_)
        | IrType::HostRequest(None)
        | IrType::ResourceToken(None) => {}
    }
    Ok(())
}

fn remap_ids(
    ids: &mut [DefinitionId],
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<(), TypedIrError> {
    for id in ids {
        *id = remapped_id(*id, mapping)?;
    }
    Ok(())
}

fn remapped_id(
    id: DefinitionId,
    mapping: &BTreeMap<DefinitionId, DefinitionId>,
) -> Result<DefinitionId, TypedIrError> {
    mapping
        .get(&id)
        .copied()
        .ok_or(TypedIrError::MissingDefinitionRemap(id))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedIrError {
    TooManyDefinitions,
    NonDenseDefinition {
        expected: DefinitionId,
        actual: DefinitionId,
    },
    UnknownDefinition(DefinitionId),
    MissingRootPackageModule(PackageId),
    DefinitionPackageWithoutModule(PackageId),
    ZeroLoopBound,
    NonConstantStaticRange,
    InvalidStaticRangeBound {
        declared: u32,
        exact: u32,
    },
    DuplicateFieldOrder(u32),
    DuplicateVariantTag(u32),
    DuplicateExternalFileId,
    UnknownExternalFileId(ArtifactFileId),
    MissingStableSymbol(DefinitionId),
    MissingDefinitionRemap(DefinitionId),
    InvalidSnapshotContentType,
    NonRuntimeStateType,
    InvalidHostAsyncResult(DefinitionId),
    InvalidAwaitOperand,
    InvalidUnitExpression,
    InvalidMigrationEffect,
    InvalidMigrationResultType,
    InvalidMigrationStateType(DefinitionId),
    InvalidMigrationStateField(DefinitionId),
    InvalidMigrationFieldOwner(DefinitionId),
    InvalidMigrationFieldType(DefinitionId),
    InvalidMigrationTargetType,
    UnresolvedTypeParameter,
    InvalidTypeParameter(u16),
    InvalidStandardFunction(DefinitionId),
    DuplicateStandardFunction(DefinitionId),
    StandardCallRequired(DefinitionId),
    StandardIntrinsicMismatch(DefinitionId),
    StandardTypeArgumentArity {
        function: DefinitionId,
        expected: usize,
        actual: usize,
    },
    StandardCallSignatureMismatch(DefinitionId),
}

impl fmt::Display for TypedIrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for TypedIrError {}

#[cfg(test)]
mod tests {
    use super::{
        DefinitionId, HostAsyncResultIr, HostBindingIr, HostFunctionBindingIr, IrAbandonPolicy,
        IrCancelPolicy, IrEffect, IrHostFunctionMode, IrLiteral, IrType, MigrationIntrinsicIr,
        MigrationTypeContext, SourceRange, TypedBlockIr, TypedExpressionIr, TypedExpressionKind,
        TypedIrError, TypedStatementIr, validate_block, validate_expression,
        validate_host_async_result, validate_migration_type_contract,
    };
    use crate::{NormalizedPackagePath, PackageId, SourceKey};
    use nexa_core::StableId;
    use std::collections::{BTreeMap, BTreeSet};

    fn expression(ty: IrType) -> TypedExpressionIr {
        TypedExpressionIr {
            ty,
            effect: IrEffect::Migration,
            span: SourceRange {
                source: SourceKey::new(
                    PackageId::new("test.package").unwrap(),
                    NormalizedPackagePath::new("main.nexa").unwrap(),
                ),
                start: 0,
                end: 0,
            },
            kind: TypedExpressionKind::Literal(IrLiteral::Unit),
        }
    }

    fn i32_expression(value: i32) -> TypedExpressionIr {
        let mut expression = expression(IrType::I32);
        expression.kind = TypedExpressionKind::Literal(IrLiteral::I32(value));
        expression
    }

    #[test]
    fn typed_ir_rejects_forged_async_result_type_identity() {
        let function = HostFunctionBindingIr {
            definition: DefinitionId(0),
            stable_id: StableId::from_name("Host::request"),
            import_index: 0,
            mode: IrHostFunctionMode::Request,
            parameters: Vec::new(),
            result: IrType::Result(Box::new(IrType::I32), Box::new(IrType::String)),
            fuel_cost: 1,
            async_result: Some(HostAsyncResultIr {
                result_type: StableId::from_name("forged-result"),
                success: IrType::I32,
                error: IrType::String,
                cancel_policy: IrCancelPolicy::CancelTask,
                abandon_policy: IrAbandonPolicy::Trap,
                cancel_error: None,
                abandon_error: None,
            }),
            source: None,
        };
        let host = HostBindingIr {
            interface: DefinitionId(0),
            interface_stable_id: StableId::from_name("Host"),
            namespaces: Vec::new(),
            types: Vec::new(),
            functions: vec![function.clone()],
        };
        assert_eq!(
            validate_host_async_result(&function, &host, &[]),
            Err(TypedIrError::InvalidHostAsyncResult(DefinitionId(0)))
        );
    }

    #[test]
    fn typed_ir_rejects_await_of_a_non_task_operand() {
        let mut operand = i32_expression(1);
        operand.effect = IrEffect::Immediate;
        let awaited = TypedExpressionIr {
            ty: IrType::I32,
            effect: IrEffect::Task,
            span: operand.span.clone(),
            kind: TypedExpressionKind::Await(Box::new(operand)),
        };
        assert_eq!(
            validate_expression(&awaited, 0),
            Err(TypedIrError::InvalidAwaitOperand)
        );
    }

    #[test]
    fn typed_ir_rejects_forged_unit_value_shapes_and_effects() {
        let mut wrong_literal_type = expression(IrType::I32);
        assert_eq!(
            validate_expression(&wrong_literal_type, 0),
            Err(TypedIrError::InvalidUnitExpression)
        );

        wrong_literal_type.ty = IrType::Unit;
        wrong_literal_type.kind = TypedExpressionKind::Unary {
            operator: super::UnaryOperator::Negate,
            operand: Box::new(i32_expression(1)),
        };
        assert_eq!(
            validate_expression(&wrong_literal_type, 0),
            Err(TypedIrError::InvalidUnitExpression)
        );

        let mut yield_value = expression(IrType::Unit);
        yield_value.kind = TypedExpressionKind::Yield;
        yield_value.effect = IrEffect::Immediate;
        assert_eq!(
            validate_expression(&yield_value, 0),
            Err(TypedIrError::InvalidUnitExpression)
        );
        yield_value.effect = IrEffect::Task;
        assert_eq!(validate_expression(&yield_value, 0), Ok(()));
    }

    #[test]
    fn typed_ir_rejects_inexact_or_nonconstant_static_range_bounds() {
        let inexact = TypedBlockIr {
            statements: vec![TypedStatementIr::StaticRangeFor {
                binding: DefinitionId(0),
                start: i32_expression(0),
                end: i32_expression(100),
                body: TypedBlockIr::default(),
                max_iterations: 1,
            }],
            tail: None,
        };
        assert_eq!(
            validate_block(&inexact, 1, &BTreeMap::new()),
            Err(TypedIrError::InvalidStaticRangeBound {
                declared: 1,
                exact: 100,
            })
        );

        let nonconstant = TypedBlockIr {
            statements: vec![TypedStatementIr::StaticRangeFor {
                binding: DefinitionId(0),
                start: TypedExpressionIr {
                    ty: IrType::I32,
                    effect: IrEffect::Immediate,
                    span: expression(IrType::Unit).span,
                    kind: TypedExpressionKind::Reference(DefinitionId(0)),
                },
                end: i32_expression(1),
                body: TypedBlockIr::default(),
                max_iterations: 1,
            }],
            tail: None,
        };
        assert_eq!(
            validate_block(&nonconstant, 1, &BTreeMap::new()),
            Err(TypedIrError::NonConstantStaticRange)
        );
    }

    #[test]
    fn migration_contract_rejects_wrong_field_owner_value_and_target_types() {
        let state = DefinitionId(0);
        let other_state = DefinitionId(1);
        let field = DefinitionId(2);
        let context = MigrationTypeContext {
            state_types: BTreeSet::from([state]),
            state_fields: BTreeMap::from([(field, (state, IrType::I32))]),
        };

        let wrong_owner = MigrationIntrinsicIr::OldFieldGet {
            object: Box::new(expression(IrType::Named(other_state))),
            field,
            value_type: IrType::I32,
        };
        assert_eq!(
            validate_migration_type_contract(&wrong_owner, &context),
            Err(TypedIrError::InvalidMigrationFieldOwner(field))
        );

        let wrong_value = MigrationIntrinsicIr::NewSet {
            object: Box::new(expression(IrType::Named(state))),
            field,
            value: Box::new(expression(IrType::Bool)),
        };
        assert_eq!(
            validate_migration_type_contract(&wrong_value, &context),
            Err(TypedIrError::InvalidMigrationFieldType(field))
        );

        let wrong_target = MigrationIntrinsicIr::Replace {
            identity: StableId::from_name("old"),
            target: Box::new(expression(IrType::Named(other_state))),
        };
        assert_eq!(
            validate_migration_type_contract(&wrong_target, &context),
            Err(TypedIrError::InvalidMigrationTargetType)
        );
    }
}
