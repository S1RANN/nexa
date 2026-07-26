//! Structural, type and continuation verification for Nexa bytecode.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use nexa_bytecode::{
    Function, FunctionEffect, HostCallMode, Instruction, Module, ValueType,
    minimum_migration_limits,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifierLimits {
    pub max_frame_bytes: u32,
    pub max_immediate_cost: u32,
    pub max_wcet_states: u32,
}

impl Default for VerifierLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_immediate_cost: 1_024,
            max_wcet_states: 100_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    pub function: usize,
    pub instruction: Option<usize>,
    pub kind: VerifyErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyErrorKind {
    EmptyFunction,
    RegisterOutOfRange(u16),
    FunctionOutOfRange(u32),
    HostImportOutOfRange(u32),
    ExportOutOfRange(u32),
    InvalidExportSignature,
    DuplicateExport,
    JumpOutOfRange(u32),
    TypeMismatch,
    ConflictingControlFlowTypes,
    InvalidReturn,
    FrameLimit,
    RootBitmapLength,
    ForgedRoot(u16),
    MissingRoot(u16),
    ImmediateCostLimit,
    MissingSafepoint(u32),
    InvalidRootMap(u32),
    InvalidLoopBound(u32),
    InvalidEffect,
    ImmediateRecursion,
    WcetComplexityLimit,
    InvalidEnumMetadata,
    InvalidStructMetadata,
    InvalidClassMetadata,
    InvalidSourceMap,
    EnumTypeOutOfRange(u64),
    EnumVariantOutOfRange(u64),
    StructTypeOutOfRange(u64),
    StructFieldOutOfRange(u64),
    ClassTypeOutOfRange(u64),
    ClassFieldOutOfRange(u64),
    InvalidReloadMetadata,
    InvalidRune(u32),
    StringOutOfRange(u32),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "verify error in function {} at {:?}: {:?}",
            self.function, self.instruction, self.kind
        )
    }
}

impl std::error::Error for VerifyError {}

#[derive(Clone, Debug)]
pub struct VerifiedModule(Module);

impl VerifiedModule {
    #[must_use]
    pub const fn module(&self) -> &Module {
        &self.0
    }

    #[must_use]
    pub fn into_module(self) -> Module {
        self.0
    }
}

pub fn verify(mut module: Module, limits: VerifierLimits) -> Result<VerifiedModule, VerifyError> {
    verify_reload_metadata(&module)?;
    verify_source_map(&module)?;
    verify_named_type_metadata(&module)?;
    verify_host_import_metadata(&module)?;
    let mut export_ids = BTreeSet::new();
    for export in &module.exports {
        if !export_ids.insert(export.stable_id) {
            return Err(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::DuplicateExport,
            });
        }
        let function = module
            .functions
            .get(export.function as usize)
            .ok_or(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::ExportOutOfRange(export.function),
            })?;
        if function.signature != export.signature {
            return Err(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::InvalidExportSignature,
            });
        }
    }
    let depths = static_call_depths(&module)?;
    for (function, depth) in module.functions.iter_mut().zip(depths) {
        function.max_static_call_depth = depth;
    }
    for (index, function) in module.functions.iter().enumerate() {
        verify_function(&module, index, function, limits)?;
        if function.effect == FunctionEffect::Immediate
            && immediate_wcet(&module, index, &mut Vec::new(), limits.max_wcet_states)?
                > limits.max_immediate_cost
        {
            return Err(VerifyError {
                function: index,
                instruction: None,
                kind: VerifyErrorKind::ImmediateCostLimit,
            });
        }
    }
    Ok(VerifiedModule(module))
}

fn verify_named_type_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut enum_ids = BTreeSet::new();
    for enum_type in &module.enum_types {
        let mut variant_ids = BTreeSet::new();
        let mut tags = BTreeSet::new();
        if !enum_ids.insert(enum_type.type_id)
            || enum_type.variants.is_empty()
            || enum_type
                .variants
                .iter()
                .any(|variant| !variant_ids.insert(variant.stable_id) || !tags.insert(variant.tag))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidEnumMetadata,
            });
        }
    }
    let mut named_ids = enum_ids;
    for struct_type in &module.struct_types {
        let mut field_ids = BTreeSet::new();
        if !named_ids.insert(struct_type.type_id)
            || struct_type.fields.len() > nexa_bytecode::MAX_STRUCT_FIELDS
            || struct_type
                .fields
                .iter()
                .any(|field| !field_ids.insert(field.stable_id))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidStructMetadata,
            });
        }
    }
    for class_type in &module.class_types {
        let mut field_ids = BTreeSet::new();
        if !named_ids.insert(class_type.type_id)
            || class_type.fields.len() > nexa_bytecode::MAX_CLASS_FIELDS
            || class_type
                .fields
                .iter()
                .any(|field| !field_ids.insert(field.stable_id))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidClassMetadata,
            });
        }
    }
    Ok(())
}

