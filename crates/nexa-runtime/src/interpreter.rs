use std::fmt::{self, Write as _};

use nexa_bytecode::{
    AsyncResultType, HostCallMode, Instruction, SCALAR_TO_STRING_BUFFER_BYTES,
    SCALAR_TO_STRING_FUEL_PASSES, SCALAR_TO_STRING_MAX_BYTES,
    STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS, STANDARD_STRING_FUEL_BLOCK_BYTES, StandardIntrinsic,
    StandardIntrinsicFuelModel, ValueType,
};
use nexa_core::{
    OPCODE_COST_TABLE_VERSION, RawHandle, SourceSpan, StableId,
    deterministic_math::{
        canonicalize_nan_f32, canonicalize_nan_f64, ceil_f32 as deterministic_ceil_f32,
        ceil_f64 as deterministic_ceil_f64, cos_f32 as deterministic_cos_f32,
        cos_f64 as deterministic_cos_f64, floor_f32 as deterministic_floor_f32,
        floor_f64 as deterministic_floor_f64, rem_f32 as deterministic_rem_f32,
        rem_f64 as deterministic_rem_f64, round_f32 as deterministic_round_f32,
        round_f64 as deterministic_round_f64, sin_f32 as deterministic_sin_f32,
        sin_f64 as deterministic_sin_f64, sqrt_f32 as deterministic_sqrt_f32,
        sqrt_f64 as deterministic_sqrt_f64,
    },
};
use nexa_verifier::VerifiedModule;

use crate::{
    ContinuationReservation, FrameArena, FrameError, FrameLimits, GcRef, Heap, HeapError,
    MapSetOutcome, RuntimeMessage, RuntimeValue,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuspendReason {
    Fuel,
    ExplicitYield,
    HostRequest,
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
    cleanup_mode: bool,
    host_call_boundary: Option<HostCallBoundary>,
}

impl InterpreterContinuation {
    pub fn new(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
    ) -> Result<Self, InterpreterError> {
        if limits.max_call_depth as usize > MAX_SCRIPT_CALL_STACK_DEPTH {
            return Err(InterpreterError::ContinuationLimit(
                FrameError::ReservationExceedsLimit,
            ));
        }
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
            cleanup_mode: false,
            host_call_boundary: None,
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
        increment_pc(&mut self.arena)?;
        self.host_call_boundary = None;
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
                            .map(|root_map| root_map.bitmap.clone())
                    })
            })
            .map_err(Into::into)
    }

    pub fn checked_gc_roots(
        &self,
        module: &VerifiedModule,
    ) -> Result<Vec<GcRef>, InterpreterError> {
        self.gc_roots(module)
    }
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
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
    pub message: crate::RuntimeMessage,
    pub module: Option<RawHandle>,
    pub epoch: Option<u64>,
    pub function: u32,
    pub pc: u32,
    pub source_span: Option<SourceSpan>,
    pub task: Option<RawHandle>,
    pub script_call_stack: ScriptCallStack,
    pub host_call_boundary: Option<HostCallBoundary>,
}

pub const MAX_SCRIPT_CALL_STACK_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScriptFrame {
    pub function: u32,
    /// Current/resume PC stored by the continuation frame.
    pub pc: u32,
    /// Exact `Call` instruction PC used for a caller frame in a rendered
    /// callee-to-caller stack.
    pub call_site_pc: Option<u32>,
    pub source_span: Option<SourceSpan>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ScriptCallStack {
    frames: [ScriptFrame; MAX_SCRIPT_CALL_STACK_DEPTH],
    len: u16,
}

impl ScriptCallStack {
    #[must_use]
    pub const fn as_slice(&self) -> &[ScriptFrame] {
        self.frames.split_at(self.len as usize).0
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn from_continuation(
        module: &VerifiedModule,
        continuation: &InterpreterContinuation,
    ) -> Self {
        let mut stack = Self::default();
        let depth = continuation.arena.depth();
        for (index, frame_index) in (0..depth).rev().enumerate() {
            let frame = *continuation
                .arena
                .frame(frame_index)
                .expect("frame index is bounded by arena depth");
            let call_site_pc = continuation
                .arena
                .frame(frame_index + 1)
                .and_then(|callee| callee.call_site_pc);
            let location_pc = call_site_pc.unwrap_or(frame.pc);
            stack.frames[index] = ScriptFrame {
                function: frame.function,
                pc: frame.pc,
                call_site_pc,
                source_span: module.module().source_span(frame.function, location_pc),
            };
            stack.len += 1;
        }
        stack
    }
}

impl Default for ScriptCallStack {
    fn default() -> Self {
        Self {
            frames: [ScriptFrame::default(); MAX_SCRIPT_CALL_STACK_DEPTH],
            len: 0,
        }
    }
}

impl fmt::Debug for ScriptCallStack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ScriptCallStack")
            .field(&self.as_slice())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCallBoundary {
    pub import: u32,
    pub function: u32,
    pub pc: u32,
    pub source_span: Option<SourceSpan>,
}

impl Trap {
    /// Stable facade diagnostic code for a runtime trap.
    #[must_use]
    pub const fn diagnostic_code(&self) -> &'static str {
        if let crate::RuntimeMessage::Code { code, .. } = self.message {
            return code.as_str();
        }
        match self.kind {
            TrapKind::Host
            | TrapKind::BytecodeTrap
            | TrapKind::DivideByZero
            | TrapKind::StringIndexOutOfBounds
            | TrapKind::ArrayIndexOutOfBounds
            | TrapKind::BufferIndexOutOfBounds
            | TrapKind::StandardLibrary
            | TrapKind::CleanupBudgetExceeded => "NX5001",
        }
    }

    /// Production module identity attached by [`crate::RealmRuntime`].
    #[must_use]
    pub const fn module_id(&self) -> Option<u32> {
        match self.module {
            Some(module) => Some(module.index),
            None => None,
        }
    }

    pub(crate) fn from_continuation(
        module: &VerifiedModule,
        continuation: &InterpreterContinuation,
        kind: TrapKind,
        message: impl Into<crate::RuntimeMessage>,
    ) -> Self {
        let script_call_stack = ScriptCallStack::from_continuation(module, continuation);
        let current = script_call_stack
            .as_slice()
            .first()
            .copied()
            .unwrap_or_default();
        Self {
            kind,
            message: message.into(),
            module: None,
            epoch: None,
            function: current.function,
            pc: current.call_site_pc.unwrap_or(current.pc),
            source_span: current.source_span,
            task: None,
            script_call_stack,
            host_call_boundary: continuation.host_call_boundary,
        }
    }

    pub(crate) fn attach_runtime_context(
        &mut self,
        module: RawHandle,
        epoch: u64,
        task: RawHandle,
    ) {
        self.module = Some(module);
        self.epoch = Some(epoch);
        self.task = Some(task);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    BytecodeTrap,
    DivideByZero,
    StringIndexOutOfBounds,
    ArrayIndexOutOfBounds,
    BufferIndexOutOfBounds,
    StandardLibrary,
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
    HeapUnavailable,
    StringLengthOverflow,
    FuelCostOverflow,
    OpcodeCostTableVersion { expected: u32, actual: u32 },
    Host(crate::HostTrap),
    Migration(crate::RuntimeMessage),
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
            FrameError::RootMapMismatch => Self::RootMapMismatch,
            error => Self::ContinuationLimit(error),
        }
    }
}

impl From<HeapError> for InterpreterError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

/// Stack-backed formatting buffer for deterministic scalar conversion.
///
/// The longest supported scalar rendering is much smaller than 64 bytes. A
/// fixed buffer keeps formatting itself allocation-free; the resulting text is
/// copied into the bounded VM heap only after the instruction reaches its GC
/// safepoint.
struct ScalarText {
    bytes: [u8; SCALAR_TO_STRING_BUFFER_BYTES],
    len: usize,
}

impl ScalarText {
    const fn new() -> Self {
        Self {
            bytes: [0; SCALAR_TO_STRING_BUFFER_BYTES],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len])
            .expect("fmt::Write only appends valid UTF-8 strings")
    }
}

impl fmt::Write for ScalarText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(fmt::Error)?;
        let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
        target.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpcodeCostTable {
    pub version: u32,
    costs: [u16; 107],
}

impl Default for OpcodeCostTable {
    fn default() -> Self {
        Self {
            version: OPCODE_COST_TABLE_VERSION,
            costs: DEFAULT_OPCODE_COSTS,
        }
    }
}

impl OpcodeCostTable {
    fn validate_version(&self) -> Result<(), InterpreterError> {
        if self.version != OPCODE_COST_TABLE_VERSION {
            return Err(InterpreterError::OpcodeCostTableVersion {
                expected: OPCODE_COST_TABLE_VERSION,
                actual: self.version,
            });
        }
        Ok(())
    }

    fn cost(&self, instruction: Instruction) -> u64 {
        if let Instruction::StandardIntrinsic { intrinsic, .. } = instruction {
            u64::from(intrinsic.base_fuel_cost())
        } else {
            u64::from(self.costs[opcode_index(instruction)])
        }
    }
}

/// Type identity for allocating instructions profiled as WP14 sites; `None`
/// marks non-allocating instructions.
const fn allocation_type_identity(instruction: Instruction) -> Option<u64> {
    match instruction {
        Instruction::StructNew { type_id, .. }
        | Instruction::ClassNew { type_id, .. }
        | Instruction::EnumNew { type_id, .. }
        | Instruction::ArrayNew { type_id, .. }
        | Instruction::MapNew { type_id, .. } => Some(type_id.0),
        Instruction::StructWith { .. }
        | Instruction::LoadString { .. }
        | Instruction::StringConcat { .. }
        | Instruction::StringToString { .. }
        | Instruction::I32ToString { .. }
        | Instruction::I64ToString { .. }
        | Instruction::F32ToString { .. }
        | Instruction::F64ToString { .. }
        | Instruction::BoolToString { .. }
        | Instruction::RuneToString { .. }
        | Instruction::BufferSlice { .. } => Some(0),
        _ => None,
    }
}

pub struct CheckedInterpreter;

pub trait InterpreterHost {
    fn call(
        &mut self,
        import: u32,
        arguments: &[RuntimeValue],
        heap: Option<&mut Heap>,
    ) -> Result<InterpreterHostOutcome, crate::HostTrap>;
}

pub trait InterpreterMigration {
    fn observe_fuel_used(&mut self, _fuel_used: u64) {}
    fn observe_call_depth(&mut self, _depth: usize) {}
    fn old_get(
        &mut self,
        stable_id: StableId,
        expected: ValueType,
    ) -> Result<RuntimeValue, crate::RuntimeMessage>;
    fn old_field_get(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        expected: ValueType,
    ) -> Result<RuntimeValue, crate::RuntimeMessage>;
    fn new_create(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, crate::RuntimeMessage>;
    fn new_set(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        value: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage>;
    fn preserve(&mut self, stable_id: StableId) -> Result<(), crate::RuntimeMessage>;
    fn replace(
        &mut self,
        old_id: StableId,
        target: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage>;
    fn delete(&mut self, stable_id: StableId) -> Result<(), crate::RuntimeMessage>;
    fn finish_staging(&mut self) -> Result<(), crate::RuntimeMessage>;
}

pub trait InterpreterState {
    fn current_object(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, crate::RuntimeMessage>;

    fn object_field(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        expected: ValueType,
    ) -> Result<RuntimeValue, crate::RuntimeMessage>;

    fn set_object_field(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        expected: ValueType,
        value: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage>;

    fn resolve(
        &mut self,
        handle: crate::StateHandle,
        target: ValueType,
    ) -> Result<RuntimeValue, crate::StateHandleError>;

    fn is_alive(&mut self, handle: crate::StateHandle) -> bool;
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
        Self::execute(module, continuation, fuel, costs, None, None, None, None)
    }

    pub fn poll_with_host(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            None,
            None,
        )
    }

    pub fn poll_with_heap(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            None,
            Some(heap),
        )
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
            None,
            Some(heap),
        )
    }

    pub(crate) fn poll_with_heap_and_state(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        state: &mut dyn InterpreterState,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            Some(state),
            Some(heap),
        )
    }

