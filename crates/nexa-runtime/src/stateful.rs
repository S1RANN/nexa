use std::collections::BTreeMap;
use std::fmt;

use nexa_core::StableId;

use crate::allocation::{AllocationBoundary, MigrationAllocationPhase, observe_migration};
use crate::interpreter::InterpreterMigration;
use crate::reload::ReloadError;
use crate::{GcRef, RuntimeValue};

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

#[derive(Clone, Debug)]
pub(crate) struct StatefulRegistry {
    domain: StatefulDomainId,
    objects: Vec<MigrationObjectSlot>,
    fields: Vec<MigrationFieldSlot>,
    payload: Vec<u8>,
    gc_roots: Vec<GcRef>,
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

impl StatefulRegistry {
    #[must_use]
    pub const fn new(domain: StatefulDomainId) -> Self {
        Self {
            domain,
            objects: Vec::new(),
            fields: Vec::new(),
            payload: Vec::new(),
            gc_roots: Vec::new(),
        }
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
            if !state_value_matches(field.value.clone(), schema_field.ty) {
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
        let (type_id, version, scalar, fields) = match value {
            StateValue::Object(object) => (
                object.type_id,
                object.version,
                None,
                object.fields.into_iter().collect::<Vec<_>>(),
            ),
            scalar => (StableId(0), 0, Some(scalar), Vec::new()),
        };
        let field_len = u32::try_from(fields.len()).expect("state field count fits u32");
        for (offset, (field_id, value)) in fields.into_iter().enumerate() {
            self.fields
                .insert(field_start + offset, MigrationFieldSlot { field_id, value });
        }
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

fn state_value_matches(value: StateValue, expected: nexa_bytecode::ValueType) -> bool {
    matches!(
        (value, expected),
        (StateValue::I32(_), nexa_bytecode::ValueType::I32)
            | (StateValue::Bool(_), nexa_bytecode::ValueType::Bool)
            | (StateValue::Ref(_), nexa_bytecode::ValueType::Ref)
            | (StateValue::Handle(_), nexa_bytecode::ValueType::Named(_))
    )
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
    }

    fn into_registry(self, domain: StatefulDomainId) -> StatefulRegistry {
        StatefulRegistry {
            domain,
            objects: self.objects,
            fields: self.fields,
            payload: self.payload,
            gc_roots: self.gc_roots,
        }
    }
}

fn reserve<T>(capacity: usize, error: MigrationLimitError) -> Result<Vec<T>, MigrationLimitError> {
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).map_err(|_| error)?;
    Ok(values)
}

pub(crate) struct MigrationContext {
    old: StatefulRegistry,
    arena: MigrationArena,
    domain: StatefulDomainId,
    schema: nexa_bytecode::StateSchema,
    flags: u8,
    invalid: Option<ReloadError>,
}

const SCHEMA_UNCHANGED: u8 = 1 << 0;
const TOUCHED: u8 = 1 << 1;
const FINALIZED: u8 = 1 << 2;
const OBSERVED_FIRST_OPCODE: u8 = 1 << 3;

impl MigrationContext {
    pub(crate) fn new(
        old: StatefulRegistry,
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
        Ok(Self {
            old,
            arena,
            domain,
            schema,
            flags: u8::from(schema_unchanged) * SCHEMA_UNCHANGED,
            invalid: None,
        })
    }

    pub(crate) fn limit_error(&self) -> Option<MigrationLimitError> {
        match self.invalid {
            Some(ReloadError::MigrationLimit(error)) => Some(error),
            _ => None,
        }
    }

    fn reject_limit<T>(&mut self, error: MigrationLimitError) -> Result<T, String> {
        self.invalid = Some(ReloadError::MigrationLimit(error));
        Err(String::new())
    }

    fn precheck(&mut self, usage: MigrationUsage) -> Result<(), String> {
        self.arena.check_usage(usage).map_err(|error| {
            self.invalid = Some(ReloadError::MigrationLimit(error));
            String::new()
        })
    }

    fn precheck_forwarding(&mut self) -> Result<(), String> {
        self.arena.check_forwarding().map_err(|error| {
            self.invalid = Some(ReloadError::MigrationLimit(error));
            String::new()
        })
    }

    pub(crate) fn finish(self) -> Result<StatefulRegistry, ReloadError> {
        let _observation = MigrationObservation::new(MigrationAllocationPhase::Finish, false);
        self.finish_inner()
    }