fn verify_host_import_metadata(module: &Module) -> Result<(), VerifyError> {
    for import in &module.host_imports {
        let valid = match (import.mode, import.async_result) {
            (HostCallMode::Immediate, None) => true,
            (HostCallMode::Async, Some(async_result)) => {
                import.result == Some(ValueType::Named(async_result.result_type))
                    && matches!(
                        (async_result.cancel_policy, async_result.cancel_error),
                        (nexa_bytecode::CancelPolicy::ReturnError, Some(_))
                            | (nexa_bytecode::CancelPolicy::CancelTask, None)
                    )
                    && matches!(
                        (async_result.abandon_policy, async_result.abandon_error),
                        (nexa_bytecode::AbandonPolicy::ReturnError, Some(_))
                            | (nexa_bytecode::AbandonPolicy::Trap, None)
                    )
                    && module.enum_types.iter().any(|enum_type| {
                        enum_type
                            == &nexa_bytecode::result_type(async_result.success, async_result.error)
                    })
            }
            _ => false,
        };
        if !valid {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidEnumMetadata,
            });
        }
    }
    Ok(())
}

fn verify_source_map(module: &Module) -> Result<(), VerifyError> {
    for entry in &module.source_map {
        let Some(function) = module.functions.get(entry.function as usize) else {
            return Err(VerifyError {
                function: entry.function as usize,
                instruction: None,
                kind: VerifyErrorKind::InvalidSourceMap,
            });
        };
        if entry.pc_start >= entry.pc_end
            || entry.pc_end as usize > function.code.len()
            || entry.span.is_empty()
        {
            return Err(VerifyError {
                function: entry.function as usize,
                instruction: Some(entry.pc_start as usize),
                kind: VerifyErrorKind::InvalidSourceMap,
            });
        }
    }
    Ok(())
}

pub fn verify_reload_transition(
    old: &VerifiedModule,
    candidate: &VerifiedModule,
) -> Result<(), VerifyError> {
    let old_hash = old.module().reload_metadata.stateful_schema_hash;
    let candidate_metadata = candidate.module().reload_metadata;
    if old_hash != candidate_metadata.stateful_schema_hash
        && candidate_metadata.migration_entry.is_none()
    {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidReloadMetadata,
        });
    }
    Ok(())
}

