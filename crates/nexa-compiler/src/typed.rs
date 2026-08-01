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
    BinaryOperator, BuiltinOperationIr, BuiltinVariantIr, DeclarationVisibility, Definition,
    DefinitionId, DefinitionKind, HostAsyncResultIr, HostTypeLayoutIr, IrAbandonPolicy,
    IrCancelPolicy, IrCompilationKind, IrEffect, IrHostFunctionMode, IrLiteral, IrType,
    MigrationIntrinsicIr, ModulePath, SourceKey, SourceRange, StateTypeIr, TypedBlockIr,
    TypedDeclarationBody, TypedExpressionIr, TypedExpressionKind, TypedFunctionIr, TypedPackageIr,
    TypedPatternIr, TypedPatternKind, TypedPlaceIr, TypedStatementIr, TypedTypeLayoutIr,
    UnaryOperator,
};
use nexa_bytecode::{
    AbandonPolicy, ArrayType, AsyncResultType, BufferType, CancelPolicy, ClassType, EnumType,
    EnumVariant, Function, FunctionEffect, HostCallMode, HostImport, Instruction, LoopBound,
    MapType, ModuleBuilder, ResourceTokenType, RootMap, ScriptExport, Signature, SnapshotType,
    SourceMapEntry, StandardIntrinsic, StateField, StateHandleType, StateSchema, StateType,
    StructField as BytecodeStructField, StructType, ValueType, array_type, buffer_type, map_type,
    option_type, parameterized_type_id, resource_token_type, result_type, snapshot_type,
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
    register_types: Vec<Option<ValueType>>,
    parameter_count: usize,
    function_effect: IrEffect,
    function_return_type: &'a IrType,
    code: Vec<Instruction>,
    spans: Vec<SourceSpan>,
    loop_bounds: Vec<LoopBound>,
    loop_stack: Vec<LoopPatch>,
    function_span: SourceSpan,
    constant_stack: BTreeSet<DefinitionId>,
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
    compile_typed_package_with_profile(package, false)
}

