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
    Class {
        fields: Vec<GcRef>,
    },
}

impl Object {
    fn references(&self) -> Vec<GcRef> {
        match self {
            Self::Class { fields } => fields.clone(),
            Self::Map(entries) => entries.iter().map(|(_, value)| *value).collect(),
            Self::Enum { payload, .. } => payload
                .iter()
                .filter_map(|payload| match payload {
                    RuntimeValue::String { reference, .. }
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

    pub fn resolve(&self, reference: GcRef) -> Result<&Object, HeapError> {
        let slot = self
            .slots
            .get(reference.index as usize)
            .filter(|slot| slot.generation == reference.generation)
            .and_then(|slot| slot.object.as_ref())
            .ok_or(HeapError::InvalidReference(reference))?;
        Ok(slot)
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

#[cfg(test)]
mod tests {
    use super::{GcRoots, Heap, HeapError, Object};
    use crate::RuntimeFailurePoint;

    #[test]
    fn cycles_collect_but_suspended_task_roots_survive() {
        let mut heap = Heap::new(4);
        let first = heap.allocate(Object::Class { fields: Vec::new() }).unwrap();
        let second = heap
            .allocate(Object::Class {
                fields: vec![first],
            })
            .unwrap();
        let Object::Class { fields } = heap.slots[first.index as usize].object.as_mut().unwrap()
        else {
            unreachable!()
        };
        fields.push(second);
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
        assert!(heap.allocate(Object::Class { fields: Vec::new() }).is_ok());
    }
}
