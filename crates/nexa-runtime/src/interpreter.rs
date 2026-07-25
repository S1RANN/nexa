use std::fmt;

use nexa_bytecode::{AsyncResultType, FunctionEffect, HostCallMode, Instruction, ValueType};
use nexa_core::StableId;
use nexa_verifier::VerifiedModule;

use crate::{
    ContinuationReservation, FrameArena, FrameError, FrameLimits, GcRef, Heap, HeapError,
    RuntimeValue,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendReason {
    Fuel,
    ExplicitYield,
    HostRequest,
    ReloadPause,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionCharge {
    pub instructions: u64,
    pub fuel_used: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuelState {
    pub slice_remaining: u64,
    pub cumulative_used: u64,
    pub cumulative_limit: u64,
}

impl FuelState {
    #[must_use]
    pub const fn new(slice: u64, cumulative_used: u64, cumulative_limit: u64) -> Self {
        Self {
            slice_remaining: slice,
            cumulative_used,
            cumulative_limit,
        }
    }
}

#[derive(Clone, Debug)]
pub struct InterpreterContinuation {
    arena: FrameArena,
    current_function: u32,
    suspend_reason: Option<SuspendReason>,
    pending_fuel: u64,
    cumulative_exhausted: bool,
}

impl InterpreterContinuation {
    pub fn new(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
    ) -> Result<Self, InterpreterError> {
        let function_meta = module
            .module()
            .functions
            .get(function as usize)
            .ok_or(InterpreterError::MissingFunction(function))?;
        validate_arguments(arguments, &function_meta.signature.parameters)?;
        let mut arena = FrameArena::with_reserved_capacity(limits, reservation)?;
        arena.push_call(function, function_meta.registers, None)?;
        for (index, argument) in arguments.iter().copied().enumerate() {
            arena.set_register(index, argument)?;
        }
        Ok(Self {
            arena,
            current_function: function,
            suspend_reason: None,
            pending_fuel: 0,
            cumulative_exhausted: false,
        })
    }

    #[must_use]
    pub const fn suspend_reason(&self) -> Option<SuspendReason> {
        self.suspend_reason
    }

    #[must_use]
    pub const fn current_function(&self) -> u32 {
        self.current_function
    }

    #[must_use]
    pub fn arena(&self) -> &FrameArena {
        &self.arena
    }

    #[must_use]
    pub const fn cumulative_exhausted(&self) -> bool {
        self.cumulative_exhausted
    }

    pub(crate) fn write_resume_value(
        &mut self,
        destination: u16,
        expected: Option<ValueType>,
        value: RuntimeValue,
    ) -> Result<(), InterpreterError> {
        if runtime_value_type(value) != expected {
            return Err(InterpreterError::TypeMismatch);
        }
        if expected.is_some() {
            set_register(&mut self.arena, destination, value)?;
        }
        Ok(())
    }

    pub fn gc_roots(&self, module: &VerifiedModule) -> Result<Vec<GcRef>, InterpreterError> {
        self.arena
            .iter_gc_roots(|function, pc| {
                module
                    .module()
                    .functions
                    .get(function as usize)
                    .and_then(|function| {
                        function
                            .root_maps
                            .iter()
                            .find(|root_map| root_map.pc == pc)
                            .or_else(|| {
                                function
                                    .root_maps
                                    .iter()
                                    .filter(|root_map| root_map.pc <= pc)
                                    .max_by_key(|root_map| root_map.pc)
                            })
                            .map(|root_map| root_map.bitmap.clone())
                    })
            })
            .map_err(Into::into)
    }

    pub fn checked_gc_roots(
        &self,
        module: &VerifiedModule,
    ) -> Result<Vec<GcRef>, InterpreterError> {
        let metadata = self.gc_roots(module)?;
        let observed = self.arena.gc_roots();
        if metadata != observed {
            return Err(InterpreterError::RootMapMismatch);
        }
        Ok(metadata)
    }
}

#[derive(Clone, Debug)]
pub enum InterpreterOutcome {
    Returned {
        value: Option<RuntimeValue>,
        charge: ExecutionCharge,
        fuel: FuelState,
    },
    Suspended {
        continuation: InterpreterContinuation,
        reason: SuspendReason,
        charge: ExecutionCharge,
        fuel: FuelState,
    },
    HostPending {
        continuation: InterpreterContinuation,
        request: crate::HostRequestHandle,
        destination: u16,
        expected_type: Option<ValueType>,
        async_result: Option<AsyncResultType>,
        charge: ExecutionCharge,
        fuel: FuelState,
    },
    Trapped {
        trap: Trap,
        charge: ExecutionCharge,
        fuel: FuelState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trap {
    pub kind: TrapKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    BytecodeTrap,
    CleanupBudgetExceeded,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpreterError {
    MissingFunction(u32),
    ArgumentCount,
    TypeMismatch,
    RegisterOutOfRange(u16),
    JumpOutOfRange(u32),
    FellOffFunction,
    ContinuationLimit(FrameError),
    RootMapMismatch,
    HostUnavailable,
    Host(crate::HostTrap),
    Migration(String),
    Heap(HeapError),
}

impl fmt::Display for InterpreterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for InterpreterError {}

impl From<FrameError> for InterpreterError {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::RegisterOutOfRange => Self::RegisterOutOfRange(u16::MAX),
            error => Self::ContinuationLimit(error),
        }
    }
}

impl From<HeapError> for InterpreterError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcodeCostTable {
    pub version: u32,
    costs: [u16; 30],
}

impl Default for OpcodeCostTable {
    fn default() -> Self {
        Self {
            version: 1,
            costs: [1; 30],
        }
    }
}

impl OpcodeCostTable {
    #[must_use]
    pub fn cost(&self, instruction: Instruction) -> u64 {
        u64::from(self.costs[opcode_index(instruction)])
    }
}

pub struct CheckedInterpreter;

pub trait InterpreterHost {
    fn call(
        &mut self,
        import: u32,
        arguments: &[RuntimeValue],
    ) -> Result<InterpreterHostOutcome, crate::HostTrap>;
}

pub trait InterpreterMigration {
    fn old_get(&mut self, stable_id: StableId, expected: ValueType)
    -> Result<RuntimeValue, String>;
    fn old_field_get(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        expected: ValueType,
    ) -> Result<RuntimeValue, String>;
    fn new_create(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, String>;
    fn new_set(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        value: RuntimeValue,
    ) -> Result<(), String>;
    fn preserve(&mut self, stable_id: StableId) -> Result<(), String>;
    fn replace(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), String>;
    fn delete(&mut self, stable_id: StableId) -> Result<(), String>;
    fn finish_staging(&mut self) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpreterHostOutcome {
    Immediate(RuntimeValue),
    Pending(crate::HostRequestHandle),
}

impl CheckedInterpreter {
    pub fn start(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
    ) -> Result<InterpreterContinuation, InterpreterError> {
        InterpreterContinuation::new(module, function, arguments, limits, reservation)
    }

    pub fn poll(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(module, continuation, fuel, costs, None, None, None)
    }

    pub fn poll_with_host(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(module, continuation, fuel, costs, Some(host), None, None)
    }

    pub fn poll_with_heap(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(module, continuation, fuel, costs, None, None, Some(heap))
    }

    pub fn poll_with_host_and_heap(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            Some(heap),
        )
    }

    pub fn run(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let limits = FrameLimits::default();
        let continuation = Self::start(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
        )?;
        Self::poll(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            &OpcodeCostTable::default(),
        )
    }

    pub fn run_with_heap(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let limits = FrameLimits::default();
        let continuation = Self::start(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
        )?;
        Self::poll_with_heap(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            &OpcodeCostTable::default(),
            heap,
        )
    }

    pub fn run_migration(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
        migration: &mut dyn InterpreterMigration,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let limits = FrameLimits::default();
        let continuation = Self::start(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
        )?;
        Self::execute(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            &OpcodeCostTable::default(),
            None,
            Some(migration),
            None,
        )
    }

    pub fn resume(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: u64,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::poll(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            &OpcodeCostTable::default(),
        )
    }

    pub fn run_cleanup(
        module: &VerifiedModule,
        continuation: &InterpreterContinuation,
        max_ops: u32,
        max_fuel: u64,
        costs: &OpcodeCostTable,
    ) -> Result<Result<ExecutionCharge, Trap>, InterpreterError> {
        let actions = continuation
            .arena
            .defers_rev()
            .take(max_ops as usize + 1)
            .collect::<Vec<_>>();
        if actions.len() > max_ops as usize {
            return Ok(Err(Trap {
                kind: TrapKind::CleanupBudgetExceeded,
                message: "cleanup operation budget exceeded".into(),
            }));
        }
        let mut total = ExecutionCharge::default();
        for action in actions {
            match action {
                crate::DeferAction::Call {
                    function,
                    args,
                    args_count,
                } => {
                    let limits = FrameLimits::default();
                    let cleanup = Self::start(
                        module,
                        function,
                        &args[..usize::from(args_count)],
                        limits,
                        ContinuationReservation::for_limits(limits),
                    )?;
                    let remaining = max_fuel.saturating_sub(total.fuel_used);
                    let outcome = Self::poll(
                        module,
                        cleanup,
                        FuelState::new(remaining, 0, remaining),
                        costs,
                    )?;
                    match outcome {
                        InterpreterOutcome::Returned { charge, .. } => {
                            total.instructions =
                                total.instructions.saturating_add(charge.instructions);
                            total.fuel_used = total.fuel_used.saturating_add(charge.fuel_used);
                        }
                        InterpreterOutcome::Trapped { trap, .. } => return Ok(Err(trap)),
                        InterpreterOutcome::HostPending { .. } => {
                            return Ok(Err(Trap {
                                kind: TrapKind::CleanupBudgetExceeded,
                                message: "cleanup attempted a host call".into(),
                            }));
                        }
                        InterpreterOutcome::Suspended { .. } => {
                            return Ok(Err(Trap {
                                kind: TrapKind::CleanupBudgetExceeded,
                                message: "cleanup attempted to suspend or exhausted fuel".into(),
                            }));
                        }
                    }
                }
                crate::DeferAction::Trap => {
                    return Ok(Err(Trap {
                        kind: TrapKind::BytecodeTrap,
                        message: "defer trapped".into(),
                    }));
                }
                crate::DeferAction::ReleaseCounter(_) | crate::DeferAction::SetFlag(_) => {}
            }
        }
        Ok(Ok(total))
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        module: &VerifiedModule,
        mut continuation: InterpreterContinuation,
        mut fuel: FuelState,
        costs: &OpcodeCostTable,
        mut host: Option<&mut dyn InterpreterHost>,
        mut migration: Option<&mut dyn InterpreterMigration>,
        mut heap: Option<&mut Heap>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        continuation.suspend_reason = None;
        continuation.cumulative_exhausted = false;
        let mut charge = ExecutionCharge::default();
        let mut pending_cost = std::mem::take(&mut continuation.pending_fuel);
        loop {
            let frame = *continuation.arena.current()?;
            continuation.current_function = frame.function;
            let function = module
                .module()
                .functions
                .get(frame.function as usize)
                .ok_or(InterpreterError::MissingFunction(frame.function))?;
            let instruction = *function
                .code
                .get(frame.pc as usize)
                .ok_or(InterpreterError::FellOffFunction)?;
            let instruction_cost = costs.cost(instruction).saturating_add(
                if let Instruction::HostCall { import, .. } = instruction {
                    u64::from(
                        module
                            .module()
                            .host_imports
                            .get(import as usize)
                            .ok_or(InterpreterError::HostUnavailable)?
                            .fuel_cost,
                    )
                } else {
                    0
                },
            );
            let settlement = pending_cost.saturating_add(instruction_cost);
            let host_resume = frame.pc > 0
                && matches!(
                    function.code.get(frame.pc as usize - 1),
                    Some(Instruction::HostCall { .. })
                );
            if frame.pc == 0 || host_resume || is_safepoint(instruction, frame.pc) {
                if settlement > fuel.slice_remaining
                    || fuel.cumulative_used.saturating_add(settlement) > fuel.cumulative_limit
                {
                    continuation.cumulative_exhausted =
                        fuel.cumulative_used.saturating_add(settlement) > fuel.cumulative_limit;
                    continuation.suspend_reason = Some(SuspendReason::Fuel);
                    continuation.pending_fuel = pending_cost;
                    return Ok(InterpreterOutcome::Suspended {
                        continuation,
                        reason: SuspendReason::Fuel,
                        charge,
                        fuel,
                    });
                }
                fuel.slice_remaining -= settlement;
                fuel.cumulative_used += settlement;
                charge.fuel_used += settlement;
                pending_cost = 0;
            } else {
                pending_cost = settlement;
            }
            charge.instructions = charge.instructions.saturating_add(1);
            match instruction {
                Instruction::LoadI32 { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::I32(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadBool { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Move { dst, source } => {
                    let value = register(&continuation.arena, source)?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Add { dst, lhs, rhs }
                | Instruction::Sub { dst, lhs, rhs }
                | Instruction::Mul { dst, lhs, rhs } => {
                    let RuntimeValue::I32(lhs) = register(&continuation.arena, lhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(rhs) = register(&continuation.arena, rhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = match instruction {
                        Instruction::Add { .. } => lhs.wrapping_add(rhs),
                        Instruction::Sub { .. } => lhs.wrapping_sub(rhs),
                        Instruction::Mul { .. } => lhs.wrapping_mul(rhs),
                        _ => unreachable!(),
                    };
                    set_register(&mut continuation.arena, dst, RuntimeValue::I32(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::CompareEq { dst, lhs, rhs } => {
                    let lhs = register(&continuation.arena, lhs)?;
                    let rhs = register(&continuation.arena, rhs)?;
                    if runtime_value_type(lhs).is_none()
                        || runtime_value_type(lhs) != runtime_value_type(rhs)
                    {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(lhs == rhs))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Jump { target } => {
                    continuation.arena.current_mut()?.pc =
                        checked_target(function.code.len(), target)?;
                }
                Instruction::JumpIfFalse { condition, target } => {
                    let RuntimeValue::Bool(condition) = register(&continuation.arena, condition)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    continuation.arena.current_mut()?.pc = if condition {
                        frame.pc + 1
                    } else {
                        checked_target(function.code.len(), target)?
                    };
                }
                Instruction::Call {
                    function: callee_id,
                    args_base,
                    args_count,
                    dst,
                } => {
                    let callee = module
                        .module()
                        .functions
                        .get(callee_id as usize)
                        .ok_or(InterpreterError::MissingFunction(callee_id))?;
                    if usize::from(args_count) != callee.signature.parameters.len() {
                        return Err(InterpreterError::ArgumentCount);
                    }
                    for (offset, expected) in
                        (0..args_count).zip(callee.signature.parameters.iter().copied())
                    {
                        let argument = args_base
                            .checked_add(offset)
                            .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                        if runtime_value_type(register(&continuation.arena, argument)?)
                            != Some(expected)
                        {
                            return Err(InterpreterError::TypeMismatch);
                        }
                    }
                    let caller_index = continuation.arena.depth() - 1;
                    continuation
                        .arena
                        .push_call(callee_id, callee.registers, Some(dst))?;
                    for offset in 0..args_count {
                        let argument = args_base
                            .checked_add(offset)
                            .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                        let value = continuation.arena.frame_register(caller_index, argument)?;
                        continuation
                            .arena
                            .set_register(usize::from(offset), value)?;
                    }
                    continuation
                        .arena
                        .set_frame_pc(caller_index, frame.pc + 1)?;
                }
                Instruction::HostCall {
                    import,
                    args_base,
                    args_count,
                    dst,
                } => {
                    let metadata = module
                        .module()
                        .host_imports
                        .get(import as usize)
                        .ok_or(InterpreterError::HostUnavailable)?;
                    if args_count > 8 {
                        return Err(InterpreterError::ArgumentCount);
                    }
                    let mut arguments = [RuntimeValue::Unit; 8];
                    for offset in 0..args_count {
                        arguments[usize::from(offset)] = register(
                            &continuation.arena,
                            args_base
                                .checked_add(offset)
                                .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?,
                        )?;
                    }
                    let outcome = host
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .call(import, &arguments[..usize::from(args_count)])
                        .map_err(InterpreterError::Host)?;
                    match outcome {
                        InterpreterHostOutcome::Immediate(value) => {
                            if metadata.result != runtime_value_type(value) {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            if metadata.result.is_some() {
                                set_register(&mut continuation.arena, dst, value)?;
                            }
                            increment_pc(&mut continuation.arena)?;
                        }
                        InterpreterHostOutcome::Pending(request) => {
                            if metadata.mode != HostCallMode::Async {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            increment_pc(&mut continuation.arena)?;
                            continuation.suspend_reason = Some(SuspendReason::HostRequest);
                            continuation.pending_fuel = pending_cost;
                            return Ok(InterpreterOutcome::HostPending {
                                continuation,
                                request,
                                destination: dst,
                                expected_type: metadata.result,
                                async_result: metadata.async_result,
                                charge,
                                fuel,
                            });
                        }
                    }
                }
                Instruction::StateOldGet { stable_id, ty, dst } => {
                    let value = migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .old_get(stable_id, ty)
                        .map_err(InterpreterError::Migration)?;
                    if runtime_value_type(value) != Some(ty) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateOldFieldGet {
                    object,
                    field_id,
                    ty,
                    dst,
                } => {
                    let object = register(&continuation.arena, object)?;
                    let value = migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .old_field_get(object, field_id, ty)
                        .map_err(InterpreterError::Migration)?;
                    if runtime_value_type(value) != Some(ty) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateNewCreate {
                    stable_id,
                    type_id,
                    dst,
                } => {
                    let value = migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .new_create(stable_id, type_id)
                        .map_err(InterpreterError::Migration)?;
                    if runtime_value_type(value) != Some(ValueType::Named(type_id)) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateNewSet {
                    object,
                    field_id,
                    source,
                } => {
                    let object = register(&continuation.arena, object)?;
                    let value = register(&continuation.arena, source)?;
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .new_set(object, field_id, value)
                        .map_err(InterpreterError::Migration)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateReplace { old_id, target } => {
                    let target = register(&continuation.arena, target)?;
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .replace(old_id, target)
                        .map_err(InterpreterError::Migration)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StatePreserve { stable_id } => {
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .preserve(stable_id)
                        .map_err(InterpreterError::Migration)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateDelete { stable_id } => {
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .delete(stable_id)
                        .map_err(InterpreterError::Migration)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumNew {
                    type_id,
                    variant,
                    payload,
                    dst,
                } => {
                    let variant = module
                        .module()
                        .enum_types
                        .iter()
                        .find(|enum_type| enum_type.type_id == type_id)
                        .and_then(|enum_type| {
                            enum_type
                                .variants
                                .iter()
                                .find(|candidate| candidate.stable_id == variant)
                        })
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let payload = payload
                        .map(|payload| register(&continuation.arena, payload))
                        .transpose()?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?;
                    let value =
                        heap.allocate_enum(type_id, variant.stable_id, variant.tag, payload)?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumTag { source, dst } => {
                    let value = register(&continuation.arena, source)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?;
                    let tag = i32::try_from(heap.enum_tag(value)?)
                        .map_err(|_| InterpreterError::TypeMismatch)?;
                    set_register(&mut continuation.arena, dst, RuntimeValue::I32(tag))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumPayload {
                    source,
                    variant,
                    dst,
                } => {
                    let value = register(&continuation.arena, source)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?;
                    let payload = heap.enum_payload(value, variant)?;
                    set_register(&mut continuation.arena, dst, payload)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateFinish => {
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .finish_staging()
                        .map_err(InterpreterError::Migration)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Return { source } => {
                    settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                    if start_next_defer(module, &mut continuation.arena)? {
                        pending_cost = 0;
                        continue;
                    }
                    let result = register(&continuation.arena, source)?;
                    let completed = continuation.arena.pop()?;
                    let returning_cleanup =
                        completed.return_target.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup || function.effect == FunctionEffect::Cleanup {
                        pending_cost = 0;
                        continue;
                    } else if continuation.arena.depth() > 0 {
                        set_register(
                            &mut continuation.arena,
                            completed
                                .return_target
                                .ok_or(InterpreterError::TypeMismatch)?,
                            result,
                        )?;
                    } else {
                        return Ok(InterpreterOutcome::Returned {
                            value: Some(result),
                            charge,
                            fuel,
                        });
                    }
                    pending_cost = 0;
                }
                Instruction::ReturnVoid => {
                    settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                    if start_next_defer(module, &mut continuation.arena)? {
                        pending_cost = 0;
                        continue;
                    }
                    let completed = continuation.arena.pop()?;
                    let returning_cleanup =
                        completed.return_target.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup || function.effect == FunctionEffect::Cleanup {
                        pending_cost = 0;
                        continue;
                    } else if continuation.arena.depth() == 0 {
                        return Ok(InterpreterOutcome::Returned {
                            value: None,
                            charge,
                            fuel,
                        });
                    }
                    pending_cost = 0;
                }
                Instruction::Safepoint => increment_pc(&mut continuation.arena)?,
                Instruction::Yield => {
                    increment_pc(&mut continuation.arena)?;
                    continuation.suspend_reason = Some(SuspendReason::ExplicitYield);
                    continuation.pending_fuel = 0;
                    return Ok(InterpreterOutcome::Suspended {
                        continuation,
                        reason: SuspendReason::ExplicitYield,
                        charge,
                        fuel,
                    });
                }
                Instruction::Trap => {
                    settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                    return Ok(InterpreterOutcome::Trapped {
                        trap: Trap {
                            kind: TrapKind::BytecodeTrap,
                            message: "bytecode trap".into(),
                        },
                        charge,
                        fuel,
                    });
                }
                Instruction::DeferPush {
                    function,
                    args_base,
                    args_count,
                } => {
                    if args_count > 8 {
                        return Err(InterpreterError::ContinuationLimit(FrameError::DeferLimit));
                    }
                    let mut arguments = [RuntimeValue::Unit; 8];
                    for offset in 0..args_count {
                        let source = args_base
                            .checked_add(offset)
                            .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                        arguments[usize::from(offset)] = register(&continuation.arena, source)?;
                    }
                    continuation
                        .arena
                        .push_defer_call(function, &arguments[..usize::from(args_count)])?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::DeferPop => {
                    continuation.arena.pop_defer_for_current_frame()?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::CleanupReturn => {
                    settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                    continuation.arena.pop()?;
                    pending_cost = 0;
                }
            }
        }
    }
}

fn start_next_defer(
    module: &VerifiedModule,
    arena: &mut FrameArena,
) -> Result<bool, InterpreterError> {
    let Some(action) = arena.pop_defer_for_current_frame()? else {
        return Ok(false);
    };
    match action {
        crate::DeferAction::Call {
            function,
            args,
            args_count,
        } => {
            let cleanup = module
                .module()
                .functions
                .get(function as usize)
                .ok_or(InterpreterError::MissingFunction(function))?;
            arena.push_call(function, cleanup.registers, None)?;
            for (index, value) in args[..usize::from(args_count)].iter().copied().enumerate() {
                arena.set_register(index, value)?;
            }
            Ok(true)
        }
        crate::DeferAction::Trap => Err(InterpreterError::TypeMismatch),
        crate::DeferAction::ReleaseCounter(_) | crate::DeferAction::SetFlag(_) => Ok(false),
    }
}

fn settle_terminal_cost(
    fuel: &mut FuelState,
    charge: &mut ExecutionCharge,
    pending: u64,
) -> Result<(), InterpreterError> {
    if pending > fuel.slice_remaining
        || fuel.cumulative_used.saturating_add(pending) > fuel.cumulative_limit
    {
        return Err(InterpreterError::ContinuationLimit(
            FrameError::FrameByteLimit,
        ));
    }
    fuel.slice_remaining -= pending;
    fuel.cumulative_used += pending;
    charge.fuel_used += pending;
    Ok(())
}

fn validate_arguments(
    arguments: &[RuntimeValue],
    parameters: &[ValueType],
) -> Result<(), InterpreterError> {
    if arguments.len() != parameters.len() {
        return Err(InterpreterError::ArgumentCount);
    }
    for (argument, expected) in arguments.iter().copied().zip(parameters.iter().copied()) {
        if runtime_value_type(argument) != Some(expected) {
            return Err(InterpreterError::TypeMismatch);
        }
    }
    Ok(())
}

fn register(arena: &FrameArena, register: u16) -> Result<RuntimeValue, InterpreterError> {
    arena
        .register(usize::from(register))
        .map_err(|_| InterpreterError::RegisterOutOfRange(register))
}

fn set_register(
    arena: &mut FrameArena,
    register: u16,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    arena
        .set_register(usize::from(register), value)
        .map_err(|_| InterpreterError::RegisterOutOfRange(register))
}

fn increment_pc(arena: &mut FrameArena) -> Result<(), InterpreterError> {
    let frame = arena.current_mut()?;
    frame.pc = frame
        .pc
        .checked_add(1)
        .ok_or(InterpreterError::FellOffFunction)?;
    Ok(())
}

fn runtime_value_type(value: RuntimeValue) -> Option<ValueType> {
    match value {
        RuntimeValue::I32(_) => Some(ValueType::I32),
        RuntimeValue::Bool(_) => Some(ValueType::Bool),
        RuntimeValue::Ref(_) => Some(ValueType::Ref),
        RuntimeValue::NamedRef { type_id, .. } | RuntimeValue::Opaque { type_id, .. } => {
            Some(ValueType::Named(type_id))
        }
        RuntimeValue::HostRequest(_) => Some(ValueType::Named(nexa_core::StableId::from_name(
            "HostRequest",
        ))),
        RuntimeValue::ResourceToken(_) => Some(ValueType::Named(nexa_core::StableId::from_name(
            "ResourceToken",
        ))),
        RuntimeValue::Snapshot(_) => {
            Some(ValueType::Named(nexa_core::StableId::from_name("Snapshot")))
        }
        RuntimeValue::Unit => None,
    }
}

fn checked_target(code_len: usize, target: u32) -> Result<u32, InterpreterError> {
    if (target as usize) < code_len {
        Ok(target)
    } else {
        Err(InterpreterError::JumpOutOfRange(target))
    }
}

fn is_safepoint(instruction: Instruction, pc: u32) -> bool {
    match instruction {
        Instruction::Safepoint
        | Instruction::Yield
        | Instruction::Call { .. }
        | Instruction::HostCall { .. }
        | Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn
        | Instruction::Trap => true,
        Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. } => target <= pc,
        _ => false,
    }
}

fn opcode_index(instruction: Instruction) -> usize {
    match instruction {
        Instruction::LoadI32 { .. } => 0,
        Instruction::LoadBool { .. } => 1,
        Instruction::Move { .. } => 2,
        Instruction::Add { .. } => 3,
        Instruction::Sub { .. } => 4,
        Instruction::Mul { .. } => 5,
        Instruction::CompareEq { .. } => 6,
        Instruction::Jump { .. } => 7,
        Instruction::JumpIfFalse { .. } => 8,
        Instruction::Call { .. } => 9,
        Instruction::Return { .. } => 10,
        Instruction::ReturnVoid => 11,
        Instruction::Safepoint => 12,
        Instruction::Yield => 13,
        Instruction::Trap => 14,
        Instruction::DeferPush { .. } => 15,
        Instruction::DeferPop => 16,
        Instruction::CleanupReturn => 17,
        Instruction::HostCall { .. } => 18,
        Instruction::StateOldGet { .. } => 19,
        Instruction::StateNewCreate { .. } => 20,
        Instruction::StateNewSet { .. } => 21,
        Instruction::StateReplace { .. } => 22,
        Instruction::StateDelete { .. } => 23,
        Instruction::EnumNew { .. } => 24,
        Instruction::EnumTag { .. } => 25,
        Instruction::EnumPayload { .. } => 26,
        Instruction::StatePreserve { .. } => 27,
        Instruction::StateFinish => 28,
        Instruction::StateOldFieldGet { .. } => 29,
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{CheckedInterpreter, InterpreterOutcome, SuspendReason};
    use crate::{GcRoots, Heap, Object, RuntimeValue};

    #[test]
    fn frame_arena_continuation_yields_and_resumes_without_repeating_add() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32, ValueType::I32],
                result: Some(ValueType::I32),
            },
            3,
        );
        function.effect(FunctionEffect::Task);
        function
            .emit(Instruction::Add {
                dst: 2,
                lhs: 0,
                rhs: 1,
            })
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 2 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let outcome = CheckedInterpreter::run(
            &module,
            0,
            &[RuntimeValue::I32(2), RuntimeValue::I32(5)],
            10,
        )
        .unwrap();
        let InterpreterOutcome::Suspended {
            continuation,
            reason: SuspendReason::ExplicitYield,
            ..
        } = outcome
        else {
            panic!("expected explicit yield");
        };
        let outcome = CheckedInterpreter::resume(&module, continuation, 10).unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(7)),
                ..
            }
        ));
    }

    #[test]
    fn metadata_roots_keep_suspended_reference_alive() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            1,
        );
        function.effect(FunctionEffect::Task);
        function
            .set_root(0)
            .unwrap()
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new(1);
        let reference = heap.allocate(Object::String("root".into())).unwrap();
        let outcome =
            CheckedInterpreter::run(&module, 0, &[RuntimeValue::Ref(reference)], 10).unwrap();
        let InterpreterOutcome::Suspended { continuation, .. } = outcome else {
            panic!("expected yield");
        };
        let roots = GcRoots {
            suspended_tasks: continuation.checked_gc_roots(&module).unwrap(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 1);
        assert!(matches!(
            CheckedInterpreter::resume(&module, continuation, 10).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Ref(value)),
                ..
            } if value == reference
        ));
        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 1);
    }

    #[test]
    fn enum_new_tag_and_payload_execute_through_the_gc_heap() {
        let enum_type = nexa_bytecode::option_type(ValueType::I32);
        let some = StableId::from_parts(&["Option", "::Some"]);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            3,
        );
        function
            .emit(Instruction::EnumNew {
                type_id: enum_type.type_id,
                variant: some,
                payload: Some(0),
                dst: 1,
            })
            .emit(Instruction::EnumTag { source: 1, dst: 2 })
            .emit(Instruction::EnumPayload {
                source: 1,
                variant: some,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut function = function.finish().unwrap();
        function.root_bitmap[1] = true;
        function.root_maps = vec![
            nexa_bytecode::RootMap {
                pc: 0,
                bitmap: vec![false, false, false],
            },
            nexa_bytecode::RootMap {
                pc: 3,
                bitmap: vec![false, true, false],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.enum_type(enum_type).function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new(8);
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[RuntimeValue::I32(41)], 32, &mut heap,)
                .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(41)),
                ..
            }
        ));
    }
}
