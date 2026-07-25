use std::collections::BTreeMap;
use std::fmt;

use nexa_core::StableId;

use crate::interpreter::InterpreterMigration;
use crate::machines::reload;
use crate::scheduler::SchedulerCheckpoint;
use crate::task::TaskExecution;
use crate::{GcRef, ModuleHandle, RuntimeValue, TaskHandle, TaskSnapshot};

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
    Handle(StateHandle),
    Object(StateObject),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateObject {
    pub type_id: StableId,
    pub version: u32,
    pub fields: BTreeMap<StableId, StateValue>,
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

    pub fn migrate_for_module(
        &self,
        module_id: u32,
        schema: &nexa_bytecode::StateSchema,
    ) -> Result<Self, StatefulError> {
        let mut entries = BTreeMap::new();
        for (stable_id, entry) in &self.entries {
            entries.insert(
                *stable_id,
                StateEntry {
                    generation: entry.generation,
                    value: migrate_state_value(&entry.value, module_id, schema)?,
                },
            );
        }
        let migrated = Self { module_id, entries };
        migrated.validate_schema(schema)?;
        migrated.validate_handles()?;
        Ok(migrated)
    }

    pub fn validate_schema(
        &self,
        schema: &nexa_bytecode::StateSchema,
    ) -> Result<(), StatefulError> {
        for entry in self.entries.values() {
            validate_state_value(&entry.value, schema)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        let mut roots = Vec::new();
        for entry in self.entries.values() {
            collect_state_roots(&entry.value, &mut roots);
        }
        roots
    }

    fn validate_handles(&self) -> Result<(), StatefulError> {
        for entry in self.entries.values() {
            validate_state_handles(&entry.value, self)?;
        }
        Ok(())
    }
}

pub(crate) struct MigrationContext {
    old: StatefulRegistry,
    staging: StatefulRegistry,
    schema: nexa_bytecode::StateSchema,
    forwarding: BTreeMap<StableId, StableId>,
    touched: bool,
}

impl MigrationContext {
    #[must_use]
    pub(crate) fn new(
        old: StatefulRegistry,
        module_id: u32,
        schema: nexa_bytecode::StateSchema,
    ) -> Self {
        Self {
            old,
            staging: StatefulRegistry::new(module_id),
            schema,
            forwarding: BTreeMap::new(),
            touched: false,
        }
    }

    pub(crate) fn finish(mut self) -> Result<StatefulRegistry, StatefulError> {
        if !self.touched {
            return self
                .old
                .migrate_for_module(self.staging.module_id, &self.schema);
        }
        for entry in self.staging.entries.values_mut() {
            remap_state_handles(&mut entry.value, &self.forwarding);
        }
        self.staging.validate_schema(&self.schema)?;
        self.staging.validate_handles()?;
        Ok(self.staging)
    }
}

impl InterpreterMigration for MigrationContext {
    fn old_get(
        &mut self,
        stable_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, String> {
        let entry = self
            .old
            .entries
            .get(&stable_id)
            .ok_or_else(|| format!("old state {stable_id:?} does not exist"))?;
        let value = state_to_runtime_value(stable_id, &entry.value);
        if runtime_state_type(value) != expected {
            return Err("old state type does not match migration opcode".into());
        }
        Ok(value)
    }

    fn new_create(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, String> {
        let schema = self
            .schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == type_id)
            .ok_or_else(|| format!("candidate state type {type_id:?} does not exist"))?;
        if self.staging.entries.contains_key(&stable_id) {
            return Err("new state object already exists".into());
        }
        self.staging.entries.insert(
            stable_id,
            StateEntry {
                generation: 0,
                value: StateValue::Object(StateObject {
                    type_id,
                    version: schema.version,
                    fields: BTreeMap::new(),
                }),
            },
        );
        self.touched = true;
        Ok(RuntimeValue::Opaque {
            type_id,
            value: stable_id.0,
        })
    }

    fn new_set(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        value: RuntimeValue,
    ) -> Result<(), String> {
        let RuntimeValue::Opaque {
            type_id,
            value: object_id,
        } = object
        else {
            return Err("STATE_NEW_SET requires a staging object".into());
        };
        let value = runtime_to_state_value(value, &self.staging)?;
        let entry = self
            .staging
            .entries
            .get_mut(&StableId(object_id))
            .ok_or_else(|| "staging object does not exist".to_string())?;
        let StateValue::Object(object) = &mut entry.value else {
            return Err("staging value is not an object".into());
        };
        if object.type_id != type_id {
            return Err("staging object type mismatch".into());
        }
        object.fields.insert(field_id, value);
        self.touched = true;
        Ok(())
    }

    fn remap(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), String> {
        let RuntimeValue::Opaque {
            value: target_id, ..
        } = target
        else {
            return Err("STATE_HANDLE_REMAP requires a staging object".into());
        };
        let target_id = StableId(target_id);
        if !self.staging.entries.contains_key(&target_id) {
            return Err("remap target does not exist".into());
        }
        self.forwarding.insert(old_id, target_id);
        self.touched = true;
        Ok(())
    }

    fn delete(&mut self, stable_id: StableId) -> Result<(), String> {
        self.staging.entries.remove(&stable_id);
        self.touched = true;
        Ok(())
    }
}

fn state_to_runtime_value(stable_id: StableId, value: &StateValue) -> RuntimeValue {
    match value {
        StateValue::I32(value) => RuntimeValue::I32(*value),
        StateValue::Bool(value) => RuntimeValue::Bool(*value),
        StateValue::Ref(reference) => RuntimeValue::Ref(*reference),
        StateValue::Handle(handle) => RuntimeValue::Opaque {
            type_id: StableId::from_name("StateHandle"),
            value: handle.stable_id.0,
        },
        StateValue::Object(object) => RuntimeValue::Opaque {
            type_id: object.type_id,
            value: stable_id.0,
        },
    }
}

fn runtime_to_state_value(
    value: RuntimeValue,
    staging: &StatefulRegistry,
) -> Result<StateValue, String> {
    match value {
        RuntimeValue::I32(value) => Ok(StateValue::I32(value)),
        RuntimeValue::Bool(value) => Ok(StateValue::Bool(value)),
        RuntimeValue::Ref(reference) | RuntimeValue::NamedRef { reference, .. } => {
            Ok(StateValue::Ref(reference))
        }
        RuntimeValue::Opaque { value, .. } => {
            let stable_id = StableId(value);
            let generation = staging
                .entries
                .get(&stable_id)
                .ok_or_else(|| "state handle target does not exist".to_string())?
                .generation;
            Ok(StateValue::Handle(StateHandle {
                module_id: staging.module_id,
                stable_id,
                generation,
            }))
        }
        RuntimeValue::HostRequest(_)
        | RuntimeValue::ResourceToken(_)
        | RuntimeValue::Snapshot(_)
        | RuntimeValue::Unit => Err("migration cannot store host capabilities".into()),
    }
}

fn runtime_state_type(value: RuntimeValue) -> nexa_bytecode::ValueType {
    match value {
        RuntimeValue::I32(_) => nexa_bytecode::ValueType::I32,
        RuntimeValue::Bool(_) => nexa_bytecode::ValueType::Bool,
        RuntimeValue::Ref(_) => nexa_bytecode::ValueType::Ref,
        RuntimeValue::NamedRef { type_id, .. } | RuntimeValue::Opaque { type_id, .. } => {
            nexa_bytecode::ValueType::Named(type_id)
        }
        RuntimeValue::HostRequest(_) => {
            nexa_bytecode::ValueType::Named(StableId::from_name("HostRequest"))
        }
        RuntimeValue::ResourceToken(_) => {
            nexa_bytecode::ValueType::Named(StableId::from_name("ResourceToken"))
        }
        RuntimeValue::Snapshot(_) => {
            nexa_bytecode::ValueType::Named(StableId::from_name("Snapshot"))
        }
        RuntimeValue::Unit => nexa_bytecode::ValueType::Named(StableId::from_name("Unit")),
    }
}

fn remap_state_handles(value: &mut StateValue, forwarding: &BTreeMap<StableId, StableId>) {
    match value {
        StateValue::Handle(handle) => {
            if let Some(target) = forwarding.get(&handle.stable_id) {
                handle.stable_id = *target;
            }
        }
        StateValue::Object(object) => {
            for value in object.fields.values_mut() {
                remap_state_handles(value, forwarding);
            }
        }
        StateValue::I32(_) | StateValue::Bool(_) | StateValue::Ref(_) => {}
    }
}

fn collect_state_roots(value: &StateValue, roots: &mut Vec<GcRef>) {
    match value {
        StateValue::Ref(reference) => roots.push(*reference),
        StateValue::Object(object) => {
            for value in object.fields.values() {
                collect_state_roots(value, roots);
            }
        }
        StateValue::I32(_) | StateValue::Bool(_) | StateValue::Handle(_) => {}
    }
}

fn migrate_state_value(
    value: &StateValue,
    module_id: u32,
    schema: &nexa_bytecode::StateSchema,
) -> Result<StateValue, StatefulError> {
    let StateValue::Object(object) = value else {
        return Ok(match value {
            StateValue::Handle(handle) => StateValue::Handle(StateHandle {
                module_id,
                stable_id: handle.stable_id,
                generation: handle.generation,
            }),
            value => value.clone(),
        });
    };
    let state_type = schema
        .types
        .iter()
        .find(|state_type| state_type.stable_id == object.type_id)
        .ok_or(StatefulError::Missing(object.type_id))?;
    let mut fields = BTreeMap::new();
    for field in &state_type.fields {
        let value = if let Some(value) = object.fields.get(&field.stable_id) {
            migrate_state_value(value, module_id, schema)?
        } else {
            default_state_value(field.ty).ok_or(StatefulError::Missing(field.stable_id))?
        };
        fields.insert(field.stable_id, value);
    }
    Ok(StateValue::Object(StateObject {
        type_id: object.type_id,
        version: state_type.version,
        fields,
    }))
}

fn default_state_value(ty: nexa_bytecode::ValueType) -> Option<StateValue> {
    match ty {
        nexa_bytecode::ValueType::I32 => Some(StateValue::I32(0)),
        nexa_bytecode::ValueType::Bool => Some(StateValue::Bool(false)),
        nexa_bytecode::ValueType::Ref | nexa_bytecode::ValueType::Named(_) => None,
    }
}

fn validate_state_handles(
    value: &StateValue,
    registry: &StatefulRegistry,
) -> Result<(), StatefulError> {
    match value {
        StateValue::Handle(handle) => {
            registry.resolve(*handle)?;
        }
        StateValue::Object(object) => {
            for value in object.fields.values() {
                validate_state_handles(value, registry)?;
            }
        }
        StateValue::I32(_) | StateValue::Bool(_) | StateValue::Ref(_) => {}
    }
    Ok(())
}

fn validate_state_value(
    value: &StateValue,
    schema: &nexa_bytecode::StateSchema,
) -> Result<(), StatefulError> {
    let StateValue::Object(object) = value else {
        return Ok(());
    };
    let state_type = schema
        .types
        .iter()
        .find(|state_type| state_type.stable_id == object.type_id)
        .ok_or(StatefulError::Missing(object.type_id))?;
    if state_type.version != object.version {
        return Err(StatefulError::Missing(object.type_id));
    }
    if state_type.fields.len() != object.fields.len() {
        return Err(StatefulError::Missing(object.type_id));
    }
    for field in &state_type.fields {
        let value = object
            .fields
            .get(&field.stable_id)
            .ok_or(StatefulError::Missing(field.stable_id))?;
        let valid = matches!(
            (value, field.ty),
            (StateValue::I32(_), nexa_bytecode::ValueType::I32)
                | (StateValue::Bool(_), nexa_bytecode::ValueType::Bool)
                | (StateValue::Ref(_), nexa_bytecode::ValueType::Ref)
                | (
                    StateValue::Object(_) | StateValue::Handle(_),
                    nexa_bytecode::ValueType::Named(_)
                )
        );
        if !valid {
            return Err(StatefulError::Missing(field.stable_id));
        }
        validate_state_value(value, schema)?;
    }
    Ok(())
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
    pub(crate) paused_tasks: Vec<PausedTask>,
}

#[derive(Clone, Debug)]
pub(crate) struct PausedTask {
    pub(crate) handle: TaskHandle,
    pub(crate) snapshot: TaskSnapshot,
    pub(crate) execution: TaskExecution,
    pub(crate) scheduler: SchedulerCheckpoint,
}

#[derive(Debug, Default)]
pub(crate) struct ReloadCoordinator {
    transaction: Option<ReloadTransaction>,
    state: Option<reload::State>,
    staging_heap: i64,
}

impl ReloadCoordinator {
    pub(crate) fn begin(&mut self, transaction: ReloadTransaction) -> Result<(), ReloadError> {
        if self.transaction.is_some() {
            return Err(ReloadError::InvalidState);
        }
        let preparing = reload::apply(reload::State::Planned, reload::Event::Start, |_| true)
            .map_err(|_| ReloadError::InvalidState)?;
        let quiescing = reload::apply(preparing.state, reload::Event::PrepareSucceeded, |_| true)
            .map_err(|_| ReloadError::InvalidState)?;
        self.staging_heap = apply_reload_deltas(0, preparing.deltas)?;
        self.staging_heap = apply_reload_deltas(self.staging_heap, quiescing.deltas)?;
        self.state = Some(quiescing.state);
        self.transaction = Some(transaction);
        Ok(())
    }

    pub(crate) fn quiesced(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::QuiesceSucceeded)
    }

    pub(crate) fn staged(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::StageSucceeded)
    }

    pub(crate) fn publish(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::Publish)
    }

    pub(crate) fn begin_activation(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::BeginActivation)
    }

    pub(crate) fn activation_succeeded(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::ActivationSucceeded)
    }

    pub(crate) fn activation_failed(&mut self) -> Result<(), ReloadError> {
        self.apply(reload::Event::ActivationFailed)
    }

    pub(crate) fn rollback(&mut self) -> Result<(), ReloadError> {
        match self.state.ok_or(ReloadError::InvalidState)? {
            reload::State::Quiescing => self.apply(reload::Event::QuiesceFailed),
            reload::State::Staging | reload::State::Committing => {
                self.apply(reload::Event::StageFailed)
            }
            _ => Err(ReloadError::InvalidState),
        }
    }

    pub(crate) fn transaction(&self) -> Result<&ReloadTransaction, ReloadError> {
        self.transaction.as_ref().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn transaction_mut(&mut self) -> Result<&mut ReloadTransaction, ReloadError> {
        self.transaction.as_mut().ok_or(ReloadError::InvalidState)
    }

    pub(crate) fn finish(&mut self) -> Result<ReloadTransaction, ReloadError> {
        let transaction = self.transaction.take().ok_or(ReloadError::InvalidState)?;
        self.state = None;
        self.staging_heap = 0;
        Ok(transaction)
    }

    #[must_use]
    pub(crate) fn active(&self) -> bool {
        self.transaction.is_some()
    }

    fn apply(&mut self, event: reload::Event) -> Result<(), ReloadError> {
        let outcome = reload::apply(self.state.ok_or(ReloadError::InvalidState)?, event, |_| {
            true
        })
        .map_err(|_| ReloadError::InvalidState)?;
        self.staging_heap = apply_reload_deltas(self.staging_heap, outcome.deltas)?;
        reload::check_invariants(outcome.state, |_| self.staging_heap)
            .map_err(|_| ReloadError::GraphCheck)?;
        self.state = Some(outcome.state);
        Ok(())
    }
}

fn apply_reload_deltas(
    mut staging_heap: i64,
    deltas: &[reload::ResourceDelta],
) -> Result<i64, ReloadError> {
    for delta in deltas {
        if delta.resource != "staging_heap" {
            return Err(ReloadError::GraphCheck);
        }
        staging_heap = staging_heap
            .checked_add(delta.amount)
            .ok_or(ReloadError::GraphCheck)?;
    }
    Ok(staging_heap)
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