    fn finish_inner(mut self) -> Result<StatefulRegistry, ReloadError> {
        if !self.has_flag(TOUCHED) {
            if self.has_flag(SCHEMA_UNCHANGED) {
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
        Ok(registry)
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
            if let Some(mut value) = self.arena.objects[index].scalar.clone() {
                self.remap_value(&mut value);
                self.arena.objects[index].scalar = Some(value);
            }
        }
        for index in 0..self.arena.fields.len() {
            let mut value = self.arena.fields[index].value.clone();
            self.remap_value(&mut value);
            self.arena.fields[index].value = value;
        }
    }

    fn remap_value(&self, value: &mut StateValue) {
        let StateValue::Handle(handle) = value else {
            return;
        };
        let Ok(index) = self.arena.forwarding_index(handle.stable_id) else {
            return;
        };
        let Some(target) = self.arena.forwarding[index].target else {
            return;
        };
        if let Ok(target_index) = self.arena.object_index(target) {
            handle.stable_id = target;
            handle.generation = self.arena.objects[target_index].generation;
        }
    }

    fn old_slot(&self, stable_id: StableId) -> Result<&MigrationObjectSlot, String> {
        self.old
            .object(stable_id)
            .map_err(|_| "old state does not exist".into())
    }
}

impl InterpreterMigration for MigrationContext {
    fn old_get(
        &mut self,
        stable_id: StableId,
        expected: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, String> {
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
    ) -> Result<RuntimeValue, String> {
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
            .map_err(|_| "old state field does not exist".to_string())?;
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
    ) -> Result<RuntimeValue, String> {
        let _observation = self.observe_opcode(MigrationAllocationPhase::NewCreate);
        self.ensure_open()?;
        let version = self
            .schema
            .types
            .iter()
            .find(|state_type| state_type.stable_id == type_id)
            .map(|state_type| state_type.version)
            .ok_or_else(|| "candidate state type does not exist".to_string())?;
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
    ) -> Result<(), String> {
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
            .map_err(|_| "staging object does not exist".to_string())?;
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
            .ok_or_else(|| "candidate state field does not exist".to_string())?;
        if !state_value_matches(value.clone(), expected) {
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

    fn preserve(&mut self, stable_id: StableId) -> Result<(), String> {
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
            .map_err(|_| "STATE_PRESERVE source does not exist".to_string())?;
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
        let old_slot = self.old.objects[old_index].clone();
        let old_fields_start = old_slot.field_start as usize;
        let old_fields_len = old_slot.field_len as usize;
        self.arena.insert_object(
            object_index,
            old_slot.stable_id,
            old_slot.type_id,
            old_slot.version,
            old_slot.generation,
            old_slot.scalar,
        );
        for offset in 0..old_fields_len {
            self.arena.insert_field(
                object_index,
                self.arena.objects[object_index].field_start as usize + offset,
                self.old.fields[old_fields_start + offset].clone(),
            );
        }
        self.arena
            .insert_forwarding(forwarding_index, stable_id, Some(stable_id));
        self.arena.rebuild_caches();
        self.set_flag(TOUCHED);
        Ok(())
    }

    fn replace(&mut self, old_id: StableId, target: RuntimeValue) -> Result<(), String> {
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
            .map_err(|_| "remap target does not exist".to_string())?;
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
        self.set_flag(TOUCHED);
        Ok(())
    }

    fn delete(&mut self, stable_id: StableId) -> Result<(), String> {
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
        self.set_flag(TOUCHED);
        Ok(())
    }

    fn finish_staging(&mut self) -> Result<(), String> {
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
    fn ensure_open(&self) -> Result<(), String> {
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
) -> Result<StateValue, String> {
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
                .map_err(|_| "state handle target does not exist".to_string())?;
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
        let type_id = StableId::from_name("GenerationLimit");
        let old_id = StableId::from_name("GenerationLimit::old");
        let target_id = StableId::from_name("GenerationLimit::target");
        let mut old = StatefulRegistry::new(StatefulDomainId::new(1));
        old.objects.push(MigrationObjectSlot {
            stable_id: old_id,
            type_id: StableId(0),
            version: 0,
            generation: u32::MAX,
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
        let registry = migration.finish().unwrap();
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
