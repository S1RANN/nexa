use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use nexa_core::StableId;

use crate::interpreter::InterpreterMigration;
use crate::machines::reload;
use crate::scheduler::SchedulerCheckpoint;
use crate::task::TaskExecution;
use crate::{GcRef, HostCompletionDelivery, ModuleHandle, RuntimeValue, TaskHandle, TaskSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatefulDomainId(u64);

impl StatefulDomainId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateHandle {
    pub domain: StatefulDomainId,
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
    domain: StatefulDomainId,
    entries: BTreeMap<StableId, StateEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatefulError {
    WrongDomain {
        expected: StatefulDomainId,
        actual: StatefulDomainId,
    },
    Missing(StableId),
    StaleGeneration,
    GenerationExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationLimits {
    pub max_objects: u32,
    pub max_fields: u32,
    pub max_forwarding_entries: u32,
    pub max_state_bytes: usize,
    pub max_gc_roots: u32,
    pub max_fuel: u64,
    pub max_call_depth: u16,
}

impl Default for MigrationLimits {
    fn default() -> Self {
        Self {
            max_objects: 4_096,
            max_fields: 16_384,
            max_forwarding_entries: 4_096,
            max_state_bytes: 16 * 1024 * 1024,
            max_gc_roots: 4_096,
            max_fuel: 4_096,
            max_call_depth: 128,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationLimitError {
    Objects,
    Fields,
    Forwarding,
    StateBytes,
    GcRoots,
    Fuel,
    CallDepth,
}

impl fmt::Display for StatefulError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StatefulError {}

impl StatefulRegistry {
    #[must_use]
    pub const fn new(domain: StatefulDomainId) -> Self {
        Self {
            domain,
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
            domain: self.domain,
            stable_id,
            generation,
        })
    }

    pub fn resolve(&self, handle: StateHandle) -> Result<&StateValue, StatefulError> {
        if handle.domain != self.domain {
            return Err(StatefulError::WrongDomain {
                expected: self.domain,
                actual: handle.domain,
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
                domain: self.domain,
                stable_id: *stable_id,
                generation: entry.generation,
            })
            .collect()
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MigrationUsage {
    objects: u64,
    fields: u64,
    state_bytes: usize,
    gc_roots: u64,
}

fn registry_usage(registry: &StatefulRegistry) -> MigrationUsage {
    let mut usage = MigrationUsage::default();
    for entry in registry.entries.values() {
        usage.objects += 1;
        usage.state_bytes = usage
            .state_bytes
            .saturating_add(std::mem::size_of::<StableId>() + std::mem::size_of::<u32>());
        add_state_value_usage(&entry.value, &mut usage);
    }
    usage
}

fn add_state_value_usage(value: &StateValue, usage: &mut MigrationUsage) {
    match value {
        StateValue::I32(_) => {
            usage.state_bytes = usage.state_bytes.saturating_add(std::mem::size_of::<i32>());
        }
        StateValue::Bool(_) => {
            usage.state_bytes = usage.state_bytes.saturating_add(1);
        }
        StateValue::Ref(_) => {
            usage.state_bytes = usage
                .state_bytes
                .saturating_add(std::mem::size_of::<GcRef>());
            usage.gc_roots += 1;
        }
        StateValue::Handle(_) => {
            usage.state_bytes = usage
                .state_bytes
                .saturating_add(std::mem::size_of::<StateHandle>());
        }
        StateValue::Object(object) => {
            usage.state_bytes = usage
                .state_bytes
                .saturating_add(std::mem::size_of::<StableId>() + std::mem::size_of::<u32>());
            for value in object.fields.values() {
                usage.fields += 1;
                usage.state_bytes = usage
                    .state_bytes
                    .saturating_add(std::mem::size_of::<StableId>());
                add_state_value_usage(value, usage);
            }
        }
    }
}

pub(crate) struct MigrationContext {
    old: StatefulRegistry,
    staging: StatefulRegistry,
    schema: nexa_bytecode::StateSchema,
    forwarding: BTreeMap<StableId, StableId>,
    decisions: BTreeMap<StableId, Option<StableId>>,
    schema_unchanged: bool,
    touched: bool,
    finalized: bool,
    limits: MigrationLimits,
    invalid: Option<ReloadError>,
}

impl MigrationContext {
    #[must_use]
    pub(crate) fn new(
        old: StatefulRegistry,
        domain: StatefulDomainId,
        schema: nexa_bytecode::StateSchema,
        schema_unchanged: bool,
        limits: MigrationLimits,
    ) -> Self {
        Self {
            old,
            staging: StatefulRegistry::new(domain),
            schema,
            forwarding: BTreeMap::new(),
            decisions: BTreeMap::new(),
            schema_unchanged,
            touched: false,
            finalized: false,
            limits,
            invalid: None,
        }
    }

    pub(crate) fn limit_error(&self) -> Option<MigrationLimitError> {
        match self.invalid {
            Some(ReloadError::MigrationLimit(error)) => Some(error),
            _ => None,
        }
    }

    fn reject_limit<T>(&mut self, error: MigrationLimitError) -> Result<T, String> {
        self.invalid = Some(ReloadError::MigrationLimit(error));
        Err(format!("migration limit exceeded: {error:?}"))
    }

    fn ensure_usage(&mut self, usage: MigrationUsage) -> Result<(), String> {
        if usage.objects > u64::from(self.limits.max_objects) {
            return self.reject_limit(MigrationLimitError::Objects);
        }
        if usage.fields > u64::from(self.limits.max_fields) {
            return self.reject_limit(MigrationLimitError::Fields);
        }
        if usage.state_bytes > self.limits.max_state_bytes {
            return self.reject_limit(MigrationLimitError::StateBytes);
        }
        if usage.gc_roots > u64::from(self.limits.max_gc_roots) {
            return self.reject_limit(MigrationLimitError::GcRoots);
        }
        Ok(())
    }

    fn ensure_forwarding_capacity(&mut self) -> Result<(), String> {
        if self.decisions.len() >= self.limits.max_forwarding_entries as usize {
            return self.reject_limit(MigrationLimitError::Forwarding);
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<StatefulRegistry, ReloadError> {
        if !self.touched {
            if self.schema_unchanged {
                self.old
                    .validate_schema(&self.schema)
                    .map_err(|_| ReloadError::InvalidStateHandle)?;
                self.old
                    .validate_handles()
                    .map_err(|_| ReloadError::InvalidStateHandle)?;
                return Ok(self.old);
            }
            return Err(ReloadError::MigrationNoOutput);
        }
        if let Some(error) = self.invalid {
            return Err(error);
        }
        if !self.finalized {
            return Err(ReloadError::MigrationNotFinished);
        }
        if self
            .old
            .entries
            .keys()
            .any(|stable_id| !self.decisions.contains_key(stable_id))
        {
            return Err(ReloadError::MissingForwarding);
        }
        let generations = self
            .staging
            .entries
            .iter()
            .map(|(stable_id, entry)| (*stable_id, entry.generation))
            .collect::<BTreeMap<_, _>>();
        for entry in self.staging.entries.values_mut() {
            remap_state_handles(&mut entry.value, &self.forwarding, &generations);
        }
        self.staging
            .validate_schema(&self.schema)
            .map_err(|_| ReloadError::GraphCheck)?;
        self.staging
            .validate_handles()
            .map_err(|_| ReloadError::InvalidStateHandle)?;
        Ok(self.staging)
    }
}

impl InterpreterMigration for MigrationContext {
    fn old_get(
        &mut self,
        stable_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
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

    fn old_field_get(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        let RuntimeValue::Opaque {
            type_id,
            value: stable_id,
        } = object
        else {
            return Err("STATE_OLD_FIELD_GET requires an old state object".into());
        };
        let entry = self
            .old
            .entries
            .get(&StableId(stable_id))
            .ok_or_else(|| "old state object does not exist".to_string())?;
        let StateValue::Object(object) = &entry.value else {
            return Err("old state value is not an object".into());
        };
        if object.type_id != type_id {
            return Err("old state object type mismatch".into());
        }
        let field = object
            .fields
            .get(&field_id)
            .ok_or_else(|| "old state field does not exist".to_string())?;
        let value = state_to_runtime_value(field_id, field);
        if runtime_state_type(value) != expected {
            return Err("old state field type mismatch".into());
        }
        Ok(value)
    }

    fn new_create(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        let version = self
            .schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == type_id)
            .map(|state_type| state_type.version)
            .ok_or_else(|| format!("candidate state type {type_id:?} does not exist"))?;
        if self.staging.entries.contains_key(&stable_id) {
            return Err("new state object already exists".into());
        }
        let mut projected = self.staging.clone();
        projected.entries.insert(
            stable_id,
            StateEntry {
                generation: 0,
                value: StateValue::Object(StateObject {
                    type_id,
                    version,
                    fields: BTreeMap::new(),
                }),
            },
        );
        self.ensure_usage(registry_usage(&projected))?;
        self.staging = projected;
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
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        let RuntimeValue::Opaque {
            type_id,
            value: object_id,
        } = object
        else {
            return Err("STATE_NEW_SET requires a staging object".into());
        };
        let value = runtime_to_state_value(value, &self.staging)?;
        let mut projected = self.staging.clone();
        let entry = projected
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
        self.ensure_usage(registry_usage(&projected))?;
        self.staging = projected;
        self.touched = true;
        Ok(())
    }

    fn preserve(&mut self, stable_id: StableId) -> Result<(), String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        if self.decisions.contains_key(&stable_id) {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        }
        self.ensure_forwarding_capacity()?;
        let entry = self
            .old
            .entries
            .get(&stable_id)
            .ok_or_else(|| "STATE_PRESERVE source does not exist".to_string())?
            .clone();
        let mut projected = self.staging.clone();
        if projected.entries.insert(stable_id, entry).is_some() {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        }
        self.ensure_usage(registry_usage(&projected))?;
        self.staging = projected;
        self.forwarding.insert(stable_id, stable_id);
        self.decisions.insert(stable_id, Some(stable_id));
        self.touched = true;
        Ok(())
    }

    fn replace(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        let RuntimeValue::Opaque {
            value: target_id, ..
        } = target
        else {
            return Err("STATE_REPLACE requires a staging object".into());
        };
        let target_id = StableId(target_id);
        if !self.staging.entries.contains_key(&target_id) {
            return Err("remap target does not exist".into());
        }
        if self.decisions.contains_key(&old_id) {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        }
        self.ensure_forwarding_capacity()?;
        let old_generation = self
            .old
            .entries
            .get(&old_id)
            .ok_or_else(|| "STATE_REPLACE source does not exist".to_string())?
            .generation;
        let replacement_generation = old_generation
            .checked_add(1)
            .ok_or_else(|| "replacement generation exhausted".to_string())?;
        let mut projected = self.staging.clone();
        projected
            .entries
            .get_mut(&target_id)
            .expect("target existence checked")
            .generation = replacement_generation;
        self.ensure_usage(registry_usage(&projected))?;
        self.staging = projected;
        self.forwarding.insert(old_id, target_id);
        self.decisions.insert(old_id, Some(target_id));
        self.touched = true;
        Ok(())
    }

    fn delete(&mut self, stable_id: StableId) -> Result<(), String> {
        if self.finalized {
            return Err("STATE_FINISH already executed".into());
        }
        if !self.old.entries.contains_key(&stable_id) {
            return Err("STATE_DELETE source does not exist".into());
        }
        if self.decisions.contains_key(&stable_id) {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        }
        self.ensure_forwarding_capacity()?;
        let mut projected = self.staging.clone();
        projected.entries.remove(&stable_id);
        self.ensure_usage(registry_usage(&projected))?;
        self.staging = projected;
        self.decisions.insert(stable_id, None);
        self.touched = true;
        Ok(())
    }

    fn finish_staging(&mut self) -> Result<(), String> {
        if self.finalized {
            return Err("STATE_FINISH may execute only once".into());
        }
        self.finalized = true;
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
                domain: staging.domain,
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

fn remap_state_handles(
    value: &mut StateValue,
    forwarding: &BTreeMap<StableId, StableId>,
    generations: &BTreeMap<StableId, u32>,
) {
    match value {
        StateValue::Handle(handle) => {
            if let Some(target) = forwarding.get(&handle.stable_id) {
                handle.stable_id = *target;
                if let Some(generation) = generations.get(target) {
                    handle.generation = *generation;
                }
            }
        }
        StateValue::Object(object) => {
            for value in object.fields.values_mut() {
                remap_state_handles(value, forwarding, generations);
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
    MigrationNoOutput,
    MigrationNotFinished,
    MissingForwarding,
    DuplicateForwarding,
    InvalidStateHandle,
    MigrationLimit(MigrationLimitError),
    CompletionBufferCapacity,
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
    pub(crate) completions: ReloadCompletionBuffer,
}

#[derive(Debug)]
pub(crate) struct ReloadCompletionBuffer {
    entries: VecDeque<HostCompletionDelivery>,
    capacity: usize,
}

impl ReloadCompletionBuffer {
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub(crate) fn push(&mut self, delivery: HostCompletionDelivery) -> Result<(), ReloadError> {
        if self.entries.len() == self.capacity {
            return Err(ReloadError::CompletionBufferCapacity);
        }
        self.entries.push_back(delivery);
        Ok(())
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn drain(&mut self) -> impl Iterator<Item = HostCompletionDelivery> + '_ {
        self.entries.drain(..)
    }
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
    use nexa_bytecode::{StateField, StateSchema, StateType, ValueType};
    use nexa_core::StableId;

    use crate::GcRef;
    use crate::interpreter::InterpreterMigration;

    use super::{
        MigrationContext, MigrationLimitError, MigrationLimits, StateValue, StatefulDomainId,
        StatefulError, StatefulRegistry,
    };

    #[test]
    fn state_handles_are_generation_and_domain_checked() {
        let mut registry = StatefulRegistry::new(StatefulDomainId::new(1));
        let id = StableId::from_name("score");
        let old = registry.insert(id, StateValue::I32(1)).unwrap();
        let new = registry.insert(id, StateValue::I32(2)).unwrap();
        assert_ne!(old, new);
        assert_eq!(registry.resolve(old), Err(StatefulError::StaleGeneration));
        assert_eq!(registry.resolve(new), Ok(&StateValue::I32(2)));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn migration_context_rejects_capacity_before_staging_mutation() {
        let type_id = StableId::from_name("LimitedState");
        let object_id = StableId::from_name("LimitedState::one");
        let field_id = StableId::from_name("LimitedState::value");
        let schema = StateSchema {
            types: vec![StateType {
                stable_id: type_id,
                version: 1,
                fields: vec![StateField {
                    stable_id: field_id,
                    ty: ValueType::Ref,
                }],
            }],
        };

        let mut objects = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            schema.clone(),
            false,
            MigrationLimits {
                max_objects: 0,
                ..MigrationLimits::default()
            },
        );
        assert!(objects.new_create(object_id, type_id).is_err());
        assert_eq!(objects.limit_error(), Some(MigrationLimitError::Objects));
        assert!(objects.staging.entries.is_empty());

        let mut fields = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            schema.clone(),
            false,
            MigrationLimits {
                max_fields: 0,
                ..MigrationLimits::default()
            },
        );
        let object = fields.new_create(object_id, type_id).unwrap();
        assert!(
            fields
                .new_set(
                    object,
                    field_id,
                    crate::RuntimeValue::Ref(GcRef {
                        index: 0,
                        generation: 0,
                    })
                )
                .is_err()
        );
        assert_eq!(fields.limit_error(), Some(MigrationLimitError::Fields));
        assert!(matches!(
            fields.staging.entries[&object_id].value,
            StateValue::Object(ref object) if object.fields.is_empty()
        ));

        let mut bytes = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            schema.clone(),
            false,
            MigrationLimits {
                max_state_bytes: 0,
                ..MigrationLimits::default()
            },
        );
        assert!(bytes.new_create(object_id, type_id).is_err());
        assert_eq!(bytes.limit_error(), Some(MigrationLimitError::StateBytes));

        let mut roots = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            schema,
            false,
            MigrationLimits {
                max_gc_roots: 0,
                ..MigrationLimits::default()
            },
        );
        let object = roots.new_create(object_id, type_id).unwrap();
        assert!(
            roots
                .new_set(
                    object,
                    field_id,
                    crate::RuntimeValue::Ref(GcRef {
                        index: 0,
                        generation: 0,
                    })
                )
                .is_err()
        );
        assert_eq!(roots.limit_error(), Some(MigrationLimitError::GcRoots));

        let old_id = StableId::from_name("old");
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        old.insert(old_id, StateValue::I32(1)).unwrap();
        let mut forwarding = MigrationContext::new(
            old,
            StatefulDomainId::new(1),
            StateSchema { types: Vec::new() },
            false,
            MigrationLimits {
                max_forwarding_entries: 0,
                ..MigrationLimits::default()
            },
        );
        assert!(forwarding.preserve(old_id).is_err());
        assert_eq!(
            forwarding.limit_error(),
            Some(MigrationLimitError::Forwarding)
        );
        assert!(forwarding.staging.entries.is_empty());
    }
}
