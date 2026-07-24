use std::collections::VecDeque;
use std::fmt;

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
    Enum { tag: u32, payload: Option<GcRef> },
    Class { fields: Vec<GcRef> },
}

impl Object {
    fn references(&self) -> Vec<GcRef> {
        match self {
            Self::Class { fields } => fields.clone(),
            Self::Map(entries) => entries.iter().map(|(_, value)| *value).collect(),
            Self::Enum { payload, .. } => payload.iter().copied().collect(),
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
    InjectedAllocationFailure,
    InvalidReference(GcRef),
}

impl fmt::Display for HeapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HeapError {}

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
    fail_next_allocation: bool,
}

impl Heap {
    #[must_use]
    pub fn new(max_objects: u32) -> Self {
        Self {
            slots: Vec::with_capacity(max_objects as usize),
            free: Vec::with_capacity(max_objects as usize),
            max_objects,
            fail_next_allocation: false,
        }
    }

    pub fn allocate(&mut self, object: Object) -> Result<GcRef, HeapError> {
        if self.fail_next_allocation {
            self.fail_next_allocation = false;
            return Err(HeapError::InjectedAllocationFailure);
        }
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.object = Some(object);
            return Ok(GcRef {
                index,
                generation: slot.generation,
            });
        }
        let index = u32::try_from(self.slots.len()).map_err(|_| HeapError::CapacityExhausted)?;
        if index >= self.max_objects {
            return Err(HeapError::CapacityExhausted);
        }
        self.slots.push(ObjectSlot {
            generation: 0,
            marked: false,
            object: Some(object),
        });
        Ok(GcRef {
            index,
            generation: 0,
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

    pub fn inject_allocation_failure_once(&mut self) {
        self.fail_next_allocation = true;
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
    fn allocation_failure_does_not_drop_live_objects() {
        let mut heap = Heap::new(2);
        let live = heap.allocate(Object::I32Array(vec![1, 2])).unwrap();
        heap.inject_allocation_failure_once();
        assert_eq!(
            heap.allocate(Object::String("no".into())),
            Err(HeapError::InjectedAllocationFailure)
        );
        assert!(heap.resolve(live).is_ok());
    }
}