#[allow(clippy::too_many_lines)]
fn compile_typed_package_with_profile(
    package: &TypedPackageIr,
    emit_standalone_main: bool,
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
                    // M5 WP37/WP38: optimization passes run on an owned copy
                    // immediately before lowering; analyzer snapshots and
                    // their fingerprints stay untouched.
                    let mut optimized = function.clone();
                    let reports = nexa_analysis::passes::PassManager::standard()
                        .optimize_function(&mut optimized);
                    let function = if reports.iter().all(|report| report.rewrites == 0) {
                        std::borrow::Cow::Borrowed(function)
                    } else {
                        std::borrow::Cow::Owned(optimized)
                    };
                    function_plans.push(TypedFunctionPlan {
                        definition,
                        function,
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
    builder.state_schema(state_schema);
    let layouts = emit_typed_type_metadata(package, &modules, &files, &mut builder)?;
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
    let mut compiled = compile_typed_package_with_profile(package, true)?;
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
            | TypedStatementIr::StaticRangeFor { body, .. } => {
                validate_repl_defer_block(package, functions, body, referenced_helpers, span)?;
            }
            TypedStatementIr::Let { .. }
            | TypedStatementIr::Assign { .. }
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
            | TypedStatementIr::StaticRangeFor { body, .. } => {
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
        TypedStatementIr::Assign { target, value } => {
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
        } => {
            reads(start)
                || reads(end)
                || repl_block_reads_pending_field(body, expected, initialized)
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
        TypedStatementIr::Assign { target, .. } => {
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
        TypedStatementIr::While { body, .. } | TypedStatementIr::StaticRangeFor { body, .. } => {
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
        0,
        &code,
        &safepoints,
        source_debug.definition_span,
    )?;
    compiled.module.functions.push(Function {
        signature: signature.clone(),
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
    if export.signature != expected_signature
        || export.effect != FunctionEffect::Task
        || (source_effect == FunctionEffect::Task && export.function != debug.function_index)
        || (source_effect == FunctionEffect::Ordinary
            && !is_standalone_sync_wrapper(&compiled.module, export.function, debug.function_index))
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

fn is_standalone_sync_wrapper(module: &nexa_bytecode::Module, wrapper: u32, main: u32) -> bool {
    module
        .functions
        .get(usize::try_from(wrapper).unwrap_or(usize::MAX))
        .is_some_and(|function| {
            function.signature == standalone_main_signature()
                && function.effect == FunctionEffect::Task
                && function.code
                    == [
                        Instruction::Call {
                            function: main,
                            args_base: 0,
                            args_count: 1,
                            dst: 1,
                        },
                        Instruction::Return { source: 1 },
                    ]
        })
}

fn invalid_standalone_main_signature(
    package: &TypedPackageIr,
    function: &TypedFunctionIr,
) -> Option<&'static str> {
    if function.parameters.len() != 1 {
        return Some("standalone main must accept exactly one Array<string> argument");
    }
    let Some(parameter) = package.definition(function.parameters[0]) else {
        return Some("standalone main parameter is missing from typed IR");
    };
    if parameter.ty != IrType::Array(Box::new(IrType::String)) {
        return Some("standalone main argument must have type Array<string>");
    }
    if function.return_type != IrType::I32 {
        return Some("standalone main must return i32");
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
    let function_index = match main.function.effect {
        IrEffect::Task => source_function,
        IrEffect::Ordinary => {
            let wrapper_index = u32::try_from(function_plans.len())
                .map_err(|_| CompileError::too_many_registers(definition_span))?;
            let code = vec![
                Instruction::Call {
                    function: source_function,
                    args_base: 0,
                    args_count: 1,
                    dst: 1,
                },
                Instruction::Return { source: 1 },
            ];
            let register_types = vec![
                Some(ValueType::Named(array_type(ValueType::String))),
                Some(ValueType::I32),
            ];
            let safepoints = collect_safepoints(&code);
            let (root_bitmap, root_maps) =
                typed_exact_root_maps(&register_types, 1, &code, &safepoints, definition_span)?;
            source_map.extend(code.iter().enumerate().map(|(pc, _)| SourceMapEntry {
                function: wrapper_index,
                pc_start: u32::try_from(pc).unwrap_or(u32::MAX),
                pc_end: u32::try_from(pc.saturating_add(1)).unwrap_or(u32::MAX),
                span: definition_span,
            }));
            builder.function(Function {
                signature: standalone_main_signature(),
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
    ) -> Result<Self, CompileError> {
        let mut locals = BTreeMap::new();
        let mut register_types = Vec::new();
        for definition in function.parameters.iter().chain(&function.locals) {
            let metadata = package
                .definition(*definition)
                .expect("TypedPackageIr validates local IDs");
            let ty = lower_type(package, &metadata.ty, function_span)?;
            let register = u16::try_from(register_types.len())
                .map_err(|_| CompileError::too_many_registers(function_span))?;
            locals.insert(*definition, register);
            register_types.push(Some(ty));
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
            register_types,
            parameter_count: function.parameters.len(),
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
                    let destination = self.local(*definition)?;
                    self.emit_expression(value, destination)?;
                }
            }
            TypedStatementIr::Assign { target, value } => match target {
                TypedPlaceIr::Definition(definition) => {
                    let destination = self.local(*definition)?;
                    let source = self.allocate_expression(value)?;
                    self.emit_expression(value, source)?;
                    self.push(
                        Instruction::Move {
                            dst: destination,
                            source,
                        },
                        self.span(&value.span)?,
                    );
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
                if captures.len() > 8 {
                    return Err(CompileError::defer_capture_limit(self.function_span));
                }
                let function = *self.function_indices.get(cleanup).ok_or_else(|| {
                    CompileError::unknown_name(self.definition_name(*cleanup), self.function_span)
                })?;
                let args_base = self.reserve_arguments(captures)?;
                for (offset, capture) in captures.iter().enumerate() {
                    let register =
                        args_base
                            .checked_add(u16::try_from(offset).map_err(|_| {
                                CompileError::too_many_registers(self.function_span)
                            })?)
                            .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
                    self.emit_expression(capture, register)?;
                }
                self.push(
                    Instruction::DeferPush {
                        function,
                        args_base,
                        args_count: u16::try_from(captures.len())
                            .map_err(|_| CompileError::too_many_registers(self.function_span))?,
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
        self.store_struct_place_root(root, updated);
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

    fn store_struct_place_root(&mut self, root: TypedStructPlaceRoot, source: u16) {
        let instruction = match root {
            TypedStructPlaceRoot::Definition { register, .. } => Instruction::Move {
                dst: register,
                source,
            },
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
                    self.push(
                        Instruction::Move {
                            dst: destination,
                            source,
                        },
                        span,
                    );
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
                let args_base = self.reserve_arguments(arguments)?;
                for (offset, argument) in arguments.iter().enumerate() {
                    let register =
                        args_base
                            .checked_add(u16::try_from(offset).map_err(|_| {
                                CompileError::too_many_registers(self.function_span)
                            })?)
                            .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
                    self.emit_expression(argument, register)?;
                }
                self.push(
                    Instruction::Call {
                        function,
                        args_base,
                        args_count: u16::try_from(arguments.len())
                            .map_err(|_| CompileError::too_many_registers(span))?,
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
                let args_base = self.reserve_arguments(arguments)?;
                for (offset, argument) in arguments.iter().enumerate() {
                    let register =
                        args_base
                            .checked_add(u16::try_from(offset).map_err(|_| {
                                CompileError::too_many_registers(self.function_span)
                            })?)
                            .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
                    self.emit_expression(argument, register)?;
                }
                self.push(
                    Instruction::HostCall {
                        import,
                        args_base,
                        args_count: u16::try_from(arguments.len())
                            .map_err(|_| CompileError::too_many_registers(span))?,
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
        let args_base = self.reserve_arguments(arguments)?;
        for (offset, argument) in arguments.iter().enumerate() {
            let register = args_base
                .checked_add(
                    u16::try_from(offset)
                        .map_err(|_| CompileError::too_many_registers(self.function_span))?,
                )
                .ok_or_else(|| CompileError::too_many_registers(self.function_span))?;
            self.emit_expression(argument, register)?;
        }
        match lowering {
            TypedStandardLowering::Intrinsic(intrinsic) => self.push(
                Instruction::StandardIntrinsic {
                    intrinsic,
                    args_base,
                    args_count: u16::try_from(arguments.len())
                        .map_err(|_| CompileError::too_many_registers(span))?,
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
        };
        Ok(())
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

        let args_base = self.reserve_arguments(arguments)?;
        for (offset, argument) in arguments.iter().enumerate() {
            let register = builtin_argument_register(args_base, offset, span)?;
            self.emit_expression(argument, register)?;
        }
        let argument = |index| builtin_argument_register(args_base, index, span);
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
            BuiltinOperationIr::MapSet => {
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
        let fields_base = self.reserve_types(&field_types)?;
        for (offset, field) in layout.fields.iter().enumerate() {
            let value = values
                .get(&field.definition)
                .copied()
                .ok_or_else(|| CompileError::type_mismatch(None, None, span))?;
            let register = fields_base
                .checked_add(
                    u16::try_from(offset).map_err(|_| CompileError::too_many_registers(span))?,
                )
                .ok_or_else(|| CompileError::too_many_registers(span))?;
            self.emit_expression(value, register)?;
        }
        let fields_count = u16::try_from(layout.fields.len())
            .map_err(|_| CompileError::too_many_registers(span))?;
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
        let fields_base = self.reserve_arguments(values)?;
        for (offset, value) in values.iter().enumerate() {
            let register = fields_base
                .checked_add(
                    u16::try_from(offset).map_err(|_| CompileError::too_many_registers(span))?,
                )
                .ok_or_else(|| CompileError::too_many_registers(span))?;
            self.emit_expression(value, register)?;
        }
        self.push(
            Instruction::StructNew {
                type_id,
                fields_base,
                fields_count: u16::try_from(values.len())
                    .map_err(|_| CompileError::too_many_registers(span))?,
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
                // A class update is an explicit `new Class { ..base }`: it creates a fresh object
                // rather than mutating or aliasing `base`.
                let source = self.allocate_expression(base)?;
                self.emit_expression(base, source)?;
                let field_types = layout
                    .fields
                    .iter()
                    .map(|field| field.ty)
                    .collect::<Vec<_>>();
                let fields_base = self.reserve_types(&field_types)?;
                for (offset, field) in layout.fields.iter().enumerate() {
                    let target = fields_base
                        .checked_add(
                            u16::try_from(offset)
                                .map_err(|_| CompileError::too_many_registers(span))?,
                        )
                        .ok_or_else(|| CompileError::too_many_registers(span))?;
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
                    let target = fields_base
                        .checked_add(
                            u16::try_from(offset)
                                .map_err(|_| CompileError::too_many_registers(span))?,
                        )
                        .ok_or_else(|| CompileError::too_many_registers(span))?;
                    self.emit_expression(value, target)?;
                }
                self.push(
                    Instruction::ClassNew {
                        type_id: layout.type_id,
                        fields_base,
                        fields_count: u16::try_from(layout.fields.len())
                            .map_err(|_| CompileError::too_many_registers(span))?,
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
                self.push(
                    Instruction::Move {
                        dst: destination,
                        source,
                    },
                    span,
                );
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

    fn reserve_arguments(&mut self, arguments: &[TypedExpressionIr]) -> Result<u16, CompileError> {
        let base = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        for argument in arguments {
            let ty = lower_type(self.package, &argument.ty, self.span(&argument.span)?)?;
            self.allocate(ty)?;
        }
        Ok(base)
    }

    fn reserve_types(&mut self, types: &[ValueType]) -> Result<u16, CompileError> {
        let base = u16::try_from(self.register_types.len())
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        for ty in types {
            self.allocate(*ty)?;
        }
        Ok(base)
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
        Ok(register)
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
        let registers = u16::try_from(self.register_types.len().max(1))
            .map_err(|_| CompileError::too_many_registers(self.function_span))?;
        while self.register_types.len() < usize::from(registers) {
            self.register_types.push(None);
        }
        let safepoints = collect_safepoints(&self.code);
        let (root_bitmap, root_maps) = typed_exact_root_maps(
            &self.register_types,
            self.parameter_count,
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

fn builtin_argument_register(
    base: u16,
    index: usize,
    span: SourceSpan,
) -> Result<u16, CompileError> {
    base.checked_add(u16::try_from(index).map_err(|_| CompileError::too_many_registers(span))?)
        .ok_or_else(|| CompileError::too_many_registers(span))
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
        BuiltinOperationIr::StringEqual => {
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
        | BuiltinOperationIr::ArraySet
        | BuiltinOperationIr::ArrayPush
        | BuiltinOperationIr::ArrayPop
        | BuiltinOperationIr::ArrayInsert
        | BuiltinOperationIr::ArrayRemove
        | BuiltinOperationIr::ArrayClear => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let array = IrType::Array(Box::new(element.clone()));
            match operation {
                BuiltinOperationIr::ArrayLen => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
                }
                BuiltinOperationIr::ArrayGet | BuiltinOperationIr::ArrayRemove => {
                    validate_builtin_arguments(arguments, &[array, IrType::I32], span)?;
                    validate_builtin_result(result, element, span)
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
                BuiltinOperationIr::ArrayClear => {
                    validate_builtin_arguments(arguments, &[array], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("array operations are matched above"),
            }
        }
        BuiltinOperationIr::MapLen
        | BuiltinOperationIr::MapGet
        | BuiltinOperationIr::MapSet
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
                BuiltinOperationIr::MapGet | BuiltinOperationIr::MapRemove => {
                    validate_builtin_arguments(arguments, &[map, key.clone()], span)?;
                    validate_builtin_result(result, &IrType::Option(Box::new(value.clone())), span)
                }
                BuiltinOperationIr::MapSet => {
                    validate_builtin_arguments(
                        arguments,
                        &[map, key.clone(), value.clone()],
                        span,
                    )?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::MapContains => {
                    validate_builtin_arguments(arguments, &[map, key.clone()], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                BuiltinOperationIr::MapClear => {
                    validate_builtin_arguments(arguments, &[map], span)?;
                    validate_builtin_result(result, &IrType::Bool, span)
                }
                _ => unreachable!("map operations are matched above"),
            }
        }
        BuiltinOperationIr::BufferLen
        | BuiltinOperationIr::BufferGet
        | BuiltinOperationIr::BufferSet
        | BuiltinOperationIr::BufferSlice
        | BuiltinOperationIr::BufferCopy => {
            let [element] = type_arguments else {
                return Err(CompileError::type_mismatch(None, None, span));
            };
            let buffer = IrType::Buffer(Box::new(element.clone()));
            match operation {
                BuiltinOperationIr::BufferLen => {
                    validate_builtin_arguments(arguments, &[buffer], span)?;
                    validate_builtin_result(result, &IrType::I32, span)
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
    }
}

#[allow(clippy::too_many_lines)]
fn typed_exact_root_maps(
    register_types: &[Option<ValueType>],
    parameter_count: usize,
    code: &[Instruction],
    safepoints: &[u32],
    span: SourceSpan,
) -> Result<(Vec<bool>, Vec<RootMap>), CompileError> {
    use std::collections::VecDeque;

    let register_count = register_types.len();
    let mut entry = vec![false; register_count];
    for register in 0..parameter_count {
        entry[register] = register_types[register].is_some_and(ValueType::is_reference);
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
        if let Some(destination) = typed_instruction_destination(code[pc]) {
            let destination = usize::from(destination);
            if destination >= register_count {
                return Err(CompileError::verify(
                    "typed emitter produced an out-of-range destination register".into(),
                    span,
                ));
            }
            if register_types[destination].is_some_and(ValueType::is_reference) {
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
            if let Some(destination) = typed_instruction_liveness_destination(code[pc]) {
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

    let root_bitmap = (0..register_count)
        .map(|register| {
            register_types[register].is_some_and(ValueType::is_reference)
                && states.iter().flatten().any(|state| state[register])
        })
        .collect::<Vec<_>>();
    let root_maps = safepoints
        .iter()
        .map(|pc| {
            let pc_index = usize::try_from(*pc).unwrap_or(usize::MAX);
            let bitmap = states.get(pc_index).and_then(Option::as_ref).map_or_else(
                || vec![false; register_count],
                |state| {
                    state
                        .iter()
                        .enumerate()
                        .map(|(register, initialized)| {
                            *initialized
                                && live_before[pc_index][register]
                                && register_types[register].is_some_and(ValueType::is_reference)
                        })
                        .collect()
                },
            );
            RootMap { pc: *pc, bitmap }
        })
        .collect();
    Ok((root_bitmap, root_maps))
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
        | Instruction::ArrayRemove { source, index, .. }
        | Instruction::BufferGet { source, index, .. } => vec![source, index],
        Instruction::StandardIntrinsic {
            args_base,
            args_count,
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
        | Instruction::ArrayPush { source, value } => vec![source, value],
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
        | Instruction::ArrayInsert { .. }
        | Instruction::ArrayClear { .. }
        | Instruction::MapSet { .. }
        | Instruction::MapClear { .. }
        | Instruction::BufferSet { .. }
        | Instruction::BufferCopy { .. }
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

fn typed_instruction_liveness_destination(instruction: Instruction) -> Option<u16> {
    // Typed emission allocates a fresh destination for every call expression.
    // Keeping it live before the call is therefore filtered by definite
    // initialization, while also matching void calls, whose encoded `dst` is
    // not written at all.
    match instruction {
        Instruction::Call { .. } | Instruction::HostCall { .. } => None,
        _ => typed_instruction_destination(instruction),
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
    builder: &mut ModuleBuilder,
) -> Result<TypedLayoutContext, CompileError> {
    let mut context = TypedLayoutContext::default();
    let mut enum_types = BTreeMap::<StableId, EnumType>::new();
    let mut struct_types = BTreeMap::<StableId, StructType>::new();
    let mut class_types = BTreeMap::<StableId, ClassType>::new();

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
                HostTypeLayoutIr::Opaque => {}
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
        | IrType::Named(_) => {}
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
            TypedStatementIr::Assign { target, value } => {
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
            } => {
                collect_expression_type_metadata(package, start, span, metadata)?;
                collect_expression_type_metadata(package, end, span, metadata)?;
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
        | IrType::Named(_) => {}
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
        | IrType::Named(_) => false,
    }
}

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
        I::ArrayLen => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayLen {
            element: type_argument(0)?,
        }),
        I::ArrayIsEmpty => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayIsEmpty {
            element: type_argument(0)?,
        }),
        I::ArrayGet => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayGet {
            element: type_argument(0)?,
        }),
        I::ArrayPush => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayPush {
            element: type_argument(0)?,
        }),
        I::ArrayPop => TypedStandardLowering::Intrinsic(StandardIntrinsic::ArrayPop {
            element: type_argument(0)?,
        }),
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
        I::DebugAssert => TypedStandardLowering::Intrinsic(StandardIntrinsic::DebugAssert),
        I::DebugTrap => TypedStandardLowering::Intrinsic(StandardIntrinsic::DebugTrap),
    };
    validate_concrete_standard_lowering(package, arguments, result, lowering, span)?;
    Ok(lowering)
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
        IrType::TypeParameter(_) => Err(CompileError::unknown_type(
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
        | IrType::HostRequest(_)
        | IrType::ResourceToken(_)
        | IrType::Snapshot(_)
        | IrType::Buffer(_)
        | IrType::StateHandle(_)
        | IrType::TypeParameter(_) => Err(CompileError::type_mismatch(None, Some(ty), span)),
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

fn collect_block_codegen_inputs(block: &TypedBlockIr, inputs: &mut CodegenInputs) {
    for statement in &block.statements {
        match statement {
            TypedStatementIr::Let { value, .. } | TypedStatementIr::Return(value) => {
                if let Some(value) = value {
                    collect_expression_codegen_inputs(value, inputs);
                }
            }
            TypedStatementIr::Assign { target, value } => {
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
            } => {
                collect_expression_codegen_inputs(start, inputs);
                collect_expression_codegen_inputs(end, inputs);
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
            TypedStatementIr::Assign { target, value } => {
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
                    | Instruction::EnumNew { .. }
                    | Instruction::EnumEqual { .. }
                    | Instruction::StructNew { .. }
                    | Instruction::StructWith { .. }
                    | Instruction::StructEqual { .. }
                    | Instruction::ClassNew { .. }
                    | Instruction::ArrayNew { .. }
                    | Instruction::ArrayLen { .. }
                    | Instruction::ArrayGet { .. }
                    | Instruction::ArraySet { .. }
                    | Instruction::ArrayPush { .. }
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
        collect_safepoints, migration_field_owner, migration_state_type_exists,
        typed_exact_root_maps, validate_static_range_bound,
    };
    use nexa_analysis::{
        DefinitionId, HostAsyncResultIr, IrAbandonPolicy, IrCancelPolicy, IrEffect, IrLiteral,
        IrType, NormalizedPackagePath, PackageId, SourceKey, SourceRange, StateFieldIr,
        StateTypeIr, TypedExpressionIr, TypedExpressionKind,
    };
    use nexa_bytecode::{
        AbandonPolicy, AsyncResultType, CancelPolicy, Function, FunctionEffect, HostCallMode,
        HostImport, Instruction, ModuleBuilder, Signature, StandardIntrinsic, StateSchema,
        ValueType, result_type,
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
        let safepoints = collect_safepoints(&code);
        let (root_bitmap, root_maps) = typed_exact_root_maps(
            register_types,
            parameter_count,
            &code,
            &safepoints,
            SourceSpan::default(),
        )
        .unwrap();
        let registers = u16::try_from(register_types.len()).unwrap();
        Function {
            signature,
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
        let function = rooted_function(
            &[Some(async_type)],
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
        assert_eq!(function.root_maps[0].bitmap, vec![false]);
        assert_eq!(function.root_maps[1].bitmap, vec![true]);

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
}