fn verify_reload_metadata(module: &Module) -> Result<(), VerifyError> {
    let invalid = |function| VerifyError {
        function,
        instruction: None,
        kind: VerifyErrorKind::InvalidReloadMetadata,
    };
    let migration_entries = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, function)| function.effect == FunctionEffect::Migration)
        .map(|(index, _)| u32::try_from(index).expect("module function count exceeds u32"))
        .collect::<Vec<_>>();
    if migration_entries.len() > 1
        || module.reload_metadata.migration_entry != migration_entries.first().copied()
    {
        return Err(invalid(
            usize::try_from(module.reload_metadata.migration_entry.unwrap_or_default())
                .unwrap_or(usize::MAX),
        ));
    }
    if let Some(entry) = module.reload_metadata.activation_entry {
        let entry = usize::try_from(entry).unwrap_or(usize::MAX);
        let function = module.functions.get(entry).ok_or_else(|| invalid(entry))?;
        if function.effect != FunctionEffect::Immediate {
            return Err(invalid(entry));
        }
    }
    let expected_schema_hash = module.state_schema.stable_hash();
    if module.reload_metadata.stateful_schema_hash != expected_schema_hash {
        return Err(invalid(0));
    }
    let required = minimum_migration_limits(module, module.reload_metadata.migration_entry);
    if !module
        .reload_metadata
        .minimum_migration_limits
        .satisfies(required)
    {
        return Err(invalid(
            usize::try_from(module.reload_metadata.migration_entry.unwrap_or_default())
                .unwrap_or(usize::MAX),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn verify_function(
    module: &Module,
    function_index: usize,
    function: &Function,
    limits: VerifierLimits,
) -> Result<(), VerifyError> {
    let error = |instruction, kind| VerifyError {
        function: function_index,
        instruction,
        kind,
    };
    if function.code.is_empty() {
        return Err(error(None, VerifyErrorKind::EmptyFunction));
    }
    if function.frame_bytes > limits.max_frame_bytes {
        return Err(error(None, VerifyErrorKind::FrameLimit));
    }
    if function.root_bitmap.len() != usize::from(function.registers) {
        return Err(error(None, VerifyErrorKind::RootBitmapLength));
    }
    verify_loop_bounds(function_index, function, limits)?;
    let register_count = usize::from(function.registers);
    let parameter_count = function.signature.parameters.len();
    if parameter_count > register_count {
        return Err(error(None, VerifyErrorKind::RegisterOutOfRange(u16::MAX)));
    }
    let mut entry = vec![None; register_count];
    for (register, ty) in function.signature.parameters.iter().copied().enumerate() {
        entry[register] = Some(ty);
    }
    let mut states = vec![None; function.code.len()];
    states[0] = Some(entry);
    let mut queue = VecDeque::from([0_usize]);
    while let Some(pc) = queue.pop_front() {
        let mut state = states[pc].clone().expect("queued state exists");
        let instruction = function.code[pc];
        let register = |value: u16| {
            if usize::from(value) < register_count {
                Ok(usize::from(value))
            } else {
                Err(error(Some(pc), VerifyErrorKind::RegisterOutOfRange(value)))
            }
        };
        let require = |state: &[Option<ValueType>], value: u16, ty: ValueType| {
            let index = register(value)?;
            if state[index] == Some(ty) {
                Ok(index)
            } else {
                Err(error(Some(pc), VerifyErrorKind::TypeMismatch))
            }
        };
        let mut successors = Vec::with_capacity(2);
        match instruction {
            Instruction::LoadI32 { dst, .. } => state[register(dst)?] = Some(ValueType::I32),
            Instruction::LoadI64 { dst, .. } => state[register(dst)?] = Some(ValueType::I64),
            Instruction::LoadF32 { dst, .. } => state[register(dst)?] = Some(ValueType::F32),
            Instruction::LoadF64 { dst, .. } => state[register(dst)?] = Some(ValueType::F64),
            Instruction::LoadRune { dst, value } => {
                if char::from_u32(value).is_none() {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidRune(value)));
                }
                state[register(dst)?] = Some(ValueType::Rune);
            }
            Instruction::LoadString { dst, string } => {
                if string as usize >= module.strings.len() {
                    return Err(error(Some(pc), VerifyErrorKind::StringOutOfRange(string)));
                }
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::LoadBool { dst, .. } => state[register(dst)?] = Some(ValueType::Bool),
            Instruction::Move { dst, source } => {
                let source = register(source)?;
                let ty =
                    state[source].ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                state[register(dst)?] = Some(ty);
            }
            Instruction::Add { dst, lhs, rhs }
            | Instruction::Sub { dst, lhs, rhs }
            | Instruction::Mul { dst, lhs, rhs }
            | Instruction::Div { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I32)?;
                require(&state, rhs, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::AddI64 { dst, lhs, rhs }
            | Instruction::SubI64 { dst, lhs, rhs }
            | Instruction::MulI64 { dst, lhs, rhs }
            | Instruction::DivI64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I64)?;
                require(&state, rhs, ValueType::I64)?;
                state[register(dst)?] = Some(ValueType::I64);
            }
            Instruction::AddF32 { dst, lhs, rhs }
            | Instruction::SubF32 { dst, lhs, rhs }
            | Instruction::MulF32 { dst, lhs, rhs }
            | Instruction::DivF32 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F32)?;
                require(&state, rhs, ValueType::F32)?;
                state[register(dst)?] = Some(ValueType::F32);
            }
            Instruction::AddF64 { dst, lhs, rhs }
            | Instruction::SubF64 { dst, lhs, rhs }
            | Instruction::MulF64 { dst, lhs, rhs }
            | Instruction::DivF64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F64)?;
                require(&state, rhs, ValueType::F64)?;
                state[register(dst)?] = Some(ValueType::F64);
            }
            Instruction::StringLen { dst, source } | Instruction::StringByteLen { dst, source } => {
                require(&state, source, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::StringEqual { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::String)?;
                require(&state, rhs, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::StringConcat { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::String)?;
                require(&state, rhs, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::StringRuneAt { dst, source, index } => {
                require(&state, source, ValueType::String)?;
                require(&state, index, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::Rune);
            }
            Instruction::StringHash { dst, source } => {
                require(&state, source, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::I64);
            }
            Instruction::CompareEq { dst, lhs, rhs } => {
                let lhs = register(lhs)?;
                let rhs = register(rhs)?;
                if state[lhs].is_none() || state[lhs] != state[rhs] {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::Jump { target } => {
                successors.push(target_index(function, function_index, pc, target)?);
            }
            Instruction::JumpIfFalse { condition, target } => {
                require(&state, condition, ValueType::Bool)?;
                successors.push(target_index(function, function_index, pc, target)?);
                if pc + 1 < function.code.len() {
                    successors.push(pc + 1);
                }
            }
            Instruction::Call {
                function: callee,
                args_base,
                args_count,
                dst,
            } => {
                let callee = module
                    .functions
                    .get(callee as usize)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::FunctionOutOfRange(callee)))?;
                if (function.effect == FunctionEffect::Immediate
                    && callee.effect != FunctionEffect::Immediate)
                    || (callee.effect == FunctionEffect::Task
                        && function.effect != FunctionEffect::Task)
                    || (function.effect == FunctionEffect::Migration
                        && !matches!(
                            callee.effect,
                            FunctionEffect::Ordinary | FunctionEffect::Migration
                        ))
                    || (callee.effect == FunctionEffect::Migration
                        && function.effect != FunctionEffect::Migration)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                if usize::from(args_count) != callee.signature.parameters.len() {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (argument, ty) in callee.signature.parameters.iter().copied().enumerate() {
                    let argument = args_base
                        .checked_add(u16::try_from(argument).unwrap())
                        .ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?;
                    require(&state, argument, ty)?;
                }
                if let Some(result) = callee.signature.result {
                    state[register(dst)?] = Some(result);
                }
            }
            Instruction::HostCall {
                import,
                args_base,
                args_count,
                dst,
            } => {
                let host = module.host_imports.get(import as usize).ok_or_else(|| {
                    error(Some(pc), VerifyErrorKind::HostImportOutOfRange(import))
                })?;
                if usize::from(args_count) != host.parameters.len()
                    || matches!(
                        function.effect,
                        FunctionEffect::Migration | FunctionEffect::Cleanup
                    )
                    || (host.mode == HostCallMode::Async && function.effect != FunctionEffect::Task)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                for (argument, ty) in host.parameters.iter().copied().enumerate() {
                    let argument = args_base
                        .checked_add(u16::try_from(argument).unwrap())
                        .ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?;
                    require(&state, argument, ty)?;
                }
                if let Some(result) = host.result {
                    state[register(dst)?] = Some(result);
                }
            }
            Instruction::StateOldGet { ty, dst, .. } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                state[register(dst)?] = Some(ty);
            }
            Instruction::StateOldFieldGet {
                object, ty, dst, ..
            } => {
                if function.effect != FunctionEffect::Migration
                    || !matches!(state[register(object)?], Some(ValueType::Named(_)))
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                state[register(dst)?] = Some(ty);
            }
            Instruction::StateHandleResolve {
                handle,
                target,
                result_type,
                dst,
            } => {
                if matches!(
                    function.effect,
                    FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                let expected_result = nexa_bytecode::result_type(
                    target,
                    ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
                );
                if result_type != expected_result.type_id
                    || !module
                        .enum_types
                        .iter()
                        .any(|enum_type| enum_type == &expected_result)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                state[register(dst)?] = Some(ValueType::Named(result_type));
            }
            Instruction::StateHandleIsAlive {
                handle,
                target,
                dst,
            }
            | Instruction::StateHandleGeneration {
                handle,
                target,
                dst,
            }
            | Instruction::StateHandleHash {
                handle,
                target,
                dst,
            } => {
                if matches!(
                    function.effect,
                    FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                state[register(dst)?] = Some(
                    if matches!(instruction, Instruction::StateHandleIsAlive { .. }) {
                        ValueType::Bool
                    } else {
                        ValueType::I32
                    },
                );
            }
            Instruction::StateHandleStableId {
                handle,
                target,
                dst,
            } => {
                if matches!(
                    function.effect,
                    FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                state[register(dst)?] = Some(nexa_bytecode::stable_id_type());
            }
            Instruction::StateHandleEqual {
                lhs,
                rhs,
                target,
                dst,
            } => {
                if matches!(
                    function.effect,
                    FunctionEffect::Migration | FunctionEffect::Cleanup
                ) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let handle_type = ValueType::Named(nexa_bytecode::state_handle_type(target));
                require(&state, lhs, handle_type)?;
                require(&state, rhs, handle_type)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::StateNewCreate { type_id, dst, .. } => {
                if function.effect != FunctionEffect::Migration
                    || !module
                        .state_schema
                        .types
                        .iter()
                        .any(|state_type| state_type.stable_id == type_id)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::StateNewSet {
                object,
                field_id,
                source,
            } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let object = register(object)?;
                let Some(ValueType::Named(type_id)) = state[object] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let field = module
                    .state_schema
                    .types
                    .iter()
                    .find(|state_type| state_type.stable_id == type_id)
                    .and_then(|state_type| {
                        state_type
                            .fields
                            .iter()
                            .find(|field| field.stable_id == field_id)
                    })
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                require(&state, source, field.ty)?;
            }
            Instruction::StateReplace { target, .. } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let target = register(target)?;
                if !matches!(state[target], Some(ValueType::Named(_))) {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
            }
            Instruction::StateDelete { .. }
            | Instruction::StatePreserve { .. }
            | Instruction::StateFinish => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
            }
            Instruction::EnumNew {
                type_id,
                variant,
                payload,
                dst,
            } => {
                let enum_type = module
                    .enum_types
                    .iter()
                    .find(|enum_type| enum_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumTypeOutOfRange(type_id.0))
                    })?;
                let variant = enum_type
                    .variants
                    .iter()
                    .find(|candidate| candidate.stable_id == variant)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumVariantOutOfRange(variant.0))
                    })?;
                match (variant.payload_type, payload) {
                    (Some(expected), Some(payload)) => {
                        require(&state, payload, expected)?;
                    }
                    (None, None) => {}
                    _ => return Err(error(Some(pc), VerifyErrorKind::TypeMismatch)),
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::EnumTag { source, dst } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .enum_types
                    .iter()
                    .any(|enum_type| enum_type.type_id == type_id)
                {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::EnumTypeOutOfRange(type_id.0),
                    ));
                }
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::EnumPayload {
                source,
                variant,
                dst,
            } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let payload_type = module
                    .enum_types
                    .iter()
                    .find(|enum_type| enum_type.type_id == type_id)
                    .and_then(|enum_type| {
                        enum_type
                            .variants
                            .iter()
                            .find(|candidate| candidate.stable_id == variant)
                    })
                    .and_then(|variant| variant.payload_type)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumVariantOutOfRange(variant.0))
                    })?;
                state[register(dst)?] = Some(payload_type);
            }
            Instruction::StructNew {
                type_id,
                fields_base,
                fields_count,
                dst,
            } => {
                let struct_type = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructTypeOutOfRange(type_id.0))
                    })?;
                if usize::from(fields_count) != struct_type.fields.len() {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (index, field) in struct_type.fields.iter().enumerate() {
                    let source = fields_base
                        .checked_add(u16::try_from(index).map_err(|_| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?)
                        .ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?;
                    require(&state, source, field.ty)?;
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::StructGet { source, field, dst } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let field_type = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| {
                        struct_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructFieldOutOfRange(field.0))
                    })?;
                state[register(dst)?] = Some(field_type);
            }
            Instruction::StructWith {
                source,
                field,
                value,
                dst,
            } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let field_type = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| {
                        struct_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructFieldOutOfRange(field.0))
                    })?;
                require(&state, value, field_type)?;
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::StructEqual { lhs, rhs, dst } => {
                let lhs = register(lhs)?;
                let Some(ValueType::Named(type_id)) = state[lhs] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .struct_types
                    .iter()
                    .any(|struct_type| struct_type.type_id == type_id)
                {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::StructTypeOutOfRange(type_id.0),
                    ));
                }
                require(&state, rhs, ValueType::Named(type_id))?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::ClassNew {
                type_id,
                fields_base,
                fields_count,
                dst,
            } => {
                let class_type = module
                    .class_types
                    .iter()
                    .find(|class_type| class_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassTypeOutOfRange(type_id.0))
                    })?;
                if usize::from(fields_count) != class_type.fields.len() {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (index, field) in class_type.fields.iter().enumerate() {
                    let source = fields_base
                        .checked_add(u16::try_from(index).map_err(|_| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?)
                        .ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?;
                    require(&state, source, field.ty)?;
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::ClassGet { source, field, dst } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let field_type = module
                    .class_types
                    .iter()
                    .find(|class_type| class_type.type_id == type_id)
                    .and_then(|class_type| {
                        class_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                state[register(dst)?] = Some(field_type);
            }
            Instruction::ClassSet {
                source,
                field,
                value,
            } => {
                let source = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let field_type = module
                    .class_types
                    .iter()
                    .find(|class_type| class_type.type_id == type_id)
                    .and_then(|class_type| {
                        class_type
                            .fields
                            .iter()
                            .find(|candidate| candidate.stable_id == field)
                    })
                    .map(|field| field.ty)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                require(&state, value, field_type)?;
            }
            Instruction::ClassEqual { lhs, rhs, dst } => {
                let lhs = register(lhs)?;
                let Some(ValueType::Named(type_id)) = state[lhs] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .class_types
                    .iter()
                    .any(|class_type| class_type.type_id == type_id)
                {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::ClassTypeOutOfRange(type_id.0),
                    ));
                }
                require(&state, rhs, ValueType::Named(type_id))?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::Return { source } => {
                let result = function
                    .signature
                    .result
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidReturn))?;
                require(&state, source, result)?;
            }
            Instruction::ReturnVoid => {
                if function.signature.result.is_some() {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidReturn));
                }
            }
            Instruction::DeferPush {
                function: cleanup,
                args_base,
                args_count,
            } => {
                let cleanup = module
                    .functions
                    .get(cleanup as usize)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::FunctionOutOfRange(cleanup)))?;
                if !matches!(
                    cleanup.effect,
                    FunctionEffect::Ordinary | FunctionEffect::Cleanup
                ) || cleanup.signature.parameters.len() != usize::from(args_count)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                for (argument, ty) in cleanup.signature.parameters.iter().copied().enumerate() {
                    let argument = args_base
                        .checked_add(u16::try_from(argument).unwrap())
                        .ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                        })?;
                    require(&state, argument, ty)?;
                }
            }
            Instruction::Yield if !matches!(function.effect, FunctionEffect::Task) => {
                return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
            }
            Instruction::CleanupReturn if function.effect != FunctionEffect::Cleanup => {
                return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
            }
            Instruction::Safepoint
            | Instruction::Yield
            | Instruction::Trap
            | Instruction::DeferPop
            | Instruction::CleanupReturn => {}
        }
        if !matches!(
            instruction,
            Instruction::Jump { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::CleanupReturn
                | Instruction::Trap
        ) && successors.is_empty()
            && pc + 1 < function.code.len()
        {
            successors.push(pc + 1);
        }
        for successor in successors {
            match &mut states[successor] {
                None => {
                    states[successor] = Some(state.clone());
                    queue.push_back(successor);
                }
                Some(existing) if *existing == state => {}
                Some(existing) => {
                    let mut changed = false;
                    for (current, incoming) in existing.iter_mut().zip(&state) {
                        match (*current, *incoming) {
                            (Some(lhs), Some(rhs)) if lhs != rhs => {
                                *current = None;
                                changed = true;
                            }
                            (Some(_), None) => {
                                *current = None;
                                changed = true;
                            }
                            (None, _) | (Some(_), Some(_)) => {}
                        }
                    }
                    if changed {
                        queue.push_back(successor);
                    }
                }
            }
        }
    }
    for register in 0..register_count {
        let can_hold_ref = states
            .iter()
            .flatten()
            .any(|state| state[register].is_some_and(ValueType::is_reference));
        match (function.root_bitmap[register], can_hold_ref) {
            (true, false) => {
                return Err(error(
                    None,
                    VerifyErrorKind::ForgedRoot(u16::try_from(register).unwrap()),
                ));
            }
            (false, true) => {
                return Err(error(
                    None,
                    VerifyErrorKind::MissingRoot(u16::try_from(register).unwrap()),
                ));
            }
            _ => {}
        }
    }
    verify_safepoints(function_index, function, &states)?;
    Ok(())
}

