use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use nexa_core::StableId;

use crate::allocation::{AllocationBoundary, MigrationAllocationPhase, observe_migration};
use crate::interpreter::InterpreterMigration;
use crate::reload::ReloadError;
use crate::{GcRef, RuntimeMessage, RuntimeValue};

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
pub(crate) struct StatefulRegistry {
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
    pub object_peak: usize,
    pub field_peak: usize,
    pub forwarding_peak: usize,
    pub payload_byte_peak: usize,
    pub gc_root_peak: usize,
    pub fuel_used: u64,
    pub max_call_depth_used: u16,
}

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
}

impl fmt::Display for StatefulError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StatefulError {}

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
            if !state_value_matches(&field.value, schema_field.ty) {
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
        StateValue::Bool(_) => 1,
        StateValue::Ref(_) => std::mem::size_of::<GcRef>(),
        StateValue::Handle(_) => std::mem::size_of::<StateHandle>(),
        StateValue::Object(_) => usize::MAX,
    }
}

fn state_value_root_count(value: &StateValue) -> usize {
    usize::from(matches!(value, StateValue::Ref(_)))
}

fn push_root(value: &StateValue, roots: &mut Vec<GcRef>) {
    if let StateValue::Ref(reference) = value {
        roots.push(*reference);
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

fn state_value_matches(value: &StateValue, expected: nexa_bytecode::ValueType) -> bool {
    matches!(
        (value, expected),
        (&StateValue::I32(_), nexa_bytecode::ValueType::I32)
            | (&StateValue::Bool(_), nexa_bytecode::ValueType::Bool)
            | (&StateValue::Ref(_), nexa_bytecode::ValueType::Ref)
            | (&StateValue::Handle(_), nexa_bytecode::ValueType::Named(_))
    )
}

fn clone_leaf_value(value: &StateValue) -> StateValue {
    match value {
        StateValue::I32(value) => StateValue::I32(*value),
        StateValue::Bool(value) => StateValue::Bool(*value),
        StateValue::Ref(reference) => StateValue::Ref(*reference),
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
    hash.write_u64(registry.domain.get());
    hash.write_u64(u64::try_from(registry.objects.len()).unwrap_or(u64::MAX));
    for slot in &registry.objects {
        hash.write_u64(slot.stable_id.0);
        hash.write_u64(slot.type_id.0);
        hash.write_u32(slot.version);
        hash.write_u32(slot.generation);
        if let Some(value) = &slot.scalar {
            hash.write_u8(1);
            hash_state_value(&mut hash, value);
        } else {
            hash.write_u8(0);
            let fields = registry.object_fields(slot);
            hash.write_u64(u64::try_from(fields.len()).unwrap_or(u64::MAX));
            for field in fields {
                hash.write_u64(field.field_id.0);
                hash_state_value(&mut hash, &field.value);
            }
        }
    }
    StableId(hash.value)
}

fn hash_state_value(hash: &mut DeterministicMigrationHasher, value: &StateValue) {
    match value {
        StateValue::I32(value) => {
            hash.write_u8(1);
            hash.write(&value.to_le_bytes());
        }
        StateValue::Bool(value) => {
            hash.write_u8(2);
            hash.write_u8(u8::from(*value));
        }
        StateValue::Ref(_) => {
            // GC slot coordinates are deliberately excluded from the stable migration identity.
            hash.write_u8(3);
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
        let registry = self.arena.into_registry(self.domain);
        registry
            .validate_schema(&self.schema)
            .map_err(|_| ReloadError::GraphCheck)?;
        registry
            .validate_handles()
            .map_err(|_| ReloadError::InvalidStateHandle)?;
        let hash = migration_registry_hash(self.operation_hash, &registry);
        Ok(MigrationOutput::Owned { registry, hash })
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
        for index in 0..self.arena.objects.len() {
            let remapped = self.arena.objects[index]
                .scalar
                .as_ref()
                .and_then(|value| self.remapped_handle(value));
            if let Some(handle) = remapped {
                self.arena.objects[index].scalar = Some(StateValue::Handle(handle));
            }
        }
        for index in 0..self.arena.fields.len() {
            let remapped = self.remapped_handle(&self.arena.fields[index].value);
            if let Some(handle) = remapped {
                self.arena.fields[index].value = StateValue::Handle(handle);
            }
        }
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
    },
    Shared {
        registry: Arc<StatefulRegistry>,
        hash: StableId,
    },
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
        let value = slot_to_runtime_value(slot);
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
    ) -> Result<RuntimeValue, RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::OldFieldGet);
        self.ensure_open()?;
        let RuntimeValue::Opaque {
            type_id,
            value: stable_id,
        } = object
        else {
            return Err("STATE_OLD_FIELD_GET requires an old state object".into());
        };
        let slot = self.old_slot(StableId(stable_id))?;
        if slot.scalar.is_some() || slot.type_id != type_id {
            return Err("old state object type mismatch".into());
        }
        let fields = self.old.object_fields(slot);
        let field = fields
            .binary_search_by_key(&field_id, |field| field.field_id)
            .map(|index| &fields[index])
            .map_err(|_| RuntimeMessage::Static("old state field does not exist"))?;
        let value = state_to_runtime_value(field_id, &field.value);
        if runtime_state_type(value) != expected {
            return Err("old state field type mismatch".into());
        }
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
    ) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::NewSet);
        self.ensure_open()?;
        let RuntimeValue::Opaque {
            type_id,
            value: object_id,
        } = object
        else {
            return Err("STATE_NEW_SET requires a staging object".into());
        };
        let value = runtime_to_state_value(value, &self.arena, self.domain)?;
        let object_index = self
            .arena
            .object_index(StableId(object_id))
            .map_err(|_| RuntimeMessage::Static("staging object does not exist"))?;
        let slot = &self.arena.objects[object_index];
        if slot.scalar.is_some() || slot.type_id != type_id {
            return Err("staging object type mismatch".into());
        }
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
        if !state_value_matches(&value, expected) {
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
        Ok(())
    }

    fn replace(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), RuntimeMessage> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::Replace);
        self.ensure_open()?;
        let RuntimeValue::Opaque {
            value: target_id, ..
        } = target
        else {
            return Err("STATE_REPLACE requires a staging object".into());
        };
        let target_id = StableId(target_id);
        let target_index = self
            .arena
            .object_index(target_id)
            .map_err(|_| RuntimeMessage::Static("remap target does not exist"))?;
        if self.arena.objects[target_index].scalar.is_some() {
            return Err("remap target is not an object".into());
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

fn slot_to_runtime_value(slot: &MigrationObjectSlot) -> RuntimeValue {
    slot.scalar.as_ref().map_or(
        RuntimeValue::Opaque {
            type_id: slot.type_id,
            value: slot.stable_id.0,
        },
        |value| state_to_runtime_value(slot.stable_id, value),
    )
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
    arena: &MigrationArena,
    domain: StatefulDomainId,
) -> Result<StateValue, RuntimeMessage> {
    match value {
        RuntimeValue::I32(value) => Ok(StateValue::I32(value)),
        RuntimeValue::Bool(value) => Ok(StateValue::Bool(value)),
        RuntimeValue::Ref(reference) | RuntimeValue::NamedRef { reference, .. } => {
            Ok(StateValue::Ref(reference))
        }
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

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, StateField,
        StateSchema, StateType, ValueType,
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

    #[test]
    fn state_handles_are_generation_and_domain_checked() {
        let mut registry = StatefulRegistry::new(StatefulDomainId::new(1));
        let id = StableId::from_name("score");
        let old = registry.insert(id, StateValue::I32(1)).unwrap();
        let new = registry.insert(id, StateValue::I32(2)).unwrap();
        assert_ne!(old, new);
        assert_eq!(registry.resolve(old), Err(StatefulError::StaleGeneration));
        assert_eq!(registry.resolve(new), Ok(StateValue::I32(2)));
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
}
