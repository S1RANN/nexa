use std::fmt;
use std::mem::size_of;

use crate::GcRef;

#[derive(Clone, Copy, PartialEq, Eq)]
struct MigrationObjectHandle {
    context: u64,
    stable_id: nexa_core::StableId,
    type_id: nexa_core::StableId,
    generation: u32,
}

/// Unforgeable handle to an object in the immutable pre-migration state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MigrationOldObjectHandle(MigrationObjectHandle);

impl MigrationOldObjectHandle {
    pub(crate) const fn new(
        context: u64,
        stable_id: nexa_core::StableId,
        type_id: nexa_core::StableId,
        generation: u32,
    ) -> Self {
        Self(MigrationObjectHandle {
            context,
            stable_id,
            type_id,
            generation,
        })
    }

    pub(crate) const fn parts(self) -> (u64, nexa_core::StableId, nexa_core::StableId, u32) {
        (
            self.0.context,
            self.0.stable_id,
            self.0.type_id,
            self.0.generation,
        )
    }
}

impl fmt::Debug for MigrationOldObjectHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationOldObjectHandle")
            .field("stable_id", &self.0.stable_id)
            .field("type_id", &self.0.type_id)
            .field("generation", &self.0.generation)
            .finish_non_exhaustive()
    }
}

/// Unforgeable handle to an object in the current migration's staging arena.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MigrationStagingObjectHandle(MigrationObjectHandle);

impl MigrationStagingObjectHandle {
    pub(crate) const fn new(
        context: u64,
        stable_id: nexa_core::StableId,
        type_id: nexa_core::StableId,
        generation: u32,
    ) -> Self {
        Self(MigrationObjectHandle {
            context,
            stable_id,
            type_id,
            generation,
        })
    }

    pub(crate) const fn parts(self) -> (u64, nexa_core::StableId, nexa_core::StableId, u32) {
        (
            self.0.context,
            self.0.stable_id,
            self.0.type_id,
            self.0.generation,
        )
    }
}

impl fmt::Debug for MigrationStagingObjectHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MigrationStagingObjectHandle")
            .field("stable_id", &self.0.stable_id)
            .field("type_id", &self.0.type_id)
            .field("generation", &self.0.generation)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeValue {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    Bool(bool),
    Rune(u32),
    String {
        reference: GcRef,
        hash: u64,
    },
    Struct {
        reference: GcRef,
        type_id: nexa_core::StableId,
        hash: u64,
    },
    Ref(GcRef),
    NamedRef {
        reference: GcRef,
        type_id: nexa_core::StableId,
    },
    HostRequest(crate::HostRequestHandle),
    ResourceToken(crate::ResourceTokenHandle),
    Snapshot(crate::SnapshotHandle),
    Opaque {
        value: u64,
        type_id: nexa_core::StableId,
    },
    MigrationOldObject(MigrationOldObjectHandle),
    MigrationStagingObject(MigrationStagingObjectHandle),
    StateHandle {
        handle_type: nexa_core::StableId,
        domain: u64,
        stable_id: nexa_core::StableId,
        generation: u32,
    },
    #[default]
    Unit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLimits {
    pub max_frame_bytes: usize,
    pub max_call_depth: u32,
    pub max_defer_records: u32,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_call_depth: 128,
            max_defer_records: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub function: u32,
    pub pc: u32,
    /// Program counter of the `Call` instruction in the caller that created
    /// this frame. Entry and detached cleanup frames have no call site.
    pub call_site_pc: Option<u32>,
    pub register_start: u32,
    pub register_count: u16,
    /// Caller-owned contiguous result range. Cleanup and entry frames have
    /// no result range.
    pub return_range: Option<ReturnRange>,
    pub defer_start: u32,
}

/// Physical result slots reserved in the caller before entering a callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReturnRange {
    pub start: u16,
    pub slots: u16,
}

