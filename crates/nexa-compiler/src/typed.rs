use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::CompileError;
use crate::package::{
    PackageCompileOutput, PackageCompiledSource, PackageDebugInfo, PackageFunctionDebugInfo,
    PackageHostImportDebugInfo, PackageMainInfo, PackageModuleDebugInfo, PackagePublicSymbol,
    PackageReplCellInfo, PackageReplStateFieldInfo, PackageStateFieldInfo, PackageStateTypeInfo,
    PackageTestCallGraphNode, PackageTestForbiddenEffect, PackageTestInfo, PackageTestRejection,
    PackageVisibility, ReplCellCompileOutput, ReplSeedCompileOutput, StandaloneCompileOutput,
    standard_library_info,
};
use nexa_analysis::{
    BinaryOperator, BuiltinOperationIr, BuiltinVariantIr, CollectionIterationKindIr,
    DeclarationVisibility, Definition, DefinitionId, DefinitionKind, HostAsyncResultIr,
    HostTypeLayoutIr, IrAbandonPolicy, IrCancelPolicy, IrCompilationKind, IrEffect,
    IrHostFunctionMode, IrLiteral, IrType, MigrationIntrinsicIr, ModulePath, SourceKey,
    SourceRange, StateTypeIr, TypedBlockIr, TypedDeclarationBody, TypedExpressionIr,
    TypedExpressionKind, TypedFunctionIr, TypedPackageIr, TypedPatternIr, TypedPatternKind,
    TypedPlaceIr, TypedStatementIr, TypedTypeLayoutIr, UnaryOperator,
};
use nexa_bytecode::layout::LayoutTable;
use nexa_bytecode::{
    AbandonPolicy, ArrayType, AsyncResultType, BufferType, CancelPolicy, ClassType,
    CollectionIteratorKind, EnumType, EnumVariant, Function, FunctionEffect, HostCallMode,
    HostImport, Instruction, IteratorStateRegisters, LoopBound, MapType, Module, ModuleBuilder,
    ResourceTokenType, RootMap, ScriptExport, SetType, Signature, SnapshotType, SourceMapEntry,
    StandardIntrinsic, StateField, StateHandleType, StateSchema, StateType,
    StructField as BytecodeStructField, StructType, ValueType, array_type, buffer_type, map_type,
    option_type, parameterized_type_id, resource_token_type, result_type, set_type, snapshot_type,
    state_handle_type,
};
use nexa_core::{CanonicalSymbolIdentity, FileId, SourceSpan, StableId, StableSymbolId};
use nexa_diagnostics::SourceIdentity;

#[derive(Clone, Debug)]
struct TypedFunctionPlan<'a> {
    definition: &'a Definition,
    /// Borrowed straight from the analyzer snapshot when no Typed IR pass
    /// rewrote the body, owned otherwise (M5 WP37).
    function: std::borrow::Cow<'a, TypedFunctionIr>,
    index: u32,
}

#[derive(Clone, Copy, Debug)]
struct StandaloneMainExport {
    function_index: u32,
    source_function_index: u32,
    definition_span: SourceSpan,
}

#[derive(Default)]
struct CodegenInputs {
    strings: BTreeSet<String>,
    host_functions: BTreeSet<DefinitionId>,
}

#[cfg(test)]
const STANDALONE_MAIN_IDENTITY: &str = "nexa.standalone.main.v1";
/// Fixed typed export identity for Standalone Profile v1.
///
/// This is `StableId::from_name("nexa.standalone.main.v1")`, materialized as a constant so
/// runtime `ScriptExport` markers can reference one authority without copying the magic value.
pub const STANDALONE_MAIN_STABLE_ID: StableId = StableId(0x6c54_4e77_81f7_db72);
const STANDALONE_MAIN_WRAPPER_NAME: &str = "__nexa_standalone_main_task";

/// Stable bytecode export identity for the standalone package `main` ABI.
#[must_use]
pub const fn standalone_main_stable_id() -> StableId {
    STANDALONE_MAIN_STABLE_ID
}

#[derive(Clone, Debug)]
struct LoopPatch {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

#[derive(Clone, Copy, Debug)]
enum TypedStandardLowering {
    Intrinsic(StandardIntrinsic),
    ToString(TypedScalarToString),
    /// `Set::new` has no intrinsic form: the bytecode surface is a plain
    /// `SetNew` instruction carrying the instantiated type id.
    SetNew {
        element: ValueType,
    },
    /// `Set::clear` returns `()` and has no intrinsic form.
    SetClear {
        element: ValueType,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedNumericKind {
    I32,
    I64,
    F32,
    F64,
}

impl TypedNumericKind {
    fn from_ir_type(ty: &IrType, span: SourceSpan) -> Result<Self, CompileError> {
        match ty {
            IrType::I32 => Ok(Self::I32),
            IrType::I64 => Ok(Self::I64),
            IrType::F32 => Ok(Self::F32),
            IrType::F64 => Ok(Self::F64),
            _ => Err(CompileError::type_mismatch(None, None, span)),
        }
    }

    const fn value_type(self) -> ValueType {
        match self {
            Self::I32 => ValueType::I32,
            Self::I64 => ValueType::I64,
            Self::F32 => ValueType::F32,
            Self::F64 => ValueType::F64,
        }
    }

    const fn zero(self, destination: u16) -> Instruction {
        match self {
            Self::I32 => Instruction::LoadI32 {
                dst: destination,
                value: 0,
            },
            Self::I64 => Instruction::LoadI64 {
                dst: destination,
                value: 0,
            },
            Self::F32 => Instruction::LoadF32 {
                dst: destination,
                bits: 0_f32.to_bits(),
            },
            Self::F64 => Instruction::LoadF64 {
                dst: destination,
                bits: 0_f64.to_bits(),
            },
        }
    }

    const fn binary(
        self,
        operator: TypedNumericOperator,
        destination: u16,
        lhs: u16,
        rhs: u16,
    ) -> Instruction {
        match self {
            Self::I32 => operator.i32_instruction(destination, lhs, rhs),
            Self::I64 => operator.i64_instruction(destination, lhs, rhs),
            Self::F32 => operator.f32_instruction(destination, lhs, rhs),
            Self::F64 => operator.f64_instruction(destination, lhs, rhs),
        }
    }

    const fn compare_less(self, destination: u16, lhs: u16, rhs: u16) -> Instruction {
        match self {
            Self::I32 => Instruction::CompareLtI32 {
                dst: destination,
                lhs,
                rhs,
            },
            Self::I64 => Instruction::CompareLtI64 {
                dst: destination,
                lhs,
                rhs,
            },
            Self::F32 => Instruction::CompareLtF32 {
                dst: destination,
                lhs,
                rhs,
            },
            Self::F64 => Instruction::CompareLtF64 {
                dst: destination,
                lhs,
                rhs,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedNumericOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl TypedNumericOperator {
    const fn i32_instruction(self, dst: u16, lhs: u16, rhs: u16) -> Instruction {
        match self {
            Self::Add => Instruction::Add { dst, lhs, rhs },
            Self::Subtract => Instruction::Sub { dst, lhs, rhs },
            Self::Multiply => Instruction::Mul { dst, lhs, rhs },
            Self::Divide => Instruction::Div { dst, lhs, rhs },
            Self::Remainder => Instruction::RemI32 { dst, lhs, rhs },
        }
    }

    const fn i64_instruction(self, dst: u16, lhs: u16, rhs: u16) -> Instruction {
        match self {
            Self::Add => Instruction::AddI64 { dst, lhs, rhs },
            Self::Subtract => Instruction::SubI64 { dst, lhs, rhs },
            Self::Multiply => Instruction::MulI64 { dst, lhs, rhs },
            Self::Divide => Instruction::DivI64 { dst, lhs, rhs },
            Self::Remainder => Instruction::RemI64 { dst, lhs, rhs },
        }
    }

    const fn f32_instruction(self, dst: u16, lhs: u16, rhs: u16) -> Instruction {
        match self {
            Self::Add => Instruction::AddF32 { dst, lhs, rhs },
            Self::Subtract => Instruction::SubF32 { dst, lhs, rhs },
            Self::Multiply => Instruction::MulF32 { dst, lhs, rhs },
            Self::Divide => Instruction::DivF32 { dst, lhs, rhs },
            Self::Remainder => Instruction::RemF32 { dst, lhs, rhs },
        }
    }

    const fn f64_instruction(self, dst: u16, lhs: u16, rhs: u16) -> Instruction {
        match self {
            Self::Add => Instruction::AddF64 { dst, lhs, rhs },
            Self::Subtract => Instruction::SubF64 { dst, lhs, rhs },
            Self::Multiply => Instruction::MulF64 { dst, lhs, rhs },
            Self::Divide => Instruction::DivF64 { dst, lhs, rhs },
            Self::Remainder => Instruction::RemF64 { dst, lhs, rhs },
        }
    }
}

impl TryFrom<BinaryOperator> for TypedNumericOperator {
    type Error = ();

    fn try_from(operator: BinaryOperator) -> Result<Self, Self::Error> {
        match operator {
            BinaryOperator::Add => Ok(Self::Add),
            BinaryOperator::Subtract => Ok(Self::Subtract),
            BinaryOperator::Multiply => Ok(Self::Multiply),
            BinaryOperator::Divide => Ok(Self::Divide),
            BinaryOperator::Remainder => Ok(Self::Remainder),
            BinaryOperator::Equal
            | BinaryOperator::NotEqual
            | BinaryOperator::Less
            | BinaryOperator::LessEqual
            | BinaryOperator::Greater
            | BinaryOperator::GreaterEqual
            | BinaryOperator::And
            | BinaryOperator::Or => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedScalarToString {
    String,
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
}

impl TypedScalarToString {
    fn from_ir_type(ty: &IrType, span: SourceSpan) -> Result<Self, CompileError> {
        match ty {
            IrType::String => Ok(Self::String),
            IrType::I32 => Ok(Self::I32),
            IrType::I64 => Ok(Self::I64),
            IrType::F32 => Ok(Self::F32),
            IrType::F64 => Ok(Self::F64),
            IrType::Bool => Ok(Self::Bool),
            IrType::Rune => Ok(Self::Rune),
            _ => Err(CompileError::type_mismatch(None, None, span)),
        }
    }

    const fn value_type(self) -> ValueType {
        match self {
            Self::String => ValueType::String,
            Self::I32 => ValueType::I32,
            Self::I64 => ValueType::I64,
            Self::F32 => ValueType::F32,
            Self::F64 => ValueType::F64,
            Self::Bool => ValueType::Bool,
            Self::Rune => ValueType::Rune,
        }
    }

    const fn instruction(self, destination: u16, source: u16) -> Instruction {
        match self {
            Self::String => Instruction::StringToString {
                dst: destination,
                source,
            },
            Self::I32 => Instruction::I32ToString {
                dst: destination,
                source,
            },
            Self::I64 => Instruction::I64ToString {
                dst: destination,
                source,
            },
            Self::F32 => Instruction::F32ToString {
                dst: destination,
                source,
            },
            Self::F64 => Instruction::F64ToString {
                dst: destination,
                source,
            },
            Self::Bool => Instruction::BoolToString {
                dst: destination,
                source,
            },
            Self::Rune => Instruction::RuneToString {
                dst: destination,
                source,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypedAggregateKind {
    Struct,
    Class,
}

#[derive(Clone, Copy, Debug)]
enum TypedStructPlaceRoot {
    Definition {
        register: u16,
        ty: ValueType,
    },
    ClassField {
        object: u16,
        field: StableId,
        ty: ValueType,
    },
    ArrayIndex {
        base: u16,
        index: u16,
        ty: ValueType,
    },
    BufferIndex {
        base: u16,
        index: u16,
        ty: ValueType,
    },
    MapIndex {
        base: u16,
        index: u16,
        ty: ValueType,
    },
}

impl TypedStructPlaceRoot {
    const fn ty(self) -> ValueType {
        match self {
            Self::Definition { ty, .. }
            | Self::ClassField { ty, .. }
            | Self::ArrayIndex { ty, .. }
            | Self::BufferIndex { ty, .. }
            | Self::MapIndex { ty, .. } => ty,
        }
    }
}

fn flatten_struct_place<'a>(
    place: &'a TypedPlaceIr,
    fields: &mut Vec<DefinitionId>,
) -> &'a TypedPlaceIr {
    match place {
        TypedPlaceIr::Field { base, field } => {
            let root = flatten_struct_place(base, fields);
            fields.push(*field);
            root
        }
        TypedPlaceIr::Definition(_)
        | TypedPlaceIr::ClassField { .. }
        | TypedPlaceIr::Index { .. }
        | TypedPlaceIr::StateField { .. } => place,
    }
}

#[derive(Clone, Debug)]
struct TypedFieldLayout {
    definition: DefinitionId,
    stable_id: StableId,
    ty: ValueType,
    mutable: bool,
}

#[derive(Clone, Debug)]
struct TypedAggregateLayout {
    type_id: StableId,
    kind: TypedAggregateKind,
    fields: Vec<TypedFieldLayout>,
}

#[derive(Clone, Debug)]
struct TypedVariantLayout {
    stable_id: StableId,
    tag: u32,
    payload: Option<ValueType>,
}

#[derive(Clone, Debug)]
struct TypedEnumLayout {
    type_id: StableId,
    variants: BTreeMap<DefinitionId, TypedVariantLayout>,
}

#[derive(Clone, Debug, Default)]
struct TypedLayoutContext {
    aggregates: BTreeMap<DefinitionId, TypedAggregateLayout>,
    fields: BTreeMap<DefinitionId, (DefinitionId, TypedFieldLayout)>,
    enums: BTreeMap<DefinitionId, TypedEnumLayout>,
    variants: BTreeMap<DefinitionId, (DefinitionId, TypedVariantLayout)>,
    layout_table: LayoutTable,
}

impl TypedLayoutContext {
    fn physical_slots(&self, ty: ValueType, span: SourceSpan) -> Result<u16, CompileError> {
        let slots = self
            .layout_table
            .layout_of(ty)
            .map_err(|error| CompileError::unknown_type(error.to_string(), span))?
            .physical_slots;
        if slots == 0 {
            return Err(CompileError::unknown_type(
                "zero-slot value parameters are not supported by bytecode v7".into(),
                span,
            ));
        }
        Ok(slots)
    }
}

fn migration_field_owner(
    state_types: &[StateTypeIr],
    object_type: &IrType,
    field: DefinitionId,
    value_type: &IrType,
) -> Option<DefinitionId> {
    state_types.iter().find_map(|state| {
        state
            .fields
            .iter()
            .any(|candidate| candidate.definition == field && candidate.ty == *value_type)
            .then_some(state.definition)
            .filter(|owner| object_type == &IrType::Named(*owner))
    })
}

fn migration_state_type_exists(state_types: &[StateTypeIr], definition: DefinitionId) -> bool {
    state_types
        .iter()
        .any(|state| state.definition == definition)
}

fn typed_constant_i32(
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
            let value = typed_constant_i32(expression, constants, visiting);
            visiting.remove(definition);
            value
        }
        TypedExpressionKind::Unary {
            operator: UnaryOperator::Negate,
            operand,
        } => typed_constant_i32(operand, constants, visiting)?.checked_neg(),
        TypedExpressionKind::Binary {
            operator,
            left,
            right,
        } => {
            let left = typed_constant_i32(left, constants, visiting)?;
            let right = typed_constant_i32(right, constants, visiting)?;
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

fn validate_static_range_bound(
    start: &TypedExpressionIr,
    end: &TypedExpressionIr,
    declared: u32,
    constants: &BTreeMap<DefinitionId, &TypedExpressionIr>,
    span: SourceSpan,
) -> Result<u32, CompileError> {
    let start = typed_constant_i32(start, constants, &mut BTreeSet::new()).ok_or_else(|| {
        CompileError::verify(
            "typed static-range start is not a compile-time i32 constant".into(),
            span,
        )
    })?;
    let end = typed_constant_i32(end, constants, &mut BTreeSet::new()).ok_or_else(|| {
        CompileError::verify(
            "typed static-range end is not a compile-time i32 constant".into(),
            span,
        )
    })?;
    let exact =
        u32::try_from(i64::from(end).saturating_sub(i64::from(start)).max(0)).map_err(|_| {
            CompileError::verify(
                "typed static-range iteration count exceeds the u32 safety bound".into(),
                span,
            )
        })?;
    if declared != exact {
        return Err(CompileError::verify(
            format!(
                "typed static-range bound mismatch: declared {declared}, exact count is {exact}"
            ),
            span,
        ));
    }
    Ok(exact)
}

struct FunctionEmitter<'a> {
    package: &'a TypedPackageIr,
    function_indices: &'a BTreeMap<DefinitionId, u32>,
    host_imports: &'a BTreeMap<DefinitionId, u32>,
    standard_functions: &'a BTreeMap<DefinitionId, nexa_stdlib::Intrinsic>,
    layouts: &'a TypedLayoutContext,
    constants: &'a BTreeMap<DefinitionId, &'a TypedExpressionIr>,
    files: &'a BTreeMap<SourceKey, FileId>,
    string_indices: &'a BTreeMap<String, u32>,
    locals: BTreeMap<DefinitionId, u16>,
    /// WP45 local-map scalar replacement. Each admitted map has at most one
    /// constant-key write in a straight-line block, so its sole value lives
    /// in one typed register until a use requires materialization.
    inline_maps: BTreeMap<DefinitionId, InlineMapState>,
    /// WP45 local-array scalar replacement for bounded straight-line arrays.
    /// Each logical element owns one typed register; dynamic indexes,
    /// control-flow mutation, and escaping handles retain the heap path.
    inline_arrays: BTreeMap<DefinitionId, InlineArrayState>,
    /// WP45 non-escaping local classes. Direct field reads and writes use
    /// typed registers; identity-observing or aliasing uses retain `ClassNew`.
    inline_classes: BTreeMap<DefinitionId, (DefinitionId, u16)>,
    /// Whether the M5 optimized emission profile is active; the WP36
    /// reference pipeline keeps every fused form disabled.
    optimize: bool,
    register_types: Vec<Option<ValueType>>,
    parameter_slots: usize,
    function_effect: IrEffect,
    function_return_type: &'a IrType,
    code: Vec<Instruction>,
    spans: Vec<SourceSpan>,
    loop_bounds: Vec<LoopBound>,
    loop_stack: Vec<LoopPatch>,
    function_span: SourceSpan,
    constant_stack: BTreeSet<DefinitionId>,
}

#[derive(Clone)]
struct InlineMapPlan {
    value_type: IrType,
}

#[derive(Clone, Copy)]
struct InlineMapState {
    value_register: u16,
    entry_key: Option<i32>,
}

#[derive(Clone)]
struct InlineArrayPlan {
    element_type: IrType,
    capacity: u16,
}

#[derive(Clone, Copy)]
struct InlineArrayState {
    slots_base: u16,
    element_type: ValueType,
    element_slots: u16,
    capacity: u16,
    length: u16,
}

/// Lowers already-resolved, already-typed package IR directly to bytecode.
///
/// This entry point never invokes the lexer, parser, resolver, or type checker.
/// Unsupported typed operations fail explicitly instead of falling back to
/// source-level inference.
#[allow(clippy::too_many_lines)]
pub fn compile_typed_package(
    package: &TypedPackageIr,
) -> Result<PackageCompileOutput, CompileError> {
    compile_typed_package_with_profile(package, false, true)
}

/// M5 WP36 reference pipeline: identical front end and lowering, with every
/// M5 emission optimization disabled (Typed IR passes and physical struct
/// inlining). The differential gate compares this side against the
/// optimized pipeline for identical results, traps, and task lifecycles;
/// fuel totals are exempt per `BENCHMARK_PROTOCOL_V1.md`.
pub fn compile_typed_package_reference(
    package: &TypedPackageIr,
) -> Result<PackageCompileOutput, CompileError> {
    compile_typed_package_with_profile(package, false, false)
}

#[allow(clippy::too_many_lines)]
fn compile_typed_package_with_profile(
    package: &TypedPackageIr,
    emit_standalone_main: bool,
    optimize: bool,
) -> Result<PackageCompileOutput, CompileError> {
    let mut modules = package.modules().iter().collect::<Vec<_>>();
    modules.sort_by(|left, right| {
        (left.package_id.as_str(), left.module.as_str(), &left.source).cmp(&(
            right.package_id.as_str(),
            right.module.as_str(),
            &right.source,
        ))
    });
    let fallback_span = SourceSpan::default();
    let first_module = modules.first().ok_or_else(|| {
        CompileError::unknown_name("typed package has no modules".into(), fallback_span)
    })?;
    let entry_module = package
        .metadata()
        .entry_module
        .as_ref()
        .map_or_else(
            || {
                modules
                    .iter()
                    .find(|module| module.package_id == *package.package_id())
                    .map_or(first_module.module.as_str(), |module| {
                        module.module.as_str()
                    })
            },
            ModulePath::as_str,
        )
        .to_owned();

    let mut files = BTreeMap::new();
    let mut sources = Vec::new();
    let mut used_files = BTreeMap::new();
    for module in &modules {
        if module
            .virtual_module_path
            .as_ref()
            .is_some_and(|virtual_module| virtual_module != &module.module)
        {
            return Err(CompileError::unknown_name(
                format!(
                    "typed virtual module `{}` disagrees with semantic module `{}`",
                    module
                        .virtual_module_path
                        .as_ref()
                        .expect("checked as present"),
                    module.module
                ),
                fallback_span,
            ));
        }
        let file = FileId(module.file_id.0);
        if files.insert(module.source.clone(), file).is_some() {
            return Err(CompileError::duplicate_name(
                module.source.path.as_str().to_owned(),
                fallback_span,
                fallback_span,
            ));
        }
        if used_files
            .insert(file, module.source.path.as_str().to_owned())
            .is_some()
        {
            return Err(CompileError::duplicate_name(
                format!("artifact file {}", file.0),
                fallback_span,
                fallback_span,
            ));
        }
        let package_id = module.package_id.as_str().to_owned();
        let path = module.source.path.as_str().to_owned();
        sources.push(PackageCompiledSource {
            source_key: Some(module.source.clone()),
            identity: SourceIdentity::package(package_id.clone(), path.clone()),
            package_id: Some(package_id),
            module_path: Some(module.module.as_str().to_owned()),
            virtual_module_path: module.virtual_module_path.as_ref().map(ToString::to_string),
            path,
            file,
            source: Arc::from(module.syntax.source.as_str()),
            compiler_provided: module.package_id.as_str() == nexa_stdlib::PACKAGE_ID,
        });
    }
    for external in package.metadata().external_sources.iter() {
        let file = FileId(external.file_id.0);
        if used_files
            .insert(file, external.identity.to_string())
            .is_some()
        {
            return Err(CompileError::duplicate_name(
                format!("artifact file {}", file.0),
                fallback_span,
                fallback_span,
            ));
        }
        sources.push(PackageCompiledSource {
            source_key: None,
            identity: external.identity.clone(),
            package_id: external.identity.package_id().map(str::to_owned),
            module_path: None,
            virtual_module_path: None,
            path: external.identity.path().to_owned(),
            file,
            source: Arc::clone(&external.text),
            compiler_provided: false,
        });
    }
    sources.sort_by_key(|source| source.file);
    for (index, source) in sources.iter().enumerate() {
        let expected = u32::try_from(index.saturating_add(1))
            .map_err(|_| CompileError::too_many_registers(fallback_span))?;
        if source.file != FileId(expected) {
            return Err(CompileError::unknown_name(
                format!(
                    "artifact FileIds must be dense: expected {expected}, found {}",
                    source.file.0
                ),
                fallback_span,
            ));
        }
    }

    let mut declarations = BTreeMap::new();
    let mut function_plans = Vec::new();
    let mut constants = BTreeMap::new();
    let mut next_function = 0_u32;
    let standard_function_definitions = package
        .metadata()
        .standard_functions
        .iter()
        .map(|binding| binding.definition)
        .collect::<BTreeSet<_>>();
    for module in &modules {
        for declaration in module.declarations.iter() {
            if declarations
                .insert(declaration.definition, (module, declaration))
                .is_some()
            {
                let definition = package
                    .definition(declaration.definition)
                    .expect("TypedPackageIr validates declaration IDs");
                return Err(CompileError::duplicate_name(
                    definition.name.clone(),
                    source_span(&definition.span, &files)?,
                    source_span(&definition.span, &files)?,
                ));
            }
            match &declaration.body {
                TypedDeclarationBody::Function(function) => {
                    if standard_function_definitions.contains(&declaration.definition) {
                        continue;
                    }
                    let definition = package
                        .definition(declaration.definition)
                        .expect("TypedPackageIr validates declaration IDs");
                    function_plans.push(TypedFunctionPlan {
                        definition,
                        function: std::borrow::Cow::Borrowed(function),
                        index: next_function,
                    });
                    next_function = next_function.saturating_add(1);
                }
                TypedDeclarationBody::Const(expression) => {
                    constants.insert(declaration.definition, expression);
                }
                TypedDeclarationBody::TypeLayout { .. } | TypedDeclarationBody::External => {}
            }
        }
    }
    if optimize {
        // M5 WP37-WP45: optimize one owned package view so cross-function
        // passes can use real call counts while analyzer snapshots and their
        // fingerprints remain immutable. Every pass is structurally
        // revalidated before its result reaches lowering.
        let invariant = nexa_analysis::passes::PassInvariant::for_package(package);
        let mut optimized_functions = function_plans
            .iter()
            .map(|plan| (plan.definition.id, plan.function.as_ref().clone()))
            .collect::<BTreeMap<_, _>>();
        let _optimization_report = nexa_analysis::passes::PassManager::standard()
            .optimize_functions(&mut optimized_functions, &invariant)
            .map_err(|error| CompileError::verify(error.to_string(), fallback_span))?;
        for plan in &mut function_plans {
            plan.function = std::borrow::Cow::Owned(
                optimized_functions
                    .remove(&plan.definition.id)
                    .expect("every function plan participates in package optimization"),
            );
        }
    }
    let provisional_function_indices = function_plans
        .iter()
        .map(|plan| (plan.definition.id, plan.index))
        .collect::<BTreeMap<_, _>>();
    let provisional_call_graph =
        typed_test_call_graph(package, &provisional_function_indices, &function_plans)
            .into_iter()
            .map(|node| (node.function_index, node.calls))
            .collect::<BTreeMap<_, _>>();
    let mut reachable_functions = function_plans
        .iter()
        .filter(|plan| plan.definition.package_id.as_str() != nexa_stdlib::PACKAGE_ID)
        .map(|plan| plan.index)
        .collect::<BTreeSet<_>>();
    let mut pending_functions = reachable_functions.iter().copied().collect::<Vec<_>>();
    while let Some(function) = pending_functions.pop() {
        for callee in provisional_call_graph
            .get(&function)
            .into_iter()
            .flatten()
            .copied()
        {
            if reachable_functions.insert(callee) {
                pending_functions.push(callee);
            }
        }
    }
    function_plans.retain(|plan| reachable_functions.contains(&plan.index));
    for (index, plan) in function_plans.iter_mut().enumerate() {
        plan.index =
            u32::try_from(index).map_err(|_| CompileError::too_many_registers(fallback_span))?;
    }
    let function_indices = function_plans
        .iter()
        .map(|plan| (plan.definition.id, plan.index))
        .collect::<BTreeMap<_, _>>();

    let mut codegen_inputs = CodegenInputs {
        strings: BTreeSet::from([String::new()]),
        host_functions: BTreeSet::new(),
    };
    for plan in &function_plans {
        collect_block_codegen_inputs(&plan.function.body, &mut codegen_inputs);
        collect_type_strings(package, plan.function.as_ref(), &mut codegen_inputs.strings);
    }
    for expression in constants.values() {
        collect_expression_codegen_inputs(expression, &mut codegen_inputs);
    }
    collect_aggregate_format_strings(package, &mut codegen_inputs.strings);
    let mut builder = ModuleBuilder::new();
    let string_indices = codegen_inputs
        .strings
        .into_iter()
        .map(|value| {
            let index = builder.string(value.clone());
            (value, index)
        })
        .collect::<BTreeMap<_, _>>();

    let state_schema = typed_state_schema(package, &files)?;
    builder.state_schema(state_schema.clone());
    let layouts = emit_typed_type_metadata(package, &modules, &files, &state_schema, &mut builder)?;
    let (host_imports, host_contract_id) =
        emit_typed_host_imports(package, &codegen_inputs.host_functions, &mut builder)?;
    let standard_functions = typed_standard_functions(package, &files)?;

    let mut source_map = Vec::new();
    let mut migration_entry = None;
    let mut activation_entry = None;
    for plan in &function_plans {
        let function_span = source_span(&plan.definition.span, &files)?;
        let signature = typed_signature(package, plan.function.as_ref(), function_span)?;
        let mut emitter = FunctionEmitter::new(
            package,
            &function_indices,
            &host_imports,
            &standard_functions,
            &layouts,
            &constants,
            &files,
            &string_indices,
            plan.function.as_ref(),
            function_span,
            optimize,
        )?;
        emitter.emit_block(&plan.function.body)?;
        let effect = lower_effect(plan.function.effect);
        let terminated = matches!(
            emitter.code.last(),
            Some(
                Instruction::Return { .. }
                    | Instruction::ReturnVoid
                    | Instruction::CleanupReturn
                    | Instruction::Trap
            )
        );
        if !terminated {
            if signature.result.is_none() {
                emitter.push(
                    if effect == FunctionEffect::Cleanup {
                        Instruction::CleanupReturn
                    } else {
                        Instruction::ReturnVoid
                    },
                    function_span,
                );
            } else {
                return Err(CompileError::missing_return(function_span));
            }
        }
        let (function, entries) = emitter.finish(signature, effect, plan.index)?;
        source_map.extend(entries);
        builder.function(function);
        if effect == FunctionEffect::Migration && migration_entry.replace(plan.index).is_some() {
            return Err(CompileError::invalid_reload_metadata(
                "multiple migration entries",
                function_span,
            ));
        }
        if plan.function.effect == IrEffect::Activation
            && activation_entry.replace(plan.index).is_some()
        {
            return Err(CompileError::invalid_reload_metadata(
                "multiple activation entries",
                function_span,
            ));
        }
    }
    let standalone_main_export = if emit_standalone_main {
        emit_standalone_main_export(
            package,
            &function_indices,
            &function_plans,
            &files,
            &entry_module,
            &mut source_map,
            &mut builder,
        )?
    } else {
        None
    };
    emit_typed_exports(
        package,
        &function_indices,
        &function_plans,
        &files,
        standalone_main_export,
        &mut builder,
    )?;
    emit_typed_test_exports(
        package,
        &function_indices,
        &function_plans,
        &files,
        &mut builder,
    )?;
    builder
        .source_map(source_map)
        .reload_entries(migration_entry, activation_entry);
    let mut module = builder.finish();
    module.host_contract_id = host_contract_id;
    module.state_schema_fingerprint = package.metadata().state_schema_fingerprint;
    module.reload_metadata.state_schema_fingerprint = package.metadata().state_schema_fingerprint;
    let debug_info = typed_debug_info(
        package,
        &modules,
        &function_plans,
        &files,
        &entry_module,
        standalone_main_export,
        &host_imports,
    )?;
    let public_symbols = typed_public_symbols(package, &files)?;
    let state_surface = typed_state_surface(package, &files)?;
    let tests = typed_test_info(package, &function_indices, &function_plans, &files)?;
    let test_call_graph = typed_test_call_graph(package, &function_indices, &function_plans);
    let is_test = package.compilation_kind() == IrCompilationKind::Test;
    let (product_sources, test_sources) = if is_test {
        (Vec::new(), sources)
    } else {
        (sources, Vec::new())
    };
    let (product_debug, test_debug_info) = if is_test {
        (
            empty_debug_info(package, &entry_module),
            Some(debug_info.clone()),
        )
    } else {
        (debug_info, None)
    };

    Ok(PackageCompileOutput {
        module: module.clone(),
        test_module: is_test.then_some(module),
        sources: product_sources,
        test_sources,
        debug_info: product_debug,
        test_debug_info,
        public_symbols: if is_test { Vec::new() } else { public_symbols },
        state_surface: if is_test { Vec::new() } else { state_surface },
        tests: if is_test { tests } else { Vec::new() },
        test_call_graph: if is_test { test_call_graph } else { Vec::new() },
        standard_library: standard_library_info(),
        public_api_fingerprint: Some(package.metadata().public_api_fingerprint),
        state_schema_fingerprint: Some(package.metadata().state_schema_fingerprint),
    })
}

/// Compiles an analyzed package and enforces the standalone profile's only legal `main` ABI.
///
/// Embedded packages are still compiled with [`compile_typed_package`] and need no `main`.
/// Standalone packages must define `main` in the manifest entry module with exactly one
/// `Array<string>` parameter, an `i32` result, and either the ordinary or async Task effect.
pub fn compile_typed_standalone_package(
    package: &TypedPackageIr,
) -> Result<StandaloneCompileOutput, CompileError> {
    if package.compilation_kind() == IrCompilationKind::ReplCell {
        return Err(CompileError::invalid_main_signature(
            "REPL cells require compile_typed_repl_cell",
            SourceSpan::default(),
        ));
    }
    let mut compiled = compile_typed_package_with_profile(package, true, true)?;
    let main = standalone_main_info(package, &compiled)?;
    retain_standalone_generated_source_provenance(&mut compiled, main.definition_span)?;
    Ok(StandaloneCompileOutput {
        package: compiled,
        main,
    })
}

fn retain_standalone_generated_source_provenance(
    compiled: &mut PackageCompileOutput,
    main_span: SourceSpan,
) -> Result<(), CompileError> {
    if main_span.is_empty() {
        return Err(CompileError::invalid_main_signature(
            "standalone main must retain a non-empty source span",
            main_span,
        ));
    }
    let function_spans = compiled
        .debug_info
        .functions
        .iter()
        .map(|function| (function.function_index, function.definition_span))
        .collect::<BTreeMap<_, _>>();
    for entry in &mut compiled.module.source_map {
        if entry.span.is_empty() {
            entry.span = function_spans
                .get(&entry.function)
                .copied()
                .filter(|span| !span.is_empty())
                .unwrap_or(main_span);
        }
    }
    Ok(())
}

/// Compiles the analyzer-owned revision-zero REPL seed.
///
/// The seed authority comes exclusively from [`nexa_analysis::repl_seed_typed_ir`]. This function
/// does not reconstruct the reserved environment identity, schema, source, or fingerprint. The
/// supplied Contract identity is the full compact runtime identity selected by the façade and is
/// retained verbatim in the bytecode module.
pub fn compile_typed_repl_seed(
    host_contract_id: StableId,
) -> Result<ReplSeedCompileOutput, CompileError> {
    let seed = nexa_analysis::repl_seed_typed_ir();
    let mut compiled = compile_typed_package(&seed)?;
    let span = compiled
        .state_surface
        .first()
        .map_or(SourceSpan::default(), |state| state.definition_span);
    validate_repl_candidate_authority(&seed, &compiled, None, span)?;

    let metadata = seed.metadata();
    let [environment] = metadata.state_types.as_ref() else {
        return Err(CompileError::invalid_repl_entrypoint(
            "revision-zero REPL seed must contain exactly the reserved environment",
            span,
        ));
    };
    if seed.package_id().as_str() != nexa_analysis::REPL_PACKAGE_ID
        || metadata.entry_module.as_ref().map(ModulePath::as_str)
            != Some(nexa_analysis::REPL_MODULE_PATH)
        || metadata.repl_entry.is_some()
        || metadata.lifecycle.migration.is_some()
        || !metadata.exports.is_empty()
        || !metadata.host_bindings.is_empty()
        || environment.version != nexa_analysis::REPL_ENVIRONMENT_STATE_VERSION
        || !environment.fields.is_empty()
        || !compiled.module.functions.is_empty()
        || !compiled.module.host_imports.is_empty()
        || !compiled.module.exports.is_empty()
        || compiled.module.reload_metadata.migration_entry.is_some()
        || compiled.module.reload_metadata.activation_entry.is_some()
        || compiled.state_surface.len() != 1
        || !compiled.state_surface[0].fields.is_empty()
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "analyzer-owned revision-zero REPL seed violates its canonical ABI",
            span,
        ));
    }

    let state_schema_fingerprint = compiled.module.state_schema.fingerprint();
    if metadata.state_schema_fingerprint != state_schema_fingerprint
        || compiled.state_schema_fingerprint != Some(state_schema_fingerprint)
        || compiled.module.state_schema_fingerprint != state_schema_fingerprint
        || compiled.module.reload_metadata.state_schema_fingerprint != state_schema_fingerprint
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "revision-zero REPL seed schema fingerprint disagrees with Typed IR",
            span,
        ));
    }

    compiled.module.host_contract_id = Some(host_contract_id);
    Ok(ReplSeedCompileOutput {
        package: compiled,
        state_schema_fingerprint,
    })
}

/// Compiles one cumulative REPL candidate and emits exactly one typed cell marker.
///
/// The analyzer-owned [`nexa_analysis::ReplEntrypointIr`] identifies the function and its stable
/// cell identity. Ordinary entries receive a hidden Task wrapper; async entries are exported
/// directly. Neither path exposes a bytecode function index to callers.
#[allow(clippy::too_many_lines)]
pub fn compile_typed_repl_cell(
    package: &TypedPackageIr,
) -> Result<ReplCellCompileOutput, CompileError> {
    if package.compilation_kind() != IrCompilationKind::ReplCell {
        return Err(CompileError::invalid_repl_entrypoint(
            "compile_typed_repl_cell requires ReplCell Typed IR",
            SourceSpan::default(),
        ));
    }
    let entry = package
        .metadata()
        .repl_entry
        .as_ref()
        .ok_or_else(|| CompileError::missing_repl_entrypoint(SourceSpan::default()))?;
    let definition = package
        .definition(entry.function)
        .ok_or_else(|| CompileError::missing_repl_entrypoint(SourceSpan::default()))?;
    let expected_entry_name = format!("cell_{}", entry.cell_ordinal);
    let expected_entry_symbol = nexa_analysis::repl_cell_entry_symbol(entry.cell_ordinal);
    let expected_entry_identity = CanonicalSymbolIdentity::automatic(
        nexa_analysis::REPL_PACKAGE_ID,
        nexa_analysis::REPL_MODULE_PATH,
        nexa_core::SymbolKind::Function,
        &expected_entry_name,
    );
    let expected_definition_kind = match entry.effect {
        IrEffect::Ordinary => DefinitionKind::Function,
        IrEffect::Task => DefinitionKind::Task,
        IrEffect::Immediate | IrEffect::Migration | IrEffect::Activation | IrEffect::Cleanup => {
            DefinitionKind::Function
        }
    };
    if entry.cell_ordinal < nexa_analysis::REPL_FIRST_CELL_ORDINAL
        || package.package_id().as_str() != nexa_analysis::REPL_PACKAGE_ID
        || package
            .metadata()
            .entry_module
            .as_ref()
            .is_none_or(|module| module.as_str() != nexa_analysis::REPL_MODULE_PATH)
        || definition.package_id.as_str() != nexa_analysis::REPL_PACKAGE_ID
        || definition.module.as_str() != nexa_analysis::REPL_MODULE_PATH
        || definition.name != expected_entry_name
        || definition.visibility != DeclarationVisibility::Private
        || definition.kind != expected_definition_kind
        || definition.effect != entry.effect
        || definition.ty != entry.result
        || definition.stable_symbol.as_ref().is_none_or(|symbol| {
            symbol.canonical != expected_entry_identity
                || symbol.runtime_id != expected_entry_identity.runtime_id()
                || symbol.runtime_id != expected_entry_symbol
        })
        || entry.stable_id != expected_entry_symbol
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL cell package, module, entry name, ordinal, or stable identity violates the fixed synthetic ABI",
            SourceSpan::default(),
        ));
    }
    let function = package
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .find_map(|declaration| {
            (declaration.definition == entry.function)
                .then_some(&declaration.body)
                .and_then(|body| match body {
                    TypedDeclarationBody::Function(function) => Some(function),
                    _ => None,
                })
        })
        .ok_or_else(|| CompileError::missing_repl_entrypoint(SourceSpan::default()))?;
    let mut compiled = compile_typed_package(package)?;
    let debug = compiled
        .debug_info
        .functions
        .iter()
        .find(|debug| debug.stable_id == entry.stable_id)
        .cloned()
        .ok_or_else(|| CompileError::missing_repl_entrypoint(SourceSpan::default()))?;
    if debug.package_id != definition.package_id.as_str()
        || debug.module_path != definition.module.as_str()
        || debug.name != definition.name
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL entry debug identity disagrees with Typed IR",
            debug.definition_span,
        ));
    }
    let (environment, new_state_fields) = validate_repl_candidate_authority(
        package,
        &compiled,
        Some(function),
        debug.definition_span,
    )?;
    if !function.parameters.is_empty()
        || function.return_type != entry.result
        || function.effect != entry.effect
        || !matches!(entry.effect, IrEffect::Ordinary | IrEffect::Task)
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL entry must be a zero-argument ordinary or async function with the analyzed result",
            debug.definition_span,
        ));
    }
    let signature = Signature {
        parameters: Vec::new(),
        result: (entry.result != IrType::Unit)
            .then(|| lower_type(package, &entry.result, debug.definition_span))
            .transpose()?,
    };
    let source_function = compiled
        .module
        .functions
        .get(usize::try_from(debug.function_index).unwrap_or(usize::MAX))
        .ok_or_else(|| {
            CompileError::invalid_repl_entrypoint(
                "REPL entry targets a missing bytecode function",
                debug.definition_span,
            )
        })?;
    if source_function.signature != signature
        || source_function.effect != lower_effect(entry.effect)
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL entry bytecode signature disagrees with Typed IR",
            debug.definition_span,
        ));
    }
    let exported_function = if entry.effect == IrEffect::Task {
        debug.function_index
    } else {
        append_repl_task_wrapper(
            &mut compiled,
            debug.function_index,
            &signature,
            &debug,
            entry.cell_ordinal,
        )?
    };
    let stable_id = entry.stable_id.0;
    if compiled
        .module
        .exports
        .iter()
        .any(|export| export.stable_id == stable_id)
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL cell identity collides with another typed export",
            debug.definition_span,
        ));
    }
    compiled.module.exports.push(ScriptExport {
        stable_id,
        function: exported_function,
        signature: signature.clone(),
        effect: FunctionEffect::Task,
    });
    compiled
        .module
        .exports
        .sort_by_key(|export| (export.stable_id.0, export.function));
    Ok(ReplCellCompileOutput {
        package: compiled,
        cell: PackageReplCellInfo {
            stable_id,
            signature,
            effect: FunctionEffect::Task,
            definition_span: debug.definition_span,
            cell_ordinal: entry.cell_ordinal,
            environment,
            new_state_fields,
        },
    })
}

#[allow(clippy::too_many_lines)]
fn validate_repl_candidate_authority(
    package: &TypedPackageIr,
    compiled: &PackageCompileOutput,
    entry_function: Option<&TypedFunctionIr>,
    span: SourceSpan,
) -> Result<(StableId, Vec<PackageReplStateFieldInfo>), CompileError> {
    let metadata = package.metadata();
    if metadata.lifecycle != nexa_analysis::LifecycleBindingsIr::default() {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL candidates cannot contain migration, activation, or cleanup lifecycle entries",
            span,
        ));
    }

    let environment_id = nexa_analysis::repl_environment_symbol();
    let [environment] = metadata.state_types.as_ref() else {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL candidates must contain exactly the reserved environment state type",
            span,
        ));
    };
    if environment.stable_id != environment_id
        || environment.version != nexa_analysis::REPL_ENVIRONMENT_STATE_VERSION
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL environment state identity or version is invalid",
            span,
        ));
    }
    let environment_definition = package.definition(environment.definition).ok_or_else(|| {
        CompileError::invalid_repl_entrypoint("REPL environment state definition is missing", span)
    })?;
    let expected_environment_identity = CanonicalSymbolIdentity::automatic(
        nexa_analysis::REPL_PACKAGE_ID,
        nexa_analysis::REPL_MODULE_PATH,
        nexa_core::SymbolKind::Type,
        nexa_analysis::REPL_ENVIRONMENT_TYPE_NAME,
    );
    if environment_definition.kind != DefinitionKind::Class
        || environment_definition.package_id.as_str() != nexa_analysis::REPL_PACKAGE_ID
        || environment_definition.module.as_str() != nexa_analysis::REPL_MODULE_PATH
        || environment_definition.name != nexa_analysis::REPL_ENVIRONMENT_TYPE_NAME
        || environment_definition
            .stable_symbol
            .as_ref()
            .is_none_or(|identity| {
                identity.canonical != expected_environment_identity
                    || identity.runtime_id != expected_environment_identity.runtime_id()
                    || identity.runtime_id != environment_id
            })
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL environment must be the analyzer-owned state Class",
            span,
        ));
    }
    let environment_layout = package
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .find_map(|declaration| {
            (declaration.definition == environment.definition).then_some(&declaration.body)
        })
        .ok_or_else(|| {
            CompileError::invalid_repl_entrypoint("REPL environment Class layout is missing", span)
        })?;
    let TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Class {
        fields,
        state: Some(state),
    }) = environment_layout
    else {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL environment must have an authoritative state Class layout",
            span,
        ));
    };
    let mut ordered_layout_fields = fields.iter().collect::<Vec<_>>();
    ordered_layout_fields.sort_by_key(|field| field.order);
    if state.stable_id != environment_id
        || state.version != environment.version
        || ordered_layout_fields.len() != environment.fields.len()
        || ordered_layout_fields
            .iter()
            .zip(&environment.fields)
            .enumerate()
            .any(|(index, (layout_field, state_field))| {
                layout_field.order != u32::try_from(index).unwrap_or(u32::MAX)
                    || layout_field.definition != state_field.definition
                    || layout_field.ty != state_field.ty
            })
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL environment state metadata disagrees with its Class layout",
            span,
        ));
    }

    let lifecycle_functions = package
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .filter_map(|declaration| {
            let TypedDeclarationBody::Function(function) = &declaration.body else {
                return None;
            };
            matches!(function.effect, IrEffect::Migration | IrEffect::Activation)
                .then_some((declaration.definition, function.effect))
        })
        .collect::<Vec<_>>();
    if !lifecycle_functions.is_empty() {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL candidates cannot contain migration or activation functions",
            span,
        ));
    }
    validate_repl_cleanup_helpers(package, span)?;

    let expected_fingerprint = compiled.module.state_schema.fingerprint();
    if compiled.module.state_schema.types.len() != 1
        || compiled.module.state_schema.types[0].stable_id != environment_id.0
        || compiled.module.state_schema.types[0].version != environment.version
        || compiled.state_surface.len() != 1
        || compiled.state_surface[0].stable_id != environment_id
        || compiled.state_surface[0].version != environment.version
        || compiled.state_surface[0].package_id != nexa_analysis::REPL_PACKAGE_ID
        || compiled.state_surface[0].module_path != nexa_analysis::REPL_MODULE_PATH
        || compiled.state_surface[0].name != nexa_analysis::REPL_ENVIRONMENT_TYPE_NAME
        || compiled.state_surface[0].canonical_identity != expected_environment_identity
        || compiled.module.state_schema_fingerprint != expected_fingerprint
        || compiled.module.reload_metadata.state_schema_fingerprint != expected_fingerprint
        || compiled.state_schema_fingerprint != Some(expected_fingerprint)
        || compiled.module.reload_metadata.migration_entry.is_some()
        || compiled.module.reload_metadata.activation_entry.is_some()
    {
        return Err(CompileError::invalid_repl_entrypoint(
            "compiled REPL state or lifecycle metadata disagrees with Typed IR",
            span,
        ));
    }
    if entry_function.is_some() {
        let [host] = metadata.host_bindings.as_ref() else {
            return Err(CompileError::invalid_repl_entrypoint(
                "REPL cells require exactly one Hosted Console Contract authority",
                span,
            ));
        };
        if compiled.module.host_contract_id != Some(host.contract_stable_id) {
            return Err(CompileError::invalid_repl_entrypoint(
                "compiled REPL cell lost its Hosted Console Contract identity",
                span,
            ));
        }
    }
    let new_state_fields =
        repl_new_state_fields(package, environment, entry_function, environment_id.0, span)?;
    Ok((environment_id.0, new_state_fields))
}

fn validate_repl_cleanup_helpers(
    package: &TypedPackageIr,
    span: SourceSpan,
) -> Result<(), CompileError> {
    let functions = package
        .modules()
        .iter()
        .flat_map(|module| module.declarations.iter())
        .filter_map(|declaration| {
            let TypedDeclarationBody::Function(function) = &declaration.body else {
                return None;
            };
            let definition = package.definition(declaration.definition)?;
            Some((declaration.definition, (definition, function)))
        })
        .collect::<BTreeMap<_, _>>();
    let cleanup_functions = functions
        .iter()
        .filter_map(|(definition, (_, function))| {
            (function.effect == IrEffect::Cleanup).then_some(*definition)
        })
        .collect::<BTreeSet<_>>();
    let mut referenced_helpers = BTreeSet::new();
    for (_, function) in functions.values() {
        validate_repl_defer_block(
            package,
            &functions,
            &function.body,
            &mut referenced_helpers,
            span,
        )?;
    }
    if cleanup_functions != referenced_helpers {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL Cleanup functions must be private compiler-generated defer helpers referenced by a typed defer",
            span,
        ));
    }
    Ok(())
}

fn validate_repl_defer_block<'a>(
    package: &TypedPackageIr,
    functions: &BTreeMap<DefinitionId, (&'a Definition, &'a TypedFunctionIr)>,
    block: &TypedBlockIr,
    referenced_helpers: &mut BTreeSet<DefinitionId>,
    span: SourceSpan,
) -> Result<(), CompileError> {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Defer { cleanup, captures } => {
                let Some((definition, function)) = functions.get(cleanup).copied() else {
                    return Err(CompileError::invalid_repl_entrypoint(
                        "REPL defer targets a missing Cleanup helper",
                        span,
                    ));
                };
                let canonical_package = if definition.package_id.as_str() == nexa_stdlib::PACKAGE_ID
                {
                    nexa_stdlib::CANONICAL_PACKAGE_ID
                } else {
                    definition.package_id.as_str()
                };
                let expected_identity = CanonicalSymbolIdentity::automatic(
                    canonical_package,
                    definition.module.as_str(),
                    nexa_core::SymbolKind::Function,
                    &definition.name,
                );
                let signature_matches = function.parameters.len() == captures.len()
                    && function
                        .parameters
                        .iter()
                        .zip(captures)
                        .all(|(parameter, capture)| {
                            package
                                .definition(*parameter)
                                .is_some_and(|parameter| parameter.ty == capture.ty)
                        });
                if definition.kind != DefinitionKind::Function
                    || definition.visibility != DeclarationVisibility::Private
                    || !definition.name.starts_with("__defer_")
                    || definition.effect != IrEffect::Cleanup
                    || function.effect != IrEffect::Cleanup
                    || function.return_type != IrType::Unit
                    || !signature_matches
                    || definition.stable_symbol.as_ref().is_none_or(|symbol| {
                        symbol.canonical != expected_identity
                            || symbol.runtime_id != expected_identity.runtime_id()
                    })
                {
                    return Err(CompileError::invalid_repl_entrypoint(
                        "REPL defer helper identity, visibility, effect, or signature is invalid",
                        span,
                    ));
                }
                referenced_helpers.insert(*cleanup);
            }
            TypedStatementIr::If {
                then_block,
                else_block,
                ..
            } => {
                validate_repl_defer_block(
                    package,
                    functions,
                    then_block,
                    referenced_helpers,
                    span,
                )?;
                if let Some(else_block) = else_block {
                    validate_repl_defer_block(
                        package,
                        functions,
                        else_block,
                        referenced_helpers,
                        span,
                    )?;
                }
            }
            TypedStatementIr::While { body, .. }
            | TypedStatementIr::StaticRangeFor { body, .. }
            | TypedStatementIr::DynamicRangeFor { body, .. }
            | TypedStatementIr::CollectionFor { body, .. } => {
                validate_repl_defer_block(package, functions, body, referenced_helpers, span)?;
            }
            TypedStatementIr::Let { .. }
            | TypedStatementIr::Assign { .. }
            | TypedStatementIr::CompoundAssign { .. }
            | TypedStatementIr::Expression(_)
            | TypedStatementIr::Return(_)
            | TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    Ok(())
}

fn repl_new_state_fields(
    package: &TypedPackageIr,
    environment: &StateTypeIr,
    entry_function: Option<&TypedFunctionIr>,
    environment_id: StableId,
    span: SourceSpan,
) -> Result<Vec<PackageReplStateFieldInfo>, CompileError> {
    let entry = package.metadata().repl_entry.as_ref();
    let (Some(entry), Some(entry_function)) = (entry, entry_function) else {
        if entry.is_none() && entry_function.is_none() && environment.fields.is_empty() {
            return Ok(Vec::new());
        }
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL entrypoint and state-extension authority are inconsistent",
            span,
        ));
    };
    let entry_definition = package.definition(entry.function).ok_or_else(|| {
        CompileError::invalid_repl_entrypoint("REPL entrypoint definition is missing", span)
    })?;
    let mut field_definitions = Vec::new();
    let mut fields = Vec::new();
    let mut reached_current_suffix = false;
    for field in &environment.fields {
        let definition = package.definition(field.definition).ok_or_else(|| {
            CompileError::invalid_repl_entrypoint(
                "REPL environment field definition is missing",
                span,
            )
        })?;
        if definition.span.source != entry_definition.span.source {
            if reached_current_suffix {
                return Err(CompileError::invalid_repl_entrypoint(
                    "new REPL environment fields must be one strict append-only suffix",
                    span,
                ));
            }
            continue;
        }
        reached_current_suffix = true;
        field_definitions.push(field.definition);
        fields.push(PackageReplStateFieldInfo {
            stable_id: field.stable_id.0,
            ty: lower_type(package, &field.ty, span)?,
        });
    }
    validate_repl_new_field_writes(
        entry_function,
        environment.definition,
        environment_id,
        &field_definitions,
        span,
    )?;
    Ok(fields)
}

fn validate_repl_new_field_writes(
    entry: &TypedFunctionIr,
    environment_definition: DefinitionId,
    environment_id: StableId,
    expected_fields: &[DefinitionId],
    span: SourceSpan,
) -> Result<(), CompileError> {
    let expected = expected_fields.iter().copied().collect::<BTreeSet<_>>();
    let mut initialized = BTreeSet::new();
    let mut first_writes = Vec::new();
    for statement in &entry.body.statements {
        if repl_statement_reads_pending_field(statement, &expected, &initialized) {
            return Err(CompileError::invalid_repl_entrypoint(
                "REPL entrypoint reads a new environment field before its initializer write",
                span,
            ));
        }
        match statement {
            TypedStatementIr::Assign { target, .. } => {
                let Some(field) = repl_direct_environment_write(
                    target,
                    environment_definition,
                    environment_id,
                    &expected,
                )?
                else {
                    continue;
                };
                if initialized.insert(field) {
                    first_writes.push(field);
                }
            }
            TypedStatementIr::If {
                then_block,
                else_block,
                ..
            } => {
                let pending = expected
                    .difference(&initialized)
                    .copied()
                    .collect::<BTreeSet<_>>();
                if repl_block_writes_any_field(then_block, &pending)
                    || else_block
                        .as_ref()
                        .is_some_and(|block| repl_block_writes_any_field(block, &pending))
                {
                    return Err(CompileError::invalid_repl_entrypoint(
                        "new REPL environment fields must be initialized by top-level writes",
                        span,
                    ));
                }
            }
            TypedStatementIr::While { body, .. }
            | TypedStatementIr::StaticRangeFor { body, .. }
            | TypedStatementIr::DynamicRangeFor { body, .. }
            | TypedStatementIr::CollectionFor { body, .. } => {
                let pending = expected
                    .difference(&initialized)
                    .copied()
                    .collect::<BTreeSet<_>>();
                if repl_block_writes_any_field(body, &pending) {
                    return Err(CompileError::invalid_repl_entrypoint(
                        "new REPL environment fields must be initialized by top-level writes",
                        span,
                    ));
                }
            }
            TypedStatementIr::Let { .. }
            | TypedStatementIr::Return(_)
            | TypedStatementIr::Expression(_)
            | TypedStatementIr::CompoundAssign { .. }
            | TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Defer { .. }
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if first_writes != expected_fields {
        return Err(CompileError::invalid_repl_entrypoint(
            "REPL entrypoint does not initialize every new environment field in source order",
            span,
        ));
    }
    Ok(())
}

fn repl_statement_reads_pending_field(
    statement: &TypedStatementIr,
    expected: &BTreeSet<DefinitionId>,
    initialized: &BTreeSet<DefinitionId>,
) -> bool {
    let reads = |expression: &TypedExpressionIr| {
        repl_expression_reads_pending_field(expression, expected, initialized)
    };
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
            value.as_ref().is_some_and(reads)
        }
        TypedStatementIr::Assign { target, value }
        | TypedStatementIr::CompoundAssign { target, value, .. } => {
            repl_place_reads_pending_field(target, expected, initialized) || reads(value)
        }
        TypedStatementIr::Expression(value) => reads(value),
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            reads(condition)
                || repl_block_reads_pending_field(then_block, expected, initialized)
                || else_block.as_ref().is_some_and(|block| {
                    repl_block_reads_pending_field(block, expected, initialized)
                })
        }
        TypedStatementIr::While {
            condition, body, ..
        } => reads(condition) || repl_block_reads_pending_field(body, expected, initialized),
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        }
        | TypedStatementIr::DynamicRangeFor {
            start, end, body, ..
        } => {
            reads(start)
                || reads(end)
                || repl_block_reads_pending_field(body, expected, initialized)
        }
        TypedStatementIr::CollectionFor { iterable, body, .. } => {
            reads(iterable) || repl_block_reads_pending_field(body, expected, initialized)
        }
        TypedStatementIr::Defer { captures, .. } => captures.iter().any(reads),
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {
            false
        }
    }
}

fn repl_block_reads_pending_field(
    block: &TypedBlockIr,
    expected: &BTreeSet<DefinitionId>,
    initialized: &BTreeSet<DefinitionId>,
) -> bool {
    block
        .statements
        .iter()
        .any(|statement| repl_statement_reads_pending_field(statement, expected, initialized))
        || block
            .tail
            .as_deref()
            .is_some_and(|tail| repl_expression_reads_pending_field(tail, expected, initialized))
}

fn repl_place_reads_pending_field(
    place: &TypedPlaceIr,
    expected: &BTreeSet<DefinitionId>,
    initialized: &BTreeSet<DefinitionId>,
) -> bool {
    match place {
        TypedPlaceIr::Definition(_) => false,
        TypedPlaceIr::Field { base, .. } => {
            repl_place_reads_pending_field(base, expected, initialized)
        }
        TypedPlaceIr::ClassField { object, .. } => {
            repl_expression_reads_pending_field(object, expected, initialized)
        }
        TypedPlaceIr::StateField { base, .. } => {
            repl_expression_reads_pending_field(base, expected, initialized)
        }
        TypedPlaceIr::Index { base, index } => {
            repl_expression_reads_pending_field(base, expected, initialized)
                || repl_expression_reads_pending_field(index, expected, initialized)
        }
    }
}

fn repl_expression_reads_pending_field(
    expression: &TypedExpressionIr,
    expected: &BTreeSet<DefinitionId>,
    initialized: &BTreeSet<DefinitionId>,
) -> bool {
    let reads = |value: &TypedExpressionIr| {
        repl_expression_reads_pending_field(value, expected, initialized)
    };
    match &expression.kind {
        TypedExpressionKind::StateField { base, field } => {
            (expected.contains(field) && !initialized.contains(field)) || reads(base)
        }
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Await(operand)
        | TypedExpressionKind::Field { base: operand, .. }
        | TypedExpressionKind::Try(operand) => reads(operand),
        TypedExpressionKind::Binary { left, right, .. }
        | TypedExpressionKind::Index {
            base: left,
            index: right,
        } => reads(left) || reads(right),
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. }
        | TypedExpressionKind::Array(arguments)
        | TypedExpressionKind::Tuple(arguments)
        | TypedExpressionKind::StringInterpolation(arguments) => arguments.iter().any(reads),
        TypedExpressionKind::Construct { fields, .. } => {
            fields.iter().any(|(_, value)| reads(value))
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            update.as_deref().is_some_and(reads) || fields.iter().any(|(_, value)| reads(value))
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            payload.as_deref().is_some_and(reads)
        }
        TypedExpressionKind::Match { value, arms } => {
            reads(value) || arms.iter().any(|arm| reads(&arm.value))
        }
        TypedExpressionKind::Update { base, fields } => {
            reads(base) || fields.iter().any(|(_, value)| reads(value))
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. } => reads(object),
            MigrationIntrinsicIr::NewSet { object, value, .. } => reads(object) || reads(value),
            MigrationIntrinsicIr::Replace { target, .. } => reads(target),
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

fn repl_direct_environment_write(
    target: &TypedPlaceIr,
    environment_definition: DefinitionId,
    environment_id: StableId,
    expected: &BTreeSet<DefinitionId>,
) -> Result<Option<DefinitionId>, CompileError> {
    let TypedPlaceIr::StateField { base, field } = target else {
        return Ok(None);
    };
    if !expected.contains(field) {
        return Ok(None);
    }
    let valid_base = base.ty == IrType::Named(environment_definition)
        && base.effect == IrEffect::Immediate
        && matches!(
            &base.kind,
            TypedExpressionKind::PersistentStateGet {
                identity,
                state_type,
            } if *identity == environment_id && *state_type == environment_definition
        );
    if !valid_base {
        return Err(CompileError::invalid_repl_entrypoint(
            "new REPL field write does not target the analyzer-owned environment",
            SourceSpan::default(),
        ));
    }
    Ok(Some(*field))
}

fn repl_block_writes_any_field(block: &TypedBlockIr, expected: &BTreeSet<DefinitionId>) -> bool {
    block.statements.iter().any(|statement| match statement {
        TypedStatementIr::Assign { target, .. }
        | TypedStatementIr::CompoundAssign { target, .. } => {
            matches!(target, TypedPlaceIr::StateField { field, .. } if expected.contains(field))
        }
        TypedStatementIr::If {
            then_block,
            else_block,
            ..
        } => {
            repl_block_writes_any_field(then_block, expected)
                || else_block
                    .as_ref()
                    .is_some_and(|block| repl_block_writes_any_field(block, expected))
        }
        TypedStatementIr::While { body, .. }
        | TypedStatementIr::StaticRangeFor { body, .. }
        | TypedStatementIr::DynamicRangeFor { body, .. }
        | TypedStatementIr::CollectionFor { body, .. } => {
            repl_block_writes_any_field(body, expected)
        }
        TypedStatementIr::Let { .. }
        | TypedStatementIr::Return(_)
        | TypedStatementIr::Expression(_)
        | TypedStatementIr::Break
        | TypedStatementIr::Continue
        | TypedStatementIr::Defer { .. }
        | TypedStatementIr::Yield { .. } => false,
    })
}

fn append_repl_task_wrapper(
    compiled: &mut PackageCompileOutput,
    source_function: u32,
    signature: &Signature,
    source_debug: &PackageFunctionDebugInfo,
    cell_ordinal: u64,
) -> Result<u32, CompileError> {
    let wrapper = u32::try_from(compiled.module.functions.len())
        .map_err(|_| CompileError::too_many_registers(source_debug.definition_span))?;
    let result_type = signature.result.unwrap_or(ValueType::I32);
    let code = vec![
        Instruction::Call {
            function: source_function,
            args_base: 0,
            args_count: 0,
            dst: 0,
        },
        signature
            .result
            .map_or(Instruction::ReturnVoid, |_| Instruction::Return {
                source: 0,
            }),
    ];
    let register_types = [Some(result_type)];
    let safepoints = collect_safepoints(&code);
    let (root_bitmap, root_maps) = typed_exact_root_maps(
        &register_types,
        None,
        0,
        &code,
        &safepoints,
        source_debug.definition_span,
    )?;
    compiled.module.functions.push(Function {
        signature: signature.clone(),
        parameter_slots: u16::try_from(signature.parameters.len())
            .map_err(|_| CompileError::too_many_registers(source_debug.definition_span))?,
        registers: 1,
        frame_bytes: 8,
        root_bitmap,
        root_maps,
        safepoints,
        loop_bounds: Vec::new(),
        effect: FunctionEffect::Task,
        max_static_call_depth: 1,
        code,
    });
    compiled
        .module
        .source_map
        .extend([0_u32, 1].into_iter().map(|pc| SourceMapEntry {
            function: wrapper,
            pc_start: pc,
            pc_end: pc + 1,
            span: source_debug.definition_span,
        }));
    let wrapper_name = format!("__nexa_repl_cell_{cell_ordinal}_task");
    let canonical_identity = CanonicalSymbolIdentity::explicit(
        &source_debug.package_id,
        nexa_core::SymbolKind::Task,
        &wrapper_name,
    );
    compiled
        .debug_info
        .functions
        .push(PackageFunctionDebugInfo {
            function_index: wrapper,
            package_id: source_debug.package_id.clone(),
            module_path: source_debug.module_path.clone(),
            name: wrapper_name,
            stable_id: canonical_identity.runtime_id(),
            canonical_identity,
            definition_span: source_debug.definition_span,
            effect: FunctionEffect::Task,
            visibility: PackageVisibility::Private,
        });
    let module = compiled
        .debug_info
        .modules
        .iter_mut()
        .find(|module| {
            module.package_id == source_debug.package_id
                && module.module_path == source_debug.module_path
                && module.file == source_debug.definition_span.file
        })
        .ok_or_else(|| {
            CompileError::invalid_repl_entrypoint(
                "REPL entry module has no debug owner",
                source_debug.definition_span,
            )
        })?;
    module.function_indices.push(wrapper);
    Ok(wrapper)
}

#[allow(clippy::too_many_lines)]
fn standalone_main_info(
    package: &TypedPackageIr,
    compiled: &PackageCompileOutput,
) -> Result<PackageMainInfo, CompileError> {
    let entry_module = package.metadata().entry_module.as_ref().map_or(
        compiled.debug_info.entry_module.as_str(),
        ModulePath::as_str,
    );
    let entry_span = compiled
        .debug_info
        .modules
        .iter()
        .find(|module| {
            module.package_id == package.package_id().as_str() && module.module_path == entry_module
        })
        .map_or_else(SourceSpan::default, |module| module.source_span);
    let plan = package
        .modules()
        .iter()
        .filter(|module| {
            module.package_id == *package.package_id() && module.module.as_str() == entry_module
        })
        .flat_map(|module| module.declarations.iter())
        .find_map(|declaration| {
            let definition = package.definition(declaration.definition)?;
            let TypedDeclarationBody::Function(function) = &declaration.body else {
                return None;
            };
            (definition.name == "main").then_some((definition, function))
        })
        .ok_or_else(|| CompileError::missing_main(entry_module.to_owned(), entry_span))?;
    let debug = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| {
            function.package_id == package.package_id().as_str()
                && function.module_path == entry_module
                && function.name == "main"
        })
        .ok_or_else(|| CompileError::missing_main(entry_module.to_owned(), entry_span))?;
    if let Some(message) = invalid_standalone_main_signature(package, plan.1) {
        return Err(CompileError::invalid_main_signature(
            message,
            debug.definition_span,
        ));
    }
    let source_effect = lower_effect(plan.1.effect);
    if debug.effect != source_effect {
        return Err(CompileError::invalid_main_signature(
            "standalone main debug effect disagrees with typed IR",
            debug.definition_span,
        ));
    }
    let expected_signature = standalone_main_signature();
    let export = compiled
        .module
        .exports
        .iter()
        .find(|export| export.stable_id == standalone_main_stable_id())
        .ok_or_else(|| {
            CompileError::invalid_main_signature(
                "standalone main export marker is missing",
                debug.definition_span,
            )
        })?;
    let source_is_canonical_task = source_effect == FunctionEffect::Task
        && plan.1.parameters.len() == 1
        && plan.1.return_type == IrType::I32;
    if export.signature != expected_signature
        || export.effect != FunctionEffect::Task
        || (source_is_canonical_task && export.function != debug.function_index)
        || (!source_is_canonical_task
            && !is_standalone_main_adapter(
                &compiled.module,
                export.function,
                debug.function_index,
                plan.1.parameters.len(),
                plan.1.return_type == IrType::Unit,
            ))
    {
        return Err(CompileError::invalid_main_signature(
            "standalone main export marker disagrees with the validated ABI",
            debug.definition_span,
        ));
    }
    let Some(exported_function) = compiled
        .module
        .functions
        .get(usize::try_from(export.function).unwrap_or(usize::MAX))
    else {
        return Err(CompileError::invalid_main_signature(
            "standalone main export targets a missing function",
            debug.definition_span,
        ));
    };
    if exported_function.effect != FunctionEffect::Task {
        return Err(CompileError::invalid_main_signature(
            "standalone main export must target a Task function",
            debug.definition_span,
        ));
    }
    Ok(PackageMainInfo {
        stable_id: standalone_main_stable_id(),
        effect: FunctionEffect::Task,
        definition_span: debug.definition_span,
    })
}

fn is_standalone_main_adapter(
    module: &nexa_bytecode::Module,
    wrapper: u32,
    main: u32,
    parameter_count: usize,
    returns_unit: bool,
) -> bool {
    module
        .functions
        .get(usize::try_from(wrapper).unwrap_or(usize::MAX))
        .is_some_and(|function| {
            let mut expected = vec![Instruction::Call {
                function: main,
                args_base: 0,
                args_count: u16::try_from(parameter_count).unwrap_or(u16::MAX),
                dst: 1,
            }];
            if returns_unit {
                expected.push(Instruction::LoadI32 { dst: 1, value: 0 });
            }
            expected.push(Instruction::Return { source: 1 });
            function.signature == standalone_main_signature()
                && function.effect == FunctionEffect::Task
                && function.code == expected
        })
}

fn invalid_standalone_main_signature(
    package: &TypedPackageIr,
    function: &TypedFunctionIr,
) -> Option<&'static str> {
    if function.parameters.len() > 1 {
        return Some("standalone main accepts either no arguments or one Array<string> argument");
    }
    if let Some(parameter) = function.parameters.first() {
        let Some(parameter) = package.definition(*parameter) else {
            return Some("standalone main parameter is missing from typed IR");
        };
        if parameter.ty != IrType::Array(Box::new(IrType::String)) {
            return Some("standalone main argument must have type Array<string>");
        }
    }
    if !matches!(function.return_type, IrType::Unit | IrType::I32) {
        return Some("standalone main must return Unit or i32");
    }
    if !matches!(function.effect, IrEffect::Ordinary | IrEffect::Task) {
        return Some("standalone main must be `fn` or `async fn` without lifecycle attributes");
    }
    None
}

fn standalone_main_signature() -> Signature {
    Signature {
        parameters: vec![ValueType::Named(array_type(ValueType::String))],
        result: Some(ValueType::I32),
    }
}

fn valid_standalone_main_plan<'a>(
    package: &TypedPackageIr,
    function_plans: &'a [TypedFunctionPlan<'a>],
    entry_module: &str,
) -> Option<&'a TypedFunctionPlan<'a>> {
    function_plans.iter().find(|plan| {
        plan.definition.package_id == *package.package_id()
            && plan.definition.module.as_str() == entry_module
            && plan.definition.name == "main"
            && invalid_standalone_main_signature(package, plan.function.as_ref()).is_none()
    })
}

fn emit_standalone_main_export(
    package: &TypedPackageIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    function_plans: &[TypedFunctionPlan<'_>],
    files: &BTreeMap<SourceKey, FileId>,
    entry_module: &str,
    source_map: &mut Vec<SourceMapEntry>,
    builder: &mut ModuleBuilder,
) -> Result<Option<StandaloneMainExport>, CompileError> {
    let Some(main) = valid_standalone_main_plan(package, function_plans, entry_module) else {
        return Ok(None);
    };
    let definition_span = source_span(&main.definition.span, files)?;
    let source_function = function_indices[&main.definition.id];
    let canonical_source_signature =
        main.function.parameters.len() == 1 && main.function.return_type == IrType::I32;
    let function_index = match main.function.effect {
        IrEffect::Task if canonical_source_signature => source_function,
        IrEffect::Task | IrEffect::Ordinary => {
            let wrapper_index = u32::try_from(function_plans.len())
                .map_err(|_| CompileError::too_many_registers(definition_span))?;
            let mut code = vec![Instruction::Call {
                function: source_function,
                args_base: 0,
                args_count: u16::try_from(main.function.parameters.len())
                    .expect("validated standalone main has at most one parameter"),
                dst: 1,
            }];
            if main.function.return_type == IrType::Unit {
                code.push(Instruction::LoadI32 { dst: 1, value: 0 });
            }
            code.push(Instruction::Return { source: 1 });
            let register_types = vec![
                Some(ValueType::Named(array_type(ValueType::String))),
                Some(ValueType::I32),
            ];
            let safepoints = collect_safepoints(&code);
            let (root_bitmap, root_maps) = typed_exact_root_maps(
                &register_types,
                None,
                1,
                &code,
                &safepoints,
                definition_span,
            )?;
            source_map.extend(code.iter().enumerate().map(|(pc, _)| SourceMapEntry {
                function: wrapper_index,
                pc_start: u32::try_from(pc).unwrap_or(u32::MAX),
                pc_end: u32::try_from(pc.saturating_add(1)).unwrap_or(u32::MAX),
                span: definition_span,
            }));
            builder.function(Function {
                signature: standalone_main_signature(),
                parameter_slots: 1,
                registers: 2,
                frame_bytes: 16,
                root_bitmap,
                root_maps,
                safepoints,
                loop_bounds: Vec::new(),
                effect: FunctionEffect::Task,
                max_static_call_depth: 1,
                code,
            });
            wrapper_index
        }
        IrEffect::Immediate | IrEffect::Activation | IrEffect::Migration | IrEffect::Cleanup => {
            return Err(CompileError::invalid_main_signature(
                "standalone main must lower from `fn` or `async fn`",
                definition_span,
            ));
        }
    };
    Ok(Some(StandaloneMainExport {
        function_index,
        source_function_index: source_function,
        definition_span,
    }))
}

/// WP45 bounded local-array scalar replacement.
const MAX_INLINE_ARRAY_SLOTS: u16 = 16;

fn inline_array_candidates(function: &TypedFunctionIr) -> BTreeMap<DefinitionId, InlineArrayPlan> {
    let mut declarations = BTreeMap::new();
    for statement in &function.body.statements {
        let TypedStatementIr::Let {
            definition,
            mutable: false,
            value: Some(value),
        } = statement
        else {
            continue;
        };
        let TypedExpressionKind::BuiltinCall {
            operation: BuiltinOperationIr::ArrayNew,
            ..
        } = &value.kind
        else {
            continue;
        };
        let IrType::Array(element_type) = &value.ty else {
            continue;
        };
        declarations.insert(*definition, element_type.as_ref().clone());
    }
    declarations
        .into_iter()
        .filter_map(|(definition, element_type)| {
            inline_array_candidate_capacity(function, definition).map(|capacity| {
                (
                    definition,
                    InlineArrayPlan {
                        element_type,
                        capacity,
                    },
                )
            })
        })
        .collect()
}

fn allocate_inline_array_states(
    package: &TypedPackageIr,
    layouts: &TypedLayoutContext,
    function: &TypedFunctionIr,
    function_span: SourceSpan,
    register_types: &mut Vec<Option<ValueType>>,
) -> Result<BTreeMap<DefinitionId, InlineArrayState>, CompileError> {
    let mut states = BTreeMap::new();
    for (definition, plan) in inline_array_candidates(function) {
        let slots_base = u16::try_from(register_types.len())
            .map_err(|_| CompileError::too_many_registers(function_span))?;
        let element_type = lower_type(package, &plan.element_type, function_span)?;
        let element_slots = layouts.physical_slots(element_type, function_span)?;
        let physical_capacity = plan
            .capacity
            .checked_mul(element_slots)
            .ok_or_else(|| CompileError::too_many_registers(function_span))?;
        if physical_capacity > MAX_INLINE_ARRAY_SLOTS {
            continue;
        }
        for _ in 0..plan.capacity {
            register_types.push(Some(element_type));
            register_types.extend((1..element_slots).map(|_| None));
        }
        states.insert(
            definition,
            InlineArrayState {
                slots_base,
                element_type,
                element_slots,
                capacity: plan.capacity,
                length: 0,
            },
        );
    }
    Ok(states)
}

fn inline_array_candidate_capacity(
    function: &TypedFunctionIr,
    definition: DefinitionId,
) -> Option<u16> {
    let mut length = 0_u16;
    for statement in &function.body.statements {
        match statement {
            TypedStatementIr::Expression(expression) => {
                match apply_inline_array_write(expression, definition, &mut length) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(()) => return None,
                }
                if !inline_array_read_expression_allowed(expression, definition, length) {
                    return None;
                }
            }
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if value.as_ref().is_some_and(|value| {
                    !inline_array_read_expression_allowed(value, definition, length)
                }) {
                    return None;
                }
            }
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
                if place_references_definition(target, definition)
                    || !inline_array_read_expression_allowed(value, definition, length)
                {
                    return None;
                }
            }
            TypedStatementIr::If { .. }
            | TypedStatementIr::While { .. }
            | TypedStatementIr::StaticRangeFor { .. }
            | TypedStatementIr::DynamicRangeFor { .. }
            | TypedStatementIr::CollectionFor { .. }
            | TypedStatementIr::Defer { .. } => {
                if statement_references_definition(statement, definition) {
                    return None;
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    function
        .body
        .tail
        .as_ref()
        .is_none_or(|tail| inline_array_read_expression_allowed(tail, definition, length))
        .then_some(length)
}

fn apply_inline_array_write(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
    length: &mut u16,
) -> Result<bool, ()> {
    let TypedExpressionKind::BuiltinCall {
        operation,
        arguments,
        ..
    } = &expression.kind
    else {
        return Ok(false);
    };
    if !matches!(
        arguments.first().map(|argument| &argument.kind),
        Some(TypedExpressionKind::Reference(receiver)) if *receiver == definition
    ) {
        return Ok(false);
    }
    match operation {
        BuiltinOperationIr::ArrayPush
            if arguments.len() == 2
                && !expression_references_definition(&arguments[1], definition) =>
        {
            if *length >= MAX_INLINE_ARRAY_SLOTS {
                return Err(());
            }
            *length += 1;
            Ok(true)
        }
        BuiltinOperationIr::ArraySet
            if arguments.len() == 3
                && !expression_references_definition(&arguments[2], definition) =>
        {
            let TypedExpressionKind::Literal(IrLiteral::I32(index)) = &arguments[1].kind else {
                return Err(());
            };
            if usize::try_from(*index)
                .ok()
                .is_none_or(|index| index >= usize::from(*length))
            {
                return Err(());
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn inline_array_read_expression_allowed(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
    length: u16,
) -> bool {
    match &expression.kind {
        TypedExpressionKind::Reference(candidate) => *candidate != definition,
        TypedExpressionKind::BuiltinCall {
            operation,
            arguments,
            ..
        } if matches!(
            arguments.first().map(|argument| &argument.kind),
            Some(TypedExpressionKind::Reference(receiver)) if *receiver == definition
        ) =>
        {
            match operation {
                BuiltinOperationIr::ArrayLen => arguments.len() == 1,
                BuiltinOperationIr::ArrayGet if arguments.len() == 2 => {
                    let TypedExpressionKind::Literal(IrLiteral::I32(index)) = &arguments[1].kind
                    else {
                        return false;
                    };
                    usize::try_from(*index)
                        .ok()
                        .is_some_and(|index| index < usize::from(length))
                }
                _ => false,
            }
        }
        _ => {
            let mut children = Vec::new();
            collect_expression_children(expression, &mut children);
            children
                .into_iter()
                .all(|child| inline_array_read_expression_allowed(child, definition, length))
        }
    }
}

/// WP45 local-map scalar replacement candidates.
///
/// This first complete slice is intentionally path-insensitive: the binding
/// must be created in the function's top-level block, may have at most one
/// top-level constant-key `set`, and may otherwise only be read by
/// constant-key `get`. Any use under control flow or as an ordinary value
/// escapes and keeps the real heap map.
fn inline_map_candidates(function: &TypedFunctionIr) -> BTreeMap<DefinitionId, InlineMapPlan> {
    let mut candidates = BTreeMap::new();
    for statement in &function.body.statements {
        let TypedStatementIr::Let {
            definition,
            mutable: false,
            value: Some(value),
        } = statement
        else {
            continue;
        };
        let TypedExpressionKind::BuiltinCall {
            operation: BuiltinOperationIr::MapNew,
            ..
        } = &value.kind
        else {
            continue;
        };
        let IrType::Map(_, value_type) = &value.ty else {
            continue;
        };
        candidates.insert(
            *definition,
            InlineMapPlan {
                value_type: value_type.as_ref().clone(),
            },
        );
    }
    candidates.retain(|definition, _| inline_map_candidate_is_scalar(function, *definition));
    candidates
}

fn inline_map_candidate_is_scalar(function: &TypedFunctionIr, definition: DefinitionId) -> bool {
    let mut writes = 0_u8;
    for statement in &function.body.statements {
        match statement {
            TypedStatementIr::Expression(expression)
                if inline_map_set_expression(expression, definition) =>
            {
                writes = writes.saturating_add(1);
                if writes > 1 {
                    return false;
                }
            }
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if value
                    .as_ref()
                    .is_some_and(|value| !inline_map_read_expression_allowed(value, definition))
                {
                    return false;
                }
            }
            TypedStatementIr::Expression(expression) => {
                if !inline_map_read_expression_allowed(expression, definition) {
                    return false;
                }
            }
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
                if place_references_definition(target, definition)
                    || !inline_map_read_expression_allowed(value, definition)
                {
                    return false;
                }
            }
            TypedStatementIr::If { .. }
            | TypedStatementIr::While { .. }
            | TypedStatementIr::StaticRangeFor { .. }
            | TypedStatementIr::DynamicRangeFor { .. }
            | TypedStatementIr::CollectionFor { .. }
            | TypedStatementIr::Defer { .. } => {
                if statement_references_definition(statement, definition) {
                    return false;
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    function
        .body
        .tail
        .as_ref()
        .is_none_or(|tail| inline_map_read_expression_allowed(tail, definition))
}

fn inline_map_set_expression(expression: &TypedExpressionIr, definition: DefinitionId) -> bool {
    let TypedExpressionKind::BuiltinCall {
        operation: BuiltinOperationIr::MapSet,
        arguments,
        ..
    } = &expression.kind
    else {
        return false;
    };
    arguments.len() == 3
        && matches!(
            arguments[0].kind,
            TypedExpressionKind::Reference(receiver) if receiver == definition
        )
        && matches!(
            arguments[1].kind,
            TypedExpressionKind::Literal(IrLiteral::I32(_))
        )
        && !expression_references_definition(&arguments[2], definition)
}

fn inline_map_read_expression_allowed(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
) -> bool {
    match &expression.kind {
        TypedExpressionKind::Reference(candidate) => *candidate != definition,
        TypedExpressionKind::Match { value, arms }
            if inline_map_get_expression(value, definition) =>
        {
            arms.iter().all(|arm| {
                matches!(
                    arm.pattern.kind,
                    TypedPatternKind::BuiltinVariant { .. } | TypedPatternKind::Wildcard
                ) && inline_map_read_expression_allowed(&arm.value, definition)
            })
        }
        TypedExpressionKind::BuiltinCall {
            operation,
            arguments,
            ..
        } if matches!(
            arguments.first().map(|argument| &argument.kind),
            Some(TypedExpressionKind::Reference(receiver)) if *receiver == definition
        ) =>
        {
            *operation == BuiltinOperationIr::MapGet
                && arguments.len() == 2
                && matches!(
                    arguments[1].kind,
                    TypedExpressionKind::Literal(IrLiteral::I32(_))
                )
        }
        _ => {
            let mut children = Vec::new();
            collect_expression_children(expression, &mut children);
            children
                .into_iter()
                .all(|child| inline_map_read_expression_allowed(child, definition))
        }
    }
}

fn inline_map_get_expression(expression: &TypedExpressionIr, definition: DefinitionId) -> bool {
    matches!(
        &expression.kind,
        TypedExpressionKind::BuiltinCall {
            operation: BuiltinOperationIr::MapGet,
            arguments,
            ..
        } if arguments.len() == 2
            && matches!(
                arguments[0].kind,
                TypedExpressionKind::Reference(receiver) if receiver == definition
            )
            && matches!(
                arguments[1].kind,
                TypedExpressionKind::Literal(IrLiteral::I32(_))
            )
    )
}

fn expression_references_definition(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
) -> bool {
    if matches!(
        expression.kind,
        TypedExpressionKind::Reference(candidate) if candidate == definition
    ) {
        return true;
    }
    let mut children = Vec::new();
    collect_expression_children(expression, &mut children);
    children
        .into_iter()
        .any(|child| expression_references_definition(child, definition))
}

fn place_references_definition(place: &TypedPlaceIr, definition: DefinitionId) -> bool {
    match place {
        TypedPlaceIr::Definition(candidate) => *candidate == definition,
        TypedPlaceIr::Field { base, .. } => place_references_definition(base, definition),
        TypedPlaceIr::ClassField { object, .. } | TypedPlaceIr::StateField { base: object, .. } => {
            expression_references_definition(object, definition)
        }
        TypedPlaceIr::Index { base, index } => {
            expression_references_definition(base, definition)
                || expression_references_definition(index, definition)
        }
    }
}

fn statement_references_definition(statement: &TypedStatementIr, definition: DefinitionId) -> bool {
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => value
            .as_ref()
            .is_some_and(|value| expression_references_definition(value, definition)),
        TypedStatementIr::Assign { target, value }
        | TypedStatementIr::CompoundAssign { target, value, .. } => {
            place_references_definition(target, definition)
                || expression_references_definition(value, definition)
        }
        TypedStatementIr::Expression(expression) => {
            expression_references_definition(expression, definition)
        }
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            expression_references_definition(condition, definition)
                || block_references_definition(then_block, definition)
                || else_block
                    .as_ref()
                    .is_some_and(|block| block_references_definition(block, definition))
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            expression_references_definition(condition, definition)
                || block_references_definition(body, definition)
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        }
        | TypedStatementIr::DynamicRangeFor {
            start, end, body, ..
        } => {
            expression_references_definition(start, definition)
                || expression_references_definition(end, definition)
                || block_references_definition(body, definition)
        }
        TypedStatementIr::CollectionFor { iterable, body, .. } => {
            expression_references_definition(iterable, definition)
                || block_references_definition(body, definition)
        }
        TypedStatementIr::Defer { captures, .. } => captures
            .iter()
            .any(|capture| expression_references_definition(capture, definition)),
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {
            false
        }
    }
}

fn block_references_definition(block: &TypedBlockIr, definition: DefinitionId) -> bool {
    block
        .statements
        .iter()
        .any(|statement| statement_references_definition(statement, definition))
        || block
            .tail
            .as_ref()
            .is_some_and(|tail| expression_references_definition(tail, definition))
}

fn inline_class_candidates(function: &TypedFunctionIr) -> BTreeMap<DefinitionId, DefinitionId> {
    let mut candidates = BTreeMap::new();
    collect_inline_class_candidates(&function.body, &mut candidates);
    candidates.retain(|definition, _| inline_class_block_is_scalar(&function.body, *definition));
    candidates
}

fn collect_inline_class_candidates(
    block: &TypedBlockIr,
    candidates: &mut BTreeMap<DefinitionId, DefinitionId>,
) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let {
                definition,
                value: Some(value),
                ..
            } => {
                if let TypedExpressionKind::ClassConstruct {
                    definition: owner,
                    update: None,
                    ..
                } = &value.kind
                {
                    candidates.insert(*definition, *owner);
                }
            }
            TypedStatementIr::If {
                then_block,
                else_block,
                ..
            } => {
                collect_inline_class_candidates(then_block, candidates);
                if let Some(else_block) = else_block {
                    collect_inline_class_candidates(else_block, candidates);
                }
            }
            TypedStatementIr::While { body, .. }
            | TypedStatementIr::StaticRangeFor { body, .. }
            | TypedStatementIr::DynamicRangeFor { body, .. }
            | TypedStatementIr::CollectionFor { body, .. } => {
                collect_inline_class_candidates(body, candidates);
            }
            _ => {}
        }
    }
}

fn inline_class_block_is_scalar(block: &TypedBlockIr, definition: DefinitionId) -> bool {
    block
        .statements
        .iter()
        .all(|statement| inline_class_statement_is_scalar(statement, definition))
        && block
            .tail
            .as_ref()
            .is_none_or(|tail| inline_class_expression_is_scalar(tail, definition))
}

fn inline_class_statement_is_scalar(
    statement: &TypedStatementIr,
    definition: DefinitionId,
) -> bool {
    match statement {
        TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => value
            .as_ref()
            .is_none_or(|value| inline_class_expression_is_scalar(value, definition)),
        TypedStatementIr::Assign { target, value }
        | TypedStatementIr::CompoundAssign { target, value, .. } => {
            inline_class_place_is_scalar(target, definition)
                && inline_class_expression_is_scalar(value, definition)
        }
        TypedStatementIr::Expression(expression) => {
            inline_class_expression_is_scalar(expression, definition)
        }
        TypedStatementIr::If {
            condition,
            then_block,
            else_block,
        } => {
            inline_class_expression_is_scalar(condition, definition)
                && inline_class_block_is_scalar(then_block, definition)
                && else_block
                    .as_ref()
                    .is_none_or(|block| inline_class_block_is_scalar(block, definition))
        }
        TypedStatementIr::While {
            condition, body, ..
        } => {
            inline_class_expression_is_scalar(condition, definition)
                && inline_class_block_is_scalar(body, definition)
        }
        TypedStatementIr::StaticRangeFor {
            start, end, body, ..
        }
        | TypedStatementIr::DynamicRangeFor {
            start, end, body, ..
        } => {
            inline_class_expression_is_scalar(start, definition)
                && inline_class_expression_is_scalar(end, definition)
                && inline_class_block_is_scalar(body, definition)
        }
        TypedStatementIr::CollectionFor { iterable, body, .. } => {
            inline_class_expression_is_scalar(iterable, definition)
                && inline_class_block_is_scalar(body, definition)
        }
        TypedStatementIr::Defer { captures, .. } => captures
            .iter()
            .all(|capture| inline_class_expression_is_scalar(capture, definition)),
        TypedStatementIr::Break | TypedStatementIr::Continue | TypedStatementIr::Yield { .. } => {
            true
        }
    }
}

fn inline_class_place_is_scalar(place: &TypedPlaceIr, definition: DefinitionId) -> bool {
    match place {
        TypedPlaceIr::Definition(candidate) => *candidate != definition,
        TypedPlaceIr::Field { base, .. } => inline_class_place_is_scalar(base, definition),
        TypedPlaceIr::ClassField { object, .. } => {
            matches!(
                object.kind,
                TypedExpressionKind::Reference(candidate) if candidate == definition
            ) || inline_class_expression_is_scalar(object, definition)
        }
        TypedPlaceIr::StateField { base, .. } => {
            inline_class_expression_is_scalar(base, definition)
        }
        TypedPlaceIr::Index { base, index } => {
            inline_class_expression_is_scalar(base, definition)
                && inline_class_expression_is_scalar(index, definition)
        }
    }
}

fn inline_class_expression_is_scalar(
    expression: &TypedExpressionIr,
    definition: DefinitionId,
) -> bool {
    match &expression.kind {
        TypedExpressionKind::Reference(candidate) => *candidate != definition,
        TypedExpressionKind::Field { base, .. }
            if matches!(
                base.kind,
                TypedExpressionKind::Reference(candidate) if candidate == definition
            ) =>
        {
            true
        }
        _ => {
            let mut children = Vec::new();
            collect_expression_children(expression, &mut children);
            children
                .into_iter()
                .all(|child| inline_class_expression_is_scalar(child, definition))
        }
    }
}

fn allocate_inline_class_states(
    layouts: &TypedLayoutContext,
    function: &TypedFunctionIr,
    function_span: SourceSpan,
    register_types: &mut Vec<Option<ValueType>>,
) -> Result<BTreeMap<DefinitionId, (DefinitionId, u16)>, CompileError> {
    let mut states = BTreeMap::new();
    for (definition, owner) in inline_class_candidates(function) {
        let Some(layout) = layouts.aggregates.get(&owner) else {
            continue;
        };
        if layout.kind != TypedAggregateKind::Class {
            continue;
        }
        let fields_base = u16::try_from(register_types.len())
            .map_err(|_| CompileError::too_many_registers(function_span))?;
        for field in &layout.fields {
            register_types.push(Some(field.ty));
            let slots = layouts.physical_slots(field.ty, function_span)?;
            register_types.extend((1..slots).map(|_| None));
        }
        states.insert(definition, (owner, fields_base));
    }
    Ok(states)
}

/// Read-only child enumeration shared by the specialized escape scans.
fn collect_expression_children<'expr>(
    expression: &'expr TypedExpressionIr,
    children: &mut Vec<&'expr TypedExpressionIr>,
) {
    match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. } => children.push(operand),
        TypedExpressionKind::Binary { left, right, .. } => {
            children.push(left);
            children.push(right);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. } => children.extend(arguments.iter()),
        TypedExpressionKind::Construct { fields, .. } => {
            children.extend(fields.iter().map(|(_, field)| field));
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            children.extend(fields.iter().map(|(_, field)| field));
            if let Some(update) = update {
                children.push(update);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                children.push(payload);
            }
        }
        TypedExpressionKind::Field { base, .. } | TypedExpressionKind::StateField { base, .. } => {
            children.push(base);
        }
        TypedExpressionKind::Index { base, index } => {
            children.push(base);
            children.push(index);
        }
        TypedExpressionKind::Array(items)
        | TypedExpressionKind::Tuple(items)
        | TypedExpressionKind::StringInterpolation(items) => children.extend(items.iter()),
        TypedExpressionKind::Match { value, arms } => {
            children.push(value);
            children.extend(arms.iter().map(|arm| &arm.value));
        }
        TypedExpressionKind::Try(inner) | TypedExpressionKind::Await(inner) => children.push(inner),
        TypedExpressionKind::Update { base, fields } => {
            children.push(base);
            children.extend(fields.iter().map(|(_, field)| field));
        }
        TypedExpressionKind::Migration(intrinsic) => match intrinsic {
            MigrationIntrinsicIr::OldFieldGet { object, .. } => children.push(object),
            MigrationIntrinsicIr::NewSet { object, value, .. } => {
                children.push(object);
                children.push(value);
            }
            MigrationIntrinsicIr::Replace { target, .. } => children.push(target),
            MigrationIntrinsicIr::OldGet { .. }
            | MigrationIntrinsicIr::NewCreate { .. }
            | MigrationIntrinsicIr::Preserve { .. }
            | MigrationIntrinsicIr::Delete { .. }
            | MigrationIntrinsicIr::Finish => {}
        },
    }
}

struct AllocatedFunctionBindings {
    locals: BTreeMap<DefinitionId, u16>,
    register_types: Vec<Option<ValueType>>,
    parameter_slots: usize,
}

fn allocate_function_bindings(
    package: &TypedPackageIr,
    layouts: &TypedLayoutContext,
    function: &TypedFunctionIr,
    span: SourceSpan,
) -> Result<AllocatedFunctionBindings, CompileError> {
    let mut locals = BTreeMap::new();
    let mut register_types = Vec::new();
    for definition in &function.parameters {
        let metadata = package
            .definition(*definition)
            .expect("TypedPackageIr validates local IDs");
        let ty = lower_type(package, &metadata.ty, span)?;
        let register = u16::try_from(register_types.len())
            .map_err(|_| CompileError::too_many_registers(span))?;
        locals.insert(*definition, register);
        register_types.push(Some(ty));
        let physical_slots = layouts.physical_slots(ty, span)?;
        register_types.extend((1..physical_slots).map(|_| None));
    }
    let parameter_slots = register_types.len();
    for definition in &function.locals {
        let metadata = package
            .definition(*definition)
            .expect("TypedPackageIr validates local IDs");
        let ty = lower_type(package, &metadata.ty, span)?;
        let register = u16::try_from(register_types.len())
            .map_err(|_| CompileError::too_many_registers(span))?;
        locals.insert(*definition, register);
        register_types.push(Some(ty));
        let physical_slots = layouts.physical_slots(ty, span)?;
        register_types.extend((1..physical_slots).map(|_| None));
    }
    Ok(AllocatedFunctionBindings {
        locals,
        register_types,
        parameter_slots,
    })
}

impl<'a> FunctionEmitter<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        package: &'a TypedPackageIr,
        function_indices: &'a BTreeMap<DefinitionId, u32>,
        host_imports: &'a BTreeMap<DefinitionId, u32>,
        standard_functions: &'a BTreeMap<DefinitionId, nexa_stdlib::Intrinsic>,
        layouts: &'a TypedLayoutContext,
        constants: &'a BTreeMap<DefinitionId, &'a TypedExpressionIr>,
        files: &'a BTreeMap<SourceKey, FileId>,
        string_indices: &'a BTreeMap<String, u32>,
        function: &'a TypedFunctionIr,
        function_span: SourceSpan,
        optimize: bool,
    ) -> Result<Self, CompileError> {
        let AllocatedFunctionBindings {
            locals,
            mut register_types,
            parameter_slots,
        } = allocate_function_bindings(package, layouts, function, function_span)?;
        let mut inline_maps = BTreeMap::new();
        let mut inline_arrays = BTreeMap::new();
        let mut inline_classes = BTreeMap::new();
        if optimize {
            for (definition, plan) in inline_map_candidates(function) {
                let value_register = u16::try_from(register_types.len())
                    .map_err(|_| CompileError::too_many_registers(function_span))?;
                let value_type = lower_type(package, &plan.value_type, function_span)?;
                let value_slots = layouts.physical_slots(value_type, function_span)?;
                register_types.push(Some(value_type));
                register_types.extend((1..value_slots).map(|_| None));
                inline_maps.insert(
                    definition,
                    InlineMapState {
                        value_register,
                        entry_key: None,
                    },
                );
            }
            inline_arrays = allocate_inline_array_states(
                package,
                layouts,
                function,
                function_span,
                &mut register_types,
            )?;
            inline_classes = allocate_inline_class_states(
                layouts,
                function,
                function_span,
                &mut register_types,
            )?;
        }
        Ok(Self {
            package,
            function_indices,
            host_imports,
            standard_functions,
            layouts,
            constants,
            files,
            string_indices,
            locals,
            inline_maps,
            inline_arrays,
            inline_classes,
            optimize,
            register_types,
            parameter_slots,
            function_effect: function.effect,
            function_return_type: &function.return_type,
            code: Vec::new(),
            spans: Vec::new(),
            loop_bounds: Vec::new(),
            loop_stack: Vec::new(),
            function_span,
            constant_stack: BTreeSet::new(),
        })
    }

    fn emit_block(&mut self, block: &TypedBlockIr) -> Result<(), CompileError> {
        for statement in &block.statements {
            self.emit_statement(statement)?;
        }
        if let Some(tail) = &block.tail {
            if tail.ty != *self.function_return_type {
                return Err(CompileError::type_mismatch(
                    None,
                    None,
                    self.span(&tail.span)?,
                ));
            }
            let source = self.allocate_expression(tail)?;
            self.emit_expression(tail, source)?;
            if tail.ty == IrType::Unit {
                self.push(
                    if self.function_effect == IrEffect::Cleanup {
                        Instruction::CleanupReturn
                    } else {
                        Instruction::ReturnVoid
                    },
                    self.span(&tail.span)?,
                );
            } else {
                self.push(Instruction::Return { source }, self.span(&tail.span)?);
            }
        }
        Ok(())
    }

    fn emit_nested_block(&mut self, block: &TypedBlockIr) -> Result<(), CompileError> {
        for statement in &block.statements {
            self.emit_statement(statement)?;
        }
        if let Some(tail) = &block.tail {
            let destination = self.allocate_expression(tail)?;
            self.emit_expression(tail, destination)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn emit_statement(&mut self, statement: &TypedStatementIr) -> Result<(), CompileError> {
        match statement {
            TypedStatementIr::Let {
                definition, value, ..
            } => {
                if let Some(value) = value {
                    if self.inline_arrays.contains_key(definition) {
                        if !matches!(
                            value.kind,
                            TypedExpressionKind::BuiltinCall {
                                operation: BuiltinOperationIr::ArrayNew,
                                ..
                            }
                        ) {
                            return Err(CompileError::type_mismatch(
                                None,
                                None,
                                self.span(&value.span)?,
                            ));
                        }
                        // The bounded local array is represented exclusively
                        // by its element registers. Candidate analysis rejects
                        // every use that could observe an array identity.
                        return Ok(());
                    }
                    if self.inline_maps.contains_key(definition) {
                        if !matches!(
                            value.kind,
                            TypedExpressionKind::BuiltinCall {
                                operation: BuiltinOperationIr::MapNew,
                                ..
                            }
                        ) {
                            return Err(CompileError::type_mismatch(
                                None,
                                None,
                                self.span(&value.span)?,
                            ));
                        }
                        // The map has no physical representation until an
                        // escaping use (which the candidate scan rejects).
                        return Ok(());
                    }
                    if let Some((owner, fields_base)) = self.inline_classes.get(definition).copied()
                    {
                        let TypedExpressionKind::ClassConstruct {
                            definition: constructed,
                            fields,
                            update: None,
                        } = &value.kind
                        else {
                            return Err(CompileError::type_mismatch(
                                None,
                                None,
                                self.span(&value.span)?,
                            ));
                        };
                        if *constructed != owner {
                            return Err(CompileError::type_mismatch(
                                None,
                                None,
                                self.span(&value.span)?,
                            ));
                        }
                        self.emit_inline_struct_fields(owner, fields_base, fields, value)?;
                        return Ok(());
                    }
                    let destination = self.local(*definition)?;
                    self.emit_expression(value, destination)?;
                }
            }
            TypedStatementIr::Assign { target, value } => match target {
                TypedPlaceIr::ClassField { object, field }
                    if matches!(&object.kind, TypedExpressionKind::Reference(definition)
                        if self.inline_classes.contains_key(definition)) =>
                {
                    let TypedExpressionKind::Reference(definition) = &object.kind else {
                        unreachable!("guard matched a reference object");
                    };
                    let (owner, fields_base) = self.inline_classes[definition];
                    let (field_owner, field_layout) =
                        self.layouts.fields.get(field).ok_or_else(|| {
                            CompileError::unknown_name(
                                self.definition_name(*field),
                                self.function_span,
                            )
                        })?;
                    if *field_owner != owner
                        || self.layouts.aggregates[&owner].kind != TypedAggregateKind::Class
                        || !field_layout.mutable
                    {
                        return Err(CompileError::type_mismatch(
                            None,
                            None,
                            self.span(&value.span)?,
                        ));
                    }
                    let register =
                        self.inline_field_register(owner, fields_base, *field, self.function_span)?;
                    self.emit_expression(value, register)?;
                }
                TypedPlaceIr::Definition(definition) => {
                    let destination = self.local(*definition)?;
                    let source = self.allocate_expression(value)?;
                    self.emit_expression(value, source)?;
                    let span = self.span(&value.span)?;
                    let ty = lower_type(self.package, &value.ty, span)?;
                    let instruction = self.copy_value_instruction(ty, destination, source, span)?;
                    self.push(instruction, span);
                }
                TypedPlaceIr::Field { .. } => {
                    self.emit_assign_struct_place(target, value)?;
                }
                TypedPlaceIr::ClassField { object, field }
                | TypedPlaceIr::StateField {
                    base: object,
                    field,
                } => {
                    let repl_state_initialization =
                        matches!(target, TypedPlaceIr::StateField { .. });
                    let base_register = self.allocate_expression(object)?;
                    self.emit_expression(object, base_register)?;
                    let value_register = self.allocate_expression(value)?;
                    self.emit_expression(value, value_register)?;
                    let (owner, field) = self.layouts.fields.get(field).ok_or_else(|| {
                        CompileError::unknown_name(self.definition_name(*field), self.function_span)
                    })?;
                    let aggregate = &self.layouts.aggregates[owner];
                    // An immutable REPL binding is initialized exactly once while its append-only
                    // environment slot is staged. `validate_repl_new_field_writes` proves that
                    // StateField authority before this output can be returned. Ordinary Class
                    // assignments still require a mutable field.
                    if aggregate.kind != TypedAggregateKind::Class
                        || (!field.mutable && !repl_state_initialization)
                    {
                        return Err(CompileError::type_mismatch(
                            None,
                            None,
                            self.span(&object.span)?,
                        ));
                    }
                    self.push(
                        Instruction::ClassSet {
                            source: base_register,
                            field: field.stable_id,
                            value: value_register,
                        },
                        self.span(&value.span)?,
                    );
                }
                TypedPlaceIr::Index { base, index } => {
                    let base_register = self.allocate_expression(base)?;
                    let index_register = self.allocate_expression(index)?;
                    let value_register = self.allocate_expression(value)?;
                    self.emit_expression(base, base_register)?;
                    self.emit_expression(index, index_register)?;
                    self.emit_expression(value, value_register)?;
                    let instruction = match &base.ty {
                        IrType::Array(_) => Instruction::ArraySet {
                            source: base_register,
                            index: index_register,
                            value: value_register,
                        },
                        IrType::Map(_, _) => Instruction::MapSet {
                            source: base_register,
                            key: index_register,
                            value: value_register,
                        },
                        IrType::Buffer(_) => Instruction::BufferSet {
                            source: base_register,
                            index: index_register,
                            value: value_register,
                        },
                        _ => {
                            return Err(CompileError::type_mismatch(
                                None,
                                None,
                                self.span(&base.span)?,
                            ));
                        }
                    };
                    self.push(instruction, self.span(&value.span)?);
                }
            },
            TypedStatementIr::CompoundAssign {
                target,
                operator,
                value,
            } => match target {
                TypedPlaceIr::ClassField { object, field }
                    if matches!(&object.kind, TypedExpressionKind::Reference(definition)
                        if self.inline_classes.contains_key(definition)) =>
                {
                    let TypedExpressionKind::Reference(definition) = &object.kind else {
                        unreachable!("guard matched a reference object");
                    };
                    let (owner, fields_base) = self.inline_classes[definition];
                    let (field_owner, field_layout) =
                        self.layouts.fields.get(field).cloned().ok_or_else(|| {
                            CompileError::unknown_name(
                                self.definition_name(*field),
                                self.function_span,
                            )
                        })?;
                    if field_owner != owner
                        || self.layouts.aggregates[&owner].kind != TypedAggregateKind::Class
                        || !field_layout.mutable
                    {
                        return Err(CompileError::type_mismatch(
                            None,
                            None,
                            self.span(&value.span)?,
                        ));
                    }
                    let destination =
                        self.inline_field_register(owner, fields_base, *field, self.function_span)?;
                    let source = self.allocate_expression(value)?;
                    self.emit_expression(value, source)?;
                    let span = self.span(&value.span)?;
                    let result = self.allocate(field_layout.ty)?;
                    self.emit_compound_binary(
                        *operator,
                        &value.ty,
                        destination,
                        source,
                        result,
                        span,
                    )?;
                    self.push(
                        self.copy_value_instruction(field_layout.ty, destination, result, span)?,
                        span,
                    );
                }
                TypedPlaceIr::Definition(definition) => {
                    let destination = self.local(*definition)?;
                    let source = self.allocate_expression(value)?;
                    self.emit_expression(value, source)?;
                    let span = self.span(&value.span)?;
                    let result = self.allocate(lower_type(self.package, &value.ty, span)?)?;
                    self.emit_compound_binary(
                        *operator,
                        &value.ty,
                        destination,
                        source,
                        result,
                        span,
                    )?;
                    let ty = lower_type(self.package, &value.ty, span)?;
                    self.push(
                        self.copy_value_instruction(ty, destination, result, span)?,
                        span,
                    );
                }
                TypedPlaceIr::Field { .. }
                | TypedPlaceIr::ClassField { .. }
                | TypedPlaceIr::StateField { .. }
                | TypedPlaceIr::Index { .. } => {
                    self.emit_compound_assign_place(target, *operator, value)?;
                }
            },
            TypedStatementIr::Expression(expression) => {
                let destination = self.allocate_expression(expression)?;
                self.emit_expression(expression, destination)?;
            }
            TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    if value.ty != *self.function_return_type {
                        return Err(CompileError::type_mismatch(
                            None,
                            None,
                            self.span(&value.span)?,
                        ));
                    }
                    let source = self.allocate_expression(value)?;
                    self.emit_expression(value, source)?;
                    self.push(
                        if value.ty == IrType::Unit {
                            if self.function_effect == IrEffect::Cleanup {
                                Instruction::CleanupReturn
                            } else {
                                Instruction::ReturnVoid
                            }
                        } else {
                            Instruction::Return { source }
                        },
                        self.span(&value.span)?,
                    );
                } else {
                    if *self.function_return_type != IrType::Unit {
                        return Err(CompileError::type_mismatch(None, None, self.function_span));
                    }
                    self.push(
                        if self.function_effect == IrEffect::Cleanup {
                            Instruction::CleanupReturn
                        } else {
                            Instruction::ReturnVoid
                        },
                        self.function_span,
                    );
                }
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                let condition_register = self.allocate_expression(condition)?;
                self.emit_expression(condition, condition_register)?;
                let branch = self.push(
                    Instruction::JumpIfFalse {
                        condition: condition_register,
                        target: 0,
                    },
                    self.span(&condition.span)?,
                );
                self.emit_nested_block(then_block)?;
                if let Some(else_block) = else_block {
                    let skip_else = self.push(Instruction::Jump { target: 0 }, self.function_span);
                    self.patch_target(branch, self.position())?;
                    self.emit_nested_block(else_block)?;
                    self.patch_target(skip_else, self.position())?;
                } else {
                    self.patch_target(branch, self.position())?;
                }
            }
            TypedStatementIr::While {
                condition,
                body,
                max_iterations,
            } => {
                let loop_start = self.position();
                let condition_register = self.allocate_expression(condition)?;
                self.emit_expression(condition, condition_register)?;
                let exit = self.push(
                    Instruction::JumpIfFalse {
                        condition: condition_register,
                        target: 0,
                    },
                    self.span(&condition.span)?,
                );
                self.loop_stack.push(LoopPatch {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                self.emit_nested_block(body)?;
                let continue_target = self.position();
                let back_edge =
                    self.push(Instruction::Jump { target: loop_start }, self.function_span);
                self.loop_bounds.push(LoopBound {
                    back_edge: u32::try_from(back_edge)
                        .map_err(|_| CompileError::too_many_registers(self.function_span))?,
                    max_iterations: *max_iterations,
                });
                let loop_end = self.position();
                self.patch_target(exit, loop_end)?;
                let patches = self.loop_stack.pop().expect("loop stack was pushed");
                for patch in patches.breaks {
                    self.patch_target(patch, loop_end)?;
                }
                for patch in patches.continues {
                    self.patch_target(patch, continue_target)?;
                }
            }
            TypedStatementIr::StaticRangeFor {
                binding,
                start,
                end,
                body,
                max_iterations,
            } => {
                let exact_iterations = validate_static_range_bound(
                    start,
                    end,
                    *max_iterations,
                    self.constants,
                    self.span(&start.span)?,
                )?;
                if exact_iterations == 0 {
                    return Ok(());
                }
                let binding_register = self.local(*binding)?;
                self.emit_expression(start, binding_register)?;
                let end_register = self.allocate_expression(end)?;
                self.emit_expression(end, end_register)?;
                let loop_start = self.position();
                let condition = self.allocate(ValueType::Bool)?;
                self.push(
                    Instruction::CompareLtI32 {
                        dst: condition,
                        lhs: binding_register,
                        rhs: end_register,
                    },
                    self.function_span,
                );
                let exit = self.push(
                    Instruction::JumpIfFalse {
                        condition,
                        target: 0,
                    },
                    self.function_span,
                );
                self.loop_stack.push(LoopPatch {
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                self.emit_nested_block(body)?;
                let continue_target = self.position();
                let one = self.allocate(ValueType::I32)?;
                self.push(
                    Instruction::LoadI32 { dst: one, value: 1 },
                    self.function_span,
                );
                self.push(
                    Instruction::Add {
                        dst: binding_register,
                        lhs: binding_register,
                        rhs: one,
                    },
                    self.function_span,
                );
                let back_edge =
                    self.push(Instruction::Jump { target: loop_start }, self.function_span);
                self.loop_bounds.push(LoopBound {
                    back_edge: u32::try_from(back_edge)
                        .map_err(|_| CompileError::too_many_registers(self.function_span))?,
                    max_iterations: exact_iterations,
                });
                let loop_end = self.position();
                self.patch_target(exit, loop_end)?;
                let patches = self.loop_stack.pop().expect("loop stack was pushed");
                for patch in patches.breaks {
                    self.patch_target(patch, loop_end)?;
                }
                for patch in patches.continues {
                    self.patch_target(patch, continue_target)?;
                }
            }
            TypedStatementIr::DynamicRangeFor {
                binding,
                start,
                end,
                body,
                max_iterations,
            } => {
                let start_register = self.allocate_expression(start)?;
                self.emit_expression(start, start_register)?;
                let end_register = self.allocate_expression(end)?;
                self.emit_expression(end, end_register)?;
                let binding_register = self.local(*binding)?;
                let first_dst = self.allocate(ValueType::I32)?;
                let has_value_dst = self.allocate(ValueType::Bool)?;
                let slot = self.allocate(ValueType::I32)?;
                let epoch = self.allocate(ValueType::I64)?;
                self.emit_iteration_loop(
                    CollectionIteratorKind::Range,
                    IteratorStateRegisters {
                        collection: start_register,
                        phase: end_register,
                        slot,
                        epoch,
                    },
                    has_value_dst,
                    first_dst,
                    None,
                    &[(binding_register, first_dst, ValueType::I32)],
                    body,
                    *max_iterations,
                    self.function_span,
                )?;
            }
            TypedStatementIr::CollectionFor {
                iterable,
                bindings,
                key_type,
                element_type,
                collection,
                body,
                max_iterations,
            } => {
                let iterable_register = self.allocate_expression(iterable)?;
                self.emit_expression(iterable, iterable_register)?;
                let (kind, first_type, second_type) = match collection {
                    CollectionIterationKindIr::Array => (
                        CollectionIteratorKind::Array {
                            element: lower_type(self.package, element_type, self.function_span)?,
                        },
                        lower_type(self.package, element_type, self.function_span)?,
                        None,
                    ),
                    CollectionIterationKindIr::Buffer => (
                        CollectionIteratorKind::Buffer {
                            element: lower_type(self.package, element_type, self.function_span)?,
                        },
                        lower_type(self.package, element_type, self.function_span)?,
                        None,
                    ),
                    CollectionIterationKindIr::Set => (
                        CollectionIteratorKind::Set {
                            element: lower_type(self.package, element_type, self.function_span)?,
                        },
                        lower_type(self.package, element_type, self.function_span)?,
                        None,
                    ),
                    CollectionIterationKindIr::Map => {
                        let key = key_type.as_ref().ok_or_else(|| {
                            CompileError::type_mismatch(None, None, self.function_span)
                        })?;
                        (
                            CollectionIteratorKind::Map {
                                key: lower_type(self.package, key, self.function_span)?,
                                value: lower_type(self.package, element_type, self.function_span)?,
                            },
                            lower_type(self.package, key, self.function_span)?,
                            Some(lower_type(self.package, element_type, self.function_span)?),
                        )
                    }
                };
                let first_dst = self.allocate(first_type)?;
                let second_dst = second_type
                    .map(|second_type| self.allocate(second_type))
                    .transpose()?;
                let mut binding_copies = Vec::with_capacity(bindings.len());
                for (index, binding) in bindings.iter().enumerate() {
                    let binding_register = self.local(*binding)?;
                    let binding_type = self
                        .register_types
                        .get(usize::from(binding_register))
                        .copied()
                        .flatten()
                        .ok_or_else(|| {
                            CompileError::unknown_name(
                                self.definition_name(*binding),
                                self.function_span,
                            )
                        })?;
                    let source = if index == 0 {
                        first_dst
                    } else {
                        second_dst.ok_or_else(|| {
                            CompileError::type_mismatch(None, None, self.function_span)
                        })?
                    };
                    binding_copies.push((binding_register, source, binding_type));
                }
                let phase = self.allocate(ValueType::I32)?;
                let slot = self.allocate(ValueType::I32)?;
                let epoch = self.allocate(ValueType::I64)?;
                let has_value_dst = self.allocate(ValueType::Bool)?;
                self.emit_iteration_loop(
                    kind,
                    IteratorStateRegisters {
                        collection: iterable_register,
                        phase,
                        slot,
                        epoch,
                    },
                    has_value_dst,
                    first_dst,
                    second_dst,
                    &binding_copies,
                    body,
                    *max_iterations,
                    self.function_span,
                )?;
            }
            TypedStatementIr::Break => {
                let patch = self.push(Instruction::Jump { target: 0 }, self.function_span);
                let Some(loop_patch) = self.loop_stack.last_mut() else {
                    return Err(CompileError::invalid_effect(self.function_span));
                };
                loop_patch.breaks.push(patch);
            }
            TypedStatementIr::Continue => {
                let patch = self.push(Instruction::Jump { target: 0 }, self.function_span);
                let Some(loop_patch) = self.loop_stack.last_mut() else {
                    return Err(CompileError::invalid_effect(self.function_span));
                };
                loop_patch.continues.push(patch);
            }
            TypedStatementIr::Defer { cleanup, captures } => {
                let function = *self.function_indices.get(cleanup).ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*cleanup), self.function_span)
                })?;
                let (args_base, capture_registers, capture_slots) =
                    self.reserve_physical_arguments(captures)?;
                if capture_slots > 8 {
                    return Err(CompileError::defer_capture_limit(self.function_span));
                }
                for (capture, register) in captures.iter().zip(capture_registers) {
                    self.emit_expression(capture, register)?;
                }
                self.push(
                    Instruction::DeferPush {
                        function,
                        args_base,
                        args_count: capture_slots,
                    },
                    self.function_span,
                );
            }
            TypedStatementIr::Yield { span } => {
                self.push(Instruction::Yield, self.span(span)?);
            }
        }
        Ok(())
    }

    /// Shared `IterNew`/`IterNext` loop shape for `DynamicRangeFor` and
    /// `CollectionFor`. The collection register (or range start) and the
    /// caller-set end bound are evaluated exactly once before `IterNew`;
    /// every iteration copies the explicit `first`/`second` payload registers
    /// into the loop bindings before the body runs.
    #[allow(clippy::too_many_arguments)]
    fn emit_iteration_loop(
        &mut self,
        kind: CollectionIteratorKind,
        state: IteratorStateRegisters,
        has_value_dst: u16,
        first_dst: u16,
        second_dst: Option<u16>,
        binding_copies: &[(u16, u16, ValueType)],
        body: &TypedBlockIr,
        max_iterations: u32,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        self.push(Instruction::IterNew { kind, state }, span);
        let loop_start = self.position();
        self.push(
            Instruction::IterNext {
                kind,
                state,
                has_value_dst,
                first_dst,
                second_dst,
            },
            span,
        );
        let exit = self.push(
            Instruction::JumpIfFalse {
                condition: has_value_dst,
                target: 0,
            },
            span,
        );
        self.loop_stack.push(LoopPatch {
            breaks: Vec::new(),
            continues: Vec::new(),
        });
        for &(binding, source, ty) in binding_copies {
            self.push(
                self.copy_value_instruction(ty, binding, source, span)?,
                span,
            );
        }
        self.emit_nested_block(body)?;
        let continue_target = self.position();
        let back_edge = self.push(Instruction::Jump { target: loop_start }, span);
        self.loop_bounds.push(LoopBound {
            back_edge: u32::try_from(back_edge)
                .map_err(|_| CompileError::too_many_registers(span))?,
            max_iterations,
        });
        let loop_end = self.position();
        self.patch_target(exit, loop_end)?;
        let patches = self.loop_stack.pop().expect("loop stack was pushed");
        for patch in patches.breaks {
            self.patch_target(patch, loop_end)?;
        }
        for patch in patches.continues {
            self.patch_target(patch, continue_target)?;
        }
        Ok(())
    }

    fn emit_assign_struct_place(
        &mut self,
        place: &TypedPlaceIr,
        value: &TypedExpressionIr,
    ) -> Result<(), CompileError> {
        let mut projection_ids = Vec::new();
        let root_place = flatten_struct_place(place, &mut projection_ids);
        if projection_ids.is_empty() {
            return Err(CompileError::type_mismatch(None, None, self.function_span));
        }

        // Materialize the writable root before the RHS so class receivers and collection indices
        // are evaluated exactly once and in source assignment order.
        let (root, mut current) = self.prepare_struct_place_root(root_place)?;
        let mut parents = Vec::with_capacity(projection_ids.len());
        for (index, field_id) in projection_ids.iter().enumerate() {
            let (owner, field) = self.layouts.fields.get(field_id).cloned().ok_or_else(|| {
                CompileError::unknown_name(self.definition_name(*field_id), self.function_span)
            })?;
            let layout = self
                .layouts
                .aggregates
                .get(&owner)
                .cloned()
                .ok_or_else(|| {
                    CompileError::unknown_type(self.definition_name(owner), self.function_span)
                })?;
            if layout.kind != TypedAggregateKind::Struct
                || self.register_types.get(usize::from(current))
                    != Some(&Some(ValueType::Named(layout.type_id)))
            {
                return Err(CompileError::type_mismatch(None, None, self.function_span));
            }
            parents.push((current, layout.type_id, field.clone()));
            if index.saturating_add(1) < projection_ids.len() {
                let child = self.allocate(field.ty)?;
                self.push(
                    Instruction::StructGet {
                        source: current,
                        field: field.stable_id,
                        dst: child,
                    },
                    self.function_span,
                );
                current = child;
            }
        }

        let leaf = parents
            .last()
            .map(|(_, _, field)| field.ty)
            .ok_or_else(|| CompileError::type_mismatch(None, None, self.function_span))?;
        let value_register = self.allocate_expression(value)?;
        if self.register_types.get(usize::from(value_register)) != Some(&Some(leaf)) {
            return Err(CompileError::type_mismatch(
                Some(leaf),
                self.register_types
                    .get(usize::from(value_register))
                    .copied()
                    .flatten(),
                self.span(&value.span)?,
            ));
        }
        self.emit_expression(value, value_register)?;

        let mut updated = value_register;
        for (parent, type_id, field) in parents.into_iter().rev() {
            let rebuilt = self.allocate(ValueType::Named(type_id))?;
            self.push(
                Instruction::StructWith {
                    source: parent,
                    field: field.stable_id,
                    value: updated,
                    dst: rebuilt,
                },
                self.function_span,
            );
            updated = rebuilt;
        }
        if self.register_types.get(usize::from(updated)) != Some(&Some(root.ty())) {
            return Err(CompileError::type_mismatch(None, None, self.function_span));
        }
        self.store_struct_place_root(root, updated)?;
        Ok(())
    }

    /// Instruction selection for one `target op= value` operation. Mirrors the
    /// numeric/String rules of [`Self::emit_binary`] but operates on already
    /// materialized registers so the target place is evaluated exactly once.
    fn emit_compound_binary(
        &mut self,
        operator: BinaryOperator,
        ty: &IrType,
        lhs: u16,
        rhs: u16,
        dst: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let value_type = lower_type(self.package, ty, span)?;
        let instruction = match operator {
            BinaryOperator::Add if value_type == ValueType::String => {
                Instruction::StringConcat { dst, lhs, rhs }
            }
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => {
                let numeric = TypedNumericKind::from_ir_type(ty, span)?;
                let operator = TypedNumericOperator::try_from(operator)
                    .map_err(|()| CompileError::type_mismatch(None, None, span))?;
                numeric.binary(operator, dst, lhs, rhs)
            }
            _ => return Err(CompileError::type_mismatch(None, None, span)),
        };
        self.push(instruction, span);
        Ok(())
    }

    /// Read-modify-write lowering for `target op= value` on a nontrivial place
    /// (Struct field, Class field, REPL State field, or collection index).
    ///
    /// The root base/index expressions are materialized exactly once (in
    /// source order), then the RHS is evaluated, then the current value is
    /// read, combined, and stored back through the same projection chain.
    #[allow(clippy::too_many_lines)]
    fn emit_compound_assign_place(
        &mut self,
        place: &TypedPlaceIr,
        operator: BinaryOperator,
        value: &TypedExpressionIr,
    ) -> Result<(), CompileError> {
        let mut projection_ids = Vec::new();
        let root_place = flatten_struct_place(place, &mut projection_ids);
        let (root, mut current) = self.prepare_struct_place_root(root_place)?;
        let mut parents = Vec::with_capacity(projection_ids.len());
        for field_id in &projection_ids {
            let (owner, field) = self.layouts.fields.get(field_id).cloned().ok_or_else(|| {
                CompileError::unknown_name(self.definition_name(*field_id), self.function_span)
            })?;
            let layout = self
                .layouts
                .aggregates
                .get(&owner)
                .cloned()
                .ok_or_else(|| {
                    CompileError::unknown_type(self.definition_name(owner), self.function_span)
                })?;
            if layout.kind != TypedAggregateKind::Struct
                || self.register_types.get(usize::from(current))
                    != Some(&Some(ValueType::Named(layout.type_id)))
            {
                return Err(CompileError::type_mismatch(None, None, self.function_span));
            }
            parents.push((current, layout.type_id, field.clone()));
            let child = self.allocate(field.ty)?;
            self.push(
                Instruction::StructGet {
                    source: current,
                    field: field.stable_id,
                    dst: child,
                },
                self.function_span,
            );
            current = child;
        }
        let span = self.span(&value.span)?;
        let leaf = lower_type(self.package, &value.ty, span)?;
        let value_register = self.allocate_expression(value)?;
        if self.register_types.get(usize::from(value_register)) != Some(&Some(leaf)) {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        self.emit_expression(value, value_register)?;
        let result = self.allocate(leaf)?;
        self.emit_compound_binary(operator, &value.ty, current, value_register, result, span)?;
        let mut updated = result;
        for (parent, type_id, field) in parents.into_iter().rev() {
            let rebuilt = self.allocate(ValueType::Named(type_id))?;
            self.push(
                Instruction::StructWith {
                    source: parent,
                    field: field.stable_id,
                    value: updated,
                    dst: rebuilt,
                },
                self.function_span,
            );
            updated = rebuilt;
        }
        if self.register_types.get(usize::from(updated)) != Some(&Some(root.ty())) {
            return Err(CompileError::type_mismatch(None, None, self.function_span));
        }
        self.store_struct_place_root(root, updated)?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn prepare_struct_place_root(
        &mut self,
        place: &TypedPlaceIr,
    ) -> Result<(TypedStructPlaceRoot, u16), CompileError> {
        match place {
            TypedPlaceIr::Definition(definition) => {
                let register = self.local(*definition)?;
                let ty = self
                    .register_types
                    .get(usize::from(register))
                    .copied()
                    .flatten()
                    .ok_or_else(|| CompileError::type_mismatch(None, None, self.function_span))?;
                Ok((TypedStructPlaceRoot::Definition { register, ty }, register))
            }
            TypedPlaceIr::ClassField { object, field }
            | TypedPlaceIr::StateField {
                base: object,
                field,
            } => {
                let object_register = self.allocate_expression(object)?;
                self.emit_expression(object, object_register)?;
                let (owner, field) = self.layouts.fields.get(field).cloned().ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*field), self.function_span)
                })?;
                let aggregate = self.layouts.aggregates.get(&owner).ok_or_else(|| {
                    CompileError::unknown_type(self.definition_name(owner), self.function_span)
                })?;
                if aggregate.kind != TypedAggregateKind::Class
                    || !field.mutable
                    || self.register_types.get(usize::from(object_register))
                        != Some(&Some(ValueType::Named(aggregate.type_id)))
                {
                    return Err(CompileError::type_mismatch(
                        None,
                        None,
                        self.span(&object.span)?,
                    ));
                }
                let value = self.allocate(field.ty)?;
                self.push(
                    Instruction::ClassGet {
                        source: object_register,
                        field: field.stable_id,
                        dst: value,
                    },
                    self.span(&object.span)?,
                );
                Ok((
                    TypedStructPlaceRoot::ClassField {
                        object: object_register,
                        field: field.stable_id,
                        ty: field.ty,
                    },
                    value,
                ))
            }
            TypedPlaceIr::Index { base, index } => {
                let base_register = self.allocate_expression(base)?;
                let index_register = self.allocate_expression(index)?;
                self.emit_expression(base, base_register)?;
                self.emit_expression(index, index_register)?;
                let (root, value) = match &base.ty {
                    IrType::Array(element) => {
                        let ty = lower_type(self.package, element, self.span(&base.span)?)?;
                        let value = self.allocate(ty)?;
                        self.push(
                            Instruction::ArrayGet {
                                source: base_register,
                                index: index_register,
                                dst: value,
                            },
                            self.span(&base.span)?,
                        );
                        (
                            TypedStructPlaceRoot::ArrayIndex {
                                base: base_register,
                                index: index_register,
                                ty,
                            },
                            value,
                        )
                    }
                    IrType::Buffer(element) => {
                        let ty = lower_type(self.package, element, self.span(&base.span)?)?;
                        let value = self.allocate(ty)?;
                        self.push(
                            Instruction::BufferGet {
                                source: base_register,
                                index: index_register,
                                dst: value,
                            },
                            self.span(&base.span)?,
                        );
                        (
                            TypedStructPlaceRoot::BufferIndex {
                                base: base_register,
                                index: index_register,
                                ty,
                            },
                            value,
                        )
                    }
                    IrType::Map(_, element) => {
                        let ty = lower_type(self.package, element, self.span(&base.span)?)?;
                        let option = option_type(ty);
                        let option_register = self.allocate(ValueType::Named(option.type_id))?;
                        self.push(
                            Instruction::MapGet {
                                source: base_register,
                                key: index_register,
                                result_type: option.type_id,
                                dst: option_register,
                            },
                            self.span(&base.span)?,
                        );
                        let value = self.allocate(ty)?;
                        let some = option
                            .variants
                            .get(1)
                            .ok_or_else(|| {
                                CompileError::type_mismatch(None, None, self.function_span)
                            })?
                            .stable_id;
                        self.push(
                            Instruction::EnumPayload {
                                source: option_register,
                                variant: some,
                                dst: value,
                            },
                            self.span(&base.span)?,
                        );
                        (
                            TypedStructPlaceRoot::MapIndex {
                                base: base_register,
                                index: index_register,
                                ty,
                            },
                            value,
                        )
                    }
                    _ => {
                        return Err(CompileError::type_mismatch(
                            None,
                            None,
                            self.span(&base.span)?,
                        ));
                    }
                };
                Ok((root, value))
            }
            TypedPlaceIr::Field { .. } => {
                Err(CompileError::type_mismatch(None, None, self.function_span))
            }
        }
    }

    fn store_struct_place_root(
        &mut self,
        root: TypedStructPlaceRoot,
        source: u16,
    ) -> Result<(), CompileError> {
        let instruction = match root {
            TypedStructPlaceRoot::Definition { register, ty } => {
                self.copy_value_instruction(ty, register, source, self.function_span)?
            }
            TypedStructPlaceRoot::ClassField { object, field, .. } => Instruction::ClassSet {
                source: object,
                field,
                value: source,
            },
            TypedStructPlaceRoot::ArrayIndex { base, index, .. } => Instruction::ArraySet {
                source: base,
                index,
                value: source,
            },
            TypedStructPlaceRoot::BufferIndex { base, index, .. } => Instruction::BufferSet {
                source: base,
                index,
                value: source,
            },
            TypedStructPlaceRoot::MapIndex { base, index, .. } => Instruction::MapSet {
                source: base,
                key: index,
                value: source,
            },
        };
        self.push(instruction, self.function_span);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn emit_expression(
        &mut self,
        expression: &TypedExpressionIr,
        destination: u16,
    ) -> Result<(), CompileError> {
        let span = self.span(&expression.span)?;
        self.validate_unit_expression_contract(expression, span)?;
        match &expression.kind {
            TypedExpressionKind::Literal(literal) => {
                self.emit_literal(literal, destination, span)?;
            }
            TypedExpressionKind::Reference(definition) => {
                if let Some(source) = self.locals.get(definition).copied() {
                    let ty = lower_type(self.package, &expression.ty, span)?;
                    let instruction = self.copy_value_instruction(ty, destination, source, span)?;
                    self.push(instruction, span);
                } else if let Some(constant) = self.constants.get(definition).copied() {
                    if !self.constant_stack.insert(*definition) {
                        return Err(CompileError::invalid_effect(span));
                    }
                    self.emit_expression(constant, destination)?;
                    self.constant_stack.remove(definition);
                } else {
                    return Err(CompileError::unknown_name(
                        self.definition_name(*definition),
                        span,
                    ));
                }
            }
            TypedExpressionKind::PersistentStateGet {
                identity,
                state_type,
            } => {
                if expression.ty != IrType::Named(*state_type)
                    || !is_state_type(self.package, *state_type)
                {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(
                    Instruction::StateCurrentGet {
                        stable_id: *identity,
                        type_id: named_type_id(self.package, *state_type, span)?,
                        dst: destination,
                    },
                    span,
                );
            }
            TypedExpressionKind::Unary { operator, operand } => match operator {
                UnaryOperator::Negate => {
                    let operand_type = TypedNumericKind::from_ir_type(&operand.ty, span)?;
                    let source = self.allocate_expression(operand)?;
                    self.emit_expression(operand, source)?;
                    match operand_type {
                        TypedNumericKind::I32 | TypedNumericKind::I64 => {
                            let zero = self.allocate(operand_type.value_type())?;
                            self.emit_numeric_zero(operand_type, zero, span);
                            self.push(
                                operand_type.binary(
                                    TypedNumericOperator::Subtract,
                                    destination,
                                    zero,
                                    source,
                                ),
                                span,
                            );
                        }
                        TypedNumericKind::F32 => {
                            let negative_one = self.allocate(ValueType::F32)?;
                            self.push(
                                Instruction::LoadF32 {
                                    dst: negative_one,
                                    bits: (-1.0_f32).to_bits(),
                                },
                                span,
                            );
                            self.push(
                                Instruction::MulF32 {
                                    dst: destination,
                                    lhs: source,
                                    rhs: negative_one,
                                },
                                span,
                            );
                        }
                        TypedNumericKind::F64 => {
                            let negative_one = self.allocate(ValueType::F64)?;
                            self.push(
                                Instruction::LoadF64 {
                                    dst: negative_one,
                                    bits: (-1.0_f64).to_bits(),
                                },
                                span,
                            );
                            self.push(
                                Instruction::MulF64 {
                                    dst: destination,
                                    lhs: source,
                                    rhs: negative_one,
                                },
                                span,
                            );
                        }
                    }
                }
                UnaryOperator::Not => {
                    let source = self.allocate_expression(operand)?;
                    self.emit_expression(operand, source)?;
                    let false_register = self.allocate(ValueType::Bool)?;
                    self.push(
                        Instruction::LoadBool {
                            dst: false_register,
                            value: false,
                        },
                        span,
                    );
                    self.push(
                        Instruction::CompareEq {
                            dst: destination,
                            lhs: source,
                            rhs: false_register,
                        },
                        span,
                    );
                }
            },
            TypedExpressionKind::Binary {
                operator,
                left,
                right,
            } => {
                self.emit_binary(*operator, left, right, destination, span)?;
            }
            TypedExpressionKind::Call { callee, arguments } => {
                let function = *self.function_indices.get(callee).ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*callee), span)
                })?;
                let (args_base, argument_registers, argument_slots) =
                    self.reserve_physical_arguments(arguments)?;
                for (argument, register) in arguments.iter().zip(argument_registers) {
                    self.emit_expression(argument, register)?;
                }
                self.push(
                    Instruction::Call {
                        function,
                        args_base,
                        // Bytecode v7 counts the exact packed physical
                        // argument range. Logical arity remains available
                        // from the callee signature and is independently
                        // checked by the verifier.
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                );
            }
            TypedExpressionKind::StandardCall {
                function,
                intrinsic,
                type_arguments,
                arguments,
            } => {
                self.emit_standard_call(
                    *function,
                    *intrinsic,
                    type_arguments,
                    arguments,
                    &expression.ty,
                    destination,
                    span,
                )?;
            }
            TypedExpressionKind::BuiltinCall {
                operation,
                type_arguments,
                arguments,
            } => {
                self.emit_builtin_call(
                    *operation,
                    type_arguments,
                    arguments,
                    &expression.ty,
                    destination,
                    span,
                )?;
            }
            TypedExpressionKind::StringInterpolation(parts) => {
                if self.optimize {
                    let parts = parts.iter().collect::<Vec<_>>();
                    self.emit_string_build(&parts, destination, span)?;
                } else {
                    self.push(
                        Instruction::LoadString {
                            dst: destination,
                            string: self.string_indices[""],
                        },
                        span,
                    );
                    for part in parts {
                        let source = self.allocate_expression(part)?;
                        self.emit_expression(part, source)?;
                        let converted = self.allocate(ValueType::String)?;
                        let part_span = self.span(&part.span)?;
                        let conversion = TypedScalarToString::from_ir_type(&part.ty, part_span)?;
                        self.push(conversion.instruction(converted, source), part_span);
                        self.push(
                            Instruction::StringConcat {
                                dst: destination,
                                lhs: destination,
                                rhs: converted,
                            },
                            span,
                        );
                    }
                }
            }
            TypedExpressionKind::Yield => {
                self.push(Instruction::Yield, span);
            }
            TypedExpressionKind::HostCall {
                contract,
                function,
                arguments,
            } => {
                let import = *self.host_imports.get(function).ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*function), span)
                })?;
                let (host, binding) = self
                    .package
                    .metadata()
                    .host_bindings
                    .iter()
                    .find_map(|host| {
                        host.functions
                            .iter()
                            .find(|binding| binding.definition == *function)
                            .map(|binding| (host, binding))
                    })
                    .ok_or_else(|| {
                        CompileError::unknown_name(self.definition_name(*function), span)
                    })?;
                if host.contract != *contract {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let destination_type = self.register_types[usize::from(destination)];
                match binding.mode {
                    IrHostFunctionMode::Sync => {
                        let expected = (binding.result != IrType::Unit)
                            .then(|| lower_type(self.package, &binding.result, span))
                            .transpose()?;
                        if expected.is_some() && destination_type != expected {
                            return Err(CompileError::type_mismatch(None, None, span));
                        }
                    }
                    IrHostFunctionMode::Request => {
                        let expected = binding
                            .async_result
                            .as_ref()
                            .map(|result| ValueType::Named(result.result_type))
                            .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
                        if destination_type != Some(expected) {
                            return Err(CompileError::type_mismatch(None, None, span));
                        }
                    }
                }
                let (args_base, argument_registers, argument_slots) =
                    self.reserve_physical_arguments(arguments)?;
                for (argument, register) in arguments.iter().zip(argument_registers) {
                    self.emit_expression(argument, register)?;
                }
                self.push(
                    Instruction::HostCall {
                        import,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                );
            }
            TypedExpressionKind::Construct { definition, fields } => {
                self.emit_construct(
                    *definition,
                    fields,
                    TypedAggregateKind::Struct,
                    destination,
                    span,
                )?;
            }
            TypedExpressionKind::ClassConstruct {
                definition,
                fields,
                update,
            } => {
                if let Some(base) = update {
                    if base.ty != IrType::Named(*definition) {
                        return Err(CompileError::type_mismatch(None, None, span));
                    }
                    self.emit_update(base, fields, TypedAggregateKind::Class, destination, span)?;
                } else {
                    self.emit_construct(
                        *definition,
                        fields,
                        TypedAggregateKind::Class,
                        destination,
                        span,
                    )?;
                }
            }
            TypedExpressionKind::EnumConstruct {
                enum_definition,
                variant_definition,
                payload,
            } => {
                self.emit_enum_construct(
                    *enum_definition,
                    *variant_definition,
                    payload.as_deref(),
                    destination,
                    span,
                )?;
            }
            TypedExpressionKind::BuiltinVariant { variant, payload } => {
                self.emit_builtin_variant(
                    *variant,
                    payload.as_deref(),
                    &expression.ty,
                    destination,
                    span,
                )?;
            }
            TypedExpressionKind::Field { base, field }
            | TypedExpressionKind::StateField { base, field } => {
                self.emit_field(base, *field, destination, span)?;
            }
            TypedExpressionKind::Index { base, index } => {
                self.emit_index(base, index, &expression.ty, destination, span)?;
            }
            TypedExpressionKind::Array(values) => {
                self.emit_array(values, &expression.ty, destination, span)?;
            }
            TypedExpressionKind::Tuple(values) => {
                self.emit_tuple(values, &expression.ty, destination, span)?;
            }
            TypedExpressionKind::Match { value, arms } => {
                self.emit_match(value, arms, destination, span)?;
            }
            TypedExpressionKind::Try(value) => {
                self.emit_try(value, destination, span)?;
            }
            TypedExpressionKind::Update { base, fields } => {
                self.emit_update(base, fields, TypedAggregateKind::Struct, destination, span)?;
            }
            TypedExpressionKind::Migration(intrinsic) => {
                self.emit_migration(intrinsic, &expression.ty, destination, span)?;
            }
            TypedExpressionKind::Await(value) => {
                let first_instruction = self.code.len();
                self.emit_expression(value, destination)?;
                let suspension = (first_instruction..self.code.len())
                    .rev()
                    .find(|index| {
                        matches!(
                            self.code[*index],
                            Instruction::Call { .. } | Instruction::HostCall { .. }
                        )
                    })
                    .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
                self.spans[suspension] = span;
            }
        }
        if expression.ty == IrType::Unit {
            self.push(
                Instruction::LoadI32 {
                    dst: destination,
                    value: 0,
                },
                span,
            );
        }
        Ok(())
    }

    fn validate_unit_expression_contract(
        &self,
        expression: &TypedExpressionIr,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let unit = expression.ty == IrType::Unit;
        if matches!(
            &expression.kind,
            TypedExpressionKind::Literal(IrLiteral::Unit)
        ) {
            return unit
                .then_some(())
                .ok_or_else(|| CompileError::type_mismatch(None, None, span));
        }
        if !unit {
            return Ok(());
        }

        let valid = match &expression.kind {
            TypedExpressionKind::Reference(definition)
            | TypedExpressionKind::Call {
                callee: definition, ..
            } => self
                .package
                .definition(*definition)
                .is_some_and(|definition| definition.ty == IrType::Unit),
            TypedExpressionKind::StandardCall { .. }
            | TypedExpressionKind::BuiltinCall { .. }
            | TypedExpressionKind::Migration(_) => true,
            TypedExpressionKind::HostCall {
                contract, function, ..
            } => self.package.metadata().host_bindings.iter().any(|host| {
                host.contract == *contract
                    && host.functions.iter().any(|binding| {
                        binding.definition == *function
                            && binding.mode == IrHostFunctionMode::Sync
                            && binding.result == IrType::Unit
                    })
            }),
            TypedExpressionKind::Field { field, .. }
            | TypedExpressionKind::StateField { field, .. } => self
                .package
                .definition(*field)
                .is_some_and(|definition| definition.ty == IrType::Unit),
            TypedExpressionKind::Index { base, .. } => match &base.ty {
                IrType::Array(element) | IrType::Buffer(element) => {
                    element.as_ref() == &IrType::Unit
                }
                IrType::Map(_, value) => value.as_ref() == &IrType::Unit,
                _ => false,
            },
            TypedExpressionKind::Match { arms, .. } => {
                !arms.is_empty() && arms.iter().all(|arm| arm.value.ty == IrType::Unit)
            }
            TypedExpressionKind::Try(value) => matches!(
                &value.ty,
                IrType::Option(success) | IrType::Result(success, _)
                    if success.as_ref() == &IrType::Unit
            ),
            TypedExpressionKind::Await(value) => {
                value.ty == IrType::Unit
                    && value.effect == IrEffect::Task
                    && expression.effect == IrEffect::Task
            }
            TypedExpressionKind::Yield => expression.effect == IrEffect::Task,
            TypedExpressionKind::Literal(_)
            | TypedExpressionKind::PersistentStateGet { .. }
            | TypedExpressionKind::Unary { .. }
            | TypedExpressionKind::Binary { .. }
            | TypedExpressionKind::StringInterpolation(_)
            | TypedExpressionKind::Construct { .. }
            | TypedExpressionKind::ClassConstruct { .. }
            | TypedExpressionKind::EnumConstruct { .. }
            | TypedExpressionKind::BuiltinVariant { .. }
            | TypedExpressionKind::Array(_)
            | TypedExpressionKind::Tuple(_)
            | TypedExpressionKind::Update { .. } => false,
        };
        valid
            .then_some(())
            .ok_or_else(|| CompileError::type_mismatch(None, None, span))
    }

    #[allow(clippy::too_many_lines)]
    fn emit_migration(
        &mut self,
        intrinsic: &MigrationIntrinsicIr,
        expression_type: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if self.function_effect != IrEffect::Migration {
            return Err(CompileError::invalid_effect(span));
        }
        match intrinsic {
            MigrationIntrinsicIr::OldGet {
                identity,
                value_type,
            } => {
                if expression_type != value_type {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(
                    Instruction::StateOldGet {
                        stable_id: *identity,
                        ty: lower_type(self.package, value_type, span)?,
                        dst: destination,
                    },
                    span,
                );
            }
            MigrationIntrinsicIr::OldFieldGet {
                object,
                field,
                value_type,
            } => {
                if expression_type != value_type {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let Some(state_owner) = migration_field_owner(
                    &self.package.metadata().state_types,
                    &object.ty,
                    *field,
                    value_type,
                ) else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let object_register = self.allocate_expression(object)?;
                self.emit_expression(object, object_register)?;
                let (layout_owner, layout_field) =
                    self.layouts.fields.get(field).ok_or_else(|| {
                        CompileError::unknown_name(self.definition_name(*field), span)
                    })?;
                if *layout_owner != state_owner
                    || layout_field.ty != lower_type(self.package, value_type, span)?
                {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(
                    Instruction::StateOldFieldGet {
                        object: object_register,
                        field_id: layout_field.stable_id,
                        ty: layout_field.ty,
                        dst: destination,
                    },
                    span,
                );
            }
            MigrationIntrinsicIr::NewCreate {
                identity,
                state_type,
            } => {
                if expression_type != &IrType::Named(*state_type) {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                if !migration_state_type_exists(&self.package.metadata().state_types, *state_type) {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let layout = self.layouts.aggregates.get(state_type).ok_or_else(|| {
                    CompileError::unknown_type(self.definition_name(*state_type), span)
                })?;
                self.push(
                    Instruction::StateNewCreate {
                        stable_id: *identity,
                        type_id: layout.type_id,
                        dst: destination,
                    },
                    span,
                );
            }
            MigrationIntrinsicIr::NewSet {
                object,
                field,
                value,
            } => {
                if expression_type != &IrType::Bool {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let Some(state_owner) = migration_field_owner(
                    &self.package.metadata().state_types,
                    &object.ty,
                    *field,
                    &value.ty,
                ) else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let object_register = self.allocate_expression(object)?;
                let value_register = self.allocate_expression(value)?;
                self.emit_expression(object, object_register)?;
                self.emit_expression(value, value_register)?;
                let (layout_owner, layout_field) =
                    self.layouts.fields.get(field).ok_or_else(|| {
                        CompileError::unknown_name(self.definition_name(*field), span)
                    })?;
                if *layout_owner != state_owner
                    || layout_field.ty != lower_type(self.package, &value.ty, span)?
                {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(
                    Instruction::StateNewSet {
                        object: object_register,
                        field_id: layout_field.stable_id,
                        source: value_register,
                    },
                    span,
                );
                self.push(
                    Instruction::LoadBool {
                        dst: destination,
                        value: true,
                    },
                    span,
                );
            }
            MigrationIntrinsicIr::Preserve { identity } => {
                self.emit_migration_action(
                    Instruction::StatePreserve {
                        stable_id: *identity,
                    },
                    expression_type,
                    destination,
                    span,
                )?;
            }
            MigrationIntrinsicIr::Replace { identity, target } => {
                if expression_type != &IrType::Bool {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let target_register = self.allocate_expression(target)?;
                self.emit_expression(target, target_register)?;
                if !matches!(
                    target.ty,
                    IrType::Named(definition)
                        if migration_state_type_exists(
                            &self.package.metadata().state_types,
                            definition
                        )
                ) {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(
                    Instruction::StateReplace {
                        old_id: *identity,
                        target: target_register,
                    },
                    span,
                );
                self.push(
                    Instruction::LoadBool {
                        dst: destination,
                        value: true,
                    },
                    span,
                );
            }
            MigrationIntrinsicIr::Delete { identity } => {
                self.emit_migration_action(
                    Instruction::StateDelete {
                        stable_id: *identity,
                    },
                    expression_type,
                    destination,
                    span,
                )?;
            }
            MigrationIntrinsicIr::Finish => {
                self.emit_migration_action(
                    Instruction::StateFinish,
                    expression_type,
                    destination,
                    span,
                )?;
            }
        }
        Ok(())
    }

    fn emit_migration_action(
        &mut self,
        instruction: Instruction,
        expression_type: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if expression_type != &IrType::Bool {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        self.push(instruction, span);
        self.push(
            Instruction::LoadBool {
                dst: destination,
                value: true,
            },
            span,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_standard_call(
        &mut self,
        function: DefinitionId,
        intrinsic: nexa_stdlib::Intrinsic,
        type_arguments: &[IrType],
        arguments: &[TypedExpressionIr],
        result: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if self.standard_functions.get(&function) != Some(&intrinsic) {
            return Err(CompileError::unknown_name(
                self.definition_name(function),
                span,
            ));
        }
        let binding = self
            .package
            .metadata()
            .standard_functions
            .iter()
            .find(|binding| binding.definition == function)
            .ok_or_else(|| CompileError::unknown_name(self.definition_name(function), span))?;
        validate_standard_call_signature(
            binding,
            intrinsic,
            type_arguments,
            arguments,
            result,
            span,
        )?;
        let lowering = standard_call_lowering(
            self.package,
            intrinsic,
            type_arguments,
            arguments,
            result,
            span,
        )?;
        let (args_base, argument_registers, argument_slots) =
            self.reserve_physical_arguments(arguments)?;
        for (argument, register) in arguments.iter().zip(argument_registers) {
            self.emit_expression(argument, register)?;
        }
        match lowering {
            TypedStandardLowering::Intrinsic(intrinsic) => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            TypedStandardLowering::ToString(ty) => {
                if arguments.len() != 1 {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.push(ty.instruction(destination, args_base), span)
            }
            TypedStandardLowering::SetNew { .. } => {
                let ValueType::Named(type_id) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::SetNew {
                        type_id,
                        dst: destination,
                    },
                    span,
                )
            }
            TypedStandardLowering::SetClear { .. } => {
                self.push(Instruction::SetClear { source: args_base }, span)
            }
        };
        Ok(())
    }

    fn emit_inline_map_call(
        &mut self,
        operation: BuiltinOperationIr,
        arguments: &[TypedExpressionIr],
        result: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<bool, CompileError> {
        let Some(TypedExpressionKind::Reference(definition)) =
            arguments.first().map(|argument| &argument.kind)
        else {
            return Ok(false);
        };
        let Some(state) = self.inline_maps.get(definition).copied() else {
            return Ok(false);
        };
        match operation {
            BuiltinOperationIr::MapSet if arguments.len() == 3 => {
                let TypedExpressionKind::Literal(IrLiteral::I32(key)) = &arguments[1].kind else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.emit_expression(&arguments[2], state.value_register)?;
                self.inline_maps
                    .get_mut(definition)
                    .expect("inline map receiver was resolved")
                    .entry_key = Some(*key);
                self.push(
                    Instruction::LoadBool {
                        dst: destination,
                        value: true,
                    },
                    span,
                );
            }
            BuiltinOperationIr::MapGet if arguments.len() == 2 => {
                let TypedExpressionKind::Literal(IrLiteral::I32(key)) = &arguments[1].kind else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let hit = state.entry_key == Some(*key);
                let variant = if hit {
                    BuiltinVariantIr::OptionSome
                } else {
                    BuiltinVariantIr::OptionNone
                };
                let (enum_type, variant) =
                    builtin_variant_layout(self.package, variant, result, span)?;
                self.push(
                    Instruction::EnumNew {
                        type_id: enum_type.type_id,
                        variant: variant.stable_id,
                        payload: hit.then_some(state.value_register),
                        dst: destination,
                    },
                    span,
                );
            }
            _ => {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        Ok(true)
    }

    fn emit_inline_array_call(
        &mut self,
        operation: BuiltinOperationIr,
        arguments: &[TypedExpressionIr],
        destination: u16,
        span: SourceSpan,
    ) -> Result<bool, CompileError> {
        let Some(TypedExpressionKind::Reference(definition)) =
            arguments.first().map(|argument| &argument.kind)
        else {
            return Ok(false);
        };
        let Some(state) = self.inline_arrays.get(definition).copied() else {
            return Ok(false);
        };
        let slot = |index: u16| {
            state
                .element_slots
                .checked_mul(index)
                .and_then(|offset| state.slots_base.checked_add(offset))
                .ok_or_else(|| CompileError::too_many_registers(span))
        };
        let literal_index = |argument: &TypedExpressionIr| {
            let TypedExpressionKind::Literal(IrLiteral::I32(index)) = argument.kind else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            u16::try_from(index).map_err(|_| CompileError::type_mismatch(None, None, span))
        };
        match operation {
            BuiltinOperationIr::ArrayPush if arguments.len() == 2 => {
                if state.length >= state.capacity {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.emit_expression(&arguments[1], slot(state.length)?)?;
                self.inline_arrays
                    .get_mut(definition)
                    .expect("inline array receiver was resolved")
                    .length += 1;
                self.push(
                    Instruction::LoadBool {
                        dst: destination,
                        value: true,
                    },
                    span,
                );
            }
            BuiltinOperationIr::ArraySet if arguments.len() == 3 => {
                let index = literal_index(&arguments[1])?;
                if index >= state.length {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                self.emit_expression(&arguments[2], slot(index)?)?;
                self.push(
                    Instruction::LoadBool {
                        dst: destination,
                        value: true,
                    },
                    span,
                );
            }
            BuiltinOperationIr::ArrayGet if arguments.len() == 2 => {
                let index = literal_index(&arguments[1])?;
                if index >= state.length {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                let source = slot(index)?;
                let instruction =
                    self.copy_value_instruction(state.element_type, destination, source, span)?;
                self.push(instruction, span);
            }
            BuiltinOperationIr::ArrayLen if arguments.len() == 1 => {
                self.push(
                    Instruction::LoadI32 {
                        dst: destination,
                        value: i32::from(state.length),
                    },
                    span,
                );
            }
            _ => {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        Ok(true)
    }

    #[allow(clippy::too_many_lines)]
    fn emit_builtin_call(
        &mut self,
        operation: BuiltinOperationIr,
        type_arguments: &[IrType],
        arguments: &[TypedExpressionIr],
        result: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        validate_builtin_call_signature(
            self.package,
            operation,
            type_arguments,
            arguments,
            result,
            span,
        )?;
        if matches!(
            operation,
            BuiltinOperationIr::StateHandleResolve
                | BuiltinOperationIr::StateHandleIsAlive
                | BuiltinOperationIr::StateHandleStableId
                | BuiltinOperationIr::StateHandleGeneration
                | BuiltinOperationIr::StateHandleEqual
                | BuiltinOperationIr::StateHandleHash
        ) && matches!(
            self.function_effect,
            IrEffect::Migration | IrEffect::Cleanup
        ) {
            return Err(CompileError::invalid_effect(span));
        }
        if self.optimize && self.emit_inline_array_call(operation, arguments, destination, span)? {
            return Ok(());
        }
        if self.optimize
            && self.emit_inline_map_call(operation, arguments, result, destination, span)?
        {
            return Ok(());
        }

        // WP52 push-side fusion: a struct literal pushed into an array
        // writes its fields straight into the element row - no StructNew,
        // no source object. Evaluation order matches the unfused pipeline
        // exactly: the array first, then every field in declared layout
        // order (the same order the construct emission uses).
        if matches!(operation, BuiltinOperationIr::ArrayPush)
            && arguments.len() == 2
            && let TypedExpressionKind::Construct {
                definition: owner,
                fields,
                ..
            } = &arguments[1].kind
            && let Some(layout) = self.layouts.aggregates.get(owner).cloned()
            && layout.kind == TypedAggregateKind::Struct
        {
            let source = self.allocate_expression(&arguments[0])?;
            self.emit_expression(&arguments[0], source)?;
            let values = fields
                .iter()
                .map(|(field, value)| (*field, value))
                .collect::<BTreeMap<_, _>>();
            if values.len() != fields.len() || values.len() != layout.fields.len() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            let field_types = layout
                .fields
                .iter()
                .map(|field| field.ty)
                .collect::<Vec<_>>();
            let (fields_base, field_registers, fields_count) =
                self.reserve_physical_types(&field_types)?;
            for (field, register) in layout.fields.iter().zip(field_registers) {
                let value = values
                    .get(&field.definition)
                    .copied()
                    .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
                self.emit_expression(value, register)?;
            }
            self.push(
                Instruction::ArrayPushRow {
                    source,
                    fields_base,
                    fields_count,
                },
                span,
            );
            self.push(
                Instruction::LoadBool {
                    dst: destination,
                    value: true,
                },
                span,
            );
            return Ok(());
        }

        if matches!(
            operation,
            BuiltinOperationIr::ArrayPush
                | BuiltinOperationIr::ArraySet
                | BuiltinOperationIr::ArrayInsert
        ) {
            let expected = match operation {
                BuiltinOperationIr::ArrayPush => 2,
                BuiltinOperationIr::ArraySet | BuiltinOperationIr::ArrayInsert => 3,
                _ => unreachable!(),
            };
            if arguments.len() != expected {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            let source = self.allocate_expression(&arguments[0])?;
            self.emit_expression(&arguments[0], source)?;
            let index = if expected == 3 {
                let index = self.allocate_expression(&arguments[1])?;
                self.emit_expression(&arguments[1], index)?;
                Some(index)
            } else {
                None
            };
            let value_index = expected - 1;
            let value = self.allocate_expression(&arguments[value_index])?;
            self.emit_expression(&arguments[value_index], value)?;
            match operation {
                BuiltinOperationIr::ArrayPush => {
                    self.push(Instruction::ArrayPush { source, value }, span);
                }
                BuiltinOperationIr::ArraySet => {
                    self.push(
                        Instruction::ArraySet {
                            source,
                            index: index.expect("ArraySet has an index"),
                            value,
                        },
                        span,
                    );
                }
                BuiltinOperationIr::ArrayInsert => {
                    self.push(
                        Instruction::ArrayInsert {
                            source,
                            index: index.expect("ArrayInsert has an index"),
                            value,
                        },
                        span,
                    );
                }
                _ => unreachable!(),
            }
            self.push(
                Instruction::LoadBool {
                    dst: destination,
                    value: true,
                },
                span,
            );
            return Ok(());
        }

        if operation == BuiltinOperationIr::ValueToString
            && let [IrType::Named(definition)] = type_arguments
            && self.layouts.aggregates.contains_key(definition)
        {
            let [value] = arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let source = self.allocate_expression(value)?;
            self.emit_expression(value, source)?;
            return self.emit_nominal_to_string(
                *definition,
                source,
                destination,
                span,
                &mut BTreeSet::new(),
            );
        }

        let (args_base, argument_registers, argument_slots) =
            self.reserve_physical_arguments(arguments)?;
        for (argument, register) in arguments.iter().zip(argument_registers.iter().copied()) {
            self.emit_expression(argument, register)?;
        }
        let argument = |index: usize| {
            argument_registers
                .get(index)
                .copied()
                .ok_or_else(|| CompileError::type_mismatch(None, None, span))
        };
        let load_true = |emitter: &mut Self| {
            emitter.push(
                Instruction::LoadBool {
                    dst: destination,
                    value: true,
                },
                span,
            )
        };

        match operation {
            BuiltinOperationIr::ArrayNew => {
                let ValueType::Named(type_id) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::ArrayNew {
                        type_id,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::MapNew => {
                let ValueType::Named(type_id) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::MapNew {
                        type_id,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::StringLen => self.push(
                Instruction::StringLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringByteLen => self.push(
                Instruction::StringByteLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringEqual => self.push(
                Instruction::StringEqual {
                    lhs: argument(0)?,
                    rhs: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringConcat => self.push(
                Instruction::StringConcat {
                    lhs: argument(0)?,
                    rhs: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringRuneAt => self.push(
                Instruction::StringRuneAt {
                    source: argument(0)?,
                    index: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringHash => self.push(
                Instruction::StringHash {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayLen => self.push(
                Instruction::ArrayLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayGet => self.push(
                Instruction::ArrayGet {
                    source: argument(0)?,
                    index: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayTryGet => {
                let [element] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic: StandardIntrinsic::ArrayGet {
                            element: lower_type(self.package, element, span)?,
                        },
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::ArraySet => {
                self.push(
                    Instruction::ArraySet {
                        source: argument(0)?,
                        index: argument(1)?,
                        value: argument(2)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::ArrayPush => {
                self.push(
                    Instruction::ArrayPush {
                        source: argument(0)?,
                        value: argument(1)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::ArrayPop => self.push(
                Instruction::ArrayPop {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayInsert => {
                self.push(
                    Instruction::ArrayInsert {
                        source: argument(0)?,
                        index: argument(1)?,
                        value: argument(2)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::ArrayRemove => self.push(
                Instruction::ArrayRemove {
                    source: argument(0)?,
                    index: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayClear => {
                self.push(
                    Instruction::ArrayClear {
                        source: argument(0)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::ArrayReserve
            | BuiltinOperationIr::ArrayCapacity
            | BuiltinOperationIr::ArrayShrinkToFit
            | BuiltinOperationIr::ArrayFirst
            | BuiltinOperationIr::ArrayLast
            | BuiltinOperationIr::ArraySwap
            | BuiltinOperationIr::ArrayReverse => {
                let [element] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let element = lower_type(self.package, element, span)?;
                let intrinsic = match operation {
                    BuiltinOperationIr::ArrayReserve => StandardIntrinsic::ArrayReserve { element },
                    BuiltinOperationIr::ArrayCapacity => {
                        StandardIntrinsic::ArrayCapacity { element }
                    }
                    BuiltinOperationIr::ArrayShrinkToFit => {
                        StandardIntrinsic::ArrayShrinkToFit { element }
                    }
                    BuiltinOperationIr::ArrayFirst => StandardIntrinsic::ArrayFirst { element },
                    BuiltinOperationIr::ArrayLast => StandardIntrinsic::ArrayLast { element },
                    BuiltinOperationIr::ArraySwap => StandardIntrinsic::ArraySwap { element },
                    BuiltinOperationIr::ArrayReverse => StandardIntrinsic::ArrayReverse { element },
                    _ => unreachable!("array capacity operations are matched above"),
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::MapLen => self.push(
                Instruction::MapLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::MapGet | BuiltinOperationIr::MapRemove => {
                let ValueType::Named(result_type) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let instruction = if operation == BuiltinOperationIr::MapGet {
                    Instruction::MapGet {
                        source: argument(0)?,
                        key: argument(1)?,
                        result_type,
                        dst: destination,
                    }
                } else {
                    Instruction::MapRemove {
                        source: argument(0)?,
                        key: argument(1)?,
                        result_type,
                        dst: destination,
                    }
                };
                self.push(instruction, span)
            }
            BuiltinOperationIr::MapSet | BuiltinOperationIr::MapInsert => {
                self.push(
                    Instruction::MapSet {
                        source: argument(0)?,
                        key: argument(1)?,
                        value: argument(2)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::MapContains => self.push(
                Instruction::MapContains {
                    source: argument(0)?,
                    key: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::MapClear => {
                self.push(
                    Instruction::MapClear {
                        source: argument(0)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::MapIsEmpty
            | BuiltinOperationIr::MapGetOr
            | BuiltinOperationIr::MapInsertIfAbsent => {
                let [key, value] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let key = lower_type(self.package, key, span)?;
                let value = lower_type(self.package, value, span)?;
                let intrinsic = match operation {
                    BuiltinOperationIr::MapIsEmpty => StandardIntrinsic::MapIsEmpty { key, value },
                    BuiltinOperationIr::MapGetOr => StandardIntrinsic::MapGetOr { key, value },
                    BuiltinOperationIr::MapInsertIfAbsent => {
                        StandardIntrinsic::MapInsertIfAbsent { key, value }
                    }
                    _ => unreachable!("map extension operations are matched above"),
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::SetNew => {
                let ValueType::Named(type_id) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::SetNew {
                        type_id,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::SetLen => self.push(
                Instruction::SetLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::SetContains => self.push(
                Instruction::SetContains {
                    source: argument(0)?,
                    value: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::SetInsert => self.push(
                Instruction::SetInsert {
                    source: argument(0)?,
                    value: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::SetRemove => self.push(
                Instruction::SetRemove {
                    source: argument(0)?,
                    value: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::SetClear => self.push(
                Instruction::SetClear {
                    source: argument(0)?,
                },
                span,
            ),
            BuiltinOperationIr::BufferLen => self.push(
                Instruction::BufferLen {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::BufferGet => self.push(
                Instruction::BufferGet {
                    source: argument(0)?,
                    index: argument(1)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::BufferSet => {
                self.push(
                    Instruction::BufferSet {
                        source: argument(0)?,
                        index: argument(1)?,
                        value: argument(2)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::BufferSlice => self.push(
                Instruction::BufferSlice {
                    source: argument(0)?,
                    start: argument(1)?,
                    length: argument(2)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::BufferCopy => {
                self.push(
                    Instruction::BufferCopy {
                        destination: argument(0)?,
                        source: argument(1)?,
                        source_start: argument(2)?,
                        destination_start: argument(3)?,
                        length: argument(4)?,
                    },
                    span,
                );
                load_true(self)
            }
            BuiltinOperationIr::BufferIsEmpty | BuiltinOperationIr::BufferFill => {
                let [element] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let element = lower_type(self.package, element, span)?;
                let intrinsic = if operation == BuiltinOperationIr::BufferIsEmpty {
                    StandardIntrinsic::BufferIsEmpty { element }
                } else {
                    StandardIntrinsic::BufferFill { element }
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::StateHandleResolve => {
                let target = lower_type(self.package, &type_arguments[0], span)?;
                let ValueType::Named(result_type) = lower_type(self.package, result, span)? else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::StateHandleResolve {
                        handle: argument(0)?,
                        target,
                        result_type,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::StateHandleIsAlive => self.push(
                Instruction::StateHandleIsAlive {
                    handle: argument(0)?,
                    target: lower_type(self.package, &type_arguments[0], span)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StateHandleStableId => self.push(
                Instruction::StateHandleStableId {
                    handle: argument(0)?,
                    target: lower_type(self.package, &type_arguments[0], span)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StateHandleGeneration => self.push(
                Instruction::StateHandleGeneration {
                    handle: argument(0)?,
                    target: lower_type(self.package, &type_arguments[0], span)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StateHandleEqual => self.push(
                Instruction::StateHandleEqual {
                    lhs: argument(0)?,
                    rhs: argument(1)?,
                    target: lower_type(self.package, &type_arguments[0], span)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StateHandleHash => self.push(
                Instruction::StateHandleHash {
                    handle: argument(0)?,
                    target: lower_type(self.package, &type_arguments[0], span)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringContains => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringContains,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringStartsWith => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringStartsWith,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringEndsWith => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringEndsWith,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringSubstring => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringSubstring,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringTrim => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringTrim,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::StringSplit => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringSplit,
                    args_base,
                    args_count: argument_slots,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ArrayIsEmpty => {
                let [element] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic: StandardIntrinsic::ArrayIsEmpty {
                            element: lower_type(self.package, element, span)?,
                        },
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::OptionIsSome
            | BuiltinOperationIr::OptionIsNone
            | BuiltinOperationIr::OptionUnwrapOr => {
                let [inner] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let inner = lower_type(self.package, inner, span)?;
                let intrinsic = match operation {
                    BuiltinOperationIr::OptionIsSome => {
                        StandardIntrinsic::OptionIsSome { value: inner }
                    }
                    BuiltinOperationIr::OptionIsNone => {
                        StandardIntrinsic::OptionIsNone { value: inner }
                    }
                    BuiltinOperationIr::OptionUnwrapOr => {
                        StandardIntrinsic::OptionUnwrapOr { value: inner }
                    }
                    _ => unreachable!("option operations are matched above"),
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::ResultIsOk
            | BuiltinOperationIr::ResultIsErr
            | BuiltinOperationIr::ResultUnwrapOr => {
                let [ok, error] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                let ok = lower_type(self.package, ok, span)?;
                let error = lower_type(self.package, error, span)?;
                let intrinsic = match operation {
                    BuiltinOperationIr::ResultIsOk => {
                        StandardIntrinsic::ResultIsOk { success: ok, error }
                    }
                    BuiltinOperationIr::ResultIsErr => {
                        StandardIntrinsic::ResultIsErr { success: ok, error }
                    }
                    BuiltinOperationIr::ResultUnwrapOr => {
                        StandardIntrinsic::ResultUnwrapOr { success: ok, error }
                    }
                    _ => unreachable!("result operations are matched above"),
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic,
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
            BuiltinOperationIr::StringToString => self.push(
                Instruction::StringToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::I32ToString => self.push(
                Instruction::I32ToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::I64ToString => self.push(
                Instruction::I64ToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::F32ToString => self.push(
                Instruction::F32ToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::F64ToString => self.push(
                Instruction::F64ToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::BoolToString => self.push(
                Instruction::BoolToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::RuneToString => self.push(
                Instruction::RuneToString {
                    source: argument(0)?,
                    dst: destination,
                },
                span,
            ),
            BuiltinOperationIr::ValueToString => {
                let [value] = type_arguments else {
                    return Err(CompileError::type_mismatch(None, None, span));
                };
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic: StandardIntrinsic::ValueToString {
                            value: lower_type(self.package, value, span)?,
                        },
                        args_base,
                        args_count: argument_slots,
                        dst: destination,
                    },
                    span,
                )
            }
        };
        Ok(())
    }

    fn emit_construct(
        &mut self,
        definition: DefinitionId,
        fields: &[(DefinitionId, TypedExpressionIr)],
        expected_kind: TypedAggregateKind,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let layout =
            self.layouts.aggregates.get(&definition).ok_or_else(|| {
                CompileError::unknown_type(self.definition_name(definition), span)
            })?;
        if layout.kind != expected_kind {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        let values = fields
            .iter()
            .map(|(field, value)| (*field, value))
            .collect::<BTreeMap<_, _>>();
        if values.len() != fields.len() || values.len() != layout.fields.len() {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        let field_types = layout
            .fields
            .iter()
            .map(|field| field.ty)
            .collect::<Vec<_>>();
        let (fields_base, field_registers, fields_count) =
            self.reserve_physical_types(&field_types)?;
        for (field, register) in layout.fields.iter().zip(field_registers) {
            let value = values
                .get(&field.definition)
                .copied()
                .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
            self.emit_expression(value, register)?;
        }
        self.push(
            match layout.kind {
                TypedAggregateKind::Struct => Instruction::StructNew {
                    type_id: layout.type_id,
                    fields_base,
                    fields_count,
                    dst: destination,
                },
                TypedAggregateKind::Class => Instruction::ClassNew {
                    type_id: layout.type_id,
                    fields_base,
                    fields_count,
                    dst: destination,
                },
            },
            span,
        );
        Ok(())
    }

    fn emit_enum_construct(
        &mut self,
        enum_definition: DefinitionId,
        variant_definition: DefinitionId,
        payload: Option<&TypedExpressionIr>,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let layout = self.layouts.enums.get(&enum_definition).ok_or_else(|| {
            CompileError::unknown_type(self.definition_name(enum_definition), span)
        })?;
        let variant = layout
            .variants
            .get(&variant_definition)
            .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
        let payload = match (variant.payload, payload) {
            (Some(_), Some(payload)) => {
                let register = self.allocate_expression(payload)?;
                self.emit_expression(payload, register)?;
                Some(register)
            }
            (None, None) => None,
            _ => return Err(CompileError::type_mismatch(None, None, span)),
        };
        self.push(
            Instruction::EnumNew {
                type_id: layout.type_id,
                variant: variant.stable_id,
                payload,
                dst: destination,
            },
            span,
        );
        Ok(())
    }

    fn emit_builtin_variant(
        &mut self,
        variant: BuiltinVariantIr,
        payload: Option<&TypedExpressionIr>,
        ty: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let (enum_type, variant) = builtin_variant_layout(self.package, variant, ty, span)?;
        let payload = match (variant.payload_type, payload) {
            (Some(_), Some(payload)) => {
                let register = self.allocate_expression(payload)?;
                self.emit_expression(payload, register)?;
                Some(register)
            }
            (None, None) => None,
            _ => return Err(CompileError::type_mismatch(None, None, span)),
        };
        self.push(
            Instruction::EnumNew {
                type_id: enum_type.type_id,
                variant: variant.stable_id,
                payload,
                dst: destination,
            },
            span,
        );
        Ok(())
    }

    fn emit_field(
        &mut self,
        base: &TypedExpressionIr,
        field: DefinitionId,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        // WP45: field reads off scalar-replaced class bindings are plain
        // register moves; the object never existed on the heap.
        if let TypedExpressionKind::Reference(definition) = &base.kind
            && let Some((owner, fields_base)) = self.inline_classes.get(definition).copied()
        {
            let source = self.inline_field_register(owner, fields_base, field, span)?;
            let (_, field_layout) = self
                .layouts
                .fields
                .get(&field)
                .ok_or_else(|| CompileError::unknown_name(self.definition_name(field), span))?;
            let instruction =
                self.copy_value_instruction(field_layout.ty, destination, source, span)?;
            self.push(instruction, span);
            return Ok(());
        }
        let source = self.allocate_expression(base)?;
        self.emit_expression(base, source)?;
        let (owner, field) = self
            .layouts
            .fields
            .get(&field)
            .ok_or_else(|| CompileError::unknown_name(self.definition_name(field), span))?;
        let layout = &self.layouts.aggregates[owner];
        self.push(
            match layout.kind {
                TypedAggregateKind::Struct => Instruction::StructGet {
                    source,
                    field: field.stable_id,
                    dst: destination,
                },
                TypedAggregateKind::Class => Instruction::ClassGet {
                    source,
                    field: field.stable_id,
                    dst: destination,
                },
            },
            span,
        );
        Ok(())
    }

    fn emit_index(
        &mut self,
        base: &TypedExpressionIr,
        index: &TypedExpressionIr,
        result: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let source = self.allocate_expression(base)?;
        let index_register = self.allocate_expression(index)?;
        self.emit_expression(base, source)?;
        self.emit_expression(index, index_register)?;
        let instruction = match &base.ty {
            IrType::Array(_) => Some(Instruction::ArrayGet {
                source,
                index: index_register,
                dst: destination,
            }),
            IrType::Buffer(_) => Some(Instruction::BufferGet {
                source,
                index: index_register,
                dst: destination,
            }),
            IrType::String => Some(Instruction::StringRuneAt {
                source,
                index: index_register,
                dst: destination,
            }),
            IrType::Map(_, _) => {
                let value_type = lower_type(self.package, result, span)?;
                let option = option_type(value_type);
                let option_register = self.allocate(ValueType::Named(option.type_id))?;
                self.push(
                    Instruction::MapGet {
                        source,
                        key: index_register,
                        result_type: option.type_id,
                        dst: option_register,
                    },
                    span,
                );
                let some = option
                    .variants
                    .get(1)
                    .ok_or_else(|| CompileError::type_mismatch(None, None, span))?
                    .stable_id;
                self.push(
                    Instruction::EnumPayload {
                        source: option_register,
                        variant: some,
                        dst: destination,
                    },
                    span,
                );
                None
            }
            _ => return Err(CompileError::type_mismatch(None, None, span)),
        };
        if let Some(instruction) = instruction {
            self.push(instruction, span);
        }
        Ok(())
    }

    fn emit_array(
        &mut self,
        values: &[TypedExpressionIr],
        ty: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let ValueType::Named(type_id) = lower_type(self.package, ty, span)? else {
            return Err(CompileError::type_mismatch(None, None, span));
        };
        self.push(
            Instruction::ArrayNew {
                type_id,
                dst: destination,
            },
            span,
        );
        for value in values {
            let source = self.allocate_expression(value)?;
            self.emit_expression(value, source)?;
            self.push(
                Instruction::ArrayPush {
                    source: destination,
                    value: source,
                },
                self.span(&value.span)?,
            );
        }
        Ok(())
    }

    fn emit_tuple(
        &mut self,
        values: &[TypedExpressionIr],
        ty: &IrType,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let ValueType::Named(type_id) = lower_type(self.package, ty, span)? else {
            return Err(CompileError::type_mismatch(None, None, span));
        };
        let field_types = values
            .iter()
            .map(|value| lower_type(self.package, &value.ty, span))
            .collect::<Result<Vec<_>, _>>()?;
        let (fields_base, field_registers, fields_count) =
            self.reserve_physical_types(&field_types)?;
        for (value, register) in values.iter().zip(field_registers) {
            self.emit_expression(value, register)?;
        }
        self.push(
            Instruction::StructNew {
                type_id,
                fields_base,
                fields_count,
                dst: destination,
            },
            span,
        );
        Ok(())
    }

    fn emit_update(
        &mut self,
        base: &TypedExpressionIr,
        fields: &[(DefinitionId, TypedExpressionIr)],
        expected_kind: TypedAggregateKind,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let IrType::Named(owner) = &base.ty else {
            return Err(CompileError::type_mismatch(None, None, span));
        };
        let owner = *owner;
        let layout = self
            .layouts
            .aggregates
            .get(&owner)
            .cloned()
            .ok_or_else(|| CompileError::unknown_type(self.definition_name(owner), span))?;
        if layout.kind != expected_kind {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        let mut overridden = BTreeSet::new();
        for (field, _) in fields {
            let Some((field_owner, _)) = self.layouts.fields.get(field) else {
                return Err(CompileError::unknown_name(
                    self.definition_name(*field),
                    span,
                ));
            };
            if *field_owner != owner || !overridden.insert(*field) {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        match layout.kind {
            TypedAggregateKind::Struct => {
                self.emit_expression(base, destination)?;
                for (field_definition, value) in fields {
                    let (_, field) = &self.layouts.fields[field_definition];
                    let value_register = self.allocate_expression(value)?;
                    self.emit_expression(value, value_register)?;
                    self.push(
                        Instruction::StructWith {
                            source: destination,
                            field: field.stable_id,
                            value: value_register,
                            dst: destination,
                        },
                        self.span(&value.span)?,
                    );
                }
            }
            TypedAggregateKind::Class => {
                // A class update is an explicit `Class { ..base }`: it creates a fresh object
                // rather than mutating or aliasing `base`.
                let source = self.allocate_expression(base)?;
                self.emit_expression(base, source)?;
                let field_types = layout
                    .fields
                    .iter()
                    .map(|field| field.ty)
                    .collect::<Vec<_>>();
                let (fields_base, field_registers, fields_count) =
                    self.reserve_physical_types(&field_types)?;
                for (target, field) in field_registers.iter().copied().zip(&layout.fields) {
                    self.push(
                        Instruction::ClassGet {
                            source,
                            field: field.stable_id,
                            dst: target,
                        },
                        span,
                    );
                }
                for (field_definition, value) in fields {
                    let offset = layout
                        .fields
                        .iter()
                        .position(|field| field.definition == *field_definition)
                        .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
                    let target = field_registers[offset];
                    self.emit_expression(value, target)?;
                }
                self.push(
                    Instruction::ClassNew {
                        type_id: layout.type_id,
                        fields_base,
                        fields_count,
                        dst: destination,
                    },
                    span,
                );
            }
        }
        Ok(())
    }

    fn emit_match(
        &mut self,
        value: &TypedExpressionIr,
        arms: &[nexa_analysis::TypedMatchArmIr],
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if let TypedExpressionKind::BuiltinCall {
            operation: BuiltinOperationIr::MapGet,
            arguments,
            ..
        } = &value.kind
            && arguments.len() == 2
            && let TypedExpressionKind::Reference(definition) = &arguments[0].kind
            && let Some(state) = self.inline_maps.get(definition).copied()
            && let TypedExpressionKind::Literal(IrLiteral::I32(key)) = &arguments[1].kind
        {
            let hit = state.entry_key == Some(*key);
            return self.emit_inline_builtin_match(
                if hit {
                    BuiltinVariantIr::OptionSome
                } else {
                    BuiltinVariantIr::OptionNone
                },
                hit.then_some(state.value_register),
                arms,
                destination,
                span,
            );
        }
        let source = self.allocate_expression(value)?;
        self.emit_expression(value, source)?;
        let mut ends = Vec::new();
        for arm in arms {
            let mut failures = Vec::new();
            self.emit_pattern_guard(source, &arm.pattern, &mut failures, span)?;
            self.emit_expression(&arm.value, destination)?;
            ends.push(self.push(Instruction::Jump { target: 0 }, span));
            let next = self.position();
            for failure in failures {
                self.patch_target(failure, next)?;
            }
        }
        self.push(Instruction::Trap, span);
        let end = self.position();
        for patch in ends {
            self.patch_target(patch, end)?;
        }
        Ok(())
    }

    fn emit_inline_builtin_match(
        &mut self,
        variant: BuiltinVariantIr,
        payload_register: Option<u16>,
        arms: &[nexa_analysis::TypedMatchArmIr],
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let mut ends = Vec::new();
        for arm in arms {
            let mut failures = Vec::new();
            match &arm.pattern.kind {
                TypedPatternKind::BuiltinVariant {
                    variant: candidate,
                    payload,
                } if *candidate == variant => match (payload.as_deref(), payload_register) {
                    (None, None) => {}
                    (Some(pattern), Some(register)) => {
                        self.emit_pattern_guard(register, pattern, &mut failures, span)?;
                    }
                    _ => return Err(CompileError::type_mismatch(None, None, span)),
                },
                TypedPatternKind::BuiltinVariant { .. } => continue,
                TypedPatternKind::Wildcard => {}
                _ => return Err(CompileError::type_mismatch(None, None, span)),
            }
            let unconditional = failures.is_empty();
            self.emit_expression(&arm.value, destination)?;
            ends.push(self.push(Instruction::Jump { target: 0 }, span));
            let next = self.position();
            for failure in failures {
                self.patch_target(failure, next)?;
            }
            if unconditional {
                break;
            }
        }
        self.push(Instruction::Trap, span);
        let end = self.position();
        for patch in ends {
            self.patch_target(patch, end)?;
        }
        Ok(())
    }

    fn emit_pattern_guard(
        &mut self,
        source: u16,
        pattern: &TypedPatternIr,
        failures: &mut Vec<usize>,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        match &pattern.kind {
            TypedPatternKind::Wildcard => {}
            TypedPatternKind::Binding(definition) => {
                let destination = self.local(*definition)?;
                let ty = lower_type(self.package, &pattern.ty, span)?;
                let instruction = self.copy_value_instruction(ty, destination, source, span)?;
                self.push(instruction, span);
            }
            TypedPatternKind::Literal(literal) => {
                let literal_register = self.allocate_expression(&TypedExpressionIr {
                    ty: pattern.ty.clone(),
                    effect: IrEffect::Immediate,
                    span: pattern.span.clone(),
                    kind: TypedExpressionKind::Literal(literal.clone()),
                })?;
                self.emit_literal(literal, literal_register, span)?;
                let condition = self.allocate(ValueType::Bool)?;
                self.push(
                    equality_instruction(
                        &pattern.ty,
                        lower_type(self.package, &pattern.ty, span)?,
                        condition,
                        source,
                        literal_register,
                        self.layouts,
                        span,
                    )?,
                    span,
                );
                failures.push(self.push(
                    Instruction::JumpIfFalse {
                        condition,
                        target: 0,
                    },
                    self.span(&pattern.span)?,
                ));
            }
            TypedPatternKind::Variant {
                definition,
                payload,
            } => {
                let (_, variant) = self.layouts.variants.get(definition).ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*definition), span)
                })?;
                let payload = payload.iter().collect::<Vec<_>>();
                self.emit_variant_pattern_guard(source, variant, &payload, failures, span)?;
            }
            TypedPatternKind::BuiltinVariant { variant, payload } => {
                let (_, variant_layout) =
                    builtin_variant_layout(self.package, *variant, &pattern.ty, span)?;
                let payload = payload.as_deref().into_iter().collect::<Vec<_>>();
                self.emit_variant_pattern_guard(
                    source,
                    &TypedVariantLayout {
                        stable_id: variant_layout.stable_id,
                        tag: variant_layout.tag,
                        payload: variant_layout.payload_type,
                    },
                    &payload,
                    failures,
                    span,
                )?;
            }
            TypedPatternKind::Struct { fields, .. } => {
                for (field_definition, field_pattern) in fields {
                    let field_value =
                        self.allocate(lower_type(self.package, &field_pattern.ty, span)?)?;
                    self.emit_field_from_register(source, *field_definition, field_value, span)?;
                    self.emit_pattern_guard(field_value, field_pattern, failures, span)?;
                }
            }
        }
        Ok(())
    }

    fn emit_variant_pattern_guard(
        &mut self,
        source: u16,
        variant: &TypedVariantLayout,
        payload_patterns: &[&TypedPatternIr],
        failures: &mut Vec<usize>,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let condition = self.allocate(ValueType::Bool)?;
        let tag = self.allocate(ValueType::I32)?;
        let expected = self.allocate(ValueType::I32)?;
        self.push(Instruction::EnumTag { source, dst: tag }, span);
        self.push(
            Instruction::LoadI32 {
                dst: expected,
                value: i32::try_from(variant.tag)
                    .map_err(|_| CompileError::type_mismatch(None, None, span))?,
            },
            span,
        );
        self.push(
            Instruction::CompareEq {
                dst: condition,
                lhs: tag,
                rhs: expected,
            },
            span,
        );
        if payload_patterns.is_empty() != variant.payload.is_none() {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        failures.push(self.push(
            Instruction::JumpIfFalse {
                condition,
                target: 0,
            },
            span,
        ));
        let Some(payload_type) = variant.payload else {
            return Ok(());
        };
        let payload = self.allocate(payload_type)?;
        self.push(
            Instruction::EnumPayload {
                source,
                variant: variant.stable_id,
                dst: payload,
            },
            span,
        );
        if let [payload_pattern] = payload_patterns {
            self.emit_pattern_guard(payload, payload_pattern, failures, span)?;
        } else if !payload_patterns.is_empty() {
            let item_types = payload_patterns
                .iter()
                .map(|pattern| lower_type(self.package, &pattern.ty, span))
                .collect::<Result<Vec<_>, _>>()?;
            let expected_tuple = parameterized_type_id("Tuple", &item_types);
            if payload_type != ValueType::Named(expected_tuple) {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            for (index, (pattern, item_type)) in payload_patterns.iter().zip(item_types).enumerate()
            {
                let field_value = self.allocate(item_type)?;
                self.push(
                    Instruction::StructGet {
                        source: payload,
                        field: tuple_field_stable_id(expected_tuple, index),
                        dst: field_value,
                    },
                    span,
                );
                self.emit_pattern_guard(field_value, pattern, failures, span)?;
            }
        }
        Ok(())
    }

    fn emit_field_from_register(
        &mut self,
        source: u16,
        field_definition: DefinitionId,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let (owner, field) = self.layouts.fields.get(&field_definition).ok_or_else(|| {
            CompileError::unknown_name(self.definition_name(field_definition), span)
        })?;
        self.push(
            match self.layouts.aggregates[owner].kind {
                TypedAggregateKind::Struct => Instruction::StructGet {
                    source,
                    field: field.stable_id,
                    dst: destination,
                },
                TypedAggregateKind::Class => Instruction::ClassGet {
                    source,
                    field: field.stable_id,
                    dst: destination,
                },
            },
            span,
        );
        Ok(())
    }

    fn emit_try(
        &mut self,
        value: &TypedExpressionIr,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let source = self.allocate_expression(value)?;
        self.emit_expression(value, source)?;
        let (success, failure, function_failure_variant) = match &value.ty {
            IrType::Option(_) => (
                BuiltinVariantIr::OptionSome,
                BuiltinVariantIr::OptionNone,
                BuiltinVariantIr::OptionNone,
            ),
            IrType::Result(_, _) => (
                BuiltinVariantIr::ResultOk,
                BuiltinVariantIr::ResultErr,
                BuiltinVariantIr::ResultErr,
            ),
            _ => return Err(CompileError::type_mismatch(None, None, span)),
        };
        let (_, success_variant) = builtin_variant_layout(self.package, success, &value.ty, span)?;
        let (_, failure_variant) = builtin_variant_layout(self.package, failure, &value.ty, span)?;
        let tag = self.allocate(ValueType::I32)?;
        let expected = self.allocate(ValueType::I32)?;
        let condition = self.allocate(ValueType::Bool)?;
        self.push(Instruction::EnumTag { source, dst: tag }, span);
        self.push(
            Instruction::LoadI32 {
                dst: expected,
                value: i32::try_from(success_variant.tag)
                    .map_err(|_| CompileError::type_mismatch(None, None, span))?,
            },
            span,
        );
        self.push(
            Instruction::CompareEq {
                dst: condition,
                lhs: tag,
                rhs: expected,
            },
            span,
        );
        let failure_jump = self.push(
            Instruction::JumpIfFalse {
                condition,
                target: 0,
            },
            span,
        );
        self.push(
            Instruction::EnumPayload {
                source,
                variant: success_variant.stable_id,
                dst: destination,
            },
            span,
        );
        let end = self.push(Instruction::Jump { target: 0 }, span);
        self.patch_target(failure_jump, self.position())?;
        let failure_payload = if let Some(payload_type) = failure_variant.payload_type {
            let payload = self.allocate(payload_type)?;
            self.push(
                Instruction::EnumPayload {
                    source,
                    variant: failure_variant.stable_id,
                    dst: payload,
                },
                span,
            );
            Some(payload)
        } else {
            None
        };
        let return_value =
            self.allocate(lower_type(self.package, self.function_return_type, span)?)?;
        let (return_enum, return_variant) = builtin_variant_layout(
            self.package,
            function_failure_variant,
            self.function_return_type,
            span,
        )?;
        self.push(
            Instruction::EnumNew {
                type_id: return_enum.type_id,
                variant: return_variant.stable_id,
                payload: failure_payload,
                dst: return_value,
            },
            span,
        );
        self.push(
            Instruction::Return {
                source: return_value,
            },
            span,
        );
        self.patch_target(end, self.position())?;
        Ok(())
    }

    fn emit_binary(
        &mut self,
        operator: BinaryOperator,
        left: &TypedExpressionIr,
        right: &TypedExpressionIr,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if matches!(operator, BinaryOperator::And | BinaryOperator::Or) {
            return self.emit_short_circuit(operator, left, right, destination, span);
        }
        let ty = lower_type(self.package, &left.ty, self.span(&left.span)?)?;
        if self.optimize && operator == BinaryOperator::Add && ty == ValueType::String {
            let mut parts = Vec::new();
            collect_string_concat_parts(left, &mut parts);
            collect_string_concat_parts(right, &mut parts);
            return self.emit_string_build(&parts, destination, span);
        }
        let lhs = self.allocate_expression(left)?;
        let rhs = self.allocate_expression(right)?;
        self.emit_expression(left, lhs)?;
        self.emit_expression(right, rhs)?;
        let instruction = match operator {
            BinaryOperator::Add if ty == ValueType::String => Instruction::StringConcat {
                dst: destination,
                lhs,
                rhs,
            },
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Remainder => {
                let numeric = TypedNumericKind::from_ir_type(&left.ty, span)?;
                let operator = TypedNumericOperator::try_from(operator)
                    .map_err(|()| CompileError::type_mismatch(None, None, span))?;
                numeric.binary(operator, destination, lhs, rhs)
            }
            BinaryOperator::Equal => {
                equality_instruction(&left.ty, ty, destination, lhs, rhs, self.layouts, span)?
            }
            BinaryOperator::NotEqual => {
                self.push(
                    equality_instruction(&left.ty, ty, destination, lhs, rhs, self.layouts, span)?,
                    span,
                );
                return self.invert_bool(destination, span);
            }
            BinaryOperator::Less
            | BinaryOperator::Greater
            | BinaryOperator::LessEqual
            | BinaryOperator::GreaterEqual => {
                let reverse = matches!(
                    operator,
                    BinaryOperator::Greater | BinaryOperator::GreaterEqual
                );
                let (comparison_lhs, comparison_rhs) =
                    if reverse { (rhs, lhs) } else { (lhs, rhs) };
                let numeric = TypedNumericKind::from_ir_type(&left.ty, span)?;
                self.push(
                    numeric.compare_less(destination, comparison_lhs, comparison_rhs),
                    span,
                );
                if matches!(
                    operator,
                    BinaryOperator::LessEqual | BinaryOperator::GreaterEqual
                ) {
                    let compare_equal = self.push(
                        Instruction::JumpIfFalse {
                            condition: destination,
                            target: 0,
                        },
                        span,
                    );
                    let finish = self.push(Instruction::Jump { target: 0 }, span);
                    self.patch_target(compare_equal, self.position())?;
                    self.push(
                        equality_instruction(
                            &left.ty,
                            ty,
                            destination,
                            lhs,
                            rhs,
                            self.layouts,
                            span,
                        )?,
                        span,
                    );
                    self.patch_target(finish, self.position())?;
                }
                return Ok(());
            }
            BinaryOperator::And | BinaryOperator::Or => unreachable!("handled above"),
        };
        self.push(instruction, span);
        Ok(())
    }

    fn emit_string_build(
        &mut self,
        parts: &[&TypedExpressionIr],
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        if parts.is_empty() {
            self.push(
                Instruction::LoadString {
                    dst: destination,
                    string: self.string_indices[""],
                },
                span,
            );
            return Ok(());
        }
        let mut types = Vec::with_capacity(parts.len());
        for part in parts {
            let part_span = self.span(&part.span)?;
            TypedScalarToString::from_ir_type(&part.ty, part_span)?;
            types.push(lower_type(self.package, &part.ty, part_span)?);
        }
        let parts_base = self.reserve_types(&types)?;
        for (offset, part) in parts.iter().enumerate() {
            let register = parts_base
                .checked_add(
                    u16::try_from(offset)
                        .map_err(|_| CompileError::too_many_registers(self.function_span))?,
                )
                .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
            self.emit_expression(part, register)?;
        }
        self.push(
            Instruction::StringBuild {
                dst: destination,
                parts_base,
                parts_count: u16::try_from(parts.len())
                    .map_err(|_| CompileError::too_many_registers(span))?,
            },
            span,
        );
        Ok(())
    }

    fn emit_formattable_register(
        &mut self,
        ty: &IrType,
        source: u16,
        destination: u16,
        span: SourceSpan,
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> Result<(), CompileError> {
        if let Ok(conversion) = TypedScalarToString::from_ir_type(ty, span) {
            self.push(conversion.instruction(destination, source), span);
            return Ok(());
        }
        match ty {
            IrType::Array(_) => {
                let value = lower_type(self.package, ty, span)?;
                let slots = self.layouts.physical_slots(value, span)?;
                self.push(
                    Instruction::StandardIntrinsic {
                        intrinsic: StandardIntrinsic::ValueToString { value },
                        args_base: source,
                        args_count: slots,
                        dst: destination,
                    },
                    span,
                );
                Ok(())
            }
            IrType::Named(definition) if self.layouts.aggregates.contains_key(definition) => {
                self.emit_nominal_to_string(*definition, source, destination, span, visiting)
            }
            _ => Err(CompileError::type_mismatch(None, None, span)),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn emit_nominal_to_string(
        &mut self,
        definition: DefinitionId,
        source: u16,
        destination: u16,
        span: SourceSpan,
        visiting: &mut BTreeSet<DefinitionId>,
    ) -> Result<(), CompileError> {
        if !visiting.insert(definition) {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        let aggregate = self
            .layouts
            .aggregates
            .get(&definition)
            .cloned()
            .ok_or_else(|| {
                CompileError::unknown_type(self.definition_name(definition), self.function_span)
            })?;
        let type_name = self
            .package
            .definition(definition)
            .map(|definition| definition.name.clone())
            .ok_or_else(|| {
                CompileError::unknown_type(self.definition_name(definition), self.function_span)
            })?;
        if aggregate.fields.is_empty() {
            let text = aggregate_empty_format(&type_name);
            let string = self.string_indices.get(&text).copied().ok_or_else(|| {
                CompileError::unknown_name("aggregate format string".into(), span)
            })?;
            self.push(
                Instruction::LoadString {
                    dst: destination,
                    string,
                },
                span,
            );
            visiting.remove(&definition);
            return Ok(());
        }

        let part_count = aggregate
            .fields
            .len()
            .checked_mul(2)
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| CompileError::too_many_registers(span))?;
        let parts_base = self.reserve_types(&vec![ValueType::String; part_count])?;
        for (index, field) in aggregate.fields.iter().enumerate() {
            let field_definition = self.package.definition(field.definition).ok_or_else(|| {
                CompileError::unknown_name(
                    format!("definition#{}", field.definition.0),
                    self.function_span,
                )
            })?;
            let prefix = aggregate_field_format(&type_name, &field_definition.name, index == 0);
            let prefix_register = parts_base
                .checked_add(
                    u16::try_from(index.saturating_mul(2))
                        .map_err(|_| CompileError::too_many_registers(span))?,
                )
                .ok_or_else(|| CompileError::too_many_registers(span))?;
            let prefix_string = self.string_indices.get(&prefix).copied().ok_or_else(|| {
                CompileError::unknown_name("aggregate format string".into(), span)
            })?;
            self.push(
                Instruction::LoadString {
                    dst: prefix_register,
                    string: prefix_string,
                },
                span,
            );

            let field_register = self.allocate(field.ty)?;
            self.push(
                match aggregate.kind {
                    TypedAggregateKind::Struct => Instruction::StructGet {
                        source,
                        field: field.stable_id,
                        dst: field_register,
                    },
                    TypedAggregateKind::Class => Instruction::ClassGet {
                        source,
                        field: field.stable_id,
                        dst: field_register,
                    },
                },
                span,
            );
            let value_register = prefix_register
                .checked_add(1)
                .ok_or_else(|| CompileError::too_many_registers(span))?;
            self.emit_formattable_register(
                &field_definition.ty,
                field_register,
                value_register,
                span,
                visiting,
            )?;
        }
        let suffix_register = parts_base
            .checked_add(
                u16::try_from(part_count.saturating_sub(1))
                    .map_err(|_| CompileError::too_many_registers(span))?,
            )
            .ok_or_else(|| CompileError::too_many_registers(span))?;
        let suffix = self
            .string_indices
            .get(AGGREGATE_FORMAT_SUFFIX)
            .copied()
            .ok_or_else(|| CompileError::unknown_name("aggregate format suffix".into(), span))?;
        self.push(
            Instruction::LoadString {
                dst: suffix_register,
                string: suffix,
            },
            span,
        );
        self.push(
            Instruction::StringBuild {
                dst: destination,
                parts_base,
                parts_count: u16::try_from(part_count)
                    .map_err(|_| CompileError::too_many_registers(span))?,
            },
            span,
        );
        visiting.remove(&definition);
        Ok(())
    }

    fn emit_short_circuit(
        &mut self,
        operator: BinaryOperator,
        left: &TypedExpressionIr,
        right: &TypedExpressionIr,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        self.emit_expression(left, destination)?;
        if operator == BinaryOperator::And {
            let skip = self.push(
                Instruction::JumpIfFalse {
                    condition: destination,
                    target: 0,
                },
                span,
            );
            self.emit_expression(right, destination)?;
            self.patch_target(skip, self.position())?;
        } else {
            let false_register = self.allocate(ValueType::Bool)?;
            self.push(
                Instruction::LoadBool {
                    dst: false_register,
                    value: false,
                },
                span,
            );
            let is_false = self.allocate(ValueType::Bool)?;
            self.push(
                Instruction::CompareEq {
                    dst: is_false,
                    lhs: destination,
                    rhs: false_register,
                },
                span,
            );
            let evaluate_right = self.push(
                Instruction::JumpIfFalse {
                    condition: is_false,
                    target: 0,
                },
                span,
            );
            self.emit_expression(right, destination)?;
            self.patch_target(evaluate_right, self.position())?;
        }
        Ok(())
    }

    fn invert_bool(&mut self, register: u16, span: SourceSpan) -> Result<(), CompileError> {
        let false_register = self.allocate(ValueType::Bool)?;
        self.push(
            Instruction::LoadBool {
                dst: false_register,
                value: false,
            },
            span,
        );
        self.push(
            Instruction::CompareEq {
                dst: register,
                lhs: register,
                rhs: false_register,
            },
            span,
        );
        Ok(())
    }

    fn emit_literal(
        &mut self,
        literal: &IrLiteral,
        destination: u16,
        span: SourceSpan,
    ) -> Result<(), CompileError> {
        let instruction = match literal {
            IrLiteral::Unit => return Ok(()),
            IrLiteral::Bool(value) => Instruction::LoadBool {
                dst: destination,
                value: *value,
            },
            IrLiteral::I32(value) => Instruction::LoadI32 {
                dst: destination,
                value: *value,
            },
            IrLiteral::I64(value) => Instruction::LoadI64 {
                dst: destination,
                value: *value,
            },
            IrLiteral::F32(value) => Instruction::LoadF32 {
                dst: destination,
                bits: value.to_bits(),
            },
            IrLiteral::F64(value) => Instruction::LoadF64 {
                dst: destination,
                bits: value.to_bits(),
            },
            IrLiteral::String(value) => Instruction::LoadString {
                dst: destination,
                string: *self.string_indices.get(value).ok_or_else(|| {
                    CompileError::unknown_name("missing typed string literal".into(), span)
                })?,
            },
            IrLiteral::Rune(value) => Instruction::LoadRune {
                dst: destination,
                value: u32::from(*value),
            },
        };
        self.push(instruction, span);
        Ok(())
    }

    fn emit_numeric_zero(&mut self, ty: TypedNumericKind, destination: u16, span: SourceSpan) {
        self.push(ty.zero(destination), span);
    }

    /// Reserves the packed physical ABI range of an ordinary Nexa call.
    ///
    /// Every logical argument owns its complete `ValueLayout` range. The
    /// returned register list contains each logical base while `slots` is
    /// the exact contiguous width encoded in `Call.args_count` or
    /// `StandardIntrinsic.args_count`.
    fn reserve_physical_arguments(
        &mut self,
        arguments: &[TypedExpressionIr],
    ) -> Result<(u16, Vec<u16>, u16), CompileError> {
        let base = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        let mut registers = Vec::with_capacity(arguments.len());
        for argument in arguments {
            registers.push(self.allocate_expression(argument)?);
        }
        let end = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        let slots = end
            .checked_sub(base)
            .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
        Ok((base, registers, slots))
    }

    fn reserve_types(&mut self, types: &[ValueType]) -> Result<u16, CompileError> {
        let base = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        for ty in types {
            self.allocate_staging(*ty)?;
        }
        Ok(base)
    }

    fn reserve_physical_types(
        &mut self,
        types: &[ValueType],
    ) -> Result<(u16, Vec<u16>, u16), CompileError> {
        let base = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        let mut registers = Vec::with_capacity(types.len());
        for ty in types {
            registers.push(self.allocate(*ty)?);
        }
        let end = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        let slots = end
            .checked_sub(base)
            .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
        Ok((base, registers, slots))
    }

    /// Evaluates a scalar-replaced class construction directly into its
    /// inline field register range.
    fn emit_inline_struct_fields(
        &mut self,
        owner: DefinitionId,
        fields_base: u16,
        fields: &[(DefinitionId, TypedExpressionIr)],
        value: &TypedExpressionIr,
    ) -> Result<(), CompileError> {
        let span = self.span(&value.span)?;
        let layout = self
            .layouts
            .aggregates
            .get(&owner)
            .cloned()
            .ok_or_else(|| CompileError::unknown_type(self.definition_name(owner), span))?;
        let values = fields
            .iter()
            .map(|(field, value)| (*field, value))
            .collect::<BTreeMap<_, _>>();
        if values.len() != fields.len() || values.len() != layout.fields.len() {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        for field in &layout.fields {
            let value = values
                .get(&field.definition)
                .copied()
                .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
            let register =
                self.inline_field_register(owner, fields_base, field.definition, span)?;
            self.emit_expression(value, register)?;
        }
        Ok(())
    }

    /// Register carrying one field of a scalar-replaced class binding.
    fn inline_field_register(
        &self,
        owner: DefinitionId,
        fields_base: u16,
        field: DefinitionId,
        span: SourceSpan,
    ) -> Result<u16, CompileError> {
        let layout = self
            .layouts
            .aggregates
            .get(&owner)
            .ok_or_else(|| CompileError::unknown_type(self.definition_name(owner), span))?;
        let mut offset = 0_u16;
        let mut found = false;
        for candidate in &layout.fields {
            if candidate.definition == field {
                found = true;
                break;
            }
            offset = offset
                .checked_add(self.layouts.physical_slots(candidate.ty, span)?)
                .ok_or_else(|| CompileError::too_many_registers(span))?;
        }
        if !found {
            return Err(CompileError::unknown_name(
                self.definition_name(field),
                span,
            ));
        }
        fields_base
            .checked_add(offset)
            .ok_or_else(|| CompileError::too_many_registers(span))
    }

    fn allocate_expression(&mut self, expression: &TypedExpressionIr) -> Result<u16, CompileError> {
        let span = self.span(&expression.span)?;
        let ty = lower_type(self.package, &expression.ty, span)?;
        self.allocate(if expression.ty == IrType::Unit {
            ValueType::I32
        } else {
            ty
        })
    }

    fn allocate(&mut self, ty: ValueType) -> Result<u16, CompileError> {
        let register = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        self.register_types.push(Some(ty));
        let physical_slots = self.layouts.physical_slots(ty, self.function_span)?;
        self.register_types
            .extend((1..physical_slots).map(|_| None));
        Ok(register)
    }

    fn allocate_staging(&mut self, ty: ValueType) -> Result<u16, CompileError> {
        let register = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        self.register_types.push(Some(ty));
        Ok(register)
    }

    fn copy_value_instruction(
        &self,
        ty: ValueType,
        dst: u16,
        source: u16,
        span: SourceSpan,
    ) -> Result<Instruction, CompileError> {
        let slots = self.layouts.physical_slots(ty, span)?;
        Ok(if slots == 1 {
            Instruction::Move { dst, source }
        } else {
            Instruction::CopyValue { dst, source, slots }
        })
    }

    fn local(&self, definition: DefinitionId) -> Result<u16, CompileError> {
        self.locals.get(&definition).copied().ok_or_else(|| {
            CompileError::unknown_name(self.definition_name(definition), self.function_span)
        })
    }

    fn definition_name(&self, definition: DefinitionId) -> String {
        self.package.definition(definition).map_or_else(
            || format!("definition#{}", definition.0),
            |definition| {
                format!(
                    "{}::{}::{}",
                    definition.package_id, definition.module, definition.name
                )
            },
        )
    }

    fn span(&self, range: &SourceRange) -> Result<SourceSpan, CompileError> {
        source_span(range, self.files)
    }

    fn position(&self) -> u32 {
        u32::try_from(self.code.len()).unwrap_or(u32::MAX)
    }

    fn push(&mut self, instruction: Instruction, span: SourceSpan) -> usize {
        let index = self.code.len();
        self.code.push(instruction);
        self.spans.push(span);
        index
    }

    fn patch_target(&mut self, instruction: usize, target: u32) -> Result<(), CompileError> {
        match self.code.get_mut(instruction) {
            Some(
                Instruction::Jump {
                    target: destination,
                }
                | Instruction::JumpIfFalse {
                    target: destination,
                    ..
                },
            ) => {
                *destination = target;
                Ok(())
            }
            _ => Err(CompileError::verify(
                "typed codegen branch patch invariant violated".into(),
                self.function_span,
            )),
        }
    }

    fn finish(
        mut self,
        signature: Signature,
        effect: FunctionEffect,
        function_index: u32,
    ) -> Result<(Function, Vec<SourceMapEntry>), CompileError> {
        if self.optimize {
            optimize_emitted_bytecode(&mut self.code, &mut self.spans, &mut self.loop_bounds);
        }
        let registers = u16::try_from(self.register_types.len().max(1))
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        while self.register_types.len() < usize::from(registers) {
            self.register_types.push(None);
        }
        let safepoints = collect_safepoints(&self.code);
        let (root_bitmap, root_maps) = typed_exact_root_maps(
            &self.register_types,
            Some(&self.layouts.layout_table),
            self.parameter_slots,
            &self.code,
            &safepoints,
            self.function_span,
        )?;
        let source_map = self
            .spans
            .into_iter()
            .enumerate()
            .map(|(pc, span)| SourceMapEntry {
                function: function_index,
                pc_start: u32::try_from(pc).unwrap_or(u32::MAX),
                pc_end: u32::try_from(pc.saturating_add(1)).unwrap_or(u32::MAX),
                span,
            })
            .collect();
        Ok((
            Function {
                signature,
                parameter_slots: u16::try_from(self.parameter_slots)
                    .map_err(|_| CompileError::too_many_registers(self.function_span))?,
                registers,
                frame_bytes: u32::from(registers).saturating_mul(8),
                root_bitmap,
                root_maps,
                safepoints,
                loop_bounds: self.loop_bounds,
                effect,
                max_static_call_depth: 1,
                code: self.code,
            },
            source_map,
        ))
    }
}

fn collect_string_concat_parts<'a>(
    expression: &'a TypedExpressionIr,
    parts: &mut Vec<&'a TypedExpressionIr>,
) {
    if let TypedExpressionKind::Binary {
        operator: BinaryOperator::Add,
        left,
        right,
    } = &expression.kind
        && expression.ty == IrType::String
    {
        collect_string_concat_parts(left, parts);
        collect_string_concat_parts(right, parts);
    } else {
        parts.push(expression);
    }
}

fn validate_builtin_arguments(
    arguments: &[TypedExpressionIr],
    expected: &[IrType],
    span: SourceSpan,
) -> Result<(), CompileError> {
    if arguments.len() != expected.len()
        || arguments
            .iter()
            .zip(expected)
            .any(|(actual, expected)| actual.ty != *expected)
    {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    Ok(())
}

fn validate_builtin_result(
    result: &IrType,
    expected: &IrType,
    span: SourceSpan,
) -> Result<(), CompileError> {
    if result != expected {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_builtin_call_signature(
    package: &TypedPackageIr,
    operation: BuiltinOperationIr,
    type_arguments: &[IrType],
    arguments: &[TypedExpressionIr],
    result: &IrType,
    span: SourceSpan,
) -> Result<(), CompileError> {
    if type_arguments.iter().any(ir_type_contains_type_parameter) {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    match operation {
        BuiltinOperationIr::ArrayNew => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(arguments, &[], span)?;
            validate_builtin_result(result, &IrType::Array(Box::new(element.clone())), span)
        }
        BuiltinOperationIr::MapNew => {
            let [key, value] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(arguments, &[], span)?;
            validate_builtin_result(
                result,
                &IrType::Map(Box::new(key.clone()), Box::new(value.clone())),
                span,
            )
        }
        BuiltinOperationIr::StringLen | BuiltinOperationIr::StringByteLen => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String], span)?;
            validate_builtin_result(result, &IrType::I32, span)
        }
        BuiltinOperationIr::StringEqual
        | BuiltinOperationIr::StringContains
        | BuiltinOperationIr::StringStartsWith
        | BuiltinOperationIr::StringEndsWith => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String, IrType::String], span)?;
            validate_builtin_result(result, &IrType::Bool, span)
        }
        BuiltinOperationIr::StringConcat => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String, IrType::String], span)?;
            validate_builtin_result(result, &IrType::String, span)
        }
        BuiltinOperationIr::StringRuneAt => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String, IrType::I32], span)?;
            validate_builtin_result(result, &IrType::Rune, span)
        }
        BuiltinOperationIr::StringHash => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String], span)?;
            validate_builtin_result(result, &IrType::I64, span)
        }
        BuiltinOperationIr::ArrayLen
        | BuiltinOperationIr::ArrayGet
        | BuiltinOperationIr::ArrayTryGet
        | BuiltinOperationIr::ArraySet
        | BuiltinOperationIr::ArrayPush
        | BuiltinOperationIr::ArrayPop
        | BuiltinOperationIr::ArrayInsert
        | BuiltinOperationIr::ArrayRemove
        | BuiltinOperationIr::ArrayClear
        | BuiltinOperationIr::ArrayReserve
        | BuiltinOperationIr::ArrayCapacity
        | BuiltinOperationIr::ArrayShrinkToFit
        | BuiltinOperationIr::ArrayFirst
        | BuiltinOperationIr::ArrayLast
        | BuiltinOperationIr::ArraySwap
        | BuiltinOperationIr::ArrayReverse => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let array = IrType::Array(Box::new(element.clone()));
            match operation {
                BuiltinOperationIr::ArrayLen | BuiltinOperationIr::ArrayCapacity => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::ArrayGet | BuiltinOperationIr::ArrayRemove => {
                    validate_builtin_arguments(arguments, &[array, IrType::I32], span)?;
                    validate_builtin_result(result, element, span)
                }
                BuiltinOperationIr::ArrayTryGet => {
                    validate_builtin_arguments(arguments, &[array, IrType::I32], span)?;
                    validate_builtin_result(
                        result,
                        &IrType::Option(Box::new(element.clone())),
                        span,
                    )
                }
                BuiltinOperationIr::ArraySet | BuiltinOperationIr::ArrayInsert => {
                    validate_builtin_arguments(
                        arguments,
                        &[array, IrType::I32, element.clone()],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::ArrayPush => {
                    validate_builtin_arguments(arguments, &[array, element.clone()], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::ArrayPop => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(result, element, span)
                }
                BuiltinOperationIr::ArrayClear
                | BuiltinOperationIr::ArrayShrinkToFit
                | BuiltinOperationIr::ArrayReverse => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::ArrayReserve => {
                    validate_builtin_arguments(arguments, &[array, IrType::I32], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::ArrayFirst | BuiltinOperationIr::ArrayLast => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(
                        result,
                        &IrType::Option(Box::new(element.clone())),
                        span,
                    )
                }
                BuiltinOperationIr::ArraySwap => {
                    validate_builtin_arguments(
                        arguments,
                        &[array, IrType::I32, IrType::I32],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("array operations are matched above"),
            }
        }
        BuiltinOperationIr::MapLen
        | BuiltinOperationIr::MapGet
        | BuiltinOperationIr::MapSet
        | BuiltinOperationIr::MapIsEmpty
        | BuiltinOperationIr::MapGetOr
        | BuiltinOperationIr::MapInsertIfAbsent
        | BuiltinOperationIr::MapRemove
        | BuiltinOperationIr::MapContains
        | BuiltinOperationIr::MapClear => {
            let [key, value] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let map = IrType::Map(Box::new(key.clone()), Box::new(value.clone()));
            match operation {
                BuiltinOperationIr::MapLen => {
                    validate_builtin_arguments(arguments, &[map], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::MapIsEmpty | BuiltinOperationIr::MapClear => {
                    validate_builtin_arguments(arguments, &[map], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::MapGet | BuiltinOperationIr::MapRemove => {
                    validate_builtin_arguments(arguments, &[map, key.clone()], span)?;
                    validate_builtin_result(result, &IrType::Option(Box::new(value.clone())), span)
                }
                BuiltinOperationIr::MapSet | BuiltinOperationIr::MapInsertIfAbsent => {
                    validate_builtin_arguments(
                        arguments,
                        &[map, key.clone(), value.clone()],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::MapGetOr => {
                    validate_builtin_arguments(
                        arguments,
                        &[map, key.clone(), value.clone()],
                        span,
                    )?;
                    validate_builtin_result(result, value, span)
                }
                BuiltinOperationIr::MapContains => {
                    validate_builtin_arguments(arguments, &[map, key.clone()], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("map operations are matched above"),
            }
        }
        BuiltinOperationIr::SetNew
        | BuiltinOperationIr::SetLen
        | BuiltinOperationIr::SetContains
        | BuiltinOperationIr::SetInsert
        | BuiltinOperationIr::SetRemove
        | BuiltinOperationIr::SetClear => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let set = IrType::Set(Box::new(element.clone()));
            match operation {
                BuiltinOperationIr::SetNew => {
                    validate_builtin_arguments(arguments, &[], span)?;
                    validate_builtin_result(result, &set, span)
                }
                BuiltinOperationIr::SetLen => {
                    validate_builtin_arguments(arguments, &[set], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::SetContains
                | BuiltinOperationIr::SetInsert
                | BuiltinOperationIr::SetRemove => {
                    validate_builtin_arguments(arguments, &[set, element.clone()], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::SetClear => {
                    validate_builtin_arguments(arguments, &[set], span)?;
                    validate_builtin_result(result, &IrType::Unit, span)
                }
                _ => unreachable!("set operations are matched above"),
            }
        }
        BuiltinOperationIr::BufferLen
        | BuiltinOperationIr::BufferIsEmpty
        | BuiltinOperationIr::BufferGet
        | BuiltinOperationIr::BufferSet
        | BuiltinOperationIr::BufferSlice
        | BuiltinOperationIr::BufferCopy
        | BuiltinOperationIr::BufferFill => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let buffer = IrType::Buffer(Box::new(element.clone()));
            match operation {
                BuiltinOperationIr::BufferLen => {
                    validate_builtin_arguments(arguments, &[buffer], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::BufferIsEmpty => {
                    validate_builtin_arguments(arguments, &[buffer], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::BufferGet => {
                    validate_builtin_arguments(arguments, &[buffer, IrType::I32], span)?;
                    validate_builtin_result(result, element, span)
                }
                BuiltinOperationIr::BufferSet => {
                    validate_builtin_arguments(
                        arguments,
                        &[buffer, IrType::I32, element.clone()],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::BufferSlice => {
                    validate_builtin_arguments(
                        arguments,
                        &[buffer.clone(), IrType::I32, IrType::I32],
                        span,
                    )?;
                    validate_builtin_result(result, &buffer, span)
                }
                BuiltinOperationIr::BufferCopy => {
                    validate_builtin_arguments(
                        arguments,
                        &[
                            buffer.clone(),
                            buffer,
                            IrType::I32,
                            IrType::I32,
                            IrType::I32,
                        ],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::BufferFill => {
                    validate_builtin_arguments(arguments, &[buffer, element.clone()], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("buffer operations are matched above"),
            }
        }
        BuiltinOperationIr::StateHandleResolve
        | BuiltinOperationIr::StateHandleIsAlive
        | BuiltinOperationIr::StateHandleStableId
        | BuiltinOperationIr::StateHandleGeneration
        | BuiltinOperationIr::StateHandleEqual
        | BuiltinOperationIr::StateHandleHash => {
            let [target] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let handle = IrType::StateHandle(Box::new(target.clone()));
            match operation {
                BuiltinOperationIr::StateHandleResolve => {
                    validate_builtin_arguments(arguments, &[handle], span)?;
                    let target = lower_type(package, target, span)?;
                    let expected = ValueType::Named(
                        result_type(
                            target,
                            ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
                        )
                        .type_id,
                    );
                    if lower_type(package, result, span)? != expected {
                        return Err(CompileError::type_mismatch(None, None, span));
                    }
                    Ok(())
                }
                BuiltinOperationIr::StateHandleIsAlive => {
                    validate_builtin_arguments(arguments, &[handle], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::StateHandleStableId => {
                    validate_builtin_arguments(arguments, &[handle], span)?;
                    if lower_type(package, result, span)? != nexa_bytecode::stable_id_type() {
                        return Err(CompileError::type_mismatch(None, None, span));
                    }
                    Ok(())
                }
                BuiltinOperationIr::StateHandleGeneration | BuiltinOperationIr::StateHandleHash => {
                    validate_builtin_arguments(arguments, &[handle], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::StateHandleEqual => {
                    validate_builtin_arguments(arguments, &[handle.clone(), handle], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("state-handle operations are matched above"),
            }
        }
        BuiltinOperationIr::StringSubstring => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(
                arguments,
                &[IrType::String, IrType::I32, IrType::I32],
                span,
            )?;
            validate_builtin_result(result, &IrType::String, span)
        }
        BuiltinOperationIr::StringTrim => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String], span)?;
            validate_builtin_result(result, &IrType::String, span)
        }
        BuiltinOperationIr::StringSplit => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            validate_builtin_arguments(arguments, &[IrType::String, IrType::String], span)?;
            validate_builtin_result(result, &IrType::Array(Box::new(IrType::String)), span)
        }
        BuiltinOperationIr::ArrayIsEmpty => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[IrType::Array(Box::new(element.clone()))],
                span,
            )?;
            validate_builtin_result(result, &IrType::Bool, span)
        }
        BuiltinOperationIr::MapInsert => {
            let [key, value] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[
                    IrType::Map(Box::new(key.clone()), Box::new(value.clone())),
                    key.clone(),
                    value.clone(),
                ],
                span,
            )?;
            validate_builtin_result(result, &IrType::Bool, span)
        }
        BuiltinOperationIr::OptionIsSome | BuiltinOperationIr::OptionIsNone => {
            let [inner] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[IrType::Option(Box::new(inner.clone()))],
                span,
            )?;
            validate_builtin_result(result, &IrType::Bool, span)
        }
        BuiltinOperationIr::OptionUnwrapOr => {
            let [inner] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[IrType::Option(Box::new(inner.clone())), inner.clone()],
                span,
            )?;
            validate_builtin_result(result, inner, span)
        }
        BuiltinOperationIr::ResultIsOk | BuiltinOperationIr::ResultIsErr => {
            let [ok, error] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[IrType::Result(
                    Box::new(ok.clone()),
                    Box::new(error.clone()),
                )],
                span,
            )?;
            validate_builtin_result(result, &IrType::Bool, span)
        }
        BuiltinOperationIr::ResultUnwrapOr => {
            let [ok, error] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(
                arguments,
                &[
                    IrType::Result(Box::new(ok.clone()), Box::new(error.clone())),
                    ok.clone(),
                ],
                span,
            )?;
            validate_builtin_result(result, ok, span)
        }
        BuiltinOperationIr::StringToString
        | BuiltinOperationIr::I32ToString
        | BuiltinOperationIr::I64ToString
        | BuiltinOperationIr::F32ToString
        | BuiltinOperationIr::F64ToString
        | BuiltinOperationIr::BoolToString
        | BuiltinOperationIr::RuneToString => {
            if !type_arguments.is_empty() {
                return Err(CompileError::type_mismatch(None, None, span));
            }
            let source = match operation {
                BuiltinOperationIr::StringToString => IrType::String,
                BuiltinOperationIr::I32ToString => IrType::I32,
                BuiltinOperationIr::I64ToString => IrType::I64,
                BuiltinOperationIr::F32ToString => IrType::F32,
                BuiltinOperationIr::F64ToString => IrType::F64,
                BuiltinOperationIr::BoolToString => IrType::Bool,
                BuiltinOperationIr::RuneToString => IrType::Rune,
                _ => unreachable!("scalar to-string operations are matched above"),
            };
            validate_builtin_arguments(arguments, &[source], span)?;
            validate_builtin_result(result, &IrType::String, span)
        }
        BuiltinOperationIr::ValueToString => {
            let [value] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            validate_builtin_arguments(arguments, std::slice::from_ref(value), span)?;
            validate_builtin_result(result, &IrType::String, span)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn typed_exact_root_maps(
    register_types: &[Option<ValueType>],
    layout_table: Option<&LayoutTable>,
    parameter_count: usize,
    code: &[Instruction],
    safepoints: &[u32],
    span: SourceSpan,
) -> Result<(Vec<bool>, Vec<RootMap>), CompileError> {
    use std::collections::VecDeque;

    let register_count = register_types.len();
    let mut entry = vec![false; register_count];
    for register in 0..parameter_count {
        entry[register] = register_types[register].is_some();
    }

    let mut successors = vec![Vec::new(); code.len()];
    for (pc, instruction) in code.iter().copied().enumerate() {
        match instruction {
            Instruction::Jump { target } => {
                successors[pc].push(usize::try_from(target).unwrap_or(usize::MAX));
            }
            Instruction::JumpIfFalse { target, .. } => {
                successors[pc].push(usize::try_from(target).unwrap_or(usize::MAX));
                if pc + 1 < code.len() {
                    successors[pc].push(pc + 1);
                }
            }
            Instruction::Return { .. }
            | Instruction::ReturnVoid
            | Instruction::CleanupReturn
            | Instruction::Trap => {}
            _ if pc + 1 < code.len() => successors[pc].push(pc + 1),
            _ => {}
        }
        if successors[pc]
            .iter()
            .any(|successor| *successor >= code.len())
        {
            return Err(CompileError::verify(
                "typed emitter produced an out-of-range control-flow target".into(),
                span,
            ));
        }
    }

    let mut states = vec![None; code.len()];
    let mut queue = VecDeque::new();
    if !code.is_empty() {
        states[0] = Some(entry);
        queue.push_back(0_usize);
    }
    while let Some(pc) = queue.pop_front() {
        let Some(mut state) = states[pc].clone() else {
            continue;
        };
        for destination in typed_instruction_destinations(code[pc]) {
            let destination = usize::from(destination);
            if destination >= register_count {
                return Err(CompileError::verify(
                    "typed emitter produced an out-of-range destination register".into(),
                    span,
                ));
            }
            if register_types[destination].is_some() {
                state[destination] = true;
            }
        }

        for &successor in &successors[pc] {
            match &mut states[successor] {
                None => {
                    states[successor] = Some(state.clone());
                    queue.push_back(successor);
                }
                Some(existing) => {
                    let mut changed = false;
                    for (current, incoming) in existing.iter_mut().zip(&state) {
                        if *current && !incoming {
                            *current = false;
                            changed = true;
                        }
                    }
                    if changed {
                        queue.push_back(successor);
                    }
                }
            }
        }
    }

    // Root maps describe values that are both definitely initialized on every
    // path reaching the safepoint and actually live before its instruction.
    // Definite initialization alone is intentionally insufficient: expression
    // temporaries and locals after their final use must not retain heap objects
    // across a suspension or allocation safepoint.
    let mut live_before = vec![vec![false; register_count]; code.len()];
    loop {
        let mut changed = false;
        for pc in (0..code.len()).rev() {
            if states[pc].is_none() {
                continue;
            }
            let mut live = vec![false; register_count];
            for &successor in &successors[pc] {
                if states[successor].is_some() {
                    for (current, incoming) in
                        live.iter_mut().zip(live_before[successor].iter().copied())
                    {
                        *current |= incoming;
                    }
                }
            }
            for destination in typed_instruction_liveness_destinations(code[pc]) {
                let destination = usize::from(destination);
                if destination >= register_count {
                    return Err(CompileError::verify(
                        "typed emitter produced an out-of-range destination register".into(),
                        span,
                    ));
                }
                live[destination] = false;
            }
            for source in typed_instruction_sources(code[pc]) {
                let source = usize::from(source);
                if source >= register_count {
                    return Err(CompileError::verify(
                        "typed emitter produced an out-of-range source register".into(),
                        span,
                    ));
                }
                live[source] = true;
            }
            if live_before[pc] != live {
                live_before[pc] = live;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut root_bitmap = vec![false; register_count];
    for state in states.iter().flatten() {
        merge_physical_roots(&mut root_bitmap, register_types, layout_table, |base| {
            state[base]
        })?;
    }
    let root_maps = safepoints
        .iter()
        .map(|pc| {
            let pc_index = usize::try_from(*pc).unwrap_or(usize::MAX);
            let mut bitmap = vec![false; register_count];
            if let Some(state) = states.get(pc_index).and_then(Option::as_ref) {
                merge_physical_roots(&mut bitmap, register_types, layout_table, |base| {
                    state[base] && live_before[pc_index][base]
                })?;
            }
            Ok(RootMap { pc: *pc, bitmap })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    Ok((root_bitmap, root_maps))
}

fn merge_physical_roots(
    roots: &mut [bool],
    register_types: &[Option<ValueType>],
    layout_table: Option<&LayoutTable>,
    active: impl Fn(usize) -> bool,
) -> Result<(), CompileError> {
    for (base, ty) in register_types.iter().copied().enumerate() {
        let Some(ty) = ty.filter(|_| active(base)) else {
            continue;
        };
        let Some(layout_table) = layout_table else {
            roots[base] |= ty.is_reference();
            continue;
        };
        let layout = layout_table
            .layout_of(ty)
            .map_err(|error| CompileError::verify(error.to_string(), SourceSpan::default()))?;
        let end = base
            .checked_add(usize::from(layout.physical_slots))
            .ok_or_else(|| {
                CompileError::verify("physical root range overflow".into(), SourceSpan::default())
            })?;
        let owns_full_range = end <= register_types.len()
            && register_types[base + 1..end].iter().all(Option::is_none);
        let physical_aggregate = matches!(
            layout.equality_strategy,
            nexa_bytecode::layout::EqualityStrategy::StructFieldwise
                | nexa_bytecode::layout::EqualityStrategy::EnumTagPayload
        ) && owns_full_range;
        if physical_aggregate || layout.physical_slots == 1 {
            for (offset, is_root) in layout.gc_bitmap.iter().copied().enumerate() {
                if is_root {
                    roots[base + offset] = true;
                }
            }
        } else {
            // Compact boundary staging uses one carrier slot until the
            // corresponding persistent/Host codec consumes a physical range.
            roots[base] |= ty.is_reference();
        }
    }
    Ok(())
}

/// Removes codegen-only register traffic after structured lowering.
///
/// This pass is deliberately narrower than a general bytecode optimizer:
/// aliases never cross a basic-block boundary, a source register may not be
/// overwritten before the last rewritten use, and destinations used outside
/// the block remain materialized. It therefore needs no SSA repair or phi
/// nodes. The second phase removes only dead scalar constants and moves;
/// instructions that can allocate, trap, call, mutate, or publish a value are
/// never classified as dead-pure.
fn optimize_emitted_bytecode(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SourceSpan>,
    loop_bounds: &mut Vec<LoopBound>,
) {
    if code.is_empty() {
        return;
    }
    simplify_emitted_control_flow(code, spans, loop_bounds);
    let blocks = emitted_basic_blocks(code);
    let mut keep = vec![true; code.len()];
    for (start, end) in blocks {
        for pc in start..end {
            let Instruction::Move { dst, source } = code[pc] else {
                continue;
            };
            let used_outside = code.iter().enumerate().any(|(candidate, instruction)| {
                (candidate < start || candidate >= end)
                    && typed_instruction_sources(*instruction).contains(&dst)
            });
            if used_outside {
                continue;
            }
            let last_use = (pc + 1..end)
                .rev()
                .find(|candidate| typed_instruction_sources(code[*candidate]).contains(&dst));
            let Some(last_use) = last_use else {
                keep[pc] = false;
                continue;
            };
            if (pc + 1..=last_use).any(|candidate| {
                typed_instruction_destinations(code[candidate])
                    .iter()
                    .any(|written| *written == source || (*written == dst && candidate != last_use))
            }) {
                continue;
            }
            if !(pc + 1..=last_use).all(|candidate| {
                let mut instruction = code[candidate];
                rewrite_emitted_source(&mut instruction, dst, source)
            }) {
                continue;
            }
            for instruction in &mut code[pc + 1..=last_use] {
                let rewritten = rewrite_emitted_source(instruction, dst, source);
                debug_assert!(rewritten);
            }
            keep[pc] = false;
        }
    }
    compact_emitted_bytecode(code, spans, loop_bounds, &keep);

    loop {
        let register_count = code
            .iter()
            .flat_map(|instruction| {
                typed_instruction_sources(*instruction)
                    .into_iter()
                    .chain(typed_instruction_destinations(*instruction))
            })
            .map(usize::from)
            .max()
            .map_or(0, |maximum| maximum + 1);
        let mut used = vec![false; register_count];
        for instruction in code.iter().copied() {
            for source in typed_instruction_sources(instruction) {
                if let Some(slot) = used.get_mut(usize::from(source)) {
                    *slot = true;
                }
            }
        }
        let keep = code
            .iter()
            .copied()
            .map(|instruction| {
                let destinations = typed_instruction_destinations(instruction);
                let dead_destination = !destinations.is_empty()
                    && destinations
                        .iter()
                        .all(|dst| !used.get(usize::from(*dst)).copied().unwrap_or(false));
                !(dead_destination && emitted_instruction_is_dead_pure(instruction))
            })
            .collect::<Vec<_>>();
        if keep.iter().all(|keep| *keep) {
            break;
        }
        compact_emitted_bytecode(code, spans, loop_bounds, &keep);
    }
    let forwarded = forward_adjacent_class_fields(code);
    if simplify_emitted_control_flow(code, spans, loop_bounds) || forwarded {
        optimize_emitted_bytecode(code, spans, loop_bounds);
    }
}

#[cfg(test)]
fn emitted_register_count(code: &[Instruction], parameter_count: usize) -> usize {
    code.iter()
        .copied()
        .flat_map(|instruction| {
            typed_instruction_sources(instruction)
                .into_iter()
                .chain(typed_instruction_destinations(instruction))
        })
        .map(usize::from)
        .max()
        .map_or(parameter_count, |maximum| {
            parameter_count.max(maximum.saturating_add(1))
        })
        .max(1)
}

fn simplify_emitted_control_flow(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SourceSpan>,
    loop_bounds: &mut Vec<LoopBound>,
) -> bool {
    if code.is_empty() {
        return false;
    }
    let mut reachable = vec![false; code.len()];
    let mut pending = vec![0_usize];
    while let Some(pc) = pending.pop() {
        if pc >= code.len() || std::mem::replace(&mut reachable[pc], true) {
            continue;
        }
        match code[pc] {
            Instruction::Jump { target } => {
                pending.push(usize::try_from(target).unwrap_or(code.len()));
            }
            Instruction::JumpIfFalse { target, .. } => {
                pending.push(pc + 1);
                pending.push(usize::try_from(target).unwrap_or(code.len()));
            }
            Instruction::Return { .. }
            | Instruction::ReturnVoid
            | Instruction::CleanupReturn
            | Instruction::Trap => {}
            _ => pending.push(pc + 1),
        }
    }
    let mut changed = reachable.iter().any(|reachable| !reachable);
    if changed {
        compact_emitted_bytecode(code, spans, loop_bounds, &reachable);
    }
    let keep = code
        .iter()
        .copied()
        .enumerate()
        .map(|(pc, instruction)| {
            !matches!(instruction, Instruction::Jump { target }
                if usize::try_from(target).ok() == Some(pc + 1))
        })
        .collect::<Vec<_>>();
    if keep.iter().any(|keep| !keep) {
        compact_emitted_bytecode(code, spans, loop_bounds, &keep);
        changed = true;
    }
    changed
}

fn forward_adjacent_class_fields(code: &mut [Instruction]) -> bool {
    let mut changed = false;
    for pc in 0..code.len().saturating_sub(1) {
        let Instruction::ClassSet {
            source,
            field,
            value,
        } = code[pc]
        else {
            continue;
        };
        let Instruction::ClassGet {
            source: read_source,
            field: read_field,
            dst,
        } = code[pc + 1]
        else {
            continue;
        };
        if source == read_source && field == read_field {
            code[pc + 1] = Instruction::Move { dst, source: value };
            changed = true;
        }
    }
    changed
}

fn emitted_basic_blocks(code: &[Instruction]) -> Vec<(usize, usize)> {
    let mut starts = BTreeSet::from([0_usize, code.len()]);
    for (pc, instruction) in code.iter().copied().enumerate() {
        match instruction {
            Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. } => {
                starts.insert(
                    usize::try_from(target)
                        .unwrap_or(code.len())
                        .min(code.len()),
                );
                starts.insert((pc + 1).min(code.len()));
            }
            Instruction::Return { .. }
            | Instruction::ReturnVoid
            | Instruction::CleanupReturn
            | Instruction::Trap
            | Instruction::Yield => {
                starts.insert((pc + 1).min(code.len()));
            }
            _ => {}
        }
    }
    let starts = starts.into_iter().collect::<Vec<_>>();
    starts
        .windows(2)
        .filter_map(|range| (range[0] < range[1]).then_some((range[0], range[1])))
        .collect()
}

fn compact_emitted_bytecode(
    code: &mut Vec<Instruction>,
    spans: &mut Vec<SourceSpan>,
    loop_bounds: &mut Vec<LoopBound>,
    keep: &[bool],
) {
    debug_assert_eq!(code.len(), spans.len());
    debug_assert_eq!(code.len(), keep.len());
    let mut boundary = vec![0_u32; code.len() + 1];
    let mut next = 0_u32;
    for (pc, retained) in keep.iter().copied().enumerate() {
        boundary[pc] = next;
        if retained {
            next = next.saturating_add(1);
        }
    }
    boundary[code.len()] = next;
    let old_code = std::mem::take(code);
    let old_spans = std::mem::take(spans);
    code.reserve(old_code.len());
    spans.reserve(old_spans.len());
    for (pc, (mut instruction, span)) in old_code.into_iter().zip(old_spans).enumerate() {
        if !keep[pc] {
            continue;
        }
        match &mut instruction {
            Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. } => {
                *target = boundary
                    .get(usize::try_from(*target).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or(next);
            }
            _ => {}
        }
        code.push(instruction);
        spans.push(span);
    }
    loop_bounds.retain_mut(|bound| {
        let old = usize::try_from(bound.back_edge).unwrap_or(usize::MAX);
        if !keep.get(old).copied().unwrap_or(false) {
            return false;
        }
        bound.back_edge = boundary.get(old).copied().unwrap_or(next);
        true
    });
}

const fn emitted_instruction_is_dead_pure(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadI32 { .. }
            | Instruction::LoadI64 { .. }
            | Instruction::LoadF32 { .. }
            | Instruction::LoadF64 { .. }
            | Instruction::LoadBool { .. }
            | Instruction::LoadRune { .. }
            | Instruction::Move { .. }
            | Instruction::Add { .. }
            | Instruction::Sub { .. }
            | Instruction::Mul { .. }
            | Instruction::AddI64 { .. }
            | Instruction::SubI64 { .. }
            | Instruction::MulI64 { .. }
            | Instruction::AddF32 { .. }
            | Instruction::SubF32 { .. }
            | Instruction::MulF32 { .. }
            | Instruction::AddF64 { .. }
            | Instruction::SubF64 { .. }
            | Instruction::MulF64 { .. }
            | Instruction::CompareEq { .. }
            | Instruction::CompareLtI32 { .. }
            | Instruction::CompareLtI64 { .. }
            | Instruction::CompareLtF32 { .. }
            | Instruction::CompareLtF64 { .. }
    )
}

/// Rewrites explicit scalar operands. Contiguous argument/field ranges are
/// intentionally rejected: replacing one member would destroy their ABI
/// contiguity, so the originating Move stays materialized.
#[allow(clippy::too_many_lines)]
fn rewrite_emitted_source(instruction: &mut Instruction, from: u16, to: u16) -> bool {
    if !typed_instruction_sources(*instruction).contains(&from) {
        return true;
    }
    let replace = |register: &mut u16| {
        if *register == from {
            *register = to;
        }
    };
    match instruction {
        Instruction::Move { source, .. }
        | Instruction::CopyValue { source, .. }
        | Instruction::StringLen { source, .. }
        | Instruction::StringByteLen { source, .. }
        | Instruction::StringHash { source, .. }
        | Instruction::I32ToString { source, .. }
        | Instruction::I64ToString { source, .. }
        | Instruction::F32ToString { source, .. }
        | Instruction::F64ToString { source, .. }
        | Instruction::BoolToString { source, .. }
        | Instruction::RuneToString { source, .. }
        | Instruction::StringToString { source, .. }
        | Instruction::EnumTag { source, .. }
        | Instruction::EnumPayload { source, .. }
        | Instruction::StructGet { source, .. }
        | Instruction::ClassGet { source, .. }
        | Instruction::ArrayLen { source, .. }
        | Instruction::ArrayPop { source, .. }
        | Instruction::ArrayClear { source }
        | Instruction::MapLen { source, .. }
        | Instruction::MapClear { source }
        | Instruction::BufferLen { source, .. }
        | Instruction::Return { source } => replace(source),
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::RemI32 { lhs, rhs, .. }
        | Instruction::AddI64 { lhs, rhs, .. }
        | Instruction::SubI64 { lhs, rhs, .. }
        | Instruction::MulI64 { lhs, rhs, .. }
        | Instruction::DivI64 { lhs, rhs, .. }
        | Instruction::RemI64 { lhs, rhs, .. }
        | Instruction::AddF32 { lhs, rhs, .. }
        | Instruction::SubF32 { lhs, rhs, .. }
        | Instruction::MulF32 { lhs, rhs, .. }
        | Instruction::DivF32 { lhs, rhs, .. }
        | Instruction::RemF32 { lhs, rhs, .. }
        | Instruction::AddF64 { lhs, rhs, .. }
        | Instruction::SubF64 { lhs, rhs, .. }
        | Instruction::MulF64 { lhs, rhs, .. }
        | Instruction::DivF64 { lhs, rhs, .. }
        | Instruction::RemF64 { lhs, rhs, .. }
        | Instruction::StringEqual { lhs, rhs, .. }
        | Instruction::StringConcat { lhs, rhs, .. }
        | Instruction::CompareEq { lhs, rhs, .. }
        | Instruction::CompareLtI32 { lhs, rhs, .. }
        | Instruction::CompareLtI64 { lhs, rhs, .. }
        | Instruction::CompareLtF32 { lhs, rhs, .. }
        | Instruction::CompareLtF64 { lhs, rhs, .. }
        | Instruction::EnumEqual { lhs, rhs, .. }
        | Instruction::StructEqual { lhs, rhs, .. }
        | Instruction::ClassEqual { lhs, rhs, .. }
        | Instruction::StateHandleEqual { lhs, rhs, .. } => {
            replace(lhs);
            replace(rhs);
        }
        Instruction::StringRuneAt { source, index, .. }
        | Instruction::ArrayGet { source, index, .. }
        | Instruction::ArrayFieldGet { source, index, .. }
        | Instruction::ArrayRemove { source, index, .. }
        | Instruction::BufferGet { source, index, .. } => {
            replace(source);
            replace(index);
        }
        Instruction::JumpIfFalse { condition, .. } => replace(condition),
        Instruction::StateNewSet { object, source, .. } => {
            replace(object);
            replace(source);
        }
        Instruction::StateReplace { target, .. } => replace(target),
        Instruction::EnumNew { payload, .. } => {
            if let Some(payload) = payload {
                replace(payload);
            }
        }
        Instruction::StructWith { source, value, .. }
        | Instruction::ClassSet { source, value, .. }
        | Instruction::ArrayPush { source, value } => {
            replace(source);
            replace(value);
        }
        Instruction::ArraySet {
            source,
            index,
            value,
        }
        | Instruction::ArrayInsert {
            source,
            index,
            value,
        }
        | Instruction::BufferSet {
            source,
            index,
            value,
        } => {
            replace(source);
            replace(index);
            replace(value);
        }
        Instruction::MapGet { source, key, .. }
        | Instruction::MapRemove { source, key, .. }
        | Instruction::MapContains { source, key, .. } => {
            replace(source);
            replace(key);
        }
        Instruction::MapSet { source, key, value } => {
            replace(source);
            replace(key);
            replace(value);
        }
        Instruction::BufferSlice {
            source,
            start,
            length,
            ..
        } => {
            replace(source);
            replace(start);
            replace(length);
        }
        Instruction::BufferCopy {
            destination,
            source,
            source_start,
            destination_start,
            length,
        } => {
            replace(destination);
            replace(source);
            replace(source_start);
            replace(destination_start);
            replace(length);
        }
        Instruction::StateOldFieldGet { object, .. } => replace(object),
        Instruction::StateHandleResolve { handle, .. }
        | Instruction::StateHandleIsAlive { handle, .. }
        | Instruction::StateHandleStableId { handle, .. }
        | Instruction::StateHandleGeneration { handle, .. }
        | Instruction::StateHandleHash { handle, .. } => replace(handle),
        // Unsupported operands include contiguous argument/field ranges;
        // replacing one member would destroy their ABI contiguity.
        _ => return false,
    }
    true
}

#[allow(clippy::too_many_lines)]
fn typed_instruction_sources(instruction: Instruction) -> Vec<u16> {
    let range = |base: u16, count: u16| {
        (0..count)
            .map(|offset| base.saturating_add(offset))
            .collect::<Vec<_>>()
    };
    match instruction {
        Instruction::Move { source, .. }
        | Instruction::CopyValue { source, .. }
        | Instruction::StringLen { source, .. }
        | Instruction::StringByteLen { source, .. }
        | Instruction::StringHash { source, .. }
        | Instruction::I32ToString { source, .. }
        | Instruction::I64ToString { source, .. }
        | Instruction::F32ToString { source, .. }
        | Instruction::F64ToString { source, .. }
        | Instruction::BoolToString { source, .. }
        | Instruction::RuneToString { source, .. }
        | Instruction::StringToString { source, .. }
        | Instruction::EnumTag { source, .. }
        | Instruction::EnumPayload { source, .. }
        | Instruction::StructGet { source, .. }
        | Instruction::ClassGet { source, .. }
        | Instruction::ArrayLen { source, .. }
        | Instruction::ArrayPop { source, .. }
        | Instruction::ArrayClear { source }
        | Instruction::MapLen { source, .. }
        | Instruction::MapClear { source }
        | Instruction::BufferLen { source, .. }
        | Instruction::SetLen { source, .. }
        | Instruction::SetClear { source }
        | Instruction::Return { source } => vec![source],
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::RemI32 { lhs, rhs, .. }
        | Instruction::AddI64 { lhs, rhs, .. }
        | Instruction::SubI64 { lhs, rhs, .. }
        | Instruction::MulI64 { lhs, rhs, .. }
        | Instruction::DivI64 { lhs, rhs, .. }
        | Instruction::RemI64 { lhs, rhs, .. }
        | Instruction::AddF32 { lhs, rhs, .. }
        | Instruction::SubF32 { lhs, rhs, .. }
        | Instruction::MulF32 { lhs, rhs, .. }
        | Instruction::DivF32 { lhs, rhs, .. }
        | Instruction::RemF32 { lhs, rhs, .. }
        | Instruction::AddF64 { lhs, rhs, .. }
        | Instruction::SubF64 { lhs, rhs, .. }
        | Instruction::MulF64 { lhs, rhs, .. }
        | Instruction::DivF64 { lhs, rhs, .. }
        | Instruction::RemF64 { lhs, rhs, .. }
        | Instruction::StringEqual { lhs, rhs, .. }
        | Instruction::StringConcat { lhs, rhs, .. }
        | Instruction::CompareEq { lhs, rhs, .. }
        | Instruction::CompareLtI32 { lhs, rhs, .. }
        | Instruction::CompareLtI64 { lhs, rhs, .. }
        | Instruction::CompareLtF32 { lhs, rhs, .. }
        | Instruction::CompareLtF64 { lhs, rhs, .. }
        | Instruction::EnumEqual { lhs, rhs, .. }
        | Instruction::StructEqual { lhs, rhs, .. }
        | Instruction::ClassEqual { lhs, rhs, .. }
        | Instruction::StateHandleEqual { lhs, rhs, .. } => vec![lhs, rhs],
        Instruction::StringRuneAt { source, index, .. }
        | Instruction::ArrayGet { source, index, .. }
        | Instruction::ArrayFieldGet { source, index, .. }
        | Instruction::ArrayRemove { source, index, .. }
        | Instruction::BufferGet { source, index, .. } => vec![source, index],
        Instruction::StandardIntrinsic {
            args_base,
            args_count,
            ..
        }
        | Instruction::StringBuild {
            parts_base: args_base,
            parts_count: args_count,
            ..
        }
        | Instruction::Call {
            args_base,
            args_count,
            ..
        }
        | Instruction::HostCall {
            args_base,
            args_count,
            ..
        }
        | Instruction::DeferPush {
            args_base,
            args_count,
            ..
        } => range(args_base, args_count),
        Instruction::JumpIfFalse { condition, .. } => vec![condition],
        Instruction::StateNewSet { object, source, .. } => vec![object, source],
        Instruction::StateReplace { target, .. } => vec![target],
        Instruction::EnumNew { payload, .. } => payload.into_iter().collect(),
        Instruction::StructNew {
            fields_base,
            fields_count,
            ..
        }
        | Instruction::ClassNew {
            fields_base,
            fields_count,
            ..
        } => range(fields_base, fields_count),
        Instruction::StructWith { source, value, .. }
        | Instruction::ClassSet { source, value, .. }
        | Instruction::ArrayPush { source, value }
        | Instruction::SetContains { source, value, .. }
        | Instruction::SetInsert { source, value, .. }
        | Instruction::SetRemove { source, value, .. } => vec![source, value],
        Instruction::ArrayPushRow {
            source,
            fields_base,
            fields_count,
        } => {
            let mut reads = range(fields_base, fields_count);
            reads.push(source);
            reads
        }
        Instruction::ArraySet {
            source,
            index,
            value,
        }
        | Instruction::ArrayInsert {
            source,
            index,
            value,
        }
        | Instruction::BufferSet {
            source,
            index,
            value,
        } => vec![source, index, value],
        Instruction::MapGet { source, key, .. }
        | Instruction::MapRemove { source, key, .. }
        | Instruction::MapContains { source, key, .. } => vec![source, key],
        Instruction::MapSet { source, key, value } => vec![source, key, value],
        Instruction::IterNew { kind, state } => match kind {
            CollectionIteratorKind::Range => vec![state.collection, state.phase],
            CollectionIteratorKind::Array { .. }
            | CollectionIteratorKind::Buffer { .. }
            | CollectionIteratorKind::Map { .. }
            | CollectionIteratorKind::Set { .. } => vec![state.collection],
        },
        Instruction::IterNext { state, .. } => {
            vec![state.collection, state.phase, state.slot, state.epoch]
        }
        Instruction::BufferSlice {
            source,
            start,
            length,
            ..
        } => vec![source, start, length],
        Instruction::BufferCopy {
            destination,
            source,
            source_start,
            destination_start,
            length,
        } => vec![destination, source, source_start, destination_start, length],
        Instruction::StateOldFieldGet { object, .. } => vec![object],
        Instruction::StateHandleResolve { handle, .. }
        | Instruction::StateHandleIsAlive { handle, .. }
        | Instruction::StateHandleStableId { handle, .. }
        | Instruction::StateHandleGeneration { handle, .. }
        | Instruction::StateHandleHash { handle, .. } => vec![handle],
        Instruction::LoadI32 { .. }
        | Instruction::LoadBool { .. }
        | Instruction::LoadI64 { .. }
        | Instruction::LoadF32 { .. }
        | Instruction::LoadF64 { .. }
        | Instruction::LoadRune { .. }
        | Instruction::LoadString { .. }
        | Instruction::Jump { .. }
        | Instruction::StateOldGet { .. }
        | Instruction::StateCurrentGet { .. }
        | Instruction::StateNewCreate { .. }
        | Instruction::StatePreserve { .. }
        | Instruction::StateDelete { .. }
        | Instruction::ArrayNew { .. }
        | Instruction::MapNew { .. }
        | Instruction::SetNew { .. }
        | Instruction::StateFinish
        | Instruction::DeferPop
        | Instruction::CleanupReturn
        | Instruction::ReturnVoid
        | Instruction::Safepoint
        | Instruction::Yield
        | Instruction::Trap => Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn typed_instruction_destination(instruction: Instruction) -> Option<u16> {
    match instruction {
        Instruction::LoadI32 { dst, .. }
        | Instruction::LoadBool { dst, .. }
        | Instruction::LoadI64 { dst, .. }
        | Instruction::LoadF32 { dst, .. }
        | Instruction::LoadF64 { dst, .. }
        | Instruction::LoadRune { dst, .. }
        | Instruction::LoadString { dst, .. }
        | Instruction::Move { dst, .. }
        | Instruction::CopyValue { dst, .. }
        | Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::RemI32 { dst, .. }
        | Instruction::AddI64 { dst, .. }
        | Instruction::SubI64 { dst, .. }
        | Instruction::MulI64 { dst, .. }
        | Instruction::DivI64 { dst, .. }
        | Instruction::RemI64 { dst, .. }
        | Instruction::AddF32 { dst, .. }
        | Instruction::SubF32 { dst, .. }
        | Instruction::MulF32 { dst, .. }
        | Instruction::DivF32 { dst, .. }
        | Instruction::RemF32 { dst, .. }
        | Instruction::AddF64 { dst, .. }
        | Instruction::SubF64 { dst, .. }
        | Instruction::MulF64 { dst, .. }
        | Instruction::DivF64 { dst, .. }
        | Instruction::RemF64 { dst, .. }
        | Instruction::StringLen { dst, .. }
        | Instruction::StringByteLen { dst, .. }
        | Instruction::StringEqual { dst, .. }
        | Instruction::StringConcat { dst, .. }
        | Instruction::StringBuild { dst, .. }
        | Instruction::StringRuneAt { dst, .. }
        | Instruction::StringHash { dst, .. }
        | Instruction::I32ToString { dst, .. }
        | Instruction::I64ToString { dst, .. }
        | Instruction::F32ToString { dst, .. }
        | Instruction::F64ToString { dst, .. }
        | Instruction::BoolToString { dst, .. }
        | Instruction::RuneToString { dst, .. }
        | Instruction::StringToString { dst, .. }
        | Instruction::StandardIntrinsic { dst, .. }
        | Instruction::CompareEq { dst, .. }
        | Instruction::CompareLtI32 { dst, .. }
        | Instruction::CompareLtI64 { dst, .. }
        | Instruction::CompareLtF32 { dst, .. }
        | Instruction::CompareLtF64 { dst, .. }
        | Instruction::Call { dst, .. }
        | Instruction::HostCall { dst, .. }
        | Instruction::StateOldGet { dst, .. }
        | Instruction::StateNewCreate { dst, .. }
        | Instruction::EnumNew { dst, .. }
        | Instruction::EnumTag { dst, .. }
        | Instruction::EnumPayload { dst, .. }
        | Instruction::EnumEqual { dst, .. }
        | Instruction::StructNew { dst, .. }
        | Instruction::StructGet { dst, .. }
        | Instruction::StructWith { dst, .. }
        | Instruction::StructEqual { dst, .. }
        | Instruction::ClassNew { dst, .. }
        | Instruction::ClassGet { dst, .. }
        | Instruction::ClassEqual { dst, .. }
        | Instruction::ArrayNew { dst, .. }
        | Instruction::ArrayLen { dst, .. }
        | Instruction::ArrayGet { dst, .. }
        | Instruction::ArrayFieldGet { dst, .. }
        | Instruction::ArrayPop { dst, .. }
        | Instruction::ArrayRemove { dst, .. }
        | Instruction::MapNew { dst, .. }
        | Instruction::MapLen { dst, .. }
        | Instruction::MapGet { dst, .. }
        | Instruction::MapRemove { dst, .. }
        | Instruction::MapContains { dst, .. }
        | Instruction::BufferLen { dst, .. }
        | Instruction::BufferGet { dst, .. }
        | Instruction::BufferSlice { dst, .. }
        | Instruction::SetNew { dst, .. }
        | Instruction::SetLen { dst, .. }
        | Instruction::SetContains { dst, .. }
        | Instruction::SetInsert { dst, .. }
        | Instruction::SetRemove { dst, .. }
        | Instruction::StateOldFieldGet { dst, .. }
        | Instruction::StateCurrentGet { dst, .. }
        | Instruction::StateHandleResolve { dst, .. }
        | Instruction::StateHandleIsAlive { dst, .. }
        | Instruction::StateHandleStableId { dst, .. }
        | Instruction::StateHandleGeneration { dst, .. }
        | Instruction::StateHandleEqual { dst, .. }
        | Instruction::StateHandleHash { dst, .. } => Some(dst),
        Instruction::Jump { .. }
        | Instruction::JumpIfFalse { .. }
        | Instruction::StateNewSet { .. }
        | Instruction::StateReplace { .. }
        | Instruction::StatePreserve { .. }
        | Instruction::StateDelete { .. }
        | Instruction::ClassSet { .. }
        | Instruction::ArraySet { .. }
        | Instruction::ArrayPush { .. }
        | Instruction::ArrayPushRow { .. }
        | Instruction::ArrayInsert { .. }
        | Instruction::ArrayClear { .. }
        | Instruction::MapSet { .. }
        | Instruction::MapClear { .. }
        | Instruction::BufferSet { .. }
        | Instruction::BufferCopy { .. }
        | Instruction::SetClear { .. }
        // IterNew and IterNext write multiple registers; see
        // typed_instruction_destinations.
        | Instruction::IterNew { .. }
        | Instruction::IterNext { .. }
        | Instruction::StateFinish
        | Instruction::DeferPush { .. }
        | Instruction::DeferPop
        | Instruction::CleanupReturn
        | Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::Safepoint
        | Instruction::Yield
        | Instruction::Trap => None,
    }
}

/// Every physical register written by an instruction.
///
/// Iterator instructions are multi-target: `IterNew` initializes the cursor
/// state (`phase`/`slot`/`epoch`; for `Range` the caller-set end bound in
/// `phase` is only read, never written) and `IterNext` writes the has-value
/// tag plus both payload registers. All write-target consumers (definite
/// initialization, liveness kills, optimizer conflict/dead/register-count
/// analysis) must iterate this list, never the single-destination wrapper.
fn typed_instruction_destinations(instruction: Instruction) -> Vec<u16> {
    match instruction {
        Instruction::IterNew { kind, state } => match kind {
            CollectionIteratorKind::Range => vec![state.slot, state.epoch],
            CollectionIteratorKind::Array { .. }
            | CollectionIteratorKind::Buffer { .. }
            | CollectionIteratorKind::Map { .. }
            | CollectionIteratorKind::Set { .. } => vec![state.phase, state.slot, state.epoch],
        },
        Instruction::IterNext {
            has_value_dst,
            first_dst,
            second_dst,
            ..
        } => {
            let mut destinations = Vec::with_capacity(3);
            destinations.push(has_value_dst);
            destinations.push(first_dst);
            if let Some(second_dst) = second_dst {
                destinations.push(second_dst);
            }
            destinations
        }
        _ => typed_instruction_destination(instruction)
            .into_iter()
            .collect(),
    }
}

fn typed_instruction_liveness_destinations(instruction: Instruction) -> Vec<u16> {
    // Typed emission allocates a fresh destination for every call expression.
    // Keeping it live before the call is therefore filtered by definite
    // initialization, while also matching void calls, whose encoded `dst` is
    // not written at all.
    match instruction {
        Instruction::Call { .. } | Instruction::HostCall { .. } => Vec::new(),
        _ => typed_instruction_destinations(instruction),
    }
}

fn source_span(
    range: &SourceRange,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<SourceSpan, CompileError> {
    let file = files.get(&range.source).copied().ok_or_else(|| {
        CompileError::unknown_name(
            format!(
                "typed source `{}` is absent from package modules",
                range.source.path
            ),
            SourceSpan::default(),
        )
    })?;
    Ok(SourceSpan::new(file, range.start, range.end))
}

fn catalog_source_span(
    package: &TypedPackageIr,
    range: &SourceRange,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<SourceSpan, CompileError> {
    if let Some(file) = files.get(&range.source).copied() {
        return Ok(SourceSpan::new(file, range.start, range.end));
    }
    let external = package
        .metadata()
        .external_sources
        .iter()
        .find(|source| nexa_analysis::external_source_key(&source.identity) == range.source)
        .ok_or_else(|| {
            CompileError::unknown_name(
                format!(
                    "typed source `{}` is absent from package and external source catalogs",
                    range.source.path
                ),
                SourceSpan::default(),
            )
        })?;
    Ok(SourceSpan::new(
        FileId(external.file_id.0),
        range.start,
        range.end,
    ))
}

fn full_source_span(
    module: &nexa_analysis::TypedModuleIr,
    files: &BTreeMap<SourceKey, FileId>,
) -> SourceSpan {
    SourceSpan::new(files[&module.source], 0, module.syntax.source.len().get())
}

fn lower_effect(effect: IrEffect) -> FunctionEffect {
    match effect {
        IrEffect::Ordinary => FunctionEffect::Ordinary,
        IrEffect::Immediate | IrEffect::Activation => FunctionEffect::Immediate,
        IrEffect::Task => FunctionEffect::Task,
        IrEffect::Migration => FunctionEffect::Migration,
        IrEffect::Cleanup => FunctionEffect::Cleanup,
    }
}

#[allow(clippy::too_many_lines)]
fn emit_typed_type_metadata(
    package: &TypedPackageIr,
    modules: &[&nexa_analysis::TypedModuleIr],
    files: &BTreeMap<SourceKey, FileId>,
    state_schema: &StateSchema,
    builder: &mut ModuleBuilder,
) -> Result<TypedLayoutContext, CompileError> {
    let mut context = TypedLayoutContext::default();
    let mut enum_types = BTreeMap::<StableId, EnumType>::new();
    let mut struct_types = BTreeMap::<StableId, StructType>::new();
    let mut class_types = BTreeMap::<StableId, ClassType>::new();
    let mut opaque_types = BTreeSet::new();

    for module in modules {
        for declaration in module.declarations.iter() {
            let TypedDeclarationBody::TypeLayout(layout) = &declaration.body else {
                continue;
            };
            let definition = package
                .definition(declaration.definition)
                .expect("TypedPackageIr validates type layout IDs");
            let span = source_span(&definition.span, files)?;
            let type_id = named_type_id(package, definition.id, span)?;
            match layout {
                TypedTypeLayoutIr::Struct { fields } | TypedTypeLayoutIr::Class { fields, .. } => {
                    let kind = if matches!(layout, TypedTypeLayoutIr::Struct { .. }) {
                        TypedAggregateKind::Struct
                    } else {
                        TypedAggregateKind::Class
                    };
                    let mut fields = fields.iter().collect::<Vec<_>>();
                    fields.sort_by_key(|field| field.order);
                    for (index, field) in fields.iter().enumerate() {
                        if field.order
                            != u32::try_from(index)
                                .map_err(|_| CompileError::too_many_registers(span))?
                        {
                            return Err(CompileError::unknown_type(
                                "aggregate field order must be dense".into(),
                                span,
                            ));
                        }
                    }
                    let fields = fields
                        .into_iter()
                        .map(|field| {
                            if kind == TypedAggregateKind::Struct && field.mutable {
                                return Err(CompileError::unknown_type(
                                    "Struct fields cannot carry Class field mutability".into(),
                                    span,
                                ));
                            }
                            let definition = package
                                .definition(field.definition)
                                .expect("TypedPackageIr validates field layout IDs");
                            let field_span = source_span(&definition.span, files)?;
                            let (_, stable_id) = stable_symbol(definition, field_span)?;
                            Ok(TypedFieldLayout {
                                definition: field.definition,
                                stable_id: stable_id.0,
                                ty: lower_type(package, &field.ty, field_span)?,
                                mutable: field.mutable,
                            })
                        })
                        .collect::<Result<Vec<_>, CompileError>>()?;
                    let aggregate = TypedAggregateLayout {
                        type_id,
                        kind,
                        fields: fields.clone(),
                    };
                    for field in &fields {
                        if context
                            .fields
                            .insert(field.definition, (declaration.definition, field.clone()))
                            .is_some()
                        {
                            return Err(CompileError::duplicate_name(
                                format!("definition#{}", field.definition.0),
                                span,
                                span,
                            ));
                        }
                    }
                    if context
                        .aggregates
                        .insert(declaration.definition, aggregate)
                        .is_some()
                    {
                        return Err(CompileError::duplicate_name(
                            definition.name.clone(),
                            span,
                            span,
                        ));
                    }
                    if kind == TypedAggregateKind::Struct {
                        let bytecode_fields = fields
                            .into_iter()
                            .map(|field| BytecodeStructField {
                                stable_id: field.stable_id,
                                ty: field.ty,
                            })
                            .collect();
                        insert_type_metadata(
                            &mut struct_types,
                            type_id,
                            StructType {
                                type_id,
                                fields: bytecode_fields,
                            },
                            span,
                        )?;
                    } else if matches!(layout, TypedTypeLayoutIr::Class { .. }) {
                        let bytecode_fields = fields
                            .into_iter()
                            .map(|field| BytecodeStructField {
                                stable_id: field.stable_id,
                                ty: field.ty,
                            })
                            .collect();
                        insert_type_metadata(
                            &mut class_types,
                            type_id,
                            ClassType {
                                type_id,
                                fields: bytecode_fields,
                            },
                            span,
                        )?;
                    }
                }
                TypedTypeLayoutIr::Enum { variants } => {
                    let mut variants = variants.iter().collect::<Vec<_>>();
                    variants.sort_by_key(|variant| variant.tag);
                    for (index, variant) in variants.iter().enumerate() {
                        if variant.tag
                            != u32::try_from(index)
                                .map_err(|_| CompileError::too_many_registers(span))?
                        {
                            return Err(CompileError::unknown_type(
                                "enum tags must be dense".into(),
                                span,
                            ));
                        }
                    }
                    let mut by_definition = BTreeMap::new();
                    let mut bytecode_variants = Vec::new();
                    for variant in variants {
                        let definition = package
                            .definition(variant.definition)
                            .expect("TypedPackageIr validates variant layout IDs");
                        let variant_span = source_span(&definition.span, files)?;
                        let (_, stable_id) = stable_symbol(definition, variant_span)?;
                        let variant_layout = TypedVariantLayout {
                            stable_id: stable_id.0,
                            tag: variant.tag,
                            payload: variant
                                .payload
                                .as_ref()
                                .map(|payload| lower_type(package, payload, variant_span))
                                .transpose()?,
                        };
                        bytecode_variants.push(EnumVariant {
                            stable_id: variant_layout.stable_id,
                            tag: variant_layout.tag,
                            payload_type: variant_layout.payload,
                        });
                        by_definition.insert(variant.definition, variant_layout.clone());
                        context
                            .variants
                            .insert(variant.definition, (declaration.definition, variant_layout));
                    }
                    insert_type_metadata(
                        &mut enum_types,
                        type_id,
                        EnumType {
                            type_id,
                            variants: bytecode_variants,
                        },
                        span,
                    )?;
                    context.enums.insert(
                        declaration.definition,
                        TypedEnumLayout {
                            type_id,
                            variants: by_definition,
                        },
                    );
                }
            }
        }
    }

    for host in package.metadata().host_bindings.iter() {
        for ty in &host.types {
            match &ty.layout {
                HostTypeLayoutIr::Opaque => {
                    builder.opaque_type(ty.stable_id);
                    opaque_types.insert(ty.stable_id);
                }
                HostTypeLayoutIr::Struct { fields } => {
                    let mut fields = fields.iter().collect::<Vec<_>>();
                    fields.sort_by_key(|field| field.order);
                    let mut typed_fields = Vec::new();
                    let mut bytecode_fields = Vec::new();
                    for (index, field) in fields.into_iter().enumerate() {
                        if field.order != u32::try_from(index).unwrap_or(u32::MAX) {
                            return Err(CompileError::unknown_type(
                                "host struct field order must be dense".into(),
                                external_span(ty.source.as_ref()),
                            ));
                        }
                        let field_layout = TypedFieldLayout {
                            definition: field.definition,
                            stable_id: field.stable_id,
                            ty: lower_type(
                                package,
                                &field.ty,
                                external_span(field.source.as_ref()),
                            )?,
                            mutable: false,
                        };
                        bytecode_fields.push(BytecodeStructField {
                            stable_id: field_layout.stable_id,
                            ty: field_layout.ty,
                        });
                        context
                            .fields
                            .insert(field.definition, (ty.definition, field_layout.clone()));
                        typed_fields.push(field_layout);
                    }
                    insert_type_metadata(
                        &mut struct_types,
                        ty.stable_id,
                        StructType {
                            type_id: ty.stable_id,
                            fields: bytecode_fields,
                        },
                        external_span(ty.source.as_ref()),
                    )?;
                    context.aggregates.insert(
                        ty.definition,
                        TypedAggregateLayout {
                            type_id: ty.stable_id,
                            kind: TypedAggregateKind::Struct,
                            fields: typed_fields,
                        },
                    );
                }
                HostTypeLayoutIr::Enum { variants } => {
                    let mut variants = variants.iter().collect::<Vec<_>>();
                    variants.sort_by_key(|variant| variant.tag);
                    let mut typed_variants = BTreeMap::new();
                    let mut bytecode_variants = Vec::new();
                    for (index, variant) in variants.into_iter().enumerate() {
                        if variant.tag != u32::try_from(index).unwrap_or(u32::MAX) {
                            return Err(CompileError::unknown_type(
                                "host enum tags must be dense".into(),
                                external_span(ty.source.as_ref()),
                            ));
                        }
                        let variant_layout = TypedVariantLayout {
                            stable_id: variant.stable_id,
                            tag: variant.tag,
                            payload: variant
                                .payload
                                .as_ref()
                                .map(|payload| {
                                    lower_type(
                                        package,
                                        payload,
                                        external_span(variant.source.as_ref()),
                                    )
                                })
                                .transpose()?,
                        };
                        bytecode_variants.push(EnumVariant {
                            stable_id: variant_layout.stable_id,
                            tag: variant_layout.tag,
                            payload_type: variant_layout.payload,
                        });
                        context
                            .variants
                            .insert(variant.definition, (ty.definition, variant_layout.clone()));
                        typed_variants.insert(variant.definition, variant_layout);
                    }
                    insert_type_metadata(
                        &mut enum_types,
                        ty.stable_id,
                        EnumType {
                            type_id: ty.stable_id,
                            variants: bytecode_variants,
                        },
                        external_span(ty.source.as_ref()),
                    )?;
                    context.enums.insert(
                        ty.definition,
                        TypedEnumLayout {
                            type_id: ty.stable_id,
                            variants: typed_variants,
                        },
                    );
                }
            }
        }
    }

    let mut generics = GenericTypeMetadata::default();
    for definition in package.definitions() {
        let span = source_span(&definition.span, files).unwrap_or_default();
        // Generic declarations describe templates, not concrete bytecode layouts. Their
        // instantiated call-site and expression types are collected below from typed bodies.
        if ir_type_contains_type_parameter(&definition.ty) {
            if definition.kind != DefinitionKind::StandardLibrary {
                return Err(CompileError::unknown_type(
                    "uninstantiated type parameter outside a standard-library template".into(),
                    span,
                ));
            }
            continue;
        }
        collect_ir_type_metadata(package, &definition.ty, span, &mut generics)?;
    }
    for module in modules {
        for declaration in module.declarations.iter() {
            if let TypedDeclarationBody::Function(function) = &declaration.body {
                let definition = package
                    .definition(declaration.definition)
                    .expect("TypedPackageIr validates function IDs");
                let span = source_span(&definition.span, files)?;
                collect_ir_type_metadata(package, &function.return_type, span, &mut generics)?;
                collect_block_type_metadata(package, &function.body, span, &mut generics)?;
            }
        }
    }
    for enum_type in generics.enums.into_values() {
        insert_type_metadata(
            &mut enum_types,
            enum_type.type_id,
            enum_type,
            SourceSpan::default(),
        )?;
    }
    for struct_type in generics.structs.into_values() {
        insert_type_metadata(
            &mut struct_types,
            struct_type.type_id,
            struct_type,
            SourceSpan::default(),
        )?;
    }
    let layout_module = Module {
        state_handle_types: generics.state_handles.values().copied().collect(),
        array_types: generics.arrays.values().copied().collect(),
        map_types: generics.maps.values().copied().collect(),
        buffer_types: generics.buffers.values().copied().collect(),
        set_types: generics.sets.values().copied().collect(),
        snapshot_types: generics.snapshots.values().copied().collect(),
        resource_token_types: generics.resource_tokens.values().copied().collect(),
        opaque_types: opaque_types.into_iter().collect(),
        enum_types: enum_types.values().cloned().collect(),
        struct_types: struct_types.values().cloned().collect(),
        class_types: class_types.values().cloned().collect(),
        state_schema: state_schema.clone(),
        ..Module::default()
    };
    context.layout_table = LayoutTable::for_module(&layout_module)
        .map_err(|error| CompileError::unknown_type(error.to_string(), SourceSpan::default()))?;
    for value in enum_types.into_values() {
        builder.enum_type(value);
    }
    for value in struct_types.into_values() {
        builder.struct_type(value);
    }
    for value in class_types.into_values() {
        builder.class_type(value);
    }
    for value in generics.arrays.into_values() {
        builder.array_type(value);
    }
    for value in generics.maps.into_values() {
        builder.map_type(value);
    }
    for value in generics.buffers.into_values() {
        builder.buffer_type(value);
    }
    for value in generics.sets.into_values() {
        builder.set_type(value);
    }
    for value in generics.snapshots.into_values() {
        builder.snapshot_type(value);
    }
    for value in generics.state_handles.into_values() {
        builder.state_handle_type(value);
    }
    for value in generics.resource_tokens.into_values() {
        builder.resource_token_type(value);
    }
    Ok(context)
}

fn insert_type_metadata<T: PartialEq>(
    types: &mut BTreeMap<StableId, T>,
    type_id: StableId,
    value: T,
    span: SourceSpan,
) -> Result<(), CompileError> {
    match types.entry(type_id) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(value);
        }
        std::collections::btree_map::Entry::Occupied(entry) if entry.get() != &value => {
            return Err(CompileError::unknown_type(
                format!("conflicting bytecode type metadata for {type_id:?}"),
                span,
            ));
        }
        std::collections::btree_map::Entry::Occupied(_) => {}
    }
    Ok(())
}

fn external_span(source: Option<&nexa_analysis::ExternalSourceRangeIr>) -> SourceSpan {
    source.map_or_else(SourceSpan::default, |source| {
        SourceSpan::new(
            FileId(source.file_id.0),
            source.range.start,
            source.range.end,
        )
    })
}

#[derive(Default)]
struct GenericTypeMetadata {
    enums: BTreeMap<StableId, EnumType>,
    structs: BTreeMap<StableId, StructType>,
    arrays: BTreeMap<StableId, ArrayType>,
    maps: BTreeMap<StableId, MapType>,
    buffers: BTreeMap<StableId, BufferType>,
    sets: BTreeMap<StableId, SetType>,
    snapshots: BTreeMap<StableId, SnapshotType>,
    state_handles: BTreeMap<StableId, StateHandleType>,
    resource_tokens: BTreeMap<StableId, ResourceTokenType>,
}

fn collect_ir_type_metadata(
    package: &TypedPackageIr,
    ty: &IrType,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    match ty {
        IrType::Option(value) => {
            collect_ir_type_metadata(package, value, span, metadata)?;
            let value = lower_type(package, value, span)?;
            let ty = option_type(value);
            metadata.enums.insert(ty.type_id, ty);
        }
        IrType::Result(success, error) => {
            collect_ir_type_metadata(package, success, span, metadata)?;
            collect_ir_type_metadata(package, error, span, metadata)?;
            let ty = result_type(
                lower_type(package, success, span)?,
                lower_type(package, error, span)?,
            );
            metadata.enums.insert(ty.type_id, ty);
        }
        IrType::Array(element) => {
            collect_ir_type_metadata(package, element, span, metadata)?;
            let value = ArrayType::new(lower_type(package, element, span)?);
            metadata.arrays.insert(value.type_id, value);
        }
        IrType::Map(key, value) => {
            collect_ir_type_metadata(package, key, span, metadata)?;
            collect_ir_type_metadata(package, value, span, metadata)?;
            let value = MapType::new(
                lower_type(package, key, span)?,
                lower_type(package, value, span)?,
            );
            metadata.maps.insert(value.type_id, value);
        }
        IrType::Tuple(items) => {
            collect_tuple_type_metadata(package, items, span, metadata)?;
        }
        IrType::Buffer(element) => {
            collect_ir_type_metadata(package, element, span, metadata)?;
            let value = BufferType::new(lower_type(package, element, span)?);
            metadata.buffers.insert(value.type_id, value);
        }
        IrType::Set(element) => {
            collect_ir_type_metadata(package, element, span, metadata)?;
            let value = SetType::new(lower_type(package, element, span)?);
            metadata.sets.insert(value.type_id, value);
        }
        IrType::Snapshot(content) => {
            collect_ir_type_metadata(package, content, span, metadata)?;
            let lowered = lower_type(package, content, span)?;
            let content_type = match lowered {
                ValueType::Named(type_id) => type_id,
                _ => parameterized_type_id("SnapshotContent", &[lowered]),
            };
            let value = SnapshotType::new(content_type);
            metadata.snapshots.insert(value.type_id, value);
        }
        IrType::StateHandle(target) => {
            collect_ir_type_metadata(package, target, span, metadata)?;
            let value = StateHandleType::new(lower_type(package, target, span)?);
            metadata.state_handles.insert(value.type_id, value);
            let error = nexa_bytecode::state_handle_error_type();
            metadata.enums.insert(error.type_id, error);
        }
        IrType::HostRequest(_) => {
            return Err(CompileError::unknown_type(
                "HostRequest is an internal awaitable and cannot appear in Typed IR".into(),
                span,
            ));
        }
        IrType::ResourceToken(value) => {
            let content_type = resource_token_content_id(package, value.as_deref(), span)?;
            let token = ResourceTokenType::new(content_type);
            metadata.resource_tokens.insert(token.type_id, token);
        }
        IrType::Named(definition)
            if compiler_builtin_type(package, *definition) == Some("StateHandleError") =>
        {
            let error = nexa_bytecode::state_handle_error_type();
            metadata.enums.insert(error.type_id, error);
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
        | IrType::Error => {}
        IrType::TypeParameter(_) => {
            return Err(CompileError::unknown_type(
                "uninstantiated standard-library type parameter".into(),
                span,
            ));
        }
    }
    Ok(())
}

fn collect_tuple_type_metadata(
    package: &TypedPackageIr,
    items: &[IrType],
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    for item in items {
        collect_ir_type_metadata(package, item, span, metadata)?;
    }
    let items = items
        .iter()
        .map(|item| lower_type(package, item, span))
        .collect::<Result<Vec<_>, _>>()?;
    let type_id = parameterized_type_id("Tuple", &items);
    let fields = items
        .into_iter()
        .enumerate()
        .map(|(index, ty)| BytecodeStructField {
            stable_id: tuple_field_stable_id(type_id, index),
            ty,
        })
        .collect();
    metadata
        .structs
        .insert(type_id, StructType { type_id, fields });
    Ok(())
}

fn tuple_field_stable_id(type_id: StableId, index: usize) -> StableId {
    StableId::from_name(&format!("nexa.tuple.{:016x}.field.{index}", type_id.0))
}

fn collect_block_type_metadata(
    package: &TypedPackageIr,
    block: &TypedBlockIr,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    collect_expression_type_metadata(package, value, span, metadata)?;
                }
            }
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
                collect_place_type_metadata(package, target, span, metadata)?;
                collect_expression_type_metadata(package, value, span, metadata)?;
            }
            TypedStatementIr::Expression(value) => {
                collect_expression_type_metadata(package, value, span, metadata)?;
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_expression_type_metadata(package, condition, span, metadata)?;
                collect_block_type_metadata(package, then_block, span, metadata)?;
                if let Some(else_block) = else_block {
                    collect_block_type_metadata(package, else_block, span, metadata)?;
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                collect_expression_type_metadata(package, condition, span, metadata)?;
                collect_block_type_metadata(package, body, span, metadata)?;
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            }
            | TypedStatementIr::DynamicRangeFor {
                start, end, body, ..
            } => {
                collect_expression_type_metadata(package, start, span, metadata)?;
                collect_expression_type_metadata(package, end, span, metadata)?;
                collect_block_type_metadata(package, body, span, metadata)?;
            }
            TypedStatementIr::CollectionFor { iterable, body, .. } => {
                collect_expression_type_metadata(package, iterable, span, metadata)?;
                collect_block_type_metadata(package, body, span, metadata)?;
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    collect_expression_type_metadata(package, capture, span, metadata)?;
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &block.tail {
        collect_expression_type_metadata(package, tail, span, metadata)?;
    }
    Ok(())
}

fn collect_place_type_metadata(
    package: &TypedPackageIr,
    place: &TypedPlaceIr,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    match place {
        TypedPlaceIr::Definition(_) => Ok(()),
        TypedPlaceIr::Field { base, .. } => {
            collect_place_type_metadata(package, base, span, metadata)
        }
        TypedPlaceIr::ClassField { object: base, .. } | TypedPlaceIr::StateField { base, .. } => {
            collect_expression_type_metadata(package, base, span, metadata)
        }
        TypedPlaceIr::Index { base, index } => {
            collect_expression_type_metadata(package, base, span, metadata)?;
            collect_expression_type_metadata(package, index, span, metadata)?;
            if let IrType::Map(_, value) = &base.ty {
                collect_ir_type_metadata(package, &IrType::Option(value.clone()), span, metadata)?;
            }
            Ok(())
        }
    }
}

fn collect_expression_type_metadata(
    package: &TypedPackageIr,
    expression: &TypedExpressionIr,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    collect_ir_type_metadata(package, &expression.ty, span, metadata)?;
    match &expression.kind {
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Await(operand)
        | TypedExpressionKind::Try(operand)
        | TypedExpressionKind::Field { base: operand, .. }
        | TypedExpressionKind::StateField { base: operand, .. } => {
            collect_expression_type_metadata(package, operand, span, metadata)?;
        }
        TypedExpressionKind::Binary { left, right, .. } => {
            collect_expression_type_metadata(package, left, span, metadata)?;
            collect_expression_type_metadata(package, right, span, metadata)?;
        }
        TypedExpressionKind::Index { base, index } => {
            collect_expression_type_metadata(package, base, span, metadata)?;
            collect_expression_type_metadata(package, index, span, metadata)?;
            if let IrType::Map(_, value) = &base.ty {
                collect_ir_type_metadata(package, &IrType::Option(value.clone()), span, metadata)?;
            }
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::HostCall { arguments, .. }
        | TypedExpressionKind::Array(arguments)
        | TypedExpressionKind::Tuple(arguments)
        | TypedExpressionKind::StringInterpolation(arguments) => {
            for argument in arguments {
                collect_expression_type_metadata(package, argument, span, metadata)?;
            }
        }
        TypedExpressionKind::StandardCall {
            type_arguments,
            arguments,
            ..
        }
        | TypedExpressionKind::BuiltinCall {
            type_arguments,
            arguments,
            ..
        } => {
            for ty in type_arguments {
                collect_ir_type_metadata(package, ty, span, metadata)?;
            }
            for argument in arguments {
                collect_expression_type_metadata(package, argument, span, metadata)?;
            }
        }
        TypedExpressionKind::Construct { fields, .. }
        | TypedExpressionKind::Update { fields, .. } => {
            if let TypedExpressionKind::Update { base, .. } = &expression.kind {
                collect_expression_type_metadata(package, base, span, metadata)?;
            }
            for (_, value) in fields {
                collect_expression_type_metadata(package, value, span, metadata)?;
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            if let Some(update) = update {
                collect_expression_type_metadata(package, update, span, metadata)?;
            }
            for (_, value) in fields {
                collect_expression_type_metadata(package, value, span, metadata)?;
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_type_metadata(package, payload, span, metadata)?;
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            collect_expression_type_metadata(package, value, span, metadata)?;
            for arm in arms {
                collect_pattern_type_metadata(package, &arm.pattern, span, metadata)?;
                collect_expression_type_metadata(package, &arm.value, span, metadata)?;
            }
        }
        TypedExpressionKind::Migration(intrinsic) => {
            collect_migration_type_metadata(package, intrinsic, span, metadata)?;
        }
    }
    Ok(())
}

fn collect_migration_type_metadata(
    package: &TypedPackageIr,
    intrinsic: &MigrationIntrinsicIr,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    match intrinsic {
        MigrationIntrinsicIr::OldGet { value_type, .. } => {
            collect_ir_type_metadata(package, value_type, span, metadata)
        }
        MigrationIntrinsicIr::OldFieldGet {
            object, value_type, ..
        } => {
            collect_expression_type_metadata(package, object, span, metadata)?;
            collect_ir_type_metadata(package, value_type, span, metadata)
        }
        MigrationIntrinsicIr::NewCreate { state_type, .. } => {
            collect_ir_type_metadata(package, &IrType::Named(*state_type), span, metadata)
        }
        MigrationIntrinsicIr::NewSet { object, value, .. } => {
            collect_expression_type_metadata(package, object, span, metadata)?;
            collect_expression_type_metadata(package, value, span, metadata)
        }
        MigrationIntrinsicIr::Replace { target, .. } => {
            collect_expression_type_metadata(package, target, span, metadata)
        }
        MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => Ok(()),
    }
}

fn collect_pattern_type_metadata(
    package: &TypedPackageIr,
    pattern: &TypedPatternIr,
    span: SourceSpan,
    metadata: &mut GenericTypeMetadata,
) -> Result<(), CompileError> {
    collect_ir_type_metadata(package, &pattern.ty, span, metadata)?;
    match &pattern.kind {
        TypedPatternKind::Wildcard
        | TypedPatternKind::Binding(_)
        | TypedPatternKind::Literal(_) => {}
        TypedPatternKind::Variant { payload, .. } => {
            for payload in payload {
                collect_pattern_type_metadata(package, payload, span, metadata)?;
            }
        }
        TypedPatternKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_pattern_type_metadata(package, payload, span, metadata)?;
            }
        }
        TypedPatternKind::Struct { fields, .. } => {
            for (_, field) in fields {
                collect_pattern_type_metadata(package, field, span, metadata)?;
            }
        }
    }
    Ok(())
}

fn typed_state_schema(
    package: &TypedPackageIr,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<StateSchema, CompileError> {
    let mut types = package
        .metadata()
        .state_types
        .iter()
        .map(|state| {
            let definition = package
                .definition(state.definition)
                .expect("TypedPackageIr validates state IDs");
            let span = source_span(&definition.span, files)?;
            let fields = state
                .fields
                .iter()
                .map(|field| {
                    let field_definition = package
                        .definition(field.definition)
                        .expect("TypedPackageIr validates state field IDs");
                    if field_definition
                        .stable_symbol
                        .as_ref()
                        .is_some_and(|symbol| symbol.runtime_id != field.stable_id)
                    {
                        return Err(CompileError::invalid_reload_metadata(
                            "analyzed state-field identity disagrees with definition identity",
                            source_span(&field_definition.span, files)?,
                        ));
                    }
                    Ok(StateField {
                        stable_id: field.stable_id.0,
                        ty: lower_type(package, &field.ty, span)?,
                    })
                })
                .collect::<Result<Vec<_>, CompileError>>()?;
            if definition
                .stable_symbol
                .as_ref()
                .is_some_and(|symbol| symbol.runtime_id != state.stable_id)
            {
                return Err(CompileError::invalid_reload_metadata(
                    "analyzed state-type identity disagrees with definition identity",
                    span,
                ));
            }
            Ok(StateType {
                stable_id: state.stable_id.0,
                version: state.version,
                fields,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    types.sort_by_key(|state| state.stable_id);
    Ok(StateSchema { types })
}

fn typed_standard_functions(
    package: &TypedPackageIr,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<BTreeMap<DefinitionId, nexa_stdlib::Intrinsic>, CompileError> {
    let mut functions = BTreeMap::new();
    for binding in package.metadata().standard_functions.iter() {
        let definition = package
            .definition(binding.definition)
            .expect("TypedPackageIr validates standard function IDs");
        let span = source_span(&definition.span, files)?;
        for ty in binding.parameters.iter().chain([&binding.result]) {
            validate_standard_signature_type(ty, binding.type_parameters.len(), span)?;
        }
        if functions
            .insert(binding.definition, binding.intrinsic)
            .is_some()
        {
            return Err(CompileError::duplicate_name(
                definition.name.clone(),
                span,
                span,
            ));
        }
    }
    Ok(functions)
}

fn validate_standard_signature_type(
    ty: &IrType,
    type_parameter_count: usize,
    span: SourceSpan,
) -> Result<(), CompileError> {
    match ty {
        IrType::TypeParameter(index) => {
            if usize::from(*index) >= type_parameter_count {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        IrType::Option(value)
        | IrType::Array(value)
        | IrType::Set(value)
        | IrType::Snapshot(value)
        | IrType::Buffer(value)
        | IrType::StateHandle(value) => {
            validate_standard_signature_type(value, type_parameter_count, span)?;
        }
        IrType::Result(success, error) | IrType::Map(success, error) => {
            validate_standard_signature_type(success, type_parameter_count, span)?;
            validate_standard_signature_type(error, type_parameter_count, span)?;
        }
        IrType::Tuple(values) => {
            for value in values {
                validate_standard_signature_type(value, type_parameter_count, span)?;
            }
        }
        IrType::HostRequest(_) => {
            return Err(CompileError::unknown_type(
                "HostRequest is not a standard-library signature type".into(),
                span,
            ));
        }
        IrType::ResourceToken(value) => {
            if let Some(value) = value {
                validate_standard_signature_type(value, type_parameter_count, span)?;
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
        | IrType::Error => {}
    }
    Ok(())
}

fn validate_standard_call_signature(
    binding: &nexa_analysis::StandardFunctionBindingIr,
    intrinsic: nexa_stdlib::Intrinsic,
    type_arguments: &[IrType],
    arguments: &[TypedExpressionIr],
    result: &IrType,
    span: SourceSpan,
) -> Result<(), CompileError> {
    if binding.intrinsic != intrinsic
        || binding.type_parameters.len() != type_arguments.len()
        || binding.parameters.len() != arguments.len()
        || type_arguments.iter().any(ir_type_contains_type_parameter)
    {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    for (expected, actual) in binding.parameters.iter().zip(arguments) {
        if instantiate_standard_type(expected, type_arguments, span)? != actual.ty {
            return Err(CompileError::type_mismatch(None, None, span));
        }
    }
    if instantiate_standard_type(&binding.result, type_arguments, span)? != *result {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    Ok(())
}

fn instantiate_standard_type(
    ty: &IrType,
    type_arguments: &[IrType],
    span: SourceSpan,
) -> Result<IrType, CompileError> {
    let instantiate = |ty: &IrType| instantiate_standard_type(ty, type_arguments, span);
    Ok(match ty {
        IrType::TypeParameter(index) => type_arguments
            .get(usize::from(*index))
            .cloned()
            .ok_or_else(|| CompileError::type_mismatch(None, None, span))?,
        IrType::Option(value) => IrType::Option(Box::new(instantiate(value)?)),
        IrType::Result(success, error) => IrType::Result(
            Box::new(instantiate(success)?),
            Box::new(instantiate(error)?),
        ),
        IrType::Array(value) => IrType::Array(Box::new(instantiate(value)?)),
        IrType::Set(value) => IrType::Set(Box::new(instantiate(value)?)),
        IrType::Map(key, value) => {
            IrType::Map(Box::new(instantiate(key)?), Box::new(instantiate(value)?))
        }
        IrType::Tuple(values) => {
            IrType::Tuple(values.iter().map(instantiate).collect::<Result<_, _>>()?)
        }
        IrType::HostRequest(_) => {
            return Err(CompileError::unknown_type(
                "HostRequest cannot be instantiated as a source-visible type".into(),
                span,
            ));
        }
        IrType::ResourceToken(value) => {
            IrType::ResourceToken(value.as_deref().map(instantiate).transpose()?.map(Box::new))
        }
        IrType::Snapshot(value) => IrType::Snapshot(Box::new(instantiate(value)?)),
        IrType::Buffer(value) => IrType::Buffer(Box::new(instantiate(value)?)),
        IrType::StateHandle(value) => IrType::StateHandle(Box::new(instantiate(value)?)),
        value => value.clone(),
    })
}

fn ir_type_contains_type_parameter(ty: &IrType) -> bool {
    match ty {
        IrType::TypeParameter(_) => true,
        IrType::Option(value)
        | IrType::Array(value)
        | IrType::Set(value)
        | IrType::Snapshot(value)
        | IrType::Buffer(value)
        | IrType::StateHandle(value) => ir_type_contains_type_parameter(value),
        IrType::Result(success, error) | IrType::Map(success, error) => {
            ir_type_contains_type_parameter(success) || ir_type_contains_type_parameter(error)
        }
        IrType::Tuple(values) => values.iter().any(ir_type_contains_type_parameter),
        IrType::HostRequest(value) | IrType::ResourceToken(value) => value
            .as_deref()
            .is_some_and(ir_type_contains_type_parameter),
        IrType::Unit
        | IrType::Bool
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::String
        | IrType::Rune
        | IrType::Named(_)
        | IrType::Error => false,
    }
}

#[allow(clippy::too_many_lines)]
fn standard_call_lowering(
    package: &TypedPackageIr,
    intrinsic: nexa_stdlib::Intrinsic,
    type_arguments: &[IrType],
    arguments: &[TypedExpressionIr],
    result: &IrType,
    span: SourceSpan,
) -> Result<TypedStandardLowering, CompileError> {
    use nexa_stdlib::Intrinsic as I;

    let type_argument = |index: usize| {
        type_arguments
            .get(index)
            .ok_or_else(|| CompileError::type_mismatch(None, None, span))
            .and_then(|ty| lower_type(package, ty, span))
    };
    let lowering = match intrinsic {
        I::StringToString => TypedStandardLowering::ToString(TypedScalarToString::String),
        I::I32ToString => TypedStandardLowering::ToString(TypedScalarToString::I32),
        I::I64ToString => TypedStandardLowering::ToString(TypedScalarToString::I64),
        I::F32ToString => TypedStandardLowering::ToString(TypedScalarToString::F32),
        I::F64ToString => TypedStandardLowering::ToString(TypedScalarToString::F64),
        I::BoolToString => TypedStandardLowering::ToString(TypedScalarToString::Bool),
        I::RuneToString => TypedStandardLowering::ToString(TypedScalarToString::Rune),
        I::OptionIsSome => TypedStandardLowering::Intrinsic(StandardIntrinsic::OptionIsSome {
            value: type_argument(0)?,
        }),
        I::OptionIsNone => TypedStandardLowering::Intrinsic(StandardIntrinsic::OptionIsNone {
            value: type_argument(0)?,
        }),
        I::ResultIsOk => TypedStandardLowering::Intrinsic(StandardIntrinsic::ResultIsOk {
            success: type_argument(0)?,
            error: type_argument(1)?,
        }),
        I::ResultIsErr => TypedStandardLowering::Intrinsic(StandardIntrinsic::ResultIsErr {
            success: type_argument(0)?,
            error: type_argument(1)?,
        }),
        I::OptionUnwrapOr => TypedStandardLowering::Intrinsic(StandardIntrinsic::OptionUnwrapOr {
            value: type_argument(0)?,
        }),
        I::ResultUnwrapOr => TypedStandardLowering::Intrinsic(StandardIntrinsic::ResultUnwrapOr {
            success: type_argument(0)?,
            error: type_argument(1)?,
        }),
        I::F32Floor => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Floor),
        I::F64Floor => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Floor),
        I::F32Ceil => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Ceil),
        I::F64Ceil => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Ceil),
        I::F32Round => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Round),
        I::F64Round => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Round),
        I::F32Sqrt => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Sqrt),
        I::F64Sqrt => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Sqrt),
        I::F32Sin => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Sin),
        I::F64Sin => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Sin),
        I::F32Cos => TypedStandardLowering::Intrinsic(StandardIntrinsic::F32Cos),
        I::F64Cos => TypedStandardLowering::Intrinsic(StandardIntrinsic::F64Cos),
        I::StringContains => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringContains),
        I::StringLen => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringLen),
        I::StringByteLen => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringByteLen),
        I::StringStartsWith => {
            TypedStandardLowering::Intrinsic(StandardIntrinsic::StringStartsWith)
        }
        I::StringEndsWith => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringEndsWith),
        I::StringSubstring => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringSubstring),
        I::StringTrim => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringTrim),
        I::StringSplit => TypedStandardLowering::Intrinsic(StandardIntrinsic::StringSplit),
        I::ArrayLen
        | I::ArrayIsEmpty
        | I::ArrayGet
        | I::ArrayPush
        | I::ArrayPop
        | I::ArrayReserve
        | I::ArrayCapacity
        | I::ArrayClear
        | I::ArrayShrinkToFit => TypedStandardLowering::Intrinsic(lower_standard_array_intrinsic(
            intrinsic,
            type_argument(0)?,
        )),
        I::MapLen => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapLen {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapContains => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapContains {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapGet => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapGet {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapInsert => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapInsert {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapRemove => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapRemove {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::SetNew => TypedStandardLowering::SetNew {
            element: type_argument(0)?,
        },
        I::SetLen => TypedStandardLowering::Intrinsic(StandardIntrinsic::SetLen {
            element: type_argument(0)?,
        }),
        I::SetContains => TypedStandardLowering::Intrinsic(StandardIntrinsic::SetContains {
            element: type_argument(0)?,
        }),
        I::SetInsert => TypedStandardLowering::Intrinsic(StandardIntrinsic::SetInsert {
            element: type_argument(0)?,
        }),
        I::SetRemove => TypedStandardLowering::Intrinsic(StandardIntrinsic::SetRemove {
            element: type_argument(0)?,
        }),
        I::SetClear => TypedStandardLowering::SetClear {
            element: type_argument(0)?,
        },
        I::ArrayFirst => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayFirst {
            element: type_argument(0)?,
        }),
        I::ArrayLast => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayLast {
            element: type_argument(0)?,
        }),
        I::ArraySwap => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArraySwap {
            element: type_argument(0)?,
        }),
        I::ArrayReverse => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayReverse {
            element: type_argument(0)?,
        }),
        I::MapIsEmpty => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapIsEmpty {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapGetOr => TypedStandardLowering::Intrinsic(StandardIntrinsic::MapGetOr {
            key: type_argument(0)?,
            value: type_argument(1)?,
        }),
        I::MapInsertIfAbsent => {
            TypedStandardLowering::Intrinsic(StandardIntrinsic::MapInsertIfAbsent {
                key: type_argument(0)?,
                value: type_argument(1)?,
            })
        }
        I::BufferIsEmpty => TypedStandardLowering::Intrinsic(StandardIntrinsic::BufferIsEmpty {
            element: type_argument(0)?,
        }),
        I::BufferFill => TypedStandardLowering::Intrinsic(StandardIntrinsic::BufferFill {
            element: type_argument(0)?,
        }),
        I::DebugAssert => TypedStandardLowering::Intrinsic(StandardIntrinsic::DebugAssert),
        I::DebugTrap => TypedStandardLowering::Intrinsic(StandardIntrinsic::DebugTrap),
    };
    validate_concrete_standard_lowering(package, arguments, result, lowering, span)?;
    Ok(lowering)
}

fn lower_standard_array_intrinsic(
    intrinsic: nexa_stdlib::Intrinsic,
    element: ValueType,
) -> StandardIntrinsic {
    use nexa_stdlib::Intrinsic as I;

    match intrinsic {
        I::ArrayLen => StandardIntrinsic::ArrayLen { element },
        I::ArrayIsEmpty => StandardIntrinsic::ArrayIsEmpty { element },
        I::ArrayGet => StandardIntrinsic::ArrayGet { element },
        I::ArrayPush => StandardIntrinsic::ArrayPush { element },
        I::ArrayPop => StandardIntrinsic::ArrayPop { element },
        I::ArrayReserve => StandardIntrinsic::ArrayReserve { element },
        I::ArrayCapacity => StandardIntrinsic::ArrayCapacity { element },
        I::ArrayClear => StandardIntrinsic::ArrayClear { element },
        I::ArrayShrinkToFit => StandardIntrinsic::ArrayShrinkToFit { element },
        _ => unreachable!("only array intrinsics reach array lowering"),
    }
}

fn validate_concrete_standard_lowering(
    package: &TypedPackageIr,
    arguments: &[TypedExpressionIr],
    result: &IrType,
    lowering: TypedStandardLowering,
    span: SourceSpan,
) -> Result<(), CompileError> {
    let parameters = arguments
        .iter()
        .map(|argument| lower_type(package, &argument.ty, span))
        .collect::<Result<Vec<_>, _>>()?;
    let result_ir = result;
    let result = lower_type(package, result, span)?;
    match lowering {
        TypedStandardLowering::ToString(ty) => {
            if parameters.as_slice() != [ty.value_type()] || result != ValueType::String {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        TypedStandardLowering::Intrinsic(intrinsic) => {
            if usize::from(intrinsic.argument_count()) != parameters.len()
                || parameters.iter().enumerate().any(|(index, actual)| {
                    intrinsic.argument_type(u16::try_from(index).unwrap_or(u16::MAX))
                        != Some(*actual)
                })
                || intrinsic.result_type() != result
            {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        TypedStandardLowering::SetNew { element } => {
            if !parameters.is_empty() || result != ValueType::Named(set_type(element)) {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
        TypedStandardLowering::SetClear { element } => {
            if parameters.as_slice() != [ValueType::Named(set_type(element))]
                || *result_ir != IrType::Unit
            {
                return Err(CompileError::type_mismatch(None, None, span));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn emit_typed_host_imports(
    package: &TypedPackageIr,
    referenced_functions: &BTreeSet<DefinitionId>,
    builder: &mut ModuleBuilder,
) -> Result<(BTreeMap<DefinitionId, u32>, Option<StableId>), CompileError> {
    let mut hosts = package.metadata().host_bindings.iter();
    let host = hosts.next();
    if hosts.next().is_some() {
        return Err(CompileError::unknown_name(
            "one bytecode module cannot contain multiple Host Contract identities".into(),
            SourceSpan::default(),
        ));
    }
    let host_contract_id = host.map(|host| host.contract_stable_id);
    let mut bindings = host
        .into_iter()
        .flat_map(|host| host.functions.iter())
        .filter(|binding| referenced_functions.contains(&binding.definition))
        .collect::<Vec<_>>();
    // NIDL declaration order is not ABI data. These are deterministic module-local slots only:
    // Runtime resolves each slot through `HostImport::stable_id`, so a referenced subset must
    // never be interpreted as dense ordinals into the complete generated Contract registry.
    bindings.sort_by(|left, right| {
        let left_name = package
            .definition(left.definition)
            .map_or("", |definition| definition.name.as_str());
        let right_name = package
            .definition(right.definition)
            .map_or("", |definition| definition.name.as_str());
        (left.stable_id.0, left_name).cmp(&(right.stable_id.0, right_name))
    });
    let mut function_imports = BTreeMap::new();
    for (expected, binding) in bindings.into_iter().enumerate() {
        let expected = u32::try_from(expected)
            .map_err(|_| CompileError::too_many_registers(SourceSpan::default()))?;
        let span = external_span(binding.source.as_ref());
        let parameters = binding
            .parameters
            .iter()
            .map(|ty| lower_type(package, ty, span))
            .collect::<Result<Vec<_>, _>>()?;
        let async_result = binding
            .async_result
            .as_ref()
            .map(|result| {
                checked_async_result(
                    result,
                    lower_type(package, &result.success, span)?,
                    lower_type(package, &result.error, span)?,
                    span,
                )
            })
            .transpose()?;
        let (mode, result) = match binding.mode {
            IrHostFunctionMode::Sync => {
                if async_result.is_some() {
                    return Err(CompileError::type_mismatch(None, None, span));
                }
                (
                    HostCallMode::Immediate,
                    (binding.result != IrType::Unit)
                        .then(|| lower_type(package, &binding.result, span))
                        .transpose()?,
                )
            }
            IrHostFunctionMode::Request => {
                let async_result = async_result
                    .as_ref()
                    .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
                (
                    HostCallMode::Async,
                    Some(ValueType::Named(async_result.result_type)),
                )
            }
        };
        let actual = builder.host_import(HostImport {
            stable_id: binding.stable_id,
            declaration_fingerprint: binding.declaration_fingerprint,
            capabilities: binding.required_capabilities.clone(),
            parameters,
            result,
            mode,
            fuel_cost: binding.fuel_cost,
            async_result,
        });
        debug_assert_eq!(actual, expected);
        if function_imports
            .insert(binding.definition, actual)
            .is_some()
        {
            return Err(CompileError::duplicate_name(
                package.definition(binding.definition).map_or_else(
                    || format!("definition#{}", binding.definition.0),
                    |value| value.name.clone(),
                ),
                span,
                span,
            ));
        }
    }
    if function_imports.len() != referenced_functions.len() {
        let missing = referenced_functions
            .iter()
            .find(|definition| !function_imports.contains_key(definition))
            .copied()
            .expect("host import count mismatch guarantees a missing referenced function");
        return Err(CompileError::unknown_name(
            package.definition(missing).map_or_else(
                || format!("host definition#{}", missing.0),
                |definition| definition.name.clone(),
            ),
            SourceSpan::default(),
        ));
    }
    Ok((function_imports, host_contract_id))
}

fn checked_async_result(
    result: &HostAsyncResultIr,
    success: ValueType,
    error: ValueType,
    span: SourceSpan,
) -> Result<AsyncResultType, CompileError> {
    let canonical = result_type(success, error);
    if result.result_type != canonical.type_id {
        return Err(CompileError::type_mismatch(None, None, span));
    }
    Ok(AsyncResultType {
        result_type: canonical.type_id,
        success,
        error,
        cancel_policy: match result.cancel_policy {
            IrCancelPolicy::ReturnError => CancelPolicy::ReturnError,
            IrCancelPolicy::CancelTask => CancelPolicy::CancelTask,
        },
        abandon_policy: match result.abandon_policy {
            IrAbandonPolicy::ReturnError => AbandonPolicy::ReturnError,
            IrAbandonPolicy::Trap => AbandonPolicy::Trap,
        },
        cancel_error: result.cancel_error,
        abandon_error: result.abandon_error,
    })
}

fn emit_typed_exports(
    package: &TypedPackageIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    function_plans: &[TypedFunctionPlan<'_>],
    files: &BTreeMap<SourceKey, FileId>,
    standalone_main: Option<StandaloneMainExport>,
    builder: &mut ModuleBuilder,
) -> Result<(), CompileError> {
    let functions = function_plans
        .iter()
        .map(|plan| (plan.definition.id, plan))
        .collect::<BTreeMap<_, _>>();
    let mut exports = package.metadata().exports.iter().collect::<Vec<_>>();
    exports.sort_by(|left, right| {
        (left.stable_id.0, left.name.as_str()).cmp(&(right.stable_id.0, right.name.as_str()))
    });
    for export in exports {
        let plan = functions.get(&export.function).copied().ok_or_else(|| {
            CompileError::unknown_name(
                format!(
                    "export `{}` does not target an emitted function",
                    export.name
                ),
                SourceSpan::default(),
            )
        })?;
        let span = source_span(&plan.definition.span, files)?;
        let analyzed_signature = Signature {
            parameters: export
                .parameters
                .iter()
                .map(|ty| lower_type(package, ty, span))
                .collect::<Result<Vec<_>, _>>()?,
            result: (export.result != IrType::Unit)
                .then(|| lower_type(package, &export.result, span))
                .transpose()?,
        };
        if analyzed_signature != typed_signature(package, plan.function.as_ref(), span)?
            || export.effect != plan.function.effect
        {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        builder.script_export(ScriptExport {
            stable_id: export.stable_id,
            function: function_indices[&export.function],
            signature: analyzed_signature,
            effect: lower_effect(export.effect),
        });
    }
    if let Some(main) = standalone_main {
        let stable_id = standalone_main_stable_id();
        if package
            .metadata()
            .exports
            .iter()
            .any(|export| export.stable_id == stable_id)
        {
            return Err(CompileError::invalid_main_signature(
                "standalone main export identity collides with a contract entrypoint",
                main.definition_span,
            ));
        }
        builder.script_export(ScriptExport {
            stable_id,
            function: main.function_index,
            signature: standalone_main_signature(),
            effect: FunctionEffect::Task,
        });
    }
    Ok(())
}

fn emit_typed_test_exports(
    package: &TypedPackageIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    function_plans: &[TypedFunctionPlan<'_>],
    files: &BTreeMap<SourceKey, FileId>,
    builder: &mut ModuleBuilder,
) -> Result<(), CompileError> {
    if package.compilation_kind() != IrCompilationKind::Test {
        return Ok(());
    }
    let functions = function_plans
        .iter()
        .map(|plan| (plan.definition.id, plan))
        .collect::<BTreeMap<_, _>>();
    let mut occupied = package
        .metadata()
        .exports
        .iter()
        .map(|export| export.stable_id)
        .collect::<BTreeSet<_>>();
    let mut tests = package.metadata().tests.iter().collect::<Vec<_>>();
    tests.sort_by(|left, right| {
        (left.module.as_str(), left.name.as_str(), left.function.0).cmp(&(
            right.module.as_str(),
            right.name.as_str(),
            right.function.0,
        ))
    });
    for test in tests {
        let plan = functions.get(&test.function).copied().ok_or_else(|| {
            CompileError::unknown_name(
                format!("test `{}` does not target an emitted function", test.name),
                source_span(&test.span, files).unwrap_or_default(),
            )
        })?;
        if !plan.function.parameters.is_empty()
            || plan.function.return_type != IrType::Bool
            || plan.function.effect != IrEffect::Immediate
        {
            continue;
        }
        let span = source_span(&test.span, files)?;
        let (identity, stable_id) = stable_symbol(plan.definition, span)?;
        if identity.kind() != nexa_core::SymbolKind::Test {
            return Err(CompileError::type_mismatch(None, None, span));
        }
        if !occupied.insert(stable_id.0) {
            return Err(CompileError::duplicate_name(test.name.clone(), span, span));
        }
        builder.script_export(ScriptExport {
            stable_id: stable_id.0,
            function: function_indices[&test.function],
            signature: Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            effect: FunctionEffect::Immediate,
        });
    }
    Ok(())
}

fn lower_type(
    package: &TypedPackageIr,
    ty: &IrType,
    span: SourceSpan,
) -> Result<ValueType, CompileError> {
    match ty {
        IrType::Unit | IrType::I32 => Ok(ValueType::I32),
        IrType::Bool => Ok(ValueType::Bool),
        IrType::I64 => Ok(ValueType::I64),
        IrType::F32 => Ok(ValueType::F32),
        IrType::F64 => Ok(ValueType::F64),
        IrType::String => Ok(ValueType::String),
        IrType::Rune => Ok(ValueType::Rune),
        IrType::Named(id) => Ok(ValueType::Named(named_type_id(package, *id, span)?)),
        IrType::Option(value) => Ok(ValueType::Named(
            option_type(lower_type(package, value, span)?).type_id,
        )),
        IrType::Result(success, error) => Ok(ValueType::Named(
            result_type(
                lower_type(package, success, span)?,
                lower_type(package, error, span)?,
            )
            .type_id,
        )),
        IrType::Array(element) => Ok(ValueType::Named(array_type(lower_type(
            package, element, span,
        )?))),
        IrType::Map(key, value) => Ok(ValueType::Named(map_type(
            lower_type(package, key, span)?,
            lower_type(package, value, span)?,
        ))),
        IrType::Set(element) => Ok(ValueType::Named(set_type(lower_type(
            package, element, span,
        )?))),
        IrType::Tuple(items) => {
            let items = items
                .iter()
                .map(|item| lower_type(package, item, span))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ValueType::Named(parameterized_type_id("Tuple", &items)))
        }
        IrType::HostRequest(_) => Err(CompileError::unknown_type(
            "HostRequest is an internal awaitable and has no bytecode value type".into(),
            span,
        )),
        IrType::ResourceToken(content) => Ok(ValueType::Named(resource_token_type(
            resource_token_content_id(package, content.as_deref(), span)?,
        ))),
        IrType::Snapshot(content) => {
            let content = lower_type(package, content, span)?;
            let content_type = match content {
                ValueType::Named(type_id) => type_id,
                _ => parameterized_type_id("SnapshotContent", &[content]),
            };
            Ok(ValueType::Named(snapshot_type(content_type)))
        }
        IrType::Buffer(element) => Ok(ValueType::Named(buffer_type(lower_type(
            package, element, span,
        )?))),
        IrType::StateHandle(target) => Ok(ValueType::Named(state_handle_type(lower_type(
            package, target, span,
        )?))),
        IrType::TypeParameter(_) | IrType::Error => Err(CompileError::unknown_type(
            "uninstantiated standard-library type parameter".into(),
            span,
        )),
    }
}

fn resource_token_content_id(
    package: &TypedPackageIr,
    content: Option<&IrType>,
    span: SourceSpan,
) -> Result<StableId, CompileError> {
    let Some(IrType::Named(definition)) = content else {
        return Err(CompileError::unknown_type(
            "ResourceToken requires one concrete nominal content type".into(),
            span,
        ));
    };
    named_type_id(package, *definition, span)
}

fn named_type_id(
    package: &TypedPackageIr,
    definition: DefinitionId,
    span: SourceSpan,
) -> Result<StableId, CompileError> {
    if let Some(host_type) = package
        .metadata()
        .host_bindings
        .iter()
        .flat_map(|host| host.types.iter())
        .find(|ty| ty.definition == definition)
    {
        return Ok(host_type.stable_id);
    }
    match compiler_builtin_type(package, definition) {
        Some("StableId") => {
            let ValueType::Named(type_id) = nexa_bytecode::stable_id_type() else {
                unreachable!("StableId bytecode type is nominal");
            };
            return Ok(type_id);
        }
        Some("StateHandleError") => {
            return Ok(nexa_bytecode::state_handle_error_type().type_id);
        }
        _ => {}
    }
    let definition = package
        .definition(definition)
        .ok_or_else(|| CompileError::unknown_type(format!("definition#{}", definition.0), span))?;
    Ok(stable_symbol(definition, span)?.1.0)
}

fn compiler_builtin_type(package: &TypedPackageIr, definition: DefinitionId) -> Option<&str> {
    let definition = package.definition(definition)?;
    (definition.kind == DefinitionKind::StandardLibrary
        && definition.module.as_str() == "nexa.builtin")
        .then_some(definition.name.as_str())
}

fn builtin_variant_layout(
    package: &TypedPackageIr,
    variant: BuiltinVariantIr,
    ty: &IrType,
    span: SourceSpan,
) -> Result<(EnumType, EnumVariant), CompileError> {
    let (enum_type, index) = match (variant, ty) {
        (BuiltinVariantIr::OptionNone, IrType::Option(value)) => {
            (option_type(lower_type(package, value, span)?), 0)
        }
        (BuiltinVariantIr::OptionSome, IrType::Option(value)) => {
            (option_type(lower_type(package, value, span)?), 1)
        }
        (BuiltinVariantIr::ResultOk, IrType::Result(success, error)) => (
            result_type(
                lower_type(package, success, span)?,
                lower_type(package, error, span)?,
            ),
            0,
        ),
        (BuiltinVariantIr::ResultErr, IrType::Result(success, error)) => (
            result_type(
                lower_type(package, success, span)?,
                lower_type(package, error, span)?,
            ),
            1,
        ),
        _ => return Err(CompileError::type_mismatch(None, None, span)),
    };
    let variant = enum_type
        .variants
        .get(index)
        .cloned()
        .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
    Ok((enum_type, variant))
}

fn equality_instruction(
    ir_ty: &IrType,
    ty: ValueType,
    dst: u16,
    lhs: u16,
    rhs: u16,
    layouts: &TypedLayoutContext,
    span: SourceSpan,
) -> Result<Instruction, CompileError> {
    match ir_ty {
        IrType::Unit
        | IrType::I32
        | IrType::I64
        | IrType::F32
        | IrType::F64
        | IrType::Bool
        | IrType::Rune => Ok(Instruction::CompareEq { dst, lhs, rhs }),
        IrType::String => Ok(Instruction::StringEqual { dst, lhs, rhs }),
        IrType::Tuple(_) => Ok(Instruction::StructEqual { dst, lhs, rhs }),
        IrType::Option(_) | IrType::Result(_, _) => Ok(Instruction::EnumEqual { dst, lhs, rhs }),
        IrType::Named(definition) => {
            if let Some(layout) = layouts.aggregates.get(definition) {
                return Ok(match layout.kind {
                    TypedAggregateKind::Struct => Instruction::StructEqual { dst, lhs, rhs },
                    TypedAggregateKind::Class => Instruction::ClassEqual { dst, lhs, rhs },
                });
            }
            if layouts.enums.contains_key(definition) {
                return Ok(Instruction::EnumEqual { dst, lhs, rhs });
            }
            Err(CompileError::type_mismatch(None, Some(ty), span))
        }
        IrType::Array(_)
        | IrType::Map(_, _)
        | IrType::Set(_)
        | IrType::HostRequest(_)
        | IrType::ResourceToken(_)
        | IrType::Snapshot(_)
        | IrType::Buffer(_)
        | IrType::StateHandle(_)
        | IrType::TypeParameter(_)
        | IrType::Error => Err(CompileError::type_mismatch(None, Some(ty), span)),
    }
}

fn typed_signature(
    package: &TypedPackageIr,
    function: &TypedFunctionIr,
    span: SourceSpan,
) -> Result<Signature, CompileError> {
    let parameters = function
        .parameters
        .iter()
        .map(|id| {
            let definition = package
                .definition(*id)
                .expect("TypedPackageIr validates parameter IDs");
            lower_type(package, &definition.ty, span)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let result = (function.return_type != IrType::Unit)
        .then(|| lower_type(package, &function.return_type, span))
        .transpose()?;
    Ok(Signature { parameters, result })
}

fn collect_type_strings(
    package: &TypedPackageIr,
    function: &TypedFunctionIr,
    strings: &mut BTreeSet<String>,
) {
    let has_string = function
        .parameters
        .iter()
        .chain(&function.locals)
        .filter_map(|id| package.definition(*id))
        .any(|definition| definition.ty == IrType::String)
        || function.return_type == IrType::String;
    if has_string {
        strings.insert(String::new());
    }
}

const AGGREGATE_FORMAT_SUFFIX: &str = " }";

fn aggregate_empty_format(type_name: &str) -> String {
    format!("{type_name} {{}}")
}

fn aggregate_field_format(type_name: &str, field_name: &str, first: bool) -> String {
    if first {
        format!("{type_name} {{ {field_name}: ")
    } else {
        format!(", {field_name}: ")
    }
}

fn collect_aggregate_format_strings(package: &TypedPackageIr, strings: &mut BTreeSet<String>) {
    for module in package.modules() {
        for declaration in module.declarations.iter() {
            let TypedDeclarationBody::TypeLayout(
                layout @ (TypedTypeLayoutIr::Struct { .. } | TypedTypeLayoutIr::Class { .. }),
            ) = &declaration.body
            else {
                continue;
            };
            let Some(definition) = package.definition(declaration.definition) else {
                continue;
            };
            let fields = match layout {
                TypedTypeLayoutIr::Struct { fields } | TypedTypeLayoutIr::Class { fields, .. } => {
                    fields
                }
                TypedTypeLayoutIr::Enum { .. } => unreachable!("pattern excludes Enum layouts"),
            };
            if fields.is_empty() {
                strings.insert(aggregate_empty_format(&definition.name));
                continue;
            }
            strings.insert(AGGREGATE_FORMAT_SUFFIX.to_owned());
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by_key(|field| field.order);
            for (index, field) in fields.into_iter().enumerate() {
                if let Some(field) = package.definition(field.definition) {
                    strings.insert(aggregate_field_format(
                        &definition.name,
                        &field.name,
                        index == 0,
                    ));
                }
            }
        }
    }
}

fn collect_block_codegen_inputs(block: &TypedBlockIr, inputs: &mut CodegenInputs) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    collect_expression_codegen_inputs(value, inputs);
                }
            }
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
                collect_place_codegen_inputs(target, inputs);
                collect_expression_codegen_inputs(value, inputs);
            }
            TypedStatementIr::Expression(value) => {
                collect_expression_codegen_inputs(value, inputs);
            }
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_expression_codegen_inputs(condition, inputs);
                collect_block_codegen_inputs(then_block, inputs);
                if let Some(else_block) = else_block {
                    collect_block_codegen_inputs(else_block, inputs);
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                collect_expression_codegen_inputs(condition, inputs);
                collect_block_codegen_inputs(body, inputs);
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            }
            | TypedStatementIr::DynamicRangeFor {
                start, end, body, ..
            } => {
                collect_expression_codegen_inputs(start, inputs);
                collect_expression_codegen_inputs(end, inputs);
                collect_block_codegen_inputs(body, inputs);
            }
            TypedStatementIr::CollectionFor { iterable, body, .. } => {
                collect_expression_codegen_inputs(iterable, inputs);
                collect_block_codegen_inputs(body, inputs);
            }
            TypedStatementIr::Defer { captures, .. } => {
                for capture in captures {
                    collect_expression_codegen_inputs(capture, inputs);
                }
            }
            TypedStatementIr::Break
            | TypedStatementIr::Continue
            | TypedStatementIr::Yield { .. } => {}
        }
    }
    if let Some(tail) = &block.tail {
        collect_expression_codegen_inputs(tail, inputs);
    }
}

fn collect_place_codegen_inputs(place: &TypedPlaceIr, inputs: &mut CodegenInputs) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, .. } => collect_place_codegen_inputs(base, inputs),
        TypedPlaceIr::ClassField { object: base, .. } | TypedPlaceIr::StateField { base, .. } => {
            collect_expression_codegen_inputs(base, inputs);
        }
        TypedPlaceIr::Index { base, index } => {
            collect_expression_codegen_inputs(base, inputs);
            collect_expression_codegen_inputs(index, inputs);
        }
    }
}

fn collect_expression_codegen_inputs(expression: &TypedExpressionIr, inputs: &mut CodegenInputs) {
    match &expression.kind {
        TypedExpressionKind::Literal(IrLiteral::String(value)) => {
            inputs.strings.insert(value.clone());
        }
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Await(operand)
        | TypedExpressionKind::Field { base: operand, .. }
        | TypedExpressionKind::StateField { base: operand, .. } => {
            collect_expression_codegen_inputs(operand, inputs);
        }
        TypedExpressionKind::Binary { left, right, .. }
        | TypedExpressionKind::Index {
            base: left,
            index: right,
        } => {
            collect_expression_codegen_inputs(left, inputs);
            collect_expression_codegen_inputs(right, inputs);
        }
        TypedExpressionKind::Call { arguments, .. }
        | TypedExpressionKind::StandardCall { arguments, .. }
        | TypedExpressionKind::BuiltinCall { arguments, .. }
        | TypedExpressionKind::Array(arguments)
        | TypedExpressionKind::Tuple(arguments) => {
            for argument in arguments {
                collect_expression_codegen_inputs(argument, inputs);
            }
        }
        TypedExpressionKind::HostCall {
            function,
            arguments,
            ..
        } => {
            inputs.host_functions.insert(*function);
            for argument in arguments {
                collect_expression_codegen_inputs(argument, inputs);
            }
        }
        TypedExpressionKind::Construct { fields, .. } => {
            for (_, value) in fields {
                collect_expression_codegen_inputs(value, inputs);
            }
        }
        TypedExpressionKind::ClassConstruct { fields, update, .. } => {
            if let Some(update) = update {
                collect_expression_codegen_inputs(update, inputs);
            }
            for (_, value) in fields {
                collect_expression_codegen_inputs(value, inputs);
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_codegen_inputs(payload, inputs);
            }
        }
        TypedExpressionKind::StringInterpolation(parts) => {
            inputs.strings.insert(String::new());
            for part in parts {
                collect_expression_codegen_inputs(part, inputs);
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            collect_expression_codegen_inputs(value, inputs);
            for arm in arms {
                collect_expression_codegen_inputs(&arm.value, inputs);
            }
        }
        TypedExpressionKind::Try(value) => collect_expression_codegen_inputs(value, inputs),
        TypedExpressionKind::Update { base, fields } => {
            collect_expression_codegen_inputs(base, inputs);
            for (_, value) in fields {
                collect_expression_codegen_inputs(value, inputs);
            }
        }
        TypedExpressionKind::Migration(intrinsic) => {
            collect_migration_codegen_inputs(intrinsic, inputs);
        }
        TypedExpressionKind::Literal(_)
        | TypedExpressionKind::Reference(_)
        | TypedExpressionKind::PersistentStateGet { .. }
        | TypedExpressionKind::Yield => {}
    }
}

fn collect_migration_codegen_inputs(intrinsic: &MigrationIntrinsicIr, inputs: &mut CodegenInputs) {
    match intrinsic {
        MigrationIntrinsicIr::OldFieldGet { object, .. } => {
            collect_expression_codegen_inputs(object, inputs);
        }
        MigrationIntrinsicIr::NewSet { object, value, .. } => {
            collect_expression_codegen_inputs(object, inputs);
            collect_expression_codegen_inputs(value, inputs);
        }
        MigrationIntrinsicIr::Replace { target, .. } => {
            collect_expression_codegen_inputs(target, inputs);
        }
        MigrationIntrinsicIr::OldGet { .. }
        | MigrationIntrinsicIr::NewCreate { .. }
        | MigrationIntrinsicIr::Preserve { .. }
        | MigrationIntrinsicIr::Delete { .. }
        | MigrationIntrinsicIr::Finish => {}
    }
}

fn stable_symbol(
    definition: &Definition,
    span: SourceSpan,
) -> Result<(&CanonicalSymbolIdentity, StableSymbolId), CompileError> {
    let symbol = definition.stable_symbol.as_ref().ok_or_else(|| {
        CompileError::unknown_name(
            format!(
                "typed definition `{}` has no analysis-assigned stable identity",
                definition.name
            ),
            span,
        )
    })?;
    if symbol.runtime_id != symbol.canonical.runtime_id() {
        return Err(CompileError::unknown_name(
            format!(
                "typed definition `{}` has a forged stable runtime identity",
                definition.name
            ),
            span,
        ));
    }
    Ok((&symbol.canonical, symbol.runtime_id))
}

fn package_visibility(visibility: DeclarationVisibility) -> PackageVisibility {
    match visibility {
        DeclarationVisibility::Private => PackageVisibility::Private,
        DeclarationVisibility::Package => PackageVisibility::Package,
        DeclarationVisibility::Public => PackageVisibility::Public,
    }
}

fn typed_debug_info(
    package: &TypedPackageIr,
    modules: &[&nexa_analysis::TypedModuleIr],
    functions: &[TypedFunctionPlan<'_>],
    files: &BTreeMap<SourceKey, FileId>,
    entry_module: &str,
    standalone_main: Option<StandaloneMainExport>,
    host_import_indices: &BTreeMap<DefinitionId, u32>,
) -> Result<PackageDebugInfo, CompileError> {
    let function_by_source = functions.iter().fold(
        BTreeMap::<SourceKey, Vec<&TypedFunctionPlan<'_>>>::new(),
        |mut map, plan| {
            map.entry(plan.definition.span.source.clone())
                .or_default()
                .push(plan);
            map
        },
    );
    let mut module_debug = Vec::new();
    for module in modules {
        let source_span = full_source_span(module, files);
        let mut function_indices = function_by_source
            .get(&module.source)
            .map_or_else(Vec::new, |plans| {
                plans.iter().map(|plan| plan.index).collect()
            });
        if module.package_id == *package.package_id()
            && module.module.as_str() == entry_module
            && let Some(main) = standalone_main
            && main.function_index != main.source_function_index
        {
            function_indices.push(main.function_index);
        }
        module_debug.push(PackageModuleDebugInfo {
            package_id: module.package_id.as_str().to_owned(),
            module_path: module.module.as_str().to_owned(),
            file: files[&module.source],
            definition_span: source_span,
            source_span,
            function_indices,
        });
    }
    let mut functions = functions
        .iter()
        .map(|plan| {
            let definition_span = source_span(&plan.definition.span, files)?;
            let (identity, stable_id) = stable_symbol(plan.definition, definition_span)?;
            Ok(PackageFunctionDebugInfo {
                function_index: plan.index,
                package_id: plan.definition.package_id.as_str().to_owned(),
                module_path: plan.definition.module.as_str().to_owned(),
                name: plan.definition.name.clone(),
                canonical_identity: identity.clone(),
                stable_id,
                definition_span,
                effect: lower_effect(plan.function.effect),
                visibility: package_visibility(plan.definition.visibility),
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    if let Some(main) = standalone_main
        && main.function_index != main.source_function_index
    {
        let canonical_identity = CanonicalSymbolIdentity::explicit(
            package.package_id().as_str(),
            nexa_core::SymbolKind::Task,
            STANDALONE_MAIN_WRAPPER_NAME,
        );
        functions.push(PackageFunctionDebugInfo {
            function_index: main.function_index,
            package_id: package.package_id().as_str().to_owned(),
            module_path: entry_module.to_owned(),
            name: STANDALONE_MAIN_WRAPPER_NAME.into(),
            stable_id: canonical_identity.runtime_id(),
            canonical_identity,
            definition_span: main.definition_span,
            effect: FunctionEffect::Task,
            visibility: PackageVisibility::Private,
        });
    }
    let host_imports = typed_host_import_debug_info(package, files, host_import_indices)?;
    Ok(PackageDebugInfo {
        root_package_id: package.package_id().as_str().to_owned(),
        entry_module: entry_module.to_owned(),
        modules: module_debug,
        functions,
        host_imports,
    })
}

fn typed_host_import_debug_info(
    package: &TypedPackageIr,
    files: &BTreeMap<SourceKey, FileId>,
    host_import_indices: &BTreeMap<DefinitionId, u32>,
) -> Result<Vec<PackageHostImportDebugInfo>, CompileError> {
    let mut imports = Vec::new();
    for host in package.metadata().host_bindings.iter() {
        let contract = package
            .definition(host.contract)
            .expect("TypedPackageIr validates Host Contract IDs");
        let contract_span = catalog_source_span(package, &contract.span, files)?;
        for function in &host.functions {
            let Some(import_index) = host_import_indices.get(&function.definition).copied() else {
                continue;
            };
            let definition = package
                .definition(function.definition)
                .expect("TypedPackageIr validates host function IDs");
            imports.push(PackageHostImportDebugInfo {
                import_index,
                stable_id: function.stable_id,
                contract_id: host.contract_stable_id,
                contract_name: contract.name.clone(),
                function_name: definition.name.clone(),
                contract_span,
                declaration_span: function.source.as_ref().map_or_else(
                    || catalog_source_span(package, &definition.span, files),
                    |source| Ok(external_span(Some(source))),
                )?,
            });
        }
    }
    imports.sort_by_key(|import| import.import_index);
    for (expected, import) in imports.iter().enumerate() {
        if import.import_index != u32::try_from(expected).unwrap_or(u32::MAX) {
            return Err(CompileError::unknown_name(
                "typed host import debug indices must be dense and unique".into(),
                import.declaration_span,
            ));
        }
    }
    Ok(imports)
}

fn typed_public_symbols(
    package: &TypedPackageIr,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<Vec<PackagePublicSymbol>, CompileError> {
    package
        .definitions()
        .iter()
        .filter(|definition| {
            definition.package_id == *package.package_id()
                && definition.visibility == DeclarationVisibility::Public
                && matches!(
                    definition.kind,
                    DefinitionKind::Function
                        | DefinitionKind::Task
                        | DefinitionKind::Struct
                        | DefinitionKind::Enum
                        | DefinitionKind::Class
                        | DefinitionKind::Const
                )
        })
        .map(|definition| {
            let definition_span = source_span(&definition.span, files)?;
            let (identity, stable_id) = stable_symbol(definition, definition_span)?;
            Ok(PackagePublicSymbol {
                package_id: definition.package_id.as_str().to_owned(),
                module_path: definition.module.as_str().to_owned(),
                name: definition.name.clone(),
                kind: identity.kind(),
                canonical_identity: identity.clone(),
                stable_id,
                definition_span,
            })
        })
        .collect()
}

fn typed_state_surface(
    package: &TypedPackageIr,
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<Vec<PackageStateTypeInfo>, CompileError> {
    let mut states = package
        .metadata()
        .state_types
        .iter()
        .map(|state| {
            let definition = package
                .definition(state.definition)
                .expect("TypedPackageIr validates state IDs");
            let definition_span = source_span(&definition.span, files)?;
            let (identity, stable_id) = stable_symbol(definition, definition_span)?;
            if stable_id != state.stable_id {
                return Err(CompileError::invalid_reload_metadata(
                    "analyzed state-type identity disagrees with definition identity",
                    definition_span,
                ));
            }
            let mut fields = state
                .fields
                .iter()
                .map(|field| {
                    let definition = package
                        .definition(field.definition)
                        .expect("TypedPackageIr validates state field IDs");
                    let definition_span = source_span(&definition.span, files)?;
                    let (identity, stable_id) = stable_symbol(definition, definition_span)?;
                    if stable_id != field.stable_id {
                        return Err(CompileError::invalid_reload_metadata(
                            "analyzed state-field identity disagrees with definition identity",
                            definition_span,
                        ));
                    }
                    Ok(PackageStateFieldInfo {
                        name: definition.name.clone(),
                        canonical_identity: identity.clone(),
                        stable_id,
                        definition_span,
                    })
                })
                .collect::<Result<Vec<_>, CompileError>>()?;
            if package.compilation_kind() != IrCompilationKind::ReplCell {
                fields.sort_by(|left, right| {
                    (&left.canonical_identity, left.stable_id)
                        .cmp(&(&right.canonical_identity, right.stable_id))
                });
            }
            Ok(PackageStateTypeInfo {
                package_id: definition.package_id.as_str().to_owned(),
                module_path: definition.module.as_str().to_owned(),
                name: definition.name.clone(),
                version: state.version,
                canonical_identity: identity.clone(),
                stable_id,
                definition_span,
                fields,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    states.sort_by(|left, right| {
        (&left.canonical_identity, left.stable_id)
            .cmp(&(&right.canonical_identity, right.stable_id))
    });
    Ok(states)
}

fn typed_test_info(
    package: &TypedPackageIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    function_plans: &[TypedFunctionPlan<'_>],
    files: &BTreeMap<SourceKey, FileId>,
) -> Result<Vec<PackageTestInfo>, CompileError> {
    let functions = function_plans
        .iter()
        .map(|plan| (plan.definition.id, plan))
        .collect::<BTreeMap<_, _>>();
    let mut tests = package
        .metadata()
        .tests
        .iter()
        .map(|test| {
            let plan = functions.get(&test.function).copied().ok_or_else(|| {
                CompileError::unknown_name(
                    format!("test `{}` does not target an emitted function", test.name),
                    source_span(&test.span, files).unwrap_or_default(),
                )
            })?;
            let definition_span = source_span(&test.span, files)?;
            let (identity, stable_id) = stable_symbol(plan.definition, definition_span)?;
            let rejection = if !plan.function.parameters.is_empty() {
                Some(PackageTestRejection::ParametersMustBeEmpty)
            } else if plan.function.return_type != IrType::Bool {
                Some(PackageTestRejection::ResultMustBeBool)
            } else if plan.function.effect != IrEffect::Immediate {
                Some(PackageTestRejection::EffectMustBeImmediate)
            } else {
                None
            };
            Ok(PackageTestInfo {
                package_id: plan.definition.package_id.as_str().to_owned(),
                module_path: test.module.as_str().to_owned(),
                name: test.name.clone(),
                function_index: function_indices[&test.function],
                canonical_identity: identity.clone(),
                stable_id,
                definition_span,
                effect: lower_effect(plan.function.effect),
                rejection,
            })
        })
        .collect::<Result<Vec<_>, CompileError>>()?;
    tests.sort_by(|left, right| {
        (
            left.package_id.as_str(),
            left.module_path.as_str(),
            left.name.as_str(),
            left.function_index,
        )
            .cmp(&(
                right.package_id.as_str(),
                right.module_path.as_str(),
                right.name.as_str(),
                right.function_index,
            ))
    });
    Ok(tests)
}

fn typed_test_call_graph(
    package: &TypedPackageIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    function_plans: &[TypedFunctionPlan<'_>],
) -> Vec<PackageTestCallGraphNode> {
    let state_fields = package
        .metadata()
        .state_types
        .iter()
        .flat_map(|state| state.fields.iter().map(|field| field.definition))
        .collect::<BTreeSet<_>>();
    let mut nodes = function_plans
        .iter()
        .map(|plan| {
            let mut calls = BTreeSet::new();
            let mut forbidden_effects = BTreeSet::new();
            match plan.function.effect {
                IrEffect::Task => {
                    forbidden_effects.insert(PackageTestForbiddenEffect::Task);
                }
                IrEffect::Migration | IrEffect::Cleanup => {
                    forbidden_effects.insert(PackageTestForbiddenEffect::Migration);
                }
                IrEffect::Activation => {
                    forbidden_effects.insert(PackageTestForbiddenEffect::Activation);
                }
                IrEffect::Ordinary | IrEffect::Immediate => {}
            }
            collect_block_call_graph(
                package,
                &plan.function.body,
                function_indices,
                &state_fields,
                &mut calls,
                &mut forbidden_effects,
            );
            PackageTestCallGraphNode {
                function_index: plan.index,
                calls: calls.into_iter().collect(),
                forbidden_effects,
            }
        })
        .collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.function_index);
    nodes
}

fn is_state_type(package: &TypedPackageIr, definition: DefinitionId) -> bool {
    package
        .metadata()
        .state_types
        .iter()
        .any(|state| state.definition == definition)
}

#[allow(clippy::too_many_lines)]
fn collect_block_call_graph(
    package: &TypedPackageIr,
    block: &TypedBlockIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    state_fields: &BTreeSet<DefinitionId>,
    calls: &mut BTreeSet<u32>,
    effects: &mut BTreeSet<PackageTestForbiddenEffect>,
) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    collect_expression_call_graph(
                        package,
                        value,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
            }
            TypedStatementIr::Assign { target, value }
            | TypedStatementIr::CompoundAssign { target, value, .. } => {
                collect_place_call_graph(
                    package,
                    target,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
            TypedStatementIr::Expression(value) => collect_expression_call_graph(
                package,
                value,
                function_indices,
                state_fields,
                calls,
                effects,
            ),
            TypedStatementIr::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_expression_call_graph(
                    package,
                    condition,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
                collect_block_call_graph(
                    package,
                    then_block,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
                if let Some(else_block) = else_block {
                    collect_block_call_graph(
                        package,
                        else_block,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
            }
            TypedStatementIr::While {
                condition, body, ..
            } => {
                collect_expression_call_graph(
                    package,
                    condition,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
                collect_block_call_graph(
                    package,
                    body,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
            TypedStatementIr::StaticRangeFor {
                start, end, body, ..
            }
            | TypedStatementIr::DynamicRangeFor {
                start, end, body, ..
            } => {
                for value in [start, end] {
                    collect_expression_call_graph(
                        package,
                        value,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
                collect_block_call_graph(
                    package,
                    body,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
            TypedStatementIr::CollectionFor { iterable, body, .. } => {
                collect_expression_call_graph(
                    package,
                    iterable,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
                collect_block_call_graph(
                    package,
                    body,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
            TypedStatementIr::Defer { cleanup, captures } => {
                if let Some(function) = function_indices.get(cleanup) {
                    calls.insert(*function);
                }
                for capture in captures {
                    collect_expression_call_graph(
                        package,
                        capture,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
            }
            TypedStatementIr::Yield { .. } => {
                effects.insert(PackageTestForbiddenEffect::Yield);
            }
            TypedStatementIr::Break | TypedStatementIr::Continue => {}
        }
    }
    if let Some(tail) = &block.tail {
        collect_expression_call_graph(
            package,
            tail,
            function_indices,
            state_fields,
            calls,
            effects,
        );
    }
}

fn collect_place_call_graph(
    package: &TypedPackageIr,
    place: &TypedPlaceIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    state_fields: &BTreeSet<DefinitionId>,
    calls: &mut BTreeSet<u32>,
    effects: &mut BTreeSet<PackageTestForbiddenEffect>,
) {
    match place {
        TypedPlaceIr::Definition(_) => {}
        TypedPlaceIr::Field { base, field } => {
            if state_fields.contains(field) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            collect_place_call_graph(
                package,
                base,
                function_indices,
                state_fields,
                calls,
                effects,
            );
        }
        TypedPlaceIr::ClassField {
            object: base,
            field,
        }
        | TypedPlaceIr::StateField { base, field } => {
            if state_fields.contains(field) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            collect_expression_call_graph(
                package,
                base,
                function_indices,
                state_fields,
                calls,
                effects,
            );
        }
        TypedPlaceIr::Index { base, index } => {
            for value in [base.as_ref(), index.as_ref()] {
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn collect_expression_call_graph(
    package: &TypedPackageIr,
    expression: &TypedExpressionIr,
    function_indices: &BTreeMap<DefinitionId, u32>,
    state_fields: &BTreeSet<DefinitionId>,
    calls: &mut BTreeSet<u32>,
    effects: &mut BTreeSet<PackageTestForbiddenEffect>,
) {
    match &expression.kind {
        TypedExpressionKind::Literal(_) => {}
        TypedExpressionKind::Reference(definition) => {
            if is_state_type(package, *definition) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
        }
        TypedExpressionKind::PersistentStateGet { .. } => {
            effects.insert(PackageTestForbiddenEffect::PersistentState);
        }
        TypedExpressionKind::Unary { operand, .. }
        | TypedExpressionKind::Try(operand)
        | TypedExpressionKind::Field { base: operand, .. } => collect_expression_call_graph(
            package,
            operand,
            function_indices,
            state_fields,
            calls,
            effects,
        ),
        TypedExpressionKind::StateField {
            base: operand,
            field,
        } => {
            effects.insert(PackageTestForbiddenEffect::PersistentState);
            if state_fields.contains(field) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            collect_expression_call_graph(
                package,
                operand,
                function_indices,
                state_fields,
                calls,
                effects,
            );
        }
        TypedExpressionKind::Await(value) => {
            effects.insert(PackageTestForbiddenEffect::Await);
            collect_expression_call_graph(
                package,
                value,
                function_indices,
                state_fields,
                calls,
                effects,
            );
        }
        TypedExpressionKind::Yield => {
            effects.insert(PackageTestForbiddenEffect::Yield);
        }
        TypedExpressionKind::Binary { left, right, .. }
        | TypedExpressionKind::Index {
            base: left,
            index: right,
        } => {
            for value in [left.as_ref(), right.as_ref()] {
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Call { callee, arguments } => {
            if let Some(function) = function_indices.get(callee) {
                calls.insert(*function);
            }
            for argument in arguments {
                collect_expression_call_graph(
                    package,
                    argument,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::StandardCall {
            function,
            arguments,
            ..
        } => {
            if let Some(function) = function_indices.get(function) {
                calls.insert(*function);
            }
            for argument in arguments {
                collect_expression_call_graph(
                    package,
                    argument,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::BuiltinCall {
            operation,
            arguments,
            ..
        } => {
            if matches!(
                operation,
                BuiltinOperationIr::StateHandleResolve
                    | BuiltinOperationIr::StateHandleIsAlive
                    | BuiltinOperationIr::StateHandleStableId
                    | BuiltinOperationIr::StateHandleGeneration
                    | BuiltinOperationIr::StateHandleEqual
                    | BuiltinOperationIr::StateHandleHash
            ) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            for argument in arguments {
                collect_expression_call_graph(
                    package,
                    argument,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::HostCall { arguments, .. } => {
            effects.insert(PackageTestForbiddenEffect::Host);
            for argument in arguments {
                collect_expression_call_graph(
                    package,
                    argument,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Construct { definition, fields } => {
            if is_state_type(package, *definition) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            for (field, value) in fields {
                if state_fields.contains(field) {
                    effects.insert(PackageTestForbiddenEffect::PersistentState);
                }
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::ClassConstruct {
            definition,
            fields,
            update,
        } => {
            if is_state_type(package, *definition) {
                effects.insert(PackageTestForbiddenEffect::PersistentState);
            }
            if let Some(update) = update {
                collect_expression_call_graph(
                    package,
                    update,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
            for (field, value) in fields {
                if state_fields.contains(field) {
                    effects.insert(PackageTestForbiddenEffect::PersistentState);
                }
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::EnumConstruct { payload, .. }
        | TypedExpressionKind::BuiltinVariant { payload, .. } => {
            if let Some(payload) = payload {
                collect_expression_call_graph(
                    package,
                    payload,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Array(values)
        | TypedExpressionKind::Tuple(values)
        | TypedExpressionKind::StringInterpolation(values) => {
            for value in values {
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Match { value, arms } => {
            collect_expression_call_graph(
                package,
                value,
                function_indices,
                state_fields,
                calls,
                effects,
            );
            for arm in arms {
                collect_expression_call_graph(
                    package,
                    &arm.value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Update { base, fields } => {
            collect_expression_call_graph(
                package,
                base,
                function_indices,
                state_fields,
                calls,
                effects,
            );
            for (field, value) in fields {
                if state_fields.contains(field) {
                    effects.insert(PackageTestForbiddenEffect::PersistentState);
                }
                collect_expression_call_graph(
                    package,
                    value,
                    function_indices,
                    state_fields,
                    calls,
                    effects,
                );
            }
        }
        TypedExpressionKind::Migration(intrinsic) => {
            effects.insert(PackageTestForbiddenEffect::PersistentState);
            match intrinsic {
                MigrationIntrinsicIr::OldFieldGet { object, .. } => {
                    collect_expression_call_graph(
                        package,
                        object,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
                MigrationIntrinsicIr::NewSet { object, value, .. } => {
                    for expression in [object.as_ref(), value.as_ref()] {
                        collect_expression_call_graph(
                            package,
                            expression,
                            function_indices,
                            state_fields,
                            calls,
                            effects,
                        );
                    }
                }
                MigrationIntrinsicIr::Replace { target, .. } => {
                    collect_expression_call_graph(
                        package,
                        target,
                        function_indices,
                        state_fields,
                        calls,
                        effects,
                    );
                }
                MigrationIntrinsicIr::OldGet { .. }
                | MigrationIntrinsicIr::NewCreate { .. }
                | MigrationIntrinsicIr::Preserve { .. }
                | MigrationIntrinsicIr::Delete { .. }
                | MigrationIntrinsicIr::Finish => {}
            }
        }
    }
}

fn collect_safepoints(code: &[Instruction]) -> Vec<u32> {
    let mut safepoints = code
        .iter()
        .enumerate()
        .filter_map(|(pc, instruction)| {
            let pc = u32::try_from(pc).ok()?;
            let explicit = matches!(
                instruction,
                Instruction::Safepoint
                    | Instruction::LoadString { .. }
                    | Instruction::StringLen { .. }
                    | Instruction::StringEqual { .. }
                    | Instruction::StringConcat { .. }
                    | Instruction::StringBuild { .. }
                    | Instruction::StringRuneAt { .. }
                    | Instruction::StringHash { .. }
                    | Instruction::I32ToString { .. }
                    | Instruction::I64ToString { .. }
                    | Instruction::F32ToString { .. }
                    | Instruction::F64ToString { .. }
                    | Instruction::BoolToString { .. }
                    | Instruction::RuneToString { .. }
                    | Instruction::StringToString { .. }
                    | Instruction::StandardIntrinsic { .. }
                    | Instruction::ClassNew { .. }
                    | Instruction::ArrayNew { .. }
                    | Instruction::ArrayLen { .. }
                    | Instruction::ArrayGet { .. }
                    | Instruction::ArrayFieldGet { .. }
                    | Instruction::ArraySet { .. }
                    | Instruction::ArrayPush { .. }
                    | Instruction::ArrayPushRow { .. }
                    | Instruction::ArrayPop { .. }
                    | Instruction::ArrayInsert { .. }
                    | Instruction::ArrayRemove { .. }
                    | Instruction::ArrayClear { .. }
                    | Instruction::MapNew { .. }
                    | Instruction::MapLen { .. }
                    | Instruction::MapGet { .. }
                    | Instruction::MapSet { .. }
                    | Instruction::MapRemove { .. }
                    | Instruction::MapContains { .. }
                    | Instruction::MapClear { .. }
                    | Instruction::BufferLen { .. }
                    | Instruction::BufferGet { .. }
                    | Instruction::BufferSet { .. }
                    | Instruction::BufferSlice { .. }
                    | Instruction::BufferCopy { .. }
                    | Instruction::SetNew { .. }
                    | Instruction::SetLen { .. }
                    | Instruction::SetContains { .. }
                    | Instruction::SetInsert { .. }
                    | Instruction::SetRemove { .. }
                    | Instruction::SetClear { .. }
                    | Instruction::IterNew { .. }
                    | Instruction::IterNext { .. }
                    | Instruction::Yield
                    | Instruction::Call { .. }
                    | Instruction::HostCall { .. }
                    | Instruction::StateCurrentGet { .. }
                    | Instruction::StateHandleResolve { .. }
                    | Instruction::Return { .. }
                    | Instruction::ReturnVoid
                    | Instruction::Trap
                    | Instruction::CleanupReturn
            );
            let back_edge = matches!(instruction, Instruction::Jump { target } if *target <= pc)
                || matches!(
                    instruction,
                    Instruction::JumpIfFalse { target, .. } if *target <= pc
                );
            (pc == 0 || explicit || back_edge).then_some(pc)
        })
        .collect::<Vec<_>>();
    for (pc, instruction) in code.iter().enumerate() {
        if matches!(
            instruction,
            Instruction::HostCall { .. } | Instruction::Yield
        ) && pc + 1 < code.len()
        {
            safepoints.push(u32::try_from(pc + 1).expect("instruction index is bounded"));
        }
    }
    safepoints.sort_unstable();
    safepoints.dedup();
    safepoints
}

fn empty_debug_info(package: &TypedPackageIr, entry_module: &str) -> PackageDebugInfo {
    PackageDebugInfo {
        root_package_id: package.package_id().as_str().to_owned(),
        entry_module: entry_module.to_owned(),
        modules: Vec::new(),
        functions: Vec::new(),
        host_imports: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        STANDALONE_MAIN_IDENTITY, STANDALONE_MAIN_STABLE_ID, checked_async_result,
        collect_safepoints, emitted_register_count, migration_field_owner,
        migration_state_type_exists, optimize_emitted_bytecode, typed_exact_root_maps,
        validate_static_range_bound,
    };
    use nexa_analysis::{
        DefinitionId, HostAsyncResultIr, IrAbandonPolicy, IrCancelPolicy, IrEffect, IrLiteral,
        IrType, NormalizedPackagePath, PackageId, SourceKey, SourceRange, StateFieldIr,
        StateTypeIr, TypedExpressionIr, TypedExpressionKind,
    };
    use nexa_bytecode::{
        AbandonPolicy, AsyncResultType, CancelPolicy, CollectionIteratorKind, Function,
        FunctionEffect, HostCallMode, HostImport, Instruction, IteratorStateRegisters, LoopBound,
        ModuleBuilder, Signature, StandardIntrinsic, StateSchema, ValueType, result_type,
    };
    use nexa_core::{SourceSpan, StableId, StableSymbolId};

    #[test]
    fn compiler_rejects_forged_async_result_type_identity() {
        let result = HostAsyncResultIr {
            result_type: StableId::from_name("forged-result"),
            success: IrType::I32,
            error: IrType::String,
            cancel_policy: IrCancelPolicy::CancelTask,
            abandon_policy: IrAbandonPolicy::Trap,
            cancel_error: None,
            abandon_error: None,
        };
        assert!(
            checked_async_result(
                &result,
                ValueType::I32,
                ValueType::String,
                SourceSpan::default(),
            )
            .is_err()
        );
    }
    use nexa_verifier::{VerifierLimits, verify};
    use std::collections::BTreeMap;

    #[test]
    fn emitted_peephole_rewrites_local_moves_and_remaps_control_metadata() {
        let mut code = vec![
            Instruction::Move { dst: 2, source: 0 },
            Instruction::LoadI32 { dst: 3, value: 1 },
            Instruction::Add {
                dst: 1,
                lhs: 2,
                rhs: 3,
            },
            Instruction::Move { dst: 4, source: 1 },
            Instruction::LoadBool {
                dst: 9,
                value: true,
            },
            Instruction::Jump { target: 7 },
            Instruction::Trap,
            Instruction::Return { source: 4 },
        ];
        let mut spans = vec![SourceSpan::default(); code.len()];
        let mut loop_bounds = vec![LoopBound {
            back_edge: 5,
            max_iterations: 3,
        }];

        optimize_emitted_bytecode(&mut code, &mut spans, &mut loop_bounds);

        assert_eq!(
            code,
            vec![
                Instruction::LoadI32 { dst: 3, value: 1 },
                Instruction::Add {
                    dst: 1,
                    lhs: 0,
                    rhs: 3,
                },
                Instruction::Return { source: 1 },
            ]
        );
        assert_eq!(emitted_register_count(&code, 1), 4);
        assert_eq!(spans.len(), code.len());
        assert!(loop_bounds.is_empty());

        let mut loop_code = vec![
            Instruction::LoadBool {
                dst: 1,
                value: true,
            },
            Instruction::JumpIfFalse {
                condition: 1,
                target: 4,
            },
            Instruction::LoadI32 { dst: 2, value: 9 },
            Instruction::Jump { target: 0 },
            Instruction::Return { source: 0 },
        ];
        let mut loop_spans = vec![SourceSpan::default(); loop_code.len()];
        let mut retained_bound = vec![LoopBound {
            back_edge: 3,
            max_iterations: 3,
        }];
        optimize_emitted_bytecode(&mut loop_code, &mut loop_spans, &mut retained_bound);
        assert_eq!(retained_bound[0].back_edge, 2);
    }

    #[test]
    fn standalone_main_constant_matches_its_normative_identity() {
        assert_eq!(
            STANDALONE_MAIN_STABLE_ID,
            StableId::from_name(STANDALONE_MAIN_IDENTITY)
        );
    }

    fn typed_i32(kind: TypedExpressionKind) -> TypedExpressionIr {
        TypedExpressionIr {
            ty: IrType::I32,
            effect: IrEffect::Immediate,
            span: SourceRange {
                source: SourceKey::new(
                    PackageId::new("compiler.test").unwrap(),
                    NormalizedPackagePath::new("src/main.nexa").unwrap(),
                ),
                start: 0,
                end: 1,
            },
            kind,
        }
    }

    #[test]
    fn static_range_bound_is_recomputed_from_typed_constants() {
        let start_definition = DefinitionId(7);
        let start_value = typed_i32(TypedExpressionKind::Literal(IrLiteral::I32(-2)));
        let start = typed_i32(TypedExpressionKind::Reference(start_definition));
        let end = typed_i32(TypedExpressionKind::Literal(IrLiteral::I32(1)));
        let constants = BTreeMap::from([(start_definition, &start_value)]);

        assert_eq!(
            validate_static_range_bound(&start, &end, 3, &constants, SourceSpan::default())
                .unwrap(),
            3
        );
        assert!(
            validate_static_range_bound(&start, &end, 2, &constants, SourceSpan::default())
                .is_err()
        );
        assert!(
            validate_static_range_bound(
                &typed_i32(TypedExpressionKind::Reference(DefinitionId(99))),
                &end,
                3,
                &constants,
                SourceSpan::default()
            )
            .is_err()
        );
        assert_eq!(
            validate_static_range_bound(
                &typed_i32(TypedExpressionKind::Literal(IrLiteral::I32(i32::MIN))),
                &typed_i32(TypedExpressionKind::Literal(IrLiteral::I32(i32::MAX))),
                u32::MAX,
                &BTreeMap::new(),
                SourceSpan::default()
            )
            .unwrap(),
            u32::MAX
        );
    }

    #[test]
    fn migration_metadata_requires_exact_state_field_owner_and_value_type() {
        let owner = DefinitionId(0);
        let other = DefinitionId(1);
        let field = DefinitionId(2);
        let states = [StateTypeIr {
            definition: owner,
            version: 1,
            stable_id: StableSymbolId(StableId::from_name("Owner")),
            fields: vec![StateFieldIr {
                definition: field,
                ty: IrType::I32,
                stable_id: StableSymbolId(StableId::from_parts(&["Owner", "::value"])),
            }],
        }];

        assert_eq!(
            migration_field_owner(&states, &IrType::Named(owner), field, &IrType::I32),
            Some(owner)
        );
        assert_eq!(
            migration_field_owner(&states, &IrType::Named(other), field, &IrType::I32),
            None
        );
        assert_eq!(
            migration_field_owner(&states, &IrType::Named(owner), field, &IrType::Bool),
            None
        );
        assert!(migration_state_type_exists(&states, owner));
        assert!(!migration_state_type_exists(&states, other));
    }

    fn rooted_function(
        register_types: &[Option<ValueType>],
        parameter_count: usize,
        signature: Signature,
        effect: FunctionEffect,
        code: Vec<Instruction>,
    ) -> Function {
        rooted_function_with_layout(
            register_types,
            None,
            parameter_count,
            signature,
            effect,
            code,
        )
    }

    fn rooted_function_with_layout(
        register_types: &[Option<ValueType>],
        layout_table: Option<&nexa_bytecode::layout::LayoutTable>,
        parameter_count: usize,
        signature: Signature,
        effect: FunctionEffect,
        code: Vec<Instruction>,
    ) -> Function {
        let safepoints = collect_safepoints(&code);
        let (root_bitmap, root_maps) = typed_exact_root_maps(
            register_types,
            layout_table,
            parameter_count,
            &code,
            &safepoints,
            SourceSpan::default(),
        )
        .unwrap();
        let registers = u16::try_from(register_types.len()).unwrap();
        Function {
            signature,
            parameter_slots: u16::try_from(parameter_count).unwrap(),
            registers,
            frame_bytes: u32::from(registers).saturating_mul(8),
            root_bitmap,
            root_maps,
            safepoints,
            loop_bounds: Vec::new(),
            effect,
            max_static_call_depth: 1,
            code,
        }
    }

    #[test]
    fn exact_roots_drop_branch_only_reference_at_join() {
        let function = rooted_function(
            &[Some(ValueType::Bool), Some(ValueType::String)],
            1,
            Signature {
                parameters: vec![ValueType::Bool],
                result: None,
            },
            FunctionEffect::Ordinary,
            vec![
                Instruction::JumpIfFalse {
                    condition: 0,
                    target: 3,
                },
                Instruction::LoadString { dst: 1, string: 0 },
                Instruction::Jump { target: 3 },
                Instruction::ReturnVoid,
            ],
        );
        assert_eq!(
            function
                .root_maps
                .iter()
                .find(|map| map.pc == 3)
                .unwrap()
                .bitmap,
            vec![false, false]
        );
        assert_eq!(function.root_bitmap, vec![false, true]);
        let mut module = ModuleBuilder::new();
        module.string("");
        module.function(function);
        verify(module.finish(), VerifierLimits::default()).unwrap();
    }

    #[test]
    fn exact_roots_add_intrinsic_reference_only_after_the_instruction() {
        let function = rooted_function(
            &[Some(ValueType::String), Some(ValueType::String)],
            1,
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::String),
            },
            FunctionEffect::Ordinary,
            vec![
                Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringTrim,
                    args_base: 0,
                    args_count: 1,
                    dst: 1,
                },
                Instruction::Return { source: 1 },
            ],
        );
        assert_eq!(function.root_maps[0].bitmap, vec![true, false]);
        assert_eq!(function.root_maps[1].bitmap, vec![false, true]);
        let mut module = ModuleBuilder::new();
        module.function(function);
        verify(module.finish(), VerifierLimits::default()).unwrap();
    }

    #[test]
    fn exact_roots_add_async_host_result_at_resume_pc() {
        let async_enum = result_type(ValueType::String, ValueType::I32);
        let async_type = ValueType::Named(async_enum.type_id);
        let mut layout_module = ModuleBuilder::new();
        layout_module.enum_type(async_enum.clone());
        let layout_module = layout_module.finish();
        let layouts = nexa_bytecode::layout::LayoutTable::for_module(&layout_module).unwrap();
        let function = rooted_function_with_layout(
            &[Some(async_type), None],
            Some(&layouts),
            0,
            Signature {
                parameters: Vec::new(),
                result: Some(async_type),
            },
            FunctionEffect::Task,
            vec![
                Instruction::HostCall {
                    import: 0,
                    args_base: 0,
                    args_count: 0,
                    dst: 0,
                },
                Instruction::Return { source: 0 },
            ],
        );
        assert_eq!(function.root_maps[0].bitmap, vec![false, false]);
        assert_eq!(function.root_maps[1].bitmap, vec![false, true]);

        let mut module = ModuleBuilder::new();
        module
            .metadata(
                StableId::from_name("test.host"),
                StateSchema { types: Vec::new() }.fingerprint(),
            )
            .enum_type(async_enum)
            .host_import(HostImport {
                stable_id: StableId::from_name("test.host.request"),
                declaration_fingerprint: [7; 32],
                capabilities: Vec::new(),
                parameters: Vec::new(),
                result: Some(async_type),
                mode: HostCallMode::Async,
                fuel_cost: 1,
                async_result: Some(AsyncResultType {
                    result_type: match async_type {
                        ValueType::Named(type_id) => type_id,
                        _ => unreachable!(),
                    },
                    success: ValueType::String,
                    error: ValueType::I32,
                    cancel_policy: CancelPolicy::CancelTask,
                    abandon_policy: AbandonPolicy::Trap,
                    cancel_error: None,
                    abandon_error: None,
                }),
            });
        module.function(function);
        verify(module.finish(), VerifierLimits::default()).unwrap();
    }

    #[test]
    fn exact_roots_are_empty_at_unreachable_safepoints() {
        let function = rooted_function(
            &[Some(ValueType::String)],
            0,
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            FunctionEffect::Ordinary,
            vec![
                Instruction::ReturnVoid,
                Instruction::LoadString { dst: 0, string: 0 },
                Instruction::ReturnVoid,
            ],
        );
        assert_eq!(function.root_bitmap, vec![false]);
        assert!(
            function
                .root_maps
                .iter()
                .all(|map| map.bitmap == vec![false])
        );
        let mut module = ModuleBuilder::new();
        module.string("");
        module.function(function);
        verify(module.finish(), VerifierLimits::default()).unwrap();
    }

    fn test_package() -> nexa_analysis::TypedPackageIr {
        use nexa_analysis::{LifecycleBindingsIr, PackageSemanticMetadata, StateSchemaFingerprint};
        use nexa_core::PublicApiFingerprint;
        nexa_analysis::TypedPackageIr::new_product(
            PackageId::new("tests.compiler").expect("test package id"),
            1,
            Vec::new(),
            Vec::new(),
            PackageSemanticMetadata {
                entry_module: None,
                state_types: std::sync::Arc::new([]),
                host_bindings: std::sync::Arc::new([]),
                exports: std::sync::Arc::new([]),
                tests: std::sync::Arc::new([]),
                external_sources: std::sync::Arc::new([]),
                lifecycle: LifecycleBindingsIr::default(),
                repl_entry: None,
                standard_functions: std::sync::Arc::new([]),
                public_api_fingerprint: PublicApiFingerprint::default(),
                state_schema_fingerprint: StateSchemaFingerprint::default(),
            },
        )
        .expect("empty test package constructs")
    }

    #[test]
    fn set_metadata_and_stdlib_lowering_are_concrete() {
        use super::{
            GenericTypeMetadata, TypedStandardLowering, collect_ir_type_metadata, lower_type,
            standard_call_lowering, validate_concrete_standard_lowering,
        };
        use nexa_bytecode::SetType;
        use nexa_stdlib::Intrinsic as I;

        let package = test_package();
        let span = SourceSpan::default();
        let range = SourceRange {
            source: SourceKey::new(
                PackageId::new("tests.compiler").expect("test package id"),
                NormalizedPackagePath::new("src/main.nexa").expect("test module path"),
            ),
            start: 0,
            end: 0,
        };
        let set_of_i32 = IrType::Set(Box::new(IrType::I32));
        let set_type = |element| ValueType::Named(nexa_bytecode::set_type(element));

        assert_eq!(
            lower_type(&package, &set_of_i32, span).expect("Set lowers"),
            set_type(ValueType::I32)
        );

        let mut metadata = GenericTypeMetadata::default();
        collect_ir_type_metadata(&package, &set_of_i32, span, &mut metadata)
            .expect("Set metadata collects");
        assert_eq!(
            metadata.sets.get(&nexa_bytecode::set_type(ValueType::I32)),
            Some(&SetType::new(ValueType::I32))
        );
        assert!(metadata.maps.is_empty());
        assert!(metadata.arrays.is_empty());

        let no_args: [TypedExpressionIr; 0] = [];
        assert!(matches!(
            standard_call_lowering(
                &package,
                I::SetNew,
                &[IrType::I32],
                &no_args,
                &set_of_i32,
                span
            )
            .expect("SetNew lowers"),
            TypedStandardLowering::SetNew {
                element: ValueType::I32
            }
        ));
        assert!(matches!(
            standard_call_lowering(
                &package,
                I::SetLen,
                &[IrType::I32],
                &no_args,
                &IrType::I32,
                span,
            )
            .expect("SetLen lowers"),
            TypedStandardLowering::Intrinsic(StandardIntrinsic::SetLen {
                element: ValueType::I32
            })
        ));
        let set_argument = TypedExpressionIr {
            ty: set_of_i32,
            effect: IrEffect::Immediate,
            span: range,
            kind: TypedExpressionKind::Reference(DefinitionId(0)),
        };
        assert!(
            validate_concrete_standard_lowering(
                &package,
                std::slice::from_ref(&set_argument),
                &IrType::Unit,
                TypedStandardLowering::SetClear {
                    element: ValueType::I32,
                },
                span,
            )
            .is_ok()
        );
        assert!(
            validate_concrete_standard_lowering(
                &package,
                &[set_argument],
                &IrType::Bool,
                TypedStandardLowering::SetClear {
                    element: ValueType::I32,
                },
                span,
            )
            .is_err()
        );
        assert_eq!(
            StandardIntrinsic::SetInsert {
                element: ValueType::I32,
            }
            .result_type(),
            ValueType::Bool,
            "SetInsert carries the duplicate/absent boolean in its own dst"
        );
    }

    #[test]
    fn iteration_instruction_shapes_match_the_v8_wire() {
        use super::{typed_instruction_destinations, typed_instruction_sources};

        let state = IteratorStateRegisters {
            collection: 1,
            phase: 2,
            slot: 3,
            epoch: 4,
        };
        let iter_next = Instruction::IterNext {
            kind: CollectionIteratorKind::Set {
                element: ValueType::String,
            },
            state,
            has_value_dst: 5,
            first_dst: 6,
            second_dst: None,
        };
        assert_eq!(typed_instruction_destinations(iter_next), vec![5, 6]);
        assert_eq!(typed_instruction_sources(iter_next), vec![1, 2, 3, 4]);
        let map_next = Instruction::IterNext {
            kind: CollectionIteratorKind::Map {
                key: ValueType::I32,
                value: ValueType::String,
            },
            state,
            has_value_dst: 5,
            first_dst: 6,
            second_dst: Some(7),
        };
        assert_eq!(typed_instruction_destinations(map_next), vec![5, 6, 7]);
        assert_eq!(
            typed_instruction_sources(Instruction::IterNew {
                kind: CollectionIteratorKind::Range,
                state,
            }),
            vec![1, 2],
            "Range carries start in collection and end in phase"
        );
        assert_eq!(
            typed_instruction_destinations(Instruction::IterNew {
                kind: CollectionIteratorKind::Range,
                state,
            }),
            vec![3, 4],
            "Range initializes slot and epoch; the caller-set end in phase is only read"
        );
        assert_eq!(
            typed_instruction_sources(Instruction::IterNew {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
            }),
            vec![1],
            "collection kinds read only the collection reference"
        );
        assert_eq!(
            typed_instruction_destinations(Instruction::IterNew {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
            }),
            vec![2, 3, 4],
            "collection kinds initialize phase, slot, and epoch"
        );
        assert_eq!(
            super::collect_safepoints(&[
                Instruction::IterNew {
                    kind: CollectionIteratorKind::Set {
                        element: ValueType::String,
                    },
                    state,
                },
                iter_next,
            ]),
            vec![0, 1],
            "both iterator operations are exact runtime safepoints"
        );
    }

    #[test]
    fn iter_new_initializes_the_full_cursor_state_before_the_loop() {
        use nexa_bytecode::set_type;

        let set_of_string = set_type(ValueType::String);
        let state = IteratorStateRegisters {
            collection: 0,
            phase: 1,
            slot: 2,
            epoch: 3,
        };
        let code = vec![
            Instruction::SetNew {
                type_id: set_of_string,
                dst: 0,
            },
            Instruction::IterNew {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
            },
            Instruction::Safepoint,
            Instruction::IterNext {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
                has_value_dst: 4,
                first_dst: 5,
                second_dst: None,
            },
            Instruction::JumpIfFalse {
                condition: 4,
                target: 8,
            },
            Instruction::Move { dst: 6, source: 5 },
            Instruction::Return { source: 6 },
            Instruction::Jump { target: 3 },
            Instruction::ReturnVoid,
        ];
        let register_types = vec![
            Some(ValueType::Named(set_of_string)),
            Some(ValueType::I32),
            Some(ValueType::I32),
            Some(ValueType::I64),
            Some(ValueType::Bool),
            Some(ValueType::String),
            Some(ValueType::String),
        ];
        let (_, maps) =
            typed_exact_root_maps(&register_types, None, 0, &code, &[2], SourceSpan::default())
                .expect("cursor root maps compute");
        assert_eq!(
            maps[0].bitmap,
            vec![true, false, false, false, false, false, false],
            "after IterNew only the collection ref is rooted; the scalar cursor is initialized"
        );
    }

    #[test]
    fn iter_next_root_maps_root_the_collection_and_live_bindings() {
        use nexa_bytecode::set_type;

        let set_of_string = set_type(ValueType::String);
        let state = IteratorStateRegisters {
            collection: 0,
            phase: 1,
            slot: 2,
            epoch: 3,
        };
        let code = vec![
            Instruction::SetNew {
                type_id: set_of_string,
                dst: 0,
            },
            Instruction::IterNew {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
            },
            Instruction::IterNext {
                kind: CollectionIteratorKind::Set {
                    element: ValueType::String,
                },
                state,
                has_value_dst: 4,
                first_dst: 5,
                second_dst: None,
            },
            Instruction::JumpIfFalse {
                condition: 4,
                target: 8,
            },
            Instruction::Move { dst: 6, source: 5 },
            Instruction::Safepoint,
            Instruction::Return { source: 6 },
            Instruction::Jump { target: 2 },
            Instruction::ReturnVoid,
        ];
        let register_types = vec![
            Some(ValueType::Named(set_of_string)),
            Some(ValueType::I32),
            Some(ValueType::I32),
            Some(ValueType::I64),
            Some(ValueType::Bool),
            Some(ValueType::String),
            Some(ValueType::String),
        ];
        let safepoints = vec![5];
        let (bitmap, maps) = typed_exact_root_maps(
            &register_types,
            None,
            0,
            &code,
            &safepoints,
            SourceSpan::default(),
        )
        .expect("iteration root maps compute");
        assert!(bitmap[0], "collection ref is a root candidate");
        assert_eq!(maps.len(), 1);
        assert_eq!(
            maps[0].bitmap,
            vec![false, false, false, false, false, false, true],
            "safepoint roots the moved binding but not the dead payload"
        );
    }

    #[test]
    fn map_iter_next_roots_a_live_second_payload() {
        use nexa_bytecode::map_type;

        let map = map_type(ValueType::I32, ValueType::String);
        let state = IteratorStateRegisters {
            collection: 0,
            phase: 1,
            slot: 2,
            epoch: 3,
        };
        let code = vec![
            Instruction::MapNew {
                type_id: map,
                dst: 0,
            },
            Instruction::IterNew {
                kind: CollectionIteratorKind::Map {
                    key: ValueType::I32,
                    value: ValueType::String,
                },
                state,
            },
            Instruction::IterNext {
                kind: CollectionIteratorKind::Map {
                    key: ValueType::I32,
                    value: ValueType::String,
                },
                state,
                has_value_dst: 4,
                first_dst: 5,
                second_dst: Some(6),
            },
            Instruction::JumpIfFalse {
                condition: 4,
                target: 9,
            },
            Instruction::Safepoint,
            Instruction::Move { dst: 7, source: 5 },
            Instruction::Move { dst: 8, source: 6 },
            Instruction::Return { source: 8 },
            Instruction::Jump { target: 2 },
            Instruction::ReturnVoid,
        ];
        let register_types = vec![
            Some(ValueType::Named(map)),
            Some(ValueType::I32),
            Some(ValueType::I32),
            Some(ValueType::I64),
            Some(ValueType::Bool),
            Some(ValueType::I32),
            Some(ValueType::String),
            Some(ValueType::I32),
            Some(ValueType::String),
        ];
        let (_, maps) =
            typed_exact_root_maps(&register_types, None, 0, &code, &[4], SourceSpan::default())
                .expect("map iteration root maps compute");
        assert_eq!(
            maps[0].bitmap,
            vec![false, false, false, false, false, false, true, false, false],
            "the still-live Map value payload is rooted at the safepoint before its move"
        );
    }
}