    pub(crate) fn poll_with_host_heap_and_state(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
        state: &mut dyn InterpreterState,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            Some(state),
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
        limits: FrameLimits,
        migration: &mut dyn InterpreterMigration,
    ) -> Result<InterpreterOutcome, InterpreterError> {
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
            None,
        )
    }

    pub fn run_migration_with_heap(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
        limits: FrameLimits,
        migration: &mut dyn InterpreterMigration,
        heap: &mut Heap,
    ) -> Result<InterpreterOutcome, InterpreterError> {
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
            Some(heap),
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
        mut continuation: InterpreterContinuation,
        max_ops: u32,
        max_fuel: u64,
        costs: &OpcodeCostTable,
    ) -> Result<Result<ExecutionCharge, Trap>, InterpreterError> {
        costs.validate_version()?;
        let exceeds_budget = continuation.arena.defer_len() > max_ops as usize;
        if exceeds_budget {
            return Ok(Err(Trap::from_continuation(
                module,
                &continuation,
                TrapKind::CleanupBudgetExceeded,
                "cleanup operation budget exceeded",
            )));
        }
        continuation.cleanup_mode = true;
        let mut initial_fuel = 0_u64;
        loop {
            let Some(action) = continuation.arena.peek_defer_for_current_frame()? else {
                return Ok(Ok(ExecutionCharge {
                    instructions: 0,
                    fuel_used: initial_fuel,
                }));
            };
            let attempt = defer_action_attempt_fuel(module.module(), &action)?;
            let cumulative_after = initial_fuel
                .checked_add(attempt)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            if cumulative_after > max_fuel {
                return Ok(Err(Trap::from_continuation(
                    module,
                    &continuation,
                    TrapKind::CleanupBudgetExceeded,
                    "cleanup attempted to suspend or exhausted fuel",
                )));
            }
            initial_fuel = cumulative_after;
            let starts_call = matches!(action, crate::DeferAction::Call { .. });
            if !start_next_defer(module, &mut continuation.arena)? {
                return Ok(Ok(ExecutionCharge {
                    instructions: 0,
                    fuel_used: initial_fuel,
                }));
            }
            if starts_call {
                break;
            }
        }
        match Self::poll(
            module,
            continuation,
            FuelState::new(max_fuel - initial_fuel, initial_fuel, max_fuel),
            costs,
        )? {
            InterpreterOutcome::Returned { mut charge, .. } => {
                charge.fuel_used = charge
                    .fuel_used
                    .checked_add(initial_fuel)
                    .ok_or(InterpreterError::FuelCostOverflow)?;
                Ok(Ok(charge))
            }
            InterpreterOutcome::Trapped { trap, .. } => Ok(Err(trap)),
            InterpreterOutcome::HostPending { continuation, .. } => {
                Ok(Err(Trap::from_continuation(
                    module,
                    &continuation,
                    TrapKind::CleanupBudgetExceeded,
                    "cleanup attempted a host call",
                )))
            }
            InterpreterOutcome::Suspended { continuation, .. } => Ok(Err(Trap::from_continuation(
                module,
                &continuation,
                TrapKind::CleanupBudgetExceeded,
                "cleanup attempted to suspend or exhausted fuel",
            ))),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn execute(
        module: &VerifiedModule,
        mut continuation: InterpreterContinuation,
        mut fuel: FuelState,
        costs: &OpcodeCostTable,
        mut host: Option<&mut dyn InterpreterHost>,
        mut migration: Option<&mut dyn InterpreterMigration>,
        mut state_registry: Option<&mut dyn InterpreterState>,
        mut heap: Option<&mut Heap>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        costs.validate_version()?;
        continuation.suspend_reason = None;
        continuation.cumulative_exhausted = false;
        let mut charge = ExecutionCharge::default();
        let mut pending_cost = std::mem::take(&mut continuation.pending_fuel);
        macro_rules! array_operation {
            ($operation:expr) => {
                match $operation {
                    Ok(value) => value,
                    Err(HeapError::IndexOutOfBounds { .. }) => {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::ArrayIndexOutOfBounds,
                                "array index out of bounds",
                            ),
                            charge,
                            fuel,
                        });
                    }
                    Err(error) => return Err(InterpreterError::Heap(error)),
                }
            };
        }
        macro_rules! array_index {
            ($value:expr) => {
                match usize::try_from($value) {
                    Ok(index) => index,
                    Err(_) => {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::ArrayIndexOutOfBounds,
                                "array index out of bounds",
                            ),
                            charge,
                            fuel,
                        });
                    }
                }
            };
        }
        macro_rules! buffer_operation {
            ($operation:expr) => {
                match $operation {
                    Ok(value) => value,
                    Err(HeapError::IndexOutOfBounds { .. }) => {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::BufferIndexOutOfBounds,
                                "buffer index out of bounds",
                            ),
                            charge,
                            fuel,
                        });
                    }
                    Err(error) => return Err(InterpreterError::Heap(error)),
                }
            };
        }
        macro_rules! buffer_index {
            ($value:expr) => {
                match usize::try_from($value) {
                    Ok(index) => index,
                    Err(_) => {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::BufferIndexOutOfBounds,
                                "buffer index out of bounds",
                            ),
                            charge,
                            fuel,
                        });
                    }
                }
            };
        }
        macro_rules! state_operation {
            ($operation:expr) => {
                match $operation {
                    Ok(value) => value,
                    Err(message) => {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::BytecodeTrap,
                                message,
                            ),
                            charge,
                            fuel,
                        });
                    }
                }
            };
        }
        if let Some(migration) = migration.as_deref_mut() {
            migration.observe_call_depth(continuation.arena.depth());
        }
        // WP15/WP16: the enabled flag is read once per poll; the disabled
        // hot path costs one predictable branch per instruction.
        let profiling = crate::profiler::enabled();
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
            let instruction_cost = instruction_attempt_fuel(
                module.module(),
                module.nominal_index_shape(),
                instruction,
                &continuation.arena,
                heap.as_deref(),
                costs,
            )?
            .checked_add(if let Instruction::HostCall { import, .. } = instruction {
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
            })
            .ok_or(InterpreterError::FuelCostOverflow)?;
            let settlement = pending_cost
                .checked_add(instruction_cost)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let host_resume = frame.pc > 0
                && matches!(
                    function.code.get(frame.pc as usize - 1),
                    Some(Instruction::HostCall { .. })
                );
            if frame.pc == 0 || host_resume || is_safepoint(instruction, frame.pc) {
                let cumulative_after = fuel
                    .cumulative_used
                    .checked_add(settlement)
                    .ok_or(InterpreterError::FuelCostOverflow)?;
                if settlement > fuel.slice_remaining || cumulative_after > fuel.cumulative_limit {
                    continuation.cumulative_exhausted = cumulative_after > fuel.cumulative_limit;
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
                fuel.cumulative_used = cumulative_after;
                charge.fuel_used = charge
                    .fuel_used
                    .checked_add(settlement)
                    .ok_or(InterpreterError::FuelCostOverflow)?;
                if let Some(migration) = migration.as_deref_mut() {
                    migration.observe_fuel_used(charge.fuel_used);
                }
                pending_cost = 0;
            } else {
                pending_cost = settlement;
            }
            charge.instructions = charge.instructions.saturating_add(1);
            if profiling {
                crate::profiler::record_instruction(
                    opcode_index(instruction),
                    frame.function,
                    allocation_type_identity(instruction).map(|type_id| (frame.pc, type_id)),
                    matches!(instruction, Instruction::HostCall { .. }),
                );
            }
            match instruction {
                Instruction::LoadI32 { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::I32(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadBool { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadI64 { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::I64(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadF32 { dst, bits } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::F32(bits))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadF64 { dst, bits } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::F64(bits))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadRune { dst, value } => {
                    set_register(&mut continuation.arena, dst, RuntimeValue::Rune(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::LoadString { dst, string } => {
                    let value = module
                        .module()
                        .strings
                        .get(string as usize)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let reference = heap.allocate_string(value)?;
                    let hash = heap.string_hash(reference)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::String { reference, hash },
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Move { dst, source } => {
                    let value = register(&continuation.arena, source)?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::Add { dst, lhs, rhs }
                | Instruction::Sub { dst, lhs, rhs }
                | Instruction::Mul { dst, lhs, rhs }
                | Instruction::Div { dst, lhs, rhs }
                | Instruction::RemI32 { dst, lhs, rhs } => {
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
                        Instruction::Div { .. } if rhs != 0 => lhs.wrapping_div(rhs),
                        Instruction::RemI32 { .. } if rhs != 0 => lhs.wrapping_rem(rhs),
                        Instruction::Div { .. } | Instruction::RemI32 { .. } => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            return Ok(InterpreterOutcome::Trapped {
                                trap: Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::DivideByZero,
                                    "integer division or remainder by zero",
                                ),
                                charge,
                                fuel,
                            });
                        }
                        _ => unreachable!(),
                    };
                    set_register(&mut continuation.arena, dst, RuntimeValue::I32(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::AddI64 { dst, lhs, rhs }
                | Instruction::SubI64 { dst, lhs, rhs }
                | Instruction::MulI64 { dst, lhs, rhs }
                | Instruction::DivI64 { dst, lhs, rhs }
                | Instruction::RemI64 { dst, lhs, rhs } => {
                    let RuntimeValue::I64(lhs) = register(&continuation.arena, lhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I64(rhs) = register(&continuation.arena, rhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = match instruction {
                        Instruction::AddI64 { .. } => lhs.wrapping_add(rhs),
                        Instruction::SubI64 { .. } => lhs.wrapping_sub(rhs),
                        Instruction::MulI64 { .. } => lhs.wrapping_mul(rhs),
                        Instruction::DivI64 { .. } if rhs != 0 => lhs.wrapping_div(rhs),
                        Instruction::RemI64 { .. } if rhs != 0 => lhs.wrapping_rem(rhs),
                        Instruction::DivI64 { .. } | Instruction::RemI64 { .. } => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            return Ok(InterpreterOutcome::Trapped {
                                trap: Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::DivideByZero,
                                    "integer division or remainder by zero",
                                ),
                                charge,
                                fuel,
                            });
                        }
                        _ => unreachable!(),
                    };
                    set_register(&mut continuation.arena, dst, RuntimeValue::I64(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::AddF32 { dst, lhs, rhs }
                | Instruction::SubF32 { dst, lhs, rhs }
                | Instruction::MulF32 { dst, lhs, rhs }
                | Instruction::DivF32 { dst, lhs, rhs }
                | Instruction::RemF32 { dst, lhs, rhs } => {
                    let RuntimeValue::F32(lhs) = register(&continuation.arena, lhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::F32(rhs) = register(&continuation.arena, rhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let lhs = f32::from_bits(lhs);
                    let rhs = f32::from_bits(rhs);
                    let value = match instruction {
                        Instruction::AddF32 { .. } => canonicalize_nan_f32(lhs + rhs),
                        Instruction::SubF32 { .. } => canonicalize_nan_f32(lhs - rhs),
                        Instruction::MulF32 { .. } => canonicalize_nan_f32(lhs * rhs),
                        Instruction::DivF32 { .. } => canonicalize_nan_f32(lhs / rhs),
                        Instruction::RemF32 { .. } => deterministic_rem_f32(lhs, rhs),
                        _ => unreachable!(),
                    };
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::F32(value.to_bits()),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::AddF64 { dst, lhs, rhs }
                | Instruction::SubF64 { dst, lhs, rhs }
                | Instruction::MulF64 { dst, lhs, rhs }
                | Instruction::DivF64 { dst, lhs, rhs }
                | Instruction::RemF64 { dst, lhs, rhs } => {
                    let RuntimeValue::F64(lhs) = register(&continuation.arena, lhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::F64(rhs) = register(&continuation.arena, rhs)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let lhs = f64::from_bits(lhs);
                    let rhs = f64::from_bits(rhs);
                    let value = match instruction {
                        Instruction::AddF64 { .. } => canonicalize_nan_f64(lhs + rhs),
                        Instruction::SubF64 { .. } => canonicalize_nan_f64(lhs - rhs),
                        Instruction::MulF64 { .. } => canonicalize_nan_f64(lhs * rhs),
                        Instruction::DivF64 { .. } => canonicalize_nan_f64(lhs / rhs),
                        Instruction::RemF64 { .. } => deterministic_rem_f64(lhs, rhs),
                        _ => unreachable!(),
                    };
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::F64(value.to_bits()),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::CompareLtI32 { dst, lhs, rhs }
                | Instruction::CompareLtI64 { dst, lhs, rhs }
                | Instruction::CompareLtF32 { dst, lhs, rhs }
                | Instruction::CompareLtF64 { dst, lhs, rhs } => {
                    let lhs = register(&continuation.arena, lhs)?;
                    let rhs = register(&continuation.arena, rhs)?;
                    let value = match (instruction, lhs, rhs) {
                        (
                            Instruction::CompareLtI32 { .. },
                            RuntimeValue::I32(lhs),
                            RuntimeValue::I32(rhs),
                        ) => lhs < rhs,
                        (
                            Instruction::CompareLtI64 { .. },
                            RuntimeValue::I64(lhs),
                            RuntimeValue::I64(rhs),
                        ) => lhs < rhs,
                        (
                            Instruction::CompareLtF32 { .. },
                            RuntimeValue::F32(lhs),
                            RuntimeValue::F32(rhs),
                        ) => f32::from_bits(lhs) < f32::from_bits(rhs),
                        (
                            Instruction::CompareLtF64 { .. },
                            RuntimeValue::F64(lhs),
                            RuntimeValue::F64(rhs),
                        ) => f64::from_bits(lhs) < f64::from_bits(rhs),
                        _ => return Err(InterpreterError::TypeMismatch),
                    };
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(value))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StringLen { dst, source }
                | Instruction::StringByteLen { dst, source }
                | Instruction::StringHash { dst, source } => {
                    let RuntimeValue::String { reference, hash } =
                        register(&continuation.arena, source)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                    let value = match instruction {
                        Instruction::StringLen { .. } => RuntimeValue::I32(
                            i32::try_from(heap.string(reference)?.chars().count())
                                .map_err(|_| InterpreterError::StringLengthOverflow)?,
                        ),
                        Instruction::StringByteLen { .. } => RuntimeValue::I32(
                            i32::try_from(heap.string(reference)?.len())
                                .map_err(|_| InterpreterError::StringLengthOverflow)?,
                        ),
                        Instruction::StringHash { .. } => {
                            RuntimeValue::I64(i64::from_ne_bytes(hash.to_ne_bytes()))
                        }
                        _ => unreachable!(),
                    };
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::I32ToString { dst, source }
                | Instruction::I64ToString { dst, source }
                | Instruction::F32ToString { dst, source }
                | Instruction::F64ToString { dst, source }
                | Instruction::BoolToString { dst, source }
                | Instruction::RuneToString { dst, source } => {
                    let value = register(&continuation.arena, source)?;
                    let mut text = ScalarText::new();
                    match (instruction, value) {
                        (Instruction::I32ToString { .. }, RuntimeValue::I32(value)) => {
                            write!(&mut text, "{value}")
                        }
                        (Instruction::I64ToString { .. }, RuntimeValue::I64(value)) => {
                            write!(&mut text, "{value}")
                        }
                        (Instruction::F32ToString { .. }, RuntimeValue::F32(bits)) => {
                            write!(&mut text, "{}", f32::from_bits(bits))
                        }
                        (Instruction::F64ToString { .. }, RuntimeValue::F64(bits)) => {
                            write!(&mut text, "{}", f64::from_bits(bits))
                        }
                        (Instruction::BoolToString { .. }, RuntimeValue::Bool(value)) => {
                            write!(&mut text, "{value}")
                        }
                        (Instruction::RuneToString { .. }, RuntimeValue::Rune(value)) => {
                            let value =
                                char::from_u32(value).ok_or(InterpreterError::TypeMismatch)?;
                            write!(&mut text, "{value}")
                        }
                        _ => return Err(InterpreterError::TypeMismatch),
                    }
                    .map_err(|_| InterpreterError::StringLengthOverflow)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let value = allocate_runtime_string(heap, text.as_str())?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StringToString { dst, source } => {
                    let value @ RuntimeValue::String { .. } =
                        register(&continuation.arena, source)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StandardIntrinsic {
                    intrinsic,
                    args_base,
                    args_count,
                    dst,
                } => {
                    let arguments = standard_intrinsic_arguments(
                        intrinsic,
                        args_base,
                        args_count,
                        &continuation.arena,
                    )?;
                    match run_standard_intrinsic(
                        intrinsic,
                        &arguments[..usize::from(args_count)],
                        heap.as_deref_mut(),
                    )? {
                        StandardIntrinsicOutcome::Returned(value) => {
                            set_register(&mut continuation.arena, dst, value)?;
                            increment_pc(&mut continuation.arena)?;
                        }
                        StandardIntrinsicOutcome::Retry => {}
                        StandardIntrinsicOutcome::Trapped(message) => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            return Ok(InterpreterOutcome::Trapped {
                                trap: Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::StandardLibrary,
                                    message,
                                ),
                                charge,
                                fuel,
                            });
                        }
                    }
                }
                Instruction::StringEqual { dst, lhs, rhs } => {
                    let RuntimeValue::String { reference: lhs, .. } =
                        register(&continuation.arena, lhs)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::String { reference: rhs, .. } =
                        register(&continuation.arena, rhs)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Bool(heap.string(lhs)? == heap.string(rhs)?),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StringConcat { dst, lhs, rhs } => {
                    let RuntimeValue::String { reference: lhs, .. } =
                        register(&continuation.arena, lhs)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::String { reference: rhs, .. } =
                        register(&continuation.arena, rhs)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let reference = heap.concat_strings(lhs, rhs)?;
                    let hash = heap.string_hash(reference)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::String { reference, hash },
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StringRuneAt { dst, source, index } => {
                    let RuntimeValue::String {
                        reference: source, ..
                    } = register(&continuation.arena, source)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = if let Ok(index) = usize::try_from(index) {
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .string_rune_at(source, index)?
                    } else {
                        None
                    };
                    let Some(value) = value else {
                        settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                        return Ok(InterpreterOutcome::Trapped {
                            trap: Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::StringIndexOutOfBounds,
                                "string rune index out of bounds",
                            ),
                            charge,
                            fuel,
                        });
                    };
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Rune(value.into()),
                    )?;
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
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Bool(runtime_values_equal(lhs, rhs)),
                    )?;
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
                    continuation.arena.push_call_at(
                        callee_id,
                        callee.registers,
                        Some(dst),
                        Some(frame.pc),
                    )?;
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_call_depth(continuation.arena.depth());
                    }
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
                    ensure_host_call_available(migration.is_some())?;
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
                    continuation.host_call_boundary = Some(HostCallBoundary {
                        import,
                        function: frame.function,
                        pc: frame.pc,
                        source_span: module.module().source_span(frame.function, frame.pc),
                    });
                    if metadata.result.is_some() {
                        set_register(&mut continuation.arena, dst, RuntimeValue::Unit)?;
                    }
                    let outcome = match host
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .call(
                            import,
                            &arguments[..usize::from(args_count)],
                            heap.as_deref_mut(),
                        ) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            let (code, argument) = match error {
                                crate::HostTrap::UnknownFunction(function) => {
                                    ("NX4001", function.0)
                                }
                                crate::HostTrap::Arity => ("NX4003", 0),
                                crate::HostTrap::Type => ("NX4003", 1),
                                crate::HostTrap::ResourceCapacity => ("NX5004", 0),
                                crate::HostTrap::Panicked => ("NX5001", 0),
                                crate::HostTrap::Host(_) => ("NX5001", 1),
                            };
                            return Ok(InterpreterOutcome::Trapped {
                                trap: Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::Host,
                                    crate::RuntimeMessage::Code {
                                        code: crate::DiagnosticCode::new(code),
                                        argument,
                                    },
                                ),
                                charge,
                                fuel,
                            });
                        }
                    };
                    match outcome {
                        InterpreterHostOutcome::Immediate(value) => {
                            if metadata.result != runtime_value_type(value) {
                                settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                                return Ok(InterpreterOutcome::Trapped {
                                    trap: Trap::from_continuation(
                                        module,
                                        &continuation,
                                        TrapKind::Host,
                                        crate::RuntimeMessage::Code {
                                            code: crate::DiagnosticCode::new("NX5001"),
                                            argument: 2,
                                        },
                                    ),
                                    charge,
                                    fuel,
                                });
                            }
                            if metadata.result.is_some() {
                                set_register(&mut continuation.arena, dst, value)?;
                            }
                            increment_pc(&mut continuation.arena)?;
                            continuation.host_call_boundary = None;
                        }
                        InterpreterHostOutcome::Pending(request) => {
                            if metadata.mode != HostCallMode::Async {
                                settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                                return Ok(InterpreterOutcome::Trapped {
                                    trap: Trap::from_continuation(
                                        module,
                                        &continuation,
                                        TrapKind::Host,
                                        crate::RuntimeMessage::Code {
                                            code: crate::DiagnosticCode::new("NX5001"),
                                            argument: 3,
                                        },
                                    ),
                                    charge,
                                    fuel,
                                });
                            }
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
                Instruction::StateCurrentGet {
                    stable_id,
                    type_id,
                    dst,
                } => {
                    let current = state_registry.as_deref_mut().map_or_else(
                        || {
                            Err(RuntimeMessage::Static(
                                "current state registry is unavailable",
                            ))
                        },
                        |registry| registry.current_object(stable_id, type_id),
                    );
                    let value = state_operation!(current);
                    if runtime_value_type(value) != Some(ValueType::Named(type_id)) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
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
                Instruction::StateHandleResolve {
                    handle,
                    target,
                    result_type,
                    dst,
                } => {
                    let handle =
                        runtime_state_handle(register(&continuation.arena, handle)?, target)?;
                    let resolved = state_registry
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .resolve(handle, target);
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?;
                    let result = match resolved {
                        Ok(value) => {
                            let variant = StableId::from_parts(&["Result", "::Ok"]);
                            heap.allocate_enum(result_type, variant, 0, Some(value))?
                        }
                        Err(error) => {
                            let error_type = nexa_bytecode::state_handle_error_type();
                            let tag = state_handle_error_tag(error);
                            let variant = error_type.variants[tag as usize].stable_id;
                            let error_value =
                                heap.allocate_enum(error_type.type_id, variant, tag, None)?;
                            let variant = StableId::from_parts(&["Result", "::Err"]);
                            heap.allocate_enum(result_type, variant, 1, Some(error_value))?
                        }
                    };
                    set_register(&mut continuation.arena, dst, result)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateHandleIsAlive {
                    handle,
                    target,
                    dst,
                } => {
                    let handle =
                        runtime_state_handle(register(&continuation.arena, handle)?, target)?;
                    let alive = state_registry
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .is_alive(handle);
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(alive))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateHandleStableId {
                    handle,
                    target,
                    dst,
                } => {
                    let handle =
                        runtime_state_handle(register(&continuation.arena, handle)?, target)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Opaque {
                            value: handle.stable_id.0,
                            type_id: StableId::from_name("StableId"),
                        },
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateHandleGeneration {
                    handle,
                    target,
                    dst,
                } => {
                    let handle =
                        runtime_state_handle(register(&continuation.arena, handle)?, target)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(i32::from_le_bytes(handle.generation.to_le_bytes())),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateHandleEqual {
                    lhs,
                    rhs,
                    target,
                    dst,
                } => {
                    let lhs = runtime_state_handle(register(&continuation.arena, lhs)?, target)?;
                    let rhs = runtime_state_handle(register(&continuation.arena, rhs)?, target)?;
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(lhs == rhs))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StateHandleHash {
                    handle,
                    target,
                    dst,
                } => {
                    let handle =
                        runtime_state_handle(register(&continuation.arena, handle)?, target)?;
                    let hash = handle.deterministic_hash().to_le_bytes();
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]])),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumNew {
                    type_id,
                    variant,
                    payload,
                    dst,
                } => {
                    let variant = module
                        .enum_variant(type_id.0, variant.0)
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
                Instruction::EnumEqual { lhs, rhs, dst } => {
                    let lhs = register(&continuation.arena, lhs)?;
                    let rhs = register(&continuation.arena, rhs)?;
                    let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Bool(heap.enum_equal(lhs, rhs)?),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructNew {
                    type_id,
                    fields_base,
                    fields_count,
                    dst,
                } => {
                    let mut fields = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
                    for index in 0..fields_count {
                        fields[usize::from(index)] = register(
                            &continuation.arena,
                            fields_base
                                .checked_add(index)
                                .ok_or(InterpreterError::TypeMismatch)?,
                        )?;
                    }
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let value =
                        heap.allocate_struct(type_id, &fields[..usize::from(fields_count)])?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructGet { source, field, dst } => {
                    let value = register(&continuation.arena, source)?;
                    let RuntimeValue::Struct { type_id, .. } = value else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = module
                        .struct_field(type_id.0, field.0)
                        .map(|(index, _)| index)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        heap.struct_field(value, index)?,
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructWith {
                    source,
                    field,
                    value,
                    dst,
                } => {
                    let source = register(&continuation.arena, source)?;
                    let replacement = register(&continuation.arena, value)?;
                    let RuntimeValue::Struct { type_id, .. } = source else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = module
                        .struct_field(type_id.0, field.0)
                        .map(|(index, _)| index)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        heap.struct_with(source, index, replacement)?,
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructEqual { lhs, rhs, dst } => {
                    let lhs = register(&continuation.arena, lhs)?;
                    let rhs = register(&continuation.arena, rhs)?;
                    let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::Bool(heap.struct_equal(lhs, rhs)?),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassNew {
                    type_id,
                    fields_base,
                    fields_count,
                    dst,
                } => {
                    let mut fields = [RuntimeValue::Unit; nexa_bytecode::MAX_CLASS_FIELDS];
                    for index in 0..fields_count {
                        fields[usize::from(index)] = register(
                            &continuation.arena,
                            fields_base
                                .checked_add(index)
                                .ok_or(InterpreterError::TypeMismatch)?,
                        )?;
                    }
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let value =
                        heap.allocate_class(type_id, &fields[..usize::from(fields_count)])?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassGet { source, field, dst } => {
                    let value = register(&continuation.arena, source)?;
                    let (RuntimeValue::NamedRef { type_id, .. }
                    | RuntimeValue::Opaque { type_id, .. }) = value
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let (index, expected) = module
                        .class_field(type_id.0, field.0)
                        .map(|(index, field)| (index, field.ty))
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let field_value = match value {
                        RuntimeValue::NamedRef { .. } => heap
                            .as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .class_field(value, index)?,
                        RuntimeValue::Opaque {
                            value: stable_id, ..
                        } => {
                            let field_value = state_registry.as_deref_mut().map_or_else(
                                || {
                                    Err(RuntimeMessage::Static(
                                        "current state registry is unavailable",
                                    ))
                                },
                                |registry| {
                                    registry.object_field(
                                        StableId(stable_id),
                                        type_id,
                                        field,
                                        expected,
                                    )
                                },
                            );
                            state_operation!(field_value)
                        }
                        _ => unreachable!("class receiver was checked above"),
                    };
                    if runtime_value_type(field_value) != Some(expected) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, field_value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassSet {
                    source,
                    field,
                    value,
                } => {
                    let object = register(&continuation.arena, source)?;
                    let replacement = register(&continuation.arena, value)?;
                    let (RuntimeValue::NamedRef { type_id, .. }
                    | RuntimeValue::Opaque { type_id, .. }) = object
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let (index, expected) = module
                        .class_field(type_id.0, field.0)
                        .map(|(index, field)| (index, field.ty))
                        .ok_or(InterpreterError::TypeMismatch)?;
                    if runtime_value_type(replacement) != Some(expected) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    match object {
                        RuntimeValue::NamedRef { .. } => heap
                            .as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .set_class_field(object, index, replacement)?,
                        RuntimeValue::Opaque {
                            value: stable_id, ..
                        } => {
                            let update = state_registry.as_deref_mut().map_or_else(
                                || {
                                    Err(RuntimeMessage::Static(
                                        "current state registry is unavailable",
                                    ))
                                },
                                |registry| {
                                    registry.set_object_field(
                                        StableId(stable_id),
                                        type_id,
                                        field,
                                        expected,
                                        replacement,
                                    )
                                },
                            );
                            state_operation!(update);
                        }
                        _ => unreachable!("class receiver was checked above"),
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassEqual { lhs, rhs, dst } => {
                    let lhs = register(&continuation.arena, lhs)?;
                    let rhs = register(&continuation.arena, rhs)?;
                    let equal = match (lhs, rhs) {
                        (
                            RuntimeValue::Opaque {
                                value: lhs,
                                type_id: lhs_type,
                            },
                            RuntimeValue::Opaque {
                                value: rhs,
                                type_id: rhs_type,
                            },
                        ) => lhs == rhs && lhs_type == rhs_type,
                        (RuntimeValue::Opaque { .. }, RuntimeValue::NamedRef { .. })
                        | (RuntimeValue::NamedRef { .. }, RuntimeValue::Opaque { .. }) => false,
                        _ => heap
                            .as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .class_equal(lhs, rhs)?,
                    };
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(equal))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayNew { type_id, dst } => {
                    let element_type = module
                        .array_type(type_id.0)
                        .map(|array_type| array_type.element)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let value = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .allocate_array(type_id, element_type)?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayLen { source, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let length = heap
                        .as_deref()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .array_len(array)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(
                            i32::try_from(length)
                                .map_err(|_| InterpreterError::StringLengthOverflow)?,
                        ),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayGet { source, index, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = array_index!(index);
                    let value = array_operation!(
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_get(array, index)
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArraySet {
                    source,
                    index,
                    value,
                } => {
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let replacement = register(&continuation.arena, value)?;
                    let index = array_index!(index);
                    array_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_set(array, index, replacement)
                    );
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayPush { source, value } => {
                    let array = register(&continuation.arena, source)?;
                    let value = register(&continuation.arena, value)?;
                    heap.as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .array_push(array, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayPop { source, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let value = array_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_pop(array)
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayInsert {
                    source,
                    index,
                    value,
                } => {
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = register(&continuation.arena, value)?;
                    let index = array_index!(index);
                    array_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_insert(array, index, value)
                    );
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayRemove { source, index, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = array_index!(index);
                    let value = array_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_remove(array, index)
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayClear { source } => {
                    let array = register(&continuation.arena, source)?;
                    heap.as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .array_clear(array)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapNew { type_id, dst } => {
                    let map_type = module
                        .map_type(type_id.0)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let value = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .allocate_map(type_id, map_type.key, map_type.value)?;
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapLen { source, dst } => {
                    let map = register(&continuation.arena, source)?;
                    let length = heap
                        .as_deref()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .map_len(map)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(
                            i32::try_from(length)
                                .map_err(|_| InterpreterError::StringLengthOverflow)?,
                        ),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapGet {
                    source,
                    key,
                    result_type,
                    dst,
                }
                | Instruction::MapRemove {
                    source,
                    key,
                    result_type,
                    dst,
                } => {
                    let map = register(&continuation.arena, source)?;
                    let key = register(&continuation.arena, key)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let mut reservation = heap.preflight(1)?;
                    let value = if matches!(instruction, Instruction::MapGet { .. }) {
                        heap.map_get(map, key)?
                    } else {
                        heap.map_remove(map, key)?
                    };
                    let (variant, tag, payload) = if let Some(value) = value {
                        (StableId::from_parts(&["Option", "::Some"]), 1, Some(value))
                    } else {
                        (StableId::from_parts(&["Option", "::None"]), 0, None)
                    };
                    let value = heap.allocate_enum_reserved(
                        &mut reservation,
                        result_type,
                        variant,
                        tag,
                        payload,
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapSet { source, key, value } => {
                    let map = register(&continuation.arena, source)?;
                    let key = register(&continuation.arena, key)?;
                    let value = register(&continuation.arena, value)?;
                    if heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .map_set(map, key, value)?
                        == MapSetOutcome::Complete
                    {
                        increment_pc(&mut continuation.arena)?;
                    }
                }
                Instruction::MapContains { source, key, dst } => {
                    let map = register(&continuation.arena, source)?;
                    let key = register(&continuation.arena, key)?;
                    let contains = heap
                        .as_deref()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .map_contains(map, key)?;
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(contains))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapClear { source } => {
                    let map = register(&continuation.arena, source)?;
                    heap.as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .map_clear(map)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::BufferLen { source, dst } => {
                    let buffer = register(&continuation.arena, source)?;
                    let length = heap
                        .as_deref()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .buffer_len(buffer)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(
                            i32::try_from(length)
                                .map_err(|_| InterpreterError::StringLengthOverflow)?,
                        ),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::BufferGet { source, index, dst } => {
                    let buffer = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = buffer_index!(index);
                    let value = buffer_operation!(
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .buffer_get(buffer, index)
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::BufferSet {
                    source,
                    index,
                    value,
                } => {
                    let buffer = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let value = register(&continuation.arena, value)?;
                    let index = buffer_index!(index);
                    buffer_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .buffer_set(buffer, index, value)
                    );
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::BufferSlice {
                    source,
                    start,
                    length,
                    dst,
                } => {
                    let buffer = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(start) = register(&continuation.arena, start)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(length) = register(&continuation.arena, length)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let start = buffer_index!(start);
                    let length = buffer_index!(length);
                    let value = buffer_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .buffer_slice(buffer, start, length)
                    );
                    set_register(&mut continuation.arena, dst, value)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::BufferCopy {
                    destination,
                    source,
                    source_start,
                    destination_start,
                    length,
                } => {
                    let destination = register(&continuation.arena, destination)?;
                    let source = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(source_start) =
                        register(&continuation.arena, source_start)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(destination_start) =
                        register(&continuation.arena, destination_start)?
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let RuntimeValue::I32(length) = register(&continuation.arena, length)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    buffer_operation!(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .buffer_copy(
                                destination,
                                source,
                                buffer_index!(source_start),
                                buffer_index!(destination_start),
                                buffer_index!(length),
                            )
                    );
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
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_fuel_used(charge.fuel_used);
                    }
                    if start_next_defer(module, &mut continuation.arena)? {
                        if let Some(migration) = migration.as_deref_mut() {
                            migration.observe_call_depth(continuation.arena.depth());
                        }
                        pending_cost = 0;
                        continue;
                    }
                    let result = register(&continuation.arena, source)?;
                    let completed = continuation.arena.pop()?;
                    let returning_cleanup =
                        completed.return_target.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup {
                        if continuation.cleanup_mode
                            && !start_next_defer(module, &mut continuation.arena)?
                        {
                            return Ok(InterpreterOutcome::Returned {
                                value: None,
                                charge,
                                fuel,
                            });
                        }
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
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_fuel_used(charge.fuel_used);
                    }
                    if start_next_defer(module, &mut continuation.arena)? {
                        if let Some(migration) = migration.as_deref_mut() {
                            migration.observe_call_depth(continuation.arena.depth());
                        }
                        pending_cost = 0;
                        continue;
                    }
                    let completed = continuation.arena.pop()?;
                    let returning_cleanup =
                        completed.return_target.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup {
                        if continuation.cleanup_mode
                            && !start_next_defer(module, &mut continuation.arena)?
                        {
                            return Ok(InterpreterOutcome::Returned {
                                value: None,
                                charge,
                                fuel,
                            });
                        }
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
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_fuel_used(charge.fuel_used);
                    }
                    return Ok(InterpreterOutcome::Trapped {
                        trap: Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::BytecodeTrap,
                            "bytecode trap",
                        ),
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
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_fuel_used(charge.fuel_used);
                    }
                    continuation.arena.pop()?;
                    if continuation.cleanup_mode
                        && continuation.arena.depth() > 0
                        && !start_next_defer(module, &mut continuation.arena)?
                    {
                        return Ok(InterpreterOutcome::Returned {
                            value: None,
                            charge,
                            fuel,
                        });
                    }
                    if continuation.arena.depth() == 0 {
                        return Ok(InterpreterOutcome::Returned {
                            value: None,
                            charge,
                            fuel,
                        });
                    }
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
        }
        crate::DeferAction::Trap => return Err(InterpreterError::TypeMismatch),
        crate::DeferAction::ReleaseCounter(_) | crate::DeferAction::SetFlag(_) => {}
    }
    Ok(true)
}

enum StandardIntrinsicOutcome {
    Returned(RuntimeValue),
    Retry,
    Trapped(crate::RuntimeMessage),
}

#[allow(clippy::too_many_lines)]
fn instruction_attempt_fuel(
    module: &nexa_bytecode::Module,
    nominal_shape: nexa_verifier::NominalIndexShape,
    instruction: Instruction,
    arena: &FrameArena,
    heap: Option<&Heap>,
    costs: &OpcodeCostTable,
) -> Result<u64, InterpreterError> {
    let heap_required = || heap.ok_or(InterpreterError::HeapUnavailable);
    let base = costs.cost(instruction);
    let work = match instruction {
        Instruction::StandardIntrinsic {
            intrinsic,
            args_base,
            args_count,
            ..
        } => {
            return standard_intrinsic_attempt_fuel(intrinsic, args_base, args_count, arena, heap);
        }
        Instruction::LoadString { string, .. } => {
            let bytes = module
                .strings
                .get(string as usize)
                .ok_or(InterpreterError::TypeMismatch)?
                .len();
            fuel_blocks(fuel_usize(bytes)?, STANDARD_STRING_FUEL_BLOCK_BYTES)?
                .checked_mul(2)
                .ok_or(InterpreterError::FuelCostOverflow)?
        }
        Instruction::StringLen { source, .. } | Instruction::StringRuneAt { source, .. } => {
            fuel_blocks(
                register_string_bytes(arena, source, heap_required()?)?,
                STANDARD_STRING_FUEL_BLOCK_BYTES,
            )?
        }
        Instruction::StringEqual { lhs, rhs, .. } => {
            let heap = heap_required()?;
            let left = register_string_bytes(arena, lhs, heap)?;
            let right = register_string_bytes(arena, rhs, heap)?;
            fuel_blocks(left.min(right), STANDARD_STRING_FUEL_BLOCK_BYTES)?
        }
        Instruction::StringConcat { lhs, rhs, .. } => {
            let heap = heap_required()?;
            let bytes = register_string_bytes(arena, lhs, heap)?
                .checked_add(register_string_bytes(arena, rhs, heap)?)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            fuel_blocks(bytes, STANDARD_STRING_FUEL_BLOCK_BYTES)?
                .checked_mul(2)
                .ok_or(InterpreterError::FuelCostOverflow)?
        }
        Instruction::I32ToString { .. }
        | Instruction::I64ToString { .. }
        | Instruction::F32ToString { .. }
        | Instruction::F64ToString { .. }
        | Instruction::BoolToString { .. }
        | Instruction::RuneToString { .. } => {
            fuel_blocks(SCALAR_TO_STRING_MAX_BYTES, STANDARD_STRING_FUEL_BLOCK_BYTES)?
                .checked_mul(SCALAR_TO_STRING_FUEL_PASSES)
                .ok_or(InterpreterError::FuelCostOverflow)?
        }
        Instruction::Call {
            function,
            args_count,
            ..
        } => call_frame_attempt_fuel(module, function, args_count)?,
        Instruction::Return { .. } | Instruction::ReturnVoid | Instruction::CleanupReturn => {
            return_defer_attempt_fuel(module, instruction, arena)?
        }
        Instruction::EnumNew { .. } => nominal_index_lookup_fuel(nominal_shape.enum_variants)?,
        Instruction::StructNew {
            fields_base,
            fields_count,
            ..
        } => register_structural_hash_fuel(arena, heap_required()?, fields_base, fields_count)?,
        Instruction::StructGet { .. } => nominal_index_lookup_fuel(nominal_shape.struct_fields)?,
        Instruction::StructWith { source, value, .. } => {
            let heap = heap_required()?;
            let source = register(arena, source)?;
            let replacement = register(arena, value)?;
            let fields = heap.struct_fields(source)?;
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.struct_fields)?,
                fuel_add(
                    runtime_values_hash_fuel(heap, fields)?,
                    fuel_add(
                        value_visit_fuel(1, 1)?,
                        runtime_value_hash_fuel(heap, replacement)?,
                    )?,
                )?,
            )?
        }
        Instruction::EnumEqual { lhs, .. } | Instruction::StructEqual { lhs, .. } => {
            runtime_value_comparison_fuel(heap_required()?, register(arena, lhs)?)?
        }
        Instruction::ClassNew { fields_count, .. } => value_visit_fuel(u64::from(fields_count), 2)?,
        Instruction::ClassGet { .. } | Instruction::ClassSet { .. } => {
            nominal_index_lookup_fuel(nominal_shape.class_fields)?
        }
        Instruction::ArrayNew { .. } => nominal_index_lookup_fuel(nominal_shape.array_types)?,
        Instruction::ArrayPush { source, .. } | Instruction::ArrayInsert { source, .. } => {
            let heap = heap_required()?;
            let array = register(arena, source)?;
            let old_length = heap.array_len(array)?;
            let new_length = old_length
                .checked_add(1)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            array_replace_attempt_fuel(heap, old_length, new_length)?
        }
        Instruction::ArrayPop { source, .. } | Instruction::ArrayRemove { source, .. } => {
            let heap = heap_required()?;
            let old_length = heap.array_len(register(arena, source)?)?;
            array_replace_attempt_fuel(heap, old_length, old_length.saturating_sub(1))?
        }
        Instruction::ArrayClear { source } => {
            let heap = heap_required()?;
            let old_length = heap.array_len(register(arena, source)?)?;
            array_replace_attempt_fuel(heap, old_length, 0)?
        }
        Instruction::MapGet { source, key, .. }
        | Instruction::MapRemove { source, key, .. }
        | Instruction::MapContains { source, key, .. } => map_lookup_fuel(
            heap_required()?,
            register(arena, source)?,
            register(arena, key)?,
        )?,
        Instruction::MapSet { source, key, .. } => map_insert_attempt_fuel(
            heap_required()?,
            register(arena, source)?,
            register(arena, key)?,
        )?,
        Instruction::MapNew { .. } => nominal_index_lookup_fuel(nominal_shape.map_types)?,
        Instruction::MapClear { source } => {
            let shape = heap_required()?.map_fuel_shape(register(arena, source)?)?;
            let slots = fuel_usize(shape.current_slots)?
                .checked_add(fuel_usize(shape.old_slots)?)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let slots = slots
                .checked_add(fuel_usize(shape.new_slots)?)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            fuel_blocks(slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?
        }
        Instruction::BufferSlice { length, .. } => {
            let work = match register(arena, length)? {
                RuntimeValue::I32(length) => u64::try_from(length).unwrap_or(0),
                _ => return Err(InterpreterError::TypeMismatch),
            };
            let element_work = fuel_blocks(work, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            let metadata_work = collection_arena_metadata_fuel(heap_required()?, work != 0, false)?;
            fuel_add(element_work, metadata_work)?
        }
        Instruction::BufferCopy { length, .. } => {
            let work = match register(arena, length)? {
                RuntimeValue::I32(length) => u64::try_from(length).unwrap_or(0),
                _ => return Err(InterpreterError::TypeMismatch),
            };
            fuel_blocks(work, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?
        }
        _ => 0,
    };
    fuel_add(base, work)
}

fn call_frame_attempt_fuel(
    module: &nexa_bytecode::Module,
    function: u32,
    args_count: u16,
) -> Result<u64, InterpreterError> {
    let callee = module
        .functions
        .get(function as usize)
        .ok_or(InterpreterError::MissingFunction(function))?;
    // Arguments are first type-checked in the caller frame and then copied to
    // the freshly initialized callee frame.
    frame_initialization_fuel(callee.registers, args_count, 2)
}

fn return_defer_attempt_fuel(
    module: &nexa_bytecode::Module,
    instruction: Instruction,
    arena: &FrameArena,
) -> Result<u64, InterpreterError> {
    let action = match instruction {
        Instruction::CleanupReturn => {
            let Some(parent_index) = arena.depth().checked_sub(2) else {
                return Ok(0);
            };
            arena.peek_defer_for_frame(parent_index)?
        }
        Instruction::Return { .. } | Instruction::ReturnVoid => {
            if let Some(action) = arena.peek_defer_for_current_frame()? {
                Some(action)
            } else {
                let current = arena.current()?;
                if current.return_target.is_none() {
                    arena
                        .depth()
                        .checked_sub(2)
                        .map(|parent_index| arena.peek_defer_for_frame(parent_index))
                        .transpose()?
                        .flatten()
                } else {
                    None
                }
            }
        }
        _ => None,
    };
    action.map_or(Ok(0), |action| defer_action_attempt_fuel(module, &action))
}

fn defer_action_attempt_fuel(
    module: &nexa_bytecode::Module,
    action: &crate::DeferAction,
) -> Result<u64, InterpreterError> {
    match *action {
        crate::DeferAction::Call {
            function,
            args_count,
            ..
        } => {
            let cleanup = module
                .functions
                .get(function as usize)
                .ok_or(InterpreterError::MissingFunction(function))?;
            frame_initialization_fuel(cleanup.registers, u16::from(args_count), 1)
        }
        crate::DeferAction::ReleaseCounter(_)
        | crate::DeferAction::SetFlag(_)
        | crate::DeferAction::Trap => Ok(1),
    }
}

fn frame_initialization_fuel(
    registers: u16,
    args_count: u16,
    argument_passes: u64,
) -> Result<u64, InterpreterError> {
    let argument_work = u64::from(args_count)
        .checked_mul(argument_passes)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let work = u64::from(registers)
        .checked_add(argument_work)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel_blocks(work, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)
}

fn nominal_index_lookup_fuel(entries: usize) -> Result<u64, InterpreterError> {
    let entries = fuel_usize(entries)?;
    if entries == 0 {
        return Ok(0);
    }
    let comparisons = u64::from(entries.ilog2())
        .checked_add(1)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel_blocks(comparisons, 8)
}

fn register_string_bytes(
    arena: &FrameArena,
    source: u16,
    heap: &Heap,
) -> Result<u64, InterpreterError> {
    let RuntimeValue::String { reference, .. } = register(arena, source)? else {
        return Err(InterpreterError::TypeMismatch);
    };
    fuel_usize(heap.string(reference)?.len())
}

fn runtime_value_comparison_fuel(
    heap: &Heap,
    value: RuntimeValue,
) -> Result<u64, InterpreterError> {
    let shape = heap.map_key_fuel_shape(value)?;
    let string_bytes = fuel_usize(shape.string_bytes)?
        .checked_mul(fuel_usize(shape.string_objects)?)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let string_work = fuel_blocks(string_bytes, STANDARD_STRING_FUEL_BLOCK_BYTES)?;
    let structural_values = fuel_usize(shape.structural_objects)?
        .checked_mul(fuel_usize(shape.fields_per_object)?)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let structural_work = fuel_blocks(structural_values, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
    fuel_add(string_work, structural_work)
}

fn register_structural_hash_fuel(
    arena: &FrameArena,
    heap: &Heap,
    fields_base: u16,
    fields_count: u16,
) -> Result<u64, InterpreterError> {
    if usize::from(fields_count) > nexa_bytecode::MAX_STRUCT_FIELDS {
        return Err(InterpreterError::TypeMismatch);
    }
    let mut work = value_visit_fuel(u64::from(fields_count), 3)?;
    for offset in 0..fields_count {
        let field = fields_base
            .checked_add(offset)
            .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
        work = fuel_add(
            work,
            runtime_value_hash_fuel(heap, register(arena, field)?)?,
        )?;
    }
    Ok(work)
}

fn runtime_values_hash_fuel(heap: &Heap, values: &[RuntimeValue]) -> Result<u64, InterpreterError> {
    let mut work = value_visit_fuel(fuel_usize(values.len())?, 3)?;
    for value in values {
        work = fuel_add(work, runtime_value_hash_fuel(heap, *value)?)?;
    }
    Ok(work)
}

fn value_visit_fuel(values: u64, passes: u64) -> Result<u64, InterpreterError> {
    let work = values
        .checked_mul(passes)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel_blocks(work, 8)
}

fn array_replace_attempt_fuel(
    heap: &Heap,
    old_length: usize,
    new_length: usize,
) -> Result<u64, InterpreterError> {
    // Replacing an arena range copies the new values and clears the released
    // old values. Both passes are visible work even though the arena storage
    // itself was reserved at Realm admission.
    let elements = fuel_usize(old_length)?
        .checked_add(fuel_usize(new_length)?)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let element_work = fuel_blocks(elements, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
    let metadata_work = collection_arena_metadata_fuel(heap, new_length != 0, old_length != 0)?;
    fuel_add(element_work, metadata_work)
}

fn collection_arena_metadata_fuel(
    heap: &Heap,
    claim: bool,
    release: bool,
) -> Result<u64, InterpreterError> {
    if !claim && !release {
        return Ok(0);
    }

    let ranges = fuel_usize(heap.collection_arena_fuel_shape().free_ranges)?;
    let mut metadata_steps = 0_u64;
    if claim {
        // find_free scan + claim position scan + one Vec remove/insert shift.
        metadata_steps = ranges
            .checked_mul(3)
            .ok_or(InterpreterError::FuelCostOverflow)?;
    }
    if release {
        // A splitting claim can add one range, and release inserts another.
        // Account for partition search, insertion shift, the merge scan, and
        // both possible adjacent-range removal shifts.
        let release_ranges = ranges
            .checked_add(2)
            .ok_or(InterpreterError::FuelCostOverflow)?;
        metadata_steps = metadata_steps
            .checked_add(
                release_ranges
                    .checked_mul(5)
                    .ok_or(InterpreterError::FuelCostOverflow)?,
            )
            .ok_or(InterpreterError::FuelCostOverflow)?;
    }
    fuel_blocks(metadata_steps, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)
}

fn standard_intrinsic_arguments(
    intrinsic: StandardIntrinsic,
    args_base: u16,
    args_count: u16,
    arena: &FrameArena,
) -> Result<[RuntimeValue; 3], InterpreterError> {
    if args_count != intrinsic.argument_count() || args_count > 3 {
        return Err(InterpreterError::TypeMismatch);
    }
    let mut arguments = [RuntimeValue::Unit; 3];
    for argument in 0..args_count {
        let source = args_base
            .checked_add(argument)
            .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
        arguments[usize::from(argument)] = register(arena, source)?;
    }
    Ok(arguments)
}

fn standard_intrinsic_attempt_fuel(
    intrinsic: StandardIntrinsic,
    args_base: u16,
    args_count: u16,
    arena: &FrameArena,
    heap: Option<&Heap>,
) -> Result<u64, InterpreterError> {
    let arguments = standard_intrinsic_arguments(intrinsic, args_base, args_count, arena)?;
    let heap_required = || heap.ok_or(InterpreterError::HeapUnavailable);
    let work = match intrinsic.fuel_model() {
        StandardIntrinsicFuelModel::Fixed => 0,
        StandardIntrinsicFuelModel::StringBytes {
            argument_count,
            passes,
        } => {
            let heap = heap_required()?;
            let mut bytes = 0_u64;
            for argument in &arguments[..usize::from(argument_count)] {
                let RuntimeValue::String { reference, .. } = argument else {
                    return Err(InterpreterError::TypeMismatch);
                };
                bytes = bytes
                    .checked_add(fuel_usize(heap.string(*reference)?.len())?)
                    .ok_or(InterpreterError::FuelCostOverflow)?;
            }
            fuel_blocks(bytes, STANDARD_STRING_FUEL_BLOCK_BYTES)?
                .checked_mul(u64::from(passes))
                .ok_or(InterpreterError::FuelCostOverflow)?
        }
        StandardIntrinsicFuelModel::StringSplit => {
            let (value, delimiter) = string_pair(&arguments)?;
            let heap = heap_required()?;
            let shape = heap.split_fuel_shape(value, delimiter)?;
            let bytes = fuel_usize(shape.source_bytes)?
                .checked_add(fuel_usize(shape.delimiter_bytes)?)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let scan_copy_and_hash = fuel_blocks(bytes, STANDARD_STRING_FUEL_BLOCK_BYTES)?
                .checked_mul(4)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let value_work = scan_copy_and_hash
                .checked_add(fuel_usize(shape.parts)?)
                .and_then(|work| work.checked_add(1))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            // The result reservation can be released after any later
            // fallible temporary allocation, so charge both claim and rollback
            // metadata before split scans or allocation begin.
            let metadata_work = collection_arena_metadata_fuel(heap, shape.parts != 0, true)?;
            fuel_add(value_work, metadata_work)?
        }
        StandardIntrinsicFuelModel::ArrayCopy => {
            let heap = heap_required()?;
            let old_length = heap.array_len(arguments[0])?;
            let new_length = if matches!(intrinsic, StandardIntrinsic::ArrayPush { .. }) {
                old_length
                    .checked_add(1)
                    .ok_or(InterpreterError::FuelCostOverflow)?
            } else {
                old_length.saturating_sub(1)
            };
            array_replace_attempt_fuel(heap, old_length, new_length)?
        }
        StandardIntrinsicFuelModel::MapLookup => {
            map_lookup_fuel(heap_required()?, arguments[0], arguments[1])?
        }
        StandardIntrinsicFuelModel::MapInsertAttempt => {
            map_insert_attempt_fuel(heap_required()?, arguments[0], arguments[1])?
        }
    };
    fuel_add(u64::from(intrinsic.base_fuel_cost()), work)
}

fn fuel_usize(value: usize) -> Result<u64, InterpreterError> {
    u64::try_from(value).map_err(|_| InterpreterError::FuelCostOverflow)
}

fn fuel_blocks(work: u64, block: u64) -> Result<u64, InterpreterError> {
    debug_assert_ne!(block, 0);
    if work == 0 {
        Ok(0)
    } else {
        (work - 1)
            .checked_div(block)
            .and_then(|blocks| blocks.checked_add(1))
            .ok_or(InterpreterError::FuelCostOverflow)
    }
}

fn fuel_add(left: u64, right: u64) -> Result<u64, InterpreterError> {
    left.checked_add(right)
        .ok_or(InterpreterError::FuelCostOverflow)
}

fn map_lookup_fuel(
    heap: &Heap,
    map: RuntimeValue,
    key: RuntimeValue,
) -> Result<u64, InterpreterError> {
    let shape = heap.map_fuel_shape(map)?;
    let slots = fuel_usize(shape.current_slots)?
        .checked_add(fuel_usize(shape.old_slots)?)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let slots = slots
        .checked_add(fuel_usize(shape.new_slots)?)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let slot_scan = fuel_blocks(slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;

    // Map lookup filters by cached hash first, but equal-hash keys can still
    // require content/structural comparison for every occupied slot.
    let key_hash = runtime_value_hash_fuel(heap, key)?;
    let comparison_per_slot = runtime_value_comparison_fuel(heap, key)?;
    key_hash
        .checked_add(slot_scan)
        .ok_or(InterpreterError::FuelCostOverflow)?
        .checked_add(
            comparison_per_slot
                .checked_mul(slots)
                .ok_or(InterpreterError::FuelCostOverflow)?,
        )
        .ok_or(InterpreterError::FuelCostOverflow)
}

fn runtime_value_hash_fuel(heap: &Heap, value: RuntimeValue) -> Result<u64, InterpreterError> {
    let shape = heap.map_key_fuel_shape(value)?;
    fuel_blocks(
        fuel_usize(shape.hash_structural_objects)?,
        STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS,
    )
}

fn map_insert_attempt_fuel(
    heap: &Heap,
    map: RuntimeValue,
    key: RuntimeValue,
) -> Result<u64, InterpreterError> {
    let shape = heap.map_fuel_shape(map)?;
    if shape.rehash_remaining != 0 {
        let probe_work = fuel_usize(shape.rehash_remaining)?
            .checked_mul(fuel_usize(shape.new_slots)?)
            .ok_or(InterpreterError::FuelCostOverflow)?;
        return fuel_blocks(probe_work, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS);
    }
    let scan = map_lookup_fuel(heap, map, key)?;
    let mutation_slots = if shape.next_rehash_slots == 0 {
        // A non-rehashing insert probes the current table once more after the
        // lookup proved the key absent. Existing-key replacement is
        // intentionally charged the same conservative attempt bound.
        shape.current_slots
    } else {
        // A rehashing attempt allocates and initializes the new table, then
        // returns Retry without performing the final insertion.
        shape.next_rehash_slots
    };
    let mutation = fuel_blocks(
        fuel_usize(mutation_slots)?,
        STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS,
    )?;
    scan.checked_add(mutation)
        .ok_or(InterpreterError::FuelCostOverflow)
}

#[allow(clippy::too_many_lines)]
fn run_standard_intrinsic(
    intrinsic: StandardIntrinsic,
    arguments: &[RuntimeValue],
    mut heap: Option<&mut Heap>,
) -> Result<StandardIntrinsicOutcome, InterpreterError> {
    use StandardIntrinsic as Intrinsic;

    let returned = |value| Ok(StandardIntrinsicOutcome::Returned(value));
    match intrinsic {
        Intrinsic::OptionIsSome { .. }
        | Intrinsic::OptionIsNone { .. }
        | Intrinsic::ResultIsOk { .. }
        | Intrinsic::ResultIsErr { .. } => {
            let tag = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .enum_tag(arguments[0])?;
            let result = match intrinsic {
                Intrinsic::OptionIsSome { .. } | Intrinsic::ResultIsErr { .. } => tag == 1,
                Intrinsic::OptionIsNone { .. } | Intrinsic::ResultIsOk { .. } => tag == 0,
                _ => unreachable!(),
            };
            returned(RuntimeValue::Bool(result))
        }
        Intrinsic::OptionUnwrapOr { .. } | Intrinsic::ResultUnwrapOr { .. } => {
            let (_, _, tag, payload) = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .enum_parts(arguments[0])?;
            let success_tag = u32::from(matches!(intrinsic, Intrinsic::OptionUnwrapOr { .. }));
            returned(if tag == success_tag {
                payload.ok_or(InterpreterError::TypeMismatch)?
            } else {
                arguments[1]
            })
        }
        Intrinsic::F32Floor
        | Intrinsic::F32Ceil
        | Intrinsic::F32Round
        | Intrinsic::F32Sqrt
        | Intrinsic::F32Sin
        | Intrinsic::F32Cos => {
            let RuntimeValue::F32(bits) = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let value = f32::from_bits(bits);
            let result = match intrinsic {
                Intrinsic::F32Floor => deterministic_floor_f32(value),
                Intrinsic::F32Ceil => deterministic_ceil_f32(value),
                Intrinsic::F32Round => deterministic_round_f32(value),
                Intrinsic::F32Sqrt => deterministic_sqrt_f32(value),
                Intrinsic::F32Sin => deterministic_sin_f32(value),
                Intrinsic::F32Cos => deterministic_cos_f32(value),
                _ => unreachable!(),
            };
            returned(RuntimeValue::F32(result.to_bits()))
        }
        Intrinsic::F64Floor
        | Intrinsic::F64Ceil
        | Intrinsic::F64Round
        | Intrinsic::F64Sqrt
        | Intrinsic::F64Sin
        | Intrinsic::F64Cos => {
            let RuntimeValue::F64(bits) = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let value = f64::from_bits(bits);
            let result = match intrinsic {
                Intrinsic::F64Floor => deterministic_floor_f64(value),
                Intrinsic::F64Ceil => deterministic_ceil_f64(value),
                Intrinsic::F64Round => deterministic_round_f64(value),
                Intrinsic::F64Sqrt => deterministic_sqrt_f64(value),
                Intrinsic::F64Sin => deterministic_sin_f64(value),
                Intrinsic::F64Cos => deterministic_cos_f64(value),
                _ => unreachable!(),
            };
            returned(RuntimeValue::F64(result.to_bits()))
        }
        Intrinsic::StringContains | Intrinsic::StringStartsWith | Intrinsic::StringEndsWith => {
            let (value, needle) = string_pair(arguments)?;
            let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
            let value = heap.string(value)?;
            let needle = heap.string(needle)?;
            let result = match intrinsic {
                Intrinsic::StringContains => value.contains(needle),
                Intrinsic::StringStartsWith => value.starts_with(needle),
                Intrinsic::StringEndsWith => value.ends_with(needle),
                _ => unreachable!(),
            };
            returned(RuntimeValue::Bool(result))
        }
        Intrinsic::StringLen | Intrinsic::StringByteLen => {
            let RuntimeValue::String { reference, .. } = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let value = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .string(reference)?;
            let length = if matches!(intrinsic, Intrinsic::StringLen) {
                value.chars().count()
            } else {
                value.len()
            };
            returned(RuntimeValue::I32(string_length_to_i32(length)?))
        }
        Intrinsic::StringSubstring => {
            let RuntimeValue::String { reference, .. } = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let RuntimeValue::I32(start) = arguments[1] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let RuntimeValue::I32(length) = arguments[2] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let (Ok(start), Ok(length)) = (usize::try_from(start), usize::try_from(length)) else {
                return Ok(StandardIntrinsicOutcome::Trapped(
                    "string scalar range is out of bounds".into(),
                ));
            };
            let range = {
                let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                let value = heap.string(reference)?;
                let Some(end) = start.checked_add(length) else {
                    return Ok(StandardIntrinsicOutcome::Trapped(
                        "string scalar range is out of bounds".into(),
                    ));
                };
                let Some(range) = scalar_range(value, start, end) else {
                    return Ok(StandardIntrinsicOutcome::Trapped(
                        "string scalar range is out of bounds".into(),
                    ));
                };
                range
            };
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            returned(heap.copy_string_range(reference, range.start, range.end)?)
        }
        Intrinsic::StringTrim => {
            let RuntimeValue::String { reference, .. } = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            returned(
                heap.as_deref_mut()
                    .ok_or(InterpreterError::HeapUnavailable)?
                    .trim_string(reference)?,
            )
        }
        Intrinsic::StringSplit => {
            let (value, delimiter) = string_pair(arguments)?;
            returned(
                heap.as_deref_mut()
                    .ok_or(InterpreterError::HeapUnavailable)?
                    .split_string(value, delimiter)?,
            )
        }
        Intrinsic::ArrayLen { .. } | Intrinsic::ArrayIsEmpty { .. } => {
            let length = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .array_len(arguments[0])?;
            returned(match intrinsic {
                Intrinsic::ArrayLen { .. } => RuntimeValue::I32(
                    i32::try_from(length).map_err(|_| InterpreterError::StringLengthOverflow)?,
                ),
                Intrinsic::ArrayIsEmpty { .. } => RuntimeValue::Bool(length == 0),
                _ => unreachable!(),
            })
        }
        Intrinsic::ArrayGet { element } => {
            let index = match arguments[1] {
                RuntimeValue::I32(index) => usize::try_from(index).ok(),
                _ => return Err(InterpreterError::TypeMismatch),
            };
            let value = if let Some(index) = index {
                match heap
                    .as_deref()
                    .ok_or(InterpreterError::HeapUnavailable)?
                    .array_get(arguments[0], index)
                {
                    Ok(value) => Some(value),
                    Err(HeapError::IndexOutOfBounds { .. }) => None,
                    Err(error) => return Err(InterpreterError::Heap(error)),
                }
            } else {
                None
            };
            let value = allocate_option(
                heap.as_deref_mut()
                    .ok_or(InterpreterError::HeapUnavailable)?,
                element,
                value,
            )?;
            returned(value)
        }
        Intrinsic::ArrayPush { .. } => {
            heap.as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .array_push(arguments[0], arguments[1])?;
            returned(RuntimeValue::Bool(true))
        }
        Intrinsic::ArrayPop { .. } => {
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            if heap.array_len(arguments[0])? == 0 {
                return Ok(StandardIntrinsicOutcome::Trapped(
                    "cannot pop an empty array".into(),
                ));
            }
            returned(heap.array_pop(arguments[0])?)
        }
        Intrinsic::MapLen { .. } => {
            let length = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .map_len(arguments[0])?;
            returned(RuntimeValue::I32(
                i32::try_from(length).map_err(|_| InterpreterError::StringLengthOverflow)?,
            ))
        }
        Intrinsic::MapContains { .. } => returned(RuntimeValue::Bool(
            heap.as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .map_contains(arguments[0], arguments[1])?,
        )),
        Intrinsic::MapGet { value, .. } | Intrinsic::MapRemove { value, .. } => {
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            // Reserve the Option result before mutating so resource failure
            // cannot remove an entry without returning its value.
            let mut reservation = heap.preflight(1)?;
            let result = if matches!(intrinsic, Intrinsic::MapGet { .. }) {
                heap.map_get(arguments[0], arguments[1])?
            } else {
                heap.map_remove(arguments[0], arguments[1])?
            };
            let option = nexa_bytecode::option_type(value);
            let (variant, tag) = if result.is_some() {
                (StableId::from_parts(&["Option", "::Some"]), 1)
            } else {
                (StableId::from_parts(&["Option", "::None"]), 0)
            };
            returned(heap.allocate_enum_reserved(
                &mut reservation,
                option.type_id,
                variant,
                tag,
                result,
            ))
        }
        Intrinsic::MapInsert { .. } => {
            let outcome = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .map_set(arguments[0], arguments[1], arguments[2])?;
            if outcome == MapSetOutcome::Complete {
                returned(RuntimeValue::Bool(true))
            } else {
                Ok(StandardIntrinsicOutcome::Retry)
            }
        }
        Intrinsic::DebugAssert => {
            let RuntimeValue::Bool(condition) = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            if condition {
                returned(RuntimeValue::Bool(true))
            } else {
                Ok(StandardIntrinsicOutcome::Trapped("assertion failed".into()))
            }
        }
        Intrinsic::DebugTrap => {
            let RuntimeValue::String { reference, .. } = arguments[0] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let message = heap
                .as_deref()
                .ok_or(InterpreterError::HeapUnavailable)?
                .string(reference)?;
            Ok(StandardIntrinsicOutcome::Trapped(
                crate::RuntimeMessage::inline(message),
            ))
        }
    }
}

fn string_pair(arguments: &[RuntimeValue]) -> Result<(GcRef, GcRef), InterpreterError> {
    let RuntimeValue::String {
        reference: left, ..
    } = arguments[0]
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    let RuntimeValue::String {
        reference: right, ..
    } = arguments[1]
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    Ok((left, right))
}

fn allocate_runtime_string(heap: &mut Heap, value: &str) -> Result<RuntimeValue, InterpreterError> {
    let reference = heap.allocate_string(value)?;
    let hash = heap.string_hash(reference)?;
    Ok(RuntimeValue::String { reference, hash })
}

fn allocate_option(
    heap: &mut Heap,
    payload_type: ValueType,
    payload: Option<RuntimeValue>,
) -> Result<RuntimeValue, InterpreterError> {
    let option = nexa_bytecode::option_type(payload_type);
    let (variant, tag) = if payload.is_some() {
        (StableId::from_parts(&["Option", "::Some"]), 1)
    } else {
        (StableId::from_parts(&["Option", "::None"]), 0)
    };
    Ok(heap.allocate_enum(option.type_id, variant, tag, payload)?)
}

fn scalar_range(
    value: &str,
    start_scalar: usize,
    end_scalar: usize,
) -> Option<std::ops::Range<usize>> {
    debug_assert!(start_scalar <= end_scalar);
    let mut scalar = 0_usize;
    let mut start = (start_scalar == 0).then_some(0);
    let mut end = (end_scalar == 0).then_some(0);
    for (byte, _) in value.char_indices() {
        if scalar == start_scalar {
            start = Some(byte);
        }
        if scalar == end_scalar {
            end = Some(byte);
            break;
        }
        scalar += 1;
    }
    if start.is_none() && scalar == start_scalar {
        start = Some(value.len());
    }
    if end.is_none() && scalar == end_scalar {
        end = Some(value.len());
    }
    Some(start?..end?)
}

fn string_length_to_i32(length: usize) -> Result<i32, InterpreterError> {
    i32::try_from(length).map_err(|_| InterpreterError::StringLengthOverflow)
}

fn settle_terminal_cost(
    fuel: &mut FuelState,
    charge: &mut ExecutionCharge,
    pending: u64,
) -> Result<(), InterpreterError> {
    let cumulative_after = fuel
        .cumulative_used
        .checked_add(pending)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    if pending > fuel.slice_remaining || cumulative_after > fuel.cumulative_limit {
        return Err(InterpreterError::ContinuationLimit(
            FrameError::FrameByteLimit,
        ));
    }
    fuel.slice_remaining -= pending;
    fuel.cumulative_used = cumulative_after;
    charge.fuel_used = charge
        .fuel_used
        .checked_add(pending)
        .ok_or(InterpreterError::FuelCostOverflow)?;
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

fn ensure_host_call_available(migration_active: bool) -> Result<(), InterpreterError> {
    if migration_active {
        Err(InterpreterError::HostUnavailable)
    } else {
        Ok(())
    }
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

#[allow(clippy::float_cmp)]
fn runtime_values_equal(lhs: RuntimeValue, rhs: RuntimeValue) -> bool {
    match (lhs, rhs) {
        (RuntimeValue::F32(lhs), RuntimeValue::F32(rhs)) => {
            f32::from_bits(lhs) == f32::from_bits(rhs)
        }
        (RuntimeValue::F64(lhs), RuntimeValue::F64(rhs)) => {
            f64::from_bits(lhs) == f64::from_bits(rhs)
        }
        _ => lhs == rhs,
    }
}

fn runtime_value_type(value: RuntimeValue) -> Option<ValueType> {
    match value {
        RuntimeValue::I32(_) => Some(ValueType::I32),
        RuntimeValue::I64(_) => Some(ValueType::I64),
        RuntimeValue::F32(_) => Some(ValueType::F32),
        RuntimeValue::F64(_) => Some(ValueType::F64),
        RuntimeValue::Bool(_) => Some(ValueType::Bool),
        RuntimeValue::Rune(_) => Some(ValueType::Rune),
        RuntimeValue::String { .. } => Some(ValueType::String),
        RuntimeValue::Struct { type_id, .. } => Some(ValueType::Named(type_id)),
        RuntimeValue::Ref(_) => Some(ValueType::Ref),
        RuntimeValue::NamedRef { type_id, .. } | RuntimeValue::Opaque { type_id, .. } => {
            Some(ValueType::Named(type_id))
        }
        RuntimeValue::MigrationOldObject(object) => Some(ValueType::Named(object.parts().2)),
        RuntimeValue::MigrationStagingObject(object) => Some(ValueType::Named(object.parts().2)),
        RuntimeValue::StateHandle { handle_type, .. } => Some(ValueType::Named(handle_type)),
        RuntimeValue::HostRequest(_) => Some(ValueType::Named(nexa_core::StableId::from_name(
            "HostRequest",
        ))),
        RuntimeValue::ResourceToken(token) => Some(ValueType::Named(token.token_type())),
        RuntimeValue::Snapshot(snapshot) => Some(ValueType::Named(snapshot.type_id())),
        RuntimeValue::Unit => None,
    }
}

fn runtime_state_handle(
    value: RuntimeValue,
    target: ValueType,
) -> Result<crate::StateHandle, InterpreterError> {
    let RuntimeValue::StateHandle {
        handle_type,
        domain,
        stable_id,
        generation,
    } = value
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    if handle_type != nexa_bytecode::state_handle_type(target) {
        return Err(InterpreterError::TypeMismatch);
    }
    Ok(crate::StateHandle {
        domain: crate::StatefulDomainId::new(domain),
        stable_id,
        generation,
    })
}

const fn state_handle_error_tag(error: crate::StateHandleError) -> u32 {
    match error {
        crate::StateHandleError::WrongDomain => 0,
        crate::StateHandleError::Missing => 1,
        crate::StateHandleError::StaleGeneration => 2,
        crate::StateHandleError::GenerationExhausted => 3,
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
        | Instruction::StructGet { .. }
        | Instruction::StructWith { .. }
        | Instruction::StructEqual { .. }
        | Instruction::ClassNew { .. }
        | Instruction::ClassGet { .. }
        | Instruction::ClassSet { .. }
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
        | Instruction::Call { .. }
        | Instruction::HostCall { .. }
        | Instruction::StateCurrentGet { .. }
        | Instruction::StateHandleResolve { .. }
        | Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn
        | Instruction::Trap => true,
        Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. } => target <= pc,
        _ => false,
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpcodeCostScheduleEntry {
    index: usize,
    name: &'static str,
    base_cost: u16,
    intrinsic_scaled: bool,
    dynamic_work: &'static str,
}

#[allow(unused_macros)]
macro_rules! opcode_dynamic_work {
    () => {
        "fixed"
    };
    ($dynamic_work:literal) => {
        $dynamic_work
    };
}

#[allow(unused_macros)]
macro_rules! opcode_intrinsic_scaled {
    () => {
        false
    };
    ($intrinsic_scaled:literal) => {
        $intrinsic_scaled
    };
}

macro_rules! define_opcode_cost_schedule {
    (
        $(
            $pattern:pat => {
                index: $index:literal,
                name: $name:literal,
                base_cost: $base_cost:literal
                $(, intrinsic_scaled: $intrinsic_scaled:literal)?
                $(, dynamic_work: $dynamic_work:literal)?
            }
        ),+ $(,)?
    ) => {
        const DEFAULT_OPCODE_COSTS: [u16; 107] = [$($base_cost),+];

        /// Stable opcode display names indexed by `opcode_index` (WP15).
        pub(crate) const OPCODE_NAMES: [&str; 107] = [$($name),+];

        #[cfg(test)]
        const OPCODE_COST_SCHEDULE: [OpcodeCostScheduleEntry; 107] = [
            $(
                OpcodeCostScheduleEntry {
                    index: $index,
                    name: $name,
                    base_cost: $base_cost,
                    intrinsic_scaled: opcode_intrinsic_scaled!($($intrinsic_scaled)?),
                    dynamic_work: opcode_dynamic_work!($($dynamic_work)?),
                },
            )+
        ];

        #[allow(clippy::too_many_lines)]
        fn opcode_index(instruction: Instruction) -> usize {
            match instruction {
                $($pattern => $index,)+
            }
        }
    };
}

define_opcode_cost_schedule! {
    Instruction::LoadI32 { .. } => { index: 0, name: "LoadI32", base_cost: 1 },
    Instruction::LoadBool { .. } => { index: 1, name: "LoadBool", base_cost: 1 },
    Instruction::Move { .. } => { index: 2, name: "Move", base_cost: 1 },
    Instruction::Add { .. } => { index: 3, name: "Add", base_cost: 1 },
    Instruction::Sub { .. } => { index: 4, name: "Sub", base_cost: 1 },
    Instruction::Mul { .. } => { index: 5, name: "Mul", base_cost: 1 },
    Instruction::CompareEq { .. } => { index: 6, name: "CompareEq", base_cost: 1 },
    Instruction::Jump { .. } => { index: 7, name: "Jump", base_cost: 1 },
    Instruction::JumpIfFalse { .. } => { index: 8, name: "JumpIfFalse", base_cost: 1 },
    Instruction::Call { .. } => {
        index: 9,
        name: "Call",
        base_cost: 1,
        dynamic_work: "ceil((callee_registers+2*args_count)/8)"
    },
    Instruction::Return { .. } => {
        index: 10,
        name: "Return",
        base_cost: 1,
        dynamic_work: "next_defer?(call:ceil((cleanup_registers+args_count)/8),other:1):0"
    },
    Instruction::ReturnVoid => {
        index: 11,
        name: "ReturnVoid",
        base_cost: 1,
        dynamic_work: "next_defer?(call:ceil((cleanup_registers+args_count)/8),other:1):0"
    },
    Instruction::Safepoint => { index: 12, name: "Safepoint", base_cost: 1 },
    Instruction::Yield => { index: 13, name: "Yield", base_cost: 1 },
    Instruction::Trap => { index: 14, name: "Trap", base_cost: 1 },
    Instruction::DeferPush { .. } => { index: 15, name: "DeferPush", base_cost: 1 },
    Instruction::DeferPop => { index: 16, name: "DeferPop", base_cost: 1 },
    Instruction::CleanupReturn => {
        index: 17,
        name: "CleanupReturn",
        base_cost: 1,
        dynamic_work: "next_parent_defer?(call:ceil((cleanup_registers+args_count)/8),other:1):0"
    },
    Instruction::HostCall { .. } => { index: 18, name: "HostCall", base_cost: 1 },
    Instruction::StateOldGet { .. } => { index: 19, name: "StateOldGet", base_cost: 1 },
    Instruction::StateNewCreate { .. } => { index: 20, name: "StateNewCreate", base_cost: 1 },
    Instruction::StateNewSet { .. } => { index: 21, name: "StateNewSet", base_cost: 1 },
    Instruction::StateReplace { .. } => { index: 22, name: "StateReplace", base_cost: 1 },
    Instruction::StateDelete { .. } => { index: 23, name: "StateDelete", base_cost: 1 },
    Instruction::EnumNew { .. } => {
        index: 24,
        name: "EnumNew",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(enum_variant_index_entries)"
    },
    Instruction::EnumTag { .. } => { index: 25, name: "EnumTag", base_cost: 1 },
    Instruction::EnumPayload { .. } => { index: 26, name: "EnumPayload", base_cost: 1 },
    Instruction::StatePreserve { .. } => { index: 27, name: "StatePreserve", base_cost: 1 },
    Instruction::StateFinish => { index: 28, name: "StateFinish", base_cost: 1 },
    Instruction::StateOldFieldGet { .. } => { index: 29, name: "StateOldFieldGet", base_cost: 1 },
    Instruction::StateHandleResolve { .. } => { index: 30, name: "StateHandleResolve", base_cost: 1 },
    Instruction::StateHandleIsAlive { .. } => { index: 31, name: "StateHandleIsAlive", base_cost: 1 },
    Instruction::StateHandleStableId { .. } => { index: 32, name: "StateHandleStableId", base_cost: 1 },
    Instruction::StateHandleGeneration { .. } => { index: 33, name: "StateHandleGeneration", base_cost: 1 },
    Instruction::StateHandleEqual { .. } => { index: 34, name: "StateHandleEqual", base_cost: 1 },
    Instruction::StateHandleHash { .. } => { index: 35, name: "StateHandleHash", base_cost: 1 },
    Instruction::LoadI64 { .. } => { index: 36, name: "LoadI64", base_cost: 1 },
    Instruction::LoadF32 { .. } => { index: 37, name: "LoadF32", base_cost: 1 },
    Instruction::LoadF64 { .. } => { index: 38, name: "LoadF64", base_cost: 1 },
    Instruction::LoadRune { .. } => { index: 39, name: "LoadRune", base_cost: 1 },
    Instruction::AddI64 { .. } => { index: 40, name: "AddI64", base_cost: 1 },
    Instruction::SubI64 { .. } => { index: 41, name: "SubI64", base_cost: 1 },
    Instruction::MulI64 { .. } => { index: 42, name: "MulI64", base_cost: 1 },
    Instruction::DivI64 { .. } => { index: 43, name: "DivI64", base_cost: 1 },
    Instruction::Div { .. } => { index: 44, name: "Div", base_cost: 1 },
    Instruction::AddF32 { .. } => { index: 45, name: "AddF32", base_cost: 1 },
    Instruction::SubF32 { .. } => { index: 46, name: "SubF32", base_cost: 1 },
    Instruction::MulF32 { .. } => { index: 47, name: "MulF32", base_cost: 1 },
    Instruction::DivF32 { .. } => { index: 48, name: "DivF32", base_cost: 1 },
    Instruction::AddF64 { .. } => { index: 49, name: "AddF64", base_cost: 1 },
    Instruction::SubF64 { .. } => { index: 50, name: "SubF64", base_cost: 1 },
    Instruction::MulF64 { .. } => { index: 51, name: "MulF64", base_cost: 1 },
    Instruction::DivF64 { .. } => { index: 52, name: "DivF64", base_cost: 1 },
    Instruction::LoadString { .. } => {
        index: 53,
        name: "LoadString",
        base_cost: 1,
        dynamic_work: "ceil(pool_bytes/32)*2"
    },
    Instruction::StringLen { .. } => {
        index: 54,
        name: "StringLen",
        base_cost: 1,
        dynamic_work: "ceil(source_bytes/32)"
    },
    Instruction::StringByteLen { .. } => { index: 55, name: "StringByteLen", base_cost: 1 },
    Instruction::StringEqual { .. } => {
        index: 56,
        name: "StringEqual",
        base_cost: 1,
        dynamic_work: "ceil(min(lhs_bytes,rhs_bytes)/32)"
    },
    Instruction::StringConcat { .. } => {
        index: 57,
        name: "StringConcat",
        base_cost: 1,
        dynamic_work: "ceil((lhs_bytes+rhs_bytes)/32)*2"
    },
    Instruction::StringRuneAt { .. } => {
        index: 58,
        name: "StringRuneAt",
        base_cost: 1,
        dynamic_work: "ceil(source_bytes/32)"
    },
    Instruction::StringHash { .. } => { index: 59, name: "StringHash", base_cost: 1 },
    Instruction::StructNew { .. } => {
        index: 60,
        name: "StructNew",
        base_cost: 1,
        dynamic_work: "ceil(fields_count*3/8)+sum(field_recursive_hash_shape)"
    },
    Instruction::StructGet { .. } => {
        index: 61,
        name: "StructGet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(struct_field_index_entries)"
    },
    Instruction::StructWith { .. } => {
        index: 62,
        name: "StructWith",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(struct_field_index_entries)+ceil(existing_fields*3/8)+sum(existing_recursive_hash_shape)+ceil(1/8)+replacement_recursive_hash_shape"
    },
    Instruction::StructEqual { .. } => {
        index: 63,
        name: "StructEqual",
        base_cost: 1,
        dynamic_work: "lhs_structural_comparison_shape"
    },
    Instruction::ClassNew { .. } => {
        index: 64,
        name: "ClassNew",
        base_cost: 1,
        dynamic_work: "ceil(fields_count*2/8)"
    },
    Instruction::ClassGet { .. } => {
        index: 65,
        name: "ClassGet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(class_field_index_entries)"
    },
    Instruction::ClassSet { .. } => {
        index: 66,
        name: "ClassSet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(class_field_index_entries)"
    },
    Instruction::ClassEqual { .. } => { index: 67, name: "ClassEqual", base_cost: 1 },
    Instruction::ArrayNew { .. } => {
        index: 68,
        name: "ArrayNew",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(array_type_index_entries)"
    },
    Instruction::ArrayLen { .. } => { index: 69, name: "ArrayLen", base_cost: 1 },
    Instruction::ArrayGet { .. } => { index: 70, name: "ArrayGet", base_cost: 1 },
    Instruction::ArraySet { .. } => { index: 71, name: "ArraySet", base_cost: 1 },
    Instruction::ArrayPush { .. } => {
        index: 72,
        name: "ArrayPush",
        base_cost: 1,
        dynamic_work: "ceil((old_len+new_len)/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayPop { .. } => {
        index: 73,
        name: "ArrayPop",
        base_cost: 1,
        dynamic_work: "ceil((old_len+new_len)/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayInsert { .. } => {
        index: 74,
        name: "ArrayInsert",
        base_cost: 1,
        dynamic_work: "ceil((old_len+new_len)/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayRemove { .. } => {
        index: 75,
        name: "ArrayRemove",
        base_cost: 1,
        dynamic_work: "ceil((old_len+new_len)/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayClear { .. } => {
        index: 76,
        name: "ArrayClear",
        base_cost: 1,
        dynamic_work: "ceil(old_len/8)+collection_release_metadata"
    },
    Instruction::MapNew { .. } => {
        index: 77,
        name: "MapNew",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(map_type_index_entries)"
    },
    Instruction::MapLen { .. } => { index: 78, name: "MapLen", base_cost: 1 },
    Instruction::MapGet { .. } => {
        index: 79,
        name: "MapGet",
        base_cost: 1,
        dynamic_work: "map_lookup_slots+key_hash_and_comparison_shape"
    },
    Instruction::MapSet { .. } => {
        index: 80,
        name: "MapSet",
        base_cost: 1,
        dynamic_work: "map_insert_attempt_slots+key_hash_and_comparison_shape"
    },
    Instruction::MapRemove { .. } => {
        index: 81,
        name: "MapRemove",
        base_cost: 1,
        dynamic_work: "map_lookup_slots+key_hash_and_comparison_shape"
    },
    Instruction::MapContains { .. } => {
        index: 82,
        name: "MapContains",
        base_cost: 1,
        dynamic_work: "map_lookup_slots+key_hash_and_comparison_shape"
    },
    Instruction::MapClear { .. } => {
        index: 83,
        name: "MapClear",
        base_cost: 1,
        dynamic_work: "ceil((current_slots+old_slots+new_slots)/8)"
    },
    Instruction::BufferLen { .. } => { index: 84, name: "BufferLen", base_cost: 1 },
    Instruction::BufferGet { .. } => { index: 85, name: "BufferGet", base_cost: 1 },
    Instruction::BufferSet { .. } => { index: 86, name: "BufferSet", base_cost: 1 },
    Instruction::BufferSlice { .. } => {
        index: 87,
        name: "BufferSlice",
        base_cost: 1,
        dynamic_work: "ceil(requested_len/8)+collection_claim_metadata"
    },
    Instruction::BufferCopy { .. } => {
        index: 88,
        name: "BufferCopy",
        base_cost: 1,
        dynamic_work: "ceil(requested_len/8)"
    },
    Instruction::I32ToString { .. } => {
        index: 89,
        name: "I32ToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::I64ToString { .. } => {
        index: 90,
        name: "I64ToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::F32ToString { .. } => {
        index: 91,
        name: "F32ToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::F64ToString { .. } => {
        index: 92,
        name: "F64ToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::BoolToString { .. } => {
        index: 93,
        name: "BoolToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::RuneToString { .. } => {
        index: 94,
        name: "RuneToString",
        base_cost: 1,
        dynamic_work: "ceil(64/32)*3"
    },
    Instruction::CompareLtI32 { .. } => { index: 95, name: "CompareLtI32", base_cost: 1 },
    Instruction::CompareLtI64 { .. } => { index: 96, name: "CompareLtI64", base_cost: 1 },
    Instruction::CompareLtF32 { .. } => { index: 97, name: "CompareLtF32", base_cost: 1 },
    Instruction::CompareLtF64 { .. } => { index: 98, name: "CompareLtF64", base_cost: 1 },
    Instruction::StringToString { .. } => { index: 99, name: "StringToString", base_cost: 1 },
    Instruction::StandardIntrinsic { .. } => {
        index: 100,
        name: "StandardIntrinsic",
        base_cost: 1,
        intrinsic_scaled: true,
        dynamic_work: "intrinsic.fuel_model(args,heap)+collection_metadata"
    },
    Instruction::RemI32 { .. } => { index: 101, name: "RemI32", base_cost: 1 },
    Instruction::RemI64 { .. } => { index: 102, name: "RemI64", base_cost: 1 },
    Instruction::RemF32 { .. } => { index: 103, name: "RemF32", base_cost: 1 },
    Instruction::RemF64 { .. } => { index: 104, name: "RemF64", base_cost: 1 },
    Instruction::StateCurrentGet { .. } => { index: 105, name: "StateCurrentGet", base_cost: 1 },
    Instruction::EnumEqual { .. } => {
        index: 106,
        name: "EnumEqual",
        base_cost: 1,
        dynamic_work: "lhs_recursive_enum_comparison_shape"
    },
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use nexa_bytecode::{
        AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
        ModuleBuilder, RootMap, SCALAR_TO_STRING_FUEL_PASSES, SCALAR_TO_STRING_MAX_BYTES,
        STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS, STANDARD_STRING_FUEL_BLOCK_BYTES, Signature,
        SourceMapEntry, StandardIntrinsic, ValueType,
    };
    use nexa_core::{CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS, FileId, SourceSpan, StableId};
    use nexa_verifier::{VerifierLimits, verify};

    use super::{
        CheckedInterpreter, FuelState, InterpreterError, InterpreterMigration, InterpreterOutcome,
        OPCODE_COST_SCHEDULE, StandardIntrinsicOutcome, SuspendReason, allocate_runtime_string,
        ensure_host_call_available, fuel_add, fuel_blocks, run_standard_intrinsic,
    };
    use crate::{
        ContinuationReservation, FrameError, FrameLimits, GcRoots, Heap, HeapError, MapSetOutcome,
        Object, OpcodeCostTable, RuntimeValue,
    };

    #[test]
    fn bytecode_v6_opcode_cost_schedule_matches_the_frozen_fixture() {
        assert_eq!(nexa_bytecode::BYTECODE_VERSION, 6);
        assert_eq!(OPCODE_COST_SCHEDULE.len(), 107);
        assert_eq!(STANDARD_STRING_FUEL_BLOCK_BYTES, 32);
        assert_eq!(STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS, 8);
        assert_eq!(SCALAR_TO_STRING_MAX_BYTES, 64);
        assert_eq!(SCALAR_TO_STRING_FUEL_PASSES, 3);

        let table = OpcodeCostTable::default();
        let mut rendered = String::new();
        writeln!(
            rendered,
            "bytecode_version={}",
            nexa_bytecode::BYTECODE_VERSION
        )
        .unwrap();
        writeln!(rendered, "opcode_cost_table_version={}", table.version).unwrap();
        writeln!(rendered, "entries={}", OPCODE_COST_SCHEDULE.len()).unwrap();
        writeln!(
            rendered,
            "index\tname\ttable_base\teffective_base\tdynamic_work"
        )
        .unwrap();
        for (expected_index, entry) in OPCODE_COST_SCHEDULE.iter().enumerate() {
            assert_eq!(entry.index, expected_index);
            assert_eq!(table.costs[entry.index], entry.base_cost);
            let effective = if entry.intrinsic_scaled {
                "intrinsic.base_fuel_cost"
            } else {
                "fixed"
            };
            writeln!(
                rendered,
                "{:03}\t{}\t{}\t{effective}\t{}",
                entry.index, entry.name, entry.base_cost, entry.dynamic_work
            )
            .unwrap();
        }
        writeln!(rendered, "runtime_transitions=1").unwrap();
        writeln!(
            rendered,
            "run_cleanup_initial_defer\tsum_until_call(call:ceil((cleanup_registers+args_count)/8),other:1)"
        )
        .unwrap();
        assert_eq!(
            rendered,
            include_str!("../fixtures/opcode-cost-table-v6.txt")
        );

        let mut mismatched = table;
        mismatched.version = mismatched.version.saturating_sub(1);
        assert_eq!(
            mismatched.validate_version(),
            Err(InterpreterError::OpcodeCostTableVersion {
                expected: nexa_core::OPCODE_COST_TABLE_VERSION,
                actual: mismatched.version,
            })
        );
    }

    #[test]
    fn migration_execution_rejects_host_calls_defensively() {
        assert_eq!(
            ensure_host_call_available(true),
            Err(super::InterpreterError::HostUnavailable)
        );
        assert_eq!(ensure_host_call_available(false), Ok(()));
    }

    #[test]
    fn async_host_pending_uses_pre_call_root_map_until_resume_value_is_written() {
        let result = nexa_bytecode::result_type(ValueType::Ref, ValueType::I32);
        let result_type = result.type_id;
        let signature = Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(result_type)),
        };
        let async_result = AsyncResultType {
            result_type,
            success: ValueType::Ref,
            error: ValueType::I32,
            cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
            cancel_error: Some(1),
            abandon_error: None,
        };
        let mut function = FunctionBuilder::new(signature, 1);
        function
            .effect(FunctionEffect::Task)
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut function = function.finish().unwrap();
        function.root_bitmap = vec![true];
        function.safepoints = vec![0, 1];
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true],
            },
        ];

        let host_contract = StableId::from_name("test::async-root-host");
        let mut module = ModuleBuilder::new();
        module
            .metadata(
                host_contract,
                nexa_bytecode::StateSchema::default().fingerprint(),
            )
            .enum_type(result);
        module.host_import(HostImport {
            stable_id: StableId::from_name("test::async-root-host::request"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::Named(result_type)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(async_result),
        });
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();

        let limits = FrameLimits::default();
        let mut continuation = CheckedInterpreter::start(
            &module,
            0,
            &[],
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .unwrap();
        continuation.host_call_boundary = Some(super::HostCallBoundary {
            import: 0,
            function: 0,
            pc: 0,
            source_span: None,
        });

        assert!(
            continuation.checked_gc_roots(&module).unwrap().is_empty(),
            "the pending destination is still Unit and must use the pre-call root map"
        );

        let mut heap = Heap::new(1);
        let reference = heap.allocate(Object::String("host result".into())).unwrap();
        continuation
            .write_resume_value(
                0,
                Some(ValueType::Named(result_type)),
                RuntimeValue::NamedRef {
                    type_id: result_type,
                    reference,
                },
            )
            .unwrap();

        assert_eq!(
            continuation.checked_gc_roots(&module).unwrap(),
            vec![reference],
            "the resumed destination becomes live only after the Host result is written"
        );
    }

    #[derive(Default)]
    struct DepthMigration {
        max_depth: usize,
    }

    impl InterpreterMigration for DepthMigration {
        fn observe_call_depth(&mut self, depth: usize) {
            self.max_depth = self.max_depth.max(depth);
        }

        fn old_get(
            &mut self,
            _stable_id: StableId,
            _expected: ValueType,
        ) -> Result<RuntimeValue, crate::RuntimeMessage> {
            Err("unexpected migration old_get".into())
        }

        fn old_field_get(
            &mut self,
            _object: RuntimeValue,
            _field_id: StableId,
            _expected: ValueType,
        ) -> Result<RuntimeValue, crate::RuntimeMessage> {
            Err("unexpected migration old_field_get".into())
        }

        fn new_create(
            &mut self,
            _stable_id: StableId,
            _type_id: StableId,
        ) -> Result<RuntimeValue, crate::RuntimeMessage> {
            Err("unexpected migration new_create".into())
        }

        fn new_set(
            &mut self,
            _object: RuntimeValue,
            _field_id: StableId,
            _value: RuntimeValue,
        ) -> Result<(), crate::RuntimeMessage> {
            Err("unexpected migration new_set".into())
        }

        fn preserve(&mut self, _stable_id: StableId) -> Result<(), crate::RuntimeMessage> {
            Err("unexpected migration preserve".into())
        }

        fn replace(
            &mut self,
            _old_id: StableId,
            _target: RuntimeValue,
        ) -> Result<(), crate::RuntimeMessage> {
            Err("unexpected migration replace".into())
        }

        fn delete(&mut self, _stable_id: StableId) -> Result<(), crate::RuntimeMessage> {
            Err("unexpected migration delete".into())
        }

        fn finish_staging(&mut self) -> Result<(), crate::RuntimeMessage> {
            Ok(())
        }
    }

    fn nested_defer_module() -> nexa_verifier::VerifiedModule {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::DeferPush {
                function: 1,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);

        let mut direct_cleanup = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        direct_cleanup
            .emit(Instruction::DeferPush {
                function: 2,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::ReturnVoid);

        let mut nested_cleanup = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        nested_cleanup
            .effect(FunctionEffect::Cleanup)
            .emit(Instruction::CleanupReturn);

        let mut module = ModuleBuilder::new();
        module.function(migration.finish().unwrap());
        module.function(direct_cleanup.finish().unwrap());
        module.function(nested_cleanup.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn poll_with_call_depth(
        module: &nexa_verifier::VerifiedModule,
        function: u32,
        max_call_depth: u32,
    ) -> Result<InterpreterOutcome, super::InterpreterError> {
        let limits = FrameLimits {
            max_call_depth,
            ..FrameLimits::default()
        };
        let continuation = CheckedInterpreter::start(
            module,
            function,
            &[],
            limits,
            ContinuationReservation::for_limits(limits),
        )?;
        CheckedInterpreter::poll(
            module,
            continuation,
            FuelState::new(64, 0, u64::MAX),
            &OpcodeCostTable::default(),
        )
    }

    #[test]
    fn direct_and_nested_defer_execution_honor_exact_call_depth_limits() {
        let module = nested_defer_module();
        assert_eq!(module.module().functions[0].max_static_call_depth, 3);
        assert_eq!(module.module().functions[1].max_static_call_depth, 2);
        assert_eq!(module.module().functions[2].max_static_call_depth, 1);

        let InterpreterOutcome::Returned { charge, .. } =
            poll_with_call_depth(&module, 1, 2).unwrap()
        else {
            panic!("the exact direct-defer depth must complete");
        };
        assert_eq!(charge.instructions, 4);
        assert!(matches!(
            poll_with_call_depth(&module, 1, 1),
            Err(super::InterpreterError::ContinuationLimit(
                FrameError::CallDepthLimit
            ))
        ));

        let limits = FrameLimits {
            max_call_depth: 3,
            ..FrameLimits::default()
        };
        let mut migration = DepthMigration::default();
        let InterpreterOutcome::Returned { charge, .. } =
            CheckedInterpreter::run_migration(&module, 0, &[], 64, limits, &mut migration).unwrap()
        else {
            panic!("the exact nested-defer depth must complete");
        };
        assert_eq!(charge.instructions, 8);
        assert_eq!(migration.max_depth, 3);

        let mut migration = DepthMigration::default();
        assert!(matches!(
            CheckedInterpreter::run_migration(
                &module,
                0,
                &[],
                64,
                FrameLimits {
                    max_call_depth: 2,
                    ..FrameLimits::default()
                },
                &mut migration,
            ),
            Err(super::InterpreterError::ContinuationLimit(
                FrameError::CallDepthLimit
            ))
        ));
    }

    fn evaluate_float_binary(
        ty: ValueType,
        instruction: Instruction,
        lhs: RuntimeValue,
        rhs: RuntimeValue,
    ) -> RuntimeValue {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ty, ty],
                result: Some(ty),
            },
            3,
        );
        function
            .emit(instruction)
            .emit(Instruction::Return { source: 2 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let InterpreterOutcome::Returned {
            value: Some(value), ..
        } = CheckedInterpreter::run(&module, 0, &[lhs, rhs], 16).unwrap()
        else {
            panic!("binary float instruction must return a value");
        };
        value
    }

    fn evaluate_f32_binary(instruction: Instruction, lhs: u32, rhs: u32) -> u32 {
        let RuntimeValue::F32(bits) = evaluate_float_binary(
            ValueType::F32,
            instruction,
            RuntimeValue::F32(lhs),
            RuntimeValue::F32(rhs),
        ) else {
            panic!("f32 instruction must return f32");
        };
        bits
    }

    fn evaluate_f64_binary(instruction: Instruction, lhs: u64, rhs: u64) -> u64 {
        let RuntimeValue::F64(bits) = evaluate_float_binary(
            ValueType::F64,
            instruction,
            RuntimeValue::F64(lhs),
            RuntimeValue::F64(rhs),
        ) else {
            panic!("f64 instruction must return f64");
        };
        bits
    }

    #[test]
    fn trap_resolves_each_script_frame_through_the_source_map() {
        let mut trap_function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        trap_function.emit(Instruction::Trap);
        let mut entry_function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        entry_function
            .effect(FunctionEffect::Task)
            .emit(Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let caller_call = SourceSpan::new(FileId(7), 10, 19);
        let caller_return = SourceSpan::new(FileId(7), 20, 26);
        let callee_trap = SourceSpan::new(FileId(7), 40, 44);
        let mut module = ModuleBuilder::new();
        module.function(entry_function.finish().unwrap());
        module.function(trap_function.finish().unwrap());
        module.source_map([
            SourceMapEntry {
                function: 0,
                pc_start: 0,
                pc_end: 1,
                span: caller_call,
            },
            SourceMapEntry {
                function: 0,
                pc_start: 1,
                pc_end: 2,
                span: caller_return,
            },
            SourceMapEntry {
                function: 1,
                pc_start: 0,
                pc_end: 1,
                span: callee_trap,
            },
        ]);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();

        let InterpreterOutcome::Trapped { trap, .. } =
            CheckedInterpreter::run(&module, 0, &[], 32).unwrap()
        else {
            panic!("nested bytecode trap must produce a script stack");
        };

        assert_eq!(trap.function, 1);
        assert_eq!(trap.pc, 0);
        assert_eq!(trap.source_span, Some(callee_trap));
        assert_eq!(
            trap.script_call_stack.as_slice(),
            [
                super::ScriptFrame {
                    function: 1,
                    pc: 0,
                    call_site_pc: None,
                    source_span: Some(callee_trap),
                },
                super::ScriptFrame {
                    function: 0,
                    pc: 1,
                    call_site_pc: Some(0),
                    source_span: Some(caller_call),
                },
            ]
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scalar_to_string_is_locale_free_and_deterministic() {
        fn render(source_type: ValueType, instruction: Instruction, value: RuntimeValue) -> String {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type],
                    result: Some(ValueType::String),
                },
                2,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 1 });
            let mut function = function.finish().unwrap();
            function.root_bitmap[1] = true;
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: vec![false, false],
                },
                RootMap {
                    pc: 1,
                    bitmap: vec![false, true],
                },
            ];
            let mut module = ModuleBuilder::new();
            module.function(function);
            let module = verify(module.finish(), VerifierLimits::default()).unwrap();
            let mut heap = Heap::new_with_string_limit(2, 64);
            let InterpreterOutcome::Returned {
                value: Some(RuntimeValue::String { reference, .. }),
                ..
            } = CheckedInterpreter::run_with_heap(&module, 0, &[value], 16, &mut heap).unwrap()
            else {
                panic!("conversion must return a VM string");
            };
            heap.string(reference).unwrap().to_owned()
        }

        assert_eq!(
            render(
                ValueType::I32,
                Instruction::I32ToString { dst: 1, source: 0 },
                RuntimeValue::I32(-42),
            ),
            "-42"
        );
        assert_eq!(
            render(
                ValueType::I64,
                Instruction::I64ToString { dst: 1, source: 0 },
                RuntimeValue::I64(i64::MIN),
            ),
            "-9223372036854775808"
        );
        assert_eq!(
            render(
                ValueType::F32,
                Instruction::F32ToString { dst: 1, source: 0 },
                RuntimeValue::F32(7.5_f32.to_bits()),
            ),
            "7.5"
        );
        assert_eq!(
            render(
                ValueType::F64,
                Instruction::F64ToString { dst: 1, source: 0 },
                RuntimeValue::F64(f64::INFINITY.to_bits()),
            ),
            "inf"
        );
        assert_eq!(
            render(
                ValueType::Bool,
                Instruction::BoolToString { dst: 1, source: 0 },
                RuntimeValue::Bool(true),
            ),
            "true"
        );
        assert_eq!(
            render(
                ValueType::Rune,
                Instruction::RuneToString { dst: 1, source: 0 },
                RuntimeValue::Rune('界' as u32),
            ),
            "界"
        );

        let mut identity = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::String),
            },
            2,
        );
        identity
            .emit(Instruction::StringToString { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 });
        let mut identity = identity.finish().unwrap();
        identity.root_bitmap = vec![true, true];
        identity.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(identity);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new_with_string_limit(2, 64);
        let reference = heap.allocate_string("already a string").unwrap();
        let input = RuntimeValue::String {
            reference,
            hash: heap.string_hash(reference).unwrap(),
        };
        let InterpreterOutcome::Returned {
            value: Some(output),
            ..
        } = CheckedInterpreter::run_with_heap(&module, 0, &[input], 16, &mut heap).unwrap()
        else {
            panic!("string identity conversion must return a string");
        };
        assert_eq!(output, input);
    }

    #[test]
    fn numeric_ordering_executes_with_ieee_nan_semantics() {
        fn compare(
            source_type: ValueType,
            instruction: Instruction,
            lhs: RuntimeValue,
            rhs: RuntimeValue,
        ) -> bool {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type, source_type],
                    result: Some(ValueType::Bool),
                },
                3,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 2 });
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            let module = verify(module.finish(), VerifierLimits::default()).unwrap();
            let InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(value)),
                ..
            } = CheckedInterpreter::run(&module, 0, &[lhs, rhs], 16).unwrap()
            else {
                panic!("ordering must return bool");
            };
            value
        }

        assert!(compare(
            ValueType::I32,
            Instruction::CompareLtI32 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            RuntimeValue::I32(-1),
            RuntimeValue::I32(2),
        ));
        assert!(compare(
            ValueType::I64,
            Instruction::CompareLtI64 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            RuntimeValue::I64(i64::MIN),
            RuntimeValue::I64(i64::MAX),
        ));
        assert!(compare(
            ValueType::F32,
            Instruction::CompareLtF32 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            RuntimeValue::F32((-1.0_f32).to_bits()),
            RuntimeValue::F32(0.0_f32.to_bits()),
        ));
        assert!(!compare(
            ValueType::F64,
            Instruction::CompareLtF64 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            RuntimeValue::F64(f64::NAN.to_bits()),
            RuntimeValue::F64(1.0_f64.to_bits()),
        ));
    }

    #[test]
    fn scalar_float_equality_uses_ieee_semantics() {
        fn compare(source_type: ValueType, lhs: RuntimeValue, rhs: RuntimeValue) -> bool {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type, source_type],
                    result: Some(ValueType::Bool),
                },
                3,
            );
            function
                .emit(Instruction::CompareEq {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                })
                .emit(Instruction::Return { source: 2 });
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            let module = verify(module.finish(), VerifierLimits::default()).unwrap();
            let InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(value)),
                ..
            } = CheckedInterpreter::run(&module, 0, &[lhs, rhs], 16).unwrap()
            else {
                panic!("equality must return bool");
            };
            value
        }

        assert!(compare(
            ValueType::F32,
            RuntimeValue::F32(0.0_f32.to_bits()),
            RuntimeValue::F32((-0.0_f32).to_bits()),
        ));
        assert!(!compare(
            ValueType::F32,
            RuntimeValue::F32(CANONICAL_NAN_F32_BITS),
            RuntimeValue::F32(CANONICAL_NAN_F32_BITS),
        ));
        assert!(compare(
            ValueType::F64,
            RuntimeValue::F64(0.0_f64.to_bits()),
            RuntimeValue::F64((-0.0_f64).to_bits()),
        ));
        assert!(!compare(
            ValueType::F64,
            RuntimeValue::F64(CANONICAL_NAN_F64_BITS),
            RuntimeValue::F64(CANONICAL_NAN_F64_BITS),
        ));

        let class_type = StableId::from_name("ClassIdentity");
        let first = RuntimeValue::NamedRef {
            reference: crate::GcRef {
                index: 1,
                generation: 1,
            },
            type_id: class_type,
        };
        let second = RuntimeValue::NamedRef {
            reference: crate::GcRef {
                index: 2,
                generation: 1,
            },
            type_id: class_type,
        };
        assert!(super::runtime_values_equal(first, first));
        assert!(!super::runtime_values_equal(first, second));
    }

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
    #[allow(clippy::similar_names)]
    fn call_drops_dead_pre_call_reference_before_scalar_result_returns() {
        let mut caller = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        caller
            .effect(FunctionEffect::Task)
            .set_root(0)
            .unwrap()
            .emit(Instruction::LoadString { dst: 0, string: 0 })
            .emit(Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut caller = caller.finish().unwrap();
        caller.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false],
            },
            RootMap {
                pc: 2,
                bitmap: vec![false],
            },
        ];

        let mut callee = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        callee
            .effect(FunctionEffect::Task)
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });

        let mut module = ModuleBuilder::new();
        module.string("stale");
        module.function(caller);
        module.function(callee.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new(4);
        let InterpreterOutcome::Suspended { continuation, .. } =
            CheckedInterpreter::run_with_heap(&module, 0, &[], 32, &mut heap).unwrap()
        else {
            panic!("callee must yield");
        };
        let roots = continuation.checked_gc_roots(&module).unwrap();
        assert!(roots.is_empty());
        let collection = heap
            .collect(&GcRoots {
                suspended_tasks: roots,
                ..GcRoots::default()
            })
            .unwrap();
        assert_eq!(collection.reclaimed, 1);
        assert_eq!(collection.live, 0);
        assert!(matches!(
            CheckedInterpreter::poll_with_heap(
                &module,
                continuation,
                FuelState::new(8, 0, u64::MAX),
                &OpcodeCostTable::default(),
                &mut heap,
            )
            .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(7)),
                ..
            }
        ));
        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 0);
    }

    #[test]
    fn standard_intrinsic_allocation_is_rooted_across_suspension() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::String),
            },
            2,
        );
        function.effect(FunctionEffect::Task);
        function.set_root(0).unwrap();
        function.set_root(1).unwrap();
        function
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::StringTrim,
                args_base: 0,
                args_count: 1,
                dst: 1,
            })
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 1 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
            RootMap {
                pc: 2,
                bitmap: vec![false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new(8);
        let input = allocate_runtime_string(&mut heap, "  rooted 😀  ").unwrap();

        let InterpreterOutcome::Suspended {
            continuation, fuel, ..
        } = CheckedInterpreter::run_with_heap(&module, 0, &[input], 32, &mut heap).unwrap()
        else {
            panic!("task must suspend after allocating the trimmed string");
        };
        let RuntimeValue::String {
            reference: trimmed, ..
        } = continuation.arena().frame_register(0, 1).unwrap()
        else {
            panic!("standard intrinsic must write a string result");
        };
        let roots = GcRoots {
            suspended_tasks: continuation.checked_gc_roots(&module).unwrap(),
            ..GcRoots::default()
        };
        let collection = heap.collect(&roots).unwrap();
        assert_eq!(collection.live, 1);
        assert_eq!(collection.reclaimed, 1);
        assert_eq!(heap.string(trimmed).unwrap(), "rooted 😀");

        assert!(matches!(
            CheckedInterpreter::poll_with_heap(
                &module,
                continuation,
                fuel,
                &OpcodeCostTable::default(),
                &mut heap,
            )
            .unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::String { reference, .. }),
                ..
            } if reference == trimmed
        ));
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
                bitmap: vec![false, false, false],
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

    #[allow(clippy::needless_pass_by_value)]
    fn returned_value(outcome: StandardIntrinsicOutcome) -> RuntimeValue {
        match outcome {
            StandardIntrinsicOutcome::Returned(value) => value,
            StandardIntrinsicOutcome::Retry => panic!("unexpected retry"),
            StandardIntrinsicOutcome::Trapped(message) => panic!("unexpected trap: {message}"),
        }
    }

    #[test]
    fn standard_core_option_and_result_intrinsics_execute() {
        let mut heap = Heap::new(16);
        let option = nexa_bytecode::option_type(ValueType::I32);
        let some = heap
            .allocate_enum(
                option.type_id,
                StableId::from_parts(&["Option", "::Some"]),
                1,
                Some(RuntimeValue::I32(7)),
            )
            .unwrap();
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::OptionIsSome {
                        value: ValueType::I32
                    },
                    &[some],
                    Some(&mut heap),
                )
                .unwrap()
            ),
            RuntimeValue::Bool(true)
        );
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::OptionUnwrapOr {
                        value: ValueType::I32
                    },
                    &[some, RuntimeValue::I32(9)],
                    Some(&mut heap),
                )
                .unwrap()
            ),
            RuntimeValue::I32(7)
        );

        let result = nexa_bytecode::result_type(ValueType::I32, ValueType::String);
        let error = allocate_runtime_string(&mut heap, "nope").unwrap();
        let err = heap
            .allocate_enum(
                result.type_id,
                StableId::from_parts(&["Result", "::Err"]),
                1,
                Some(error),
            )
            .unwrap();
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::ResultIsErr {
                        success: ValueType::I32,
                        error: ValueType::String,
                    },
                    &[err],
                    Some(&mut heap),
                )
                .unwrap()
            ),
            RuntimeValue::Bool(true)
        );
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::ResultUnwrapOr {
                        success: ValueType::I32,
                        error: ValueType::String,
                    },
                    &[err, RuntimeValue::I32(11)],
                    Some(&mut heap),
                )
                .unwrap()
            ),
            RuntimeValue::I32(11)
        );
    }

    #[test]
    #[allow(clippy::float_cmp, clippy::too_many_lines)]
    fn standard_math_is_platform_independent_and_preserves_special_values() {
        let evaluate_f64 = |intrinsic, value: f64| {
            let outcome =
                run_standard_intrinsic(intrinsic, &[RuntimeValue::F64(value.to_bits())], None)
                    .unwrap();
            let RuntimeValue::F64(bits) = returned_value(outcome) else {
                panic!("expected f64")
            };
            f64::from_bits(bits)
        };
        let evaluate_f32 = |intrinsic, value: f32| {
            let outcome =
                run_standard_intrinsic(intrinsic, &[RuntimeValue::F32(value.to_bits())], None)
                    .unwrap();
            let RuntimeValue::F32(bits) = returned_value(outcome) else {
                panic!("expected f32")
            };
            f32::from_bits(bits)
        };
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Floor, -0.0).to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(evaluate_f64(StandardIntrinsic::F64Round, -1.5), -2.0);
        assert_eq!(
            evaluate_f64(
                StandardIntrinsic::F64Round,
                f64::from_bits(0x3fdf_ffff_ffff_ffff),
            )
            .to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(
            evaluate_f64(
                StandardIntrinsic::F64Round,
                -f64::from_bits(0x3fdf_ffff_ffff_ffff),
            )
            .to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Round, f32::from_bits(0x3eff_ffff)).to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(evaluate_f64(StandardIntrinsic::F64Sqrt, 4.0), 2.0);
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sqrt, -1.0).to_bits(),
            0x7ff8_0000_0000_0000
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sqrt, f64::from_bits(1)).to_bits(),
            486_u64 << 52
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Sqrt, f32::from_bits(2)).to_bits(),
            53_u32 << 23
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sin, 1.0e300).to_bits(),
            0xbfea_2c16_b010_e385
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Cos, 1.0e300).to_bits(),
            0xbfe2_6990_22ad_c4c1
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Sin, 1.0e30).to_bits(),
            0xbf4a_89b0
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Cos, 1.0e30).to_bits(),
            0xbf1c_9222
        );
        for intrinsic in [
            StandardIntrinsic::F64Floor,
            StandardIntrinsic::F64Ceil,
            StandardIntrinsic::F64Round,
            StandardIntrinsic::F64Sqrt,
            StandardIntrinsic::F64Sin,
            StandardIntrinsic::F64Cos,
        ] {
            assert_eq!(
                evaluate_f64(intrinsic, f64::from_bits(0x7ff0_0000_0000_0001)).to_bits(),
                0x7ff8_0000_0000_0000
            );
        }
        for intrinsic in [
            StandardIntrinsic::F32Floor,
            StandardIntrinsic::F32Ceil,
            StandardIntrinsic::F32Round,
            StandardIntrinsic::F32Sqrt,
            StandardIntrinsic::F32Sin,
            StandardIntrinsic::F32Cos,
        ] {
            assert_eq!(
                evaluate_f32(intrinsic, f32::from_bits(0x7f80_0001)).to_bits(),
                0x7fc0_0000
            );
        }
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sqrt, f64::INFINITY).to_bits(),
            f64::INFINITY.to_bits()
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sqrt, f64::NEG_INFINITY).to_bits(),
            0x7ff8_0000_0000_0000
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sin, f64::INFINITY).to_bits(),
            0x7ff8_0000_0000_0000
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Cos, f32::NEG_INFINITY).to_bits(),
            0x7fc0_0000
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sin, -0.0).to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Cos, -0.0).to_bits(),
            1.0_f64.to_bits()
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Sin, -0.0).to_bits(),
            (-0.0_f32).to_bits()
        );
        assert_eq!(
            evaluate_f32(StandardIntrinsic::F32Cos, -0.0).to_bits(),
            1.0_f32.to_bits()
        );
        assert_eq!(
            evaluate_f64(StandardIntrinsic::F64Sin, 0.75).to_bits(),
            evaluate_f64(StandardIntrinsic::F64Sin, 0.75).to_bits()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn f32_arithmetic_opcodes_canonicalize_nan_and_preserve_ieee_boundaries() {
        let signaling_nan = 0x7f80_0001;
        for (instruction, lhs, rhs) in [
            (
                Instruction::AddF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f32.to_bits(),
            ),
            (
                Instruction::AddF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                f32::INFINITY.to_bits(),
                f32::NEG_INFINITY.to_bits(),
            ),
            (
                Instruction::SubF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f32.to_bits(),
            ),
            (
                Instruction::SubF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                f32::INFINITY.to_bits(),
                f32::INFINITY.to_bits(),
            ),
            (
                Instruction::MulF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f32.to_bits(),
            ),
            (
                Instruction::MulF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                0.0_f32.to_bits(),
                f32::INFINITY.to_bits(),
            ),
            (
                Instruction::DivF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f32.to_bits(),
            ),
            (
                Instruction::DivF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                0.0_f32.to_bits(),
                0.0_f32.to_bits(),
            ),
        ] {
            assert_eq!(
                evaluate_f32_binary(instruction, lhs, rhs),
                CANONICAL_NAN_F32_BITS
            );
        }

        for (instruction, lhs, rhs, expected) in [
            (
                Instruction::AddF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f32).to_bits(),
                (-0.0_f32).to_bits(),
                (-0.0_f32).to_bits(),
            ),
            (
                Instruction::SubF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f32).to_bits(),
                0.0_f32.to_bits(),
                (-0.0_f32).to_bits(),
            ),
            (
                Instruction::MulF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f32).to_bits(),
                2.0_f32.to_bits(),
                (-0.0_f32).to_bits(),
            ),
            (
                Instruction::DivF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-1.0_f32).to_bits(),
                f32::INFINITY.to_bits(),
                (-0.0_f32).to_bits(),
            ),
        ] {
            assert_eq!(evaluate_f32_binary(instruction, lhs, rhs), expected);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn f64_arithmetic_opcodes_canonicalize_nan_and_preserve_ieee_boundaries() {
        let signaling_nan = 0x7ff0_0000_0000_0001;
        for (instruction, lhs, rhs) in [
            (
                Instruction::AddF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f64.to_bits(),
            ),
            (
                Instruction::AddF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                f64::INFINITY.to_bits(),
                f64::NEG_INFINITY.to_bits(),
            ),
            (
                Instruction::SubF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f64.to_bits(),
            ),
            (
                Instruction::SubF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                f64::INFINITY.to_bits(),
                f64::INFINITY.to_bits(),
            ),
            (
                Instruction::MulF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f64.to_bits(),
            ),
            (
                Instruction::MulF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                0.0_f64.to_bits(),
                f64::INFINITY.to_bits(),
            ),
            (
                Instruction::DivF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                signaling_nan,
                1.0_f64.to_bits(),
            ),
            (
                Instruction::DivF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                0.0_f64.to_bits(),
                0.0_f64.to_bits(),
            ),
        ] {
            assert_eq!(
                evaluate_f64_binary(instruction, lhs, rhs),
                CANONICAL_NAN_F64_BITS
            );
        }

        for (instruction, lhs, rhs, expected) in [
            (
                Instruction::AddF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f64).to_bits(),
                (-0.0_f64).to_bits(),
                (-0.0_f64).to_bits(),
            ),
            (
                Instruction::SubF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f64).to_bits(),
                0.0_f64.to_bits(),
                (-0.0_f64).to_bits(),
            ),
            (
                Instruction::MulF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-0.0_f64).to_bits(),
                2.0_f64.to_bits(),
                (-0.0_f64).to_bits(),
            ),
            (
                Instruction::DivF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                (-1.0_f64).to_bits(),
                f64::INFINITY.to_bits(),
                (-0.0_f64).to_bits(),
            ),
        ] {
            assert_eq!(evaluate_f64_binary(instruction, lhs, rhs), expected);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn typed_remainder_is_deterministic_and_traps_integer_zero() {
        fn evaluate(
            ty: ValueType,
            instruction: Instruction,
            arguments: &[RuntimeValue],
        ) -> InterpreterOutcome {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![ty, ty],
                    result: Some(ty),
                },
                3,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 2 });
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            let module = verify(module.finish(), VerifierLimits::default()).unwrap();
            CheckedInterpreter::run(&module, 0, arguments, 16).unwrap()
        }

        assert!(matches!(
            evaluate(
                ValueType::I32,
                Instruction::RemI32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                &[RuntimeValue::I32(7), RuntimeValue::I32(3)],
            ),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(1)),
                ..
            }
        ));
        assert!(matches!(
            evaluate(
                ValueType::I64,
                Instruction::RemI64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                &[RuntimeValue::I64(i64::MIN), RuntimeValue::I64(-1)],
            ),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I64(0)),
                ..
            }
        ));
        let integer_zero = evaluate(
            ValueType::I32,
            Instruction::RemI32 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            &[RuntimeValue::I32(1), RuntimeValue::I32(0)],
        );
        assert!(matches!(
            integer_zero,
            InterpreterOutcome::Trapped {
                trap: super::Trap {
                    kind: super::TrapKind::DivideByZero,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            evaluate(
                ValueType::F32,
                Instruction::RemF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                &[
                    RuntimeValue::F32((-0.0_f32).to_bits()),
                    RuntimeValue::F32(3.0_f32.to_bits()),
                ],
            ),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::F32(bits)),
                ..
            } if bits == (-0.0_f32).to_bits()
        ));
        assert!(matches!(
            evaluate(
                ValueType::F64,
                Instruction::RemF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
                &[
                    RuntimeValue::F64(1.0_f64.to_bits()),
                    RuntimeValue::F64(0.0_f64.to_bits()),
                ],
            ),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::F64(0x7ff8_0000_0000_0000)),
                ..
            }
        ));
    }

    #[test]
    fn standard_string_intrinsics_use_unicode_scalar_indices_and_bounded_heap_values() {
        let mut heap = Heap::new_with_arena_limits(64, 4096, 64, 256, 64);
        let value = allocate_runtime_string(&mut heap, "  a😀b😀  ").unwrap();
        let needle = allocate_runtime_string(&mut heap, "😀").unwrap();
        assert_eq!(
            returned_value(
                run_standard_intrinsic(StandardIntrinsic::StringLen, &[value], Some(&mut heap),)
                    .unwrap(),
            ),
            RuntimeValue::I32(8)
        );
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::StringByteLen,
                    &[value],
                    Some(&mut heap),
                )
                .unwrap(),
            ),
            RuntimeValue::I32(14)
        );
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::StringContains,
                    &[value, needle],
                    Some(&mut heap),
                )
                .unwrap(),
            ),
            RuntimeValue::Bool(true)
        );

        let substring = returned_value(
            run_standard_intrinsic(
                StandardIntrinsic::StringSubstring,
                &[value, RuntimeValue::I32(2), RuntimeValue::I32(3)],
                Some(&mut heap),
            )
            .unwrap(),
        );
        let RuntimeValue::String {
            reference: substring,
            ..
        } = substring
        else {
            panic!("expected string")
        };
        assert_eq!(heap.string(substring).unwrap(), "a😀b");

        let trimmed = returned_value(
            run_standard_intrinsic(StandardIntrinsic::StringTrim, &[value], Some(&mut heap))
                .unwrap(),
        );
        let RuntimeValue::String {
            reference: trimmed, ..
        } = trimmed
        else {
            panic!("expected string")
        };
        assert_eq!(heap.string(trimmed).unwrap(), "a😀b😀");

        let compact = allocate_runtime_string(&mut heap, "a😀b😀").unwrap();
        let split = returned_value(
            run_standard_intrinsic(
                StandardIntrinsic::StringSplit,
                &[compact, needle],
                Some(&mut heap),
            )
            .unwrap(),
        );
        let parts = heap.array_values(split).unwrap();
        assert_eq!(parts.len(), 3);
        let rendered = parts
            .iter()
            .map(|part| {
                let RuntimeValue::String { reference, .. } = part else {
                    panic!("expected string")
                };
                heap.string(*reference).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["a", "b", ""]);
    }

    #[test]
    fn standard_string_lengths_reject_overflow_and_split_is_preflighted() {
        assert!(matches!(
            super::string_length_to_i32(i32::MAX as usize + 1),
            Err(super::InterpreterError::StringLengthOverflow)
        ));

        let mut heap = Heap::new_with_arena_limits(16, 4096, 4, 16, 16);
        let value = allocate_runtime_string(&mut heap, "abcdefgh").unwrap();
        let delimiter = allocate_runtime_string(&mut heap, "").unwrap();
        let before = heap.collection_inspection();
        assert!(matches!(
            run_standard_intrinsic(
                StandardIntrinsic::StringSplit,
                &[value, delimiter],
                Some(&mut heap),
            ),
            Err(super::InterpreterError::Heap(
                HeapError::CollectionTooLarge {
                    length: 5,
                    max_length: 4,
                }
            ))
        ));
        assert_eq!(heap.collection_inspection(), before);
        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn standard_collection_intrinsics_mutate_locally_and_return_typed_options() {
        let mut heap = Heap::new_with_arena_limits(64, 1024, 64, 256, 64);
        let array = heap
            .allocate_array(nexa_bytecode::array_type(ValueType::I32), ValueType::I32)
            .unwrap();
        for value in [3, 5] {
            assert_eq!(
                returned_value(
                    run_standard_intrinsic(
                        StandardIntrinsic::ArrayPush {
                            element: ValueType::I32
                        },
                        &[array, RuntimeValue::I32(value)],
                        Some(&mut heap),
                    )
                    .unwrap()
                ),
                RuntimeValue::Bool(true)
            );
        }
        let second = returned_value(
            run_standard_intrinsic(
                StandardIntrinsic::ArrayGet {
                    element: ValueType::I32,
                },
                &[array, RuntimeValue::I32(1)],
                Some(&mut heap),
            )
            .unwrap(),
        );
        assert_eq!(
            heap.enum_payload(second, StableId::from_parts(&["Option", "::Some"]))
                .unwrap(),
            RuntimeValue::I32(5)
        );
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::ArrayPop {
                        element: ValueType::I32
                    },
                    &[array],
                    Some(&mut heap),
                )
                .unwrap()
            ),
            RuntimeValue::I32(5)
        );

        let key = allocate_runtime_string(&mut heap, "score").unwrap();
        let map = heap
            .allocate_map(
                nexa_bytecode::map_type(ValueType::String, ValueType::I32),
                ValueType::String,
                ValueType::I32,
            )
            .unwrap();
        loop {
            match run_standard_intrinsic(
                StandardIntrinsic::MapInsert {
                    key: ValueType::String,
                    value: ValueType::I32,
                },
                &[map, key, RuntimeValue::I32(42)],
                Some(&mut heap),
            )
            .unwrap()
            {
                StandardIntrinsicOutcome::Returned(RuntimeValue::Bool(true)) => break,
                StandardIntrinsicOutcome::Retry => {}
                outcome => panic!(
                    "unexpected map insert: {:?}",
                    std::mem::discriminant(&outcome)
                ),
            }
        }
        assert_eq!(
            returned_value(
                run_standard_intrinsic(
                    StandardIntrinsic::MapContains {
                        key: ValueType::String,
                        value: ValueType::I32,
                    },
                    &[map, key],
                    Some(&mut heap),
                )
                .unwrap(),
            ),
            RuntimeValue::Bool(true)
        );
        let value = returned_value(
            run_standard_intrinsic(
                StandardIntrinsic::MapRemove {
                    key: ValueType::String,
                    value: ValueType::I32,
                },
                &[map, key],
                Some(&mut heap),
            )
            .unwrap(),
        );
        assert_eq!(
            heap.enum_payload(value, StableId::from_parts(&["Option", "::Some"]))
                .unwrap(),
            RuntimeValue::I32(42)
        );
    }

    #[test]
    fn standard_debug_traps_are_deterministic_and_do_not_require_host_capabilities() {
        let mut heap = Heap::new(8);
        let message = allocate_runtime_string(&mut heap, "boom 😀").unwrap();
        assert!(matches!(
            run_standard_intrinsic(
                StandardIntrinsic::DebugAssert,
                &[RuntimeValue::Bool(true)],
                Some(&mut heap),
            )
            .unwrap(),
            StandardIntrinsicOutcome::Returned(RuntimeValue::Bool(true))
        ));
        let StandardIntrinsicOutcome::Trapped(trap) =
            run_standard_intrinsic(StandardIntrinsic::DebugTrap, &[message], Some(&mut heap))
                .unwrap()
        else {
            panic!("expected trap")
        };
        assert_eq!(trap.to_string(), "boom 😀");
    }

    #[test]
    fn standard_intrinsic_fixed_fuel_is_checked_before_execution() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::F64],
                result: Some(ValueType::F64),
            },
            2,
        );
        function
            .effect(FunctionEffect::Immediate)
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::F64Sin,
                args_base: 0,
                args_count: 1,
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        assert!(matches!(
            CheckedInterpreter::run(&module, 0, &[RuntimeValue::F64(0)], 15).unwrap(),
            InterpreterOutcome::Suspended {
                reason: SuspendReason::Fuel,
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run(&module, 0, &[RuntimeValue::F64(0)], 17).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::F64(bits)),
                charge,
                ..
            } if bits == 0.0_f64.to_bits() && charge.fuel_used == 17
        ));
    }

    #[test]
    fn scalar_formatting_fuel_is_reserved_before_string_allocation() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I64],
                result: Some(ValueType::String),
            },
            2,
        );
        function
            .set_root(1)
            .unwrap()
            .emit(Instruction::I64ToString { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new_with_string_limit(2, 64);

        let InterpreterOutcome::Suspended { continuation, .. } = CheckedInterpreter::run_with_heap(
            &module,
            0,
            &[RuntimeValue::I64(i64::MIN)],
            6,
            &mut heap,
        )
        .unwrap() else {
            panic!("underfunded conversion must suspend");
        };
        assert_eq!(heap.live_len(), 0);

        let InterpreterOutcome::Returned {
            value: Some(RuntimeValue::String { reference, .. }),
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(8, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("funded conversion must return");
        };
        assert_eq!(charge.fuel_used, 8);
        assert_eq!(heap.string(reference), Ok("-9223372036854775808"));
    }

    #[test]
    fn string_concat_dynamic_fuel_precedes_interpolation_allocation() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String, ValueType::String],
                result: Some(ValueType::String),
            },
            3,
        );
        for root in 0..3 {
            function.set_root(root).unwrap();
        }
        function
            .emit(Instruction::StringConcat {
                dst: 2,
                lhs: 0,
                rhs: 1,
            })
            .emit(Instruction::Return { source: 2 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();

        let mut heap = Heap::new_with_arena_limits(16, 4096, 64, 128, 16);
        let left = allocate_runtime_string(&mut heap, "a").unwrap();
        let right = allocate_runtime_string(&mut heap, "b").unwrap();
        let InterpreterOutcome::Suspended { continuation, .. } =
            CheckedInterpreter::run_with_heap(&module, 0, &[left, right], 2, &mut heap).unwrap()
        else {
            panic!("concat must suspend before allocating its result");
        };
        let roots = GcRoots {
            suspended_tasks: continuation.checked_gc_roots(&module).unwrap(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 2);
        let InterpreterOutcome::Returned {
            value: Some(RuntimeValue::String { reference, .. }),
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(4, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("funded concat must complete once");
        };
        assert_eq!(heap.string(reference), Ok("ab"));
        assert_eq!(charge.fuel_used, 4);

        let mut heap = Heap::new_with_arena_limits(16, 4096, 64, 128, 16);
        let left = allocate_runtime_string(&mut heap, "123456789012345678901234567890123").unwrap();
        let right = allocate_runtime_string(&mut heap, "").unwrap();
        let InterpreterOutcome::Returned { charge, .. } =
            CheckedInterpreter::run_with_heap(&module, 0, &[left, right], 6, &mut heap).unwrap()
        else {
            panic!("large concat must complete with its exact deterministic fuel");
        };
        assert_eq!(charge.fuel_used, 6);
    }

    #[test]
    fn standard_string_split_fuel_tracks_work_and_precedes_allocation() {
        fn module() -> nexa_verifier::VerifiedModule {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![ValueType::String, ValueType::String],
                    result: Some(ValueType::Named(nexa_bytecode::array_type(
                        ValueType::String,
                    ))),
                },
                3,
            );
            for root in 0..3 {
                function.set_root(root).unwrap();
            }
            function
                .emit(Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::StringSplit,
                    args_base: 0,
                    args_count: 2,
                    dst: 2,
                })
                .emit(Instruction::Return { source: 2 });
            let mut function = function.finish().unwrap();
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: vec![true, true, false],
                },
                RootMap {
                    pc: 1,
                    bitmap: vec![false, false, true],
                },
            ];
            let mut module = ModuleBuilder::new();
            module
                .array_type(nexa_bytecode::ArrayType::new(ValueType::String))
                .function(function);
            verify(module.finish(), VerifierLimits::default()).unwrap()
        }

        let module = module();
        let execute = |text: &str| {
            let mut heap = Heap::new_with_arena_limits(64, 4096, 64, 256, 64);
            let text = allocate_runtime_string(&mut heap, text).unwrap();
            let delimiter = allocate_runtime_string(&mut heap, ",").unwrap();
            let InterpreterOutcome::Returned { charge, .. } =
                CheckedInterpreter::run_with_heap(&module, 0, &[text, delimiter], 64, &mut heap)
                    .unwrap()
            else {
                panic!("split must complete");
            };
            charge.fuel_used
        };
        assert_eq!(execute("a,b"), 25);
        assert_eq!(execute("a long prefix without delimiters,b"), 60);

        let mut heap = Heap::new_with_arena_limits(64, 4096, 64, 256, 64);
        let text = allocate_runtime_string(&mut heap, "a,b").unwrap();
        let delimiter = allocate_runtime_string(&mut heap, ",").unwrap();
        let InterpreterOutcome::Suspended { continuation, .. } =
            CheckedInterpreter::run_with_heap(&module, 0, &[text, delimiter], 20, &mut heap)
                .unwrap()
        else {
            panic!("fuel is checked before split allocation");
        };
        let roots = GcRoots {
            suspended_tasks: continuation.checked_gc_roots(&module).unwrap(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 2);

        let InterpreterOutcome::Returned { charge, .. } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(25, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap() else {
            panic!("one funded retry must finish the split exactly once");
        };
        assert_eq!(charge.fuel_used, 25);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn standard_map_insert_charges_each_rehash_attempt_once() {
        fn module() -> nexa_verifier::VerifiedModule {
            let map_type = nexa_bytecode::map_type(ValueType::I32, ValueType::I32);
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![ValueType::Named(map_type), ValueType::I32, ValueType::I32],
                    result: Some(ValueType::Bool),
                },
                4,
            );
            function.set_root(0).unwrap();
            function
                .emit(Instruction::StandardIntrinsic {
                    intrinsic: StandardIntrinsic::MapInsert {
                        key: ValueType::I32,
                        value: ValueType::I32,
                    },
                    args_base: 0,
                    args_count: 3,
                    dst: 3,
                })
                .emit(Instruction::Return { source: 3 });
            let mut function = function.finish().unwrap();
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: vec![true, false, false, false],
                },
                RootMap {
                    pc: 1,
                    bitmap: vec![false, false, false, false],
                },
            ];
            let mut module = ModuleBuilder::new();
            module
                .map_type(nexa_bytecode::MapType::new(ValueType::I32, ValueType::I32))
                .function(function);
            verify(module.finish(), VerifierLimits::default()).unwrap()
        }

        fn full_map(heap: &mut Heap) -> RuntimeValue {
            let map = heap
                .allocate_map(
                    nexa_bytecode::map_type(ValueType::I32, ValueType::I32),
                    ValueType::I32,
                    ValueType::I32,
                )
                .unwrap();
            for value in 0..12 {
                while heap.map_set(map, RuntimeValue::I32(value), RuntimeValue::I32(value))
                    == Ok(MapSetOutcome::RehashPending)
                {}
            }
            map
        }

        let module = module();
        let mut heap = Heap::new_with_arena_limits(64, 4096, 64, 512, 64);
        let map = full_map(&mut heap);
        let before = heap.map_fuel_shape(map).unwrap();
        let InterpreterOutcome::Suspended { .. } = CheckedInterpreter::run_with_heap(
            &module,
            0,
            &[map, RuntimeValue::I32(12), RuntimeValue::I32(12)],
            13,
            &mut heap,
        )
        .unwrap() else {
            panic!("an underfunded insert must suspend");
        };
        assert_eq!(heap.map_fuel_shape(map).unwrap(), before);
        assert_eq!(heap.map_len(map), Ok(12));

        let limits = FrameLimits::default();
        let continuation = CheckedInterpreter::start(
            &module,
            0,
            &[map, RuntimeValue::I32(12), RuntimeValue::I32(12)],
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .unwrap();
        let InterpreterOutcome::Suspended {
            continuation,
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(14, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("first insert attempt must initialize rehash then suspend");
        };
        assert_eq!(charge.fuel_used, 14);
        assert_eq!(heap.map_fuel_shape(map).unwrap().rehash_remaining, 8);

        let InterpreterOutcome::Suspended {
            continuation,
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(39, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("underfunded rehash chunk must suspend");
        };
        assert_eq!(charge.fuel_used, 0);
        assert_eq!(heap.map_fuel_shape(map).unwrap().rehash_remaining, 8);

        let InterpreterOutcome::Suspended {
            continuation,
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(40, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("funded rehash chunk must run exactly once");
        };
        assert_eq!(charge.fuel_used, 40);
        assert_eq!(heap.map_fuel_shape(map).unwrap().rehash_remaining, 8);

        let InterpreterOutcome::Suspended {
            continuation,
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(40, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("second funded rehash chunk must run exactly once");
        };
        assert_eq!(charge.fuel_used, 40);
        assert_eq!(heap.map_fuel_shape(map).unwrap().rehash_remaining, 0);

        let InterpreterOutcome::Suspended {
            continuation,
            charge,
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(16, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("final insert must complete before the unfunded return");
        };
        assert_eq!(charge.fuel_used, 16);
        assert_eq!(heap.map_len(map), Ok(13));

        let InterpreterOutcome::Returned { charge, .. } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(1, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap() else {
            panic!("return must complete");
        };
        assert_eq!(charge.fuel_used, 1);
    }

    #[test]
    fn dynamic_fuel_arithmetic_fails_closed() {
        assert_eq!(
            fuel_add(u64::MAX, 1),
            Err(super::InterpreterError::FuelCostOverflow)
        );
        assert_eq!(
            fuel_blocks(u64::MAX, STANDARD_STRING_FUEL_BLOCK_BYTES),
            Ok(576_460_752_303_423_488)
        );
    }
}
