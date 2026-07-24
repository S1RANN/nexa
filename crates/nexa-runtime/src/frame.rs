use std::fmt;
use std::mem::size_of;

use crate::GcRef;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeValue {
    I32(i32),
    Bool(bool),
    Ref(GcRef),
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
    pub program_id: u32,
    pub pc: usize,
    register_start: usize,
    register_count: usize,
    defer_start: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferAction {
    ReleaseCounter(u32),
    SetFlag(u32),
    Trap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    CallDepthLimit,
    FrameByteLimit,
    DeferLimit,
    NoFrame,
    RegisterOutOfRange,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for FrameError {}

/// Safe, index-based continuation storage. Capacity is reserved before execution starts.
#[derive(Debug)]
pub struct FrameArena {
    frames: Vec<Frame>,
    registers: Vec<RuntimeValue>,
    defer_records: Vec<DeferAction>,
    limits: FrameLimits,
}

impl FrameArena {
    #[must_use]
    pub fn new(limits: FrameLimits) -> Self {
        let register_capacity = limits.max_frame_bytes / size_of::<RuntimeValue>().max(1);
        Self {
            frames: Vec::with_capacity(limits.max_call_depth as usize),
            registers: Vec::with_capacity(register_capacity),
            defer_records: Vec::with_capacity(limits.max_defer_records as usize),
            limits,
        }
    }

    pub fn push(&mut self, program_id: u32, register_count: usize) -> Result<(), FrameError> {
        if self.frames.len() >= self.limits.max_call_depth as usize {
            return Err(FrameError::CallDepthLimit);
        }
        let next_registers = self
            .registers
            .len()
            .checked_add(register_count)
            .ok_or(FrameError::FrameByteLimit)?;
        let next_bytes = next_registers
            .checked_mul(size_of::<RuntimeValue>())
            .ok_or(FrameError::FrameByteLimit)?;
        if next_bytes > self.limits.max_frame_bytes {
            return Err(FrameError::FrameByteLimit);
        }
        self.frames.push(Frame {
            program_id,
            pc: 0,
            register_start: self.registers.len(),
            register_count,
            defer_start: self.defer_records.len(),
        });
        self.registers.resize(next_registers, RuntimeValue::Unit);
        Ok(())
    }

    pub fn pop(&mut self) -> Result<Frame, FrameError> {
        let frame = self.frames.pop().ok_or(FrameError::NoFrame)?;
        self.registers.truncate(frame.register_start);
        self.defer_records.truncate(frame.defer_start);
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
        if index >= frame.register_count {
            return Err(FrameError::RegisterOutOfRange);
        }
        Ok(self.registers[frame.register_start + index])
    }

    pub fn set_register(&mut self, index: usize, value: RuntimeValue) -> Result<(), FrameError> {
        let frame = *self.current()?;
        if index >= frame.register_count {
            return Err(FrameError::RegisterOutOfRange);
        }
        self.registers[frame.register_start + index] = value;
        Ok(())
    }

    pub fn push_defer(&mut self, action: DeferAction) -> Result<(), FrameError> {
        if self.defer_records.len() >= self.limits.max_defer_records as usize {
            return Err(FrameError::DeferLimit);
        }
        self.defer_records.push(action);
        Ok(())
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
                RuntimeValue::Ref(reference) => Some(*reference),
                RuntimeValue::I32(_) | RuntimeValue::Bool(_) | RuntimeValue::Unit => None,
            })
            .collect()
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