fn verify_loop_bounds(
    function_index: usize,
    function: &Function,
    limits: VerifierLimits,
) -> Result<(), VerifyError> {
    let mut seen = BTreeSet::new();
    for loop_bound in &function.loop_bounds {
        let pc = usize::try_from(loop_bound.back_edge).unwrap_or(usize::MAX);
        let valid_edge = function.code.get(pc).is_some_and(|instruction| {
            matches!(instruction, Instruction::Jump { target } if *target <= loop_bound.back_edge)
                || matches!(
                    instruction,
                    Instruction::JumpIfFalse { target, .. } if *target <= loop_bound.back_edge
                )
        });
        if !valid_edge
            || loop_bound.max_iterations == 0
            || !seen.insert(loop_bound.back_edge)
            || (function.effect == FunctionEffect::Immediate
                && loop_bound.max_iterations > limits.max_immediate_cost)
        {
            return Err(VerifyError {
                function: function_index,
                instruction: (pc < function.code.len()).then_some(pc),
                kind: VerifyErrorKind::InvalidLoopBound(loop_bound.back_edge),
            });
        }
    }
    Ok(())
}

fn verify_safepoints(
    function_index: usize,
    function: &Function,
    states: &[Option<Vec<Option<ValueType>>>],
) -> Result<(), VerifyError> {
    let mut mapped = BTreeSet::new();
    for root_map in &function.root_maps {
        let pc = usize::try_from(root_map.pc).unwrap_or(usize::MAX);
        if pc >= function.code.len()
            || root_map.bitmap.len() != usize::from(function.registers)
            || !mapped.insert(root_map.pc)
        {
            return Err(VerifyError {
                function: function_index,
                instruction: (pc < function.code.len()).then_some(pc),
                kind: VerifyErrorKind::InvalidRootMap(root_map.pc),
            });
        }
    }
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        let pc_u32 = u32::try_from(pc).expect("bytecode position fits u32");
        let required = pc == 0
            || (pc > 0 && matches!(function.code[pc - 1], Instruction::HostCall { .. }))
            || matches!(
                instruction,
                Instruction::Safepoint
                    | Instruction::Yield
                    | Instruction::LoadString { .. }
                    | Instruction::StringConcat { .. }
                    | Instruction::EnumNew { .. }
                    | Instruction::StructNew { .. }
                    | Instruction::StructWith { .. }
                    | Instruction::ClassNew { .. }
                    | Instruction::Call { .. }
                    | Instruction::HostCall { .. }
                    | Instruction::StateHandleResolve { .. }
                    | Instruction::Return { .. }
                    | Instruction::ReturnVoid
                    | Instruction::Trap
                    | Instruction::CleanupReturn
            )
            || matches!(instruction, Instruction::Jump { target } if target <= pc_u32)
            || matches!(
                instruction,
                Instruction::JumpIfFalse { target, .. } if target <= pc_u32
            );
        if required && !function.safepoints.contains(&pc_u32) {
            return Err(VerifyError {
                function: function_index,
                instruction: Some(pc),
                kind: VerifyErrorKind::MissingSafepoint(pc_u32),
            });
        }
        if required {
            let Some(root_map) = function.root_maps.iter().find(|map| map.pc == pc_u32) else {
                return Err(VerifyError {
                    function: function_index,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::InvalidRootMap(pc_u32),
                });
            };
            let exact = states[pc].as_ref().map_or_else(
                || vec![false; usize::from(function.registers)],
                |state| {
                    state
                        .iter()
                        .map(|ty| ty.is_some_and(ValueType::is_reference))
                        .collect()
                },
            );
            if root_map.bitmap != exact {
                return Err(VerifyError {
                    function: function_index,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::InvalidRootMap(pc_u32),
                });
            }
        } else if mapped.contains(&pc_u32) {
            return Err(VerifyError {
                function: function_index,
                instruction: Some(pc),
                kind: VerifyErrorKind::InvalidRootMap(pc_u32),
            });
        }
    }
    Ok(())
}

