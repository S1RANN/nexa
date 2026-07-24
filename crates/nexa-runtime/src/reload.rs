use std::collections::BTreeMap;
use std::fmt;

use nexa_core::StableId;

use crate::{GcRef, ModuleHandle, TaskHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateHandle {
    pub module_id: u32,
    pub stable_id: StableId,
    pub generation: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateValue {
    I32(i32),
    Bool(bool),
    Ref(GcRef),
}

#[derive(Clone, Debug)]
struct StateEntry {
    generation: u32,
    value: StateValue,
}

#[derive(Clone, Debug)]
pub(crate) struct StatefulRegistry {
    module_id: u32,
    entries: BTreeMap<StableId, StateEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatefulError {
    WrongModule { expected: u32, actual: u32 },
    Missing(StableId),
    StaleGeneration,
    GenerationExhausted,
}

impl fmt::Display for StatefulError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StatefulError {}

impl StatefulRegistry {
    #[must_use]
    pub const fn new(module_id: u32) -> Self {
        Self {
            module_id,
            entries: BTreeMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        stable_id: StableId,
        value: StateValue,
    ) -> Result<StateHandle, StatefulError> {
        let generation = self.entries.get(&stable_id).map_or(Ok(0), |entry| {
            entry
                .generation
                .checked_add(1)
                .ok_or(StatefulError::GenerationExhausted)
        })?;
        self.entries
            .insert(stable_id, StateEntry { generation, value });
        Ok(StateHandle {
            module_id: self.module_id,
            stable_id,
            generation,
        })
    }

    pub fn resolve(&self, handle: StateHandle) -> Result<&StateValue, StatefulError> {
        if handle.module_id != self.module_id {
            return Err(StatefulError::WrongModule {
                expected: self.module_id,
                actual: handle.module_id,
            });
        }
        let entry = self
            .entries
            .get(&handle.stable_id)
            .ok_or(StatefulError::Missing(handle.stable_id))?;
        if entry.generation != handle.generation {
            return Err(StatefulError::StaleGeneration);
        }
        Ok(&entry.value)
    }

    #[must_use]
    pub fn handles(&self) -> Vec<StateHandle> {
        self.entries
            .iter()
            .map(|(stable_id, entry)| StateHandle {
                module_id: self.module_id,
                stable_id: *stable_id,
                generation: entry.generation,
            })
            .collect()
    }

    #[must_use]
    pub fn clone_for_module(&self, module_id: u32) -> Self {
        Self {
            module_id,
            entries: self.entries.clone(),
        }
    }

    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        self.entries
            .values()
            .filter_map(|entry| match entry.value {
                StateValue::Ref(reference) => Some(reference),
                StateValue::I32(_) | StateValue::Bool(_) => None,
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReloadError {
    InvalidState,
    EpochNotNewer,
    HostHashMismatch,
    StagingCapacity,
    Migration(String),
    GraphCheck,
    QuiesceTimeout,
    Activation(String),
}

#[derive(Debug)]
pub(crate) struct ReloadTransaction {
    pub(crate) old_module: ModuleHandle,
    pub(crate) candidate: ModuleHandle,
    pub(crate) paused_tasks: Vec<TaskHandle>,
}

#[derive(Debug, Default)]
pub(crate) struct ReloadCoordinator {
    transaction: Option<ReloadTransaction>,
}

impl ReloadCoordinator {
    pub(crate) fn begin(&mut self, transaction: ReloadTransaction) -> Result<(), ReloadError> {
        if self.transaction.is_some() {
            return Err(ReloadError::InvalidState);
        }
        self.transaction = Some(transaction);
        Ok(())
    }

    pub(crate) fn transaction(&self) -> Result<&ReloadTransaction, ReloadError> {
        self.transaction.as_ref().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn transaction_mut(&mut self) -> Result<&mut ReloadTransaction, ReloadError> {
        self.transaction.as_mut().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn finish(&mut self) -> Result<ReloadTransaction, ReloadError> {
        self.transaction.take().ok_or(ReloadError::InvalidState)
    }

    #[must_use]
    pub(crate) fn active(&self) -> bool {
        self.transaction.is_some()
    }
}

impl fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReloadError {}

#[cfg(test)]
mod tests {
    use nexa_core::StableId;

    use super::{StateValue, StatefulError, StatefulRegistry};

    #[test]
    fn state_handles_are_generation_and_module_checked() {
        let mut registry = StatefulRegistry::new(1);
        let id = StableId::from_name("score");
        let old = registry.insert(id, StateValue::I32(1)).unwrap();
        let new = registry.insert(id, StateValue::I32(2)).unwrap();
        assert_ne!(old, new);
        assert_eq!(registry.resolve(old), Err(StatefulError::StaleGeneration));
        assert_eq!(registry.resolve(new), Ok(&StateValue::I32(2)));
    }
}
