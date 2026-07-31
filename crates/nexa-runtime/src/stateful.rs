use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexa_bytecode::ValueType;
use nexa_core::StableId;

use crate::allocation::{AllocationBoundary, MigrationAllocationPhase, observe_migration};
use crate::interpreter::InterpreterMigration;
use crate::reload::ReloadError;
use crate::{
    GcRef, MigrationOldObjectHandle, MigrationStagingObjectHandle, RuntimeMessage, RuntimeValue,
};

static NEXT_MIGRATION_CONTEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_migration_context_token() -> u64 {
    loop {
        let token = NEXT_MIGRATION_CONTEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

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

impl StateHandle {
    #[must_use]
    pub const fn stable_id(self) -> StableId {
        self.stable_id
    }

    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    #[must_use]
    pub fn deterministic_hash(self) -> u64 {
        let mut hash = DeterministicMigrationHasher::new();
        hash.write_u64(self.domain.get());
        hash.write_u64(self.stable_id.0);
        hash.write_u32(self.generation);
        hash.value
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateValue {
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
        type_id: StableId,
        hash: u64,
    },
    Ref {
        reference: GcRef,
        type_id: Option<StableId>,
    },
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
struct MigrationObjectSlot {
    stable_id: StableId,
    type_id: StableId,
    version: u32,
    generation: u32,
    field_start: u32,
    field_len: u32,
    scalar: Option<StateValue>,
}

#[derive(Clone, Debug)]
struct MigrationFieldSlot {
    field_id: StableId,
    value: StateValue,
}

#[derive(Clone, Copy, Debug)]
struct ForwardingSlot {
    old_id: StableId,
    target: Option<StableId>,
}

#[derive(Debug)]
pub struct StatefulRegistry {
    domain: StatefulDomainId,
    objects: Vec<MigrationObjectSlot>,
    fields: Vec<MigrationFieldSlot>,
    payload: Vec<u8>,
    gc_roots: Vec<GcRef>,
    object_capacity: usize,
    field_capacity: usize,
    byte_capacity: usize,
    gc_root_capacity: usize,
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
    NestedObject,
    Capacity(MigrationLimitError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateHandleError {
    WrongDomain,
    Missing,
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
    Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationCapacityReport {
    pub object_capacity: usize,
    pub field_capacity: usize,
    pub forwarding_capacity: usize,
    pub payload_byte_capacity: usize,
    pub metadata_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationUsageReport {
    pub objects_read: usize,
    pub objects_created: usize,
    pub fields_written: usize,
    pub preserved: usize,
    pub replaced: usize,
    pub deleted: usize,
    pub generation_changes: usize,
    pub handle_remaps: usize,
    pub object_peak: usize,
    pub field_peak: usize,
    pub forwarding_peak: usize,
    pub payload_byte_peak: usize,
    pub gc_root_peak: usize,
    pub fuel_used: u64,
    pub max_call_depth_used: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineStateObject {
    pub stable_id: StableId,
    pub type_id: StableId,
    pub generation: u32,
    pub fields: Vec<OfflineStateField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineStateField {
    pub stable_id: StableId,
    pub value: OfflineStateValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfflineStateValue {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    Bool(bool),
    Rune(u32),
    String(String),
    Handle(StateHandle),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineMigrationResult {
    pub objects: Vec<OfflineStateObject>,
    pub migration_hash: StableId,
    pub final_state_hash: StableId,
    pub usage: MigrationUsageReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfflineMigrationError {
    MissingMigrationEntry,
    DuplicateObject(StableId),
    DuplicateField { object: StableId, field: StableId },
    UnknownType { object: StableId, type_id: StableId },
    Capacity(MigrationLimitError),
    InvalidOldState,
    InvalidHandle,
    Interpreter(String),
    Migration(ReloadError),
    UnsupportedOutputValue,
}

impl fmt::Display for OfflineMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for OfflineMigrationError {}

impl MigrationLimits {
    #[must_use]
    pub fn capacity_report(self) -> MigrationCapacityReport {
        let object_capacity = self.max_objects as usize;
        let field_capacity = self.max_fields as usize;
        let forwarding_capacity = self.max_forwarding_entries as usize;
        MigrationCapacityReport {
            object_capacity,
            field_capacity,
            forwarding_capacity,
            payload_byte_capacity: self.max_state_bytes,
            metadata_bytes: object_capacity
                .saturating_mul(std::mem::size_of::<MigrationObjectSlot>())
                .saturating_add(
                    field_capacity.saturating_mul(std::mem::size_of::<MigrationFieldSlot>()),
                )
                .saturating_add(
                    forwarding_capacity.saturating_mul(std::mem::size_of::<ForwardingSlot>()),
                ),
        }
    }

    pub fn validate_requirements(
        self,
        required: nexa_bytecode::MigrationLimitRequirements,
    ) -> Result<(), MigrationLimitError> {
        if self.max_objects < required.max_objects {
            Err(MigrationLimitError::Objects)
        } else if self.max_fields < required.max_fields {
            Err(MigrationLimitError::Fields)
        } else if self.max_forwarding_entries < required.max_forwarding_entries {
            Err(MigrationLimitError::Forwarding)
        } else if u64::try_from(self.max_state_bytes).unwrap_or(u64::MAX) < required.max_state_bytes
        {
            Err(MigrationLimitError::StateBytes)
        } else if self.max_gc_roots < required.max_gc_roots {
            Err(MigrationLimitError::GcRoots)
        } else if self.max_fuel < required.max_fuel {
            Err(MigrationLimitError::Fuel)
        } else if self.max_call_depth < required.max_call_depth {
            Err(MigrationLimitError::CallDepth)
        } else {
            Ok(())
        }
    }
}

impl fmt::Display for StatefulError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StatefulError {}

impl fmt::Display for StateHandleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StateHandleError {}

impl Clone for StatefulRegistry {
    fn clone(&self) -> Self {
        let mut objects = Vec::with_capacity(self.object_capacity);
        objects.extend_from_slice(&self.objects);
        let mut fields = Vec::with_capacity(self.field_capacity);
        fields.extend_from_slice(&self.fields);
        let mut payload = Vec::with_capacity(self.byte_capacity);
        payload.extend_from_slice(&self.payload);
        let mut gc_roots = Vec::with_capacity(self.gc_root_capacity);
        gc_roots.extend_from_slice(&self.gc_roots);
        Self {
            domain: self.domain,
            objects,
            fields,
            payload,
            gc_roots,
            object_capacity: self.object_capacity,
            field_capacity: self.field_capacity,
            byte_capacity: self.byte_capacity,
            gc_root_capacity: self.gc_root_capacity,
        }
    }
}

impl StatefulRegistry {
    #[must_use]
    pub fn new(domain: StatefulDomainId) -> Self {
        Self::try_new(domain, MigrationLimits::default())
            .expect("default stateful registry capacity can be reserved")
    }

    pub(crate) fn try_new(
        domain: StatefulDomainId,
        limits: MigrationLimits,
    ) -> Result<Self, MigrationLimitError> {
        let report = limits.capacity_report();
        Ok(Self {
            domain,
            objects: reserve(report.object_capacity, MigrationLimitError::Objects)?,
            fields: reserve(report.field_capacity, MigrationLimitError::Fields)?,
            payload: reserve(
                report.payload_byte_capacity,
                MigrationLimitError::StateBytes,
            )?,
            gc_roots: reserve(limits.max_gc_roots as usize, MigrationLimitError::GcRoots)?,
            object_capacity: report.object_capacity,
            field_capacity: report.field_capacity,
            byte_capacity: report.payload_byte_capacity,
            gc_root_capacity: limits.max_gc_roots as usize,
        })
    }

    #[must_use]
    pub(crate) fn object_count(&self) -> usize {
        self.objects.len()
    }

    pub fn insert(
        &mut self,
        stable_id: StableId,
        value: StateValue,
    ) -> Result<StateHandle, StatefulError> {
        reject_nested_object(&value)?;
        let search = self
            .objects
            .binary_search_by_key(&stable_id, |slot| slot.stable_id);
        let removed_usage = search
            .ok()
            .map(|index| self.slot_usage(&self.objects[index]))
            .unwrap_or_default();
        let inserted_usage = value_usage(&value);
        let usage = registry_usage(&self.objects, &self.fields);
        self.check_usage(MigrationUsage {
            objects: usage.objects + usize::from(search.is_err()),
            fields: usage
                .fields
                .saturating_sub(removed_usage.fields)
                .saturating_add(inserted_usage.fields),
            payload_bytes: usage
                .payload_bytes
                .saturating_sub(removed_usage.payload_bytes)
                .saturating_add(inserted_usage.payload_bytes),
            gc_roots: usage
                .gc_roots
                .saturating_sub(removed_usage.gc_roots)
                .saturating_add(inserted_usage.gc_roots),
        })?;
        let (index, generation) = match search {
            Ok(index) => {
                let generation = self.objects[index]
                    .generation
                    .checked_add(1)
                    .ok_or(StatefulError::GenerationExhausted)?;
                self.remove_object(index);
                (index, generation)
            }
            Err(index) => (index, 0),
        };
        self.insert_value(index, stable_id, generation, value);
        self.rebuild_caches();
        Ok(StateHandle {
            domain: self.domain,
            stable_id,
            generation,
        })
    }

    pub fn resolve(&self, handle: StateHandle) -> Result<StateValue, StatefulError> {
        if handle.domain != self.domain {
            return Err(StatefulError::WrongDomain {
                expected: self.domain,
                actual: handle.domain,
            });
        }
        let slot = self.object(handle.stable_id)?;
        if slot.generation != handle.generation {
            return Err(StatefulError::StaleGeneration);
        }
        Ok(self.materialize(slot))
    }

    pub(crate) fn runtime_handle(
        &self,
        handle: StateHandle,
    ) -> Result<RuntimeValue, StatefulError> {
        let slot = self.checked_handle_slot(handle)?;
        let target = slot
            .scalar
            .as_ref()
            .map_or(ValueType::Named(slot.type_id), state_value_type);
        Ok(RuntimeValue::StateHandle {
            handle_type: nexa_bytecode::state_handle_type(target),
            domain: handle.domain.get(),
            stable_id: handle.stable_id,
            generation: handle.generation,
        })
    }

    pub(crate) fn resolve_runtime_handle(
        &self,
        handle: StateHandle,
        target: ValueType,
    ) -> Result<RuntimeValue, StateHandleError> {
        let slot = self.checked_runtime_handle_slot(handle)?;
        let actual = slot
            .scalar
            .as_ref()
            .map_or(ValueType::Named(slot.type_id), state_value_type);
        if actual != target {
            return Err(StateHandleError::Missing);
        }
        Ok(slot.scalar.as_ref().map_or(
            RuntimeValue::Opaque {
                type_id: slot.type_id,
                value: slot.stable_id.0,
            },
            |value| state_to_runtime_value(slot.stable_id, value),
        ))
    }

    pub(crate) fn is_handle_alive(&self, handle: StateHandle) -> bool {
        if handle.domain != self.domain {
            return false;
        }
        match self
            .objects
            .binary_search_by_key(&handle.stable_id, |slot| slot.stable_id)
        {
            Ok(index) => self.objects[index].generation == handle.generation,
            Err(_) => false,
        }
    }

    #[must_use]
    pub fn handles(&self) -> Vec<StateHandle> {
        self.objects
            .iter()
            .map(|slot| StateHandle {
                domain: self.domain,
                stable_id: slot.stable_id,
                generation: slot.generation,
            })
            .collect()
    }

    pub fn validate_schema(
        &self,
        schema: &nexa_bytecode::StateSchema,
    ) -> Result<(), StatefulError> {
        for slot in &self.objects {
            self.validate_slot_schema(slot, schema)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn gc_roots(&self) -> Vec<GcRef> {
        self.gc_roots.clone()
    }

    fn validate_handles(&self) -> Result<(), StatefulError> {
        for slot in &self.objects {
            if let Some(value) = &slot.scalar {
                self.validate_value_handle(value)?;
            } else {
                for field in self.object_fields(slot) {
                    self.validate_value_handle(&field.value)?;
                }
            }
        }
        Ok(())
    }

    fn object(&self, stable_id: StableId) -> Result<&MigrationObjectSlot, StatefulError> {
        self.objects
            .binary_search_by_key(&stable_id, |slot| slot.stable_id)
            .map(|index| &self.objects[index])
            .map_err(|_| StatefulError::Missing(stable_id))
    }

    fn checked_handle_slot(
        &self,
        handle: StateHandle,
    ) -> Result<&MigrationObjectSlot, StatefulError> {
        if handle.domain != self.domain {
            return Err(StatefulError::WrongDomain {
                expected: self.domain,
                actual: handle.domain,
            });
        }
        let slot = self.object(handle.stable_id)?;
        if slot.generation != handle.generation {
            return Err(StatefulError::StaleGeneration);
        }
        Ok(slot)
    }

    fn checked_runtime_handle_slot(
        &self,
        handle: StateHandle,
    ) -> Result<&MigrationObjectSlot, StateHandleError> {
        if handle.domain != self.domain {
            return Err(StateHandleError::WrongDomain);
        }
        let slot = self
            .object(handle.stable_id)
            .map_err(|_| StateHandleError::Missing)?;
        if slot.generation != handle.generation {
            return Err(StateHandleError::StaleGeneration);
        }
        Ok(slot)
    }

    fn object_fields(&self, slot: &MigrationObjectSlot) -> &[MigrationFieldSlot] {
        let start = slot.field_start as usize;
        &self.fields[start..start + slot.field_len as usize]
    }

    fn materialize(&self, slot: &MigrationObjectSlot) -> StateValue {
        if let Some(value) = &slot.scalar {
            return value.clone();
        }
        let fields = self
            .object_fields(slot)
            .iter()
            .map(|field| (field.field_id, field.value.clone()))
            .collect();
        StateValue::Object(StateObject {
            type_id: slot.type_id,
            version: slot.version,
            fields,
        })
    }

    fn validate_slot_schema(
        &self,
        slot: &MigrationObjectSlot,
        schema: &nexa_bytecode::StateSchema,
    ) -> Result<(), StatefulError> {
        if slot.scalar.is_some() {
            return Ok(());
        }
        let state_type = schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == slot.type_id)
            .ok_or(StatefulError::Missing(slot.type_id))?;
        let fields = self.object_fields(slot);
        if state_type.version != slot.version || state_type.fields.len() != fields.len() {
            return Err(StatefulError::Missing(slot.type_id));
        }
        for schema_field in &state_type.fields {
            let field = fields
                .binary_search_by_key(&schema_field.stable_id, |field| field.field_id)
                .map(|index| &fields[index])
                .map_err(|_| StatefulError::Missing(schema_field.stable_id))?;
            let handle_target = match &field.value {
                StateValue::Handle(handle) => self
                    .checked_handle_slot(*handle)
                    .ok()
                    .map(|target| ValueType::Named(target.type_id)),
                _ => None,
            };
            if !state_value_matches(&field.value, schema_field.ty, handle_target) {
                return Err(StatefulError::Missing(schema_field.stable_id));
            }
        }
        Ok(())
    }

    fn validate_value_handle(&self, value: &StateValue) -> Result<(), StatefulError> {
        if let StateValue::Handle(handle) = value {
            if handle.domain != self.domain {
                return Err(StatefulError::WrongDomain {
                    expected: self.domain,
                    actual: handle.domain,
                });
            }
            let target = self.object(handle.stable_id)?;
            if target.generation != handle.generation {
                return Err(StatefulError::StaleGeneration);
            }
        }
        Ok(())
    }

    fn remove_object(&mut self, index: usize) {
        let removed = self.objects.remove(index);
        let start = removed.field_start as usize;
        let len = removed.field_len as usize;
        self.fields.drain(start..start + len);
        for slot in &mut self.objects[index..] {
            slot.field_start -= removed.field_len;
        }
    }

    fn insert_value(
        &mut self,
        index: usize,
        stable_id: StableId,
        generation: u32,
        value: StateValue,
    ) {
        let field_start = self
            .objects
            .get(index)
            .map_or(self.fields.len(), |slot| slot.field_start as usize);
        let (type_id, version, scalar, field_len) = match value {
            StateValue::Object(object) => {
                let field_len =
                    u32::try_from(object.fields.len()).expect("state field count fits u32");
                for (offset, (field_id, value)) in object.fields.into_iter().enumerate() {
                    self.fields
                        .insert(field_start + offset, MigrationFieldSlot { field_id, value });
                }
                (object.type_id, object.version, None, field_len)
            }
            scalar => (StableId(0), 0, Some(scalar), 0),
        };
        for slot in &mut self.objects[index..] {
            slot.field_start += field_len;
        }
        self.objects.insert(
            index,
            MigrationObjectSlot {
                stable_id,
                type_id,
                version,
                generation,
                field_start: u32::try_from(field_start).expect("state field offset fits u32"),
                field_len,
                scalar,
            },
        );
    }

    fn rebuild_caches(&mut self) {
        let usage = registry_usage(&self.objects, &self.fields);
        self.payload.resize(usage.payload_bytes, 0);
        self.gc_roots.clear();
        for slot in &self.objects {
            if let Some(value) = &slot.scalar {
                push_root(value, &mut self.gc_roots);
            } else {
                let start = slot.field_start as usize;
                let end = start + slot.field_len as usize;
                for field in &self.fields[start..end] {
                    push_root(&field.value, &mut self.gc_roots);
                }
            }
        }
    }

    fn slot_usage(&self, slot: &MigrationObjectSlot) -> MigrationUsage {
        let fields = self.object_fields(slot);
        MigrationUsage {
            objects: 1,
            fields: fields.len(),
            payload_bytes: object_payload_bytes(slot).saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        std::mem::size_of::<StableId>()
                            .saturating_add(state_value_payload_bytes(&field.value))
                    })
                    .sum(),
            ),
            gc_roots: slot
                .scalar
                .as_ref()
                .map_or(0, state_value_root_count)
                .saturating_add(
                    fields
                        .iter()
                        .map(|field| state_value_root_count(&field.value))
                        .sum(),
                ),
        }
    }

    fn check_usage(&self, usage: MigrationUsage) -> Result<(), StatefulError> {
        if usage.objects > self.object_capacity {
            return Err(StatefulError::Capacity(MigrationLimitError::Objects));
        }
        if usage.fields > self.field_capacity {
            return Err(StatefulError::Capacity(MigrationLimitError::Fields));
        }
        if usage.payload_bytes > self.byte_capacity {
            return Err(StatefulError::Capacity(MigrationLimitError::StateBytes));
        }
        if usage.gc_roots > self.gc_root_capacity {
            return Err(StatefulError::Capacity(MigrationLimitError::GcRoots));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MigrationUsage {
    objects: usize,
    fields: usize,
    payload_bytes: usize,
    gc_roots: usize,
}

fn registry_usage(
    objects: &[MigrationObjectSlot],
    fields: &[MigrationFieldSlot],
) -> MigrationUsage {
    MigrationUsage {
        objects: objects.len(),
        fields: fields.len(),
        payload_bytes: objects
            .iter()
            .map(object_payload_bytes)
            .sum::<usize>()
            .saturating_add(
                fields
                    .iter()
                    .map(|field| {
                        std::mem::size_of::<StableId>()
                            .saturating_add(state_value_payload_bytes(&field.value))
                    })
                    .sum(),
            ),
        gc_roots: objects
            .iter()
            .filter_map(|slot| slot.scalar.as_ref())
            .map(state_value_root_count)
            .sum::<usize>()
            .saturating_add(
                fields
                    .iter()
                    .map(|field| state_value_root_count(&field.value))
                    .sum(),
            ),
    }
}

fn value_usage(value: &StateValue) -> MigrationUsage {
    match value {
        StateValue::Object(object) => MigrationUsage {
            objects: 1,
            fields: object.fields.len(),
            payload_bytes: std::mem::size_of::<StableId>()
                .saturating_add(std::mem::size_of::<u32>())
                .saturating_add(
                    object
                        .fields
                        .values()
                        .map(|value| {
                            std::mem::size_of::<StableId>()
                                .saturating_add(state_value_payload_bytes(value))
                        })
                        .sum(),
                ),
            gc_roots: object.fields.values().map(state_value_root_count).sum(),
        },
        scalar => MigrationUsage {
            objects: 1,
            fields: 0,
            payload_bytes: state_value_payload_bytes(scalar),
            gc_roots: state_value_root_count(scalar),
        },
    }
}

fn object_payload_bytes(slot: &MigrationObjectSlot) -> usize {
    if let Some(value) = &slot.scalar {
        state_value_payload_bytes(value)
    } else {
        std::mem::size_of::<StableId>() + std::mem::size_of::<u32>()
    }
}

fn state_value_payload_bytes(value: &StateValue) -> usize {
    match value {
        StateValue::I32(_) => std::mem::size_of::<i32>(),
        StateValue::I64(_) | StateValue::F64(_) => std::mem::size_of::<u64>(),
        StateValue::F32(_) | StateValue::Rune(_) => std::mem::size_of::<u32>(),
        StateValue::Bool(_) => 1,
        StateValue::String { .. } => std::mem::size_of::<GcRef>() + std::mem::size_of::<u64>(),
        StateValue::Struct { .. } => {
            std::mem::size_of::<GcRef>()
                + std::mem::size_of::<StableId>()
                + std::mem::size_of::<u64>()
        }
        StateValue::Ref { .. } => {
            std::mem::size_of::<GcRef>() + std::mem::size_of::<Option<StableId>>()
        }
        StateValue::Handle(_) => std::mem::size_of::<StateHandle>(),
        StateValue::Object(_) => usize::MAX,
    }
}

fn state_value_type(value: &StateValue) -> ValueType {
    match value {
        StateValue::I32(_) => ValueType::I32,
        StateValue::I64(_) => ValueType::I64,
        StateValue::F32(_) => ValueType::F32,
        StateValue::F64(_) => ValueType::F64,
        StateValue::Bool(_) => ValueType::Bool,
        StateValue::Rune(_) => ValueType::Rune,
        StateValue::String { .. } => ValueType::String,
        StateValue::Struct { type_id, .. }
        | StateValue::Ref {
            type_id: Some(type_id),
            ..
        } => ValueType::Named(*type_id),
        StateValue::Ref { type_id: None, .. } => ValueType::Ref,
        StateValue::Handle(_) => ValueType::Named(nexa_bytecode::state_handle_type(
            ValueType::Named(StableId::from_name("StateValue")),
        )),
        StateValue::Object(object) => ValueType::Named(object.type_id),
    }
}

fn state_value_root_count(value: &StateValue) -> usize {
    usize::from(matches!(
        value,
        StateValue::String { .. } | StateValue::Struct { .. } | StateValue::Ref { .. }
    ))
}

fn push_root(value: &StateValue, roots: &mut Vec<GcRef>) {
    match value {
        StateValue::String { reference, .. }
        | StateValue::Struct { reference, .. }
        | StateValue::Ref { reference, .. } => {
            roots.push(*reference);
        }
        _ => {}
    }
}

fn reject_nested_object(value: &StateValue) -> Result<(), StatefulError> {
    if let StateValue::Object(object) = value
        && object
            .fields
            .values()
            .any(|field| matches!(field, StateValue::Object(_)))
    {
        return Err(StatefulError::NestedObject);
    }
    Ok(())
}

fn state_value_matches(
    value: &StateValue,
    expected: nexa_bytecode::ValueType,
    handle_target: Option<ValueType>,
) -> bool {
    match (value, expected) {
        (&StateValue::Rune(value), nexa_bytecode::ValueType::Rune) => {
            char::from_u32(value).is_some()
        }
        (&StateValue::String { .. }, nexa_bytecode::ValueType::String)
        | (&StateValue::I32(_), nexa_bytecode::ValueType::I32)
        | (&StateValue::I64(_), nexa_bytecode::ValueType::I64)
        | (&StateValue::F32(_), nexa_bytecode::ValueType::F32)
        | (&StateValue::F64(_), nexa_bytecode::ValueType::F64)
        | (&StateValue::Bool(_), nexa_bytecode::ValueType::Bool)
        | (&StateValue::Ref { type_id: None, .. }, nexa_bytecode::ValueType::Ref) => true,
        (
            &StateValue::Ref {
                type_id: actual, ..
            },
            nexa_bytecode::ValueType::Named(expected),
        ) => actual == Some(expected),
        (&StateValue::Handle(_), nexa_bytecode::ValueType::Named(expected)) => {
            handle_target.is_some_and(|target| nexa_bytecode::state_handle_type(target) == expected)
        }
        (
            &StateValue::Struct {
                type_id: actual, ..
            },
            nexa_bytecode::ValueType::Named(expected),
        ) => actual == expected,
        _ => false,
    }
}

fn clone_leaf_value(value: &StateValue) -> StateValue {
    match value {
        StateValue::I32(value) => StateValue::I32(*value),
        StateValue::I64(value) => StateValue::I64(*value),
        StateValue::F32(value) => StateValue::F32(*value),
        StateValue::F64(value) => StateValue::F64(*value),
        StateValue::Bool(value) => StateValue::Bool(*value),
        StateValue::Rune(value) => StateValue::Rune(*value),
        StateValue::String { reference, hash } => StateValue::String {
            reference: *reference,
            hash: *hash,
        },
        StateValue::Struct {
            reference,
            type_id,
            hash,
        } => StateValue::Struct {
            reference: *reference,
            type_id: *type_id,
            hash: *hash,
        },
        StateValue::Ref { reference, type_id } => StateValue::Ref {
            reference: *reference,
            type_id: *type_id,
        },
        StateValue::Handle(handle) => StateValue::Handle(*handle),
        StateValue::Object(_) => unreachable!("nested state objects are rejected at admission"),
    }
}

struct MigrationArena {
    objects: Vec<MigrationObjectSlot>,
    fields: Vec<MigrationFieldSlot>,
    forwarding: Vec<ForwardingSlot>,
    payload: Vec<u8>,
    gc_roots: Vec<GcRef>,
    object_capacity: usize,
    field_capacity: usize,
    forwarding_capacity: usize,
    byte_capacity: usize,
    gc_root_capacity: usize,
    usage_report: MigrationUsageReport,
}

impl MigrationArena {
    fn try_new(limits: MigrationLimits) -> Result<Self, MigrationLimitError> {
        let report = limits.capacity_report();
        Ok(Self {
            objects: reserve(report.object_capacity, MigrationLimitError::Objects)?,
            fields: reserve(report.field_capacity, MigrationLimitError::Fields)?,
            forwarding: reserve(report.forwarding_capacity, MigrationLimitError::Forwarding)?,
            payload: reserve(
                report.payload_byte_capacity,
                MigrationLimitError::StateBytes,
            )?,
            gc_roots: reserve(limits.max_gc_roots as usize, MigrationLimitError::GcRoots)?,
            object_capacity: report.object_capacity,
            field_capacity: report.field_capacity,
            forwarding_capacity: report.forwarding_capacity,
            byte_capacity: report.payload_byte_capacity,
            gc_root_capacity: limits.max_gc_roots as usize,
            usage_report: MigrationUsageReport::default(),
        })
    }

    fn usage(&self) -> MigrationUsage {
        MigrationUsage {
            objects: self.objects.len(),
            fields: self.fields.len(),
            payload_bytes: self.payload.len(),
            gc_roots: self.gc_roots.len(),
        }
    }

    fn object_index(&self, stable_id: StableId) -> Result<usize, usize> {
        self.objects
            .binary_search_by_key(&stable_id, |slot| slot.stable_id)
    }

    fn forwarding_index(&self, stable_id: StableId) -> Result<usize, usize> {
        self.forwarding
            .binary_search_by_key(&stable_id, |slot| slot.old_id)
    }

    fn check_usage(&self, usage: MigrationUsage) -> Result<(), MigrationLimitError> {
        if usage.objects > self.object_capacity {
            return Err(MigrationLimitError::Objects);
        }
        if usage.fields > self.field_capacity {
            return Err(MigrationLimitError::Fields);
        }
        if usage.payload_bytes > self.byte_capacity {
            return Err(MigrationLimitError::StateBytes);
        }
        if usage.gc_roots > self.gc_root_capacity {
            return Err(MigrationLimitError::GcRoots);
        }
        Ok(())
    }

    fn check_forwarding(&self) -> Result<(), MigrationLimitError> {
        if self.forwarding.len() == self.forwarding_capacity {
            Err(MigrationLimitError::Forwarding)
        } else {
            Ok(())
        }
    }

    fn insert_forwarding(&mut self, index: usize, old_id: StableId, target: Option<StableId>) {
        self.forwarding
            .insert(index, ForwardingSlot { old_id, target });
        self.record_peaks();
    }

    fn insert_object(
        &mut self,
        index: usize,
        stable_id: StableId,
        type_id: StableId,
        version: u32,
        generation: u32,
        scalar: Option<StateValue>,
    ) {
        let field_start = self
            .objects
            .get(index)
            .map_or(self.fields.len(), |slot| slot.field_start as usize);
        self.objects.insert(
            index,
            MigrationObjectSlot {
                stable_id,
                type_id,
                version,
                generation,
                field_start: u32::try_from(field_start).expect("migration field offset fits u32"),
                field_len: 0,
                scalar,
            },
        );
    }

    fn insert_field(&mut self, object_index: usize, field_index: usize, field: MigrationFieldSlot) {
        self.fields.insert(field_index, field);
        self.objects[object_index].field_len += 1;
        for slot in &mut self.objects[object_index + 1..] {
            slot.field_start += 1;
        }
    }

    fn rebuild_caches(&mut self) {
        let usage = registry_usage(&self.objects, &self.fields);
        self.payload.resize(usage.payload_bytes, 0);
        self.gc_roots.clear();
        for slot in &self.objects {
            if let Some(value) = &slot.scalar {
                push_root(value, &mut self.gc_roots);
            }
        }
        for field in &self.fields {
            push_root(&field.value, &mut self.gc_roots);
        }
        self.record_peaks();
    }

    fn record_peaks(&mut self) {
        self.usage_report.object_peak = self.usage_report.object_peak.max(self.objects.len());
        self.usage_report.field_peak = self.usage_report.field_peak.max(self.fields.len());
        self.usage_report.forwarding_peak =
            self.usage_report.forwarding_peak.max(self.forwarding.len());
        self.usage_report.payload_byte_peak =
            self.usage_report.payload_byte_peak.max(self.payload.len());
        self.usage_report.gc_root_peak = self.usage_report.gc_root_peak.max(self.gc_roots.len());
    }

    fn into_registry(self, domain: StatefulDomainId) -> StatefulRegistry {
        StatefulRegistry {
            domain,
            objects: self.objects,
            fields: self.fields,
            payload: self.payload,
            gc_roots: self.gc_roots,
            object_capacity: self.object_capacity,
            field_capacity: self.field_capacity,
            byte_capacity: self.byte_capacity,
            gc_root_capacity: self.gc_root_capacity,
        }
    }
}

fn reserve<T>(capacity: usize, error: MigrationLimitError) -> Result<Vec<T>, MigrationLimitError> {
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|_| error)?;
    Ok(values)
}

#[derive(Clone, Copy)]
struct DeterministicMigrationHasher {
    value: u64,
}

impl DeterministicMigrationHasher {
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    const fn new() -> Self {
        Self {
            value: 0xcbf2_9ce4_8422_2325,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.value ^= u64::from(*byte);
            self.value = self.value.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write(&[value]);
    }

    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }
}

fn migration_registry_hash(
    mut hash: DeterministicMigrationHasher,
    registry: &StatefulRegistry,
) -> StableId {
    hash.write_u8(0x53);
    write_registry_hash(&mut hash, registry);
    StableId(hash.value)
}

fn final_registry_hash(registry: &StatefulRegistry) -> StableId {
    let mut hash = DeterministicMigrationHasher::new();
    hash.write_u8(0x46);
    write_registry_hash(&mut hash, registry);
    StableId(hash.value)
}

fn write_registry_hash(hash: &mut DeterministicMigrationHasher, registry: &StatefulRegistry) {
    hash.write_u64(registry.domain.get());
    hash.write_u64(u64::try_from(registry.objects.len()).unwrap_or(u64::MAX));
    for slot in &registry.objects {
        hash.write_u64(slot.stable_id.0);
        hash.write_u64(slot.type_id.0);
        hash.write_u32(slot.version);
        hash.write_u32(slot.generation);
        if let Some(value) = &slot.scalar {
            hash.write_u8(1);
            hash_state_value(hash, value);
        } else {
            hash.write_u8(0);
            let fields = registry.object_fields(slot);
            hash.write_u64(u64::try_from(fields.len()).unwrap_or(u64::MAX));
            for field in fields {
                hash.write_u64(field.field_id.0);
                hash_state_value(hash, &field.value);
            }
        }
    }
}

fn hash_state_value(hash: &mut DeterministicMigrationHasher, value: &StateValue) {
    match value {
        StateValue::I32(value) => {
            hash.write_u8(1);
            hash.write(&value.to_le_bytes());
        }
        StateValue::I64(value) => {
            hash.write_u8(5);
            hash.write(&value.to_le_bytes());
        }
        StateValue::F32(bits) => {
            hash.write_u8(6);
            hash.write_u32(*bits);
        }
        StateValue::F64(bits) => {
            hash.write_u8(7);
            hash.write_u64(*bits);
        }
        StateValue::Rune(value) => {
            hash.write_u8(8);
            hash.write_u32(*value);
        }
        StateValue::String { hash: value, .. } => {
            hash.write_u8(9);
            hash.write_u64(*value);
        }
        StateValue::Struct {
            type_id,
            hash: value,
            ..
        } => {
            hash.write_u8(10);
            hash.write_u64(type_id.0);
            hash.write_u64(*value);
        }
        StateValue::Bool(value) => {
            hash.write_u8(2);
            hash.write_u8(u8::from(*value));
        }
        StateValue::Ref { type_id, .. } => {
            // GC slot coordinates are deliberately excluded from the stable migration identity.
            hash.write_u8(3);
            hash.write_u64(type_id.unwrap_or(StableId(0)).0);
        }
        StateValue::Handle(handle) => {
            hash.write_u8(4);
            hash.write_u64(handle.domain.get());
            hash.write_u64(handle.stable_id.0);
            hash.write_u32(handle.generation);
        }
        StateValue::Object(_) => unreachable!("nested state objects are rejected at admission"),
    }
}

pub(crate) struct MigrationContext {
    old: Arc<StatefulRegistry>,
    arena: MigrationArena,
    domain: StatefulDomainId,
    context_token: u64,
    schema: nexa_bytecode::StateSchema,
    flags: u8,
    invalid: Option<ReloadError>,
    operation_hash: DeterministicMigrationHasher,
}

const SCHEMA_UNCHANGED: u8 = 1 << 0;
const TOUCHED: u8 = 1 << 1;
const FINALIZED: u8 = 1 << 2;
const OBSERVED_FIRST_OPCODE: u8 = 1 << 3;

impl MigrationContext {
    pub(crate) fn new(
        old: impl Into<Arc<StatefulRegistry>>,
        domain: StatefulDomainId,
        schema: nexa_bytecode::StateSchema,
        schema_unchanged: bool,
        limits: MigrationLimits,
    ) -> Result<Self, ReloadError> {
        observe_migration(
            MigrationAllocationPhase::ContextConstruction,
            AllocationBoundary::Begin,
        );
        let arena = MigrationArena::try_new(limits).map_err(ReloadError::MigrationLimit);
        observe_migration(
            MigrationAllocationPhase::ContextConstruction,
            AllocationBoundary::End,
        );
        let arena = arena?;
        let mut operation_hash = DeterministicMigrationHasher::new();
        operation_hash.write_u8(0x4d);
        operation_hash.write_u64(domain.get());
        Ok(Self {
            old: old.into(),
            arena,
            domain,
            context_token: next_migration_context_token(),
            schema,
            flags: u8::from(schema_unchanged) * SCHEMA_UNCHANGED,
            invalid: None,
            operation_hash,
        })
    }

    pub(crate) fn limit_error(&self) -> Option<MigrationLimitError> {
        match self.invalid {
            Some(ReloadError::MigrationLimit(error)) => Some(error),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn usage_report(&self) -> MigrationUsageReport {
        self.arena.usage_report
    }

    fn reject_limit<T>(&mut self, error: MigrationLimitError) -> Result<T, RuntimeMessage> {
        self.invalid = Some(ReloadError::MigrationLimit(error));
        Err(RuntimeMessage::Static("migration limit exceeded"))
    }

    fn precheck(&mut self, usage: MigrationUsage) -> Result<(), RuntimeMessage> {
        self.arena.check_usage(usage).map_err(|error| {
            self.invalid = Some(ReloadError::MigrationLimit(error));
            RuntimeMessage::Static("migration capacity exceeded")
        })
    }

    fn precheck_forwarding(&mut self) -> Result<(), RuntimeMessage> {
        self.arena.check_forwarding().map_err(|error| {
            self.invalid = Some(ReloadError::MigrationLimit(error));
            RuntimeMessage::Static("migration forwarding capacity exceeded")
        })
    }

    pub(crate) fn finish(self) -> Result<MigrationOutput, ReloadError> {
        let _observation = MigrationObservation::new(MigrationAllocationPhase::Finish, false);
        self.finish_inner()
    }

    fn finish_inner(mut self) -> Result<MigrationOutput, ReloadError> {
        if !self.has_flag(TOUCHED) {
            if self.has_flag(SCHEMA_UNCHANGED) {
                self.old
                    .validate_schema(&self.schema)
                    .map_err(|_| ReloadError::InvalidStateHandle)?;
                self.old
                    .validate_handles()
                    .map_err(|_| ReloadError::InvalidStateHandle)?;
                let hash = migration_registry_hash(self.operation_hash, &self.old);
                return Ok(MigrationOutput::Shared {
                    registry: self.old,
                    hash,
                    usage: self.arena.usage_report,
                });
            }
            return Err(ReloadError::MigrationNoOutput);
        }
        if let Some(error) = self.invalid {
            return Err(error);
        }
        if !self.has_flag(FINALIZED) {
            return Err(ReloadError::MigrationNotFinished);
        }
        if self
            .old
            .objects
            .iter()
            .any(|slot| self.arena.forwarding_index(slot.stable_id).is_err())
        {
            return Err(ReloadError::MissingForwarding);
        }
        self.remap_handles();
        let usage = self.arena.usage_report;
        let registry = self.arena.into_registry(self.domain);
        registry
            .validate_schema(&self.schema)
            .map_err(|_| ReloadError::GraphCheck)?;
        registry
            .validate_handles()
            .map_err(|_| ReloadError::InvalidStateHandle)?;
        let hash = migration_registry_hash(self.operation_hash, &registry);
        Ok(MigrationOutput::Owned {
            registry,
            hash,
            usage,
        })
    }

    fn observe_opcode(&mut self, phase: MigrationAllocationPhase) -> MigrationObservation {
        let first = !self.has_flag(OBSERVED_FIRST_OPCODE);
        self.set_flag(OBSERVED_FIRST_OPCODE);
        MigrationObservation::new(phase, first)
    }

    const fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    fn set_flag(&mut self, flag: u8) {
        self.flags |= flag;
    }

    fn remap_handles(&mut self) {
        let mut remaps = 0_usize;
        for index in 0..self.arena.objects.len() {
            let remapped = self.arena.objects[index]
                .scalar
                .as_ref()
                .and_then(|value| self.remapped_handle(value));
            if let Some(handle) = remapped {
                self.arena.objects[index].scalar = Some(StateValue::Handle(handle));
                remaps = remaps.saturating_add(1);
            }
        }
        for index in 0..self.arena.fields.len() {
            let remapped = self.remapped_handle(&self.arena.fields[index].value);
            if let Some(handle) = remapped {
                self.arena.fields[index].value = StateValue::Handle(handle);
                remaps = remaps.saturating_add(1);
            }
        }
        self.arena.usage_report.handle_remaps =
            self.arena.usage_report.handle_remaps.saturating_add(remaps);
    }

    fn remapped_handle(&self, value: &StateValue) -> Option<StateHandle> {
        let StateValue::Handle(handle) = value else {
            return None;
        };
        let mut handle = *handle;
        let Ok(index) = self.arena.forwarding_index(handle.stable_id) else {
            return None;
        };
        let target = self.arena.forwarding[index].target?;
        if let Ok(target_index) = self.arena.object_index(target) {
            handle.stable_id = target;
            handle.generation = self.arena.objects[target_index].generation;
            Some(handle)
        } else {
            None
        }
    }

    fn old_slot(&self, stable_id: StableId) -> Result<&MigrationObjectSlot, RuntimeMessage> {
        self.old
            .object(stable_id)
            .map_err(|_| "old state does not exist".into())
    }
}

pub(crate) enum MigrationOutput {
    Owned {
        registry: StatefulRegistry,
        hash: StableId,
        usage: MigrationUsageReport,
    },
    Shared {
        registry: Arc<StatefulRegistry>,
        hash: StableId,
        usage: MigrationUsageReport,
    },
}

impl MigrationOutput {
    pub(crate) fn into_shared(self) -> (Arc<StatefulRegistry>, StableId, MigrationUsageReport) {
        match self {
            Self::Owned {
                registry,
                hash,
                usage,
            } => (Arc::new(registry), hash, usage),
            Self::Shared {
                registry,
                hash,
                usage,
            } => (registry, hash, usage),
        }
    }
}

impl InterpreterMigration for MigrationContext {
    fn observe_fuel_used(&mut self, fuel_used: u64) {
        self.arena.usage_report.fuel_used = fuel_used;
    }

    fn observe_call_depth(&mut self, depth: usize) {
        self.arena.usage_report.max_call_depth_used = self
            .arena
            .usage_report
            .max_call_depth_used
            .max(u16::try_from(depth).unwrap_or(u16::MAX));
    }

    fn old_get(
        &mut self,
        stable_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::OldGet);
        self.ensure_open()?;
        let slot = self.old_slot(stable_id)?;
        let value = slot_to_runtime_value(self.context_token, slot);
        if runtime_state_type(value) != expected {
            return Err("old state type does not match migration opcode".into());
        }
        self.arena.usage_report.objects_read =
            self.arena.usage_report.objects_read.saturating_add(1);
        Ok(value)
    }

    fn old_field_get(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::OldFieldGet);
        self.ensure_open()?;
        let RuntimeValue::MigrationOldObject(object) = object else {
            return Err("STATE_OLD_FIELD_GET requires an old state object".into());
        };
        let (context, stable_id, type_id, generation) = object.parts();
        if context != self.context_token {
            return Err("old state object belongs to another migration".into());
        }
        let slot = self.old_slot(stable_id)?;
        if slot.scalar.is_some() || slot.type_id != type_id || slot.generation != generation {
            return Err("old state object type mismatch".into());
        }
        let fields = self.old.object_fields(slot);
        let field = fields
            .binary_search_by_key(&field_id, |field| field.field_id)
            .map(|index| &fields[index])
            .map_err(|_| RuntimeMessage::Static("old state field does not exist"))?;
        let value = state_field_to_runtime_value(field_id, &field.value, expected)?;
        if runtime_state_type(value) != expected {
            return Err("old state field type mismatch".into());
        }
        self.arena.usage_report.objects_read =
            self.arena.usage_report.objects_read.saturating_add(1);
        Ok(value)
    }

    fn new_create(
        &mut self,
        stable_id: StableId,
        type_id: StableId,
    ) -> Result<RuntimeValue, RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::NewCreate);
        self.ensure_open()?;
        let version = self
            .schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == type_id)
            .map(|state_type| state_type.version)
            .ok_or(RuntimeMessage::Static(
                "candidate state type does not exist",
            ))?;
        let Err(index) = self.arena.object_index(stable_id) else {
            return Err("new state object already exists".into());
        };
        let usage = self.arena.usage();
        self.precheck(MigrationUsage {
            objects: usage.objects + 1,
            payload_bytes: usage
                .payload_bytes
                .saturating_add(std::mem::size_of::<StableId>() + std::mem::size_of::<u32>()),
            ..usage
        })?;
        self.arena
            .insert_object(index, stable_id, type_id, version, 0, None);
        self.arena.rebuild_caches();
        self.set_flag(TOUCHED);
        self.arena.usage_report.objects_created =
            self.arena.usage_report.objects_created.saturating_add(1);
        let generation = self.arena.objects[index].generation;
        Ok(RuntimeValue::MigrationStagingObject(
            MigrationStagingObjectHandle::new(self.context_token, stable_id, type_id, generation),
        ))
    }

    fn new_set(
        &mut self,
        object: RuntimeValue,
        field_id: StableId,
        value: RuntimeValue,
    ) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::NewSet);
        self.ensure_open()?;
        let RuntimeValue::MigrationStagingObject(object) = object else {
            return Err("STATE_NEW_SET requires a staging object".into());
        };
        let (context, object_id, type_id, generation) = object.parts();
        if context != self.context_token {
            return Err("staging object belongs to another migration".into());
        }
        let object_index = self
            .arena
            .object_index(object_id)
            .map_err(|_| RuntimeMessage::Static("staging object does not exist"))?;
        let slot = &self.arena.objects[object_index];
        if slot.scalar.is_some() || slot.type_id != type_id || slot.generation != generation {
            return Err("staging object type mismatch".into());
        }
        let value = runtime_to_state_value(value, &self.arena, self.domain)?;
        let expected = self
            .schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == type_id)
            .and_then(|state_type| {
                state_type
                    .fields
                    .iter()
                    .find(|field| field.stable_id == field_id)
            })
            .map(|field| field.ty)
            .ok_or(RuntimeMessage::Static(
                "candidate state field does not exist",
            ))?;
        let handle_target = match &value {
            StateValue::Handle(handle) if handle.domain == self.domain => self
                .arena
                .object_index(handle.stable_id)
                .ok()
                .filter(|index| self.arena.objects[*index].generation == handle.generation)
                .map(|index| ValueType::Named(self.arena.objects[index].type_id)),
            _ => None,
        };
        if !state_value_matches(&value, expected, handle_target) {
            return Err("candidate state field type mismatch".into());
        }
        let start = slot.field_start as usize;
        let fields = &self.arena.fields[start..start + slot.field_len as usize];
        let search = fields.binary_search_by_key(&field_id, |field| field.field_id);
        let usage = self.arena.usage();
        let old = search.ok().map(|offset| &fields[offset].value);
        let payload_bytes = usage
            .payload_bytes
            .saturating_sub(old.map_or(0, state_value_payload_bytes))
            .saturating_add(
                old.map_or(std::mem::size_of::<StableId>(), |_| 0)
                    + state_value_payload_bytes(&value),
            );
        let gc_roots = usage
            .gc_roots
            .saturating_sub(old.map_or(0, state_value_root_count))
            .saturating_add(state_value_root_count(&value));
        self.precheck(MigrationUsage {
            fields: usage.fields + usize::from(old.is_none()),
            payload_bytes,
            gc_roots,
            ..usage
        })?;
        match search {
            Ok(offset) => self.arena.fields[start + offset].value = value,
            Err(offset) => self.arena.insert_field(
                object_index,
                start + offset,
                MigrationFieldSlot { field_id, value },
            ),
        }
        self.arena.rebuild_caches();
        self.set_flag(TOUCHED);
        self.arena.usage_report.fields_written =
            self.arena.usage_report.fields_written.saturating_add(1);
        Ok(())
    }

    fn preserve(&mut self, stable_id: StableId) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::Preserve);
        self.ensure_open()?;
        let Err(forwarding_index) = self.arena.forwarding_index(stable_id) else {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        };
        self.precheck_forwarding()?;
        let old_index = self
            .old
            .objects
            .binary_search_by_key(&stable_id, |slot| slot.stable_id)
            .map_err(|_| RuntimeMessage::Static("STATE_PRESERVE source does not exist"))?;
        let old_slot = &self.old.objects[old_index];
        let Err(object_index) = self.arena.object_index(stable_id) else {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        };
        let old_fields = self.old.object_fields(old_slot);
        let usage = self.arena.usage();
        let payload_delta = object_payload_bytes(old_slot).saturating_add(
            old_fields
                .iter()
                .map(|field| {
                    std::mem::size_of::<StableId>()
                        .saturating_add(state_value_payload_bytes(&field.value))
                })
                .sum(),
        );
        let root_delta = old_slot
            .scalar
            .as_ref()
            .map_or(0, state_value_root_count)
            .saturating_add(
                old_fields
                    .iter()
                    .map(|field| state_value_root_count(&field.value))
                    .sum(),
            );
        self.precheck(MigrationUsage {
            objects: usage.objects + 1,
            fields: usage.fields + old_fields.len(),
            payload_bytes: usage.payload_bytes.saturating_add(payload_delta),
            gc_roots: usage.gc_roots.saturating_add(root_delta),
        })?;
        let old_slot = &self.old.objects[old_index];
        let old_fields_start = old_slot.field_start as usize;
        let old_fields_len = old_slot.field_len as usize;
        self.arena.insert_object(
            object_index,
            old_slot.stable_id,
            old_slot.type_id,
            old_slot.version,
            old_slot.generation,
            old_slot.scalar.as_ref().map(clone_leaf_value),
        );
        for offset in 0..old_fields_len {
            let old_field = &self.old.fields[old_fields_start + offset];
            self.arena.insert_field(
                object_index,
                self.arena.objects[object_index].field_start as usize + offset,
                MigrationFieldSlot {
                    field_id: old_field.field_id,
                    value: clone_leaf_value(&old_field.value),
                },
            );
        }
        self.arena
            .insert_forwarding(forwarding_index, stable_id, Some(stable_id));
        self.operation_hash.write_u8(1);
        self.operation_hash.write_u64(stable_id.0);
        self.arena.rebuild_caches();
        self.set_flag(TOUCHED);
        self.arena.usage_report.preserved = self.arena.usage_report.preserved.saturating_add(1);
        Ok(())
    }

    fn replace(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::Replace);
        self.ensure_open()?;
        let RuntimeValue::MigrationStagingObject(target) = target else {
            return Err("STATE_REPLACE requires a staging object".into());
        };
        let (context, target_id, type_id, generation) = target.parts();
        if context != self.context_token {
            return Err("staging object belongs to another migration".into());
        }
        let target_index = self
            .arena
            .object_index(target_id)
            .map_err(|_| RuntimeMessage::Static("remap target does not exist"))?;
        if self.arena.objects[target_index].scalar.is_some()
            || self.arena.objects[target_index].type_id != type_id
            || self.arena.objects[target_index].generation != generation
        {
            return Err("remap target is not a matching staging object".into());
        }
        let Err(forwarding_index) = self.arena.forwarding_index(old_id) else {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        };
        self.precheck_forwarding()?;
        let old_generation = self.old_slot(old_id)?.generation;
        let Some(replacement_generation) = old_generation.checked_add(1) else {
            return self.reject_limit(MigrationLimitError::Generation);
        };
        self.arena.objects[target_index].generation = replacement_generation;
        self.arena
            .insert_forwarding(forwarding_index, old_id, Some(target_id));
        self.operation_hash.write_u8(2);
        self.operation_hash.write_u64(old_id.0);
        self.operation_hash.write_u64(target_id.0);
        self.set_flag(TOUCHED);
        self.arena.usage_report.replaced = self.arena.usage_report.replaced.saturating_add(1);
        self.arena.usage_report.generation_changes =
            self.arena.usage_report.generation_changes.saturating_add(1);
        Ok(())
    }

    fn delete(&mut self, stable_id: StableId) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::Delete);
        self.ensure_open()?;
        self.old_slot(stable_id)?;
        let Err(forwarding_index) = self.arena.forwarding_index(stable_id) else {
            self.invalid = Some(ReloadError::DuplicateForwarding);
            return Ok(());
        };
        self.precheck_forwarding()?;
        self.arena
            .insert_forwarding(forwarding_index, stable_id, None);
        self.operation_hash.write_u8(3);
        self.operation_hash.write_u64(stable_id.0);
        self.set_flag(TOUCHED);
        self.arena.usage_report.deleted = self.arena.usage_report.deleted.saturating_add(1);
        Ok(())
    }

    fn finish_staging(&mut self) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::StateFinish);
        self.ensure_open()?;
        self.set_flag(FINALIZED);
        Ok(())
    }
}

