use std::fmt;

use nexa_core::RawHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Vacant,
    Occupied,
    Retired,
}

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    state: SlotState,
    value: Option<T>,
}

/// Generation-protected storage for one kind of runtime object.
#[derive(Debug)]
pub struct SlotPool<T> {
    realm_id: u32,
    max_capacity: u32,
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleError {
    WrongRealm {
        expected: u32,
        actual: u32,
    },
    OutOfRange {
        index: u32,
    },
    StaleGeneration {
        index: u32,
        expected: u32,
        actual: u32,
    },
    Vacant {
        index: u32,
    },
    Retired {
        index: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotAllocError {
    CapacityExhausted,
    NoFreeSlot,
}

impl fmt::Display for SlotAllocError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExhausted => formatter.write_str("slot capacity exhausted"),
            Self::NoFreeSlot => formatter.write_str("no free slot is available"),
        }
    }
}

impl std::error::Error for SlotAllocError {}

impl fmt::Display for HandleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongRealm { expected, actual } => {
                write!(formatter, "handle realm {actual} does not match {expected}")
            }
            Self::OutOfRange { index } => write!(formatter, "handle index {index} is out of range"),
            Self::StaleGeneration {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "handle index {index} has generation {actual}, expected {expected}"
            ),
            Self::Vacant { index } => write!(formatter, "handle index {index} is vacant"),
            Self::Retired { index } => write!(formatter, "handle index {index} is retired"),
        }
    }
}

impl std::error::Error for HandleError {}

impl<T> SlotPool<T> {
    #[must_use]
    pub const fn new(realm_id: u32) -> Self {
        Self {
            realm_id,
            max_capacity: u32::MAX,
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_capacity_limit(realm_id: u32, max_capacity: u32) -> Self {
        // Pre-reserve ordinary runtime limits without turning an intentionally broad logical
        // limit (for example u32::MAX in handle tests) into a process-sized allocation.
        let capacity = usize::try_from(max_capacity.min(4_096)).unwrap_or(4_096);
        Self {
            realm_id,
            max_capacity,
            slots: Vec::with_capacity(capacity),
            free: Vec::with_capacity(capacity),
        }
    }

    pub fn try_allocate(&mut self, value: T) -> Result<RawHandle, SlotAllocError> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert_eq!(slot.state, SlotState::Vacant);
            debug_assert!(slot.value.is_none());
            slot.state = SlotState::Occupied;
            slot.value = Some(value);
            return Ok(RawHandle::new(self.realm_id, index, slot.generation));
        }

        let index =
            u32::try_from(self.slots.len()).map_err(|_| SlotAllocError::CapacityExhausted)?;
        if index >= self.max_capacity {
            return Err(SlotAllocError::CapacityExhausted);
        }
        self.slots.push(Slot {
            generation: 0,
            state: SlotState::Occupied,
            value: Some(value),
        });
        Ok(RawHandle::new(self.realm_id, index, 0))
    }

    pub fn resolve(&self, handle: RawHandle) -> Result<&T, HandleError> {
        let slot = self.resolve_slot(handle)?;
        slot.value.as_ref().ok_or(HandleError::Vacant {
            index: handle.index,
        })
    }

    pub fn resolve_mut(&mut self, handle: RawHandle) -> Result<&mut T, HandleError> {
        let slot = self.resolve_slot_mut(handle)?;
        slot.value.as_mut().ok_or(HandleError::Vacant {
            index: handle.index,
        })
    }

    pub fn release(&mut self, handle: RawHandle) -> Result<T, HandleError> {
        let slot = self.resolve_slot_mut(handle)?;
        let value = slot.value.take().ok_or(HandleError::Vacant {
            index: handle.index,
        })?;
        if slot.generation == u32::MAX {
            slot.state = SlotState::Retired;
        } else {
            slot.generation += 1;
            slot.state = SlotState::Vacant;
            self.free.push(handle.index);
        }
        Ok(value)
    }

    #[must_use]
    pub fn occupied_len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.state == SlotState::Occupied)
            .count()
    }

    #[must_use]
    pub fn occupied_handles(&self) -> Vec<RawHandle> {
        self.occupied_handles_iter().collect()
    }

    pub fn occupied_handles_iter(&self) -> impl Iterator<Item = RawHandle> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.state == SlotState::Occupied)
            .map(|(index, slot)| {
                RawHandle::new(
                    self.realm_id,
                    u32::try_from(index).expect("slot indices originate as u32"),
                    slot.generation,
                )
            })
    }

    #[must_use]
    pub fn reserved_capacity(&self) -> usize {
        self.slots.capacity().min(self.free.capacity())
    }

    fn resolve_slot(&self, handle: RawHandle) -> Result<&Slot<T>, HandleError> {
        self.validate_realm(handle)?;
        let slot = self
            .slots
            .get(handle.index as usize)
            .ok_or(HandleError::OutOfRange {
                index: handle.index,
            })?;
        validate_slot(slot, handle)?;
        Ok(slot)
    }

    fn resolve_slot_mut(&mut self, handle: RawHandle) -> Result<&mut Slot<T>, HandleError> {
        self.validate_realm(handle)?;
        let slot = self
            .slots
            .get_mut(handle.index as usize)
            .ok_or(HandleError::OutOfRange {
                index: handle.index,
            })?;
        validate_slot(slot, handle)?;
        Ok(slot)
    }

    fn validate_realm(&self, handle: RawHandle) -> Result<(), HandleError> {
        if handle.realm_id == self.realm_id {
            Ok(())
        } else {
            Err(HandleError::WrongRealm {
                expected: self.realm_id,
                actual: handle.realm_id,
            })
        }
    }
}

fn validate_slot<T>(slot: &Slot<T>, handle: RawHandle) -> Result<(), HandleError> {
    if slot.generation != handle.generation {
        return Err(HandleError::StaleGeneration {
            index: handle.index,
            expected: slot.generation,
            actual: handle.generation,
        });
    }
    match slot.state {
        SlotState::Occupied => Ok(()),
        SlotState::Vacant => Err(HandleError::Vacant {
            index: handle.index,
        }),
        SlotState::Retired => Err(HandleError::Retired {
            index: handle.index,
        }),
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::RawHandle;

    use super::{HandleError, SlotPool};

    #[test]
    fn stale_handle_cannot_resolve_a_reused_slot() {
        let mut pool = SlotPool::new(9);
        let first = pool.try_allocate("first").unwrap();
        assert_eq!(pool.release(first), Ok("first"));
        let second = pool.try_allocate("second").unwrap();
        assert_eq!(first.index, second.index);
        assert_ne!(first.generation, second.generation);
        assert!(matches!(
            pool.resolve(first),
            Err(HandleError::StaleGeneration { .. })
        ));
        assert_eq!(pool.resolve(second), Ok(&"second"));
    }

    #[test]
    fn cross_realm_handle_is_rejected_before_index_lookup() {
        let mut pool = SlotPool::new(3);
        let handle = pool.try_allocate(42).unwrap();
        let foreign = RawHandle::new(4, handle.index, handle.generation);
        assert!(matches!(
            pool.resolve(foreign),
            Err(HandleError::WrongRealm { .. })
        ));
    }

    #[test]
    fn capacity_exhaustion_is_fallible_and_preserves_existing_slots() {
        let mut pool = SlotPool::with_capacity_limit(3, 1);
        let handle = pool.try_allocate(42).unwrap();
        assert_eq!(
            pool.try_allocate(99),
            Err(super::SlotAllocError::CapacityExhausted)
        );
        assert_eq!(pool.resolve(handle), Ok(&42));
    }
}
