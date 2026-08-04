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

use crate::frame::VerifiedCallPlan;
use crate::{
    ContinuationReservation, FrameArena, FrameError, FrameLimits, GcRef, Heap, HeapError,
    MapSetOutcome, ReturnRange, RuntimeMessage, RuntimeValue, executable::ExecutableNominalOperand,
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

#[derive(Clone, Copy, Debug)]
#[cfg(test)]
pub(crate) struct AsyncResumeValue {
    pub expected: Option<ValueType>,
    pub result: AsyncResultType,
    pub success: bool,
    pub payload: RuntimeValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticLeafOutcome {
    pub result: Result<Option<RuntimeValue>, Box<Trap>>,
    pub charge: ExecutionCharge,
    pub fuel: FuelState,
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
        Self::new_with_storage(module, function, arguments, limits, reservation, None)
    }

    /// H1: builds a continuation, reusing `storage` when its retained
    /// capacities satisfy the reservation; otherwise the storage is
    /// dropped and a fresh reservation is made. Reuse changes only where
    /// the backing vectors come from, never any admission check.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_storage(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
        storage: Option<FrameArena>,
    ) -> Result<Self, InterpreterError> {
        Self::new_with_storage_and_heap(
            module,
            function,
            arguments,
            limits,
            reservation,
            storage,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_storage_and_heap(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
        storage: Option<FrameArena>,
        heap: Option<&Heap>,
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
        let function_abi = module
            .module_abi()
            .function(function as usize)
            .ok_or(InterpreterError::TypeMismatch)?;
        validate_arguments(arguments, &function_meta.signature.parameters)?;
        let mut arena = match storage {
            Some(mut arena) => {
                if arena.reset_for_verified(limits, reservation).is_ok() {
                    arena
                } else {
                    FrameArena::with_reserved_capacity(limits, reservation)?
                }
            }
            None => FrameArena::with_reserved_capacity(limits, reservation)?,
        };
        arena.push_call(function, function_meta.registers, None)?;
        for (argument, parameter) in arguments.iter().copied().zip(&function_abi.parameters) {
            write_materialized_value_range(
                &mut arena,
                parameter.slot_offset,
                parameter.logical_type,
                argument,
                module.layout_table(),
                heap,
            )?;
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

    /// H1: extracts the arena storage for pooling, leaving an empty shell
    /// behind. Called only on terminal exits where the continuation is
    /// dropped immediately afterwards; the swap performs no allocation.
    pub(crate) fn recycle_storage(&mut self) -> FrameArena {
        std::mem::replace(&mut self.arena, FrameArena::empty_shell())
    }

    #[must_use]
    pub const fn cumulative_exhausted(&self) -> bool {
        self.cumulative_exhausted
    }

    pub(crate) fn write_resume_materialized_value(
        &mut self,
        module: &VerifiedModule,
        destination: u16,
        expected: Option<ValueType>,
        value: RuntimeValue,
        heap: Option<&Heap>,
    ) -> Result<(), InterpreterError> {
        if runtime_value_type(value) != expected {
            return Err(InterpreterError::TypeMismatch);
        }
        if let Some(expected) = expected {
            write_materialized_value_range(
                &mut self.arena,
                destination,
                expected,
                value,
                module.layout_table(),
                heap,
            )?;
        }
        self.finish_host_resume()
    }

    #[cfg(test)]
    pub(crate) fn write_resume_async_result(
        &mut self,
        module: &VerifiedModule,
        destination: u16,
        resume: AsyncResumeValue,
        heap: Option<&Heap>,
    ) -> Result<(), InterpreterError> {
        let (destination, payload_type) = self.prepare_resume_async_result(
            module,
            destination,
            resume.expected,
            resume.result,
            resume.success,
        )?;
        flatten_materialized_value(
            destination,
            payload_type,
            resume.payload,
            module.layout_table(),
            heap,
        )?;
        self.finish_host_resume()
    }

    pub(crate) fn prepare_resume_async_result(
        &mut self,
        module: &VerifiedModule,
        destination: u16,
        expected: Option<ValueType>,
        result: AsyncResultType,
        success: bool,
    ) -> Result<(&mut [RuntimeValue], ValueType), InterpreterError> {
        if expected != Some(ValueType::Named(result.result_type)) {
            return Err(InterpreterError::TypeMismatch);
        }
        let layout = module
            .layout_table()
            .named_layout(result.result_type)
            .ok_or(InterpreterError::TypeMismatch)?;
        let enum_layout = layout
            .enum_layout
            .as_ref()
            .ok_or(InterpreterError::TypeMismatch)?;
        let tag = u32::from(!success);
        let payload_type = if success {
            result.success
        } else {
            result.error
        };
        let variant = enum_layout
            .variants
            .iter()
            .find(|variant| variant.tag == tag && variant.payload_type == Some(payload_type))
            .ok_or(InterpreterError::TypeMismatch)?;
        let destination = self
            .arena
            .register_range_mut(destination, layout.physical_slots)?;
        destination.fill(RuntimeValue::Unit);
        destination[usize::from(enum_layout.tag_offset)] =
            RuntimeValue::I32(i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?);
        let payload_start = usize::from(enum_layout.payload_offset);
        let payload_end = payload_start
            .checked_add(usize::from(variant.payload_slots))
            .ok_or(InterpreterError::TypeMismatch)?;
        Ok((
            destination
                .get_mut(payload_start..payload_end)
                .ok_or(InterpreterError::TypeMismatch)?,
            payload_type,
        ))
    }

    pub(crate) fn finish_host_resume(&mut self) -> Result<(), InterpreterError> {
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
                            .map(|root_map| root_map.bitmap.as_slice())
                    })
            })
            .map_err(Into::into)
    }

    pub(crate) fn checked_gc_roots_with_executable(
        &self,
        module: &VerifiedModule,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<Vec<GcRef>, InterpreterError> {
        self.arena
            .iter_gc_roots(|function, pc| {
                let function_index = function as usize;
                let root_index = executable
                    .functions()
                    .get(function_index)?
                    .root_map_index(pc)?;
                module
                    .module()
                    .functions
                    .get(function_index)?
                    .root_maps
                    .get(root_index)
                    .filter(|root_map| root_map.pc == pc)
                    .map(|root_map| root_map.bitmap.as_slice())
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

    fn from_static_leaf(module: &VerifiedModule, function: u32, pc: u32) -> Self {
        let mut stack = Self::default();
        stack.frames[0] = ScriptFrame {
            function,
            pc,
            call_site_pc: None,
            source_span: module.module().source_span(function, pc),
        };
        stack.len = 1;
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

    fn from_static_leaf(module: &VerifiedModule, function: u32, pc: u32) -> Self {
        let script_call_stack = ScriptCallStack::from_static_leaf(module, function, pc);
        Self {
            kind: TrapKind::BytecodeTrap,
            message: crate::RuntimeMessage::from("bytecode trap"),
            module: None,
            epoch: None,
            function,
            pc,
            source_span: module.module().source_span(function, pc),
            task: None,
            script_call_stack,
            host_call_boundary: None,
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
    costs: [u16; 111],
}

static CANONICAL_OPCODE_COST_TABLE: OpcodeCostTable = OpcodeCostTable {
    version: OPCODE_COST_TABLE_VERSION,
    costs: DEFAULT_OPCODE_COSTS,
};

impl Default for OpcodeCostTable {
    fn default() -> Self {
        CANONICAL_OPCODE_COST_TABLE.clone()
    }
}

impl OpcodeCostTable {
    /// Shared immutable v7 schedule for the overwhelmingly common canonical
    /// runtime. Avoids copying the 111-entry table at every convenience API
    /// call while custom-version tests can still own a mutable table.
    #[must_use]
    pub const fn canonical() -> &'static Self {
        &CANONICAL_OPCODE_COST_TABLE
    }

    fn validate_version(&self) -> Result<(), InterpreterError> {
        if self.version != OPCODE_COST_TABLE_VERSION {
            return Err(InterpreterError::OpcodeCostTableVersion {
                expected: OPCODE_COST_TABLE_VERSION,
                actual: self.version,
            });
        }
        Ok(())
    }

    pub(crate) fn cost(&self, instruction: Instruction) -> u64 {
        if let Instruction::StandardIntrinsic { intrinsic, .. } = instruction {
            u64::from(intrinsic.base_fuel_cost())
        } else {
            u64::from(self.costs[opcode_index(instruction)])
        }
    }
}

const fn profile_builtin_type(name: &[u8]) -> StableId {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut index = 0;
    while index < name.len() {
        hash ^= name[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    StableId(hash)
}

const PROFILE_STRING_TYPE: StableId = profile_builtin_type(b"String");
const PROFILE_BUFFER_TYPE: StableId = profile_builtin_type(b"Buffer");

/// Stable WP14 allocation kind and type identity; `None` marks a
/// non-allocating instruction. Struct values live in frame slot ranges and
/// therefore never appear here.
pub(crate) const fn allocation_profile(
    instruction: Instruction,
    _resolved: nexa_verifier::ResolvedNominalOperand,
) -> Option<(crate::profiler::AllocationKind, StableId)> {
    use crate::profiler::AllocationKind;
    match instruction {
        Instruction::ClassNew { type_id, .. } => Some((AllocationKind::Class, type_id)),
        Instruction::ArrayNew { type_id, .. } => Some((AllocationKind::ArrayStorage, type_id)),
        Instruction::MapNew { type_id, .. } => Some((AllocationKind::MapSlots, type_id)),
        Instruction::LoadString { .. }
        | Instruction::StringConcat { .. }
        | Instruction::StringBuild { .. }
        | Instruction::StringToString { .. }
        | Instruction::I32ToString { .. }
        | Instruction::I64ToString { .. }
        | Instruction::F32ToString { .. }
        | Instruction::F64ToString { .. }
        | Instruction::BoolToString { .. }
        | Instruction::RuneToString { .. } => Some((AllocationKind::String, PROFILE_STRING_TYPE)),
        Instruction::BufferSlice { .. } => {
            Some((AllocationKind::BufferStorage, PROFILE_BUFFER_TYPE))
        }
        _ => None,
    }
}

#[inline]
fn record_executable_profile_instruction(
    profile_poll: &mut crate::profiler::ProfilePoll,
    function: u32,
    pc: u32,
    execution_row: crate::executable::ExecutableInstruction,
    profile_row: crate::executable::ExecutableProfileRow,
) {
    crate::profiler::record_instruction(profile_poll, execution_row.profile_opcode());
    if execution_row.has_profile_event() {
        crate::profiler::record_instruction_event(
            profile_poll,
            function,
            profile_row
                .allocation
                .map(|(kind, type_id)| crate::profiler::AllocationEvent {
                    pc,
                    source_span: profile_row.source_span,
                    kind,
                    type_id,
                }),
            profile_row.host_call,
        );
    }
}

pub struct CheckedInterpreter;

type CachedFunctionMetadata<'a> = (
    u32,
    &'a nexa_bytecode::Function,
    Option<&'a [crate::executable::ExecutableInstruction]>,
    Option<&'a [crate::executable::ExecutableProfileRow]>,
);

#[derive(Clone, Copy)]
enum StaticLeafStep {
    Next,
    Jump(usize),
    Return(RuntimeValue),
    Trap,
}

#[derive(Default)]
struct PreparedStaticLeafBuffers {
    copy: Option<crate::heap::PreparedBufferCopy>,
    get: Option<crate::heap::PreparedBufferGet>,
}

#[inline]
fn execute_static_leaf_instruction(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    module: &VerifiedModule,
    heap: &mut Heap,
    executable: &crate::executable::ExecutableModule,
    buffers: &PreparedStaticLeafBuffers,
) -> Result<StaticLeafStep, InterpreterError> {
    match instruction {
        instruction @ (Instruction::LoadI32 { .. }
        | Instruction::LoadI64 { .. }
        | Instruction::LoadString { .. }
        | Instruction::Move { .. }
        | Instruction::Add { .. }
        | Instruction::StringByteLen { .. }) => {
            execute_static_leaf_value(instruction, registers, module, heap, executable)
        }
        instruction @ (Instruction::CompareEq { .. }
        | Instruction::Jump { .. }
        | Instruction::JumpIfFalse { .. }) => execute_static_leaf_control(instruction, registers),
        instruction @ (Instruction::EnumNew { .. }
        | Instruction::EnumTag { .. }
        | Instruction::EnumPayload { .. }) => execute_static_leaf_enum(instruction, row, registers),
        instruction @ (Instruction::CopyValue { .. }
        | Instruction::StructNew { .. }
        | Instruction::StructGet { .. }) => {
            execute_static_leaf_aggregate(instruction, row, registers)?;
            Ok(StaticLeafStep::Next)
        }
        instruction @ (Instruction::ClassNew { .. }
        | Instruction::ClassGet { .. }
        | Instruction::ClassSet { .. }) => {
            execute_static_leaf_class(instruction, row, registers, module, heap)?;
            Ok(StaticLeafStep::Next)
        }
        instruction @ (Instruction::ArrayNew { .. }
        | Instruction::ArrayPush { .. }
        | Instruction::ArraySet { .. }
        | Instruction::ArrayGet { .. }
        | Instruction::ArrayLen { .. }) => {
            execute_static_leaf_array(instruction, row, registers, module, heap)?;
            Ok(StaticLeafStep::Next)
        }
        instruction @ (Instruction::MapNew { .. }
        | Instruction::MapSet { .. }
        | Instruction::MapGet { .. }) => {
            execute_static_leaf_map(instruction, row, registers, module, heap)?;
            Ok(StaticLeafStep::Next)
        }
        instruction @ (Instruction::BufferCopy { .. } | Instruction::BufferGet { .. }) => {
            execute_static_leaf_buffer(instruction, registers, heap, buffers)?;
            Ok(StaticLeafStep::Next)
        }
        Instruction::Return { source } => Ok(StaticLeafStep::Return(
            crate::trusted::read_static_leaf(registers, source),
        )),
        Instruction::Trap => Ok(StaticLeafStep::Trap),
        _ => {
            debug_assert!(
                false,
                "executable static-leaf certification and executor diverged"
            );
            Err(InterpreterError::TypeMismatch)
        }
    }
}

fn execute_static_leaf_value(
    instruction: Instruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    module: &VerifiedModule,
    heap: &mut Heap,
    executable: &crate::executable::ExecutableModule,
) -> Result<StaticLeafStep, InterpreterError> {
    match instruction {
        Instruction::LoadI32 { dst, value } => {
            crate::trusted::write_static_leaf(registers, dst, RuntimeValue::I32(value));
        }
        Instruction::LoadI64 { dst, value } => {
            crate::trusted::write_static_leaf(registers, dst, RuntimeValue::I64(value));
        }
        Instruction::LoadString { dst, string } => {
            let value = load_static_leaf_string(module, heap, executable, string)?;
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        Instruction::Move { dst, source } => {
            let value = crate::trusted::read_static_leaf(registers, source);
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        Instruction::Add { dst, lhs, rhs } => {
            let RuntimeValue::I32(lhs) = crate::trusted::read_static_leaf(registers, lhs) else {
                return Err(InterpreterError::TypeMismatch);
            };
            let RuntimeValue::I32(rhs) = crate::trusted::read_static_leaf(registers, rhs) else {
                return Err(InterpreterError::TypeMismatch);
            };
            crate::trusted::write_static_leaf(
                registers,
                dst,
                RuntimeValue::I32(lhs.wrapping_add(rhs)),
            );
        }
        Instruction::StringByteLen { dst, source } => {
            let RuntimeValue::String { reference, .. } =
                crate::trusted::read_static_leaf(registers, source)
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            let length = string_length_to_i32(heap.string(reference)?.len())?;
            crate::trusted::write_static_leaf(registers, dst, RuntimeValue::I32(length));
        }
        _ => unreachable!("value leaf helper receives only value instructions"),
    }
    Ok(StaticLeafStep::Next)
}

fn execute_static_leaf_aggregate(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
) -> Result<(), InterpreterError> {
    match instruction {
        Instruction::CopyValue { dst, source, slots } => {
            crate::trusted::copy_static_leaf_range(registers, source, dst, slots);
        }
        Instruction::StructNew {
            fields_base,
            fields_count,
            dst,
            ..
        } => {
            let ExecutableNominalOperand::StructLayout { slots } = row.resolved_nominal else {
                return Err(InterpreterError::TypeMismatch);
            };
            if slots != fields_count {
                return Err(InterpreterError::TypeMismatch);
            }
            crate::trusted::copy_static_leaf_range(registers, fields_base, dst, slots);
        }
        Instruction::StructGet { source, dst, .. } => {
            let ExecutableNominalOperand::StructField { offset, slots, .. } = row.resolved_nominal
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            crate::trusted::copy_static_leaf_range(
                registers,
                source
                    .checked_add(offset)
                    .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?,
                dst,
                slots,
            );
        }
        _ => unreachable!("aggregate leaf helper receives only physical aggregate instructions"),
    }
    Ok(())
}

fn load_static_leaf_string(
    module: &VerifiedModule,
    heap: &mut Heap,
    executable: &crate::executable::ExecutableModule,
    string: u32,
) -> Result<RuntimeValue, InterpreterError> {
    let (reference, hash) = if let Some((pool, constant)) = executable.pooled_string(string) {
        heap.load_pooled_string(
            pool,
            string,
            std::sync::Arc::clone(&constant.value),
            constant.hash,
        )?
    } else {
        let value = module
            .module()
            .strings
            .get(string as usize)
            .ok_or(InterpreterError::TypeMismatch)?;
        heap.load_string_literal_with_hash(value)?
    };
    Ok(RuntimeValue::String { reference, hash })
}

fn execute_static_leaf_control(
    instruction: Instruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
) -> Result<StaticLeafStep, InterpreterError> {
    match instruction {
        Instruction::CompareEq { dst, lhs, rhs } => {
            let lhs = crate::trusted::read_static_leaf(registers, lhs);
            let rhs = crate::trusted::read_static_leaf(registers, rhs);
            if runtime_value_type(lhs).is_none()
                || runtime_value_type(lhs) != runtime_value_type(rhs)
            {
                return Err(InterpreterError::TypeMismatch);
            }
            crate::trusted::write_static_leaf(
                registers,
                dst,
                RuntimeValue::Bool(runtime_values_equal(lhs, rhs)),
            );
            Ok(StaticLeafStep::Next)
        }
        Instruction::Jump { target } => Ok(StaticLeafStep::Jump(
            usize::try_from(target).map_err(|_| InterpreterError::TypeMismatch)?,
        )),
        Instruction::JumpIfFalse { condition, target } => {
            let RuntimeValue::Bool(condition) =
                crate::trusted::read_static_leaf(registers, condition)
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            Ok(if condition {
                StaticLeafStep::Next
            } else {
                StaticLeafStep::Jump(
                    usize::try_from(target).map_err(|_| InterpreterError::TypeMismatch)?,
                )
            })
        }
        _ => unreachable!("control leaf helper receives only control instructions"),
    }
}

fn execute_static_leaf_enum(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
) -> Result<StaticLeafStep, InterpreterError> {
    match instruction {
        Instruction::EnumNew { payload, dst, .. } => {
            let ExecutableNominalOperand::EnumVariant {
                tag,
                payload_offset,
                payload_slots,
                owner_slots,
                ..
            } = row.resolved_nominal
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            crate::trusted::clear_static_leaf_range(registers, dst, owner_slots);
            crate::trusted::write_static_leaf(
                registers,
                dst,
                RuntimeValue::I32(i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?),
            );
            match (payload, payload_slots) {
                (Some(payload), slots) if slots > 0 => {
                    let destination = dst
                        .checked_add(payload_offset)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    crate::trusted::copy_static_leaf_range(registers, payload, destination, slots);
                }
                (None, 0) => {}
                _ => return Err(InterpreterError::TypeMismatch),
            }
        }
        Instruction::EnumTag { source, dst } => {
            let ExecutableNominalOperand::EnumLayout { .. } = row.resolved_nominal else {
                return Err(InterpreterError::TypeMismatch);
            };
            let tag = crate::trusted::read_static_leaf(registers, source);
            if !matches!(tag, RuntimeValue::I32(_)) {
                return Err(InterpreterError::TypeMismatch);
            }
            crate::trusted::write_static_leaf(registers, dst, tag);
        }
        Instruction::EnumPayload { source, dst, .. } => {
            let ExecutableNominalOperand::EnumVariant {
                tag,
                payload_offset,
                payload_slots,
                ..
            } = row.resolved_nominal
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            if crate::trusted::read_static_leaf(registers, source)
                != RuntimeValue::I32(
                    i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?,
                )
            {
                return Err(InterpreterError::TypeMismatch);
            }
            let payload_source = source
                .checked_add(payload_offset)
                .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
            crate::trusted::copy_static_leaf_range(registers, payload_source, dst, payload_slots);
        }
        _ => unreachable!("enum leaf helper receives only enum instructions"),
    }
    Ok(StaticLeafStep::Next)
}

fn execute_static_leaf_class(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    module: &VerifiedModule,
    heap: &mut Heap,
) -> Result<(), InterpreterError> {
    match instruction {
        Instruction::ClassNew {
            type_id,
            fields_base,
            fields_count,
            dst,
        } => {
            let fields =
                crate::trusted::read_static_leaf_window(registers, fields_base, fields_count);
            let value = heap.allocate_class(type_id, fields)?;
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        Instruction::ClassGet {
            source,
            field: _,
            dst,
        } => {
            let value = crate::trusted::read_static_leaf(registers, source);
            let RuntimeValue::NamedRef { .. } = value else {
                return Err(InterpreterError::TypeMismatch);
            };
            let resolved = resolved_class_field(module, row.resolved_nominal)?;
            let field_values = heap.class_field_range(value, resolved.offset, resolved.slots)?;
            crate::trusted::write_static_leaf_values(
                registers,
                dst,
                u16::try_from(resolved.slots).map_err(|_| InterpreterError::TypeMismatch)?,
                field_values.iter(),
            )?;
        }
        Instruction::ClassSet {
            source,
            field: _,
            value,
        } => {
            let object = crate::trusted::read_static_leaf(registers, source);
            let RuntimeValue::NamedRef { .. } = object else {
                return Err(InterpreterError::TypeMismatch);
            };
            let resolved = resolved_class_field(module, row.resolved_nominal)?;
            let replacement = crate::trusted::read_static_leaf_window(
                registers,
                value,
                u16::try_from(resolved.slots).map_err(|_| InterpreterError::TypeMismatch)?,
            );
            heap.set_class_field_range(object, resolved.offset, replacement)?;
        }
        _ => unreachable!("class leaf helper receives only class instructions"),
    }
    Ok(())
}

fn execute_static_leaf_array(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    module: &VerifiedModule,
    heap: &mut Heap,
) -> Result<(), InterpreterError> {
    match instruction {
        Instruction::ArrayNew { type_id, dst } => {
            let (element_type, row_fields) =
                resolved_array_layout(module, type_id, row.resolved_nominal)?;
            let value = match row_fields {
                Some(field_count) => {
                    heap.allocate_value_row_array(type_id, element_type, field_count)?
                }
                None => heap.allocate_array(type_id, element_type)?,
            };
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        Instruction::ArrayPush { source, value } => {
            let array = crate::trusted::read_static_leaf(registers, source);
            let (element, element_slots, row_slots) =
                resolved_array_value(module, row.resolved_nominal)?;
            let physical = crate::trusted::read_static_leaf_window(registers, value, element_slots);
            if row_slots.is_some() {
                heap.array_push_row(array, physical)?;
            } else {
                let value = materialize_physical_value_range(
                    physical,
                    element,
                    module.layout_table(),
                    Some(heap),
                )?;
                heap.array_push(array, value)?;
            }
        }
        Instruction::ArraySet {
            source,
            index,
            value,
        } => {
            let array = crate::trusted::read_static_leaf(registers, source);
            let RuntimeValue::I32(index) = crate::trusted::read_static_leaf(registers, index)
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            let index = usize::try_from(index).map_err(|_| InterpreterError::TypeMismatch)?;
            let (element, element_slots, row_slots) =
                resolved_array_value(module, row.resolved_nominal)?;
            let physical = crate::trusted::read_static_leaf_window(registers, value, element_slots);
            if row_slots.is_some() {
                heap.array_set_row(array, index, physical)?;
            } else {
                let replacement = materialize_physical_value_range(
                    physical,
                    element,
                    module.layout_table(),
                    Some(heap),
                )?;
                heap.array_set(array, index, replacement)?;
            }
        }
        Instruction::ArrayGet { source, index, dst } => {
            let array = crate::trusted::read_static_leaf(registers, source);
            let RuntimeValue::I32(index) = crate::trusted::read_static_leaf(registers, index)
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            let index = usize::try_from(index).map_err(|_| InterpreterError::TypeMismatch)?;
            let (element, element_slots, _row_slots) =
                resolved_array_value(module, row.resolved_nominal)?;
            let value = heap.array_value_range(array, index)?;
            write_static_leaf_array_element(
                registers,
                dst,
                element,
                element_slots,
                value,
                module.layout_table(),
                heap,
            )?;
        }
        Instruction::ArrayLen { source, dst } => {
            let array = crate::trusted::read_static_leaf(registers, source);
            let length = i32::try_from(heap.array_len(array)?)
                .map_err(|_| InterpreterError::StringLengthOverflow)?;
            crate::trusted::write_static_leaf(registers, dst, RuntimeValue::I32(length));
        }
        _ => unreachable!("array leaf helper receives only array instructions"),
    }
    Ok(())
}

fn write_static_leaf_array_element(
    registers: &mut crate::trusted::StaticLeafRegisters,
    destination: u16,
    element: ValueType,
    element_slots: u16,
    value: crate::CollectionView<'_>,
    layouts: &nexa_bytecode::layout::LayoutTable,
    heap: &Heap,
) -> Result<(), InterpreterError> {
    if value.len() == usize::from(element_slots) {
        return crate::trusted::write_static_leaf_values(
            registers,
            destination,
            element_slots,
            value.iter(),
        );
    }
    let mut physical = [RuntimeValue::Unit; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY];
    let physical = physical
        .get_mut(..usize::from(element_slots))
        .ok_or(InterpreterError::TypeMismatch)?;
    write_collection_element_physical(value, physical, element, layouts, heap)?;
    crate::trusted::write_static_leaf_values(
        registers,
        destination,
        element_slots,
        physical.iter().copied(),
    )
}

fn execute_static_leaf_map(
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    module: &VerifiedModule,
    heap: &mut Heap,
) -> Result<(), InterpreterError> {
    match instruction {
        Instruction::MapNew { type_id, dst } => {
            let (key, value) = resolved_map_layout(module, type_id, row.resolved_nominal)?;
            let (_key_type, _value_type, key_slots, value_slots, _, _) =
                resolved_map_value(module, row.resolved_nominal)?;
            if key_slots != 1 {
                return Err(InterpreterError::TypeMismatch);
            }
            let value = allocate_resolved_map(heap, module, type_id, key, value, value_slots)?;
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        Instruction::MapSet { source, key, value } => {
            let (_key_type, _value_type, key_slots, value_slots, _, _) =
                resolved_map_value(module, row.resolved_nominal)?;
            if key_slots != 1 || value_slots == 0 {
                return Err(InterpreterError::TypeMismatch);
            }
            let map = crate::trusted::read_static_leaf(registers, source);
            let key = crate::trusted::read_static_leaf(registers, key);
            let value = crate::trusted::read_static_leaf_window(registers, value, value_slots);
            if heap.map_set_value_range(map, key, value)? != MapSetOutcome::Complete {
                debug_assert!(false, "certified one-entry map unexpectedly entered rehash");
                return Err(InterpreterError::TypeMismatch);
            }
        }
        Instruction::MapGet {
            source,
            key,
            result_type: _,
            dst,
        } => {
            let (_key_type, _value_type, key_slots, value_slots, option_slots, payload_offset) =
                resolved_map_value(module, row.resolved_nominal)?;
            if key_slots != 1 || value_slots == 0 || option_slots == 0 {
                return Err(InterpreterError::TypeMismatch);
            }
            let map = crate::trusted::read_static_leaf(registers, source);
            let key = crate::trusted::read_static_leaf(registers, key);
            let value = heap.map_get_value_range(map, key)?;
            crate::trusted::clear_static_leaf_range(registers, dst, option_slots);
            crate::trusted::write_static_leaf(
                registers,
                dst,
                RuntimeValue::I32(i32::from(value.is_some())),
            );
            if let Some(value) = value {
                let payload = dst
                    .checked_add(payload_offset)
                    .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                crate::trusted::write_static_leaf_values(
                    registers,
                    payload,
                    value_slots,
                    value.iter(),
                )?;
            }
        }
        _ => unreachable!("map leaf helper receives only map instructions"),
    }
    Ok(())
}

fn execute_static_leaf_buffer(
    instruction: Instruction,
    registers: &mut crate::trusted::StaticLeafRegisters,
    heap: &mut Heap,
    buffers: &PreparedStaticLeafBuffers,
) -> Result<(), InterpreterError> {
    match instruction {
        Instruction::BufferCopy { .. } => {
            let prepared = buffers.copy.ok_or(InterpreterError::TypeMismatch)?;
            heap.execute_prepared_buffer_copy(prepared)?;
        }
        Instruction::BufferGet { dst, .. } => {
            let prepared = buffers.get.ok_or(InterpreterError::TypeMismatch)?;
            let value = heap.execute_prepared_buffer_get(prepared)?;
            crate::trusted::write_static_leaf(registers, dst, value);
        }
        _ => unreachable!("buffer leaf helper receives only buffer instructions"),
    }
    Ok(())
}

fn execute_prepared_buffer_kernel(
    certificate: crate::executable::StaticLeafCertificate,
    buffers: &PreparedStaticLeafBuffers,
    fuel: FuelState,
    fuel_used: u64,
    heap: &mut Heap,
) -> Result<StaticLeafOutcome, InterpreterError> {
    let instructions = certificate
        .buffer_kernel_instructions
        .ok_or(InterpreterError::TypeMismatch)?;
    heap.execute_prepared_buffer_copy(buffers.copy.ok_or(InterpreterError::TypeMismatch)?)?;
    let value =
        heap.execute_prepared_buffer_get(buffers.get.ok_or(InterpreterError::TypeMismatch)?)?;
    settle_static_leaf_return(value, u64::from(instructions), fuel, fuel_used)
}

fn execute_static_leaf_constant_kernel(
    kernel: crate::executable::StaticLeafConstantKernel,
    module: &VerifiedModule,
    executable: &crate::executable::ExecutableModule,
    fuel: FuelState,
    fuel_used: u64,
    heap: &mut Heap,
) -> Result<StaticLeafOutcome, InterpreterError> {
    match kernel.effect {
        crate::executable::StaticLeafConstantEffect::None => {}
        crate::executable::StaticLeafConstantEffect::LoadString { string } => {
            let _ = load_static_leaf_string(module, heap, executable, string)?;
        }
    }
    settle_static_leaf_return(
        RuntimeValue::I32(kernel.result),
        u64::from(kernel.instructions),
        fuel,
        fuel_used,
    )
}

fn settle_static_leaf_return(
    value: RuntimeValue,
    instructions: u64,
    mut fuel: FuelState,
    fuel_used: u64,
) -> Result<StaticLeafOutcome, InterpreterError> {
    fuel.slice_remaining = fuel
        .slice_remaining
        .checked_sub(fuel_used)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel.cumulative_used = fuel
        .cumulative_used
        .checked_add(fuel_used)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    Ok(StaticLeafOutcome {
        result: Ok(Some(value)),
        charge: ExecutionCharge {
            instructions,
            fuel_used,
        },
        fuel,
    })
}

fn finish_static_leaf(
    step: StaticLeafStep,
    module: &VerifiedModule,
    function: u32,
    pc: usize,
    charge: ExecutionCharge,
    mut fuel: FuelState,
) -> Result<StaticLeafOutcome, InterpreterError> {
    fuel.slice_remaining = fuel
        .slice_remaining
        .checked_sub(charge.fuel_used)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel.cumulative_used = fuel
        .cumulative_used
        .checked_add(charge.fuel_used)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let result = match step {
        StaticLeafStep::Return(value) => Ok(Some(value)),
        StaticLeafStep::Trap => Err(Box::new(Trap::from_static_leaf(
            module,
            function,
            u32::try_from(pc).map_err(|_| InterpreterError::TypeMismatch)?,
        ))),
        StaticLeafStep::Next | StaticLeafStep::Jump(_) => unreachable!(),
    };
    Ok(StaticLeafOutcome {
        result,
        charge,
        fuel,
    })
}

fn resolved_array_layout(
    module: &VerifiedModule,
    type_id: StableId,
    resolved: ExecutableNominalOperand,
) -> Result<(ValueType, Option<std::num::NonZeroU16>), InterpreterError> {
    let (element, _element_slots, row_slots) = resolved_array_value(module, resolved)?;
    module
        .array_type(type_id.0)
        .filter(|array_type| array_type.element == element)
        .map(|_| (element, row_slots))
        .ok_or(InterpreterError::TypeMismatch)
}

fn resolved_array_value(
    module: &VerifiedModule,
    resolved: ExecutableNominalOperand,
) -> Result<(ValueType, u16, Option<std::num::NonZeroU16>), InterpreterError> {
    let ExecutableNominalOperand::ArrayLayout {
        type_index,
        element_slots,
        row_slots,
    } = resolved
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    module
        .module()
        .array_types
        .get(usize::from(type_index))
        .map(|array_type| {
            (
                array_type.element,
                element_slots,
                std::num::NonZeroU16::new(row_slots),
            )
        })
        .ok_or(InterpreterError::TypeMismatch)
}

fn resolved_array_field(
    resolved: ExecutableNominalOperand,
) -> Result<(u16, u16, std::num::NonZeroU16), InterpreterError> {
    let ExecutableNominalOperand::ArrayField {
        offset,
        slots,
        row_slots,
        ..
    } = resolved
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    let row_slots = std::num::NonZeroU16::new(row_slots).ok_or(InterpreterError::TypeMismatch)?;
    Ok((offset, slots, row_slots))
}

fn resolved_map_layout(
    module: &VerifiedModule,
    type_id: StableId,
    resolved: ExecutableNominalOperand,
) -> Result<(ValueType, ValueType), InterpreterError> {
    let (key, value, ..) = resolved_map_value(module, resolved)?;
    module
        .map_type(type_id.0)
        .filter(|map_type| map_type.key == key && map_type.value == value)
        .map(|_| (key, value))
        .ok_or(InterpreterError::TypeMismatch)
}

fn resolved_map_value(
    module: &VerifiedModule,
    resolved: ExecutableNominalOperand,
) -> Result<(ValueType, ValueType, u16, u16, u16, u16), InterpreterError> {
    let ExecutableNominalOperand::MapLayout {
        type_index,
        key_slots,
        value_slots,
        option_slots,
        option_payload_offset,
    } = resolved
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    module
        .module()
        .map_types
        .get(usize::from(type_index))
        .map(|map_type| {
            (
                map_type.key,
                map_type.value,
                key_slots,
                value_slots,
                option_slots,
                option_payload_offset,
            )
        })
        .ok_or(InterpreterError::TypeMismatch)
}

fn allocate_resolved_map(
    heap: &mut Heap,
    module: &VerifiedModule,
    type_id: StableId,
    key_type: ValueType,
    value_type: ValueType,
    value_slots: u16,
) -> Result<RuntimeValue, InterpreterError> {
    if let ValueType::Named(named_id) = value_type
        && let Some(layout) = module.layout_table().named_layout(named_id)
    {
        return Ok(heap.allocate_physical_map_with_layout(
            type_id,
            key_type,
            value_type,
            value_slots,
            layout,
        )?);
    }
    Ok(heap.allocate_physical_map(type_id, key_type, value_type, value_slots)?)
}

#[derive(Clone, Copy)]
struct ResolvedClassField {
    offset: usize,
    slots: usize,
    expected: ValueType,
    state_index: Option<usize>,
}

fn resolved_class_field(
    module: &VerifiedModule,
    resolved: ExecutableNominalOperand,
) -> Result<ResolvedClassField, InterpreterError> {
    let ExecutableNominalOperand::ClassField {
        type_index,
        index,
        offset,
        slots,
        state_index,
    } = resolved
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    let expected = module
        .module()
        .class_types
        .get(usize::from(type_index))
        .and_then(|class_type| class_type.fields.get(usize::from(index)))
        .map(|field| field.ty)
        .ok_or(InterpreterError::TypeMismatch)?;
    Ok(ResolvedClassField {
        offset: usize::from(offset),
        slots: usize::from(slots),
        expected,
        state_index: (state_index != u16::MAX).then_some(usize::from(state_index)),
    })
}

fn resolved_state_field(
    module: &VerifiedModule,
    resolved: ExecutableNominalOperand,
) -> Result<(usize, ValueType), InterpreterError> {
    let ExecutableNominalOperand::StateField {
        type_index,
        field_index,
        sorted_index,
    } = resolved
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    let expected = module
        .module()
        .state_schema
        .types
        .get(usize::from(type_index))
        .and_then(|state_type| state_type.fields.get(usize::from(field_index)))
        .map(|field| field.ty)
        .ok_or(InterpreterError::TypeMismatch)?;
    Ok((usize::from(sorted_index), expected))
}

fn static_leaf_upper_fuel(
    certificate: crate::executable::StaticLeafCertificate,
    heap: &Heap,
) -> Result<Option<u64>, InterpreterError> {
    let mut upper = fuel_add(
        fuel_add(certificate.fixed_fuel, certificate.physical_value_fuel)?,
        fuel_add(
            certificate.array_push_element_fuel,
            certificate.buffer_work_fuel,
        )?,
    )?;
    if certificate.array_pushes != 0 {
        let initial_ranges = fuel_usize(heap.collection_arena_fuel_shape().free_ranges)?;
        for push in 0..certificate.array_pushes {
            // A splitting claim and the following release can each add at most
            // one free range. Pretend every prior push grew so this remains an
            // upper bound for any allocator shape the certified local array sees.
            let ranges = initial_ranges
                .checked_add(u64::from(push).saturating_mul(2))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            upper = fuel_add(
                upper,
                collection_arena_metadata_shape_fuel(ranges, true, true)?,
            )?;
        }
    }
    let map_attempts = u64::from(certificate.map_sets)
        .checked_mul(2)
        .and_then(|sets| sets.checked_add(u64::from(certificate.map_lookups)))
        .ok_or(InterpreterError::FuelCostOverflow)?;
    if map_attempts != 0 {
        let slots = heap.empty_map_capacity();
        // With fewer than two slots, even the first insertion enters the
        // retrying rehash protocol. Keep those unusual heaps on the full
        // interpreter before MapNew mutates them.
        if slots < 2 {
            return Ok(None);
        }
        let scan = fuel_blocks(fuel_usize(slots)?, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
        upper = fuel_add(
            upper,
            scan.checked_mul(map_attempts)
                .ok_or(InterpreterError::FuelCostOverflow)?,
        )?;
    }
    Ok(Some(upper))
}

#[cold]
#[inline(never)]
fn write_static_leaf_materialized_argument(
    registers: &mut crate::trusted::StaticLeafRegisters,
    destination: u16,
    slots: u16,
    expected: ValueType,
    value: RuntimeValue,
    module: &VerifiedModule,
    heap: &Heap,
) -> Result<(), InterpreterError> {
    let slots = usize::from(slots);
    if slots > crate::trusted::STATIC_LEAF_REGISTER_CAPACITY {
        return Err(InterpreterError::TypeMismatch);
    }
    let mut physical = [RuntimeValue::Unit; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY];
    flatten_materialized_value(
        &mut physical[..slots],
        expected,
        value,
        module.layout_table(),
        Some(heap),
    )?;
    crate::trusted::write_static_leaf_values(
        registers,
        destination,
        u16::try_from(slots).map_err(|_| InterpreterError::TypeMismatch)?,
        physical[..slots].iter().copied(),
    )
}

fn static_leaf_attempt_fuel(
    nominal_shape: nexa_verifier::NominalIndexShape,
    instruction: Instruction,
    row: crate::executable::ExecutableInstruction,
    registers: &crate::trusted::StaticLeafRegisters,
    heap: &Heap,
) -> Result<u64, InterpreterError> {
    if !row.dynamic_fuel() || matches!(instruction, Instruction::Return { .. }) {
        return Ok(row.attempt_fuel);
    }
    let physical_work =
        physical_instruction_work_fuel(nominal_shape, instruction, row.resolved_nominal)?
            .unwrap_or(0);
    let work = match instruction {
        Instruction::ArrayPush { source, .. } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = row.resolved_nominal
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            let array = crate::trusted::read_static_leaf(registers, source);
            let (live, capacity) = heap.array_fuel_shape(array)?;
            let moved = if live < capacity { 1 } else { live.max(1) };
            let moved_slots = fuel_usize(moved)?
                .checked_mul(u64::from(element_slots))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let element_work = fuel_blocks(moved_slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            fuel_add(
                element_work,
                collection_arena_metadata_fuel(heap, live >= capacity, live >= capacity)?,
            )?
        }
        Instruction::BufferCopy { length, .. } => {
            let RuntimeValue::I32(length) = crate::trusted::read_static_leaf(registers, length)
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_blocks(
                u64::try_from(length).map_err(|_| InterpreterError::TypeMismatch)?,
                STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS,
            )?
        }
        Instruction::MapSet { source, key, .. } => map_insert_attempt_fuel(
            heap,
            crate::trusted::read_static_leaf(registers, source),
            crate::trusted::read_static_leaf(registers, key),
        )?,
        Instruction::MapGet { source, key, .. } => map_lookup_fuel(
            heap,
            crate::trusted::read_static_leaf(registers, source),
            crate::trusted::read_static_leaf(registers, key),
        )?,
        _ if physical_work != 0 => 0,
        _ => {
            debug_assert!(false, "certified leaf dynamic-fuel surface diverged");
            return Err(InterpreterError::TypeMismatch);
        }
    };
    fuel_add(row.attempt_fuel, fuel_add(work, physical_work)?)
}

fn prepare_static_leaf_buffers(
    certificate: crate::executable::StaticLeafCertificate,
    registers: &crate::trusted::StaticLeafRegisters,
    heap: &Heap,
) -> Option<PreparedStaticLeafBuffers> {
    let copy = if let Some(check) = certificate.buffer_copy {
        let destination = crate::trusted::read_static_leaf(registers, check.destination);
        let source = crate::trusted::read_static_leaf(registers, check.source);
        Some(
            heap.prepare_buffer_copy(
                destination,
                source,
                check.source_start,
                check.destination_start,
                check.length,
            )
            .ok()?,
        )
    } else {
        None
    };
    let get = if let Some(check) = certificate.buffer_get {
        let buffer = crate::trusted::read_static_leaf(registers, check.source);
        Some(heap.prepare_buffer_get(buffer, check.index).ok()?)
    } else {
        None
    };
    Some(PreparedStaticLeafBuffers { copy, get })
}

/// Verifier-packed Host arguments borrowed directly from the current frame.
///
/// `slots` is the contiguous physical ABI range while `types` preserves the
/// logical argument boundary required by generated bindings. The layout
/// table lets the Host codec expose Struct/Enum views without constructing
/// temporary VM objects.
#[derive(Clone, Copy, Debug)]
pub struct InterpreterHostArguments<'a> {
    slots: &'a [RuntimeValue],
    types: &'a [ValueType],
    layouts: &'a nexa_bytecode::layout::LayoutTable,
}

impl<'a> InterpreterHostArguments<'a> {
    fn new(
        slots: &'a [RuntimeValue],
        types: &'a [ValueType],
        layouts: &'a nexa_bytecode::layout::LayoutTable,
    ) -> Self {
        Self {
            slots,
            types,
            layouts,
        }
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.types.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.types.is_empty()
    }

    pub(crate) const fn parts(
        self,
    ) -> (
        &'a [RuntimeValue],
        &'a [ValueType],
        &'a nexa_bytecode::layout::LayoutTable,
    ) {
        (self.slots, self.types, self.layouts)
    }
}

pub trait InterpreterHost {
    fn call(
        &mut self,
        import: u32,
        arguments: InterpreterHostArguments<'_>,
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
    fn old_field_get_dense(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        field_index: usize,
        expected: ValueType,
    ) -> Result<RuntimeValue, crate::RuntimeMessage> {
        let _ = field_index;
        self.old_field_get(object, field_id, expected)
    }
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
    fn new_set_dense(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        field_index: usize,
        expected: ValueType,
        value: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage> {
        let _ = (field_index, expected);
        self.new_set(object, field_id, value)
    }
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
    fn object_field_dense(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        field_index: usize,
        expected: ValueType,
    ) -> Result<RuntimeValue, crate::RuntimeMessage> {
        let _ = field_index;
        self.object_field(stable_id, type_id, field_id, expected)
    }

    fn set_object_field(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        expected: ValueType,
        value: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage>;
    fn set_object_field_dense(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
        field_id: StableId,
        field_index: usize,
        expected: ValueType,
        value: RuntimeValue,
    ) -> Result<(), crate::RuntimeMessage> {
        let _ = field_index;
        self.set_object_field(stable_id, type_id, field_id, expected, value)
    }

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

    pub fn start_with_heap(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        limits: FrameLimits,
        reservation: ContinuationReservation,
        heap: &Heap,
    ) -> Result<InterpreterContinuation, InterpreterError> {
        InterpreterContinuation::new_with_storage_and_heap(
            module,
            function,
            arguments,
            limits,
            reservation,
            None,
            Some(heap),
        )
    }

    /// Executes a verifier-certified tiny leaf without constructing a
    /// continuation or touching the frame pool. Admission is all-or-nothing:
    /// the executable image supplies a static upper fuel bound, and
    /// insufficient budgets return `None` before any heap mutation so the
    /// caller can preserve ordinary suspension semantics through the full
    /// interpreter.
    pub fn try_run_static_leaf(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<Option<StaticLeafOutcome>, InterpreterError> {
        Self::try_run_static_leaf_internal::<true>(
            module, function, arguments, fuel, costs, heap, executable,
        )
    }

    /// Measurement-only A/B control with profiler support compiled out.
    ///
    /// This exists only for the Benchmark v7 WP16 overhead authority and is
    /// unavailable in normal Runtime builds.
    #[cfg(feature = "profiler-overhead-control")]
    #[doc(hidden)]
    pub fn try_run_static_leaf_without_profiler(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<Option<StaticLeafOutcome>, InterpreterError> {
        Self::try_run_static_leaf_internal::<false>(
            module, function, arguments, fuel, costs, heap, executable,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn try_run_static_leaf_internal<const PROFILING: bool>(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<Option<StaticLeafOutcome>, InterpreterError> {
        if executable.cost_table_version() != costs.version {
            return Err(InterpreterError::OpcodeCostTableVersion {
                expected: executable.cost_table_version(),
                actual: costs.version,
            });
        }
        let function_meta = module
            .module()
            .functions
            .get(function as usize)
            .ok_or(InterpreterError::MissingFunction(function))?;
        let executable_function = executable
            .functions()
            .get(function as usize)
            .ok_or(InterpreterError::MissingFunction(function))?;
        let Some(certificate) = executable_function.static_leaf_certificate() else {
            return Ok(None);
        };
        if executable_function
            .abi()
            .result
            .as_ref()
            .is_some_and(|result| {
                result.slot_count != 1
                    || matches!(
                        result.logical_type,
                        ValueType::Named(type_id)
                            if module.layout_table().named_layout(type_id).is_some_and(|layout| {
                                matches!(
                                    layout.equality_strategy,
                                    nexa_bytecode::layout::EqualityStrategy::StructFieldwise
                                        | nexa_bytecode::layout::EqualityStrategy::EnumTagPayload
                                )
                            })
                    )
            })
        {
            // The fixed leaf outcome carries one persistent RuntimeValue.
            // Aggregate export results therefore stay on the full path,
            // which performs the transactional materialization boundary.
            return Ok(None);
        }
        // `ExecutableModule` is normally owned beside the exact verified
        // module it was built from. Keep the fixed register kernel sound
        // even for direct callers that accidentally cross those objects:
        // the verifier-owned code backing must be the exact input used for
        // certification, independently fit the leaf register bank, and have
        // one bytecode row per executable row.
        if usize::from(function_meta.registers) > crate::trusted::STATIC_LEAF_REGISTER_CAPACITY
            || function_meta.code.len() != executable_function.rows().len()
            || function_meta.code.as_ptr() as usize != executable_function.code_identity()
        {
            return Ok(None);
        }
        validate_arguments(arguments, &function_meta.signature.parameters)?;
        let abi = executable_function.abi();
        let Some(upper_fuel) = static_leaf_upper_fuel(certificate, heap)? else {
            return Ok(None);
        };
        let Some(cumulative_after_upper) = fuel.cumulative_used.checked_add(upper_fuel) else {
            return Ok(None);
        };
        if upper_fuel > fuel.slice_remaining || cumulative_after_upper > fuel.cumulative_limit {
            return Ok(None);
        }
        let mut profile_module = if PROFILING && crate::profiler::enabled() {
            crate::profiler::begin_module(module)
        } else {
            None
        };
        if let Some(profile_module) = profile_module.as_mut() {
            profile_module.resolve_function(function);
        }
        let mut record_prefix = |count: usize| {
            let Some(profile_module) = profile_module.as_mut() else {
                return;
            };
            for (pc, (execution_row, profile_row)) in executable_function
                .rows()
                .iter()
                .copied()
                .zip(executable_function.profile_rows())
                .take(count)
                .enumerate()
            {
                record_executable_profile_instruction(
                    profile_module,
                    function,
                    u32::try_from(pc).unwrap_or(u32::MAX),
                    execution_row,
                    *profile_row,
                );
            }
        };
        if abi.parameter_slots == 0
            && let Some(kernel) = executable_function.static_leaf_constant_kernel()
        {
            record_prefix(usize::from(kernel.instructions));
            return execute_static_leaf_constant_kernel(
                kernel, module, executable, fuel, upper_fuel, heap,
            )
            .map(Some);
        }
        let mut registers = crate::trusted::new_static_leaf_registers();
        for (argument, parameter) in arguments.iter().copied().zip(&abi.parameters) {
            if parameter.slot_count == 1
                && !matches!(
                    parameter.logical_type,
                    ValueType::Named(type_id)
                        if module.layout_table().named_layout(type_id).is_some_and(|layout| {
                            matches!(
                                layout.equality_strategy,
                                nexa_bytecode::layout::EqualityStrategy::StructFieldwise
                                    | nexa_bytecode::layout::EqualityStrategy::EnumTagPayload
                            )
                        })
                )
            {
                crate::trusted::write_static_leaf(&mut registers, parameter.slot_offset, argument);
            } else {
                write_static_leaf_materialized_argument(
                    &mut registers,
                    parameter.slot_offset,
                    parameter.slot_count,
                    parameter.logical_type,
                    argument,
                    module,
                    heap,
                )?;
            }
        }
        let Some(prepared_buffers) = prepare_static_leaf_buffers(certificate, &registers, heap)
        else {
            return Ok(None);
        };
        if let Some(instructions) = certificate.buffer_kernel_instructions {
            record_prefix(usize::from(instructions));
            return execute_prepared_buffer_kernel(
                certificate,
                &prepared_buffers,
                fuel,
                upper_fuel,
                heap,
            )
            .map(Some);
        }
        if let Some(kernel) = executable_function.static_leaf_constant_kernel() {
            record_prefix(usize::from(kernel.instructions));
            return execute_static_leaf_constant_kernel(
                kernel, module, executable, fuel, upper_fuel, heap,
            )
            .map(Some);
        }
        let mut pc = 0_usize;
        let mut charge = ExecutionCharge::default();
        loop {
            let instruction = *function_meta
                .code
                .get(pc)
                .ok_or(InterpreterError::FellOffFunction)?;
            let row = executable_function
                .rows()
                .get(pc)
                .ok_or(InterpreterError::FellOffFunction)?;
            charge.instructions = charge.instructions.saturating_add(1);
            let attempt_fuel = static_leaf_attempt_fuel(
                module.nominal_index_shape(),
                instruction,
                *row,
                &registers,
                heap,
            )?;
            charge.fuel_used = charge
                .fuel_used
                .checked_add(attempt_fuel)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            if let Some(profile_module) = profile_module.as_mut() {
                let profile_row = executable_function
                    .profile_rows()
                    .get(pc)
                    .copied()
                    .ok_or(InterpreterError::FellOffFunction)?;
                record_executable_profile_instruction(
                    profile_module,
                    function,
                    u32::try_from(pc).unwrap_or(u32::MAX),
                    *row,
                    profile_row,
                );
            }
            let step = execute_static_leaf_instruction(
                instruction,
                *row,
                &mut registers,
                module,
                heap,
                executable,
                &prepared_buffers,
            )?;
            match step {
                StaticLeafStep::Next => pc += 1,
                StaticLeafStep::Jump(target) => pc = target,
                StaticLeafStep::Return(_) | StaticLeafStep::Trap => {
                    debug_assert!(charge.fuel_used <= upper_fuel);
                    return finish_static_leaf(step, module, function, pc, charge, fuel).map(Some);
                }
            }
        }
    }

    pub fn poll(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    pub fn poll_with_host(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            None,
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
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            None,
            Some(heap),
            None,
            None,
        )
    }

    /// F3: heap poll through predecoded rows. The rows must originate from
    /// `ExecutableModule::build` over the same verified module and cost
    /// table; fuel accounting is bit-identical to [`Self::poll_with_heap`].
    pub fn poll_with_heap_and_executable(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<true, true>(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            None,
            Some(heap),
            Some(executable),
            None,
        )
    }

    /// H2: the engine fast path for embedders driving many short task
    /// lifecycles through one call site. Terminal exits hand the frame
    /// arena's storage back through `recycle`; feeding it into the next
    /// [`InterpreterContinuation::new_with_storage`] makes steady-state
    /// churn allocation-free, exactly like realm task admission (H1).
    /// Suspensions leave the slot untouched. Outcomes and fuel accounting
    /// are identical to the non-recycling entry points.
    #[allow(clippy::too_many_arguments)]
    pub fn poll_recycling(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: Option<&mut Heap>,
        executable: Option<&crate::executable::ExecutableModule>,
        recycle: &mut Option<FrameArena>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        match (heap, executable) {
            (Some(heap), Some(executable)) => Self::execute::<true, true>(
                module,
                continuation,
                fuel,
                costs,
                None,
                None,
                None,
                Some(heap),
                Some(executable),
                Some(recycle),
            ),
            (heap, executable) => Self::execute::<false, true>(
                module,
                continuation,
                fuel,
                costs,
                None,
                None,
                None,
                heap,
                executable,
                Some(recycle),
            ),
        }
    }

    /// Measurement-only predecoded A/B control with profiler support
    /// compiled out. Benchmark v7 compares this monomorphization with the
    /// ordinary disabled and enabled paths for WP16.
    #[cfg(feature = "profiler-overhead-control")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn poll_recycling_without_profiler(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
        recycle: &mut Option<FrameArena>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<true, false>(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            None,
            Some(heap),
            Some(executable),
            Some(recycle),
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
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            None,
            Some(heap),
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn poll_with_heap_and_state(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        state: &mut dyn InterpreterState,
        heap: &mut Heap,
        executable: Option<&crate::executable::ExecutableModule>,
        recycle: Option<&mut Option<FrameArena>>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            None,
            None,
            Some(state),
            Some(heap),
            executable,
            recycle,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn poll_with_host_heap_and_state(
        module: &VerifiedModule,
        continuation: InterpreterContinuation,
        fuel: FuelState,
        costs: &OpcodeCostTable,
        host: &mut dyn InterpreterHost,
        state: &mut dyn InterpreterState,
        heap: &mut Heap,
        executable: Option<&crate::executable::ExecutableModule>,
        recycle: Option<&mut Option<FrameArena>>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        Self::execute::<false, true>(
            module,
            continuation,
            fuel,
            costs,
            Some(host),
            None,
            Some(state),
            Some(heap),
            executable,
            recycle,
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
            OpcodeCostTable::canonical(),
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
        let continuation = Self::start_with_heap(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
            heap,
        )?;
        Self::poll_with_heap(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            OpcodeCostTable::canonical(),
            heap,
        )
    }

    /// F3: one-shot run through predecoded rows (see
    /// [`Self::poll_with_heap_and_executable`] for the parity contract).
    pub fn run_with_heap_and_executable(
        module: &VerifiedModule,
        function: u32,
        arguments: &[RuntimeValue],
        fuel: u64,
        heap: &mut Heap,
        executable: &crate::executable::ExecutableModule,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let limits = FrameLimits::default();
        let continuation = Self::start_with_heap(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
            heap,
        )?;
        Self::poll_with_heap_and_executable(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            OpcodeCostTable::canonical(),
            heap,
            executable,
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
        Self::execute::<false, true>(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            OpcodeCostTable::canonical(),
            None,
            Some(migration),
            None,
            None,
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
        let continuation = Self::start_with_heap(
            module,
            function,
            arguments,
            limits,
            ContinuationReservation::for_limits(limits),
            heap,
        )?;
        Self::execute::<false, true>(
            module,
            continuation,
            FuelState::new(fuel, 0, u64::MAX),
            OpcodeCostTable::canonical(),
            None,
            Some(migration),
            None,
            Some(heap),
            None,
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
            OpcodeCostTable::canonical(),
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
    fn execute<const PREDECODED: bool, const PROFILING: bool>(
        module: &VerifiedModule,
        mut continuation: InterpreterContinuation,
        mut fuel: FuelState,
        costs: &OpcodeCostTable,
        mut host: Option<&mut dyn InterpreterHost>,
        mut migration: Option<&mut dyn InterpreterMigration>,
        mut state_registry: Option<&mut dyn InterpreterState>,
        mut heap: Option<&mut Heap>,
        executable: Option<&crate::executable::ExecutableModule>,
        mut recycle: Option<&mut Option<FrameArena>>,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        if PREDECODED {
            let executable = executable.ok_or(InterpreterError::TypeMismatch)?;
            if executable.cost_table_version() != costs.version {
                return Err(InterpreterError::OpcodeCostTableVersion {
                    expected: executable.cost_table_version(),
                    actual: costs.version,
                });
            }
        } else {
            costs.validate_version()?;
        }
        // H1: on terminal exits the caller may reclaim the continuation's
        // arena storage through this slot; suspension paths never touch it.
        macro_rules! reclaim_storage {
            () => {
                if let Some(slot) = recycle.as_deref_mut() {
                    *slot = Some(continuation.recycle_storage());
                }
            };
        }
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::ArrayIndexOutOfBounds,
                            "array index out of bounds",
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::ArrayIndexOutOfBounds,
                            "array index out of bounds",
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::BufferIndexOutOfBounds,
                            "buffer index out of bounds",
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::BufferIndexOutOfBounds,
                            "buffer index out of bounds",
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::BytecodeTrap,
                            message,
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
                    }
                }
            };
        }
        if let Some(migration) = migration.as_deref_mut() {
            migration.observe_call_depth(continuation.arena.depth());
        }
        // WP15/WP16: the enabled flag is read once per poll; the disabled
        // hot path costs one predictable branch per instruction.
        let mut profile_module = if PROFILING && crate::profiler::enabled() {
            crate::profiler::begin_module(module)
        } else {
            None
        };
        // K2: the verified function metadata and its predecoded rows are
        // immutable for the whole poll, so they are re-resolved only when
        // the executing function changes (Call/Return/defer boundaries)
        // instead of once per instruction.
        let mut cached_function: Option<CachedFunctionMetadata<'_>> = None;
        loop {
            let frame = *continuation.arena.current()?;
            continuation.current_function = frame.function;
            let (function, function_rows, function_profile_rows) = match cached_function {
                Some((cached_id, function, rows, profile_rows)) if cached_id == frame.function => {
                    (function, rows, profile_rows)
                }
                _ => {
                    let function = module
                        .module()
                        .functions
                        .get(frame.function as usize)
                        .ok_or(InterpreterError::MissingFunction(frame.function))?;
                    let (rows, profile_rows) = if PREDECODED {
                        let executable_function = executable
                            .expect("predecoded execution requires an executable module")
                            .functions()
                            .get(frame.function as usize)
                            .ok_or(InterpreterError::FellOffFunction)?;
                        (
                            Some(executable_function.rows()),
                            profile_module
                                .as_ref()
                                .map(|_| executable_function.profile_rows()),
                        )
                    } else {
                        let executable_function = executable
                            .and_then(|rows| rows.functions().get(frame.function as usize));
                        (
                            executable_function.map(crate::executable::ExecutableFunction::rows),
                            profile_module.as_ref().and_then(|_| {
                                executable_function
                                    .map(crate::executable::ExecutableFunction::profile_rows)
                            }),
                        )
                    };
                    if let Some(profile_module) = profile_module.as_mut() {
                        profile_module.resolve_function(frame.function);
                    }
                    cached_function = Some((frame.function, function, rows, profile_rows));
                    (function, rows, profile_rows)
                }
            };
            let instruction_cost;
            let fuel_boundary;
            let resolved_nominal;
            let instruction = *function
                .code
                .get(frame.pc as usize)
                .ok_or(InterpreterError::FellOffFunction)?;
            if PREDECODED {
                let rows =
                    function_rows.expect("predecoded execution caches executable metadata rows");
                // F2: the predecoded row carries the full static charge
                // (HostCall import surcharge folded at build time) and the
                // load-time safepoint flag; only operand-dependent
                // surcharges still consult the arena and heap.
                let row = rows
                    .get(frame.pc as usize)
                    .ok_or(InterpreterError::FellOffFunction)?;
                resolved_nominal = row.resolved_nominal;
                instruction_cost = if row.dynamic_fuel() {
                    dynamic_instruction_fuel(
                        instruction,
                        &continuation.arena,
                        heap.as_deref(),
                        DynamicFuelPlan {
                            module: module.module(),
                            nominal_shape: module.nominal_index_shape(),
                            costs,
                            resolved: resolved_nominal,
                            predecoded_base: Some(row.attempt_fuel),
                        },
                    )?
                } else {
                    row.attempt_fuel
                };
                fuel_boundary = row.fuel_boundary();
            } else if let Some(rows) = function_rows {
                let row = rows
                    .get(frame.pc as usize)
                    .ok_or(InterpreterError::FellOffFunction)?;
                resolved_nominal = row.resolved_nominal;
                instruction_cost = if row.dynamic_fuel() {
                    dynamic_instruction_fuel(
                        instruction,
                        &continuation.arena,
                        heap.as_deref(),
                        DynamicFuelPlan {
                            module: module.module(),
                            nominal_shape: module.nominal_index_shape(),
                            costs,
                            resolved: resolved_nominal,
                            predecoded_base: Some(row.attempt_fuel),
                        },
                    )?
                } else {
                    row.attempt_fuel
                };
                fuel_boundary = row.fuel_boundary();
            } else {
                resolved_nominal = module
                    .resolved_operand(frame.function as usize, frame.pc as usize)
                    .into();
                instruction_cost = instruction_attempt_fuel(
                    module.module(),
                    module.nominal_index_shape(),
                    instruction,
                    &continuation.arena,
                    heap.as_deref(),
                    costs,
                    resolved_nominal,
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
                let safepoint = is_safepoint(instruction, frame.pc);
                let host_resume = frame.pc > 0
                    && matches!(
                        function.code.get(frame.pc as usize - 1),
                        Some(Instruction::HostCall { .. })
                    );
                fuel_boundary = frame.pc == 0 || host_resume || safepoint;
            }
            let settlement = pending_cost
                .checked_add(instruction_cost)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            if fuel_boundary {
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
            if let Some(profile_module) = profile_module.as_mut() {
                if let Some(execution_row) = function_rows
                    .and_then(|rows| rows.get(frame.pc as usize))
                    .copied()
                {
                    crate::profiler::record_instruction(
                        profile_module,
                        execution_row.profile_opcode(),
                    );
                    if execution_row.has_profile_event() {
                        let profile_row = function_profile_rows
                            .and_then(|rows| rows.get(frame.pc as usize))
                            .copied()
                            .ok_or(InterpreterError::FellOffFunction)?;
                        crate::profiler::record_instruction_event(
                            profile_module,
                            frame.function,
                            profile_row.allocation.map(|(kind, type_id)| {
                                crate::profiler::AllocationEvent {
                                    pc: frame.pc,
                                    source_span: profile_row.source_span,
                                    kind,
                                    type_id,
                                }
                            }),
                            profile_row.host_call,
                        );
                    }
                } else {
                    let allocation = allocation_profile(
                        instruction,
                        module.resolved_operand(frame.function as usize, frame.pc as usize),
                    )
                    .map(|(kind, type_id)| crate::profiler::AllocationEvent {
                        pc: frame.pc,
                        source_span: module.module().source_span(frame.function, frame.pc),
                        kind,
                        type_id,
                    });
                    let host_call = if let Instruction::HostCall { import, .. } = instruction {
                        module
                            .module()
                            .host_imports
                            .get(import as usize)
                            .map(|host| (host.stable_id, host.mode))
                    } else {
                        None
                    };
                    crate::profiler::record_instruction(profile_module, opcode_index(instruction));
                    if allocation.is_some() || host_call.is_some() {
                        crate::profiler::record_instruction_event(
                            profile_module,
                            frame.function,
                            allocation,
                            host_call,
                        );
                    }
                }
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
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    // WP56: executable modules allocate immutable literal
                    // bytes and hashes once at load. The heap publishes only
                    // a GC header plus Arc reference; the portable path keeps
                    // the content-keyed fallback.
                    let (reference, hash) = if let Some((pool, constant)) =
                        executable.and_then(|image| image.pooled_string(string))
                    {
                        heap.load_pooled_string(
                            pool,
                            string,
                            std::sync::Arc::clone(&constant.value),
                            constant.hash,
                        )?
                    } else {
                        let value = module
                            .module()
                            .strings
                            .get(string as usize)
                            .ok_or(InterpreterError::TypeMismatch)?;
                        heap.load_string_literal_with_hash(value)?
                    };
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
                Instruction::CopyValue { dst, source, slots } => {
                    continuation.arena.copy_register_range(source, dst, slots)?;
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
                            let trap = Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::DivideByZero,
                                "integer division or remainder by zero",
                            );
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                            let trap = Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::DivideByZero,
                                "integer division or remainder by zero",
                            );
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                    if !matches!(
                        (instruction, value),
                        (Instruction::I32ToString { .. }, RuntimeValue::I32(_))
                            | (Instruction::I64ToString { .. }, RuntimeValue::I64(_))
                            | (Instruction::F32ToString { .. }, RuntimeValue::F32(_))
                            | (Instruction::F64ToString { .. }, RuntimeValue::F64(_))
                            | (Instruction::BoolToString { .. }, RuntimeValue::Bool(_))
                            | (Instruction::RuneToString { .. }, RuntimeValue::Rune(_))
                    ) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let mut text = ScalarText::new();
                    write_scalar_text(value, &mut text)?;
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
                    dst,
                    ..
                } => {
                    match run_physical_standard_intrinsic(
                        intrinsic,
                        args_base,
                        dst,
                        &mut continuation.arena,
                        heap.as_deref_mut(),
                        module.layout_table(),
                        resolved_nominal,
                    )? {
                        PhysicalStandardIntrinsicOutcome::Returned => {
                            increment_pc(&mut continuation.arena)?;
                        }
                        PhysicalStandardIntrinsicOutcome::Retry => {}
                        PhysicalStandardIntrinsicOutcome::Trapped(message) => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            let trap = Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::StandardLibrary,
                                message,
                            );
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                Instruction::StringBuild {
                    dst,
                    parts_base,
                    parts_count,
                } => {
                    let parts = crate::trusted::read_register_window(
                        &continuation.arena,
                        parts_base,
                        parts_count,
                    )?;
                    let value = build_runtime_string(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?,
                        parts,
                    )?;
                    set_register(&mut continuation.arena, dst, value)?;
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
                        let trap = Trap::from_continuation(
                            module,
                            &continuation,
                            TrapKind::StringIndexOutOfBounds,
                            "string rune index out of bounds",
                        );
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                    if function_rows.is_some() {
                        let ExecutableNominalOperand::CallFrame {
                            register_count,
                            parameter_slots,
                            result_slots,
                        } = resolved_nominal
                        else {
                            return Err(InterpreterError::TypeMismatch);
                        };
                        let abi = module
                            .module_abi()
                            .function(callee_id as usize)
                            .filter(|abi| abi.parameter_slots == parameter_slots)
                            .ok_or(InterpreterError::TypeMismatch)?;
                        continuation
                            .arena
                            .push_verified_abi_call(VerifiedCallPlan {
                                function: callee_id,
                                register_count,
                                return_range: ReturnRange {
                                    start: dst,
                                    slots: result_slots.max(1),
                                },
                                call_site_pc: frame.pc,
                                args_base,
                                args_slots: args_count,
                                abi,
                            })?;
                    } else {
                        let callee = module
                            .module()
                            .functions
                            .get(callee_id as usize)
                            .ok_or(InterpreterError::MissingFunction(callee_id))?;
                        let abi = module
                            .module_abi()
                            .function(callee_id as usize)
                            .ok_or(InterpreterError::TypeMismatch)?;
                        if args_count != abi.parameter_slots {
                            return Err(InterpreterError::ArgumentCount);
                        }
                        let result_slots =
                            abi.result.as_ref().map_or(0, |result| result.slot_count);
                        continuation
                            .arena
                            .push_verified_abi_call(VerifiedCallPlan {
                                function: callee_id,
                                register_count: callee.registers,
                                return_range: ReturnRange {
                                    start: dst,
                                    slots: result_slots.max(1),
                                },
                                call_site_pc: frame.pc,
                                args_base,
                                args_slots: args_count,
                                abi,
                            })?;
                    }
                    if let Some(migration) = migration.as_deref_mut() {
                        migration.observe_call_depth(continuation.arena.depth());
                    }
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
                    if metadata.parameters.len() > 8 {
                        return Err(InterpreterError::ArgumentCount);
                    }
                    let layouts = module.layout_table();
                    let expected_argument_slots =
                        metadata.parameters.iter().try_fold(0_u16, |total, ty| {
                            total
                                .checked_add(
                                    layouts
                                        .physical_slots(*ty)
                                        .map_err(|_| InterpreterError::TypeMismatch)?,
                                )
                                .ok_or(InterpreterError::ArgumentCount)
                        })?;
                    if args_count != expected_argument_slots {
                        return Err(InterpreterError::ArgumentCount);
                    }
                    let result_slots = metadata
                        .result
                        .map(|ty| {
                            layouts
                                .physical_slots(ty)
                                .map_err(|_| InterpreterError::TypeMismatch)
                        })
                        .transpose()?
                        .unwrap_or(0);
                    continuation.host_call_boundary = Some(HostCallBoundary {
                        import,
                        function: frame.function,
                        pc: frame.pc,
                        source_span: module.module().source_span(frame.function, frame.pc),
                    });
                    if metadata.result.is_some() {
                        continuation.arena.clear_register_range(dst, result_slots)?;
                    }
                    let argument_slots =
                        continuation.arena.register_range(args_base, args_count)?;
                    let arguments = InterpreterHostArguments::new(
                        argument_slots,
                        &metadata.parameters,
                        layouts,
                    );
                    let outcome = match host
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .call(import, arguments, heap.as_deref_mut())
                    {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                            let (code, argument) = match error {
                                crate::HostTrap::UnknownFunction(function) => {
                                    ("NX4001", function.0)
                                }
                                crate::HostTrap::InvalidFunctionSlot(slot) => {
                                    ("NX4001", u64::from(slot.index()))
                                }
                                crate::HostTrap::Arity => ("NX4003", 0),
                                crate::HostTrap::Type => ("NX4003", 1),
                                crate::HostTrap::ResourceCapacity => ("NX5004", 0),
                                crate::HostTrap::Panicked => ("NX5001", 0),
                                crate::HostTrap::Host(_) => ("NX5001", 1),
                            };
                            let trap = Trap::from_continuation(
                                module,
                                &continuation,
                                TrapKind::Host,
                                crate::RuntimeMessage::Code {
                                    code: crate::DiagnosticCode::new(code),
                                    argument,
                                },
                            );
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
                        }
                    };
                    match outcome {
                        InterpreterHostOutcome::Immediate(value) => {
                            if metadata.result != runtime_value_type(value) {
                                settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                                let trap = Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::Host,
                                    crate::RuntimeMessage::Code {
                                        code: crate::DiagnosticCode::new("NX5001"),
                                        argument: 2,
                                    },
                                );
                                reclaim_storage!();
                                return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
                            }
                            if let Some(result) = metadata.result {
                                write_materialized_value_range(
                                    &mut continuation.arena,
                                    dst,
                                    result,
                                    value,
                                    layouts,
                                    heap.as_deref(),
                                )?;
                            }
                            increment_pc(&mut continuation.arena)?;
                            continuation.host_call_boundary = None;
                        }
                        InterpreterHostOutcome::Pending(request) => {
                            if metadata.mode != HostCallMode::Async {
                                settle_terminal_cost(&mut fuel, &mut charge, pending_cost)?;
                                let trap = Trap::from_continuation(
                                    module,
                                    &continuation,
                                    TrapKind::Host,
                                    crate::RuntimeMessage::Code {
                                        code: crate::DiagnosticCode::new("NX5001"),
                                        argument: 3,
                                    },
                                );
                                reclaim_storage!();
                                return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                    let (field_index, expected) = resolved_state_field(module, resolved_nominal)?;
                    if expected != ty {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let value = migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .old_field_get_dense(object, field_id, field_index, ty)
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
                    let (field_index, expected) = resolved_state_field(module, resolved_nominal)?;
                    migration
                        .as_deref_mut()
                        .ok_or(InterpreterError::HostUnavailable)?
                        .new_set_dense(object, field_id, field_index, expected, value)
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
                Instruction::EnumNew { payload, dst, .. } => {
                    let ExecutableNominalOperand::EnumVariant {
                        tag,
                        payload_offset,
                        payload_slots,
                        owner_slots,
                        ..
                    } = resolved_nominal
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    continuation.arena.clear_register_range(dst, owner_slots)?;
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(
                            i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?,
                        ),
                    )?;
                    match (payload, payload_slots) {
                        (Some(payload), slots) if slots > 0 => {
                            let destination = dst
                                .checked_add(payload_offset)
                                .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                            continuation
                                .arena
                                .copy_register_range(payload, destination, slots)?;
                        }
                        (None, 0) => {}
                        _ => return Err(InterpreterError::TypeMismatch),
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumTag { source, dst } => {
                    let ExecutableNominalOperand::EnumLayout { .. } = resolved_nominal else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let tag = register(&continuation.arena, source)?;
                    if !matches!(tag, RuntimeValue::I32(_)) {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    set_register(&mut continuation.arena, dst, tag)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumPayload { source, dst, .. } => {
                    let ExecutableNominalOperand::EnumVariant {
                        tag,
                        payload_offset,
                        payload_slots,
                        ..
                    } = resolved_nominal
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    if register(&continuation.arena, source)?
                        != RuntimeValue::I32(
                            i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?,
                        )
                    {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let payload_source = source
                        .checked_add(payload_offset)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    continuation
                        .arena
                        .copy_register_range(payload_source, dst, payload_slots)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::EnumEqual { lhs, rhs, dst } => {
                    let ExecutableNominalOperand::EnumLayout { slots, .. } = resolved_nominal
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let equal = physical_ranges_equal(
                        &continuation.arena,
                        lhs,
                        rhs,
                        slots,
                        heap.as_deref(),
                    )?;
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(equal))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructNew {
                    fields_base,
                    fields_count,
                    dst,
                    ..
                } => {
                    let ExecutableNominalOperand::StructLayout { slots } = resolved_nominal else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    if slots != fields_count {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    continuation
                        .arena
                        .copy_register_range(fields_base, dst, slots)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructGet { source, dst, .. } => {
                    let ExecutableNominalOperand::StructField { offset, slots, .. } =
                        resolved_nominal
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let field_source = source
                        .checked_add(offset)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    continuation
                        .arena
                        .copy_register_range(field_source, dst, slots)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructWith {
                    source, value, dst, ..
                } => {
                    let ExecutableNominalOperand::StructField {
                        offset,
                        slots,
                        owner_slots,
                        ..
                    } = resolved_nominal
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    continuation
                        .arena
                        .copy_register_range(source, dst, owner_slots)?;
                    let field_destination = dst
                        .checked_add(offset)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    continuation
                        .arena
                        .copy_register_range(value, field_destination, slots)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::StructEqual { lhs, rhs, dst } => {
                    let ExecutableNominalOperand::StructLayout { slots } = resolved_nominal else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let equal = physical_ranges_equal(
                        &continuation.arena,
                        lhs,
                        rhs,
                        slots,
                        heap.as_deref(),
                    )?;
                    set_register(&mut continuation.arena, dst, RuntimeValue::Bool(equal))?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassNew {
                    type_id,
                    fields_base,
                    fields_count,
                    dst,
                } => {
                    let fields = crate::trusted::read_register_window(
                        &continuation.arena,
                        fields_base,
                        fields_count,
                    )?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    let value = heap.allocate_class(type_id, fields)?;
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
                    let resolved = resolved_class_field(module, resolved_nominal)?;
                    match value {
                        RuntimeValue::NamedRef { .. } => {
                            let field_values = heap
                                .as_deref()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .class_field_range(value, resolved.offset, resolved.slots)?;
                            continuation.arena.write_register_values(
                                dst,
                                u16::try_from(resolved.slots)
                                    .map_err(|_| InterpreterError::TypeMismatch)?,
                                field_values.iter(),
                            )?;
                        }
                        RuntimeValue::Opaque {
                            value: stable_id, ..
                        } => {
                            if resolved.slots != 1 {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            let state_index =
                                resolved.state_index.ok_or(InterpreterError::TypeMismatch)?;
                            let field_value = state_registry.as_deref_mut().map_or_else(
                                || {
                                    Err(RuntimeMessage::Static(
                                        "current state registry is unavailable",
                                    ))
                                },
                                |registry| {
                                    registry.object_field_dense(
                                        StableId(stable_id),
                                        type_id,
                                        field,
                                        state_index,
                                        resolved.expected,
                                    )
                                },
                            );
                            let field_value = state_operation!(field_value);
                            if runtime_value_type(field_value) != Some(resolved.expected) {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            set_register(&mut continuation.arena, dst, field_value)?;
                        }
                        _ => unreachable!("class receiver was checked above"),
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ClassSet {
                    source,
                    field,
                    value,
                } => {
                    let object = register(&continuation.arena, source)?;
                    let (RuntimeValue::NamedRef { type_id, .. }
                    | RuntimeValue::Opaque { type_id, .. }) = object
                    else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let resolved = resolved_class_field(module, resolved_nominal)?;
                    match object {
                        RuntimeValue::NamedRef { .. } => {
                            let replacement = crate::trusted::read_register_window(
                                &continuation.arena,
                                value,
                                u16::try_from(resolved.slots)
                                    .map_err(|_| InterpreterError::TypeMismatch)?,
                            )?;
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .set_class_field_range(object, resolved.offset, replacement)?;
                        }
                        RuntimeValue::Opaque {
                            value: stable_id, ..
                        } => {
                            if resolved.slots != 1 {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            let replacement = register(&continuation.arena, value)?;
                            if runtime_value_type(replacement) != Some(resolved.expected) {
                                return Err(InterpreterError::TypeMismatch);
                            }
                            let state_index =
                                resolved.state_index.ok_or(InterpreterError::TypeMismatch)?;
                            let update = state_registry.as_deref_mut().map_or_else(
                                || {
                                    Err(RuntimeMessage::Static(
                                        "current state registry is unavailable",
                                    ))
                                },
                                |registry| {
                                    registry.set_object_field_dense(
                                        StableId(stable_id),
                                        type_id,
                                        field,
                                        state_index,
                                        resolved.expected,
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
                    let (element_type, row_fields) =
                        resolved_array_layout(module, type_id, resolved_nominal)?;
                    let heap = heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?;
                    // WP52: struct elements flatten into arena rows - one
                    // field cell per struct field, zero objects per element.
                    // Named element types that are not structs (classes,
                    // enums) and fieldless structs keep the cell layout.
                    let value = match row_fields {
                        Some(field_count) => {
                            heap.allocate_value_row_array(type_id, element_type, field_count)?
                        }
                        None => heap.allocate_array(type_id, element_type)?,
                    };
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
                    let (element, element_slots, _row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = array_index!(index);
                    let values = array_operation!(
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_value_range(array, index)
                    );
                    let destination = continuation.arena.register_range_mut(dst, element_slots)?;
                    write_collection_element_physical(
                        values,
                        destination,
                        element,
                        module.layout_table(),
                        heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?,
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayFieldGet {
                    source,
                    index,
                    field: _,
                    dst,
                } => {
                    let (offset, slots, _row_slots) = resolved_array_field(resolved_nominal)?;
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = array_index!(index);
                    let values = array_operation!(
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_field_range(
                                array,
                                index,
                                usize::from(offset),
                                usize::from(slots),
                            )
                    );
                    continuation
                        .arena
                        .write_register_values(dst, slots, values.iter())?;
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
                    let index = array_index!(index);
                    let (element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    let physical = crate::trusted::read_register_window(
                        &continuation.arena,
                        value,
                        element_slots,
                    )?;
                    if row_slots.is_some() {
                        array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_set_row(array, index, physical)
                        );
                    } else {
                        let heap = heap
                            .as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?;
                        let replacement = materialize_physical_value_range(
                            physical,
                            element,
                            module.layout_table(),
                            Some(heap),
                        )?;
                        array_operation!(heap.array_set(array, index, replacement));
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayPush { source, value } => {
                    let array = register(&continuation.arena, source)?;
                    let (element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    let physical = crate::trusted::read_register_window(
                        &continuation.arena,
                        value,
                        element_slots,
                    )?;
                    if row_slots.is_some() {
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .array_push_row(array, physical)?;
                    } else {
                        let heap = heap
                            .as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?;
                        let value = materialize_physical_value_range(
                            physical,
                            element,
                            module.layout_table(),
                            Some(heap),
                        )?;
                        heap.array_push(array, value)?;
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayPushRow {
                    source,
                    fields_base,
                    fields_count,
                } => {
                    let (_element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    if row_slots.is_none() || fields_count != element_slots {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let array = register(&continuation.arena, source)?;
                    let fields = crate::trusted::read_register_window(
                        &continuation.arena,
                        fields_base,
                        fields_count,
                    )?;
                    heap.as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .array_push_row(array, fields)?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayPop { source, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let (element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    if row_slots.is_some() {
                        let heap_ref = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
                        let index = heap_ref.array_len(array)?.saturating_sub(1);
                        let values = array_operation!(heap_ref.array_value_range(array, index));
                        continuation.arena.write_register_values(
                            dst,
                            element_slots,
                            values.iter(),
                        )?;
                        array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_pop_row_discard(array)
                        );
                    } else {
                        let value = array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_pop(array)
                        );
                        write_materialized_value_range(
                            &mut continuation.arena,
                            dst,
                            element,
                            value,
                            module.layout_table(),
                            heap.as_deref(),
                        )?;
                    }
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
                    let index = array_index!(index);
                    let (element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    let physical = crate::trusted::read_register_window(
                        &continuation.arena,
                        value,
                        element_slots,
                    )?;
                    if row_slots.is_some() {
                        array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_insert_row(array, index, physical)
                        );
                    } else {
                        let heap = heap
                            .as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?;
                        let value = materialize_physical_value_range(
                            physical,
                            element,
                            module.layout_table(),
                            Some(heap),
                        )?;
                        array_operation!(heap.array_insert(array, index, value));
                    }
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::ArrayRemove { source, index, dst } => {
                    let array = register(&continuation.arena, source)?;
                    let RuntimeValue::I32(index) = register(&continuation.arena, index)? else {
                        return Err(InterpreterError::TypeMismatch);
                    };
                    let index = array_index!(index);
                    let (element, element_slots, row_slots) =
                        resolved_array_value(module, resolved_nominal)?;
                    if row_slots.is_some() {
                        let values = array_operation!(
                            heap.as_deref()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_value_range(array, index)
                        );
                        continuation.arena.write_register_values(
                            dst,
                            element_slots,
                            values.iter(),
                        )?;
                        array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_remove_row_discard(array, index)
                        );
                    } else {
                        let value = array_operation!(
                            heap.as_deref_mut()
                                .ok_or(InterpreterError::HeapUnavailable)?
                                .array_remove(array, index)
                        );
                        write_materialized_value_range(
                            &mut continuation.arena,
                            dst,
                            element,
                            value,
                            module.layout_table(),
                            heap.as_deref(),
                        )?;
                    }
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
                    let (key, value) = resolved_map_layout(module, type_id, resolved_nominal)?;
                    let (_key_type, _value_type, key_slots, value_slots, _, _) =
                        resolved_map_value(module, resolved_nominal)?;
                    if key_slots != 1 {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let value = allocate_resolved_map(
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?,
                        module,
                        type_id,
                        key,
                        value,
                        value_slots,
                    )?;
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
                    result_type: _,
                    dst,
                }
                | Instruction::MapRemove {
                    source,
                    key,
                    result_type: _,
                    dst,
                } => {
                    let (
                        _key_type,
                        _value_type,
                        key_slots,
                        value_slots,
                        option_slots,
                        payload_offset,
                    ) = resolved_map_value(module, resolved_nominal)?;
                    if key_slots != 1 || value_slots == 0 || option_slots == 0 {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let map = register(&continuation.arena, source)?;
                    let key = register(&continuation.arena, key)?;
                    continuation.arena.clear_register_range(dst, option_slots)?;
                    let payload = dst
                        .checked_add(payload_offset)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    let destination = continuation
                        .arena
                        .register_range_mut(payload, value_slots)?;
                    let found = if matches!(instruction, Instruction::MapGet { .. }) {
                        heap.as_deref()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .map_get_value_into(map, key, destination)?
                    } else {
                        heap.as_deref_mut()
                            .ok_or(InterpreterError::HeapUnavailable)?
                            .map_remove_value_into(map, key, destination)?
                    };
                    set_register(
                        &mut continuation.arena,
                        dst,
                        RuntimeValue::I32(i32::from(found)),
                    )?;
                    increment_pc(&mut continuation.arena)?;
                }
                Instruction::MapSet { source, key, value } => {
                    let (_key_type, _value_type, key_slots, value_slots, _, _) =
                        resolved_map_value(module, resolved_nominal)?;
                    if key_slots != 1 || value_slots == 0 {
                        return Err(InterpreterError::TypeMismatch);
                    }
                    let map = register(&continuation.arena, source)?;
                    let key = register(&continuation.arena, key)?;
                    let value = crate::trusted::read_register_window(
                        &continuation.arena,
                        value,
                        value_slots,
                    )?;
                    if heap
                        .as_deref_mut()
                        .ok_or(InterpreterError::HeapUnavailable)?
                        .map_set_value_range(map, key, value)?
                        == MapSetOutcome::Complete
                    {
                        increment_pc(&mut continuation.arena)?;
                    }
                }
                Instruction::MapContains { source, key, dst } => {
                    let (_key_type, _value_type, key_slots, _value_slots, _, _) =
                        resolved_map_value(module, resolved_nominal)?;
                    if key_slots != 1 {
                        return Err(InterpreterError::TypeMismatch);
                    }
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
                    if continuation.arena.depth() > 1
                        && continuation.arena.current()?.return_range.is_some()
                    {
                        let returning_function = continuation.arena.current()?.function as usize;
                        let result_slots = module
                            .module_abi()
                            .function(returning_function)
                            .and_then(|abi| abi.result.as_ref())
                            .map(|result| result.slot_count)
                            .ok_or(InterpreterError::TypeMismatch)?;
                        continuation
                            .arena
                            .return_verified_range(source, result_slots)?;
                        pending_cost = 0;
                        continue;
                    }
                    let returning_function = continuation.arena.current()?.function as usize;
                    let result_type = module
                        .module()
                        .functions
                        .get(returning_function)
                        .and_then(|function| function.signature.result)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let result_slots = module
                        .layout_table()
                        .physical_slots(result_type)
                        .map_err(|_| InterpreterError::TypeMismatch)?;
                    let result_range = continuation.arena.register_range(source, result_slots)?;
                    let result = materialize_physical_value_range(
                        result_range,
                        result_type,
                        module.layout_table(),
                        heap.as_deref_mut(),
                    )?;
                    let completed = continuation.arena.pop_verified()?;
                    let returning_cleanup =
                        completed.return_range.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup {
                        if continuation.cleanup_mode
                            && !start_next_defer(module, &mut continuation.arena)?
                        {
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Returned {
                                value: None,
                                charge,
                                fuel,
                            });
                        }
                        pending_cost = 0;
                        continue;
                    }
                    reclaim_storage!();
                    return Ok(InterpreterOutcome::Returned {
                        value: Some(result),
                        charge,
                        fuel,
                    });
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
                    let completed = continuation.arena.pop_verified()?;
                    let returning_cleanup =
                        completed.return_range.is_none() && continuation.arena.depth() > 0;
                    if returning_cleanup {
                        if continuation.cleanup_mode
                            && !start_next_defer(module, &mut continuation.arena)?
                        {
                            reclaim_storage!();
                            return Ok(InterpreterOutcome::Returned {
                                value: None,
                                charge,
                                fuel,
                            });
                        }
                        pending_cost = 0;
                        continue;
                    } else if continuation.arena.depth() == 0 {
                        reclaim_storage!();
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
                    let trap = Trap::from_continuation(
                        module,
                        &continuation,
                        TrapKind::BytecodeTrap,
                        "bytecode trap",
                    );
                    reclaim_storage!();
                    return Ok(InterpreterOutcome::Trapped { trap, charge, fuel });
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
                    continuation.arena.push_defer_call_physical(
                        function,
                        &arguments[..usize::from(args_count)],
                    )?;
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
                    continuation.arena.pop_verified()?;
                    if continuation.cleanup_mode
                        && continuation.arena.depth() > 0
                        && !start_next_defer(module, &mut continuation.arena)?
                    {
                        reclaim_storage!();
                        return Ok(InterpreterOutcome::Returned {
                            value: None,
                            charge,
                            fuel,
                        });
                    }
                    if continuation.arena.depth() == 0 {
                        reclaim_storage!();
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
            let abi = module
                .module_abi()
                .function(function as usize)
                .ok_or(InterpreterError::TypeMismatch)?;
            arena.push_call(function, cleanup.registers, None)?;
            if u16::from(args_count) != abi.parameter_slots {
                return Err(InterpreterError::ArgumentCount);
            }
            arena.initialize_packed_abi_arguments(abi, &args[..usize::from(args_count)])?;
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

enum PhysicalStandardIntrinsicOutcome {
    Returned,
    Retry,
    Trapped(crate::RuntimeMessage),
}

#[derive(Clone, Copy)]
struct PhysicalStandardIntrinsicAbi {
    argument_slots: [u16; 3],
    result_slots: u16,
}

impl PhysicalStandardIntrinsicAbi {
    fn from_resolved(resolved: ExecutableNominalOperand) -> Result<Self, InterpreterError> {
        let ExecutableNominalOperand::StandardIntrinsic {
            argument_slots,
            result_slots,
        } = resolved
        else {
            return Err(InterpreterError::TypeMismatch);
        };
        Ok(Self {
            argument_slots,
            result_slots,
        })
    }

    fn argument_base(self, args_base: u16, index: usize) -> Result<u16, InterpreterError> {
        if index >= self.argument_slots.len() || self.argument_slots[index] == 0 {
            return Err(InterpreterError::TypeMismatch);
        }
        self.argument_slots[..index]
            .iter()
            .try_fold(args_base, |base, slots| {
                base.checked_add(*slots)
                    .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))
            })
    }

    fn physical_argument_slots(self, logical_count: u16) -> Result<u16, InterpreterError> {
        self.argument_slots[..usize::from(logical_count)]
            .iter()
            .try_fold(0_u16, |total, slots| {
                if *slots == 0 {
                    return Err(InterpreterError::TypeMismatch);
                }
                total
                    .checked_add(*slots)
                    .ok_or(InterpreterError::FuelCostOverflow)
            })
    }
}

const PHYSICAL_ENUM_PAYLOAD_OFFSET: u16 = 1;

#[allow(clippy::too_many_lines)]
/// F1: attempt fuel for instructions whose whole cost is determined by the
/// module, nominal index shape, and cost table alone. Returns `None` for
/// operand-dependent instructions (their surcharge needs the frame arena or
/// the heap). `ExecutableModule` rows precompute exactly these values, and
/// `instruction_attempt_fuel` consumes this function first, so the portable
/// interpreter and predecoded rows cannot diverge by construction.
pub(crate) fn static_instruction_fuel(
    module: &nexa_bytecode::Module,
    nominal_shape: nexa_verifier::NominalIndexShape,
    instruction: Instruction,
    costs: &OpcodeCostTable,
) -> Result<Option<u64>, InterpreterError> {
    let work = match instruction {
        // Operand-dependent surcharges: dynamic by definition.
        Instruction::StandardIntrinsic { .. }
        | Instruction::StringLen { .. }
        | Instruction::StringRuneAt { .. }
        | Instruction::StringEqual { .. }
        | Instruction::StringConcat { .. }
        | Instruction::StringBuild { .. }
        | Instruction::StructGet { .. }
        | Instruction::StructWith { .. }
        | Instruction::EnumNew { .. }
        | Instruction::EnumPayload { .. }
        | Instruction::EnumEqual { .. }
        | Instruction::StructEqual { .. }
        | Instruction::ArrayGet { .. }
        | Instruction::ArrayFieldGet { .. }
        | Instruction::ArraySet { .. }
        | Instruction::ClassGet { .. }
        | Instruction::ClassSet { .. }
        | Instruction::ArrayPush { .. }
        | Instruction::ArrayPushRow { .. }
        | Instruction::ArrayInsert { .. }
        | Instruction::ArrayPop { .. }
        | Instruction::ArrayRemove { .. }
        | Instruction::ArrayClear { .. }
        | Instruction::MapGet { .. }
        | Instruction::MapRemove { .. }
        | Instruction::MapContains { .. }
        | Instruction::MapSet { .. }
        | Instruction::MapClear { .. }
        | Instruction::BufferSlice { .. }
        | Instruction::BufferCopy { .. }
        | Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn => return Ok(None),
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
        Instruction::CopyValue { slots, .. } => value_visit_fuel(u64::from(slots), 1)?,
        Instruction::StructNew { fields_count, .. } => {
            value_visit_fuel(u64::from(fields_count), 1)?
        }
        Instruction::ClassNew { fields_count, .. } => value_visit_fuel(u64::from(fields_count), 2)?,
        Instruction::ArrayNew { .. } => nominal_index_lookup_fuel(nominal_shape.array_types)?,
        Instruction::MapNew { .. } => nominal_index_lookup_fuel(nominal_shape.map_types)?,
        _ => 0,
    };
    Ok(Some(fuel_add(costs.cost(instruction), work)?))
}

fn instruction_attempt_fuel(
    module: &nexa_bytecode::Module,
    nominal_shape: nexa_verifier::NominalIndexShape,
    instruction: Instruction,
    arena: &FrameArena,
    heap: Option<&Heap>,
    costs: &OpcodeCostTable,
    resolved: ExecutableNominalOperand,
) -> Result<u64, InterpreterError> {
    // F1 single source of truth: instructions whose whole attempt fuel is
    // known at module load time settle here; only operand-dependent
    // surcharges fall through to the dynamic arms below.
    if let Some(static_fuel) = static_instruction_fuel(module, nominal_shape, instruction, costs)? {
        return Ok(static_fuel);
    }
    dynamic_instruction_fuel(
        instruction,
        arena,
        heap,
        DynamicFuelPlan {
            module,
            nominal_shape,
            costs,
            resolved,
            predecoded_base: None,
        },
    )
}

/// Operand-dependent attempt-fuel surcharges (frame arena or heap inputs).
/// Reached only for instructions [`static_instruction_fuel`] declines.
pub(crate) fn physical_instruction_work_fuel(
    nominal_shape: nexa_verifier::NominalIndexShape,
    instruction: Instruction,
    resolved: ExecutableNominalOperand,
) -> Result<Option<u64>, InterpreterError> {
    let work = match instruction {
        Instruction::StructGet { .. } => {
            let ExecutableNominalOperand::StructField { slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.struct_fields)?,
                value_visit_fuel(u64::from(slots), 1)?,
            )?
        }
        Instruction::EnumNew { .. } => {
            let ExecutableNominalOperand::EnumVariant {
                payload_slots,
                owner_slots,
                ..
            } = resolved
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.enum_variants)?,
                value_visit_fuel(u64::from(owner_slots) + u64::from(payload_slots), 1)?,
            )?
        }
        Instruction::EnumPayload { .. } => {
            let ExecutableNominalOperand::EnumVariant { payload_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.enum_variants)?,
                value_visit_fuel(u64::from(payload_slots), 1)?,
            )?
        }
        Instruction::EnumEqual { .. } => {
            let ExecutableNominalOperand::EnumLayout { slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(slots), 1)?
        }
        Instruction::ClassGet { .. } | Instruction::ClassSet { .. } => {
            let ExecutableNominalOperand::ClassField { slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.class_fields)?,
                value_visit_fuel(u64::from(slots), 1)?,
            )?
        }
        Instruction::ArrayGet { .. } | Instruction::ArraySet { .. } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(element_slots), 1)?
        }
        Instruction::ArrayFieldGet { .. } => {
            let ExecutableNominalOperand::ArrayField { slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(slots), 1)?
        }
        Instruction::MapGet { .. } | Instruction::MapRemove { .. } => {
            let ExecutableNominalOperand::MapLayout {
                value_slots,
                option_slots,
                ..
            } = resolved
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(option_slots) + u64::from(value_slots), 1)?
        }
        Instruction::MapSet { .. } => {
            let ExecutableNominalOperand::MapLayout {
                key_slots,
                value_slots,
                ..
            } = resolved
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(key_slots) + u64::from(value_slots), 1)?
        }
        _ => return Ok(None),
    };
    Ok(Some(work))
}

#[derive(Clone, Copy)]
struct DynamicFuelPlan<'a> {
    module: &'a nexa_bytecode::Module,
    nominal_shape: nexa_verifier::NominalIndexShape,
    costs: &'a OpcodeCostTable,
    resolved: ExecutableNominalOperand,
    predecoded_base: Option<u64>,
}

#[allow(clippy::too_many_lines)]
fn dynamic_instruction_fuel(
    instruction: Instruction,
    arena: &FrameArena,
    heap: Option<&Heap>,
    plan: DynamicFuelPlan<'_>,
) -> Result<u64, InterpreterError> {
    let DynamicFuelPlan {
        module,
        nominal_shape,
        costs,
        resolved,
        predecoded_base,
    } = plan;
    let heap_required = || heap.ok_or(InterpreterError::HeapUnavailable);
    let base = predecoded_base.unwrap_or_else(|| costs.cost(instruction));
    let physical_work =
        physical_instruction_work_fuel(nominal_shape, instruction, resolved)?.unwrap_or(0);
    let work = match instruction {
        Instruction::StandardIntrinsic {
            intrinsic,
            args_base,
            args_count,
            ..
        } => {
            return standard_intrinsic_attempt_fuel(
                intrinsic, args_base, args_count, arena, heap, resolved,
            );
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
        Instruction::StringBuild {
            parts_base,
            parts_count,
            ..
        } => string_build_attempt_fuel(arena, heap_required()?, parts_base, parts_count)?,
        Instruction::Return { .. } | Instruction::ReturnVoid | Instruction::CleanupReturn => {
            return_defer_attempt_fuel(module, instruction, arena)?
        }
        Instruction::StructWith { .. } => {
            let ExecutableNominalOperand::StructField {
                slots, owner_slots, ..
            } = resolved
            else {
                return Err(InterpreterError::TypeMismatch);
            };
            fuel_add(
                nominal_index_lookup_fuel(nominal_shape.struct_fields)?,
                value_visit_fuel(u64::from(owner_slots) + u64::from(slots), 1)?,
            )?
        }
        Instruction::StructEqual { .. } => {
            let ExecutableNominalOperand::StructLayout { slots } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            value_visit_fuel(u64::from(slots), 1)?
        }
        Instruction::ArrayPush { source, .. } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            let heap = heap_required()?;
            let array = register(arena, source)?;
            let (live, capacity) = heap.array_fuel_shape(array)?;
            // Amortized push (WP49): spare capacity is a constant-cost
            // in-place write; growth relocates exactly the live prefix.
            let moved = if live < capacity { 1 } else { live.max(1) };
            let moved_slots = fuel_usize(moved)?
                .checked_mul(u64::from(element_slots))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let element_work = fuel_blocks(moved_slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            fuel_add(
                element_work,
                collection_arena_metadata_fuel(heap, live >= capacity, live >= capacity)?,
            )?
        }
        Instruction::ArrayPushRow { source, .. } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            let heap = heap_required()?;
            let array = register(arena, source)?;
            let (live, capacity) = heap.array_fuel_shape(array)?;
            let moved = if live < capacity { 1 } else { live.max(1) };
            let moved_slots = fuel_usize(moved)?
                .checked_mul(u64::from(element_slots))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let element_work = fuel_blocks(moved_slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            fuel_add(
                element_work,
                collection_arena_metadata_fuel(heap, live >= capacity, live >= capacity)?,
            )?
        }
        Instruction::ArrayInsert { source, .. } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            let heap = heap_required()?;
            let array = register(arena, source)?;
            let (live, capacity) = heap.array_fuel_shape(array)?;
            let moved = live.max(1);
            let moved_slots = fuel_usize(moved)?
                .checked_mul(u64::from(element_slots))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let element_work = fuel_blocks(moved_slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            fuel_add(
                element_work,
                collection_arena_metadata_fuel(heap, live >= capacity, live >= capacity)?,
            )?
        }
        Instruction::ArrayPop { source, .. }
        | Instruction::ArrayRemove { source, .. }
        | Instruction::ArrayClear { source } => {
            let ExecutableNominalOperand::ArrayLayout { element_slots, .. } = resolved else {
                return Err(InterpreterError::TypeMismatch);
            };
            let heap = heap_required()?;
            let old_length = heap.array_len(register(arena, source)?)?;
            let visited_slots = fuel_usize(old_length.max(1))?
                .checked_mul(u64::from(element_slots))
                .ok_or(InterpreterError::FuelCostOverflow)?;
            fuel_blocks(visited_slots, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?
        }
        Instruction::MapGet { source, key, .. } | Instruction::MapRemove { source, key, .. } => {
            map_lookup_fuel(
                heap_required()?,
                register(arena, source)?,
                register(arena, key)?,
            )?
        }
        Instruction::MapContains { source, key, .. } => map_lookup_fuel(
            heap_required()?,
            register(arena, source)?,
            register(arena, key)?,
        )?,
        Instruction::MapSet { source, key, .. } => map_insert_attempt_fuel(
            heap_required()?,
            register(arena, source)?,
            register(arena, key)?,
        )?,
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
    fuel_add(base, fuel_add(work, physical_work)?)
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
                if current.return_range.is_none() {
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

fn string_build_attempt_fuel(
    arena: &FrameArena,
    heap: &Heap,
    parts_base: u16,
    parts_count: u16,
) -> Result<u64, InterpreterError> {
    let parts = crate::trusted::read_register_window(arena, parts_base, parts_count)?;
    let mut output_bytes = 0_u64;
    let mut scalar_parts = 0_u64;
    for part in parts {
        output_bytes = output_bytes
            .checked_add(fuel_usize(runtime_text_length(heap, *part)?)?)
            .ok_or(InterpreterError::FuelCostOverflow)?;
        if !matches!(*part, RuntimeValue::String { .. }) {
            scalar_parts = scalar_parts
                .checked_add(1)
                .ok_or(InterpreterError::FuelCostOverflow)?;
        }
    }
    let part_scan = fuel_blocks(
        u64::from(parts_count),
        STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS,
    )?;
    let output_work = fuel_blocks(output_bytes, STANDARD_STRING_FUEL_BLOCK_BYTES)?
        .checked_mul(2)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let scalar_unit = fuel_blocks(SCALAR_TO_STRING_MAX_BYTES, STANDARD_STRING_FUEL_BLOCK_BYTES)?
        .checked_mul(SCALAR_TO_STRING_FUEL_PASSES)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel_add(
        part_scan,
        fuel_add(
            output_work,
            scalar_parts
                .checked_mul(scalar_unit)
                .ok_or(InterpreterError::FuelCostOverflow)?,
        )?,
    )
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

fn value_visit_fuel(values: u64, passes: u64) -> Result<u64, InterpreterError> {
    let work = values
        .checked_mul(passes)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    fuel_blocks(work, 8)
}

fn collection_arena_metadata_fuel(
    heap: &Heap,
    claim: bool,
    release: bool,
) -> Result<u64, InterpreterError> {
    collection_arena_metadata_shape_fuel(
        fuel_usize(heap.collection_arena_fuel_shape().free_ranges)?,
        claim,
        release,
    )
}

fn collection_arena_metadata_shape_fuel(
    ranges: u64,
    claim: bool,
    release: bool,
) -> Result<u64, InterpreterError> {
    if !claim && !release {
        return Ok(0);
    }

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
    abi: PhysicalStandardIntrinsicAbi,
) -> Result<[RuntimeValue; 3], InterpreterError> {
    if intrinsic.argument_count() > 3
        || args_count != abi.physical_argument_slots(intrinsic.argument_count())?
    {
        return Err(InterpreterError::TypeMismatch);
    }
    let mut arguments = [RuntimeValue::Unit; 3];
    for argument in 0..intrinsic.argument_count() {
        let source = abi.argument_base(args_base, usize::from(argument))?;
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
    resolved: ExecutableNominalOperand,
) -> Result<u64, InterpreterError> {
    let abi = PhysicalStandardIntrinsicAbi::from_resolved(resolved)?;
    let arguments = standard_intrinsic_arguments(intrinsic, args_base, args_count, arena, abi)?;
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
            let (live, capacity, stride) = heap.array_physical_fuel_shape(arguments[0])?;
            if matches!(intrinsic, StandardIntrinsic::ArrayPush { .. }) {
                // Amortized push (WP49): in-place unless the extent is full.
                let moved = if live < capacity { 1 } else { live.max(1) };
                let cells = moved
                    .checked_mul(stride)
                    .ok_or(InterpreterError::FuelCostOverflow)?;
                let element_work =
                    fuel_blocks(fuel_usize(cells)?, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
                fuel_add(
                    element_work,
                    collection_arena_metadata_fuel(heap, live >= capacity, live >= capacity)?,
                )?
            } else {
                fuel_blocks(fuel_usize(stride)?, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?
            }
        }
        StandardIntrinsicFuelModel::ArrayResize => {
            array_resize_intrinsic_fuel(intrinsic, &arguments, heap_required()?)?
        }
        StandardIntrinsicFuelModel::ArrayClear => {
            let heap = heap_required()?;
            let (live, _, stride) = heap.array_physical_fuel_shape(arguments[0])?;
            let cells = live
                .checked_mul(stride)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            fuel_blocks(fuel_usize(cells)?, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?
        }
        StandardIntrinsicFuelModel::MapLookup => {
            map_lookup_fuel(heap_required()?, arguments[0], arguments[1])?
        }
        StandardIntrinsicFuelModel::MapInsertAttempt => {
            map_insert_attempt_fuel(heap_required()?, arguments[0], arguments[1])?
        }
    };
    let logical_slots = intrinsic.argument_count().saturating_add(1);
    let physical_slots = args_count
        .checked_add(abi.result_slots)
        .ok_or(InterpreterError::FuelCostOverflow)?;
    let physical_copy_work = if matches!(
        intrinsic.fuel_model(),
        StandardIntrinsicFuelModel::ArrayCopy
    ) {
        0
    } else {
        value_visit_fuel(u64::from(physical_slots.saturating_sub(logical_slots)), 1)?
    };
    fuel_add(
        fuel_add(u64::from(intrinsic.base_fuel_cost()), work)?,
        physical_copy_work,
    )
}

fn array_resize_intrinsic_fuel(
    intrinsic: StandardIntrinsic,
    arguments: &[RuntimeValue; 3],
    heap: &Heap,
) -> Result<u64, InterpreterError> {
    let (live, capacity, stride) = heap.array_physical_fuel_shape(arguments[0])?;
    match intrinsic {
        StandardIntrinsic::ArrayReserve { .. } => {
            let RuntimeValue::I32(additional) = arguments[1] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let grows = usize::try_from(additional)
                .ok()
                .and_then(|additional| live.checked_add(additional))
                .is_some_and(|needed| needed > capacity);
            if !grows {
                return Ok(0);
            }
            let cells = live
                .checked_mul(stride)
                .ok_or(InterpreterError::FuelCostOverflow)?;
            let move_work =
                fuel_blocks(fuel_usize(cells)?, STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS)?;
            fuel_add(
                move_work,
                collection_arena_metadata_fuel(heap, true, capacity != 0)?,
            )
        }
        StandardIntrinsic::ArrayShrinkToFit { .. } => {
            if live == capacity {
                Ok(0)
            } else {
                // The typed arenas split and release the unused tail in
                // place; no element scan or copy occurs.
                collection_arena_metadata_fuel(heap, false, capacity != 0)
            }
        }
        _ => unreachable!("array resize model belongs to resize intrinsics"),
    }
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
fn run_physical_standard_intrinsic(
    intrinsic: StandardIntrinsic,
    args_base: u16,
    dst: u16,
    arena: &mut FrameArena,
    mut heap: Option<&mut Heap>,
    layouts: &nexa_bytecode::layout::LayoutTable,
    resolved: ExecutableNominalOperand,
) -> Result<PhysicalStandardIntrinsicOutcome, InterpreterError> {
    use StandardIntrinsic as Intrinsic;

    let abi = PhysicalStandardIntrinsicAbi::from_resolved(resolved)?;
    let argument_base = |index| abi.argument_base(args_base, index);
    let argument = |arena: &FrameArena, index| register(arena, argument_base(index)?);

    match intrinsic {
        Intrinsic::OptionIsSome { .. }
        | Intrinsic::OptionIsNone { .. }
        | Intrinsic::ResultIsOk { .. }
        | Intrinsic::ResultIsErr { .. } => {
            let RuntimeValue::I32(tag) = argument(arena, 0)? else {
                return Err(InterpreterError::TypeMismatch);
            };
            let result = match intrinsic {
                Intrinsic::OptionIsSome { .. } | Intrinsic::ResultIsErr { .. } => tag == 1,
                Intrinsic::OptionIsNone { .. } | Intrinsic::ResultIsOk { .. } => tag == 0,
                _ => unreachable!(),
            };
            set_register(arena, dst, RuntimeValue::Bool(result))?;
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::OptionUnwrapOr { .. } | Intrinsic::ResultUnwrapOr { .. } => {
            let RuntimeValue::I32(tag) = argument(arena, 0)? else {
                return Err(InterpreterError::TypeMismatch);
            };
            let success_tag = i32::from(matches!(intrinsic, Intrinsic::OptionUnwrapOr { .. }));
            let source = if tag == success_tag {
                argument_base(0)?
                    .checked_add(PHYSICAL_ENUM_PAYLOAD_OFFSET)
                    .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?
            } else {
                argument_base(1)?
            };
            arena.copy_register_range(source, dst, abi.result_slots)?;
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::ArrayGet { element } => {
            let array = argument(arena, 0)?;
            let RuntimeValue::I32(index) = argument(arena, 1)? else {
                return Err(InterpreterError::TypeMismatch);
            };
            let index = usize::try_from(index).ok();
            arena.clear_register_range(dst, abi.result_slots)?;
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            let value = index
                .map(|index| heap.array_value_range(array, index))
                .transpose();
            match value {
                Ok(Some(value)) => {
                    let payload_slots = abi
                        .result_slots
                        .checked_sub(PHYSICAL_ENUM_PAYLOAD_OFFSET)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let payload = dst
                        .checked_add(PHYSICAL_ENUM_PAYLOAD_OFFSET)
                        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
                    let destination = arena.register_range_mut(payload, payload_slots)?;
                    write_collection_element_physical(value, destination, element, layouts, heap)?;
                    set_register(arena, dst, RuntimeValue::I32(1))?;
                }
                Ok(None) | Err(HeapError::IndexOutOfBounds { .. }) => {
                    set_register(arena, dst, RuntimeValue::I32(0))?;
                }
                Err(error) => return Err(InterpreterError::Heap(error)),
            }
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::ArrayPush { element } => {
            let array = argument(arena, 0)?;
            let value_base = argument_base(1)?;
            let value =
                crate::trusted::read_register_window(arena, value_base, abi.argument_slots[1])?;
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            if heap.array_rows(array)?.is_some() || value.len() == 1 {
                heap.array_push_value_range(array, value)?;
            } else {
                let value = materialize_physical_value_range(value, element, layouts, Some(heap))?;
                heap.array_push(array, value)?;
            }
            set_register(arena, dst, RuntimeValue::Bool(true))?;
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::ArrayPop { element } => {
            let array = argument(arena, 0)?;
            let heap = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?;
            let length = heap.array_len(array)?;
            if length == 0 {
                return Ok(PhysicalStandardIntrinsicOutcome::Trapped(
                    "cannot pop an empty array".into(),
                ));
            }
            {
                let value = heap.array_value_range(array, length - 1)?;
                let destination = arena.register_range_mut(dst, abi.result_slots)?;
                write_collection_element_physical(value, destination, element, layouts, heap)?;
            }
            heap.array_pop_value_discard(array)?;
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::MapGet { .. } | Intrinsic::MapRemove { .. } => {
            if abi.argument_slots[0] != 1 || abi.argument_slots[1] != 1 {
                return Err(InterpreterError::TypeMismatch);
            }
            let value_slots = abi
                .result_slots
                .checked_sub(PHYSICAL_ENUM_PAYLOAD_OFFSET)
                .filter(|slots| *slots != 0)
                .ok_or(InterpreterError::TypeMismatch)?;
            let map = argument(arena, 0)?;
            let key = argument(arena, 1)?;
            arena.clear_register_range(dst, abi.result_slots)?;
            let payload = dst
                .checked_add(PHYSICAL_ENUM_PAYLOAD_OFFSET)
                .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
            let destination = arena.register_range_mut(payload, value_slots)?;
            let found = if matches!(intrinsic, Intrinsic::MapGet { .. }) {
                heap.as_deref()
                    .ok_or(InterpreterError::HeapUnavailable)?
                    .map_get_value_into(map, key, destination)?
            } else {
                heap.as_deref_mut()
                    .ok_or(InterpreterError::HeapUnavailable)?
                    .map_remove_value_into(map, key, destination)?
            };
            set_register(arena, dst, RuntimeValue::I32(i32::from(found)))?;
            Ok(PhysicalStandardIntrinsicOutcome::Returned)
        }
        Intrinsic::MapInsert { .. } => {
            if abi.argument_slots[0] != 1
                || abi.argument_slots[1] != 1
                || abi.argument_slots[2] == 0
                || abi.result_slots != 1
            {
                return Err(InterpreterError::TypeMismatch);
            }
            let value_base = argument_base(2)?;
            let value =
                crate::trusted::read_register_window(arena, value_base, abi.argument_slots[2])?;
            let outcome = heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .map_set_value_range(argument(arena, 0)?, argument(arena, 1)?, value)?;
            if outcome == MapSetOutcome::Complete {
                set_register(arena, dst, RuntimeValue::Bool(true))?;
                Ok(PhysicalStandardIntrinsicOutcome::Returned)
            } else {
                Ok(PhysicalStandardIntrinsicOutcome::Retry)
            }
        }
        _ => {
            if abi.result_slots != 1
                || abi.argument_slots[..usize::from(intrinsic.argument_count())]
                    .iter()
                    .any(|slots| *slots != 1)
            {
                return Err(InterpreterError::TypeMismatch);
            }
            let mut arguments = [RuntimeValue::Unit; 3];
            for (index, value) in arguments
                .iter_mut()
                .enumerate()
                .take(usize::from(intrinsic.argument_count()))
            {
                *value = argument(arena, index)?;
            }
            match run_standard_intrinsic(
                intrinsic,
                &arguments[..usize::from(intrinsic.argument_count())],
                heap,
            )? {
                StandardIntrinsicOutcome::Returned(value) => {
                    set_register(arena, dst, value)?;
                    Ok(PhysicalStandardIntrinsicOutcome::Returned)
                }
                StandardIntrinsicOutcome::Retry => Ok(PhysicalStandardIntrinsicOutcome::Retry),
                StandardIntrinsicOutcome::Trapped(message) => {
                    Ok(PhysicalStandardIntrinsicOutcome::Trapped(message))
                }
            }
        }
    }
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
        Intrinsic::ArrayLen { .. }
        | Intrinsic::ArrayIsEmpty { .. }
        | Intrinsic::ArrayCapacity { .. } => {
            let heap = heap.as_deref().ok_or(InterpreterError::HeapUnavailable)?;
            let length = heap.array_len(arguments[0])?;
            returned(match intrinsic {
                Intrinsic::ArrayLen { .. } => RuntimeValue::I32(
                    i32::try_from(length).map_err(|_| InterpreterError::StringLengthOverflow)?,
                ),
                Intrinsic::ArrayIsEmpty { .. } => RuntimeValue::Bool(length == 0),
                Intrinsic::ArrayCapacity { .. } => RuntimeValue::I32(
                    i32::try_from(heap.array_capacity(arguments[0])?)
                        .map_err(|_| InterpreterError::StringLengthOverflow)?,
                ),
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
                    .as_deref_mut()
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
        Intrinsic::ArrayReserve { .. } => {
            let RuntimeValue::I32(additional) = arguments[1] else {
                return Err(InterpreterError::TypeMismatch);
            };
            let Ok(additional) = usize::try_from(additional) else {
                return Ok(StandardIntrinsicOutcome::Trapped(
                    "array reserve additional capacity must be non-negative".into(),
                ));
            };
            match heap
                .as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .array_reserve(arguments[0], additional)
            {
                Ok(()) => returned(RuntimeValue::Bool(true)),
                Err(HeapError::CollectionTooLarge { .. }) => Ok(StandardIntrinsicOutcome::Trapped(
                    "array reserve exceeds the collection length limit".into(),
                )),
                Err(error) => Err(InterpreterError::Heap(error)),
            }
        }
        Intrinsic::ArrayClear { .. } => {
            heap.as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .array_clear(arguments[0])?;
            returned(RuntimeValue::Bool(true))
        }
        Intrinsic::ArrayShrinkToFit { .. } => {
            heap.as_deref_mut()
                .ok_or(InterpreterError::HeapUnavailable)?
                .array_shrink_to_fit(arguments[0])?;
            returned(RuntimeValue::Bool(true))
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

fn write_scalar_text(value: RuntimeValue, text: &mut ScalarText) -> Result<(), InterpreterError> {
    match value {
        RuntimeValue::I32(value) => write!(text, "{value}"),
        RuntimeValue::I64(value) => write!(text, "{value}"),
        RuntimeValue::F32(bits) => write!(text, "{}", f32::from_bits(bits)),
        RuntimeValue::F64(bits) => write!(text, "{}", f64::from_bits(bits)),
        RuntimeValue::Bool(value) => write!(text, "{value}"),
        RuntimeValue::Rune(value) => {
            let value = char::from_u32(value).ok_or(InterpreterError::TypeMismatch)?;
            write!(text, "{value}")
        }
        _ => return Err(InterpreterError::TypeMismatch),
    }
    .map_err(|_| InterpreterError::StringLengthOverflow)
}

fn runtime_text_length(heap: &Heap, value: RuntimeValue) -> Result<usize, InterpreterError> {
    if let RuntimeValue::String { reference, .. } = value {
        return Ok(heap.string(reference)?.len());
    }
    let mut text = ScalarText::new();
    write_scalar_text(value, &mut text)?;
    Ok(text.as_str().len())
}

fn append_runtime_text(
    output: &mut String,
    heap: &Heap,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    if let RuntimeValue::String { reference, .. } = value {
        output.push_str(heap.string(reference)?);
        return Ok(());
    }
    let mut text = ScalarText::new();
    write_scalar_text(value, &mut text)?;
    output.push_str(text.as_str());
    Ok(())
}

fn build_runtime_string(
    heap: &mut Heap,
    parts: &[RuntimeValue],
) -> Result<RuntimeValue, InterpreterError> {
    let length = parts.iter().try_fold(0_usize, |length, part| {
        length
            .checked_add(runtime_text_length(heap, *part)?)
            .ok_or(InterpreterError::StringLengthOverflow)
    })?;
    let mut reservation = heap.preflight_string_build(length)?;
    let mut output = String::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| InterpreterError::Heap(HeapError::CapacityExhausted))?;
    for part in parts {
        append_runtime_text(&mut output, heap, *part)?;
    }
    debug_assert_eq!(output.len(), length);
    Ok(heap.commit_owned_string(&mut reservation, output)?)
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

// The three hot register/pc funnels below carry every instruction arm's
// register traffic (WP69). They delegate to the trusted kernel
// (`trusted.rs`), which drops only the re-checks the verifier has already
// discharged; the error mapping is bit-identical to the checked arena
// path, and the checked `FrameArena` API remains the boundary for every
// non-interpreter caller.
fn register(arena: &FrameArena, register: u16) -> Result<RuntimeValue, InterpreterError> {
    crate::trusted::read_register(arena, register)
}

fn set_register(
    arena: &mut FrameArena,
    register: u16,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    crate::trusted::write_register(arena, register, value)
}

fn increment_pc(arena: &mut FrameArena) -> Result<(), InterpreterError> {
    crate::trusted::advance_pc(arena)
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

fn physical_ranges_equal(
    arena: &FrameArena,
    lhs: u16,
    rhs: u16,
    slots: u16,
    heap: Option<&Heap>,
) -> Result<bool, InterpreterError> {
    let lhs = crate::trusted::read_register_window(arena, lhs, slots)?;
    let rhs = crate::trusted::read_register_window(arena, rhs, slots)?;
    for (lhs, rhs) in lhs.iter().copied().zip(rhs.iter().copied()) {
        let equal = if matches!(
            (lhs, rhs),
            (
                RuntimeValue::String { .. }
                    | RuntimeValue::Struct { .. }
                    | RuntimeValue::NamedRef { .. },
                _
            )
        ) {
            heap.ok_or(InterpreterError::HeapUnavailable)?
                .runtime_value_equal(lhs, rhs)?
        } else {
            runtime_values_equal(lhs, rhs)
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn runtime_value_type(value: RuntimeValue) -> Option<ValueType> {
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

fn write_materialized_value_range(
    arena: &mut FrameArena,
    destination: u16,
    expected: ValueType,
    value: RuntimeValue,
    layouts: &nexa_bytecode::layout::LayoutTable,
    heap: Option<&Heap>,
) -> Result<(), InterpreterError> {
    let slots = layouts
        .physical_slots(expected)
        .map_err(|_| InterpreterError::TypeMismatch)?;
    let destination = arena.register_range_mut(destination, slots)?;
    flatten_materialized_value(destination, expected, value, layouts, heap)
}

fn flatten_materialized_value(
    destination: &mut [RuntimeValue],
    expected: ValueType,
    value: RuntimeValue,
    layouts: &nexa_bytecode::layout::LayoutTable,
    heap: Option<&Heap>,
) -> Result<(), InterpreterError> {
    if runtime_value_type(value) != Some(expected) {
        return Err(InterpreterError::TypeMismatch);
    }
    let expected_slots = layouts
        .physical_slots(expected)
        .map_err(|_| InterpreterError::TypeMismatch)?;
    if destination.len() != usize::from(expected_slots) {
        return Err(InterpreterError::TypeMismatch);
    }

    let ValueType::Named(type_id) = expected else {
        let [slot] = destination else {
            return Err(InterpreterError::TypeMismatch);
        };
        *slot = value;
        return Ok(());
    };
    let Some(layout) = layouts.named_layout(type_id) else {
        let [slot] = destination else {
            return Err(InterpreterError::TypeMismatch);
        };
        *slot = value;
        return Ok(());
    };
    match layout.equality_strategy {
        nexa_bytecode::layout::EqualityStrategy::StructFieldwise => {
            let heap = heap.ok_or(InterpreterError::HeapUnavailable)?;
            let fields = heap.struct_fields(value)?;
            if fields.len() != layout.field_offsets.len() {
                return Err(InterpreterError::TypeMismatch);
            }
            destination.fill(RuntimeValue::Unit);
            for (index, field) in layout.field_offsets.iter().enumerate() {
                let start = usize::from(field.offset);
                let end = start
                    .checked_add(usize::from(field.slots))
                    .ok_or(InterpreterError::TypeMismatch)?;
                let target = destination
                    .get_mut(start..end)
                    .ok_or(InterpreterError::TypeMismatch)?;
                let field_value = fields.get(index).ok_or(InterpreterError::TypeMismatch)?;
                flatten_materialized_value(
                    target,
                    field.logical_type,
                    field_value,
                    layouts,
                    Some(heap),
                )?;
            }
            Ok(())
        }
        nexa_bytecode::layout::EqualityStrategy::EnumTagPayload => {
            let heap = heap.ok_or(InterpreterError::HeapUnavailable)?;
            let (actual, variant_id, tag, payload) = heap.enum_parts(value)?;
            if actual != type_id {
                return Err(InterpreterError::TypeMismatch);
            }
            let enum_layout = layout
                .enum_layout
                .as_ref()
                .ok_or(InterpreterError::TypeMismatch)?;
            let variant = enum_layout
                .variants
                .iter()
                .find(|variant| variant.tag == tag && variant.stable_id == variant_id)
                .ok_or(InterpreterError::TypeMismatch)?;
            destination.fill(RuntimeValue::Unit);
            destination[usize::from(enum_layout.tag_offset)] =
                RuntimeValue::I32(i32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)?);
            match (variant.payload_type, payload) {
                (Some(payload_type), Some(payload)) => {
                    let start = usize::from(enum_layout.payload_offset);
                    let end = start
                        .checked_add(usize::from(variant.payload_slots))
                        .ok_or(InterpreterError::TypeMismatch)?;
                    let target = destination
                        .get_mut(start..end)
                        .ok_or(InterpreterError::TypeMismatch)?;
                    flatten_materialized_value(target, payload_type, payload, layouts, Some(heap))
                }
                (None, None) => Ok(()),
                _ => Err(InterpreterError::TypeMismatch),
            }
        }
        _ => {
            let [slot] = destination else {
                return Err(InterpreterError::TypeMismatch);
            };
            *slot = value;
            Ok(())
        }
    }
}

fn materialize_physical_value_range(
    slots: &[RuntimeValue],
    expected: ValueType,
    layouts: &nexa_bytecode::layout::LayoutTable,
    heap: Option<&mut Heap>,
) -> Result<RuntimeValue, InterpreterError> {
    let requirements = physical_materialization_requirements(slots, expected, layouts)?;
    if requirements == crate::HostReturnRequirements::ZERO {
        return physical_scalar_value(slots, expected);
    }
    let heap = heap.ok_or(InterpreterError::HeapUnavailable)?;
    let mut transaction = crate::HostReturnTransaction::new_runtime_boundary(heap, requirements)
        .map_err(InterpreterError::Host)?;
    let value = materialize_physical_value(&mut transaction, slots, expected, layouts)?;
    transaction.commit(value).map_err(InterpreterError::Host)
}

fn physical_materialization_requirements(
    slots: &[RuntimeValue],
    expected: ValueType,
    layouts: &nexa_bytecode::layout::LayoutTable,
) -> Result<crate::HostReturnRequirements, InterpreterError> {
    let expected_slots = layouts
        .physical_slots(expected)
        .map_err(|_| InterpreterError::TypeMismatch)?;
    if slots.len() != usize::from(expected_slots) {
        return Err(InterpreterError::TypeMismatch);
    }
    let ValueType::Named(type_id) = expected else {
        physical_scalar_value(slots, expected)?;
        return Ok(crate::HostReturnRequirements::ZERO);
    };
    let Some(layout) = layouts.named_layout(type_id) else {
        physical_scalar_value(slots, expected)?;
        return Ok(crate::HostReturnRequirements::ZERO);
    };
    match layout.equality_strategy {
        nexa_bytecode::layout::EqualityStrategy::StructFieldwise => {
            let mut requirements = crate::HostReturnRequirements::ZERO
                .with_object()
                .map_err(InterpreterError::Host)?
                .with_struct_fields(layout.field_offsets.len())
                .map_err(InterpreterError::Host)?;
            for field in &layout.field_offsets {
                let start = usize::from(field.offset);
                let end = start
                    .checked_add(usize::from(field.slots))
                    .ok_or(InterpreterError::TypeMismatch)?;
                requirements = requirements
                    .checked_add(physical_materialization_requirements(
                        slots
                            .get(start..end)
                            .ok_or(InterpreterError::TypeMismatch)?,
                        field.logical_type,
                        layouts,
                    )?)
                    .map_err(InterpreterError::Host)?;
            }
            Ok(requirements)
        }
        nexa_bytecode::layout::EqualityStrategy::EnumTagPayload => {
            let enum_layout = layout
                .enum_layout
                .as_ref()
                .ok_or(InterpreterError::TypeMismatch)?;
            let tag = physical_enum_tag(slots, enum_layout)?;
            let variant = enum_layout
                .variants
                .iter()
                .find(|variant| variant.tag == tag)
                .ok_or(InterpreterError::TypeMismatch)?;
            let mut requirements = crate::HostReturnRequirements::ZERO
                .with_object()
                .map_err(InterpreterError::Host)?;
            if let Some(payload_type) = variant.payload_type {
                let start = usize::from(enum_layout.payload_offset);
                let end = start
                    .checked_add(usize::from(variant.payload_slots))
                    .ok_or(InterpreterError::TypeMismatch)?;
                requirements = requirements
                    .checked_add(physical_materialization_requirements(
                        slots
                            .get(start..end)
                            .ok_or(InterpreterError::TypeMismatch)?,
                        payload_type,
                        layouts,
                    )?)
                    .map_err(InterpreterError::Host)?;
            }
            Ok(requirements)
        }
        _ => {
            physical_scalar_value(slots, expected)?;
            Ok(crate::HostReturnRequirements::ZERO)
        }
    }
}

fn materialize_physical_value(
    transaction: &mut crate::HostReturnTransaction<'_>,
    slots: &[RuntimeValue],
    expected: ValueType,
    layouts: &nexa_bytecode::layout::LayoutTable,
) -> Result<RuntimeValue, InterpreterError> {
    let ValueType::Named(type_id) = expected else {
        return physical_scalar_value(slots, expected);
    };
    let Some(layout) = layouts.named_layout(type_id) else {
        return physical_scalar_value(slots, expected);
    };
    match layout.equality_strategy {
        nexa_bytecode::layout::EqualityStrategy::StructFieldwise => {
            let mut fields = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
            if layout.field_offsets.len() > fields.len() {
                return Err(InterpreterError::TypeMismatch);
            }
            for (index, field) in layout.field_offsets.iter().enumerate() {
                let start = usize::from(field.offset);
                let end = start
                    .checked_add(usize::from(field.slots))
                    .ok_or(InterpreterError::TypeMismatch)?;
                fields[index] = materialize_physical_value(
                    transaction,
                    slots
                        .get(start..end)
                        .ok_or(InterpreterError::TypeMismatch)?,
                    field.logical_type,
                    layouts,
                )?;
            }
            transaction
                .write_struct(type_id, &fields[..layout.field_offsets.len()])
                .map_err(InterpreterError::Host)
        }
        nexa_bytecode::layout::EqualityStrategy::EnumTagPayload => {
            let enum_layout = layout
                .enum_layout
                .as_ref()
                .ok_or(InterpreterError::TypeMismatch)?;
            let tag = physical_enum_tag(slots, enum_layout)?;
            let variant = enum_layout
                .variants
                .iter()
                .find(|variant| variant.tag == tag)
                .ok_or(InterpreterError::TypeMismatch)?;
            let payload = if let Some(payload_type) = variant.payload_type {
                let start = usize::from(enum_layout.payload_offset);
                let end = start
                    .checked_add(usize::from(variant.payload_slots))
                    .ok_or(InterpreterError::TypeMismatch)?;
                Some(materialize_physical_value(
                    transaction,
                    slots
                        .get(start..end)
                        .ok_or(InterpreterError::TypeMismatch)?,
                    payload_type,
                    layouts,
                )?)
            } else {
                None
            };
            transaction
                .write_enum(type_id, variant.stable_id, tag, payload)
                .map_err(InterpreterError::Host)
        }
        _ => physical_scalar_value(slots, expected),
    }
}

fn physical_scalar_value(
    slots: &[RuntimeValue],
    expected: ValueType,
) -> Result<RuntimeValue, InterpreterError> {
    let [value] = slots else {
        return Err(InterpreterError::TypeMismatch);
    };
    if runtime_value_type(*value) != Some(expected) {
        return Err(InterpreterError::TypeMismatch);
    }
    Ok(*value)
}

fn write_collection_element_physical(
    values: crate::CollectionView<'_>,
    destination: &mut [RuntimeValue],
    expected: ValueType,
    layouts: &nexa_bytecode::layout::LayoutTable,
    heap: &Heap,
) -> Result<(), InterpreterError> {
    if values.len() == destination.len() {
        for (destination, value) in destination.iter_mut().zip(values.iter()) {
            *destination = value;
        }
        return Ok(());
    }
    if values.len() != 1 {
        return Err(InterpreterError::TypeMismatch);
    }
    let value = values.get(0).ok_or(InterpreterError::TypeMismatch)?;
    flatten_materialized_value(destination, expected, value, layouts, Some(heap))
}

fn physical_enum_tag(
    slots: &[RuntimeValue],
    layout: &nexa_bytecode::layout::EnumLayout,
) -> Result<u32, InterpreterError> {
    let RuntimeValue::I32(tag) = *slots
        .get(usize::from(layout.tag_offset))
        .ok_or(InterpreterError::TypeMismatch)?
    else {
        return Err(InterpreterError::TypeMismatch);
    };
    u32::try_from(tag).map_err(|_| InterpreterError::TypeMismatch)
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

pub(crate) fn is_safepoint(instruction: Instruction, pc: u32) -> bool {
    match instruction {
        Instruction::Safepoint
        | Instruction::Yield
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
        | Instruction::ClassGet { .. }
        | Instruction::ClassSet { .. }
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
        const DEFAULT_OPCODE_COSTS: [u16; 111] = [$($base_cost),+];

        /// Stable opcode display names indexed by `opcode_index` (WP15).
        pub(crate) const OPCODE_NAMES: [&str; 111] = [$($name),+];

        #[cfg(test)]
        const OPCODE_COST_SCHEDULE: [OpcodeCostScheduleEntry; 111] = [
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
        #[inline(always)]
        pub(crate) const fn opcode_index(instruction: Instruction) -> usize {
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
        dynamic_work: "binary_search_fuel(enum_variant_index_entries)+ceil((owner_slots+payload_slots)/8)"
    },
    Instruction::EnumTag { .. } => { index: 25, name: "EnumTag", base_cost: 1 },
    Instruction::EnumPayload { .. } => {
        index: 26,
        name: "EnumPayload",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(enum_variant_index_entries)+ceil(payload_slots/8)"
    },
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
        dynamic_work: "ceil(physical_slots/8)"
    },
    Instruction::StructGet { .. } => {
        index: 61,
        name: "StructGet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(struct_field_index_entries)+ceil(field_slots/8)"
    },
    Instruction::StructWith { .. } => {
        index: 62,
        name: "StructWith",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(struct_field_index_entries)+ceil((owner_slots+field_slots)/8)"
    },
    Instruction::StructEqual { .. } => {
        index: 63,
        name: "StructEqual",
        base_cost: 1,
        dynamic_work: "ceil(physical_slots/8)"
    },
    Instruction::ClassNew { .. } => {
        index: 64,
        name: "ClassNew",
        base_cost: 1,
        dynamic_work: "ceil(physical_field_slots*2/8)"
    },
    Instruction::ClassGet { .. } => {
        index: 65,
        name: "ClassGet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(class_field_index_entries)+ceil(field_slots/8)"
    },
    Instruction::ClassSet { .. } => {
        index: 66,
        name: "ClassSet",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(class_field_index_entries)+ceil(field_slots/8)"
    },
    Instruction::ClassEqual { .. } => { index: 67, name: "ClassEqual", base_cost: 1 },
    Instruction::ArrayNew { .. } => {
        index: 68,
        name: "ArrayNew",
        base_cost: 1,
        dynamic_work: "binary_search_fuel(array_type_index_entries)"
    },
    Instruction::ArrayLen { .. } => { index: 69, name: "ArrayLen", base_cost: 1 },
    Instruction::ArrayGet { .. } => {
        index: 70,
        name: "ArrayGet",
        base_cost: 1,
        dynamic_work: "ceil(element_slots/8)"
    },
    Instruction::ArraySet { .. } => {
        index: 71,
        name: "ArraySet",
        base_cost: 1,
        dynamic_work: "ceil(element_slots/8)"
    },
    Instruction::ArrayPush { .. } => {
        index: 72,
        name: "ArrayPush",
        base_cost: 1,
        dynamic_work: "ceil(moved_elements*element_slots/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayPop { .. } => {
        index: 73,
        name: "ArrayPop",
        base_cost: 1,
        dynamic_work: "ceil(max(old_len,1)*element_slots/8)"
    },
    Instruction::ArrayInsert { .. } => {
        index: 74,
        name: "ArrayInsert",
        base_cost: 1,
        dynamic_work: "ceil(max(old_len,1)*element_slots/8)+collection_claim_release_metadata"
    },
    Instruction::ArrayRemove { .. } => {
        index: 75,
        name: "ArrayRemove",
        base_cost: 1,
        dynamic_work: "ceil(max(old_len,1)*element_slots/8)"
    },
    Instruction::ArrayClear { .. } => {
        index: 76,
        name: "ArrayClear",
        base_cost: 1,
        dynamic_work: "ceil(max(old_len,1)*element_slots/8)"
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
        dynamic_work: "map_lookup_slots+key_hash_and_comparison_shape+ceil((option_slots+value_slots)/8)"
    },
    Instruction::MapSet { .. } => {
        index: 80,
        name: "MapSet",
        base_cost: 1,
        dynamic_work: "map_insert_attempt_slots+key_hash_and_comparison_shape+ceil((key_slots+value_slots)/8)"
    },
    Instruction::MapRemove { .. } => {
        index: 81,
        name: "MapRemove",
        base_cost: 1,
        dynamic_work: "map_lookup_slots+key_hash_and_comparison_shape+ceil((option_slots+value_slots)/8)"
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
        dynamic_work: "ceil(physical_slots/8)"
    },
    Instruction::ArrayFieldGet { .. } => {
        index: 107,
        name: "ArrayFieldGet",
        base_cost: 1,
        dynamic_work: "ceil(field_slots/8)"
    },
    Instruction::ArrayPushRow { .. } => {
        index: 108,
        name: "ArrayPushRow",
        base_cost: 1,
        dynamic_work: "ceil(moved_elements*element_slots/8)+collection_claim_release_metadata"
    },
    Instruction::StringBuild { .. } => {
        index: 109,
        name: "StringBuild",
        base_cost: 1,
        dynamic_work: "parts_scan+scalar_format+ceil(output_bytes/32)*2"
    },
    Instruction::CopyValue { .. } => {
        index: 110,
        name: "CopyValue",
        base_cost: 1,
        dynamic_work: "ceil(physical_slots/8)"
    },
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use nexa_bytecode::{
        AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
        ModuleBuilder, RootMap, SCALAR_TO_STRING_FUEL_PASSES, SCALAR_TO_STRING_MAX_BYTES,
        STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS, STANDARD_STRING_FUEL_BLOCK_BYTES, Signature,
        SourceMapEntry, StandardIntrinsic, StructField, StructType, ValueType,
    };
    use nexa_core::{CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS, FileId, SourceSpan, StableId};
    use nexa_verifier::{VerifierLimits, verify};

    use super::{
        CheckedInterpreter, FuelState, InterpreterError, InterpreterHost, InterpreterHostArguments,
        InterpreterHostOutcome, InterpreterMigration, InterpreterOutcome, OPCODE_COST_SCHEDULE,
        StandardIntrinsicOutcome, SuspendReason, allocate_runtime_string,
        ensure_host_call_available, fuel_add, fuel_blocks, run_standard_intrinsic,
    };
    use crate::{
        ContinuationReservation, ExecutableModule, FrameError, FrameLimits, GcRoots, Heap,
        HeapError, HostTrap, MapSetOutcome, Object, OpcodeCostTable, RuntimeValue,
    };

    #[test]
    #[allow(clippy::too_many_lines)]
    fn physical_abi_scatter_preserves_aggregate_and_following_scalar_parameters() {
        let source = r"
struct Pair { first: i32, second: i32, }

fn echo(pair: Pair) -> Pair {
    return pair;
}

fn sum(pair: Pair, bias: i32) -> i32 {
    return pair.first + pair.second + bias;
}

fn work() -> i32 {
    let pair: Pair = echo(Pair { first: 3, second: 5 });
    return sum(pair, 4);
}
";
        let module = nexa_compiler::compile(source).expect("physical ABI corpus compiles");
        let function = module
            .module()
            .functions
            .iter()
            .position(|function| function.signature.parameters.is_empty())
            .and_then(|index| u32::try_from(index).ok())
            .expect("work function");
        let sum_index = module
            .module()
            .functions
            .iter()
            .position(|function| function.signature.parameters.len() == 2)
            .expect("sum function");
        let sum = &module.module().functions[sum_index];
        assert_eq!(sum.parameter_slots, 3);
        assert!(sum.registers >= 3);
        let echo = module
            .module()
            .functions
            .iter()
            .position(|function| {
                function.signature.parameters.len() == 1
                    && function.signature.result == function.signature.parameters.first().copied()
            })
            .expect("echo function");
        assert_eq!(
            module
                .module_abi()
                .function(echo)
                .and_then(|abi| abi.result.as_ref())
                .map(|result| result.slot_count),
            Some(2)
        );
        let call_slots = module.module().functions[function as usize]
            .code
            .iter()
            .filter_map(|instruction| match instruction {
                Instruction::Call { args_count, .. } => Some(*args_count),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(call_slots, [2, 3]);
        assert!(
            module
                .module()
                .functions
                .iter()
                .flat_map(|function| &function.code)
                .any(|instruction| matches!(instruction, Instruction::CopyValue { slots: 2, .. })),
            "aggregate references must copy their complete physical range"
        );

        let mut portable_heap = Heap::new_with_limits(64, 4_096, 64);
        let portable =
            CheckedInterpreter::run_with_heap(&module, function, &[], 1_000, &mut portable_heap)
                .expect("portable execution");
        let executable =
            ExecutableModule::build(&module, OpcodeCostTable::canonical()).expect("dense image");
        let mut dense_heap = Heap::new_with_limits(64, 4_096, 64);
        let dense = CheckedInterpreter::run_with_heap_and_executable(
            &module,
            function,
            &[],
            1_000,
            &mut dense_heap,
            &executable,
        )
        .expect("dense execution");
        assert!(matches!(
            portable,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(12)),
                ..
            }
        ));
        assert!(matches!(
            dense,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(12)),
                ..
            }
        ));
        assert_eq!(portable_heap.live_len(), 0);
        assert_eq!(dense_heap.live_len(), 0);
        assert_eq!(portable_heap.vm_allocation_counters().object_allocations, 0);
        assert_eq!(dense_heap.vm_allocation_counters().object_allocations, 0);
        assert_eq!(
            portable_heap
                .vm_allocation_counters()
                .struct_materializations,
            0
        );
        assert_eq!(
            dense_heap.vm_allocation_counters().struct_materializations,
            0
        );

        let pair_type = module.module().struct_types[0].type_id;
        let mut entry_heap = Heap::new_with_limits(64, 4_096, 64);
        let pair = entry_heap
            .allocate_struct(pair_type, &[RuntimeValue::I32(20), RuntimeValue::I32(22)])
            .unwrap();
        let materializations = entry_heap.vm_allocation_counters().struct_materializations;
        let outcome = CheckedInterpreter::run_with_heap(
            &module,
            u32::try_from(sum_index).unwrap(),
            &[pair, RuntimeValue::I32(1)],
            1_000,
            &mut entry_heap,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(43)),
                ..
            }
        ));
        assert_eq!(
            entry_heap.vm_allocation_counters().struct_materializations,
            materializations,
            "entry flattening borrows the boundary object and creates no temporary Struct"
        );
    }

    struct BorrowedPairHost {
        pair: StableId,
    }

    impl InterpreterHost for BorrowedPairHost {
        fn call(
            &mut self,
            import: u32,
            arguments: InterpreterHostArguments<'_>,
            _heap: Option<&mut Heap>,
        ) -> Result<InterpreterHostOutcome, HostTrap> {
            if import != 0 || arguments.len() != 1 {
                return Err(HostTrap::Arity);
            }
            let (slots, types, layouts) = arguments.parts();
            let values = crate::RuntimeHostArgs::from_physical(slots, types, layouts, None)?;
            let pair = values.struct_ref(0, self.pair)?;
            if pair.len() != 2 {
                return Err(HostTrap::Type);
            }
            let first = pair.field(0)?.i32()?;
            let second = pair.field(1)?.i64()?;
            Ok(InterpreterHostOutcome::Immediate(RuntimeValue::I32(
                first
                    .checked_add(i32::try_from(second).map_err(|_| HostTrap::Type)?)
                    .ok_or(HostTrap::Type)?,
            )))
        }
    }

    #[test]
    fn host_arguments_borrow_physical_struct_slots_without_materialization() {
        let pair = StableId::from_name("test.HostPair");
        let first = StableId::from_name("test.HostPair.first");
        let second = StableId::from_name("test.HostPair.second");
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            5,
        );
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::LoadI64 { dst: 1, value: 9 })
            .emit(Instruction::StructNew {
                type_id: pair,
                fields_base: 0,
                fields_count: 2,
                dst: 2,
            })
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 2,
                args_count: 2,
                dst: 4,
            })
            .emit(Instruction::Return { source: 4 });
        let mut function = function.finish().unwrap();
        function.root_bitmap = vec![false; 5];
        function.safepoints = vec![0, 3, 4];
        function.root_maps = function
            .safepoints
            .iter()
            .copied()
            .map(|pc| RootMap {
                pc,
                bitmap: vec![false; 5],
            })
            .collect();

        let mut module = ModuleBuilder::new();
        module
            .metadata(
                StableId::from_name("test.physical-host"),
                nexa_bytecode::StateSchema::default().fingerprint(),
            )
            .struct_type(StructType {
                type_id: pair,
                fields: vec![
                    StructField {
                        stable_id: first,
                        ty: ValueType::I32,
                    },
                    StructField {
                        stable_id: second,
                        ty: ValueType::I64,
                    },
                ],
            });
        module.host_import(HostImport {
            stable_id: StableId::from_name("test.physical-host.sum"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: vec![ValueType::Named(pair)],
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        });
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let limits = FrameLimits::default();
        let continuation = CheckedInterpreter::start(
            &module,
            0,
            &[],
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .unwrap();
        let mut host = BorrowedPairHost { pair };
        let mut heap = Heap::new_with_limits(8, 256, 8);
        let outcome = CheckedInterpreter::poll_with_host_and_heap(
            &module,
            continuation,
            FuelState::new(100, 0, 100),
            OpcodeCostTable::canonical(),
            &mut host,
            &mut heap,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(16)),
                ..
            }
        ));
        assert_eq!(heap.vm_allocation_counters().struct_materializations, 0);
    }

    #[test]
    fn static_leaf_trap_matches_full_outcome_and_rejects_foreign_code_backing() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        function.emit(Instruction::Trap);
        let mut builder = ModuleBuilder::new();
        builder.metadata(
            StableId::from_name("static-leaf-trap"),
            nexa_bytecode::StateSchema::default().fingerprint(),
        );
        builder.function(function.finish().expect("trap function"));
        let module = verify(builder.finish(), VerifierLimits::default()).expect("verified trap");
        let costs = OpcodeCostTable::canonical();
        let executable = ExecutableModule::build(&module, costs).expect("trap executable");

        let limits = FrameLimits::default();
        let continuation = CheckedInterpreter::start(
            &module,
            0,
            &[],
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .expect("start full trap");
        let mut full_heap = Heap::new_with_limits(8, 64, 8);
        let full = CheckedInterpreter::poll_with_heap_and_executable(
            &module,
            continuation,
            FuelState::new(64, 0, u64::MAX),
            costs,
            &mut full_heap,
            &executable,
        )
        .expect("full trap executes");
        let InterpreterOutcome::Trapped {
            trap: full_trap,
            charge: full_charge,
            fuel: full_fuel,
        } = full
        else {
            panic!("full trap must trap");
        };

        let mut leaf_heap = Heap::new_with_limits(8, 64, 8);
        let leaf = CheckedInterpreter::try_run_static_leaf(
            &module,
            0,
            &[],
            FuelState::new(64, 0, u64::MAX),
            costs,
            &mut leaf_heap,
            &executable,
        )
        .expect("leaf trap executes")
        .expect("trap function is certified");
        assert_eq!(*leaf.result.expect_err("leaf trap must trap"), full_trap);
        assert_eq!(leaf.charge, full_charge);
        assert_eq!(leaf.fuel, full_fuel);
        assert_eq!(leaf_heap.byte_inspection(), full_heap.byte_inspection());

        let foreign =
            verify(module.module().clone(), VerifierLimits::default()).expect("foreign clone");
        assert!(
            CheckedInterpreter::try_run_static_leaf(
                &foreign,
                0,
                &[],
                FuelState::new(64, 0, u64::MAX),
                costs,
                &mut leaf_heap,
                &executable,
            )
            .expect("foreign module falls back")
            .is_none(),
            "a same-shaped but separately verified code backing cannot reuse the certificate"
        );
    }

    fn make_buffer_leaf_heap(buffer_type: StableId) -> (Heap, [RuntimeValue; 2]) {
        let mut heap = Heap::new_with_limits(16, 128, 16);
        let destination = heap
            .allocate_buffer(
                buffer_type,
                ValueType::I32,
                &[
                    RuntimeValue::I32(0),
                    RuntimeValue::I32(0),
                    RuntimeValue::I32(0),
                ],
            )
            .expect("destination buffer");
        let source = heap
            .allocate_buffer(
                buffer_type,
                ValueType::I32,
                &[
                    RuntimeValue::I32(1),
                    RuntimeValue::I32(2),
                    RuntimeValue::I32(3),
                ],
            )
            .expect("source buffer");
        (heap, [destination, source])
    }

    fn assert_constant_leaf_parity(
        module: &nexa_verifier::VerifiedModule,
        executable: &ExecutableModule,
        function: u32,
    ) {
        let costs = OpcodeCostTable::canonical();
        let limits = FrameLimits::default();
        let mut full_heap = Heap::new_with_limits(16, 128, 16);
        let continuation = CheckedInterpreter::start(
            module,
            function,
            &[],
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .expect("start full constant leaf");
        let full = CheckedInterpreter::poll_with_heap_and_executable(
            module,
            continuation,
            FuelState::new(256, 0, u64::MAX),
            costs,
            &mut full_heap,
            executable,
        )
        .expect("full constant leaf");
        let InterpreterOutcome::Returned {
            value: full_value,
            charge: full_charge,
            fuel: full_fuel,
            ..
        } = full
        else {
            panic!("full constant leaf must return");
        };

        let mut leaf_heap = Heap::new_with_limits(16, 128, 16);
        let leaf = CheckedInterpreter::try_run_static_leaf(
            module,
            function,
            &[],
            FuelState::new(256, 0, u64::MAX),
            costs,
            &mut leaf_heap,
            executable,
        )
        .expect("constant kernel executes")
        .expect("constant function is certified");
        assert_eq!(
            leaf.result.expect("constant kernel returns"),
            full_value,
            "function {function} result"
        );
        assert_eq!(leaf.charge, full_charge, "function {function} charge");
        assert_eq!(leaf.fuel, full_fuel, "function {function} fuel");
        assert_eq!(
            leaf_heap.byte_inspection(),
            full_heap.byte_inspection(),
            "function {function} heap"
        );
    }

    #[test]
    fn static_leaf_constant_kernel_and_physical_enum_path_match_full_execution() {
        let source = r#"
class Cell { mut value: i32, next: Option<Cell>, }
struct Row { value: i32, wide: i64, label: string, }
fn string_constant() -> i32 {
    let text: string = "kernel";
    return text.byte_len();
}
fn struct_constant() -> i32 {
    let row: Row = Row { value: 7, wide: 9, label: "row" };
    return row.value;
}
fn class_constant() -> i32 {
    let cell: Cell = new Cell { value: 7, next: Option::None };
    cell.value = cell.value + 1;
    return cell.value;
}
fn arithmetic_constant() -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    values.push(2);
    values.set(0, 3);
    return values.get(0) + values.len();
}
"#;
        let module = nexa_compiler::compile(source).expect("constant kernels compile");
        let executable = ExecutableModule::build(&module, OpcodeCostTable::canonical())
            .expect("constant kernels build");
        assert!(
            executable.functions()[0]
                .static_leaf_constant_kernel()
                .is_some()
        );
        assert!(
            executable.functions()[1]
                .static_leaf_constant_kernel()
                .is_some()
        );
        assert!(
            executable.functions()[2]
                .static_leaf_constant_kernel()
                .is_some(),
            "scalar-replaced Enum/Class work must be load-time partially evaluated"
        );
        assert!(
            executable.functions()[3]
                .static_leaf_constant_kernel()
                .is_some()
        );
        for function in 0..4 {
            assert_constant_leaf_parity(
                &module,
                &executable,
                u32::try_from(function).expect("small fixture index"),
            );
        }
    }

    #[test]
    fn static_leaf_prepares_buffer_headers_once_and_matches_full_execution() {
        let source = r"
fn copy(destination: Buffer<i32>, source: Buffer<i32>) -> i32 {
    destination.copy(source, 0, 0, 3);
    return destination.get(2);
}
";
        let module = nexa_compiler::compile(source).expect("buffer leaf compiles");
        let costs = OpcodeCostTable::canonical();
        let executable = ExecutableModule::build(&module, costs).expect("buffer leaf executable");
        assert!(
            executable.functions()[0].static_leaf_fuel().is_some(),
            "the fixture must exercise the certified leaf path"
        );
        assert!(
            executable.functions()[0]
                .static_leaf_certificate()
                .is_some_and(|certificate| certificate.buffer_kernel_instructions == Some(7)),
            "the exact copy-then-get shape must receive the fused-kernel certificate"
        );
        let buffer_type = module.module().buffer_types[0].type_id;

        let limits = FrameLimits::default();
        let (mut full_heap, full_arguments) = make_buffer_leaf_heap(buffer_type);
        let continuation = CheckedInterpreter::start(
            &module,
            0,
            &full_arguments,
            limits,
            ContinuationReservation::for_limits(limits),
        )
        .expect("start full buffer path");
        let full = CheckedInterpreter::poll_with_heap_and_executable(
            &module,
            continuation,
            FuelState::new(256, 0, u64::MAX),
            costs,
            &mut full_heap,
            &executable,
        )
        .expect("full buffer path");
        let InterpreterOutcome::Returned {
            value: full_value,
            charge: full_charge,
            fuel: full_fuel,
            ..
        } = full
        else {
            panic!("full buffer path must return");
        };

        let (mut leaf_heap, leaf_arguments) = make_buffer_leaf_heap(buffer_type);
        let leaf = CheckedInterpreter::try_run_static_leaf(
            &module,
            0,
            &leaf_arguments,
            FuelState::new(256, 0, u64::MAX),
            costs,
            &mut leaf_heap,
            &executable,
        )
        .expect("prepared buffer path executes")
        .expect("buffer function remains certified");
        assert_eq!(leaf.result.expect("buffer leaf returns"), full_value);
        assert_eq!(leaf.charge, full_charge);
        assert_eq!(leaf.fuel, full_fuel);
        assert_eq!(leaf_heap.byte_inspection(), full_heap.byte_inspection());

        let required_fuel = leaf.charge.fuel_used;
        let (mut limited_heap, limited_arguments) = make_buffer_leaf_heap(buffer_type);
        let before = limited_heap
            .buffer_values(limited_arguments[0])
            .expect("destination view")
            .iter()
            .collect::<Vec<_>>();
        assert!(
            CheckedInterpreter::try_run_static_leaf(
                &module,
                0,
                &limited_arguments,
                FuelState::new(required_fuel - 1, 0, u64::MAX),
                costs,
                &mut limited_heap,
                &executable,
            )
            .expect("insufficient fuel falls back")
            .is_none(),
            "fused execution must not start without its exact fuel budget"
        );
        let after = limited_heap
            .buffer_values(limited_arguments[0])
            .expect("destination view")
            .iter()
            .collect::<Vec<_>>();
        assert_eq!(after, before, "fuel fallback occurs before buffer mutation");
    }

    /// F2: the predecoded-row path and the recompute path must charge
    /// bit-identical fuel and suspend at identical points across a
    /// slice-by-slice replay of a mixed program.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn predecoded_rows_charge_identical_fuel_and_suspend_points() {
        let source = r#"
struct Pair { first: i32, second: i32, }
enum Signal { Quiet, Loud(i32), }

fn work(x: i32) -> i32 {
    let text: string = "row-parity";
    let cell: Pair = Pair { first: x, second: text.byte_len() };
    let values: Array<i32> = Array::new();
    let mut index: i32 = 0;
    while index < 24 {
        values.push(cell.first + index);
        index = index + 1;
    }
    let table: Map<i32, i32> = Map::new();
    table.set(1, cell.second);
    let signal: Signal = Signal::Loud(x);
    return match signal {
        Signal::Quiet => 0,
        Signal::Loud(value) => value + values.len() + table.len(),
    };
}
"#;
        let module = nexa_compiler::compile(source).expect("row parity corpus compiles");
        let costs = OpcodeCostTable::default();
        let executable =
            crate::executable::ExecutableModule::build(&module, &costs).expect("build rows");
        let run = |rows: Option<&crate::executable::ExecutableModule>| {
            let limits = FrameLimits::default();
            let mut heap = Heap::new_with_limits(256, 16_384, 256);
            let mut continuation = CheckedInterpreter::start(
                &module,
                0,
                &[RuntimeValue::I32(9)],
                limits,
                ContinuationReservation::for_limits(limits),
            )
            .expect("start row parity continuation");
            let mut cumulative = 0;
            let mut trace = Vec::new();
            loop {
                let outcome = match rows {
                    Some(rows) => CheckedInterpreter::execute::<true, true>(
                        &module,
                        continuation,
                        FuelState::new(64, cumulative, u64::MAX),
                        &costs,
                        None,
                        None,
                        None,
                        Some(&mut heap),
                        Some(rows),
                        None,
                    ),
                    None => CheckedInterpreter::execute::<false, true>(
                        &module,
                        continuation,
                        FuelState::new(64, cumulative, u64::MAX),
                        &costs,
                        None,
                        None,
                        None,
                        Some(&mut heap),
                        None,
                        None,
                    ),
                }
                .expect("row parity slice");
                match outcome {
                    InterpreterOutcome::Suspended {
                        continuation: next,
                        reason,
                        charge,
                        fuel,
                    } => {
                        assert_eq!(reason, SuspendReason::Fuel);
                        trace.push((charge.fuel_used, charge.instructions));
                        cumulative = fuel.cumulative_used;
                        continuation = next;
                    }
                    InterpreterOutcome::Returned {
                        value,
                        charge,
                        fuel,
                    } => {
                        trace.push((charge.fuel_used, charge.instructions));
                        return (value, fuel.cumulative_used, trace);
                    }
                    other => panic!("row parity run ended unexpectedly: {other:?}"),
                }
            }
        };
        let (reference_value, reference_fuel, reference_trace) = run(None);
        let (row_value, row_fuel, row_trace) = run(Some(&executable));
        assert_eq!(row_value, reference_value, "identical results");
        assert_eq!(row_fuel, reference_fuel, "identical cumulative fuel");
        assert_eq!(
            row_trace, reference_trace,
            "identical per-slice charges and suspend points"
        );
        let (static_rows, total) = executable.static_fuel_coverage();
        assert!(
            static_rows * 2 > total,
            "parity corpus keeps majority static coverage ({static_rows}/{total})"
        );
    }

    #[test]
    fn bytecode_v7_opcode_cost_schedule_matches_the_frozen_fixture() {
        assert_eq!(nexa_bytecode::BYTECODE_VERSION, 7);
        assert_eq!(OPCODE_COST_SCHEDULE.len(), 111);
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
            include_str!("../fixtures/opcode-cost-table-v7.txt")
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
        let mut function = FunctionBuilder::new(signature, 2);
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
        function.root_bitmap = vec![false, true];
        function.safepoints = vec![0, 1];
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
            .write_resume_async_result(
                &module,
                0,
                super::AsyncResumeValue {
                    expected: Some(ValueType::Named(result_type)),
                    result: async_result,
                    success: true,
                    payload: RuntimeValue::Ref(reference),
                },
                Some(&heap),
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
    fn enum_new_tag_and_payload_stay_in_physical_registers() {
        let enum_type = nexa_bytecode::option_type(ValueType::I32);
        let some = StableId::from_parts(&["Option", "::Some"]);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            4,
        );
        function
            .emit(Instruction::EnumNew {
                type_id: enum_type.type_id,
                variant: some,
                payload: Some(0),
                dst: 1,
            })
            .emit(Instruction::EnumTag { source: 1, dst: 3 })
            .emit(Instruction::EnumPayload {
                source: 1,
                variant: some,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let function = function.finish().unwrap();
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
        let counters = heap.vm_allocation_counters();
        assert_eq!(counters.object_allocations, 0);
        assert_eq!(counters.enum_materializations, 0);
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
        let mut unwrap = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            6,
        );
        unwrap
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::EnumNew {
                type_id: option.type_id,
                variant: StableId::from_parts(&["Option", "::Some"]),
                payload: Some(0),
                dst: 2,
            })
            .emit(Instruction::LoadI32 { dst: 4, value: 9 })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::OptionUnwrapOr {
                    value: ValueType::I32,
                },
                args_base: 2,
                args_count: 3,
                dst: 5,
            })
            .emit(Instruction::Return { source: 5 });

        let result = nexa_bytecode::result_type(ValueType::I32, ValueType::String);
        let mut is_err = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Bool),
            },
            4,
        );
        is_err
            .set_root(0)
            .unwrap()
            .set_root(2)
            .unwrap()
            .emit(Instruction::LoadString { dst: 0, string: 0 })
            .emit(Instruction::EnumNew {
                type_id: result.type_id,
                variant: StableId::from_parts(&["Result", "::Err"]),
                payload: Some(0),
                dst: 1,
            })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::ResultIsErr {
                    success: ValueType::I32,
                    error: ValueType::String,
                },
                args_base: 1,
                args_count: 2,
                dst: 3,
            })
            .emit(Instruction::Return { source: 3 });
        let mut is_err = is_err.finish().unwrap();
        is_err.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false, false, false],
            },
            RootMap {
                pc: 2,
                bitmap: vec![false, false, true, false],
            },
            RootMap {
                pc: 3,
                bitmap: vec![false, false, false, false],
            },
        ];

        let mut module = ModuleBuilder::new();
        module.string("nope");
        module.enum_type(option).enum_type(result);
        module.function(unwrap.finish().unwrap());
        module.function(is_err);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();

        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[], 32, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(7)),
                ..
            }
        ));
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 1, &[], 32, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::Bool(true)),
                ..
            }
        ));
        assert_eq!(heap.vm_allocation_counters().enum_materializations, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn standard_map_intrinsics_preserve_physical_struct_values() {
        let pair = StableId::from_name("test.Pair");
        let first = StableId::from_name("test.Pair.first");
        let second = StableId::from_name("test.Pair.second");
        let pair_type = ValueType::Named(pair);
        let map_type = nexa_bytecode::map_type(ValueType::I32, pair_type);
        let option = nexa_bytecode::option_type(pair_type);
        let some = StableId::from_parts(&["Option", "::Some"]);

        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            19,
        );
        for root in [0, 6, 11] {
            function.set_root(root).unwrap();
        }
        function
            .emit(Instruction::MapNew {
                type_id: map_type,
                dst: 0,
            })
            .emit(Instruction::LoadI32 { dst: 1, value: 7 })
            .emit(Instruction::LoadI32 { dst: 2, value: 11 })
            .emit(Instruction::LoadI32 { dst: 3, value: 29 })
            .emit(Instruction::StructNew {
                type_id: pair,
                fields_base: 2,
                fields_count: 2,
                dst: 4,
            })
            .emit(Instruction::Move { dst: 6, source: 0 })
            .emit(Instruction::Move { dst: 7, source: 1 })
            .emit(Instruction::CopyValue {
                dst: 8,
                source: 4,
                slots: 2,
            })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::MapInsert {
                    key: ValueType::I32,
                    value: pair_type,
                },
                args_base: 6,
                args_count: 4,
                dst: 10,
            })
            .emit(Instruction::Move { dst: 11, source: 0 })
            .emit(Instruction::Move { dst: 12, source: 1 })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::MapGet {
                    key: ValueType::I32,
                    value: pair_type,
                },
                args_base: 11,
                args_count: 2,
                dst: 13,
            })
            .emit(Instruction::EnumPayload {
                source: 13,
                variant: some,
                dst: 16,
            })
            .emit(Instruction::StructGet {
                source: 16,
                field: second,
                dst: 18,
            })
            .emit(Instruction::Return { source: 18 });
        let mut function = function.finish().unwrap();
        let roots = |registers: &[usize]| {
            let mut bitmap = vec![false; 19];
            for register in registers {
                bitmap[*register] = true;
            }
            bitmap
        };
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: roots(&[]),
            },
            RootMap {
                pc: 8,
                bitmap: roots(&[0, 6]),
            },
            RootMap {
                pc: 11,
                bitmap: roots(&[11]),
            },
            RootMap {
                pc: 14,
                bitmap: roots(&[]),
            },
        ];

        let mut module = ModuleBuilder::new();
        module
            .struct_type(StructType {
                type_id: pair,
                fields: vec![
                    StructField {
                        stable_id: first,
                        ty: ValueType::I32,
                    },
                    StructField {
                        stable_id: second,
                        ty: ValueType::I32,
                    },
                ],
            })
            .map_type(nexa_bytecode::MapType::new(ValueType::I32, pair_type))
            .enum_type(option);
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new_with_limits(16, usize::MAX, 16);
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[], 256, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(29)),
                ..
            }
        ));
        let counters = heap.vm_allocation_counters();
        assert_eq!(counters.struct_materializations, 0);
        assert_eq!(counters.enum_materializations, 0);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn standard_array_intrinsics_preserve_physical_struct_values() {
        let pair = StableId::from_name("test.ArrayPair");
        let first = StableId::from_name("test.ArrayPair.first");
        let second = StableId::from_name("test.ArrayPair.second");
        let pair_type = ValueType::Named(pair);
        let array_type = nexa_bytecode::array_type(pair_type);
        let option = nexa_bytecode::option_type(pair_type);
        let some = StableId::from_parts(&["Option", "::Some"]);

        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            17,
        );
        for root in [0, 5, 9] {
            function.set_root(root).unwrap();
        }
        function
            .emit(Instruction::ArrayNew {
                type_id: array_type,
                dst: 0,
            })
            .emit(Instruction::LoadI32 { dst: 1, value: 13 })
            .emit(Instruction::LoadI32 { dst: 2, value: 31 })
            .emit(Instruction::StructNew {
                type_id: pair,
                fields_base: 1,
                fields_count: 2,
                dst: 3,
            })
            .emit(Instruction::Move { dst: 5, source: 0 })
            .emit(Instruction::CopyValue {
                dst: 6,
                source: 3,
                slots: 2,
            })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::ArrayPush { element: pair_type },
                args_base: 5,
                args_count: 3,
                dst: 8,
            })
            .emit(Instruction::Move { dst: 9, source: 0 })
            .emit(Instruction::LoadI32 { dst: 10, value: 0 })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::ArrayGet { element: pair_type },
                args_base: 9,
                args_count: 2,
                dst: 11,
            })
            .emit(Instruction::EnumPayload {
                source: 11,
                variant: some,
                dst: 14,
            })
            .emit(Instruction::StructGet {
                source: 14,
                field: second,
                dst: 16,
            })
            .emit(Instruction::Return { source: 16 });
        let mut function = function.finish().unwrap();
        let roots = |registers: &[usize]| {
            let mut bitmap = vec![false; 17];
            for register in registers {
                bitmap[*register] = true;
            }
            bitmap
        };
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: roots(&[]),
            },
            RootMap {
                pc: 6,
                bitmap: roots(&[0, 5]),
            },
            RootMap {
                pc: 9,
                bitmap: roots(&[9]),
            },
            RootMap {
                pc: 12,
                bitmap: roots(&[]),
            },
        ];

        let mut module = ModuleBuilder::new();
        module
            .struct_type(StructType {
                type_id: pair,
                fields: vec![
                    StructField {
                        stable_id: first,
                        ty: ValueType::I32,
                    },
                    StructField {
                        stable_id: second,
                        ty: ValueType::I32,
                    },
                ],
            })
            .array_type(nexa_bytecode::ArrayType::new(pair_type))
            .enum_type(option);
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();
        let mut heap = Heap::new_with_limits(16, usize::MAX, 16);
        assert!(matches!(
            CheckedInterpreter::run_with_heap(&module, 0, &[], 256, &mut heap).unwrap(),
            InterpreterOutcome::Returned {
                value: Some(RuntimeValue::I32(31)),
                ..
            }
        ));
        let counters = heap.vm_allocation_counters();
        assert_eq!(counters.struct_materializations, 0);
        assert_eq!(counters.enum_materializations, 0);
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
                heap.string(reference).unwrap()
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
    fn string_build_precharges_fuel_and_publishes_once() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::String,
                    ValueType::I32,
                    ValueType::Bool,
                    ValueType::Rune,
                ],
                result: Some(ValueType::String),
            },
            5,
        );
        function.set_root(0).unwrap().set_root(4).unwrap();
        function
            .emit(Instruction::StringBuild {
                dst: 4,
                parts_base: 0,
                parts_count: 4,
            })
            .emit(Instruction::Return { source: 4 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false, false, false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, false, false, false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function);
        let module = verify(module.finish(), VerifierLimits::default()).unwrap();

        let mut heap = Heap::new_with_arena_limits(16, 4096, 64, 128, 16);
        let text = allocate_runtime_string(&mut heap, "Nexa").unwrap();
        let before = heap.vm_allocation_counters().string_allocations;
        let InterpreterOutcome::Suspended { continuation, .. } = CheckedInterpreter::run_with_heap(
            &module,
            0,
            &[
                text,
                RuntimeValue::I32(-7),
                RuntimeValue::Bool(true),
                RuntimeValue::Rune(u32::from('界')),
            ],
            1,
            &mut heap,
        )
        .unwrap() else {
            panic!("underfunded string build must suspend before allocation");
        };
        assert_eq!(heap.vm_allocation_counters().string_allocations, before);
        let roots = GcRoots {
            suspended_tasks: continuation.checked_gc_roots(&module).unwrap(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 1);

        let InterpreterOutcome::Returned {
            value: Some(RuntimeValue::String { reference, .. }),
            ..
        } = CheckedInterpreter::poll_with_heap(
            &module,
            continuation,
            FuelState::new(64, 0, u64::MAX),
            &OpcodeCostTable::default(),
            &mut heap,
        )
        .unwrap()
        else {
            panic!("funded string build must return");
        };
        assert_eq!(heap.string(reference), Ok("Nexa-7true界"));
        assert_eq!(heap.vm_allocation_counters().string_allocations - before, 1);
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