struct MigrationObservation {
    phase: MigrationAllocationPhase,
    first: bool,
}

impl MigrationObservation {
    fn new(phase: MigrationAllocationPhase, first: bool) -> Self {
        if first {
            observe_migration(
                MigrationAllocationPhase::FirstOpcode,
                AllocationBoundary::Begin,
            );
        }
        observe_migration(phase, AllocationBoundary::Begin);
        Self { phase, first }
    }
}

impl Drop for MigrationObservation {
    fn drop(&mut self) {
        observe_migration(self.phase, AllocationBoundary::End);
        if self.first {
            observe_migration(
                MigrationAllocationPhase::FirstOpcode,
                AllocationBoundary::End,
            );
        }
    }
}

impl MigrationContext {
    fn ensure_open(&self) -> Result<(), RuntimeMessage> {
        if self.has_flag(FINALIZED) {
            Err("STATE_FINISH already executed".into())
        } else {
            Ok(())
        }
    }
}

fn slot_to_runtime_value(context: u64, slot: &MigrationObjectSlot) -> RuntimeValue {
    slot.scalar.as_ref().map_or(
        RuntimeValue::MigrationOldObject(MigrationOldObjectHandle::new(
            context,
            slot.stable_id,
            slot.type_id,
            slot.generation,
        )),
        |value| state_to_runtime_value(slot.stable_id, value),
    )
}

