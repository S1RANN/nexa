use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;

use nexa_core::StableId;

use crate::{GcRef, TaskHandle};

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
pub struct StatefulRegistry {
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
    pub const fn module_id(&self) -> u32 {
        self.module_id
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
}

#[derive(Clone, Debug)]
pub struct ModuleEpochRoot {
    pub module_id: u32,
    pub epoch: u64,
    pub functions: Arc<[u32]>,
    pub state_registry: StatefulRegistry,
    pub schema_hash: StableId,
    pub host_hash: StableId,
    _vm_thread_only: PhantomData<Rc<()>>,
}

impl ModuleEpochRoot {
    #[must_use]
    pub fn new(
        module_id: u32,
        epoch: u64,
        functions: Arc<[u32]>,
        state_registry: StatefulRegistry,
        schema_hash: StableId,
        host_hash: StableId,
    ) -> Self {
        Self {
            module_id,
            epoch,
            functions,
            state_registry,
            schema_hash,
            host_hash,
            _vm_thread_only: PhantomData,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadState {
    Active,
    Prepared,
    Quiescing,
    Staging,
    ActivationFaulted,
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

impl fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ReloadError {}

/// Single-module reload transaction. The root is published exactly once at commit.
#[derive(Debug)]
pub struct ReloadManager {
    current: ModuleEpochRoot,
    candidate: Option<ModuleEpochRoot>,
    staged: Option<ModuleEpochRoot>,
    state: ReloadState,
    paused_tasks: Vec<TaskHandle>,
    deferred_completions: Vec<(u64, i32)>,
    max_staging_states: usize,
}

impl ReloadManager {
    #[must_use]
    pub fn new(current: ModuleEpochRoot, max_staging_states: usize) -> Self {
        Self {
            current,
            candidate: None,
            staged: None,
            state: ReloadState::Active,
            paused_tasks: Vec::new(),
            deferred_completions: Vec::new(),
            max_staging_states,
        }
    }

    pub fn prepare(&mut self, candidate: ModuleEpochRoot) -> Result<(), ReloadError> {
        if self.state != ReloadState::Active {
            return Err(ReloadError::InvalidState);
        }
        if candidate.epoch <= self.current.epoch {
            return Err(ReloadError::EpochNotNewer);
        }
        if candidate.host_hash != self.current.host_hash {
            return Err(ReloadError::HostHashMismatch);
        }
        if self.current.state_registry.entries.len() > self.max_staging_states {
            return Err(ReloadError::StagingCapacity);
        }
        self.candidate = Some(candidate);
        self.state = ReloadState::Prepared;
        Ok(())
    }

    pub fn begin_quiesce(&mut self) -> Result<(), ReloadError> {
        if self.state != ReloadState::Prepared {
            return Err(ReloadError::InvalidState);
        }
        self.state = ReloadState::Quiescing;
        Ok(())
    }

    pub fn task_paused(&mut self, task: TaskHandle) -> Result<(), ReloadError> {
        if self.state != ReloadState::Quiescing {
            return Err(ReloadError::InvalidState);
        }
        if !self.paused_tasks.contains(&task) {
            self.paused_tasks.push(task);
        }
        Ok(())
    }

    pub fn defer_completion(&mut self, epoch: u64, value: i32) -> Result<(), ReloadError> {
        if self.state != ReloadState::Quiescing {
            return Err(ReloadError::InvalidState);
        }
        self.deferred_completions.push((epoch, value));
        Ok(())
    }

    pub fn stage(
        &mut self,
        migrate: impl FnOnce(&StatefulRegistry, &mut StatefulRegistry) -> Result<(), String>,
    ) -> Result<(), ReloadError> {
        if self.state != ReloadState::Quiescing {
            return Err(ReloadError::InvalidState);
        }
        self.state = ReloadState::Staging;
        let mut candidate = self.candidate.take().ok_or(ReloadError::InvalidState)?;
        let mut migrated = StatefulRegistry::new(candidate.module_id);
        migrate(&self.current.state_registry, &mut migrated).map_err(ReloadError::Migration)?;
        if migrated.entries.len() > self.max_staging_states
            || migrated
                .handles()
                .iter()
                .any(|handle| handle.module_id != candidate.module_id)
        {
            return Err(ReloadError::GraphCheck);
        }
        candidate.state_registry = migrated;
        self.staged = Some(candidate);
        Ok(())
    }

    pub fn commit(
        &mut self,
        activate: impl FnOnce(&ModuleEpochRoot) -> Result<(), String>,
    ) -> Result<Vec<(u64, i32)>, ReloadError> {
        if self.state != ReloadState::Staging {
            return Err(ReloadError::InvalidState);
        }
        self.current = self.staged.take().ok_or(ReloadError::InvalidState)?;
        self.paused_tasks.clear();
        let completions = std::mem::take(&mut self.deferred_completions);
        match activate(&self.current) {
            Ok(()) => {
                self.state = ReloadState::Active;
                Ok(completions)
            }
            Err(error) => {
                self.state = ReloadState::ActivationFaulted;
                Err(ReloadError::Activation(error))
            }
        }
    }

    pub fn rollback_timeout(&mut self) -> Result<Vec<TaskHandle>, ReloadError> {
        if !matches!(
            self.state,
            ReloadState::Prepared | ReloadState::Quiescing | ReloadState::Staging
        ) {
            return Err(ReloadError::InvalidState);
        }
        self.candidate = None;
        self.staged = None;
        self.deferred_completions.clear();
        self.state = ReloadState::Active;
        Ok(std::mem::take(&mut self.paused_tasks))
    }

    #[must_use]
    pub fn accepts_calls(&self) -> bool {
        self.state == ReloadState::Active
    }

    #[must_use]
    pub const fn current(&self) -> &ModuleEpochRoot {
        &self.current
    }

    #[must_use]
    pub const fn state(&self) -> ReloadState {
        self.state
    }

    pub fn reset_fault(&mut self) -> Result<(), ReloadError> {
        if self.state != ReloadState::ActivationFaulted {
            return Err(ReloadError::InvalidState);
        }
        self.state = ReloadState::Active;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexa_core::StableId;

    use super::{
        ModuleEpochRoot, ReloadError, ReloadManager, ReloadState, StateValue, StatefulError,
        StatefulRegistry,
    };

    fn root(epoch: u64) -> ModuleEpochRoot {
        ModuleEpochRoot::new(
            1,
            epoch,
            Arc::from([0_u32]),
            StatefulRegistry::new(1),
            StableId::from_name("schema"),
            StableId::from_name("host"),
        )
    }

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

    #[test]
    fn activation_fault_keeps_new_root_and_does_not_reactivate_calls() {
        let mut reload = ReloadManager::new(root(1), 4);
        reload.prepare(root(2)).unwrap();
        reload.begin_quiesce().unwrap();
        reload
            .stage(|_, target| {
                target
                    .insert(StableId::from_name("score"), StateValue::I32(3))
                    .map_err(|error| error.to_string())?;
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            reload.commit(|_| Err("activation".into())),
            Err(ReloadError::Activation(_))
        ));
        assert_eq!(reload.current().epoch, 2);
        assert_eq!(reload.state(), ReloadState::ActivationFaulted);
        assert!(!reload.accepts_calls());
        reload.reset_fault().unwrap();
        assert!(reload.accepts_calls());
    }
}
