//! Structural, type and continuation verification for Nexa bytecode.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use nexa_bytecode::{Function, FunctionEffect, Instruction, Module, ValueType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifierLimits {
    pub max_frame_bytes: u32,
    pub max_immediate_cost: u32,
}

impl Default for VerifierLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_immediate_cost: 1_024,
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
}

pub fn verify(mut module: Module, limits: VerifierLimits) -> Result<VerifiedModule, VerifyError> {
    let depths = static_call_depths(&module)?;
    for (function, depth) in module.functions.iter_mut().zip(depths) {
        function.max_static_call_depth = depth;
    }
    for (index, function) in module.functions.iter().enumerate() {
        verify_function(&module, index, function, limits)?;
        if function.effect == FunctionEffect::Immediate
            && immediate_wcet(&module, index, &mut Vec::new())? > limits.max_immediate_cost
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
            Instruction::LoadBool { dst, .. } => state[register(dst)?] = Some(ValueType::Bool),
            Instruction::Move { dst, source } => {
                let source = register(source)?;
                let ty =
                    state[source].ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                state[register(dst)?] = Some(ty);
            }
            Instruction::Add { dst, lhs, rhs }
            | Instruction::Sub { dst, lhs, rhs }
            | Instruction::Mul { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I32)?;
                require(&state, rhs, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::I32);
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
    verify_safepoints(function_index, function)?;
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

fn verify_safepoints(function_index: usize, function: &Function) -> Result<(), VerifyError> {
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        let pc_u32 = u32::try_from(pc).expect("bytecode position fits u32");
        let required = pc == 0
            || matches!(
                instruction,
                Instruction::Safepoint
                    | Instruction::Yield
                    | Instruction::Call { .. }
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
        if required
            && !function
                .root_maps
                .iter()
                .any(|map| map.pc == pc_u32 && map.bitmap.len() == usize::from(function.registers))
        {
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
    let cost = longest_path(module, function, 0, visiting, &mut remaining)?;
    visiting.pop();
    Ok(cost)
}

fn longest_path(
    module: &Module,
    function: usize,
    pc: usize,
    visiting: &mut Vec<usize>,
    remaining: &mut [(u32, u32)],
) -> Result<u32, VerifyError> {
    let instruction = module.functions[function].code[pc];
    let callee_cost = if let Instruction::Call {
        function: callee, ..
    } = instruction
    {
        immediate_wcet(module, callee as usize, visiting)?
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
    Ok(cost)
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
    };

    use super::{VerifierLimits, VerifyErrorKind, verify};

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
}
