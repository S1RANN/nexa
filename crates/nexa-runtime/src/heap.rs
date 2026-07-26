use std::collections::VecDeque;
use std::fmt;

use nexa_core::StableId;

use crate::{RuntimeFailureInjector, RuntimeFailurePoint, RuntimeValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GcRef {
    pub index: u32,
    pub generation: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
// Struct storage stays inline so construction and `with` updates use only the
// preallocated heap slot pool instead of allocating a system-heap side object.
#[allow(clippy::large_enum_variant)]
pub enum Object {
    String(String),
    I32Array(Vec<i32>),
    Map(Vec<(String, GcRef)>),
    Enum {
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    },
    Struct {
        type_id: StableId,
        fields: [RuntimeValue; nexa_bytecode::MAX_STRUCT_FIELDS],
        field_count: u8,
        hash: u64,
    },
    Class {
        type_id: StableId,
        fields: [RuntimeValue; nexa_bytecode::MAX_CLASS_FIELDS],
        field_count: u8,
    },
}

impl Object {
    fn references(&self) -> Vec<GcRef> {
        match self {
            Self::Class {
                fields,
                field_count,
                ..
            } => fields[..usize::from(*field_count)]
                .iter()
                .filter_map(|field| match field {
                    RuntimeValue::String { reference, .. }
                    | RuntimeValue::Struct { reference, .. }
                    | RuntimeValue::Ref(reference)
                    | RuntimeValue::NamedRef { reference, .. } => Some(*reference),
                    _ => None,
                })
                .collect(),
            Self::Map(entries) => entries.iter().map(|(_, value)| *value).collect(),
            Self::Enum { payload, .. } => payload
                .iter()
                .filter_map(|payload| match payload {
                    RuntimeValue::String { reference, .. }
                    | RuntimeValue::Struct { reference, .. }
                    | RuntimeValue::Ref(reference)
                    | RuntimeValue::NamedRef { reference, .. } => Some(*reference),
                    _ => None,
                })
                .collect(),
            Self::Struct {
                fields,
                field_count,
                ..
            } => fields[..usize::from(*field_count)]
                .iter()
                .filter_map(|field| match field {
                    RuntimeValue::String { reference, .. }
                    | RuntimeValue::Struct { reference, .. }
                    | RuntimeValue::Ref(reference)
                    | RuntimeValue::NamedRef { reference, .. } => Some(*reference),
                    _ => None,
                })
                .collect(),
            Self::String(_) | Self::I32Array(_) => Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct ObjectSlot {
    generation: u32,
    marked: bool,
    object: Option<Object>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    CapacityExhausted,
    StringTooLarge { bytes: usize, max_bytes: usize },
    InjectedFailure(RuntimeFailurePoint),
    InvalidReference(GcRef),
}

impl fmt::Display for HeapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HeapError {}

const fn invalid_value_reference() -> HeapError {
    HeapError::InvalidReference(GcRef {
        index: u32::MAX,
        generation: u32::MAX,
    })
}

#[derive(Debug)]
pub(crate) struct HeapReservation {
    remaining: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcRoots {
    pub running_frames: Vec<GcRef>,
    pub suspended_tasks: Vec<GcRef>,
    pub module_globals: Vec<GcRef>,
    pub stateful_registry: Vec<GcRef>,
    pub staging_heap: Vec<GcRef>,
}

impl GcRoots {
    fn iter(&self) -> impl Iterator<Item = GcRef> + '_ {
        self.running_frames
            .iter()
            .chain(&self.suspended_tasks)
            .chain(&self.module_globals)
            .chain(&self.stateful_registry)
            .chain(&self.staging_heap)
            .copied()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionStats {
    pub marked: usize,
    pub reclaimed: usize,
    pub live: usize,
}

/// Safe-Rust stop-the-world mark/sweep heap with generation-protected references.
#[derive(Debug)]
pub struct Heap {
    slots: Vec<ObjectSlot>,
    free: Vec<u32>,
    max_objects: u32,
    max_string_bytes: usize,
    failure_injector: RuntimeFailureInjector,
}

impl Heap {
    #[must_use]
    pub fn new(max_objects: u32) -> Self {
        Self::new_with_string_limit(max_objects, usize::MAX)
    }

    #[must_use]
    pub fn new_with_string_limit(max_objects: u32, max_string_bytes: usize) -> Self {
        Self {
            slots: Vec::with_capacity(max_objects as usize),
            free: Vec::with_capacity(max_objects as usize),
            max_objects,
            max_string_bytes,
            failure_injector: RuntimeFailureInjector::default(),
        }
    }

    pub fn allocate_string(&mut self, value: &str) -> Result<GcRef, HeapError> {
        self.validate_string_length(value.len())?;
        let mut reservation = self.preflight(1)?;
        let value = value.to_owned();
        Ok(self.commit(&mut reservation, Object::String(value)))
    }

    pub fn concat_strings(&mut self, lhs: GcRef, rhs: GcRef) -> Result<GcRef, HeapError> {
        let (lhs_len, rhs_len) = (self.string(lhs)?.len(), self.string(rhs)?.len());
        let length = lhs_len
            .checked_add(rhs_len)
            .ok_or(HeapError::StringTooLarge {
                bytes: usize::MAX,
                max_bytes: self.max_string_bytes,
            })?;
        self.validate_string_length(length)?;
        let mut reservation = self.preflight(1)?;
        let mut value = String::with_capacity(length);
        value.push_str(self.string(lhs)?);
        value.push_str(self.string(rhs)?);
        Ok(self.commit(&mut reservation, Object::String(value)))
    }

    pub fn string(&self, reference: GcRef) -> Result<&str, HeapError> {
        match self.resolve(reference)? {
            Object::String(value) => Ok(value),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn string_rune_at(
        &self,
        reference: GcRef,
        index: usize,
    ) -> Result<Option<char>, HeapError> {
        Ok(self.string(reference)?.chars().nth(index))
    }

    pub fn string_hash(&self, reference: GcRef) -> Result<u64, HeapError> {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in self.string(reference)?.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Ok(hash)
    }

    pub(crate) fn validate_string_length(&self, bytes: usize) -> Result<(), HeapError> {
        if bytes > self.max_string_bytes {
            Err(HeapError::StringTooLarge {
                bytes,
                max_bytes: self.max_string_bytes,
            })
        } else {
            Ok(())
        }
    }

    pub fn allocate(&mut self, object: Object) -> Result<GcRef, HeapError> {
        let mut reservation = self.preflight(1)?;
        Ok(self.commit(&mut reservation, object))
    }

    pub(crate) fn preflight(&mut self, count: usize) -> Result<HeapReservation, HeapError> {
        if self.failure_injector.trigger(RuntimeFailurePoint::HeapSlot) {
            return Err(HeapError::InjectedFailure(RuntimeFailurePoint::HeapSlot));
        }
        let unused = usize::try_from(self.max_objects)
            .unwrap_or(usize::MAX)
            .saturating_sub(self.slots.len());
        if self.free.len().saturating_add(unused) < count {
            return Err(HeapError::CapacityExhausted);
        }
        Ok(HeapReservation { remaining: count })
    }

    pub(crate) fn commit(&mut self, reservation: &mut HeapReservation, object: Object) -> GcRef {
        reservation.remaining = reservation
            .remaining
            .checked_sub(1)
            .expect("heap allocation was preflighted");
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert!(slot.object.is_none());
            slot.object = Some(object);
            return GcRef {
                index,
                generation: slot.generation,
            };
        }
        let index = u32::try_from(self.slots.len()).expect("heap capacity was preflighted");
        debug_assert!(index < self.max_objects);
        self.slots.push(ObjectSlot {
            generation: 0,
            marked: false,
            object: Some(object),
        });
        GcRef {
            index,
            generation: 0,
        }
    }

    pub(crate) const fn reservation_complete(reservation: &HeapReservation) -> bool {
        reservation.remaining == 0
    }

    pub fn allocate_enum(
        &mut self,
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    ) -> Result<RuntimeValue, HeapError> {
        let reference = self.allocate(Object::Enum {
            type_id,
            variant,
            tag,
            payload,
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn enum_tag(&self, value: RuntimeValue) -> Result<u32, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        match self.resolve(reference)? {
            Object::Enum {
                type_id: actual,
                tag,
                ..
            } if *actual == type_id => Ok(*tag),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn enum_parts(
        &self,
        value: RuntimeValue,
    ) -> Result<(StableId, StableId, u32, Option<RuntimeValue>), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        match self.resolve(reference)? {
            Object::Enum {
                type_id: actual,
                variant,
                tag,
                payload,
            } if *actual == type_id => Ok((*actual, *variant, *tag, *payload)),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn enum_payload(
        &self,
        value: RuntimeValue,
        expected_variant: StableId,
    ) -> Result<RuntimeValue, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        match self.resolve(reference)? {
            Object::Enum {
                type_id: actual,
                variant,
                payload: Some(payload),
                ..
            } if *actual == type_id && *variant == expected_variant => Ok(*payload),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn allocate_struct(
        &mut self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if fields.len() > nexa_bytecode::MAX_STRUCT_FIELDS {
            return Err(HeapError::CapacityExhausted);
        }
        let mut reservation = self.preflight(1)?;
        self.commit_struct(&mut reservation, type_id, fields)
    }

    pub(crate) fn commit_struct(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if fields.len() > nexa_bytecode::MAX_STRUCT_FIELDS {
            return Err(HeapError::CapacityExhausted);
        }
        let hash = self.structural_hash(type_id, fields)?;
        let mut stored = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        stored[..fields.len()].copy_from_slice(fields);
        let reference = self.commit(
            reservation,
            Object::Struct {
                type_id,
                fields: stored,
                field_count: u8::try_from(fields.len()).expect("struct field limit fits into u8"),
                hash,
            },
        );
        Ok(RuntimeValue::Struct {
            reference,
            type_id,
            hash,
        })
    }

    pub fn struct_fields(&self, value: RuntimeValue) -> Result<&[RuntimeValue], HeapError> {
        let RuntimeValue::Struct {
            reference,
            type_id,
            hash,
        } = value
        else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        match self.resolve(reference)? {
            Object::Struct {
                type_id: actual,
                fields,
                field_count,
                hash: actual_hash,
            } if *actual == type_id && *actual_hash == hash => {
                Ok(&fields[..usize::from(*field_count)])
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn struct_field(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        self.struct_fields(value)?
            .get(index)
            .copied()
            .ok_or(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }))
    }

    pub fn struct_with(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<RuntimeValue, HeapError> {
        let RuntimeValue::Struct { type_id, .. } = value else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        let fields = self.struct_fields(value)?;
        if index >= fields.len() {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        }
        let mut updated = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        updated[..fields.len()].copy_from_slice(fields);
        updated[index] = replacement;
        self.allocate_struct(type_id, &updated[..fields.len()])
    }

    pub fn struct_equal(&self, lhs: RuntimeValue, rhs: RuntimeValue) -> Result<bool, HeapError> {
        let (
            RuntimeValue::Struct {
                type_id: lhs_type,
                hash: lhs_hash,
                ..
            },
            RuntimeValue::Struct {
                type_id: rhs_type,
                hash: rhs_hash,
                ..
            },
        ) = (lhs, rhs)
        else {
            return Err(HeapError::InvalidReference(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }));
        };
        if lhs_type != rhs_type || lhs_hash != rhs_hash {
            return Ok(false);
        }
        let lhs = self.struct_fields(lhs)?;
        let rhs = self.struct_fields(rhs)?;
        if lhs.len() != rhs.len() {
            return Ok(false);
        }
        lhs.iter().zip(rhs).try_fold(true, |equal, (lhs, rhs)| {
            Ok(equal && self.runtime_value_equal(*lhs, *rhs)?)
        })
    }

    pub fn allocate_class(
        &mut self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if fields.len() > nexa_bytecode::MAX_CLASS_FIELDS {
            return Err(HeapError::CapacityExhausted);
        }
        let mut stored = [RuntimeValue::Unit; nexa_bytecode::MAX_CLASS_FIELDS];
        stored[..fields.len()].copy_from_slice(fields);
        let reference = self.allocate(Object::Class {
            type_id,
            fields: stored,
            field_count: u8::try_from(fields.len()).expect("class field limit fits into u8"),
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn class_field(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Class {
                type_id: actual,
                fields,
                field_count,
            } if *actual == type_id => fields[..usize::from(*field_count)]
                .get(index)
                .copied()
                .ok_or(HeapError::InvalidReference(reference)),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn set_class_field(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve_mut(reference)? {
            Object::Class {
                type_id: actual,
                fields,
                field_count,
            } if *actual == type_id && index < usize::from(*field_count) => {
                fields[index] = replacement;
                Ok(())
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn class_equal(&self, lhs: RuntimeValue, rhs: RuntimeValue) -> Result<bool, HeapError> {
        let (
            RuntimeValue::NamedRef {
                reference: lhs,
                type_id: lhs_type,
            },
            RuntimeValue::NamedRef {
                reference: rhs,
                type_id: rhs_type,
            },
        ) = (lhs, rhs)
        else {
            return Err(invalid_value_reference());
        };
        if lhs_type != rhs_type {
            return Ok(false);
        }
        if !matches!(
            (self.resolve(lhs)?, self.resolve(rhs)?),
            (
                Object::Class { type_id: left, .. },
                Object::Class { type_id: right, .. }
            ) if *left == lhs_type && *right == rhs_type
        ) {
            return Err(HeapError::InvalidReference(lhs));
        }
        Ok(lhs == rhs)
    }

    fn structural_hash(
        &self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<u64, HeapError> {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        write_hash(&mut hash, &type_id.0.to_le_bytes());
        for field in fields {
            write_hash(&mut hash, &self.runtime_value_hash(*field)?.to_le_bytes());
        }
        Ok(hash)
    }

    fn runtime_value_hash(&self, value: RuntimeValue) -> Result<u64, HeapError> {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        match value {
            RuntimeValue::I32(value) => {
                write_hash(&mut hash, &[1]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::I64(value) => {
                write_hash(&mut hash, &[2]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::F32(value) => {
                write_hash(&mut hash, &[3]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::F64(value) => {
                write_hash(&mut hash, &[4]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::Bool(value) => write_hash(&mut hash, &[5, u8::from(value)]),
            RuntimeValue::Rune(value) => {
                write_hash(&mut hash, &[6]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::String { hash: value, .. } | RuntimeValue::Struct { hash: value, .. } => {
                write_hash(&mut hash, &[7]);
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::NamedRef { reference, type_id } => {
                write_hash(&mut hash, &[8]);
                write_hash(&mut hash, &type_id.0.to_le_bytes());
                let (_, variant, tag, payload) =
                    self.enum_parts(RuntimeValue::NamedRef { reference, type_id })?;
                write_hash(&mut hash, &variant.0.to_le_bytes());
                write_hash(&mut hash, &tag.to_le_bytes());
                if let Some(payload) = payload {
                    write_hash(&mut hash, &self.runtime_value_hash(payload)?.to_le_bytes());
                }
            }
            RuntimeValue::Ref(reference) => {
                write_hash(&mut hash, &[9]);
                write_hash(&mut hash, &reference.index.to_le_bytes());
                write_hash(&mut hash, &reference.generation.to_le_bytes());
            }
            RuntimeValue::HostRequest(value) => {
                write_hash(&mut hash, &[10]);
                write_hash(&mut hash, &value.raw().index.to_le_bytes());
                write_hash(&mut hash, &value.raw().generation.to_le_bytes());
            }
            RuntimeValue::ResourceToken(value) => {
                write_hash(&mut hash, &[11]);
                write_hash(&mut hash, &value.raw().index.to_le_bytes());
                write_hash(&mut hash, &value.raw().generation.to_le_bytes());
            }
            RuntimeValue::Snapshot(value) => {
                write_hash(&mut hash, &[12]);
                write_hash(&mut hash, &value.raw().index.to_le_bytes());
                write_hash(&mut hash, &value.raw().generation.to_le_bytes());
            }
            RuntimeValue::Opaque { value, type_id } => {
                write_hash(&mut hash, &[13]);
                write_hash(&mut hash, &type_id.0.to_le_bytes());
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::StateHandle {
                domain,
                stable_id,
                generation,
                handle_type,
            } => {
                write_hash(&mut hash, &[14]);
                write_hash(&mut hash, &domain.to_le_bytes());
                write_hash(&mut hash, &stable_id.0.to_le_bytes());
                write_hash(&mut hash, &generation.to_le_bytes());
                write_hash(&mut hash, &handle_type.0.to_le_bytes());
            }
            RuntimeValue::Unit => write_hash(&mut hash, &[15]),
        }
        Ok(hash)
    }

    #[allow(clippy::float_cmp)]
    fn runtime_value_equal(&self, lhs: RuntimeValue, rhs: RuntimeValue) -> Result<bool, HeapError> {
        Ok(match (lhs, rhs) {
            (RuntimeValue::F32(lhs), RuntimeValue::F32(rhs)) => {
                f32::from_bits(lhs) == f32::from_bits(rhs)
            }
            (RuntimeValue::F64(lhs), RuntimeValue::F64(rhs)) => {
                f64::from_bits(lhs) == f64::from_bits(rhs)
            }
            (
                RuntimeValue::String { reference: lhs, .. },
                RuntimeValue::String { reference: rhs, .. },
            ) => self.string(lhs)? == self.string(rhs)?,
            (lhs @ RuntimeValue::Struct { .. }, rhs @ RuntimeValue::Struct { .. }) => {
                self.struct_equal(lhs, rhs)?
            }
            (
                lhs @ RuntimeValue::NamedRef {
                    type_id: lhs_type, ..
                },
                rhs @ RuntimeValue::NamedRef {
                    type_id: rhs_type, ..
                },
            ) if lhs_type == rhs_type => {
                let (_, lhs_variant, lhs_tag, lhs_payload) = self.enum_parts(lhs)?;
                let (_, rhs_variant, rhs_tag, rhs_payload) = self.enum_parts(rhs)?;
                lhs_variant == rhs_variant
                    && lhs_tag == rhs_tag
                    && match (lhs_payload, rhs_payload) {
                        (Some(lhs), Some(rhs)) => self.runtime_value_equal(lhs, rhs)?,
                        (None, None) => true,
                        _ => false,
                    }
            }
            _ => lhs == rhs,
        })
    }

    pub fn resolve(&self, reference: GcRef) -> Result<&Object, HeapError> {
        let slot = self
            .slots
            .get(reference.index as usize)
            .filter(|slot| slot.generation == reference.generation)
            .and_then(|slot| slot.object.as_ref())
            .ok_or(HeapError::InvalidReference(reference))?;
        Ok(slot)
    }

    fn resolve_mut(&mut self, reference: GcRef) -> Result<&mut Object, HeapError> {
        self.slots
            .get_mut(reference.index as usize)
            .filter(|slot| slot.generation == reference.generation)
            .and_then(|slot| slot.object.as_mut())
            .ok_or(HeapError::InvalidReference(reference))
    }

    pub fn collect(&mut self, roots: &GcRoots) -> Result<CollectionStats, HeapError> {
        for slot in &mut self.slots {
            slot.marked = false;
        }
        let mut queue = VecDeque::new();
        for root in roots.iter() {
            self.validate_reference(root)?;
            queue.push_back(root);
        }
        let mut marked = 0;
        while let Some(reference) = queue.pop_front() {
            let slot = &mut self.slots[reference.index as usize];
            if slot.marked {
                continue;
            }
            slot.marked = true;
            marked += 1;
            let object = slot.object.as_ref().expect("validated live object");
            let references = object.references();
            for child in references {
                self.validate_reference(child)?;
                queue.push_back(child);
            }
        }
        let mut reclaimed = 0;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.object.is_some() && !slot.marked {
                slot.object = None;
                if let Some(generation) = slot.generation.checked_add(1) {
                    slot.generation = generation;
                    self.free
                        .push(u32::try_from(index).expect("slot indices originate as u32"));
                }
                reclaimed += 1;
            }
        }
        Ok(CollectionStats {
            marked,
            reclaimed,
            live: self.live_len(),
        })
    }

    pub fn failure_injector(&mut self) -> &mut RuntimeFailureInjector {
        &mut self.failure_injector
    }

    #[must_use]
    pub fn live_len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.object.is_some())
            .count()
    }

    fn validate_reference(&self, reference: GcRef) -> Result<(), HeapError> {
        self.resolve(reference).map(|_| ())
    }
}

fn write_hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::StableId;

    use super::{GcRoots, Heap, HeapError, Object};
    use crate::{RuntimeFailurePoint, RuntimeValue};

    #[test]
    fn cycles_collect_but_suspended_task_roots_survive() {
        let mut heap = Heap::new(4);
        let type_id = StableId::from_name("Node");
        let first = heap.allocate_class(type_id, &[RuntimeValue::Unit]).unwrap();
        let second = heap.allocate_class(type_id, &[first]).unwrap();
        heap.set_class_field(first, 0, second).unwrap();
        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.reclaimed, 2);

        let waiting = heap.allocate(Object::String("waiting".into())).unwrap();
        let roots = GcRoots {
            suspended_tasks: vec![waiting],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 1);
        assert!(heap.resolve(waiting).is_ok());
    }

    #[test]
    fn string_limits_are_checked_before_concat_allocation() {
        let mut heap = Heap::new_with_string_limit(3, 4);
        let lhs = heap.allocate_string("ab").unwrap();
        let rhs = heap.allocate_string("界").unwrap();
        let before = heap.live_len();
        assert_eq!(
            heap.concat_strings(lhs, rhs),
            Err(HeapError::StringTooLarge {
                bytes: 5,
                max_bytes: 4,
            })
        );
        assert_eq!(heap.live_len(), before);
        assert_eq!(heap.string(lhs), Ok("ab"));
    }

    #[test]
    fn allocation_failure_does_not_drop_live_objects() {
        let mut heap = Heap::new(2);
        let live = heap.allocate(Object::I32Array(vec![1, 2])).unwrap();
        heap.failure_injector()
            .arm_once(RuntimeFailurePoint::HeapSlot);
        assert_eq!(
            heap.allocate(Object::String("no".into())),
            Err(HeapError::InjectedFailure(RuntimeFailurePoint::HeapSlot))
        );
        assert!(heap.resolve(live).is_ok());
    }

    #[test]
    fn multi_slot_preflight_rejects_before_any_heap_mutation() {
        let mut heap = Heap::new(1);
        assert!(matches!(
            heap.preflight(2),
            Err(HeapError::CapacityExhausted)
        ));
        assert_eq!(heap.live_len(), 0);
        assert!(
            heap.allocate_class(StableId::from_name("Empty"), &[])
                .is_ok()
        );
    }
}