fn target_index(
    function: &Function,
    function_index: usize,
    pc: usize,
    target: u32,
) -> Result<usize, VerifyError> {
    let target_index = usize::try_from(target).unwrap_or(usize::MAX);
    if target_index < function.code.len() {
        Ok(target_index)
    } else {
        Err(VerifyError {
            function: function_index,
            instruction: Some(pc),
            kind: VerifyErrorKind::JumpOutOfRange(target),
        })
    }
}

fn static_call_depths(module: &Module) -> Result<Vec<u16>, VerifyError> {
    (0..module.functions.len())
        .map(|function| call_depth(module, function, &mut Vec::new()))
        .collect()
}

fn call_depth(
    module: &Module,
    function: usize,
    visiting: &mut Vec<usize>,
) -> Result<u16, VerifyError> {
    if visiting.contains(&function) {
        if visiting
            .iter()
            .any(|index| module.functions[*index].effect == FunctionEffect::Immediate)
        {
            return Err(VerifyError {
                function,
                instruction: None,
                kind: VerifyErrorKind::ImmediateRecursion,
            });
        }
        return Ok(u16::MAX);
    }
    visiting.push(function);
    let mut depth = 1_u16;
    for instruction in &module.functions[function].code {
        if let Instruction::Call {
            function: callee, ..
        } = instruction
        {
            let callee_depth = call_depth(module, *callee as usize, visiting)?;
            depth = depth.max(callee_depth.saturating_add(1));
        }
    }
    visiting.pop();
    Ok(depth)
}