fn state_field_to_runtime_value(
    stable_id: StableId,
    value: &StateValue,
    expected: ValueType,
) -> Result<RuntimeValue, RuntimeMessage> {
    if let StateValue::Handle(handle) = value {
        let ValueType::Named(handle_type) = expected else {
            return Err("state handle field has non-handle schema type".into());
        };
        return Ok(RuntimeValue::StateHandle {
            handle_type,
            domain: handle.domain.get(),
            stable_id: handle.stable_id,
            generation: handle.generation,
        });
    }
    Ok(state_to_runtime_value(stable_id, value))
}

fn state_to_runtime_value(stable_id: StableId, value: &StateValue) -> RuntimeValue {
    match value {
        StateValue::I32(value) => RuntimeValue::I32(*value),
        StateValue::I64(value) => RuntimeValue::I64(*value),
        StateValue::F32(bits) => RuntimeValue::F32(*bits),
        StateValue::F64(bits) => RuntimeValue::F64(*bits),
        StateValue::Bool(value) => RuntimeValue::Bool(*value),
        StateValue::Rune(value) => RuntimeValue::Rune(*value),
        StateValue::String { reference, hash } => RuntimeValue::String {
            reference: *reference,
            hash: *hash,
        },
        StateValue::Struct {
            reference,
            type_id,
            hash,
        } => RuntimeValue::Struct {
            reference: *reference,
            type_id: *type_id,
            hash: *hash,
        },
        StateValue::Ref {
            reference,
            type_id: Some(type_id),
        } => RuntimeValue::NamedRef {
            reference: *reference,
            type_id: *type_id,
        },
        StateValue::Ref {
            reference,
            type_id: None,
        } => RuntimeValue::Ref(*reference),
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
    arena: &MigrationArena,
    domain: StatefulDomainId,
) -> Result<StateValue, RuntimeMessage> {
    match value {
        RuntimeValue::I32(value) => Ok(StateValue::I32(value)),
        RuntimeValue::I64(value) => Ok(StateValue::I64(value)),
        RuntimeValue::F32(bits) => Ok(StateValue::F32(bits)),
        RuntimeValue::F64(bits) => Ok(StateValue::F64(bits)),
        RuntimeValue::Bool(value) => Ok(StateValue::Bool(value)),
        RuntimeValue::Rune(value) => Ok(StateValue::Rune(value)),
        RuntimeValue::String { reference, hash } => Ok(StateValue::String { reference, hash }),
        RuntimeValue::Struct {
            reference,
            type_id,
            hash,
        } => Ok(StateValue::Struct {
            reference,
            type_id,
            hash,
        }),
        RuntimeValue::Ref(reference) => Ok(StateValue::Ref {
            reference,
            type_id: None,
        }),
        RuntimeValue::NamedRef { reference, type_id } => Ok(StateValue::Ref {
            reference,
            type_id: Some(type_id),
        }),
        RuntimeValue::Opaque { value, .. } => {
            let stable_id = StableId(value);
            let generation = arena
                .object_index(stable_id)
                .map(|index| arena.objects[index].generation)
                .map_err(|_| RuntimeMessage::Static("state handle target does not exist"))?;
            Ok(StateValue::Handle(StateHandle {
                domain,
                stable_id,
                generation,
            }))
        }
        RuntimeValue::StateHandle {
            domain,
            stable_id,
            generation,
            ..
        } => Ok(StateValue::Handle(StateHandle {
            domain: StatefulDomainId::new(domain),
            stable_id,
            generation,
        })),
        RuntimeValue::MigrationOldObject(_) | RuntimeValue::MigrationStagingObject(_) => {
            Err("migration object handles cannot be stored in state".into())
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
        RuntimeValue::I64(_) => nexa_bytecode::ValueType::I64,
        RuntimeValue::F32(_) => nexa_bytecode::ValueType::F32,
        RuntimeValue::F64(_) => nexa_bytecode::ValueType::F64,
        RuntimeValue::Bool(_) => nexa_bytecode::ValueType::Bool,
        RuntimeValue::Rune(_) => nexa_bytecode::ValueType::Rune,
        RuntimeValue::String { .. } => nexa_bytecode::ValueType::String,
        RuntimeValue::Struct { type_id, .. } => nexa_bytecode::ValueType::Named(type_id),
        RuntimeValue::Ref(_) => nexa_bytecode::ValueType::Ref,
        RuntimeValue::NamedRef { type_id, .. } | RuntimeValue::Opaque { type_id, .. } => {
            nexa_bytecode::ValueType::Named(type_id)
        }
        RuntimeValue::MigrationOldObject(object) => {
            nexa_bytecode::ValueType::Named(object.parts().2)
        }
        RuntimeValue::MigrationStagingObject(object) => {
            nexa_bytecode::ValueType::Named(object.parts().2)
        }
        RuntimeValue::StateHandle { handle_type, .. } => {
            nexa_bytecode::ValueType::Named(handle_type)
        }
        RuntimeValue::HostRequest(_) => {
            nexa_bytecode::ValueType::Named(StableId::from_name("HostRequest"))
        }
        RuntimeValue::ResourceToken(_) => {
            nexa_bytecode::ValueType::Named(StableId::from_name("ResourceToken"))
        }
        RuntimeValue::Snapshot(snapshot) => nexa_bytecode::ValueType::Named(snapshot.type_id()),
        RuntimeValue::Unit => nexa_bytecode::ValueType::Named(StableId::from_name("Unit")),
    }
}

#[allow(clippy::too_many_lines)]
pub fn run_offline_migration(
    domain: StatefulDomainId,
    old_objects: Vec<OfflineStateObject>,
    old_module: &nexa_verifier::VerifiedModule,
    new_module: &nexa_verifier::VerifiedModule,
    limits: MigrationLimits,
) -> Result<OfflineMigrationResult, OfflineMigrationError> {
    let migration_entry = new_module
        .module()
        .reload_metadata
        .migration_entry
        .ok_or(OfflineMigrationError::MissingMigrationEntry)?;
    let mut old =
        StatefulRegistry::try_new(domain, limits).map_err(OfflineMigrationError::Capacity)?;
    let mut sorted = old_objects;
    sorted.sort_by_key(|object| object.stable_id);
    for pair in sorted.windows(2) {
        if pair[0].stable_id == pair[1].stable_id {
            return Err(OfflineMigrationError::DuplicateObject(pair[0].stable_id));
        }
    }
    let heap_capacity = limits
        .max_gc_roots
        .saturating_add(limits.max_objects)
        .saturating_add(64)
        .max(1);
    let mut heap = crate::Heap::new_with_string_limit(heap_capacity, limits.max_state_bytes);
    for object in sorted {
        let version = old_module
            .module()
            .state_schema
            .types
            .iter()
            .find(|ty| ty.stable_id == object.type_id)
            .map(|ty| ty.version)
            .ok_or(OfflineMigrationError::UnknownType {
                object: object.stable_id,
                type_id: object.type_id,
            })?;
        if old.objects.len() == old.object_capacity {
            return Err(OfflineMigrationError::Capacity(
                MigrationLimitError::Objects,
            ));
        }
        let field_start = u32::try_from(old.fields.len())
            .map_err(|_| OfflineMigrationError::Capacity(MigrationLimitError::Fields))?;
        let mut fields = object.fields;
        fields.sort_by_key(|field| field.stable_id);
        for pair in fields.windows(2) {
            if pair[0].stable_id == pair[1].stable_id {
                return Err(OfflineMigrationError::DuplicateField {
                    object: object.stable_id,
                    field: pair[0].stable_id,
                });
            }
        }
        if old.fields.len().saturating_add(fields.len()) > old.field_capacity {
            return Err(OfflineMigrationError::Capacity(MigrationLimitError::Fields));
        }
        let field_len = u32::try_from(fields.len())
            .map_err(|_| OfflineMigrationError::Capacity(MigrationLimitError::Fields))?;
        old.objects.push(MigrationObjectSlot {
            stable_id: object.stable_id,
            type_id: object.type_id,
            version,
            generation: object.generation,
            field_start,
            field_len,
            scalar: None,
        });
        old.fields.extend(
            fields
                .into_iter()
                .map(|field| {
                    Ok(MigrationFieldSlot {
                        field_id: field.stable_id,
                        value: match field.value {
                            OfflineStateValue::I32(value) => StateValue::I32(value),
                            OfflineStateValue::I64(value) => StateValue::I64(value),
                            OfflineStateValue::F32(value) => StateValue::F32(value),
                            OfflineStateValue::F64(value) => StateValue::F64(value),
                            OfflineStateValue::Bool(value) => StateValue::Bool(value),
                            OfflineStateValue::Rune(value) => StateValue::Rune(value),
                            OfflineStateValue::String(value) => {
                                let reference = heap.allocate_string(&value).map_err(|error| {
                                    OfflineMigrationError::Interpreter(error.to_string())
                                })?;
                                let hash = heap.string_hash(reference).map_err(|error| {
                                    OfflineMigrationError::Interpreter(error.to_string())
                                })?;
                                StateValue::String { reference, hash }
                            }
                            OfflineStateValue::Handle(handle) => StateValue::Handle(handle),
                        },
                    })
                })
                .collect::<Result<Vec<_>, OfflineMigrationError>>()?,
        );
    }
    old.rebuild_caches();
    if old.payload.len() > old.byte_capacity || old.gc_roots.len() > old.gc_root_capacity {
        return Err(OfflineMigrationError::Capacity(
            MigrationLimitError::StateBytes,
        ));
    }
    old.validate_schema(&old_module.module().state_schema)
        .map_err(|_| OfflineMigrationError::InvalidOldState)?;
    old.validate_handles()
        .map_err(|_| OfflineMigrationError::InvalidHandle)?;

    let mut migration = MigrationContext::new(
        old,
        domain,
        new_module.module().state_schema.clone(),
        old_module.module().state_schema_fingerprint
            == new_module.module().state_schema_fingerprint,
        limits,
    )
    .map_err(OfflineMigrationError::Migration)?;
    let outcome = crate::CheckedInterpreter::run_migration_with_heap(
        new_module,
        migration_entry,
        &[],
        limits.max_fuel,
        crate::FrameLimits {
            max_call_depth: u32::from(limits.max_call_depth),
            ..crate::FrameLimits::default()
        },
        &mut migration,
        &mut heap,
    )
    .map_err(|error| OfflineMigrationError::Interpreter(error.to_string()))?;
    if !matches!(outcome, crate::InterpreterOutcome::Returned { .. }) {
        return Err(OfflineMigrationError::Interpreter(format!(
            "migration did not return: {outcome:?}"
        )));
    }
    let (registry, migration_hash, usage) = match migration
        .finish()
        .map_err(OfflineMigrationError::Migration)?
    {
        MigrationOutput::Owned {
            registry,
            hash,
            usage,
        } => (registry, hash, usage),
        MigrationOutput::Shared {
            registry,
            hash,
            usage,
        } => (
            Arc::try_unwrap(registry).map_err(|_| OfflineMigrationError::InvalidOldState)?,
            hash,
            usage,
        ),
    };
    let final_state_hash = final_registry_hash(&registry);
    let objects = registry
        .objects
        .iter()
        .map(|slot| {
            if slot.scalar.is_some() {
                return Err(OfflineMigrationError::UnsupportedOutputValue);
            }
            let fields = registry
                .object_fields(slot)
                .iter()
                .map(|field| {
                    let value = match field.value {
                        StateValue::I32(value) => OfflineStateValue::I32(value),
                        StateValue::I64(value) => OfflineStateValue::I64(value),
                        StateValue::F32(value) => OfflineStateValue::F32(value),
                        StateValue::F64(value) => OfflineStateValue::F64(value),
                        StateValue::Bool(value) => OfflineStateValue::Bool(value),
                        StateValue::Rune(value) => OfflineStateValue::Rune(value),
                        StateValue::Handle(handle) => OfflineStateValue::Handle(handle),
                        StateValue::String { reference, .. } => OfflineStateValue::String(
                            heap.string(reference)
                                .map_err(|_| OfflineMigrationError::UnsupportedOutputValue)?
                                .to_owned(),
                        ),
                        StateValue::Struct { .. }
                        | StateValue::Ref { .. }
                        | StateValue::Object(_) => {
                            return Err(OfflineMigrationError::UnsupportedOutputValue);
                        }
                    };
                    Ok(OfflineStateField {
                        stable_id: field.field_id,
                        value,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(OfflineStateObject {
                stable_id: slot.stable_id,
                type_id: slot.type_id,
                generation: slot.generation,
                fields,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(OfflineMigrationResult {
        objects,
        migration_hash,
        final_state_hash,
        usage,
    })
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_stateful_registry(data: &[u8]) {
    if data.len() > 256 {
        return;
    }
    let limits = MigrationLimits {
        max_objects: 16,
        max_fields: 16,
        max_forwarding_entries: 16,
        max_state_bytes: 1_024,
        max_gc_roots: 8,
        max_fuel: 128,
        max_call_depth: 8,
    };
    let Ok(mut registry) = StatefulRegistry::try_new(StatefulDomainId::new(7), limits) else {
        return;
    };
    let mut handles = Vec::with_capacity(16);
    for chunk in data.chunks(4).take(64) {
        let stable_id = StableId(u64::from(chunk.get(1).copied().unwrap_or_default()) + 1);
        match chunk.first().copied().unwrap_or_default() % 4 {
            0 => {
                if let Ok(handle) = registry.insert(
                    stable_id,
                    StateValue::I32(i32::from(chunk.get(2).copied().unwrap_or_default())),
                ) {
                    if handles.len() == handles.capacity() {
                        handles.remove(0);
                    }
                    handles.push(handle);
                }
            }
            1 => {
                let _ = registry.insert(
                    stable_id,
                    StateValue::Bool(chunk.get(2).copied().unwrap_or_default() & 1 != 0),
                );
            }
            2 => {
                if let Some(handle) = handles.get(
                    usize::from(chunk.get(2).copied().unwrap_or_default()) % handles.len().max(1),
                ) {
                    let _ = registry.resolve(*handle);
                }
            }
            _ => {
                let _ = registry.object(stable_id);
            }
        }
    }
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_migration_arena(data: &[u8]) {
    if data.len() > 256 {
        return;
    }
    let limits = MigrationLimits {
        max_objects: 16,
        max_fields: 32,
        max_forwarding_entries: 16,
        max_state_bytes: 2_048,
        max_gc_roots: 8,
        max_fuel: 128,
        max_call_depth: 8,
    };
    let Ok(mut arena) = MigrationArena::try_new(limits) else {
        return;
    };
    for chunk in data.chunks(4).take(64) {
        let id = StableId(u64::from(chunk.get(1).copied().unwrap_or_default()) + 1);
        match chunk.first().copied().unwrap_or_default() % 4 {
            0 => {
                if let Err(index) = arena.object_index(id) {
                    let usage = arena.usage();
                    let next = MigrationUsage {
                        objects: usage.objects + 1,
                        payload_bytes: usage
                            .payload_bytes
                            .saturating_add(std::mem::size_of::<i32>()),
                        ..usage
                    };
                    if arena.check_usage(next).is_ok() {
                        arena.insert_object(
                            index,
                            id,
                            StableId::from_name("FuzzScalar"),
                            1,
                            0,
                            Some(StateValue::I32(i32::from(
                                chunk.get(2).copied().unwrap_or_default(),
                            ))),
                        );
                        arena.rebuild_caches();
                    }
                }
            }
            1 => {
                if let Err(index) = arena.forwarding_index(id)
                    && arena.check_forwarding().is_ok()
                {
                    let target = arena.objects.first().map(|slot| slot.stable_id);
                    arena.insert_forwarding(index, id, target);
                }
            }
            2 => {
                arena.rebuild_caches();
            }
            _ => {
                let _ = arena.check_usage(arena.usage());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, StandardIntrinsic,
        StateField, StateSchema, StateType, ValueType,
    };
    use nexa_verifier::{VerifierLimits, verify};

    use super::*;
    use crate::{
        CheckedInterpreter, FrameError, FrameLimits, InterpreterError, InterpreterOutcome,
        SuspendReason,
    };

    fn schema(type_id: StableId, fields: &[(StableId, ValueType)]) -> StateSchema {
        StateSchema {
            types: vec![StateType {
                stable_id: type_id,
                version: 1,
                fields: fields
                    .iter()
                    .map(|(stable_id, ty)| StateField {
                        stable_id: *stable_id,
                        ty: *ty,
                    })
                    .collect(),
            }],
        }
    }

    fn limits() -> MigrationLimits {
        MigrationLimits {
            max_objects: 8,
            max_fields: 8,
            max_forwarding_entries: 8,
            max_state_bytes: 4_096,
            max_gc_roots: 8,
            max_fuel: 64,
            max_call_depth: 8,
        }
    }

    #[test]
    fn stateful_string_fields_hold_gc_roots() {
        let mut heap = crate::Heap::new_with_string_limit(2, 16);
        let reference = heap.allocate_string("persistent").unwrap();
        let hash = heap.string_hash(reference).unwrap();
        let mut registry = StatefulRegistry::try_new(StatefulDomainId::new(7), limits()).unwrap();
        let stable_id = StableId::from_name("persistent-label");
        let handle = registry
            .insert(stable_id, StateValue::String { reference, hash })
            .unwrap();
        assert_eq!(
            registry.resolve(handle),
            Ok(StateValue::String { reference, hash })
        );
        assert_eq!(registry.gc_roots(), vec![reference]);
    }

    #[test]
    fn stateful_struct_fields_preserve_nominal_value_and_nested_gc_roots() {
        let mut heap = crate::Heap::new_with_string_limit(4, 32);
        let label = heap.allocate_string("persistent").unwrap();
        let label_hash = heap.string_hash(label).unwrap();
        let type_id = StableId::from_name("Position");
        let value = heap
            .allocate_struct(
                type_id,
                &[
                    RuntimeValue::I32(7),
                    RuntimeValue::String {
                        reference: label,
                        hash: label_hash,
                    },
                ],
            )
            .unwrap();
        let RuntimeValue::Struct {
            reference, hash, ..
        } = value
        else {
            panic!("struct allocation must produce a struct value");
        };
        let mut registry = StatefulRegistry::try_new(StatefulDomainId::new(7), limits()).unwrap();
        let stable_id = StableId::from_name("persistent-position");
        let state = StateValue::Struct {
            reference,
            type_id,
            hash,
        };
        let handle = registry.insert(stable_id, state.clone()).unwrap();
        assert_eq!(registry.resolve(handle), Ok(state));
        assert_eq!(registry.gc_roots(), vec![reference]);
        assert!(heap.struct_fields(value).unwrap().iter().any(
            |value| matches!(value, RuntimeValue::String { reference: field, .. } if *field == label)
        ));
    }

    #[test]
    fn stateful_handle_fields_preserve_parameterized_target_types() {
        let domain = StatefulDomainId::new(7);
        let target_type = StableId::from_name("Entity");
        let link_type = StableId::from_name("Link");
        let next_field = StableId::from_parts(&["Link", "::next"]);
        let target_id = StableId::from_name("entity");
        let link_id = StableId::from_name("link");
        let mut registry = StatefulRegistry::try_new(domain, limits()).unwrap();
        let target = registry
            .insert(
                target_id,
                StateValue::Object(StateObject {
                    type_id: target_type,
                    version: 1,
                    fields: BTreeMap::new(),
                }),
            )
            .unwrap();
        registry
            .insert(
                link_id,
                StateValue::Object(StateObject {
                    type_id: link_type,
                    version: 1,
                    fields: BTreeMap::from([(next_field, StateValue::Handle(target))]),
                }),
            )
            .unwrap();
        let handle_type = nexa_bytecode::state_handle_type(ValueType::Named(target_type));
        let state_schema = StateSchema {
            types: vec![
                StateType {
                    stable_id: target_type,
                    version: 1,
                    fields: Vec::new(),
                },
                StateType {
                    stable_id: link_type,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: next_field,
                        ty: ValueType::Named(handle_type),
                    }],
                },
            ],
        };
        registry.validate_schema(&state_schema).unwrap();

        let mut wrong_schema = state_schema.clone();
        wrong_schema.types[1].fields[0].ty = ValueType::Named(nexa_bytecode::state_handle_type(
            ValueType::Named(link_type),
        ));
        assert!(registry.validate_schema(&wrong_schema).is_err());

        let mut migration =
            MigrationContext::new(registry, domain, state_schema, false, limits()).unwrap();
        let old_link = migration
            .old_get(link_id, ValueType::Named(link_type))
            .unwrap();
        let old_value = migration
            .old_field_get(old_link, next_field, ValueType::Named(handle_type))
            .unwrap();
        assert!(matches!(
            old_value,
            RuntimeValue::StateHandle {
                handle_type: actual,
                ..
            } if actual == handle_type
        ));
        migration.preserve(target_id).unwrap();
        let replacement = migration
            .new_create(StableId::from_name("replacement"), link_type)
            .unwrap();
        migration
            .new_set(replacement, next_field, old_value)
            .unwrap();
    }

    fn context(schema: StateSchema, limits: MigrationLimits) -> MigrationContext {
        MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            schema,
            false,
            limits,
        )
        .unwrap()
    }

    fn run_migration(
        module: &nexa_verifier::VerifiedModule,
        fuel: u64,
        max_call_depth: u32,
    ) -> Result<InterpreterOutcome, InterpreterError> {
        let mut migration = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            StateSchema { types: Vec::new() },
            true,
            limits(),
        )
        .unwrap();
        CheckedInterpreter::run_migration(
            module,
            0,
            &[],
            fuel,
            FrameLimits {
                max_call_depth,
                ..FrameLimits::default()
            },
            &mut migration,
        )
    }

    fn fuel_module(instructions: &[Instruction]) -> nexa_verifier::VerifiedModule {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        function.effect(FunctionEffect::Migration);
        for instruction in instructions {
            function.emit(*instruction);
        }
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn call_depth_module(depth: u32) -> nexa_verifier::VerifiedModule {
        let mut module = ModuleBuilder::new();
        for function_id in 0..depth {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: None,
                },
                0,
            );
            if function_id == 0 {
                function.effect(FunctionEffect::Migration);
            }
            if function_id + 1 < depth {
                function.emit(Instruction::Call {
                    function: function_id + 1,
                    args_base: 0,
                    args_count: 0,
                    dst: 0,
                });
            }
            function.emit(Instruction::ReturnVoid);
            module.function(function.finish().unwrap());
        }
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn heapless_scalar_helper_migration_module() -> nexa_verifier::VerifiedModule {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);

        let mut scalar_helper = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::F32),
            },
            2,
        );
        scalar_helper
            .emit(Instruction::LoadF32 {
                dst: 0,
                bits: 3.75_f32.to_bits(),
            })
            .emit(Instruction::StandardIntrinsic {
                intrinsic: StandardIntrinsic::F32Floor,
                args_base: 0,
                args_count: 1,
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });

        let mut module = ModuleBuilder::new();
        module.function(migration.finish().unwrap());
        module.function(scalar_helper.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn generation_context(old_generation: u32) -> (MigrationContext, StableId, RuntimeValue) {
        let type_id = StableId::from_name("GenerationLimit");
        let old_id = StableId::from_name("GenerationLimit::old");
        let target_id = StableId::from_name("GenerationLimit::target");
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        old.objects.push(MigrationObjectSlot {
            stable_id: old_id,
            type_id: StableId(0),
            version: 0,
            generation: old_generation,
            field_start: 0,
            field_len: 0,
            scalar: Some(StateValue::I32(1)),
        });
        old.rebuild_caches();
        let mut migration = MigrationContext::new(
            old,
            StatefulDomainId::new(1),
            schema(type_id, &[]),
            false,
            limits(),
        )
        .unwrap();
        let target = migration.new_create(target_id, type_id).unwrap();
        (migration, old_id, target)
    }

    fn verified_object_migration(
        state_schema: &StateSchema,
        type_id: StableId,
        field_id: StableId,
        object_id: StableId,
    ) -> nexa_verifier::VerifiedModule {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            3,
        );
        function
            .effect(FunctionEffect::Migration)
            .set_root(0)
            .unwrap()
            .set_root(2)
            .unwrap()
            .emit(Instruction::StateOldGet {
                stable_id: object_id,
                ty: ValueType::Named(type_id),
                dst: 0,
            })
            .emit(Instruction::StateOldFieldGet {
                object: 0,
                field_id,
                ty: ValueType::I32,
                dst: 1,
            })
            .emit(Instruction::StateNewCreate {
                stable_id: object_id,
                type_id,
                dst: 2,
            })
            .emit(Instruction::StateNewSet {
                object: 2,
                field_id,
                source: 1,
            })
            .emit(Instruction::StateReplace {
                old_id: object_id,
                target: 2,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            nexa_bytecode::RootMap {
                pc: 0,
                bitmap: vec![false, false, false],
            },
            nexa_bytecode::RootMap {
                pc: 6,
                bitmap: vec![true, false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.state_schema(state_schema.clone());
        module.function(function);
        module.reload_entries(Some(0), None);
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    #[test]
    fn verified_migration_bytecode_executes_the_complete_object_staging_protocol() {
        let domain = StatefulDomainId::new(1);
        let type_id = StableId::from_name("VerifiedMigration");
        let field_id = StableId::from_name("VerifiedMigration::value");
        let object_id = StableId::from_name("VerifiedMigration::root");
        let state_schema = schema(type_id, &[(field_id, ValueType::I32)]);
        let mut old = StatefulRegistry::new(domain);
        let old_handle = old
            .insert(
                object_id,
                StateValue::Object(StateObject {
                    type_id,
                    version: 1,
                    fields: BTreeMap::from([(field_id, StateValue::I32(17))]),
                }),
            )
            .unwrap();

        let module = verified_object_migration(&state_schema, type_id, field_id, object_id);
        let mut migration =
            MigrationContext::new(old, domain, state_schema, false, limits()).unwrap();

        assert!(matches!(
            CheckedInterpreter::run_migration(
                &module,
                0,
                &[],
                limits().max_fuel,
                FrameLimits {
                    max_call_depth: u32::from(limits().max_call_depth),
                    ..FrameLimits::default()
                },
                &mut migration,
            )
            .unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        let MigrationOutput::Owned {
            registry, usage, ..
        } = migration.finish().unwrap()
        else {
            panic!("the complete migration protocol must produce owned staging state");
        };
        assert_eq!(usage.objects_read, 2);
        assert_eq!(usage.objects_created, 1);
        assert_eq!(usage.fields_written, 1);
        assert_eq!(usage.replaced, 1);
        assert_eq!(
            registry.resolve(old_handle),
            Err(StatefulError::StaleGeneration)
        );
        let replacement = registry
            .objects
            .iter()
            .find(|object| object.stable_id == object_id)
            .expect("replacement object");
        assert_eq!(replacement.generation, old_handle.generation() + 1);
        let fields = registry.object_fields(replacement);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_id, field_id);
        assert_eq!(fields[0].value, StateValue::I32(17));
    }

    #[test]
    fn rejected_migration_object_operands_do_not_mutate_staging_state() {
        let domain = StatefulDomainId::new(1);
        let type_id = StableId::from_name("OperandValidation");
        let other_type = StableId::from_name("OperandValidationOther");
        let field_id = StableId::from_name("OperandValidation::value");
        let old_id = StableId::from_name("OperandValidation::old");
        let target_id = StableId::from_name("OperandValidation::target");
        let missing_id = StableId::from_name("OperandValidation::missing");
        let state_schema = StateSchema {
            types: vec![
                StateType {
                    stable_id: type_id,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: field_id,
                        ty: ValueType::I32,
                    }],
                },
                StateType {
                    stable_id: other_type,
                    version: 1,
                    fields: Vec::new(),
                },
            ],
        };
        let mut old = StatefulRegistry::new(domain);
        old.insert(
            old_id,
            StateValue::Object(StateObject {
                type_id,
                version: 1,
                fields: BTreeMap::from([(field_id, StateValue::I32(9))]),
            }),
        )
        .unwrap();
        let mut migration =
            MigrationContext::new(old, domain, state_schema, false, limits()).unwrap();
        let old_object = migration
            .old_get(old_id, ValueType::Named(type_id))
            .unwrap();
        let target = migration.new_create(target_id, type_id).unwrap();
        let usage = migration.arena.usage();
        let report = migration.usage_report();
        let forwarding_len = migration.arena.forwarding.len();
        let target_generation = migration.arena.objects[0].generation;

        assert!(
            migration
                .new_set(
                    RuntimeValue::Opaque {
                        type_id,
                        value: missing_id.0,
                    },
                    field_id,
                    RuntimeValue::I32(1),
                )
                .is_err(),
            "an object not produced in the staging arena must be rejected"
        );
        assert!(
            migration
                .new_set(
                    RuntimeValue::Opaque {
                        type_id: other_type,
                        value: target_id.0,
                    },
                    field_id,
                    RuntimeValue::I32(1),
                )
                .is_err(),
            "a forged nominal owner must be rejected"
        );
        assert!(
            migration
                .new_set(target, field_id, RuntimeValue::Bool(true))
                .is_err(),
            "the candidate field value must have its declared type"
        );
        assert!(
            migration
                .old_field_get(old_object, field_id, ValueType::Bool)
                .is_err(),
            "the old field result type must match the stored field"
        );
        assert!(
            migration
                .replace(
                    old_id,
                    RuntimeValue::Opaque {
                        type_id: other_type,
                        value: target_id.0,
                    },
                )
                .is_err(),
            "replace must reject a forged target nominal"
        );

        assert_eq!(migration.arena.usage(), usage);
        assert_eq!(migration.usage_report(), report);
        assert_eq!(migration.arena.forwarding.len(), forwarding_len);
        assert_eq!(migration.arena.objects[0].generation, target_generation);
        assert!(migration.arena.fields.is_empty());
    }

    #[test]
    fn state_handles_are_generation_and_domain_checked() {
        let mut registry = StatefulRegistry::new(StatefulDomainId::new(1));
        let id = StableId::from_name("score");
        let old = registry.insert(id, StateValue::I32(1)).unwrap();
        let new = registry.insert(id, StateValue::I32(2)).unwrap();
        assert_ne!(old, new);
        assert_eq!(registry.resolve(old), Err(StatefulError::StaleGeneration));
        assert_eq!(registry.resolve(new), Ok(StateValue::I32(2)));
        let wrong_domain = StateHandle {
            domain: StatefulDomainId::new(2),
            ..new
        };
        assert!(matches!(
            registry.checked_runtime_handle_slot(wrong_domain),
            Err(StateHandleError::WrongDomain)
        ));
        let missing = StateHandle {
            stable_id: StableId::from_name("missing"),
            ..new
        };
        assert!(matches!(
            registry.checked_runtime_handle_slot(missing),
            Err(StateHandleError::Missing)
        ));
        assert!(matches!(
            registry.checked_runtime_handle_slot(old),
            Err(StateHandleError::StaleGeneration)
        ));

        registry.objects[0].generation = u32::MAX;
        assert_eq!(
            registry.insert(id, StateValue::I32(3)),
            Err(StatefulError::GenerationExhausted)
        );
        assert_eq!(new.stable_id(), id);
        assert_eq!(new.generation(), 1);
        assert_eq!(new, new);
        assert_eq!(new.deterministic_hash(), new.deterministic_hash());
    }

    #[test]
    fn stateful_registry_keeps_its_configured_slot_bounds() {
        let mut registry = StatefulRegistry::try_new(
            StatefulDomainId::new(1),
            MigrationLimits {
                max_objects: 1,
                max_fields: 1,
                max_state_bytes: 128,
                max_gc_roots: 1,
                ..limits()
            },
        )
        .unwrap();
        let first = StableId::from_name("RegistryLimit::first");
        let second = StableId::from_name("RegistryLimit::second");
        registry.insert(first, StateValue::I32(1)).unwrap();
        assert_eq!(
            registry.insert(second, StateValue::I32(2)),
            Err(StatefulError::Capacity(MigrationLimitError::Objects))
        );
        assert_eq!(registry.object_count(), 1);
        assert_eq!(registry.objects.capacity(), 1);
    }

    #[test]
    fn migration_object_handles_reject_kind_context_and_stale_generation_confusion() {
        let domain = StatefulDomainId::new(7);
        let type_id = StableId::from_name("ProvenanceState");
        let field_id = StableId::from_name("ProvenanceState::value");
        let shared_id = StableId::from_name("ProvenanceState::shared");
        let state_schema = schema(type_id, &[(field_id, ValueType::I32)]);
        let mut old = StatefulRegistry::new(domain);
        old.insert(
            shared_id,
            StateValue::Object(StateObject {
                type_id,
                version: 1,
                fields: BTreeMap::from([(field_id, StateValue::I32(3))]),
            }),
        )
        .unwrap();
        let old = Arc::new(old);
        let mut first = MigrationContext::new(
            Arc::clone(&old),
            domain,
            state_schema.clone(),
            false,
            limits(),
        )
        .unwrap();
        let mut second = MigrationContext::new(old, domain, state_schema, false, limits()).unwrap();

        let old_object = first.old_get(shared_id, ValueType::Named(type_id)).unwrap();
        let staging_object = first.new_create(shared_id, type_id).unwrap();

        assert!(
            first
                .old_field_get(staging_object, field_id, ValueType::I32)
                .is_err(),
            "an equal stable ID must not turn a staging handle into an old-state handle"
        );
        assert!(
            first
                .new_set(old_object, field_id, RuntimeValue::I32(4))
                .is_err(),
            "an equal stable ID must not turn an old-state handle into a staging handle"
        );
        assert!(
            second
                .old_field_get(old_object, field_id, ValueType::I32)
                .is_err(),
            "old-state handles must be scoped to their MigrationContext"
        );
        assert!(
            second
                .new_set(staging_object, field_id, RuntimeValue::I32(4))
                .is_err(),
            "staging handles must be scoped to their MigrationContext"
        );
        assert!(
            second.replace(shared_id, staging_object).is_err(),
            "replace must reject a target from another MigrationContext"
        );

        first
            .new_set(staging_object, field_id, RuntimeValue::I32(5))
            .unwrap();
        first.replace(shared_id, staging_object).unwrap();
        assert!(
            first
                .new_set(staging_object, field_id, RuntimeValue::I32(6))
                .is_err(),
            "replace must invalidate the pre-replacement staging generation"
        );
    }

    #[test]
    fn every_migration_opcode_preserves_all_vector_capacities() {
        let type_id = StableId::from_name("CapacityInvariant");
        let field_id = StableId::from_name("CapacityInvariant::field");
        let preserved_id = StableId::from_name("CapacityInvariant::preserved");
        let replaced_id = StableId::from_name("CapacityInvariant::replaced");
        let deleted_id = StableId::from_name("CapacityInvariant::deleted");
        let target_id = StableId::from_name("CapacityInvariant::target");
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        old.insert(
            preserved_id,
            StateValue::Object(StateObject {
                type_id,
                version: 1,
                fields: BTreeMap::from([(field_id, StateValue::I32(1))]),
            }),
        )
        .unwrap();
        old.insert(replaced_id, StateValue::I32(2)).unwrap();
        old.insert(deleted_id, StateValue::I32(3)).unwrap();
        let mut migration = MigrationContext::new(
            old,
            StatefulDomainId::new(1),
            schema(type_id, &[(field_id, ValueType::I32)]),
            false,
            limits(),
        )
        .unwrap();
        let capacities = (
            migration.arena.objects.capacity(),
            migration.arena.fields.capacity(),
            migration.arena.forwarding.capacity(),
            migration.arena.payload.capacity(),
            migration.arena.gc_roots.capacity(),
        );
        let assert_capacities = |migration: &MigrationContext| {
            assert_eq!(
                (
                    migration.arena.objects.capacity(),
                    migration.arena.fields.capacity(),
                    migration.arena.forwarding.capacity(),
                    migration.arena.payload.capacity(),
                    migration.arena.gc_roots.capacity(),
                ),
                capacities
            );
        };

        migration.old_get(replaced_id, ValueType::I32).unwrap();
        assert_capacities(&migration);
        let preserved = migration
            .old_get(preserved_id, ValueType::Named(type_id))
            .unwrap();
        migration
            .old_field_get(preserved, field_id, ValueType::I32)
            .unwrap();
        assert_capacities(&migration);
        let target = migration.new_create(target_id, type_id).unwrap();
        assert_capacities(&migration);
        migration
            .new_set(target, field_id, RuntimeValue::I32(4))
            .unwrap();
        assert_capacities(&migration);
        migration.preserve(preserved_id).unwrap();
        assert_capacities(&migration);
        migration.replace(replaced_id, target).unwrap();
        assert_capacities(&migration);
        migration.delete(deleted_id).unwrap();
        assert_capacities(&migration);
        migration.finish_staging().unwrap();
        assert_capacities(&migration);
    }

    #[test]
    fn usage_report_tracks_peaks_fuel_depth_and_failed_admission() {
        let nested = call_depth_module(3);
        let mut depth_migration = MigrationContext::new(
            StatefulRegistry::new(StatefulDomainId::new(1)),
            StatefulDomainId::new(1),
            StateSchema { types: Vec::new() },
            true,
            limits(),
        )
        .unwrap();
        assert!(matches!(
            CheckedInterpreter::run_migration(
                &nested,
                0,
                &[],
                64,
                FrameLimits {
                    max_call_depth: 3,
                    ..FrameLimits::default()
                },
                &mut depth_migration,
            )
            .unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        let report = depth_migration.usage_report();
        assert_eq!(report.fuel_used, 5);
        assert_eq!(report.max_call_depth_used, 3);

        let type_id = StableId::from_name("UsageReport");
        let ids = [
            StableId::from_name("UsageReport::first"),
            StableId::from_name("UsageReport::second"),
        ];
        let mut capacity_migration = context(
            schema(type_id, &[]),
            MigrationLimits {
                max_objects: 1,
                ..limits()
            },
        );
        capacity_migration.new_create(ids[0], type_id).unwrap();
        assert!(capacity_migration.new_create(ids[1], type_id).is_err());
        assert_eq!(
            capacity_migration.usage_report(),
            MigrationUsageReport {
                objects_created: 1,
                object_peak: 1,
                payload_byte_peak: std::mem::size_of::<StableId>() + std::mem::size_of::<u32>(),
                ..MigrationUsageReport::default()
            }
        );
    }

    #[test]
    fn migration_hash_is_stable_for_one_hundred_identical_runs() {
        fn run_once() -> StableId {
            let type_id = StableId::from_name("MigrationHash");
            let field_id = StableId::from_name("MigrationHash::field");
            let preserved_id = StableId::from_name("MigrationHash::preserved");
            let replaced_id = StableId::from_name("MigrationHash::replaced");
            let deleted_id = StableId::from_name("MigrationHash::deleted");
            let target_id = StableId::from_name("MigrationHash::target");
            let mut old = StatefulRegistry::new(StatefulDomainId::new(7));
            old.insert(
                preserved_id,
                StateValue::Object(StateObject {
                    type_id,
                    version: 1,
                    fields: BTreeMap::from([(field_id, StateValue::I32(9))]),
                }),
            )
            .unwrap();
            old.insert(replaced_id, StateValue::I32(2)).unwrap();
            old.insert(deleted_id, StateValue::Bool(true)).unwrap();
            let mut migration = MigrationContext::new(
                old,
                StatefulDomainId::new(7),
                schema(type_id, &[(field_id, ValueType::I32)]),
                false,
                limits(),
            )
            .unwrap();
            let target = migration.new_create(target_id, type_id).unwrap();
            migration
                .new_set(target, field_id, RuntimeValue::I32(11))
                .unwrap();
            let before_forwarding = migration.operation_hash.value;
            migration.preserve(preserved_id).unwrap();
            let after_preserve = migration.operation_hash.value;
            migration.replace(replaced_id, target).unwrap();
            let after_replace = migration.operation_hash.value;
            migration.delete(deleted_id).unwrap();
            let after_delete = migration.operation_hash.value;
            assert_ne!(before_forwarding, after_preserve);
            assert_ne!(after_preserve, after_replace);
            assert_ne!(after_replace, after_delete);
            migration.finish_staging().unwrap();
            let MigrationOutput::Owned { hash, .. } = migration.finish().unwrap() else {
                panic!("the migrated graph is owned");
            };
            hash
        }

        let expected = run_once();
        for _ in 1..100 {
            assert_eq!(run_once(), expected);
        }
    }

    #[test]
    fn object_capacity_accepts_limit_minus_one_and_limit_then_rejects_limit_plus_one() {
        let type_id = StableId::from_name("ObjectLimit");
        let ids = [
            StableId::from_name("ObjectLimit::one"),
            StableId::from_name("ObjectLimit::two"),
            StableId::from_name("ObjectLimit::three"),
        ];
        let mut migration = context(
            schema(type_id, &[]),
            MigrationLimits {
                max_objects: 2,
                ..limits()
            },
        );
        migration.new_create(ids[0], type_id).unwrap();
        assert_eq!(migration.arena.usage().objects, 1);
        migration.new_create(ids[1], type_id).unwrap();
        assert_eq!(migration.arena.usage().objects, 2);
        let before = migration.arena.usage();
        assert!(migration.new_create(ids[2], type_id).is_err());
        assert_eq!(migration.limit_error(), Some(MigrationLimitError::Objects));
        assert_eq!(migration.arena.usage(), before);
        assert!(migration.arena.object_index(ids[2]).is_err());
    }

    #[test]
    fn field_capacity_accepts_limit_minus_one_and_limit_then_rejects_limit_plus_one() {
        let type_id = StableId::from_name("FieldLimit");
        let object_id = StableId::from_name("FieldLimit::object");
        let fields = [
            StableId::from_name("FieldLimit::one"),
            StableId::from_name("FieldLimit::two"),
            StableId::from_name("FieldLimit::three"),
        ];
        let mut migration = context(
            schema(type_id, &fields.map(|field| (field, ValueType::I32))),
            MigrationLimits {
                max_fields: 2,
                ..limits()
            },
        );
        let object = migration.new_create(object_id, type_id).unwrap();
        migration
            .new_set(object, fields[0], RuntimeValue::I32(1))
            .unwrap();
        assert_eq!(migration.arena.usage().fields, 1);
        migration
            .new_set(object, fields[1], RuntimeValue::I32(2))
            .unwrap();
        assert_eq!(migration.arena.usage().fields, 2);
        let before = migration.arena.usage();
        assert!(
            migration
                .new_set(object, fields[2], RuntimeValue::I32(3))
                .is_err()
        );
        assert_eq!(migration.limit_error(), Some(MigrationLimitError::Fields));
        assert_eq!(migration.arena.usage(), before);
    }

    #[test]
    fn forwarding_capacity_accepts_limit_minus_one_and_limit_then_rejects_limit_plus_one() {
        let ids = [
            StableId::from_name("Forwarding::one"),
            StableId::from_name("Forwarding::two"),
            StableId::from_name("Forwarding::three"),
        ];
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        for (value, id) in [0, 1, 2].into_iter().zip(ids) {
            old.insert(id, StateValue::I32(value)).unwrap();
        }
        let mut migration = MigrationContext::new(
            old,
            StatefulDomainId::new(1),
            StateSchema { types: Vec::new() },
            false,
            MigrationLimits {
                max_forwarding_entries: 2,
                ..limits()
            },
        )
        .unwrap();
        migration.delete(ids[0]).unwrap();
        assert_eq!(migration.arena.forwarding.len(), 1);
        migration.delete(ids[1]).unwrap();
        assert_eq!(migration.arena.forwarding.len(), 2);
        let before = migration.arena.usage();
        assert!(migration.delete(ids[2]).is_err());
        assert_eq!(
            migration.limit_error(),
            Some(MigrationLimitError::Forwarding)
        );
        assert_eq!(migration.arena.usage(), before);
        assert_eq!(migration.arena.forwarding.len(), 2);
    }

    #[test]
    fn payload_capacity_is_an_exact_preallocated_boundary() {
        let type_id = StableId::from_name("PayloadLimit");
        let object_id = StableId::from_name("PayloadLimit::object");
        let fields = [
            StableId::from_name("PayloadLimit::one"),
            StableId::from_name("PayloadLimit::two"),
            StableId::from_name("PayloadLimit::three"),
        ];
        let object_bytes = std::mem::size_of::<StableId>() + std::mem::size_of::<u32>();
        let field_bytes = std::mem::size_of::<StableId>() + std::mem::size_of::<i32>();
        let exact = object_bytes + field_bytes * 2;
        let mut migration = context(
            schema(type_id, &fields.map(|field| (field, ValueType::I32))),
            MigrationLimits {
                max_state_bytes: exact,
                ..limits()
            },
        );
        let object = migration.new_create(object_id, type_id).unwrap();
        migration
            .new_set(object, fields[0], RuntimeValue::I32(1))
            .unwrap();
        assert_eq!(migration.arena.payload.len(), exact - field_bytes);
        migration
            .new_set(object, fields[1], RuntimeValue::I32(2))
            .unwrap();
        assert_eq!(migration.arena.payload.len(), exact);
        let before = migration.arena.usage();
        assert!(
            migration
                .new_set(object, fields[2], RuntimeValue::I32(3))
                .is_err()
        );
        assert_eq!(
            migration.limit_error(),
            Some(MigrationLimitError::StateBytes)
        );
        assert_eq!(migration.arena.usage(), before);
    }

    #[test]
    fn gc_root_capacity_accepts_limit_minus_one_and_limit_then_rejects_limit_plus_one() {
        let type_id = StableId::from_name("RootLimit");
        let object_id = StableId::from_name("RootLimit::object");
        let fields = [
            StableId::from_name("RootLimit::one"),
            StableId::from_name("RootLimit::two"),
            StableId::from_name("RootLimit::three"),
        ];
        let mut migration = context(
            schema(type_id, &fields.map(|field| (field, ValueType::Ref))),
            MigrationLimits {
                max_gc_roots: 2,
                ..limits()
            },
        );
        let object = migration.new_create(object_id, type_id).unwrap();
        let reference = RuntimeValue::Ref(GcRef {
            index: 1,
            generation: 1,
        });
        migration.new_set(object, fields[0], reference).unwrap();
        assert_eq!(migration.arena.usage().gc_roots, 1);
        migration.new_set(object, fields[1], reference).unwrap();
        assert_eq!(migration.arena.usage().gc_roots, 2);
        let before = migration.arena.usage();
        assert!(migration.new_set(object, fields[2], reference).is_err());
        assert_eq!(migration.limit_error(), Some(MigrationLimitError::GcRoots));
        assert_eq!(migration.arena.usage(), before);
    }

    #[test]
    fn generation_overflow_is_rejected_before_forwarding_or_target_mutation() {
        let (mut below, below_old, below_target) = generation_context(u32::MAX - 2);
        below.replace(below_old, below_target).unwrap();
        assert_eq!(below.arena.objects[0].generation, u32::MAX - 1);

        let (mut exact, exact_old, exact_target) = generation_context(u32::MAX - 1);
        exact.replace(exact_old, exact_target).unwrap();
        assert_eq!(exact.arena.objects[0].generation, u32::MAX);

        let (mut migration, old_id, target) = generation_context(u32::MAX);
        let target_generation = migration.arena.objects[0].generation;
        assert!(migration.replace(old_id, target).is_err());
        assert_eq!(
            migration.limit_error(),
            Some(MigrationLimitError::Generation)
        );
        assert!(migration.arena.forwarding.is_empty());
        assert_eq!(migration.arena.objects[0].generation, target_generation);
    }

    #[test]
    fn preserve_is_atomic_and_finish_moves_arena_storage_without_rebuilding() {
        let type_id = StableId::from_name("Preserve");
        let object_id = StableId::from_name("Preserve::object");
        let field_id = StableId::from_name("Preserve::field");
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        let old_handle = old
            .insert(
                object_id,
                StateValue::Object(StateObject {
                    type_id,
                    version: 1,
                    fields: BTreeMap::from([(field_id, StateValue::I32(7))]),
                }),
            )
            .unwrap();
        let mut migration = MigrationContext::new(
            old,
            StatefulDomainId::new(1),
            schema(type_id, &[(field_id, ValueType::I32)]),
            false,
            limits(),
        )
        .unwrap();
        migration.preserve(object_id).unwrap();
        migration.finish_staging().unwrap();
        let objects_pointer = migration.arena.objects.as_ptr();
        let fields_pointer = migration.arena.fields.as_ptr();
        let payload_pointer = migration.arena.payload.as_ptr();
        let MigrationOutput::Owned { registry, .. } = migration.finish().unwrap() else {
            panic!("a touched migration owns its completed registry");
        };
        assert_eq!(registry.objects.as_ptr(), objects_pointer);
        assert_eq!(registry.fields.as_ptr(), fields_pointer);
        assert_eq!(registry.payload.as_ptr(), payload_pointer);
        assert_eq!(
            registry.resolve(old_handle).unwrap(),
            StateValue::Object(StateObject {
                type_id,
                version: 1,
                fields: BTreeMap::from([(field_id, StateValue::I32(7))]),
            })
        );
    }

    #[test]
    fn capacity_report_accounts_for_every_reserved_metadata_slot() {
        let limits = MigrationLimits {
            max_objects: 2,
            max_fields: 3,
            max_forwarding_entries: 4,
            max_state_bytes: 5,
            max_gc_roots: 6,
            max_fuel: 7,
            max_call_depth: 8,
        };
        let report = limits.capacity_report();
        assert_eq!(report.object_capacity, 2);
        assert_eq!(report.field_capacity, 3);
        assert_eq!(report.forwarding_capacity, 4);
        assert_eq!(report.payload_byte_capacity, 5);
        assert_eq!(
            report.metadata_bytes,
            2 * std::mem::size_of::<MigrationObjectSlot>()
                + 3 * std::mem::size_of::<MigrationFieldSlot>()
                + 4 * std::mem::size_of::<ForwardingSlot>()
        );
    }

    #[test]
    fn fuel_limit_accepts_smaller_and_exact_workloads_then_rejects_limit_plus_one() {
        let one = fuel_module(&[Instruction::ReturnVoid]);
        let two = fuel_module(&[Instruction::StateFinish, Instruction::ReturnVoid]);
        let three = fuel_module(&[
            Instruction::LoadI32 { dst: 0, value: 1 },
            Instruction::StateFinish,
            Instruction::ReturnVoid,
        ]);
        assert!(matches!(
            run_migration(&one, 2, 8).unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        assert!(matches!(
            run_migration(&two, 2, 8).unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        assert!(matches!(
            run_migration(&three, 2, 8).unwrap(),
            InterpreterOutcome::Suspended {
                reason: SuspendReason::Fuel,
                ..
            }
        ));
    }

    #[test]
    fn call_depth_limit_accepts_smaller_and_exact_depth_then_rejects_limit_plus_one() {
        assert!(matches!(
            run_migration(&call_depth_module(1), 16, 2).unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        assert!(matches!(
            run_migration(&call_depth_module(2), 16, 2).unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
        assert!(matches!(
            run_migration(&call_depth_module(3), 16, 2),
            Err(InterpreterError::ContinuationLimit(
                FrameError::CallDepthLimit
            ))
        ));
    }

    #[test]
    fn verified_heapless_migration_helper_executes_without_heap_unavailable() {
        let result = run_migration(&heapless_scalar_helper_migration_module(), 16, 8);
        assert!(
            !matches!(result, Err(InterpreterError::HeapUnavailable)),
            "a verified heapless migration must never reach a missing Heap"
        );
        assert!(matches!(
            result.unwrap(),
            InterpreterOutcome::Returned { .. }
        ));
    }
}
