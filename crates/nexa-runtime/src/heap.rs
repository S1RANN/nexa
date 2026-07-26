use std::collections::VecDeque;
use std::fmt;

use nexa_core::StableId;

use crate::{RuntimeFailureInjector, RuntimeFailurePoint, RuntimeValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapEntry {
    key: RuntimeValue,
    value: RuntimeValue,
    hash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MapRehash {
    old_slots: Vec<Option<MapEntry>>,
    new_slots: Vec<Option<MapEntry>>,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmMap {
    type_id: StableId,
    key_type: nexa_bytecode::ValueType,
    value_type: nexa_bytecode::ValueType,
    slots: Vec<Option<MapEntry>>,
    length: usize,
    rehash: Option<MapRehash>,
}

impl VmMap {
    fn references(&self) -> Vec<GcRef> {
        let current = self
            .slots
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|entry| [entry.key, entry.value]);
        let rehash = self.rehash.iter().flat_map(|rehash| {
            rehash
                .old_slots
                .iter()
                .chain(&rehash.new_slots)
                .filter_map(Option::as_ref)
                .flat_map(|entry| [entry.key, entry.value])
        });
        current.chain(rehash).filter_map(value_reference).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapSetOutcome {
    Complete,
    RehashPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MapLocation {
    Current(usize),
    RehashOld(usize),
    RehashNew(usize),
}

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
    Map(VmMap),
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
    Array {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: Vec<RuntimeValue>,
    },
    Buffer {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: Vec<RuntimeValue>,
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
            Self::Array { values, .. } | Self::Buffer { values, .. } => values
                .iter()
                .filter_map(|value| match value {
                    RuntimeValue::String { reference, .. }
                    | RuntimeValue::Struct { reference, .. }
                    | RuntimeValue::Ref(reference)
                    | RuntimeValue::NamedRef { reference, .. } => Some(*reference),
                    _ => None,
                })
                .collect(),
            Self::Map(map) => map.references(),
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

const fn value_reference(value: RuntimeValue) -> Option<GcRef> {
    match value {
        RuntimeValue::String { reference, .. }
        | RuntimeValue::Struct { reference, .. }
        | RuntimeValue::Ref(reference)
        | RuntimeValue::NamedRef { reference, .. } => Some(reference),
        _ => None,
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
    CollectionTooLarge { length: usize, max_length: usize },
    IndexOutOfBounds { index: usize, length: usize },
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
    max_collection_length: usize,
    failure_injector: RuntimeFailureInjector,
}

impl Heap {
    pub const DEFAULT_MAX_COLLECTION_LENGTH: usize = 1_024;

    #[must_use]
    pub fn new(max_objects: u32) -> Self {
        Self::new_with_string_limit(max_objects, usize::MAX)
    }

    #[must_use]
    pub fn new_with_string_limit(max_objects: u32, max_string_bytes: usize) -> Self {
        Self::new_with_limits(
            max_objects,
            max_string_bytes,
            Self::DEFAULT_MAX_COLLECTION_LENGTH,
        )
    }

    #[must_use]
    pub fn new_with_limits(
        max_objects: u32,
        max_string_bytes: usize,
        max_collection_length: usize,
    ) -> Self {
        Self {
            slots: Vec::with_capacity(max_objects as usize),
            free: Vec::with_capacity(max_objects as usize),
            max_objects,
            max_string_bytes,
            max_collection_length: max_collection_length.min(i32::MAX as usize),
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

    pub(crate) fn validate_collection_length(&self, length: usize) -> Result<(), HeapError> {
        if length > self.max_collection_length {
            Err(HeapError::CollectionTooLarge {
                length,
                max_length: self.max_collection_length,
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

    pub(crate) fn commit_owned_string(
        &mut self,
        reservation: &mut HeapReservation,
        value: String,
    ) -> Result<RuntimeValue, HeapError> {
        self.validate_string_length(value.len())?;
        let reference = self.commit(reservation, Object::String(value));
        let hash = self.string_hash(reference)?;
        Ok(RuntimeValue::String { reference, hash })
    }

    pub(crate) fn commit_array_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: Vec<RuntimeValue>,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::array_type(element_type) {
            return Err(invalid_value_reference());
        }
        self.validate_collection_length(values.len())?;
        let reference = self.commit(
            reservation,
            Object::Array {
                type_id,
                element_type,
                values,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub(crate) fn commit_buffer_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: Vec<RuntimeValue>,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::buffer_type(element_type) {
            return Err(invalid_value_reference());
        }
        self.validate_collection_length(values.len())?;
        let reference = self.commit(
            reservation,
            Object::Buffer {
                type_id,
                element_type,
                values,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn allocate_enum(
        &mut self,
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    ) -> Result<RuntimeValue, HeapError> {
        let mut reservation = self.preflight(1)?;
        Ok(self.allocate_enum_reserved(&mut reservation, type_id, variant, tag, payload))
    }

    pub(crate) fn allocate_enum_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    ) -> RuntimeValue {
        let reference = self.commit(
            reservation,
            Object::Enum {
                type_id,
                variant,
                tag,
                payload,
            },
        );
        RuntimeValue::NamedRef { reference, type_id }
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

    pub fn allocate_array(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::array_type(element_type) {
            return Err(invalid_value_reference());
        }
        let reference = self.allocate(Object::Array {
            type_id,
            element_type,
            values: Vec::new(),
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn array_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.array_values(value)?.len())
    }

    pub fn array_get(&self, value: RuntimeValue, index: usize) -> Result<RuntimeValue, HeapError> {
        let values = self.array_values(value)?;
        values
            .get(index)
            .copied()
            .ok_or(HeapError::IndexOutOfBounds {
                index,
                length: values.len(),
            })
    }

    pub fn array_set(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let values = self.array_values_mut(value)?;
        let length = values.len();
        let slot = values
            .get_mut(index)
            .ok_or(HeapError::IndexOutOfBounds { index, length })?;
        *slot = replacement;
        Ok(())
    }

    pub fn array_push(
        &mut self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let max_length = self.max_collection_length;
        let values = self.array_values_mut(value)?;
        let length = values
            .len()
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length,
            })?;
        if length > max_length {
            return Err(HeapError::CollectionTooLarge { length, max_length });
        }
        values
            .try_reserve(1)
            .map_err(|_| HeapError::CapacityExhausted)?;
        values.push(element);
        Ok(())
    }

    pub fn array_pop(&mut self, value: RuntimeValue) -> Result<RuntimeValue, HeapError> {
        let values = self.array_values_mut(value)?;
        let length = values.len();
        values
            .pop()
            .ok_or(HeapError::IndexOutOfBounds { index: 0, length })
    }

    pub fn array_insert(
        &mut self,
        value: RuntimeValue,
        index: usize,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let max_length = self.max_collection_length;
        let values = self.array_values_mut(value)?;
        let current = values.len();
        if index > current {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: current,
            });
        }
        let length = current
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length,
            })?;
        if length > max_length {
            return Err(HeapError::CollectionTooLarge { length, max_length });
        }
        values
            .try_reserve(1)
            .map_err(|_| HeapError::CapacityExhausted)?;
        values.insert(index, element);
        Ok(())
    }

    pub fn array_remove(
        &mut self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let values = self.array_values_mut(value)?;
        if index >= values.len() {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: values.len(),
            });
        }
        Ok(values.remove(index))
    }

    pub fn array_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        self.array_values_mut(value)?.clear();
        Ok(())
    }

    pub fn array_values(&self, value: RuntimeValue) -> Result<&[RuntimeValue], HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Array {
                type_id: actual,
                element_type,
                values,
            } if *actual == type_id && type_id == nexa_bytecode::array_type(*element_type) => {
                Ok(values)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn array_values_mut(
        &mut self,
        value: RuntimeValue,
    ) -> Result<&mut Vec<RuntimeValue>, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve_mut(reference)? {
            Object::Array {
                type_id: actual,
                element_type,
                values,
            } if *actual == type_id && type_id == nexa_bytecode::array_type(*element_type) => {
                Ok(values)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn allocate_buffer(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        source: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::buffer_type(element_type) {
            return Err(invalid_value_reference());
        }
        if source.len() > self.max_collection_length {
            return Err(HeapError::CollectionTooLarge {
                length: source.len(),
                max_length: self.max_collection_length,
            });
        }
        let mut values = Vec::new();
        values
            .try_reserve_exact(source.len())
            .map_err(|_| HeapError::CapacityExhausted)?;
        values.extend_from_slice(source);
        let reference = self.allocate(Object::Buffer {
            type_id,
            element_type,
            values,
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn buffer_values(&self, value: RuntimeValue) -> Result<&[RuntimeValue], HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                values,
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok(values)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn buffer_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.buffer_values(value)?.len())
    }

    pub fn buffer_get(&self, value: RuntimeValue, index: usize) -> Result<RuntimeValue, HeapError> {
        let values = self.buffer_values(value)?;
        values
            .get(index)
            .copied()
            .ok_or(HeapError::IndexOutOfBounds {
                index,
                length: values.len(),
            })
    }

    pub fn buffer_set(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let values = self.buffer_values_mut(value)?;
        let length = values.len();
        let slot = values
            .get_mut(index)
            .ok_or(HeapError::IndexOutOfBounds { index, length })?;
        *slot = replacement;
        Ok(())
    }

    pub fn buffer_slice(
        &mut self,
        value: RuntimeValue,
        start: usize,
        length: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let (type_id, element_type) = self.buffer_metadata(value)?;
        let values = self.buffer_values(value)?;
        let end = checked_collection_end(start, length, values.len())?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(length)
            .map_err(|_| HeapError::CapacityExhausted)?;
        copy.extend_from_slice(&values[start..end]);
        self.allocate_buffer(type_id, element_type, &copy)
    }

    pub fn buffer_copy(
        &mut self,
        destination: RuntimeValue,
        source: RuntimeValue,
        source_start: usize,
        destination_start: usize,
        length: usize,
    ) -> Result<(), HeapError> {
        let destination_metadata = self.buffer_metadata(destination)?;
        if self.buffer_metadata(source)? != destination_metadata {
            return Err(invalid_value_reference());
        }
        let source_values = self.buffer_values(source)?;
        let source_end = checked_collection_end(source_start, length, source_values.len())?;
        let destination_end = checked_collection_end(
            destination_start,
            length,
            self.buffer_values(destination)?.len(),
        )?;
        let mut copy = Vec::new();
        copy.try_reserve_exact(length)
            .map_err(|_| HeapError::CapacityExhausted)?;
        copy.extend_from_slice(&source_values[source_start..source_end]);
        self.buffer_values_mut(destination)?[destination_start..destination_end]
            .copy_from_slice(&copy);
        Ok(())
    }

    fn buffer_metadata(
        &self,
        value: RuntimeValue,
    ) -> Result<(StableId, nexa_bytecode::ValueType), HeapError> {
        let RuntimeValue::NamedRef { type_id, .. } = value else {
            return Err(invalid_value_reference());
        };
        let RuntimeValue::NamedRef { reference, .. } = value else {
            unreachable!("named reference checked")
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                ..
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok((type_id, *element_type))
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn buffer_values_mut(
        &mut self,
        value: RuntimeValue,
    ) -> Result<&mut Vec<RuntimeValue>, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve_mut(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                values,
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok(values)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn allocate_map(
        &mut self,
        type_id: StableId,
        key_type: nexa_bytecode::ValueType,
        value_type: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::map_type(key_type, value_type) {
            return Err(invalid_value_reference());
        }
        let initial_capacity = self.max_collection_length.min(8);
        let slots = empty_map_slots(initial_capacity)?;
        let reference = self.allocate(Object::Map(VmMap {
            type_id,
            key_type,
            value_type,
            slots,
            length: 0,
            rehash: None,
        }))?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn map_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.map(value)?.length)
    }

    pub fn map_get(
        &self,
        value: RuntimeValue,
        key: RuntimeValue,
    ) -> Result<Option<RuntimeValue>, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let map = self.map(value)?;
        Ok(self
            .find_map_entry(map, key, hash)?
            .map(|location| map_entry(map, location).value))
    }

    pub fn map_contains(&self, value: RuntimeValue, key: RuntimeValue) -> Result<bool, HeapError> {
        self.map_get(value, key).map(|value| value.is_some())
    }

    pub fn map_set(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
        replacement: RuntimeValue,
    ) -> Result<MapSetOutcome, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let location = {
            let map = self.map(value)?;
            self.find_map_entry(map, key, hash)?
        };
        if let Some(location) = location {
            map_entry_mut(self.map_mut(value)?, location).value = replacement;
            return Ok(MapSetOutcome::Complete);
        }

        if self.map(value)?.length >= self.max_collection_length {
            return Err(HeapError::CollectionTooLarge {
                length: self.map(value)?.length.saturating_add(1),
                max_length: self.max_collection_length,
            });
        }
        if self.map(value)?.rehash.is_some() {
            progress_map_rehash(self.map_mut(value)?)?;
            return Ok(MapSetOutcome::RehashPending);
        }
        if map_needs_rehash(self.map(value)?) {
            let old_capacity = self.map(value)?.slots.len();
            let maximum_capacity = self
                .max_collection_length
                .saturating_mul(2)
                .checked_next_power_of_two()
                .unwrap_or(usize::MAX);
            let new_capacity = old_capacity.saturating_mul(2).max(1).min(maximum_capacity);
            if new_capacity > old_capacity {
                let new_slots = empty_map_slots(new_capacity)?;
                let map = self.map_mut(value)?;
                let old_slots = std::mem::take(&mut map.slots);
                map.rehash = Some(MapRehash {
                    old_slots,
                    new_slots,
                    cursor: 0,
                });
                return Ok(MapSetOutcome::RehashPending);
            }
        }

        let entry = MapEntry {
            key,
            value: replacement,
            hash,
        };
        let map = self.map_mut(value)?;
        insert_map_entry(&mut map.slots, entry)?;
        map.length += 1;
        Ok(MapSetOutcome::Complete)
    }

    pub fn map_remove(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
    ) -> Result<Option<RuntimeValue>, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let location = {
            let map = self.map(value)?;
            self.find_map_entry(map, key, hash)?
        };
        let Some(location) = location else {
            return Ok(None);
        };
        let entry = take_map_entry(self.map_mut(value)?, location);
        self.map_mut(value)?.length -= 1;
        Ok(Some(entry.value))
    }

    pub fn map_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let map = self.map_mut(value)?;
        map.slots.fill(None);
        map.rehash = None;
        map.length = 0;
        Ok(())
    }

    fn map(&self, value: RuntimeValue) -> Result<&VmMap, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Map(map)
                if map.type_id == type_id
                    && type_id == nexa_bytecode::map_type(map.key_type, map.value_type) =>
            {
                Ok(map)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn map_mut(&mut self, value: RuntimeValue) -> Result<&mut VmMap, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve_mut(reference)? {
            Object::Map(map)
                if map.type_id == type_id
                    && type_id == nexa_bytecode::map_type(map.key_type, map.value_type) =>
            {
                Ok(map)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn find_map_entry(
        &self,
        map: &VmMap,
        key: RuntimeValue,
        hash: u64,
    ) -> Result<Option<MapLocation>, HeapError> {
        for (index, entry) in map.slots.iter().enumerate() {
            if entry.is_some_and(|entry| entry.hash == hash)
                && self.runtime_value_equal(entry.expect("checked entry").key, key)?
            {
                return Ok(Some(MapLocation::Current(index)));
            }
        }
        if let Some(rehash) = &map.rehash {
            for (index, entry) in rehash.new_slots.iter().enumerate() {
                if entry.is_some_and(|entry| entry.hash == hash)
                    && self.runtime_value_equal(entry.expect("checked entry").key, key)?
                {
                    return Ok(Some(MapLocation::RehashNew(index)));
                }
            }
            for (index, entry) in rehash.old_slots.iter().enumerate() {
                if entry.is_some_and(|entry| entry.hash == hash)
                    && self.runtime_value_equal(entry.expect("checked entry").key, key)?
                {
                    return Ok(Some(MapLocation::RehashOld(index)));
                }
            }
        }
        Ok(None)
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
                match self.resolve(reference)? {
                    Object::Enum {
                        variant,
                        tag,
                        payload,
                        ..
                    } => {
                        write_hash(&mut hash, &variant.0.to_le_bytes());
                        write_hash(&mut hash, &tag.to_le_bytes());
                        if let Some(payload) = payload {
                            write_hash(
                                &mut hash,
                                &self.runtime_value_hash(*payload)?.to_le_bytes(),
                            );
                        }
                    }
                    Object::Class { .. } | Object::Array { .. } | Object::Buffer { .. } => {
                        write_hash(&mut hash, &reference.index.to_le_bytes());
                        write_hash(&mut hash, &reference.generation.to_le_bytes());
                    }
                    _ => return Err(HeapError::InvalidReference(reference)),
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
                let (
                    RuntimeValue::NamedRef {
                        reference: lhs_reference,
                        ..
                    },
                    RuntimeValue::NamedRef {
                        reference: rhs_reference,
                        ..
                    },
                ) = (lhs, rhs)
                else {
                    unreachable!("matched named references")
                };
                match (self.resolve(lhs_reference)?, self.resolve(rhs_reference)?) {
                    (Object::Enum { .. }, Object::Enum { .. }) => {
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
                    (Object::Class { .. }, Object::Class { .. })
                    | (Object::Array { .. }, Object::Array { .. })
                    | (Object::Buffer { .. }, Object::Buffer { .. }) => {
                        lhs_reference == rhs_reference
                    }
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

    pub(crate) fn set_failure_injector(&mut self, injector: RuntimeFailureInjector) {
        self.failure_injector = injector;
    }

    #[must_use]
    pub fn live_len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.object.is_some())
            .count()
    }

    #[cfg(any(test, feature = "model-adapter"))]
    #[must_use]
    pub(crate) const fn capacity_limit(&self) -> u32 {
        self.max_objects
    }

    fn validate_reference(&self, reference: GcRef) -> Result<(), HeapError> {
        self.resolve(reference).map(|_| ())
    }
}

fn checked_collection_end(
    start: usize,
    length: usize,
    collection_length: usize,
) -> Result<usize, HeapError> {
    let end = start
        .checked_add(length)
        .ok_or(HeapError::IndexOutOfBounds {
            index: usize::MAX,
            length: collection_length,
        })?;
    if end > collection_length {
        Err(HeapError::IndexOutOfBounds {
            index: end,
            length: collection_length,
        })
    } else {
        Ok(end)
    }
}

fn empty_map_slots(capacity: usize) -> Result<Vec<Option<MapEntry>>, HeapError> {
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(capacity)
        .map_err(|_| HeapError::CapacityExhausted)?;
    slots.resize(capacity, None);
    Ok(slots)
}

fn map_needs_rehash(map: &VmMap) -> bool {
    map.slots.is_empty()
        || map.length.saturating_add(1).saturating_mul(4) > map.slots.len().saturating_mul(3)
}

fn insert_map_entry(slots: &mut [Option<MapEntry>], entry: MapEntry) -> Result<(), HeapError> {
    if slots.is_empty() {
        return Err(HeapError::CapacityExhausted);
    }
    let start = usize::try_from(entry.hash % slots.len() as u64)
        .expect("hash modulo slot count fits usize");
    for offset in 0..slots.len() {
        let index = (start + offset) % slots.len();
        if slots[index].is_none() {
            slots[index] = Some(entry);
            return Ok(());
        }
    }
    Err(HeapError::CapacityExhausted)
}

fn progress_map_rehash(map: &mut VmMap) -> Result<(), HeapError> {
    const REHASH_CHUNK: usize = 8;
    let rehash = map.rehash.as_mut().expect("rehash state checked by caller");
    let end = rehash
        .cursor
        .saturating_add(REHASH_CHUNK)
        .min(rehash.old_slots.len());
    for index in rehash.cursor..end {
        if let Some(entry) = rehash.old_slots[index].take() {
            insert_map_entry(&mut rehash.new_slots, entry)?;
        }
    }
    rehash.cursor = end;
    if end == rehash.old_slots.len() {
        map.slots = std::mem::take(&mut rehash.new_slots);
        map.rehash = None;
    }
    Ok(())
}

fn map_entry(map: &VmMap, location: MapLocation) -> MapEntry {
    match location {
        MapLocation::Current(index) => map.slots[index].expect("located map entry exists"),
        MapLocation::RehashOld(index) => map
            .rehash
            .as_ref()
            .expect("located rehash entry has state")
            .old_slots[index]
            .expect("located map entry exists"),
        MapLocation::RehashNew(index) => map
            .rehash
            .as_ref()
            .expect("located rehash entry has state")
            .new_slots[index]
            .expect("located map entry exists"),
    }
}

fn map_entry_mut(map: &mut VmMap, location: MapLocation) -> &mut MapEntry {
    match location {
        MapLocation::Current(index) => map.slots[index].as_mut().expect("located map entry exists"),
        MapLocation::RehashOld(index) => map
            .rehash
            .as_mut()
            .expect("located rehash entry has state")
            .old_slots[index]
            .as_mut()
            .expect("located map entry exists"),
        MapLocation::RehashNew(index) => map
            .rehash
            .as_mut()
            .expect("located rehash entry has state")
            .new_slots[index]
            .as_mut()
            .expect("located map entry exists"),
    }
}

fn take_map_entry(map: &mut VmMap, location: MapLocation) -> MapEntry {
    match location {
        MapLocation::Current(index) => map.slots[index].take().expect("located map entry exists"),
        MapLocation::RehashOld(index) => map
            .rehash
            .as_mut()
            .expect("located rehash entry has state")
            .old_slots[index]
            .take()
            .expect("located map entry exists"),
        MapLocation::RehashNew(index) => map
            .rehash
            .as_mut()
            .expect("located rehash entry has state")
            .new_slots[index]
            .take()
            .expect("located map entry exists"),
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

    use super::{GcRoots, Heap, HeapError, MapSetOutcome, Object};
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

    #[test]
    fn arrays_enforce_bounds_and_max_length_before_mutation() {
        let mut heap = Heap::new_with_limits(4, usize::MAX, 2);
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();

        heap.array_push(array, RuntimeValue::I32(10)).unwrap();
        heap.array_insert(array, 0, RuntimeValue::I32(5)).unwrap();
        assert_eq!(heap.array_len(array), Ok(2));
        assert_eq!(heap.array_get(array, 0), Ok(RuntimeValue::I32(5)));
        assert_eq!(
            heap.array_push(array, RuntimeValue::I32(99)),
            Err(HeapError::CollectionTooLarge {
                length: 3,
                max_length: 2,
            })
        );
        assert_eq!(
            heap.array_insert(array, 3, RuntimeValue::I32(99)),
            Err(HeapError::IndexOutOfBounds {
                index: 3,
                length: 2,
            })
        );
        assert_eq!(heap.array_len(array), Ok(2));

        heap.array_set(array, 1, RuntimeValue::I32(7)).unwrap();
        assert_eq!(heap.array_remove(array, 0), Ok(RuntimeValue::I32(5)));
        assert_eq!(heap.array_pop(array), Ok(RuntimeValue::I32(7)));
        assert_eq!(
            heap.array_pop(array),
            Err(HeapError::IndexOutOfBounds {
                index: 0,
                length: 0,
            })
        );
        heap.array_clear(array).unwrap();
    }

    #[test]
    fn array_elements_are_traced_from_the_array_root() {
        let mut heap = Heap::new_with_limits(4, usize::MAX, 4);
        let string = heap.allocate_string("kept").unwrap();
        let string_value = RuntimeValue::String {
            reference: string,
            hash: heap.string_hash(string).unwrap(),
        };
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::String);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::String)
            .unwrap();
        heap.array_push(array, string_value).unwrap();
        let RuntimeValue::NamedRef {
            reference: array_reference,
            ..
        } = array
        else {
            unreachable!("array allocations are named references")
        };

        let roots = GcRoots {
            running_frames: vec![array_reference],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().marked, 2);
        assert_eq!(heap.string(string), Ok("kept"));

        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.reclaimed, 2);
        assert_eq!(stats.live, 0);
    }

    #[test]
    fn buffers_copy_slice_and_enforce_bounds_without_partial_mutation() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 4);
        let element = nexa_bytecode::ValueType::I32;
        let type_id = nexa_bytecode::buffer_type(element);
        let destination = heap
            .allocate_buffer(
                type_id,
                element,
                &[
                    RuntimeValue::I32(1),
                    RuntimeValue::I32(2),
                    RuntimeValue::I32(3),
                    RuntimeValue::I32(4),
                ],
            )
            .unwrap();
        let source = heap
            .allocate_buffer(
                type_id,
                element,
                &[
                    RuntimeValue::I32(9),
                    RuntimeValue::I32(8),
                    RuntimeValue::I32(7),
                ],
            )
            .unwrap();

        heap.buffer_set(destination, 0, RuntimeValue::I32(6))
            .unwrap();
        heap.buffer_copy(destination, source, 0, 1, 2).unwrap();
        assert_eq!(
            heap.buffer_values(destination),
            Ok(&[
                RuntimeValue::I32(6),
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
                RuntimeValue::I32(4),
            ][..])
        );
        assert_eq!(
            heap.buffer_values(source),
            Ok(&[
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
                RuntimeValue::I32(7),
            ][..])
        );

        let slice = heap.buffer_slice(destination, 1, 2).unwrap();
        heap.buffer_set(slice, 0, RuntimeValue::I32(5)).unwrap();
        assert_eq!(heap.buffer_get(slice, 0), Ok(RuntimeValue::I32(5)));
        assert_eq!(heap.buffer_get(destination, 1), Ok(RuntimeValue::I32(9)));

        let before = heap.buffer_values(destination).unwrap().to_vec();
        assert_eq!(
            heap.buffer_copy(destination, source, 2, 0, 2),
            Err(HeapError::IndexOutOfBounds {
                index: 4,
                length: 3,
            })
        );
        assert_eq!(heap.buffer_values(destination), Ok(before.as_slice()));
        assert_eq!(
            heap.buffer_get(destination, 4),
            Err(HeapError::IndexOutOfBounds {
                index: 4,
                length: 4,
            })
        );
        assert_eq!(
            heap.allocate_buffer(
                type_id,
                element,
                &[
                    RuntimeValue::I32(0),
                    RuntimeValue::I32(1),
                    RuntimeValue::I32(2),
                    RuntimeValue::I32(3),
                    RuntimeValue::I32(4),
                ],
            ),
            Err(HeapError::CollectionTooLarge {
                length: 5,
                max_length: 4,
            })
        );
    }

    #[test]
    fn buffer_elements_are_traced_from_the_buffer_root() {
        let mut heap = Heap::new_with_limits(4, usize::MAX, 4);
        let string = heap.allocate_string("kept").unwrap();
        let string_value = RuntimeValue::String {
            reference: string,
            hash: heap.string_hash(string).unwrap(),
        };
        let element = nexa_bytecode::ValueType::String;
        let buffer = heap
            .allocate_buffer(
                nexa_bytecode::buffer_type(element),
                element,
                &[string_value],
            )
            .unwrap();
        let RuntimeValue::NamedRef {
            reference: buffer_reference,
            ..
        } = buffer
        else {
            unreachable!("buffer allocations are named references")
        };
        let roots = GcRoots {
            running_frames: vec![buffer_reference],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().marked, 2);
        assert_eq!(heap.string(string), Ok("kept"));
    }

    #[test]
    fn maps_rehash_in_bounded_chunks_and_enforce_max_length_atomically() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 7);
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I64);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I64,
            )
            .unwrap();
        for key in 0..7 {
            loop {
                if heap
                    .map_set(
                        map,
                        RuntimeValue::I32(key),
                        RuntimeValue::I64(i64::from(key)),
                    )
                    .unwrap()
                    == MapSetOutcome::Complete
                {
                    break;
                }
                assert_eq!(
                    heap.map_len(map).unwrap(),
                    usize::try_from(key).expect("test keys are non-negative"),
                );
            }
        }
        assert_eq!(heap.map_len(map), Ok(7));
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(7), RuntimeValue::I64(7)),
            Err(HeapError::CollectionTooLarge {
                length: 8,
                max_length: 7,
            })
        );
        assert_eq!(
            heap.map_get(map, RuntimeValue::I32(4)),
            Ok(Some(RuntimeValue::I64(4)))
        );
        assert_eq!(heap.map_contains(map, RuntimeValue::I32(99)), Ok(false));
        assert_eq!(
            heap.map_remove(map, RuntimeValue::I32(2)),
            Ok(Some(RuntimeValue::I64(2)))
        );
        assert_eq!(heap.map_remove(map, RuntimeValue::I32(2)), Ok(None));
        heap.map_clear(map).unwrap();
        assert_eq!(heap.map_len(map), Ok(0));
    }

    #[test]
    fn map_keys_and_values_remain_gc_roots_during_rehash() {
        let mut heap = Heap::new_with_limits(20, usize::MAX, 16);
        let map_type = nexa_bytecode::map_type(
            nexa_bytecode::ValueType::String,
            nexa_bytecode::ValueType::String,
        );
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::String,
                nexa_bytecode::ValueType::String,
            )
            .unwrap();
        let mut strings = Vec::new();
        for index in 0..13 {
            let reference = heap.allocate_string(&format!("value-{index}")).unwrap();
            let value = RuntimeValue::String {
                reference,
                hash: heap.string_hash(reference).unwrap(),
            };
            strings.push(reference);
            if index < 12 {
                while heap.map_set(map, value, value).unwrap() == MapSetOutcome::RehashPending {}
            } else {
                assert_eq!(
                    heap.map_set(map, value, value).unwrap(),
                    MapSetOutcome::RehashPending
                );
                assert_eq!(
                    heap.map_set(map, value, value).unwrap(),
                    MapSetOutcome::RehashPending
                );
            }
        }
        let RuntimeValue::NamedRef {
            reference: map_reference,
            ..
        } = map
        else {
            unreachable!("map allocations are named references")
        };
        let roots = GcRoots {
            running_frames: vec![map_reference],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 13);
        assert!(
            strings[..12]
                .iter()
                .all(|reference| heap.string(*reference).is_ok())
        );
        assert!(heap.string(strings[12]).is_err());
    }
}