fn immediate_wcet(
    module: &Module,
    function: usize,
    visiting: &mut Vec<usize>,
    max_states: u32,
) -> Result<u32, VerifyError> {
    if visiting.contains(&function) {
        return Err(VerifyError {
            function,
            instruction: None,
            kind: VerifyErrorKind::ImmediateRecursion,
        });
    }
    visiting.push(function);
    let mut remaining = module.functions[function]
        .loop_bounds
        .iter()
        .map(|loop_bound| (loop_bound.back_edge, loop_bound.max_iterations))
        .collect::<Vec<_>>();
    let mut memo = BTreeMap::new();
    let mut explored = 0_u32;
    let cost = longest_path(
        module,
        function,
        0,
        visiting,
        &mut remaining,
        &mut memo,
        &mut explored,
        max_states,
    )?;
    visiting.pop();
    Ok(cost)
}

type WcetMemo = BTreeMap<(usize, Vec<(u32, u32)>), u32>;

#[allow(clippy::too_many_arguments)]
fn longest_path(
    module: &Module,
    function: usize,
    pc: usize,
    visiting: &mut Vec<usize>,
    remaining: &mut [(u32, u32)],
    memo: &mut WcetMemo,
    explored: &mut u32,
    max_states: u32,
) -> Result<u32, VerifyError> {
    let key = (pc, remaining.to_vec());
    if let Some(cost) = memo.get(&key) {
        return Ok(*cost);
    }
    *explored = explored.saturating_add(1);
    if *explored > max_states {
        return Err(VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::WcetComplexityLimit,
        });
    }
    let instruction = module.functions[function].code[pc];
    let callee_cost = if let Instruction::Call {
        function: callee, ..
    } = instruction
    {
        immediate_wcet(module, callee as usize, visiting, max_states)?
    } else if let Instruction::HostCall { import, .. } = instruction {
        module
            .host_imports
            .get(import as usize)
            .ok_or(VerifyError {
                function,
                instruction: Some(pc),
                kind: VerifyErrorKind::HostImportOutOfRange(import),
            })?
            .fuel_cost
    } else {
        0
    };
    let successors = match instruction {
        Instruction::Jump { target } => {
            vec![target as usize]
        }
        Instruction::JumpIfFalse { target, .. } => {
            let mut successors = vec![target as usize];
            if pc + 1 < module.functions[function].code.len() {
                successors.push(pc + 1);
            }
            successors
        }
        Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn
        | Instruction::Trap => Vec::new(),
        _ if pc + 1 < module.functions[function].code.len() => vec![pc + 1],
        _ => Vec::new(),
    };
    let mut suffix = 0;
    for successor in successors {
        let mut branch_remaining = remaining.to_vec();
        if successor <= pc {
            let back_edge = u32::try_from(pc).expect("bytecode position fits u32");
            let Some((_, budget)) = branch_remaining
                .iter_mut()
                .find(|(edge, _)| *edge == back_edge)
            else {
                return Err(VerifyError {
                    function,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::ImmediateCostLimit,
                });
            };
            if *budget == 0 {
                continue;
            }
            *budget -= 1;
        }
        suffix = suffix.max(longest_path(
            module,
            function,
            successor,
            visiting,
            &mut branch_remaining,
            memo,
            explored,
            max_states,
        )?);
    }
    let cost = 1_u32
        .checked_add(callee_cost)
        .and_then(|value| value.checked_add(suffix))
        .ok_or(VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })?;
    memo.insert(key, cost);
    Ok(cost)
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        ClassType, FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, RootMap, Signature,
        SourceMapEntry, StateSchema, StateType, StructField, ValueType,
    };
    use nexa_core::{FileId, SourceSpan, StableId};

    use super::{VerifierLimits, VerifyErrorKind, verify, verify_reload_transition};

    #[test]
    fn class_metadata_rejects_duplicate_fields_and_named_type_collisions() {
        let type_id = StableId::from_name("Node");
        let field = StructField {
            stable_id: StableId::from_parts(&["Node", "::value"]),
            ty: ValueType::I32,
        };
        let mut duplicate = ModuleBuilder::new();
        duplicate.class_type(ClassType {
            type_id,
            fields: vec![field, field],
        });
        assert_eq!(
            verify(duplicate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidClassMetadata
        );

        let mut collision = ModuleBuilder::new();
        collision
            .struct_type(nexa_bytecode::StructType {
                type_id,
                fields: vec![field],
            })
            .class_type(ClassType {
                type_id,
                fields: vec![field],
            });
        assert_eq!(
            verify(collision.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidClassMetadata
        );
    }

    #[test]
    fn rejects_source_map_ranges_outside_their_function() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        function.emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        module.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 2,
            span: SourceSpan::new(FileId(1), 2, 3),
        }]);

        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidSourceMap
        );
    }

    #[test]
    fn rejects_non_scalar_rune_constants() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Rune),
            },
            1,
        );
        function
            .emit(Instruction::LoadRune {
                dst: 0,
                value: 0xD800,
            })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidRune(0xD800)
        );
    }

    #[test]
    fn rejects_bad_jump_type_and_forged_root_bitmap() {
        let signature = Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        };
        let mut function = FunctionBuilder::new(signature.clone(), 1);
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 1 })
            .emit(Instruction::Jump { target: 99 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::JumpOutOfRange(99)
        ));

        let mut function = FunctionBuilder::new(signature, 1);
        function
            .set_root(0)
            .unwrap()
            .emit(Instruction::LoadI32 { dst: 0, value: 1 })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ForgedRoot(0)
        ));

        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            1,
        );
        function.emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::MissingRoot(0)
        ));

        let eight_i32 = vec![ValueType::I32; 8];
        let mut target_function = FunctionBuilder::new(
            Signature {
                parameters: eight_i32.clone(),
                result: Some(ValueType::I32),
            },
            8,
        );
        target_function.emit(Instruction::Return { source: 0 });
        let mut out_of_range_call = FunctionBuilder::new(
            Signature {
                parameters: eight_i32,
                result: Some(ValueType::I32),
            },
            8,
        );
        out_of_range_call
            .emit(Instruction::Call {
                function: 0,
                args_base: 1,
                args_count: 8,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(target_function.finish().unwrap());
        module.function(out_of_range_call.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::RegisterOutOfRange(8)
        ));
    }

    #[test]
    fn immediate_wcet_requires_and_consumes_static_loop_bounds() {
        fn immediate_loop(bound: Option<u32>) -> nexa_bytecode::Module {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: Some(ValueType::I32),
                },
                2,
            );
            function
                .effect(FunctionEffect::Immediate)
                .emit(Instruction::LoadI32 { dst: 0, value: 1 })
                .emit(Instruction::LoadBool {
                    dst: 1,
                    value: false,
                })
                .emit(Instruction::JumpIfFalse {
                    condition: 1,
                    target: 5,
                })
                .emit(Instruction::Safepoint)
                .emit(Instruction::Jump { target: 2 })
                .emit(Instruction::Return { source: 0 });
            if let Some(bound) = bound {
                function.loop_bound(4, bound);
            }
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            module.finish()
        }

        assert!(matches!(
            verify(immediate_loop(None), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ImmediateCostLimit
        ));
        assert!(verify(immediate_loop(Some(3)), VerifierLimits::default()).is_ok());
    }

    #[test]
    fn reload_metadata_is_verified_independently_of_the_compiler() {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);
        let migration = migration.finish().unwrap();

        let mut forged = ModuleBuilder::new();
        forged.function(migration.clone());
        let mut forged = forged.finish();
        forged.reload_metadata.migration_entry = None;
        assert!(matches!(
            verify(forged, VerifierLimits::default()).unwrap_err().kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut underreported = ModuleBuilder::new();
        underreported.function(migration.clone());
        let mut underreported = underreported.finish();
        underreported
            .reload_metadata
            .minimum_migration_limits
            .max_fuel = 0;
        assert!(matches!(
            verify(underreported, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut duplicate = ModuleBuilder::new();
        duplicate.function(migration.clone());
        duplicate.function(migration);
        assert!(matches!(
            verify(duplicate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut ordinary = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        ordinary.emit(Instruction::ReturnVoid);
        let ordinary = ordinary.finish().unwrap();
        let mut invalid_activation = ModuleBuilder::new();
        invalid_activation.function(ordinary.clone());
        let mut invalid_activation = invalid_activation.finish();
        invalid_activation.reload_metadata.activation_entry = Some(0);
        assert!(matches!(
            verify(invalid_activation, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let old_hash = nexa_bytecode::StateSchema::default().stable_hash();
        let mut new_hash = old_hash;
        new_hash.0 ^= 1;
        let mut old = ModuleBuilder::new();
        old.metadata(old_hash, old_hash).function(ordinary.clone());
        let old = verify(old.finish(), VerifierLimits::default()).unwrap();
        let mut candidate = ModuleBuilder::new();
        candidate
            .metadata(new_hash, new_hash)
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: old_hash,
                    version: 1,
                    fields: Vec::new(),
                }],
            })
            .function(ordinary);
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();
        assert!(matches!(
            verify_reload_transition(&old, &candidate).unwrap_err().kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));
    }

    #[test]
    fn root_maps_are_exact_for_each_safepoint_program_counter() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            2,
        );
        function
            .set_root(0)
            .unwrap()
            .set_root(1)
            .unwrap()
            .emit(Instruction::Move { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function.clone());
        assert!(verify(module.finish(), VerifierLimits::default()).is_ok());

        function.root_maps[0].bitmap[1] = true;
        let mut module = ModuleBuilder::new();
        module.function(function);
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidRootMap(0)
        ));
    }
}