impl ReturnRange {
    #[must_use]
    pub const fn scalar(start: u16) -> Self {
        Self { start, slots: 1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationReservation {
    pub frame_capacity: u32,
    pub register_capacity: u32,
    pub defer_capacity: u32,
}

impl ContinuationReservation {
    #[must_use]
    pub fn for_limits(limits: FrameLimits) -> Self {
        Self {
            frame_capacity: limits.max_call_depth,
            register_capacity: u32::try_from(
                limits.max_frame_bytes / size_of::<RuntimeValue>().max(1),
            )
            .unwrap_or(u32::MAX),
            defer_capacity: limits.max_defer_records,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum DeferAction {
    ReleaseCounter(u32),
    SetFlag(u32),
    Trap,
    Call {
        function: u32,
        args: [RuntimeValue; 8],
        args_count: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    CallDepthLimit,
    FrameByteLimit,
    DeferLimit,
    NoFrame,
    RegisterOutOfRange,
    RootMapMismatch,
    ReservationExceedsLimit,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for FrameError {}

/// Safe, index-based continuation storage. Capacity is reserved before execution starts.
#[derive(Clone, Debug)]
pub struct FrameArena {
    frames: Vec<Frame>,
    /// Logical end of the live register stack. The backing vector keeps its
    /// initialized high-water mark across pooled continuations so a fresh
    /// call does not rewrite every wide `RuntimeValue` slot to `Unit`.
    register_top: usize,
    registers: Vec<RuntimeValue>,
    defer_records: Vec<DeferAction>,
    limits: FrameLimits,
}

impl FrameArena {
    #[must_use]
    pub fn new(limits: FrameLimits) -> Self {
        Self::with_reserved_capacity(limits, ContinuationReservation::for_limits(limits))
            .expect("default reservation is derived from limits")
    }

    pub fn with_reserved_capacity(
        limits: FrameLimits,
        reservation: ContinuationReservation,
    ) -> Result<Self, FrameError> {
        let max_registers = limits.max_frame_bytes / size_of::<RuntimeValue>().max(1);
        if reservation.frame_capacity > limits.max_call_depth
            || reservation.register_capacity as usize > max_registers
            || reservation.defer_capacity > limits.max_defer_records
        {
            return Err(FrameError::ReservationExceedsLimit);
        }
        crate::allocation::record(crate::allocation::AllocationPhase::Admission, 3);
        Ok(Self {
            frames: Vec::with_capacity(reservation.frame_capacity as usize),
            register_top: 0,
            registers: Vec::with_capacity(reservation.register_capacity as usize),
            defer_records: Vec::with_capacity(reservation.defer_capacity as usize),
            limits,
        })
    }

    /// H1: an allocation-free arena shell for storage swaps; unusable for
    /// execution until [`Self::reset_for_verified`] succeeds on it.
    #[must_use]
    pub(crate) fn empty_shell() -> Self {
        Self {
            frames: Vec::new(),
            register_top: 0,
            registers: Vec::new(),
            defer_records: Vec::new(),
            limits: FrameLimits::default(),
        }
    }

    /// H1: reuses this arena's storage for a fresh continuation. Succeeds
    /// only when the retained capacities already satisfy the reservation
    /// (and the reservation satisfies the limits), so a reused arena is
    /// indistinguishable from a freshly reserved one - including the
    /// admission byte ceiling checks - and performs zero allocations.
    /// Exact root maps and definite initialization make stale backing bits
    /// unreachable, so pooled scalar-heavy calls do not rescan wide register
    /// slots merely to overwrite values no instruction can observe.
    pub(crate) fn reset_for_verified(
        &mut self,
        limits: FrameLimits,
        reservation: ContinuationReservation,
    ) -> Result<(), FrameError> {
        let max_registers = limits.max_frame_bytes / size_of::<RuntimeValue>().max(1);
        if reservation.frame_capacity > limits.max_call_depth
            || reservation.register_capacity as usize > max_registers
            || reservation.defer_capacity > limits.max_defer_records
        {
            return Err(FrameError::ReservationExceedsLimit);
        }
        if self.frames.capacity() < reservation.frame_capacity as usize
            || self.registers.capacity() < reservation.register_capacity as usize
            || self.defer_records.capacity() < reservation.defer_capacity as usize
        {
            return Err(FrameError::ReservationExceedsLimit);
        }
        self.frames.clear();
        // Verified definite-initialization guarantees that no instruction
        // observes these retained bits before writing the corresponding
        // slot; exact root maps never trace an uninitialized slot.
        self.register_top = 0;
        self.defer_records.clear();
        self.limits = limits;
        crate::allocation::record(crate::allocation::AllocationPhase::Admission, 0);
        Ok(())
    }

    pub fn push(&mut self, program_id: u32, register_count: usize) -> Result<(), FrameError> {
        self.push_call(
            program_id,
            u16::try_from(register_count).map_err(|_| FrameError::FrameByteLimit)?,
            None,
        )
    }

    pub fn push_call(
        &mut self,
        function: u32,
        register_count: u16,
        return_target: Option<u16>,
    ) -> Result<(), FrameError> {
        self.push_call_range_at(
            function,
            register_count,
            return_target.map(ReturnRange::scalar),
            None,
        )
    }

    pub fn push_call_at(
        &mut self,
        function: u32,
        register_count: u16,
        return_target: Option<u16>,
        call_site_pc: Option<u32>,
    ) -> Result<(), FrameError> {
        self.push_call_range_at(
            function,
            register_count,
            return_target.map(ReturnRange::scalar),
            call_site_pc,
        )
    }

    pub(crate) fn push_call_range_at(
        &mut self,
        function: u32,
        register_count: u16,
        return_range: Option<ReturnRange>,
        call_site_pc: Option<u32>,
    ) -> Result<(), FrameError> {
        if self.frames.len() >= self.limits.max_call_depth as usize {
            return Err(FrameError::CallDepthLimit);
        }
        if return_range.is_some_and(|range| {
            range.slots == 0
                || self.frames.last().is_none_or(|caller| {
                    range
                        .start
                        .checked_add(range.slots)
                        .is_none_or(|end| end > caller.register_count)
                })
        }) {
            return Err(FrameError::RegisterOutOfRange);
        }
        let next_registers = self
            .register_top
            .checked_add(usize::from(register_count))
            .ok_or(FrameError::FrameByteLimit)?;
        let next_bytes = next_registers
            .checked_mul(size_of::<RuntimeValue>())
            .ok_or(FrameError::FrameByteLimit)?;
        if next_bytes > self.limits.max_frame_bytes {
            return Err(FrameError::FrameByteLimit);
        }
        if self.frames.len() == self.frames.capacity() || next_registers > self.registers.capacity()
        {
            return Err(FrameError::FrameByteLimit);
        }
        self.frames.push(Frame {
            function,
            pc: 0,
            call_site_pc,
            register_start: u32::try_from(self.register_top)
                .map_err(|_| FrameError::FrameByteLimit)?,
            register_count,
            return_range,
            defer_start: u32::try_from(self.defer_records.len())
                .map_err(|_| FrameError::DeferLimit)?,
        });
        if next_registers > self.registers.len() {
            self.registers.resize(next_registers, RuntimeValue::Unit);
        }
        self.register_top = next_registers;
        Ok(())
    }

    /// Pushes a verifier-planned call and copies its contiguous argument
    /// window directly into the callee frame. All checks that can fail are
    /// completed before [`Self::push_call_at`] mutates the arena; the copy is
    /// then an in-bounds move between disjoint live frame ranges.
    pub(crate) fn push_verified_call(
        &mut self,
        function: u32,
        register_count: u16,
        return_target: u16,
        call_site_pc: u32,
        args_base: u16,
        args_count: u16,
    ) -> Result<(), FrameError> {
        self.push_verified_call_range(
            function,
            register_count,
            ReturnRange::scalar(return_target),
            call_site_pc,
            args_base,
            args_count,
        )
    }

    /// Physical sibling of [`Self::push_verified_call`]. Argument slots and
    /// the caller-allocated result range are validated before the arena is
    /// mutated, then copied directly between disjoint frame ranges.
    pub(crate) fn push_verified_call_range(
        &mut self,
        function: u32,
        register_count: u16,
        return_range: ReturnRange,
        call_site_pc: u32,
        args_base: u16,
        args_count: u16,
    ) -> Result<(), FrameError> {
        let caller_index = self
            .frames
            .len()
            .checked_sub(1)
            .ok_or(FrameError::NoFrame)?;
        let caller = self.frames[caller_index];
        let caller_next_pc = call_site_pc
            .checked_add(1)
            .ok_or(FrameError::RegisterOutOfRange)?;
        if args_count > register_count
            || args_base
                .checked_add(args_count)
                .is_none_or(|end| end > caller.register_count)
            || return_range.slots == 0
            || return_range
                .start
                .checked_add(return_range.slots)
                .is_none_or(|end| end > caller.register_count)
        {
            return Err(FrameError::RegisterOutOfRange);
        }
        let source_start = caller.register_start as usize + usize::from(args_base);
        let source_end = source_start + usize::from(args_count);
        self.push_call_range_at(
            function,
            register_count,
            Some(return_range),
            Some(call_site_pc),
        )?;
        let destination = self
            .frames
            .last()
            .expect("the verified call frame was just pushed")
            .register_start as usize;
        self.registers
            .copy_within(source_start..source_end, destination);
        self.frames[caller_index].pc = caller_next_pc;
        Ok(())
    }

    /// Copies a verified multi-slot result into its caller-owned range and
    /// then removes the callee frame. Every range check happens before either
    /// the copy or the pop, so failure leaves the continuation unchanged.
    pub(crate) fn return_verified_range(
        &mut self,
        source: u16,
        slots: u16,
    ) -> Result<Frame, FrameError> {
        let child_index = self
            .frames
            .len()
            .checked_sub(1)
            .ok_or(FrameError::NoFrame)?;
        let caller_index = child_index
            .checked_sub(1)
            .ok_or(FrameError::RegisterOutOfRange)?;
        let child = self.frames[child_index];
        let caller = self.frames[caller_index];
        let range = child.return_range.ok_or(FrameError::RegisterOutOfRange)?;
        if slots == 0
            || range.slots != slots
            || source
                .checked_add(slots)
                .is_none_or(|end| end > child.register_count)
            || range
                .start
                .checked_add(slots)
                .is_none_or(|end| end > caller.register_count)
        {
            return Err(FrameError::RegisterOutOfRange);
        }
        let source_start = child.register_start as usize + usize::from(source);
        let source_end = source_start + usize::from(slots);
        let destination = caller.register_start as usize + usize::from(range.start);
        self.registers
            .copy_within(source_start..source_end, destination);
        self.pop_mode(false)
    }

    pub fn pop(&mut self) -> Result<Frame, FrameError> {
        self.pop_mode(true)
    }

    pub(crate) fn pop_verified(&mut self) -> Result<Frame, FrameError> {
        self.pop_mode(false)
    }

    fn pop_mode(&mut self, clear_gc_backing: bool) -> Result<Frame, FrameError> {
        let frame = self.frames.pop().ok_or(FrameError::NoFrame)?;
        if clear_gc_backing {
            clear_gc_values(&mut self.registers[frame.register_start as usize..self.register_top]);
        }
        self.register_top = frame.register_start as usize;
        self.defer_records.truncate(frame.defer_start as usize);
        Ok(frame)
    }

    pub fn current(&self) -> Result<&Frame, FrameError> {
        self.frames.last().ok_or(FrameError::NoFrame)
    }

    /// M5 K1 kernel view: the live frame stack and register store,
    /// borrowed together so the trusted kernel (`trusted.rs`) can index
    /// them without re-deriving the current frame per access. Safe by
    /// itself - every unchecked access lives in the kernel module with
    /// its `SAFETY:` justification.
    pub(crate) fn trusted_parts(&self) -> (&[Frame], &[RuntimeValue]) {
        (&self.frames, &self.registers[..self.register_top])
    }

    /// Mutable sibling of [`Self::trusted_parts`].
    pub(crate) fn trusted_parts_mut(&mut self) -> (&mut [Frame], &mut [RuntimeValue]) {
        (&mut self.frames, &mut self.registers[..self.register_top])
    }

    pub fn current_mut(&mut self) -> Result<&mut Frame, FrameError> {
        self.frames.last_mut().ok_or(FrameError::NoFrame)
    }

    pub fn register(&self, index: usize) -> Result<RuntimeValue, FrameError> {
        let frame = self.current()?;
        if index >= usize::from(frame.register_count) {
            return Err(FrameError::RegisterOutOfRange);
        }
        Ok(self.registers[frame.register_start as usize + index])
    }

    pub fn set_register(&mut self, index: usize, value: RuntimeValue) -> Result<(), FrameError> {
        let frame = *self.current()?;
        if index >= usize::from(frame.register_count) {
            return Err(FrameError::RegisterOutOfRange);
        }
        self.registers[frame.register_start as usize + index] = value;
        Ok(())
    }

    pub fn frame_register(
        &self,
        frame_index: usize,
        register: u16,
    ) -> Result<RuntimeValue, FrameError> {
        let frame = self.frames.get(frame_index).ok_or(FrameError::NoFrame)?;
        if register >= frame.register_count {
            return Err(FrameError::RegisterOutOfRange);
        }
        Ok(self.registers[frame.register_start as usize + usize::from(register)])
    }

    pub fn set_frame_register(
        &mut self,
        frame_index: usize,
        register: u16,
        value: RuntimeValue,
    ) -> Result<(), FrameError> {
        let frame = *self.frames.get(frame_index).ok_or(FrameError::NoFrame)?;
        if register >= frame.register_count {
            return Err(FrameError::RegisterOutOfRange);
        }
        self.registers[frame.register_start as usize + usize::from(register)] = value;
        Ok(())
    }

    pub fn set_frame_pc(&mut self, frame_index: usize, pc: u32) -> Result<(), FrameError> {
        self.frames
            .get_mut(frame_index)
            .ok_or(FrameError::NoFrame)?
            .pc = pc;
        Ok(())
    }

    #[must_use]
    pub fn frame(&self, index: usize) -> Option<&Frame> {
        self.frames.get(index)
    }

    pub fn push_defer(&mut self, action: DeferAction) -> Result<(), FrameError> {
        if self.defer_records.len() >= self.limits.max_defer_records as usize
            || self.defer_records.len() == self.defer_records.capacity()
        {
            return Err(FrameError::DeferLimit);
        }
        self.defer_records.push(action);
        Ok(())
    }

    pub fn push_defer_call(
        &mut self,
        function: u32,
        arguments: &[RuntimeValue],
    ) -> Result<(), FrameError> {
        if arguments.len() > 8 {
            return Err(FrameError::DeferLimit);
        }
        let mut args = [RuntimeValue::Unit; 8];
        args[..arguments.len()].copy_from_slice(arguments);
        self.push_defer(DeferAction::Call {
            function,
            args,
            args_count: u8::try_from(arguments.len()).map_err(|_| FrameError::DeferLimit)?,
        })
    }

    pub fn pop_defer_for_current_frame(&mut self) -> Result<Option<DeferAction>, FrameError> {
        let defer_start = self.current()?.defer_start as usize;
        if self.defer_records.len() > defer_start {
            Ok(self.defer_records.pop())
        } else {
            Ok(None)
        }
    }

    pub(crate) fn peek_defer_for_current_frame(&self) -> Result<Option<DeferAction>, FrameError> {
        self.peek_defer_for_frame(self.depth().checked_sub(1).ok_or(FrameError::NoFrame)?)
    }

    pub(crate) fn peek_defer_for_frame(
        &self,
        frame_index: usize,
    ) -> Result<Option<DeferAction>, FrameError> {
        let frame = self.frames.get(frame_index).ok_or(FrameError::NoFrame)?;
        let defer_end = self
            .frames
            .get(frame_index + 1)
            .map_or(self.defer_records.len(), |child| child.defer_start as usize);
        let defer_start = frame.defer_start as usize;
        if defer_end > defer_start {
            Ok(self.defer_records.get(defer_end - 1).copied())
        } else {
            Ok(None)
        }
    }

    pub fn defers_rev(&self) -> impl Iterator<Item = DeferAction> + '_ {
        self.defer_records.iter().rev().copied()
    }

    #[must_use]
    pub(crate) fn defer_len(&self) -> usize {
        self.defer_records.len()
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub fn register_len(&self) -> usize {
        self.register_top
    }

    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        let mut roots = self
            .registers
            .iter()
            .take(self.register_top)
            .filter_map(|value| runtime_gc_root(*value))
            .collect::<Vec<_>>();
        self.extend_defer_gc_roots(&mut roots);
        roots
    }

    pub fn iter_gc_roots<'a>(
        &self,
        mut root_bitmap: impl FnMut(u32, u32) -> Option<&'a [bool]>,
    ) -> Result<Vec<GcRef>, FrameError> {
        let mut roots = Vec::new();
        for (frame_index, frame) in self.frames.iter().enumerate() {
            let suspended_pc = self
                .frames
                .get(frame_index + 1)
                .and_then(|callee| callee.call_site_pc)
                .unwrap_or(frame.pc);
            let bitmap =
                root_bitmap(frame.function, suspended_pc).ok_or(FrameError::RootMapMismatch)?;
            if bitmap.len() != usize::from(frame.register_count) {
                return Err(FrameError::RootMapMismatch);
            }
            for (register, is_root) in bitmap.iter().copied().enumerate() {
                if is_root {
                    match self.registers[frame.register_start as usize + register] {
                        RuntimeValue::String { reference, .. }
                        | RuntimeValue::Struct { reference, .. }
                        | RuntimeValue::Ref(reference)
                        | RuntimeValue::NamedRef { reference, .. } => {
                            roots.push(reference);
                        }
                        RuntimeValue::HostRequest(_)
                        | RuntimeValue::ResourceToken(_)
                        | RuntimeValue::Snapshot(_)
                        | RuntimeValue::Opaque { .. }
                        | RuntimeValue::MigrationOldObject(_)
                        | RuntimeValue::MigrationStagingObject(_)
                        | RuntimeValue::StateHandle { .. } => {}
                        RuntimeValue::I32(_)
                        | RuntimeValue::I64(_)
                        | RuntimeValue::F32(_)
                        | RuntimeValue::F64(_)
                        | RuntimeValue::Bool(_)
                        | RuntimeValue::Rune(_)
                        | RuntimeValue::Unit => return Err(FrameError::RootMapMismatch),
                    }
                }
            }
        }
        self.extend_defer_gc_roots(&mut roots);
        Ok(roots)
    }

    fn extend_defer_gc_roots(&self, roots: &mut Vec<GcRef>) {
        for action in &self.defer_records {
            let DeferAction::Call {
                args, args_count, ..
            } = action
            else {
                continue;
            };
            roots.extend(
                args.iter()
                    .take(usize::from(*args_count))
                    .filter_map(|value| runtime_gc_root(*value)),
            );
        }
    }
}

fn runtime_gc_root(value: RuntimeValue) -> Option<GcRef> {
    match value {
        RuntimeValue::String { reference, .. }
        | RuntimeValue::Struct { reference, .. }
        | RuntimeValue::Ref(reference)
        | RuntimeValue::NamedRef { reference, .. } => Some(reference),
        RuntimeValue::I32(_)
        | RuntimeValue::I64(_)
        | RuntimeValue::F32(_)
        | RuntimeValue::F64(_)
        | RuntimeValue::Bool(_)
        | RuntimeValue::Rune(_)
        | RuntimeValue::HostRequest(_)
        | RuntimeValue::ResourceToken(_)
        | RuntimeValue::Snapshot(_)
        | RuntimeValue::Opaque { .. }
        | RuntimeValue::MigrationOldObject(_)
        | RuntimeValue::MigrationStagingObject(_)
        | RuntimeValue::StateHandle { .. }
        | RuntimeValue::Unit => None,
    }
}

fn clear_gc_values(values: &mut [RuntimeValue]) {
    for value in values {
        if runtime_gc_root(*value).is_some() {
            *value = RuntimeValue::Unit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContinuationReservation, DeferAction, FrameArena, FrameError, FrameLimits, ReturnRange,
        RuntimeValue,
    };
    use crate::GcRef;

    #[test]
    fn register_slot_footprint_is_pinned() {
        // K4 baseline pin: every register and collection cell currently
        // carries the full 40-byte tagged RuntimeValue enum, even when it
        // holds one i32. The RawSlot arc shrinks this footprint
        // deliberately; any change to this number must be a conscious
        // layout decision, never an accident.
        assert_eq!(std::mem::size_of::<RuntimeValue>(), 40);
        assert_eq!(std::mem::size_of::<Option<RuntimeValue>>(), 40);
    }

    #[test]
    fn frame_limits_fail_before_mutation_and_pop_restores_segments() {
        let mut arena = FrameArena::new(FrameLimits {
            max_frame_bytes: 2 * std::mem::size_of::<RuntimeValue>(),
            max_call_depth: 1,
            max_defer_records: 1,
        });
        arena.push(1, 2).unwrap();
        arena.set_register(1, RuntimeValue::I32(9)).unwrap();
        arena.push_defer(DeferAction::SetFlag(0)).unwrap();
        assert_eq!(arena.push(2, 0), Err(FrameError::CallDepthLimit));
        assert_eq!(
            arena.push_defer(DeferAction::Trap),
            Err(FrameError::DeferLimit)
        );
        assert_eq!(arena.depth(), 1);
        assert_eq!(arena.register_len(), 2);
        arena.pop().unwrap();
        assert_eq!(arena.depth(), 0);
        assert_eq!(arena.register_len(), 0);
    }

    #[test]
    fn register_quota_rejection_does_not_create_a_frame() {
        let mut arena = FrameArena::new(FrameLimits {
            max_frame_bytes: std::mem::size_of::<RuntimeValue>(),
            ..FrameLimits::default()
        });
        assert_eq!(arena.push(1, 2), Err(FrameError::FrameByteLimit));
        assert_eq!(arena.depth(), 0);
    }

    #[test]
    fn verified_call_copies_one_argument_window_and_is_fail_atomic() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(1, 4).unwrap();
        arena.set_register(1, RuntimeValue::I32(7)).unwrap();
        arena.set_register(2, RuntimeValue::Bool(true)).unwrap();
        arena.push_verified_call(2, 4, 3, 5, 1, 2).unwrap();
        assert_eq!(arena.depth(), 2);
        assert_eq!(arena.current().unwrap().function, 2);
        assert_eq!(arena.register(0), Ok(RuntimeValue::I32(7)));
        assert_eq!(arena.register(1), Ok(RuntimeValue::Bool(true)));
        assert_eq!(arena.frame(0).unwrap().pc, 6);
        arena.pop_verified().unwrap();

        let before = arena.clone();
        assert_eq!(
            arena.push_verified_call(3, 1, 4, 6, 0, 2),
            Err(FrameError::RegisterOutOfRange)
        );
        assert_eq!(arena.frames, before.frames);
        assert_eq!(arena.register_top, before.register_top);
        assert_eq!(arena.registers, before.registers);
    }

    #[test]
    fn physical_call_returns_a_contiguous_range_atomically() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(1, 8).unwrap();
        arena.set_register(1, RuntimeValue::I64(11)).unwrap();
        arena.set_register(2, RuntimeValue::I64(13)).unwrap();
        arena
            .push_verified_call_range(2, 6, ReturnRange { start: 4, slots: 2 }, 7, 1, 2)
            .unwrap();
        assert_eq!(arena.register(0), Ok(RuntimeValue::I64(11)));
        assert_eq!(arena.register(1), Ok(RuntimeValue::I64(13)));
        arena.set_register(3, RuntimeValue::I64(17)).unwrap();
        arena.set_register(4, RuntimeValue::I64(19)).unwrap();

        let before = arena.clone();
        assert_eq!(
            arena.return_verified_range(3, 1),
            Err(FrameError::RegisterOutOfRange)
        );
        assert_eq!(arena.frames, before.frames);
        assert_eq!(arena.register_top, before.register_top);
        assert_eq!(arena.registers, before.registers);

        let completed = arena.return_verified_range(3, 2).unwrap();
        assert_eq!(
            completed.return_range,
            Some(ReturnRange { start: 4, slots: 2 })
        );
        assert_eq!(arena.depth(), 1);
        assert_eq!(arena.register(4), Ok(RuntimeValue::I64(17)));
        assert_eq!(arena.register(5), Ok(RuntimeValue::I64(19)));
    }

    #[test]
    fn verified_pooled_reset_retains_backing_but_exact_maps_ignore_stale_slots() {
        let limits = FrameLimits::default();
        let reservation = ContinuationReservation::for_limits(limits);
        let mut arena = FrameArena::with_reserved_capacity(limits, reservation).unwrap();
        arena.push(1, 2).unwrap();
        arena.set_register(0, RuntimeValue::I32(41)).unwrap();
        arena
            .set_register(
                1,
                RuntimeValue::Ref(GcRef {
                    index: 7,
                    generation: 3,
                }),
            )
            .unwrap();
        let initialized_high_water = arena.registers.len();

        arena.reset_for_verified(limits, reservation).unwrap();
        assert_eq!(arena.register_len(), 0);
        assert_eq!(arena.registers.len(), initialized_high_water);
        assert_eq!(arena.registers[0], RuntimeValue::I32(41));
        assert!(matches!(arena.registers[1], RuntimeValue::Ref(_)));

        arena.push(2, 2).unwrap();
        let no_roots = [false, false];
        assert!(
            arena
                .iter_gc_roots(|_, _| Some(&no_roots))
                .unwrap()
                .is_empty()
        );
        assert_eq!(arena.registers.len(), initialized_high_water);
    }

    #[test]
    fn root_maps_accept_unmarked_stale_references_but_reject_forged_roots() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(7, 4).unwrap();
        arena
            .set_register(
                0,
                RuntimeValue::Ref(GcRef {
                    index: 3,
                    generation: 1,
                }),
            )
            .unwrap();
        arena.set_register(1, RuntimeValue::I32(42)).unwrap();
        arena
            .set_register(
                2,
                RuntimeValue::Opaque {
                    value: 9,
                    type_id: nexa_core::StableId::from_name("HostOpaque"),
                },
            )
            .unwrap();
        arena
            .set_register(
                3,
                RuntimeValue::StateHandle {
                    handle_type: nexa_core::StableId::from_name("StateHandle<Model>"),
                    domain: 1,
                    stable_id: nexa_core::StableId::from_name("model"),
                    generation: 0,
                },
            )
            .unwrap();

        let opaque_roots = [false, false, true, true];
        assert_eq!(
            arena
                .iter_gc_roots(|function, _| {
                    assert_eq!(function, 7);
                    Some(&opaque_roots)
                })
                .unwrap(),
            Vec::new()
        );
        let forged_scalar_root = [false, true, false, false];
        assert_eq!(
            arena.iter_gc_roots(|_, _| Some(&forged_scalar_root)),
            Err(FrameError::RootMapMismatch)
        );
        let wrong_width = [false];
        assert_eq!(
            arena.iter_gc_roots(|_, _| Some(&wrong_width)),
            Err(FrameError::RootMapMismatch)
        );
        assert_eq!(
            arena.iter_gc_roots(|_, _| None),
            Err(FrameError::RootMapMismatch)
        );
    }

    #[test]
    fn defer_call_gc_roots_cover_only_gc_backed_arguments_before_args_count() {
        let references = std::array::from_fn::<_, 5, _>(|index| GcRef {
            index: u32::try_from(index).unwrap(),
            generation: 1,
        });
        let type_id = nexa_core::StableId::from_name("DeferredNominal");
        let args = [
            RuntimeValue::String {
                reference: references[0],
                hash: 1,
            },
            RuntimeValue::Struct {
                reference: references[1],
                type_id,
                hash: 2,
            },
            RuntimeValue::Ref(references[2]),
            RuntimeValue::NamedRef {
                reference: references[3],
                type_id,
            },
            RuntimeValue::Opaque { value: 9, type_id },
            RuntimeValue::I32(42),
            RuntimeValue::Ref(references[4]),
            RuntimeValue::Unit,
        ];
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(7, 1).unwrap();
        arena.set_register(0, RuntimeValue::Bool(true)).unwrap();
        arena
            .push_defer(DeferAction::Call {
                function: 8,
                args,
                args_count: 6,
            })
            .unwrap();

        let expected = references[..4].to_vec();
        assert_eq!(arena.gc_roots(), expected);
        let no_roots = [false];
        assert_eq!(
            arena.iter_gc_roots(|_, _| Some(&no_roots)).unwrap(),
            expected
        );
    }
}
