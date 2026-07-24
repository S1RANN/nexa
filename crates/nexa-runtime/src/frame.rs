use std::fmt;
use std::mem::size_of;

use crate::GcRef;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeValue {
    I32(i32),
    Bool(bool),
    Ref(GcRef),
    NamedRef {
        reference: GcRef,
        type_id: nexa_core::StableId,
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
    pub register_start: u32,
    pub register_count: u16,
    pub return_target: Option<u16>,
    pub defer_start: u32,
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
            registers: Vec::with_capacity(reservation.register_capacity as usize),
            defer_records: Vec::with_capacity(reservation.defer_capacity as usize),
            limits,
        })
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
        if self.frames.len() >= self.limits.max_call_depth as usize {
            return Err(FrameError::CallDepthLimit);
        }
        let next_registers = self
            .registers
            .len()
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
            register_start: u32::try_from(self.registers.len())
                .map_err(|_| FrameError::FrameByteLimit)?,
            register_count,
            return_target,
            defer_start: u32::try_from(self.defer_records.len())
                .map_err(|_| FrameError::DeferLimit)?,
        });
        self.registers.resize(next_registers, RuntimeValue::Unit);
        Ok(())
    }

    pub fn pop(&mut self) -> Result<Frame, FrameError> {
        let frame = self.frames.pop().ok_or(FrameError::NoFrame)?;
        self.registers.truncate(frame.register_start as usize);
        self.defer_records.truncate(frame.defer_start as usize);
        Ok(frame)
    }

    pub fn current(&self) -> Result<&Frame, FrameError> {
        self.frames.last().ok_or(FrameError::NoFrame)
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

    pub fn defers_rev(&self) -> impl Iterator<Item = DeferAction> + '_ {
        self.defer_records.iter().rev().copied()
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    #[must_use]
    pub fn register_len(&self) -> usize {
        self.registers.len()
    }

    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        self.registers
            .iter()
            .filter_map(|value| match value {
                RuntimeValue::Ref(reference) | RuntimeValue::NamedRef { reference, .. } => {
                    Some(*reference)
                }
                RuntimeValue::I32(_) | RuntimeValue::Bool(_) | RuntimeValue::Unit => None,
            })
            .collect()
    }

    pub fn iter_gc_roots(
        &self,
        mut root_bitmap: impl FnMut(u32, u32) -> Option<Vec<bool>>,
    ) -> Result<Vec<GcRef>, FrameError> {
        let mut roots = Vec::new();
        for frame in &self.frames {
            let bitmap =
                root_bitmap(frame.function, frame.pc).ok_or(FrameError::RegisterOutOfRange)?;
            if bitmap.len() != usize::from(frame.register_count) {
                return Err(FrameError::RegisterOutOfRange);
            }
            for (register, is_root) in bitmap.into_iter().enumerate() {
                if is_root {
                    match self.registers[frame.register_start as usize + register] {
                        RuntimeValue::Ref(reference) | RuntimeValue::NamedRef { reference, .. } => {
                            roots.push(reference);
                        }
                        RuntimeValue::I32(_) | RuntimeValue::Bool(_) | RuntimeValue::Unit => {}
                    }
                }
            }
        }
        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::{DeferAction, FrameArena, FrameError, FrameLimits, RuntimeValue};

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
}
