use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use nexa_core::StableId;

use crate::trusted::{ScalarArenaMut, ScalarArenaSet};
use crate::{RuntimeFailureInjector, RuntimeFailurePoint, RuntimeValue};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapEntry {
    key: RuntimeValue,
    hash: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MapRehash {
    old_slots: CollectionRange,
    new_slots: CollectionRange,
    old_values: CollectionRange,
    new_values: CollectionRange,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmMap {
    type_id: StableId,
    key_type: nexa_bytecode::ValueType,
    value_type: nexa_bytecode::ValueType,
    value_slots: u16,
    value_storage: CollectionStorage,
    slots: CollectionRange,
    values: CollectionRange,
    length: usize,
    rehash: Option<MapRehash>,
    /// `LANGUAGE_V3` 4.3: bumped on every observable structural mutation
    /// (insert of a new key, value overwrite, successful removal, and
    /// non-empty clear); `IterNew` snapshots it and every `IterNext`
    /// revalidates it so mutation during iteration traps deterministically.
    mutation_epoch: u64,
}

/// Key-only rehash state for a set: no companion value table exists.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SetRehash {
    old_slots: CollectionRange,
    new_slots: CollectionRange,
    cursor: usize,
}

/// `LANGUAGE_V3` `Set<T>`: dedicated key-only hash storage reusing the
/// proven `VmMap` linear-probe slot machinery. Never modeled as
/// `Map<T, Unit>`; entries are exactly `MapEntry` keys without any value
/// table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmSet {
    type_id: StableId,
    element_type: nexa_bytecode::ValueType,
    slots: CollectionRange,
    length: usize,
    rehash: Option<SetRehash>,
    /// `LANGUAGE_V3` 4.3 mutation epoch, snapshot by `IterNew` and
    /// revalidated by every `IterNext`.
    mutation_epoch: u64,
}

impl VmMap {
    // WP73: stream child references into the mark queue instead of
    // materializing a temporary Vec per object during GC.
    fn trace_references(
        &self,
        arena: &MapSlotArena,
        values: &CollectionArena,
        scalar_values: &ScalarArenaSet,
        visit: &mut impl FnMut(GcRef),
    ) {
        let mut trace_table = |slots: CollectionRange, value_range: CollectionRange| {
            for (index, entry) in arena.slots(slots).iter().copied().enumerate() {
                let Some(entry) = entry else {
                    continue;
                };
                if let Some(reference) = value_reference(entry.key) {
                    visit(reference);
                }
                let row = map_value_row(value_range, self.value_slots, index)
                    .expect("map value table matches its slot capacity");
                let row = typed_collection_view_from_arenas(
                    values,
                    scalar_values,
                    self.value_storage,
                    self.value_type,
                    row,
                )
                .expect("map value table remains live with its slots");
                for reference in row.iter().filter_map(value_reference) {
                    visit(reference);
                }
            }
        };
        trace_table(self.slots, self.values);
        if let Some(rehash) = &self.rehash {
            trace_table(rehash.old_slots, rehash.old_values);
            trace_table(rehash.new_slots, rehash.new_values);
        }
    }

    /// G4 byte accounting: system bytes held by the slot vectors,
    /// including both sides of an in-flight incremental rehash.
    fn storage_bytes(&self) -> usize {
        let slot_bytes = size_of::<Option<MapEntry>>();
        let (rehash_slots, rehash_values) = self.rehash.as_ref().map_or((0, 0), |rehash| {
            (
                rehash
                    .old_slots
                    .length
                    .saturating_add(rehash.new_slots.length),
                rehash
                    .old_values
                    .length
                    .saturating_add(rehash.new_values.length),
            )
        });
        self.slots
            .length
            .saturating_add(rehash_slots)
            .saturating_mul(slot_bytes)
            .saturating_add(
                self.values
                    .length
                    .saturating_add(rehash_values)
                    .saturating_mul(self.value_storage.cell_size()),
            )
    }
}

impl VmSet {
    fn trace_references(&self, arena: &MapSlotArena, visit: &mut impl FnMut(GcRef)) {
        let mut trace_table = |slots: CollectionRange| {
            for entry in arena.slots(slots).iter().copied().flatten() {
                if let Some(reference) = value_reference(entry.key) {
                    visit(reference);
                }
            }
        };
        trace_table(self.slots);
        if let Some(rehash) = &self.rehash {
            trace_table(rehash.old_slots);
            trace_table(rehash.new_slots);
        }
    }

    /// G4 byte accounting: system bytes held by the set's key slot vectors,
    /// including both sides of an in-flight incremental rehash.
    fn storage_bytes(&self) -> usize {
        let slot_bytes = size_of::<Option<MapEntry>>();
        let rehash_slots = self.rehash.as_ref().map_or(0, |rehash| {
            rehash
                .old_slots
                .length
                .saturating_add(rehash.new_slots.length)
        });
        self.slots
            .length
            .saturating_add(rehash_slots)
            .saturating_mul(slot_bytes)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MapEntries<'a> {
    current: &'a [Option<MapEntry>],
    old: &'a [Option<MapEntry>],
    new: &'a [Option<MapEntry>],
    values: &'a CollectionArena,
    scalar_values: &'a ScalarArenaSet,
    current_values: CollectionRange,
    old_values: CollectionRange,
    new_values: CollectionRange,
    value_type: nexa_bytecode::ValueType,
    value_slots: u16,
    value_storage: CollectionStorage,
    phase: u8,
    index: usize,
    remaining: usize,
}

impl Iterator for MapEntries<'_> {
    type Item = (RuntimeValue, RuntimeValue);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let slots = match self.phase {
                0 => self.current,
                1 => self.old,
                2 => self.new,
                _ => return None,
            };
            let Some(slot) = slots.get(self.index) else {
                self.phase += 1;
                self.index = 0;
                continue;
            };
            self.index += 1;
            if let Some(entry) = slot {
                self.remaining = self
                    .remaining
                    .checked_sub(1)
                    .expect("map length matches occupied slots");
                let values = match self.phase {
                    0 => self.current_values,
                    1 => self.old_values,
                    2 => self.new_values,
                    _ => unreachable!(),
                };
                let row = map_value_row(values, self.value_slots, self.index - 1)
                    .expect("scalar Host map table has a matching value row");
                let value = typed_collection_view_from_arenas(
                    self.values,
                    self.scalar_values,
                    self.value_storage,
                    self.value_type,
                    row,
                )
                .expect("scalar Host map entry has a live value extent")
                .get(0)
                .expect("scalar Host map entry has one physical value");
                return Some((entry.key, value));
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for MapEntries<'_> {
    fn len(&self) -> usize {
        self.remaining
    }
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct SetEntries<'a> {
    current: &'a [Option<MapEntry>],
    old: &'a [Option<MapEntry>],
    new: &'a [Option<MapEntry>],
    phase: u8,
    index: usize,
    remaining: usize,
}

#[cfg(test)]
impl Iterator for SetEntries<'_> {
    type Item = RuntimeValue;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let slots = match self.phase {
                0 => self.current,
                1 => self.old,
                2 => self.new,
                _ => return None,
            };
            let Some(slot) = slots.get(self.index) else {
                self.phase += 1;
                self.index = 0;
                continue;
            };
            self.index += 1;
            if let Some(entry) = slot {
                self.remaining = self
                    .remaining
                    .checked_sub(1)
                    .expect("set length matches occupied slots");
                return Some(entry.key);
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[cfg(test)]
impl ExactSizeIterator for SetEntries<'_> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapSetOutcome {
    Complete,
    RehashPending,
}

/// `LANGUAGE_V3` `SetInsert` outcome: `Complete(bool)` reports whether the
/// element was newly inserted; `RehashPending` retries without re-hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetInsertOutcome {
    Complete(bool),
    RehashPending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StringSplitFuelShape {
    pub source_bytes: usize,
    pub delimiter_bytes: usize,
    pub parts: usize,
}

/// O(1) collection-arena metadata used to settle deterministic fuel before an
/// operation searches or mutates the free-range index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CollectionArenaFuelShape {
    pub free_ranges: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MapFuelShape {
    pub current_slots: usize,
    pub old_slots: usize,
    pub new_slots: usize,
    pub rehash_remaining: usize,
    pub next_rehash_slots: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SetFuelShape {
    pub current_slots: usize,
    pub old_slots: usize,
    pub new_slots: usize,
    pub rehash_remaining: usize,
    pub next_rehash_slots: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MapKeyFuelShape {
    pub string_bytes: usize,
    pub string_objects: usize,
    pub structural_objects: usize,
    pub fields_per_object: usize,
    pub hash_structural_objects: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MapLocation {
    Current(usize),
    RehashOld(usize),
    RehashNew(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetLocation {
    Current(usize),
    RehashOld(usize),
    RehashNew(usize),
}

/// Slot-table surface shared by `VmMap` and `VmSet` so the deterministic
/// phase/slot iteration cursor walks either collection identically.
trait SlotTable {
    fn slots_for<'a>(&self, heap: &'a Heap, phase: u8) -> Option<&'a [Option<MapEntry>]>;
}

impl SlotTable for VmMap {
    fn slots_for<'a>(&self, heap: &'a Heap, phase: u8) -> Option<&'a [Option<MapEntry>]> {
        match phase {
            0 => Some(heap.map_slots.slots(self.slots)),
            1 => self
                .rehash
                .as_ref()
                .map(|rehash| heap.map_slots.slots(rehash.old_slots)),
            2 => self
                .rehash
                .as_ref()
                .map(|rehash| heap.map_slots.slots(rehash.new_slots)),
            _ => None,
        }
    }
}

impl SlotTable for VmSet {
    fn slots_for<'a>(&self, heap: &'a Heap, phase: u8) -> Option<&'a [Option<MapEntry>]> {
        match phase {
            0 => Some(heap.map_slots.slots(self.slots)),
            1 => self
                .rehash
                .as_ref()
                .map(|rehash| heap.map_slots.slots(rehash.old_slots)),
            2 => self
                .rehash
                .as_ref()
                .map(|rehash| heap.map_slots.slots(rehash.new_slots)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GcRef {
    pub index: u32,
    pub generation: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionRange {
    pub start: usize,
    pub length: usize,
}

impl CollectionRange {
    const fn end(self) -> usize {
        self.start + self.length
    }
}

fn restore_reserved_vec<T>(target: &mut Vec<T>, snapshot: Vec<T>) {
    debug_assert!(
        snapshot.len() <= target.capacity(),
        "heap restore must fit the constructor-reserved capacity"
    );
    target.clear();
    target.extend(snapshot);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionArena {
    values: Vec<RuntimeValue>,
    free_ranges: Vec<CollectionRange>,
    capacity: usize,
}

fn claim_scalar_regrow<T: Copy>(
    arena: &mut ScalarArenaMut<'_, T>,
    new_range: CollectionRange,
    old_range: CollectionRange,
    live: usize,
    write: impl FnOnce(&mut [T]),
) -> Result<CollectionRange, HeapError> {
    arena.claim_exact(new_range)?;
    let capacity = new_range.length;
    let old_start = old_range.start;
    let old_end = old_range.end();
    let new_start = new_range.start;
    let new_end = new_range.end();
    if old_range.length != 0 && new_end > old_start && old_end > new_start {
        arena.release(new_range);
        return Err(HeapError::CapacityExhausted);
    }
    let arena_length = arena.length();
    let all_values = arena.values_mut(CollectionRange {
        start: 0,
        length: arena_length,
    })?;
    if old_range.length == 0 {
        write(&mut all_values[new_start..new_end]);
    } else {
        let (destination, source) = if new_end <= old_start {
            let (left, right) = all_values.split_at_mut(old_start);
            (&mut left[new_start..new_end], &right[..old_range.length])
        } else {
            let (left, right) = all_values.split_at_mut(new_start);
            (&mut right[..capacity], &left[old_start..old_end])
        };
        destination[..live].copy_from_slice(&source[..live]);
        write(destination);
    }
    Ok(new_range)
}

/// Preallocated typed storage for Map entries. Extents move between maps
/// during incremental rehash without asking the system allocator for a new
/// `Vec`, so Map creation and its common growth path stay allocation-free
/// after Heap construction.
#[derive(Debug)]
struct MapSlotArena {
    values: Vec<Option<MapEntry>>,
    free_ranges: Vec<CollectionRange>,
}

impl MapSlotArena {
    fn new(capacity: usize, max_ranges: usize) -> Self {
        let mut free_ranges = Vec::with_capacity(max_ranges.max(1));
        if capacity != 0 {
            free_ranges.push(CollectionRange {
                start: 0,
                length: capacity,
            });
        }
        Self {
            values: Vec::with_capacity(capacity),
            free_ranges,
        }
    }

    fn claim(&mut self, count: usize) -> Result<CollectionRange, HeapError> {
        if count == 0 {
            return Ok(CollectionRange::default());
        }
        let index = self
            .free_ranges
            .iter()
            .position(|range| range.length >= count)
            .ok_or(HeapError::CapacityExhausted)?;
        let range = CollectionRange {
            start: self.free_ranges[index].start,
            length: count,
        };
        if self.free_ranges[index].length == count {
            self.free_ranges.remove(index);
        } else {
            self.free_ranges[index].start += count;
            self.free_ranges[index].length -= count;
        }
        if self.values.len() < range.end() {
            // The full address capacity was reserved at Heap construction;
            // extending the initialized prefix cannot invoke the allocator.
            self.values.resize(range.end(), None);
        }
        Ok(range)
    }

    fn release(&mut self, range: CollectionRange) {
        if range.length == 0 {
            return;
        }
        self.values[range.start..range.end()].fill(None);
        let insertion = self
            .free_ranges
            .partition_point(|candidate| candidate.start < range.start);
        debug_assert!(self.free_ranges.len() < self.free_ranges.capacity());
        self.free_ranges.insert(insertion, range);
        let mut index = insertion.saturating_sub(1);
        while index + 1 < self.free_ranges.len() {
            let left = self.free_ranges[index];
            let right = self.free_ranges[index + 1];
            if left.end() < right.start {
                index += 1;
                continue;
            }
            debug_assert!(left.end() <= right.start, "map slot ranges overlap");
            self.free_ranges[index].length = right.end() - left.start;
            self.free_ranges.remove(index + 1);
        }
    }

    fn slots(&self, range: CollectionRange) -> &[Option<MapEntry>] {
        &self.values[range.start..range.end()]
    }

    fn slots_mut(&mut self, range: CollectionRange) -> &mut [Option<MapEntry>] {
        &mut self.values[range.start..range.end()]
    }
}

impl Clone for MapSlotArena {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            free_ranges: self.free_ranges.clone(),
        }
    }
}

impl MapSlotArena {
    fn restore_checkpoint(&mut self, checkpoint: Self) {
        restore_reserved_vec(&mut self.values, checkpoint.values);
        restore_reserved_vec(&mut self.free_ranges, checkpoint.free_ranges);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionArenaInspection {
    pub capacity: usize,
    pub free_elements: usize,
    pub free_ranges: usize,
}

/// `GC_V1` byte accounting by category (G4).
///
/// `object_header_bytes` counts occupied physical slots plus occupied typed
/// header-arena cells (currently maps). It includes the inline Enum payload,
/// but never separately allocated collection/map storage. Class/Struct fields
/// live in exclusive collection-arena extents, so `class_payload_bytes` is an
/// out-of-slot category and participates in [`Self::total`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapByteInspection {
    pub object_header_bytes: u64,
    pub class_payload_bytes: u64,
    pub string_bytes: u64,
    pub array_bytes: u64,
    pub buffer_bytes: u64,
    pub map_bytes: u64,
    pub allocator_slack_bytes: u64,
    pub profiler_bytes: u64,
}

impl HeapByteInspection {
    /// Bytes owned by live collection/map arena extents. Class and Struct
    /// fields share the collection arena and therefore participate.
    #[must_use]
    pub const fn collection_total(&self) -> u64 {
        self.class_payload_bytes
            .saturating_add(self.array_bytes)
            .saturating_add(self.buffer_bytes)
            .saturating_add(self.map_bytes)
    }

    /// Bytes owned by live VM objects, excluding reserved slack and profiler
    /// storage.
    #[must_use]
    pub const fn live_total(&self) -> u64 {
        self.object_header_bytes
            .saturating_add(self.class_payload_bytes)
            .saturating_add(self.string_bytes)
            .saturating_add(self.array_bytes)
            .saturating_add(self.buffer_bytes)
            .saturating_add(self.map_bytes)
    }

    /// Exclusive-category sum: headers, out-of-slot payloads, slack, and
    /// profiler storage.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.live_total()
            .saturating_add(self.allocator_slack_bytes)
            .saturating_add(self.profiler_bytes)
    }
}

impl CollectionArena {
    fn new(capacity: usize, max_ranges: usize) -> Self {
        let mut free_ranges = Vec::with_capacity(max_ranges.max(1));
        if capacity != 0 {
            free_ranges.push(CollectionRange {
                start: 0,
                length: capacity,
            });
        }
        Self {
            values: Vec::with_capacity(capacity),
            free_ranges,
            capacity,
        }
    }

    fn find_free(&self, count: usize) -> Option<CollectionRange> {
        if count == 0 {
            return Some(CollectionRange::default());
        }
        self.free_ranges
            .iter()
            .copied()
            .find(|range| range.length >= count)
            .map(|range| CollectionRange {
                start: range.start,
                length: count,
            })
    }

    fn claim(&mut self, range: CollectionRange) -> Result<(), HeapError> {
        if range.length == 0 {
            return Ok(());
        }
        let index = self
            .free_ranges
            .iter()
            .position(|free| {
                range.start >= free.start
                    && range.end() <= free.end()
                    && range.end() <= self.capacity
            })
            .ok_or(HeapError::CapacityExhausted)?;
        let free = self.free_ranges[index];
        let prefix = range.start - free.start;
        let suffix = free.end() - range.end();
        match (prefix, suffix) {
            (0, 0) => {
                self.free_ranges.remove(index);
            }
            (0, _) => {
                self.free_ranges[index] = CollectionRange {
                    start: range.end(),
                    length: suffix,
                };
            }
            (_, 0) => self.free_ranges[index].length = prefix,
            (_, _) => {
                if self.free_ranges.len() == self.free_ranges.capacity() {
                    return Err(HeapError::CapacityExhausted);
                }
                self.free_ranges[index].length = prefix;
                self.free_ranges.insert(
                    index + 1,
                    CollectionRange {
                        start: range.end(),
                        length: suffix,
                    },
                );
            }
        }
        Ok(())
    }

    fn release(&mut self, range: CollectionRange) {
        if range.length == 0 {
            return;
        }
        if range.start < self.values.len() {
            let initialized_end = range.end().min(self.values.len());
            self.values[range.start..initialized_end].fill(RuntimeValue::Unit);
        }
        let insertion = self
            .free_ranges
            .partition_point(|candidate| candidate.start < range.start);
        debug_assert!(self.free_ranges.len() < self.free_ranges.capacity());
        self.free_ranges.insert(insertion, range);
        let mut index = insertion.saturating_sub(1);
        while index + 1 < self.free_ranges.len() {
            let left = self.free_ranges[index];
            let right = self.free_ranges[index + 1];
            if left.end() < right.start {
                index += 1;
                continue;
            }
            debug_assert!(left.end() <= right.start, "collection ranges overlap");
            self.free_ranges[index].length = right.end() - left.start;
            self.free_ranges.remove(index + 1);
        }
    }

    fn values(&self, range: CollectionRange) -> Result<&[RuntimeValue], HeapError> {
        self.values
            .get(range.start..range.end())
            .ok_or(HeapError::IndexOutOfBounds {
                index: range.end(),
                length: self.capacity,
            })
    }

    fn values_mut(&mut self, range: CollectionRange) -> Result<&mut [RuntimeValue], HeapError> {
        self.values
            .get_mut(range.start..range.end())
            .ok_or(HeapError::IndexOutOfBounds {
                index: range.end(),
                length: self.capacity,
            })
    }

    fn initialize(&mut self, range: CollectionRange) -> Result<(), HeapError> {
        if range.end() > self.capacity {
            return Err(HeapError::CapacityExhausted);
        }
        if self.values.len() < range.end() {
            self.values.resize(range.end(), RuntimeValue::Unit);
        }
        Ok(())
    }

    fn checkpoint_clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            free_ranges: self.free_ranges.clone(),
            capacity: self.capacity,
        }
    }

    fn restore_checkpoint(&mut self, checkpoint: Self) {
        debug_assert_eq!(self.capacity, checkpoint.capacity);
        restore_reserved_vec(&mut self.values, checkpoint.values);
        restore_reserved_vec(&mut self.free_ranges, checkpoint.free_ranges);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Object {
    String(String),
    /// Immutable module-owned string storage (WP56). The heap owns only an
    /// `Arc` reference and a GC header; literal bytes are allocated once by
    /// `ExecutableModule`, never on `LoadString`.
    SharedString(Arc<str>),
    /// WP72: map headers live in the heap's typed map arena. The physical
    /// object slot carries only the arena index instead of the widest
    /// `VmMap` header (including its optional rehash state).
    Map {
        storage: u32,
    },
    /// `LANGUAGE_V3`: set headers live in the heap's typed set arena, the
    /// same slot-arena indirection as maps; the slot carries the arena
    /// index instead of the full `VmSet` header.
    Set {
        storage: u32,
    },
    Enum {
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    },
    // K5: Struct/Class fields live in the collection arena (`field_count`
    // cells starting at `range.start`), so every heap slot stops carrying
    // the maximum inline field array (GC_V1: slots must not bear the
    // widest object-enum footprint). Extents are claimed at construction
    // and released by sweep/rollback exactly like Array extents.
    Struct {
        type_id: StableId,
        storage: CollectionStorage,
        range: CollectionRange,
        field_count: u16,
        hash: u64,
    },
    Class {
        type_id: StableId,
        storage: CollectionStorage,
        range: CollectionRange,
        field_count: u16,
    },
    Array {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        storage: CollectionStorage,
        /// Capacity extent inside the collection arena (WP48); `length`
        /// tracks the live prefix so pushes grow amortized (WP49). The
        /// extent is measured in arena cells: `row_stride` cells per
        /// logical element for flattened struct rows, one otherwise.
        range: CollectionRange,
        /// Logical element count, independent of the row stride.
        length: usize,
        /// WP52: `Some(fields)` flattens struct elements into `fields`
        /// arena cells per element instead of one heap object each;
        /// `None` keeps the plain one-cell-per-element layout.
        row_stride: Option<std::num::NonZeroU16>,
        /// `LANGUAGE_V3` 4.3 mutation epoch; every observable element or
        /// structural write bumps it so dynamic iteration traps on mixed
        /// snapshots.
        mutation_epoch: u64,
    },
    Buffer {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        storage: CollectionStorage,
        range: CollectionRange,
        /// `LANGUAGE_V3` 4.3 mutation epoch; every observable element write
        /// (set/fill/copy destination) bumps it.
        mutation_epoch: u64,
    },
}

impl Object {
    // WP73: allocation-free reference traversal for the GC mark phase.
    // Array/Buffer extents and (K5) Struct/Class field extents live in
    // the collection arena and are traced directly inside `collect`,
    // which owns the arena borrow.
    fn trace_references(&self, visit: &mut impl FnMut(GcRef)) {
        match self {
            Self::Enum { payload, .. } => {
                for reference in payload.iter().copied().filter_map(value_reference) {
                    visit(reference);
                }
            }
            Self::Array { .. }
            | Self::Buffer { .. }
            | Self::Struct { .. }
            | Self::Class { .. }
            | Self::Map { .. }
            | Self::Set { .. }
            | Self::String(_)
            | Self::SharedString(_) => {}
        }
    }

    /// G4 byte accounting: bytes this object owns *outside* its slot -
    /// system allocations (String storage, i32 backing, map slot vectors)
    /// plus exclusively held collection-arena extents (Array/Buffer
    /// capacity and, since K5, Struct/Class field extents). Enum payloads
    /// are inline in the slot and report zero here; the slot header
    /// itself is pool-owned and accounted separately.
    fn payload_bytes(&self) -> u64 {
        let bytes = match self {
            Self::String(text) => text.capacity(),
            Self::Array { range, storage, .. }
            | Self::Buffer { range, storage, .. }
            | Self::Struct { storage, range, .. }
            | Self::Class { storage, range, .. } => {
                range.length.saturating_mul(storage.cell_size())
            }
            // Shared literal bytes belong to ExecutableModule; Map/Set
            // payload lives in their typed arenas; Enum payload remains
            // inline.
            Self::SharedString(_) | Self::Map { .. } | Self::Set { .. } | Self::Enum { .. } => 0,
        };
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }
}

/// Physical arena selected for one logical collection element. Named values
/// are deliberately split between flattened/wide values and references:
/// `ValueType::Named` alone cannot distinguish structs from classes/enums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionStorage {
    Values,
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    Ref,
    NamedRef,
}

impl CollectionStorage {
    pub(crate) const fn for_type(element_type: nexa_bytecode::ValueType) -> Self {
        match element_type {
            nexa_bytecode::ValueType::I32 => Self::I32,
            nexa_bytecode::ValueType::I64 => Self::I64,
            nexa_bytecode::ValueType::F32 => Self::F32,
            nexa_bytecode::ValueType::F64 => Self::F64,
            nexa_bytecode::ValueType::Bool => Self::Bool,
            nexa_bytecode::ValueType::Rune => Self::Rune,
            nexa_bytecode::ValueType::String => Self::String,
            nexa_bytecode::ValueType::Ref => Self::Ref,
            nexa_bytecode::ValueType::Named(_) => Self::NamedRef,
        }
    }

    pub(crate) const fn cell_size(self) -> usize {
        match self {
            Self::I32 | Self::F32 | Self::Rune => 4,
            Self::I64 | Self::F64 | Self::Ref | Self::NamedRef => 8,
            Self::Bool => 1,
            Self::String => size_of::<(GcRef, u64)>(),
            Self::Values => size_of::<RuntimeValue>(),
        }
    }

    const fn is_compact(self) -> bool {
        !matches!(self, Self::Values)
    }
}

const fn default_map_value_storage(
    value_type: nexa_bytecode::ValueType,
    value_slots: u16,
) -> CollectionStorage {
    if value_slots != 1 {
        return CollectionStorage::Values;
    }
    match value_type {
        nexa_bytecode::ValueType::Named(_) => CollectionStorage::Values,
        other => CollectionStorage::for_type(other),
    }
}

fn map_value_storage_for_layout(
    value_type: nexa_bytecode::ValueType,
    layout: &nexa_bytecode::layout::ValueLayout,
) -> CollectionStorage {
    use nexa_bytecode::layout::{CopyStrategy, PhysicalSlotKind};

    if layout.physical_slots != 1 {
        return CollectionStorage::Values;
    }
    let nexa_bytecode::ValueType::Named(_) = value_type else {
        return CollectionStorage::for_type(value_type);
    };

    if layout.copy_strategy == CopyStrategy::ReferenceShare
        && layout.slot_kinds.as_slice() == [PhysicalSlotKind::GcReference]
    {
        return CollectionStorage::NamedRef;
    }
    if layout
        .enum_layout
        .as_ref()
        .is_some_and(|enum_layout| enum_layout.payload_slots == 0)
    {
        return CollectionStorage::I32;
    }

    // A one-slot flattened struct can share the exact scalar arena of its
    // physical field. Named reference fields stay wide because reconstructing
    // them requires the nested field's type id, not the wrapper's type id.
    let Some(field) = layout
        .field_offsets
        .iter()
        .find(|field| field.offset == 0 && field.slots == 1)
    else {
        return CollectionStorage::Values;
    };
    match field.logical_type {
        nexa_bytecode::ValueType::Named(_) => CollectionStorage::Values,
        other => CollectionStorage::for_type(other),
    }
}

fn collection_storage_for_values(
    element_type: nexa_bytecode::ValueType,
    values: &[RuntimeValue],
) -> Result<CollectionStorage, HeapError> {
    let storage = match element_type {
        nexa_bytecode::ValueType::Named(expected) => match values.first().copied() {
            Some(RuntimeValue::NamedRef { type_id, .. }) if type_id == expected => {
                CollectionStorage::NamedRef
            }
            Some(RuntimeValue::Struct { type_id, .. }) if type_id == expected => {
                CollectionStorage::Values
            }
            None => CollectionStorage::Values,
            Some(_) => return Err(invalid_value_reference()),
        },
        other => CollectionStorage::for_type(other),
    };
    Ok(storage)
}

/// Selects a compact physical arena for a homogeneous object field row.
///
/// Field signatures are not retained by `Heap`, so named values stay in the
/// wide arena: reconstructing a named reference requires its declared
/// `StableId`. Scalars, strings and untyped references carry all information
/// needed at the storage boundary and can safely use their exact-width arena.
fn homogeneous_field_storage(fields: &[RuntimeValue]) -> CollectionStorage {
    let Some(first) = fields.first().copied() else {
        return CollectionStorage::Values;
    };
    let storage = match first {
        RuntimeValue::I32(_) => CollectionStorage::I32,
        RuntimeValue::I64(_) => CollectionStorage::I64,
        RuntimeValue::F32(_) => CollectionStorage::F32,
        RuntimeValue::F64(_) => CollectionStorage::F64,
        RuntimeValue::Bool(_) => CollectionStorage::Bool,
        RuntimeValue::Rune(_) => CollectionStorage::Rune,
        RuntimeValue::String { .. } => CollectionStorage::String,
        RuntimeValue::Ref(_) => CollectionStorage::Ref,
        _ => return CollectionStorage::Values,
    };
    if fields
        .iter()
        .copied()
        .all(|value| field_value_matches_storage(value, storage))
    {
        storage
    } else {
        CollectionStorage::Values
    }
}

const fn field_value_matches_storage(value: RuntimeValue, storage: CollectionStorage) -> bool {
    matches!(
        (value, storage),
        (RuntimeValue::I32(_), CollectionStorage::I32)
            | (RuntimeValue::I64(_), CollectionStorage::I64)
            | (RuntimeValue::F32(_), CollectionStorage::F32)
            | (RuntimeValue::F64(_), CollectionStorage::F64)
            | (RuntimeValue::Bool(_), CollectionStorage::Bool)
            | (RuntimeValue::Rune(_), CollectionStorage::Rune)
            | (RuntimeValue::String { .. }, CollectionStorage::String)
            | (RuntimeValue::Ref(_), CollectionStorage::Ref)
    )
}

const fn field_type_for_storage(storage: CollectionStorage) -> Option<nexa_bytecode::ValueType> {
    match storage {
        CollectionStorage::I32 => Some(nexa_bytecode::ValueType::I32),
        CollectionStorage::I64 => Some(nexa_bytecode::ValueType::I64),
        CollectionStorage::F32 => Some(nexa_bytecode::ValueType::F32),
        CollectionStorage::F64 => Some(nexa_bytecode::ValueType::F64),
        CollectionStorage::Bool => Some(nexa_bytecode::ValueType::Bool),
        CollectionStorage::Rune => Some(nexa_bytecode::ValueType::Rune),
        CollectionStorage::String => Some(nexa_bytecode::ValueType::String),
        CollectionStorage::Ref => Some(nexa_bytecode::ValueType::Ref),
        CollectionStorage::Values | CollectionStorage::NamedRef => None,
    }
}

/// Bounded geometric growth for the array capacity extent (WP49): at least
/// four slots, at most double, never past the collection length ceiling.
const fn grown_array_capacity(current: usize, needed: usize, ceiling: usize) -> usize {
    let doubled = current.saturating_mul(2);
    let mut capacity = if doubled < 4 { 4 } else { doubled };
    if capacity < needed {
        capacity = needed;
    }
    if capacity > ceiling {
        ceiling
    } else {
        capacity
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

/// Deterministic FNV-1a content hash shared by string values and the
/// WP56 literal cache; computed once per interned literal (WP69).
pub(crate) fn fnv_content_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// One WP56 literal-cache entry: the shared live copy plus its content
/// hash, computed once at interning time (WP69 hot-path discipline).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedStringLiteral {
    reference: GcRef,
    hash: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedPooledString {
    pool: u64,
    literal: u32,
    reference: GcRef,
    hash: u64,
}

/// Borrowed WP52 row view over a struct-element array: the live flattened
/// cells, the per-element stride, and the element struct type.
#[derive(Clone, Copy, Debug)]
pub struct ArrayRowsView<'a> {
    pub cells: &'a [RuntimeValue],
    pub stride: usize,
    pub struct_type: StableId,
}

/// Borrowed logical collection elements over their physical WP47 storage.
/// Scalar variants reconstruct `RuntimeValue` only at the API edge; the VM
/// hot path reads and writes the compact typed cells directly.
#[derive(Clone, Copy, Debug)]
pub enum CollectionView<'a> {
    Values(&'a [RuntimeValue]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    F32(&'a [u32]),
    F64(&'a [u64]),
    Bool(&'a [u8]),
    Rune(&'a [u32]),
    String(&'a [(GcRef, u64)]),
    Ref(&'a [GcRef]),
    NamedRef {
        values: &'a [GcRef],
        type_id: StableId,
    },
}

fn typed_collection_view_from_arenas<'a>(
    values: &'a CollectionArena,
    scalar_values: &'a ScalarArenaSet,
    storage: CollectionStorage,
    element_type: nexa_bytecode::ValueType,
    range: CollectionRange,
) -> Result<CollectionView<'a>, HeapError> {
    match storage {
        CollectionStorage::Values => Ok(CollectionView::Values(values.values(range)?)),
        CollectionStorage::I32 => Ok(CollectionView::I32(scalar_values.i32().values(range)?)),
        CollectionStorage::I64 => Ok(CollectionView::I64(scalar_values.i64().values(range)?)),
        CollectionStorage::F32 => Ok(CollectionView::F32(scalar_values.f32().values(range)?)),
        CollectionStorage::F64 => Ok(CollectionView::F64(scalar_values.f64().values(range)?)),
        CollectionStorage::Bool => Ok(CollectionView::Bool(scalar_values.bools().values(range)?)),
        CollectionStorage::Rune => Ok(CollectionView::Rune(scalar_values.runes().values(range)?)),
        CollectionStorage::String => Ok(CollectionView::String(
            scalar_values.strings().values(range)?,
        )),
        CollectionStorage::Ref => Ok(CollectionView::Ref(scalar_values.refs().values(range)?)),
        CollectionStorage::NamedRef => {
            let nexa_bytecode::ValueType::Named(type_id) = element_type else {
                return Err(invalid_value_reference());
            };
            Ok(CollectionView::NamedRef {
                values: scalar_values.refs().values(range)?,
                type_id,
            })
        }
    }
}

impl<'a> CollectionView<'a> {
    #[must_use]
    pub const fn len(self) -> usize {
        match self {
            Self::Values(values) => values.len(),
            Self::I32(values) => values.len(),
            Self::I64(values) => values.len(),
            Self::F32(values) | Self::Rune(values) => values.len(),
            Self::F64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
            Self::Ref(values) | Self::NamedRef { values, .. } => values.len(),
        }
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn get(self, index: usize) -> Option<RuntimeValue> {
        match self {
            Self::Values(values) => values.get(index).copied(),
            Self::I32(values) => values.get(index).copied().map(RuntimeValue::I32),
            Self::I64(values) => values.get(index).copied().map(RuntimeValue::I64),
            Self::F32(values) => values.get(index).copied().map(RuntimeValue::F32),
            Self::F64(values) => values.get(index).copied().map(RuntimeValue::F64),
            Self::Bool(values) => values
                .get(index)
                .copied()
                .map(|value| RuntimeValue::Bool(value != 0)),
            Self::Rune(values) => values.get(index).copied().map(RuntimeValue::Rune),
            Self::String(values) => values
                .get(index)
                .copied()
                .map(|(reference, hash)| RuntimeValue::String { reference, hash }),
            Self::Ref(values) => values.get(index).copied().map(RuntimeValue::Ref),
            Self::NamedRef { values, type_id } => values
                .get(index)
                .copied()
                .map(|reference| RuntimeValue::NamedRef { reference, type_id }),
        }
    }

    #[must_use]
    pub fn iter(self) -> impl ExactSizeIterator<Item = RuntimeValue> + 'a {
        (0..self.len()).map(move |index| {
            self.get(index)
                .expect("collection view iterator stays within bounds")
        })
    }

    fn prefix(self, length: usize) -> Option<Self> {
        match self {
            Self::Values(values) => values.get(..length).map(Self::Values),
            Self::I32(values) => values.get(..length).map(Self::I32),
            Self::I64(values) => values.get(..length).map(Self::I64),
            Self::F32(values) => values.get(..length).map(Self::F32),
            Self::F64(values) => values.get(..length).map(Self::F64),
            Self::Bool(values) => values.get(..length).map(Self::Bool),
            Self::Rune(values) => values.get(..length).map(Self::Rune),
            Self::String(values) => values.get(..length).map(Self::String),
            Self::Ref(values) => values.get(..length).map(Self::Ref),
            Self::NamedRef { values, type_id } => values
                .get(..length)
                .map(|values| Self::NamedRef { values, type_id }),
        }
    }

    fn slice(self, start: usize, length: usize) -> Option<Self> {
        let end = start.checked_add(length)?;
        match self {
            Self::Values(values) => values.get(start..end).map(Self::Values),
            Self::I32(values) => values.get(start..end).map(Self::I32),
            Self::I64(values) => values.get(start..end).map(Self::I64),
            Self::F32(values) => values.get(start..end).map(Self::F32),
            Self::F64(values) => values.get(start..end).map(Self::F64),
            Self::Bool(values) => values.get(start..end).map(Self::Bool),
            Self::Rune(values) => values.get(start..end).map(Self::Rune),
            Self::String(values) => values.get(start..end).map(Self::String),
            Self::Ref(values) => values.get(start..end).map(Self::Ref),
            Self::NamedRef { values, type_id } => values
                .get(start..end)
                .map(|values| Self::NamedRef { values, type_id }),
        }
    }
}

/// Resolved array header shared by every logical array operation (WP52).
#[derive(Clone, Copy)]
struct ArrayParts {
    reference: GcRef,
    range: CollectionRange,
    /// Logical element count.
    length: usize,
    row_stride: Option<std::num::NonZeroU16>,
    element_type: nexa_bytecode::ValueType,
    storage: CollectionStorage,
}

impl ArrayParts {
    /// Arena cells per logical element.
    fn stride(self) -> usize {
        self.row_stride
            .map_or(1, |stride| usize::from(stride.get()))
    }

    /// `Some(stride)` when elements are flattened physical value rows.
    fn rows(self) -> Option<usize> {
        self.row_stride.map(|stride| usize::from(stride.get()))
    }

    fn element_struct_type(self) -> Result<StableId, HeapError> {
        match self.element_type {
            nexa_bytecode::ValueType::Named(id) => Ok(id),
            _ => Err(invalid_value_reference()),
        }
    }
}

/// One validated buffer header. Buffer operations keep this compact copy
/// instead of resolving the same heap object separately for type, storage,
/// and range metadata.
#[derive(Clone, Copy)]
struct BufferParts {
    type_id: StableId,
    reference: GcRef,
    element_type: nexa_bytecode::ValueType,
    storage: CollectionStorage,
    range: CollectionRange,
}

/// A buffer copy whose handles, element layout, and logical bounds were
/// resolved once before a certified leaf starts mutating the heap.
#[derive(Clone, Copy)]
pub(crate) struct PreparedBufferCopy {
    destination: BufferParts,
    source_absolute: usize,
    destination_absolute: usize,
    destination_start: usize,
    length: usize,
}

/// A buffer read whose handle and logical index were resolved once during
/// certified-leaf admission.
#[derive(Clone, Copy)]
pub(crate) struct PreparedBufferGet {
    storage: CollectionStorage,
    element_type: nexa_bytecode::ValueType,
    absolute_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    CapacityExhausted,
    StringTooLarge {
        bytes: usize,
        max_bytes: usize,
    },
    CollectionTooLarge {
        length: usize,
        max_length: usize,
    },
    IndexOutOfBounds {
        index: usize,
        length: usize,
    },
    InjectedFailure(RuntimeFailurePoint),
    InvalidReference(GcRef),
    /// `LANGUAGE_V3` 4.3: the collection mutation epoch reached `u64::MAX`.
    /// Advancing it further would wrap and could defeat the iteration trap
    /// via ABA, so the write traps deterministically before it happens.
    MutationEpochExhausted,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionReservation {
    range: CollectionRange,
    written: usize,
    claimed: bool,
}

#[derive(Debug)]
pub(crate) struct CollectionQuotaReservation {
    range: CollectionRange,
    written: usize,
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

/// Incremental cycle phase (WP75):
/// `Idle -> RootSnapshot -> Mark -> Sweep -> Complete`.
///
/// `Complete` is a latched, inactive phase. A subsequent explicit
/// incremental collection request starts the next cycle; ordinary trigger
/// polling can therefore observe the completed cycle without treating it as
/// active GC work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GcPhase {
    #[default]
    Idle,
    RootSnapshot,
    Mark,
    Sweep,
    Complete,
}

impl GcPhase {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::RootSnapshot | Self::Mark | Self::Sweep)
    }
}

/// Per-step work budget for one incremental collection step, in the
/// `GC_V1` shape: object-shaped work units, payload bytes processed, and a
/// wall-clock ceiling. Each step performs at least one work unit so a
/// degenerate budget can never stall the cycle; the overshoot is the
/// "budget overrun" the spec allows the runtime to report.
///
/// `max_duration` is deliberately outside the deterministic fuel domain:
/// GC pauses are host-side scheduling, never program-observable cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcBudget {
    /// Slot-shaped work units: gray-queue pops during Mark plus slots
    /// visited during Sweep. Zero returns an empty report unchanged.
    pub max_objects: usize,
    /// Payload bytes processed: traced payload during Mark, reclaimed
    /// payload during Sweep. Inline-only objects cost zero bytes, so this
    /// axis binds only when byte-carrying objects flow through the step.
    pub max_bytes: u64,
    /// Wall-clock ceiling for the step; `Duration::MAX` disables the
    /// clock entirely (no time syscalls on the deterministic test path).
    pub max_duration: std::time::Duration,
}

impl GcBudget {
    /// Object-count-only budget: bytes and duration unlimited.
    #[must_use]
    pub const fn objects(max_objects: usize) -> Self {
        Self {
            max_objects,
            max_bytes: u64::MAX,
            max_duration: std::time::Duration::MAX,
        }
    }
}

/// Whole-cycle telemetry required by `GC_V1`. The value is a snapshot: it
/// grows monotonically while a cycle is active and remains available in the
/// latched [`GcPhase::Complete`] report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcCycleTelemetry {
    pub cycle: u64,
    pub phase: GcPhase,
    pub roots: usize,
    pub objects_marked: usize,
    pub bytes_marked: u64,
    pub objects_swept: usize,
    pub bytes_reclaimed: u64,
    pub live_bytes: u64,
    /// Longest individual incremental pause in this cycle.
    pub pause_time: std::time::Duration,
    /// Sum of all incremental pause durations in this cycle.
    pub incremental_work_time: std::time::Duration,
    /// Reference-publication barriers executed while Mark was active.
    pub barrier_count: u64,
    /// White references actually shaded by those barriers.
    pub remembered_writes: u64,
    pub fragmentation_per_mille: u16,
}

/// Live tracker for one incremental step (G5). The object axis is a strict
/// pre-check (G1 semantics); bytes and deadline are charged after each
/// completed unit with a first-unit guarantee, so a degenerate budget
/// overruns by at most one unit instead of stalling the cycle.
struct StepBudget {
    limit: GcBudget,
    objects: usize,
    bytes: u64,
    started: std::time::Instant,
    deadline: Option<std::time::Instant>,
    spent: bool,
    work_objects: usize,
    work_bytes: u64,
}

impl StepBudget {
    fn new(budget: GcBudget) -> Self {
        let started = std::time::Instant::now();
        Self {
            limit: budget,
            objects: budget.max_objects,
            bytes: budget.max_bytes,
            started,
            // `Duration::MAX` disables the deadline; `checked_add` also
            // makes unusually large host-provided durations fail open
            // instead of panicking in the scheduler.
            deadline: (budget.max_duration != std::time::Duration::MAX)
                .then(|| started.checked_add(budget.max_duration))
                .flatten(),
            spent: false,
            work_objects: 0,
            work_bytes: 0,
        }
    }

    /// Whether another work unit may start.
    fn available(&self) -> bool {
        if self.objects == 0 {
            return false;
        }
        if !self.spent {
            return true;
        }
        if self.bytes == 0 {
            return false;
        }
        self.deadline
            .is_none_or(|deadline| std::time::Instant::now() < deadline)
    }

    /// Charges one completed work unit and its payload bytes.
    fn charge(&mut self, payload_bytes: u64) {
        self.objects = self.objects.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(payload_bytes);
        self.spent = true;
        self.work_objects = self.work_objects.saturating_add(1);
        self.work_bytes = self.work_bytes.saturating_add(payload_bytes);
    }

    fn finish(self, report: &mut IncrementalGcReport) {
        let elapsed = self.started.elapsed();
        report.work_objects = self.work_objects;
        report.work_bytes = self.work_bytes;
        report.pause_time = elapsed;
        report.object_budget_overrun = self.work_objects.saturating_sub(self.limit.max_objects);
        report.byte_budget_overrun = self.work_bytes.saturating_sub(self.limit.max_bytes);
        report.duration_budget_overrun = if self.limit.max_duration == std::time::Duration::MAX {
            std::time::Duration::ZERO
        } else {
            elapsed.saturating_sub(self.limit.max_duration)
        };
    }
}

/// Telemetry for one incremental step: work actually performed, the phase
/// after the step, and the whole-cycle stats when the cycle completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncrementalGcReport {
    pub cycle: u64,
    pub phase: GcPhase,
    pub roots_scanned: usize,
    pub roots_seeded: usize,
    pub objects_marked: usize,
    pub bytes_marked: u64,
    pub slots_swept: usize,
    pub barrier_writes: u64,
    pub barrier_shades: u64,
    pub work_objects: usize,
    pub work_bytes: u64,
    pub object_budget_overrun: usize,
    pub byte_budget_overrun: u64,
    pub duration_budget_overrun: std::time::Duration,
    pub pause_time: std::time::Duration,
    /// G4: payload bytes released by this step's sweep slice (String
    /// storage, i32 backing, map slot vectors, collection-arena extents).
    pub bytes_reclaimed: u64,
    pub live_bytes: u64,
    /// Fragmentation of free collection capacity in permille: zero means
    /// all free cells are contiguous, 1000 means no useful contiguous run.
    pub fragmentation_per_mille: u16,
    pub telemetry: GcCycleTelemetry,
    pub completed: Option<CollectionStats>,
}

/// Cumulative VM allocation and copy counters (M5 WP13).
///
/// Counters are monotonic work totals, not live-state gauges: checkpoint
/// restores (REPL transaction rollback) intentionally do not rewind them,
/// because the allocation and copy work still happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmAllocationCounters {
    pub object_allocations: u64,
    pub string_allocations: u64,
    pub class_allocations: u64,
    pub collection_storage_allocations: u64,
    pub map_slot_allocations: u64,
    pub struct_materializations: u64,
    pub enum_materializations: u64,
    pub allocated_bytes: u64,
    pub collection_relocation_bytes: u64,
    pub string_copy_bytes: u64,
    pub host_codec_copy_bytes: u64,
}

impl VmAllocationCounters {
    /// Saturating element-wise accumulation for report aggregation.
    pub fn accumulate(&mut self, other: Self) {
        self.object_allocations = self
            .object_allocations
            .saturating_add(other.object_allocations);
        self.string_allocations = self
            .string_allocations
            .saturating_add(other.string_allocations);
        self.class_allocations = self
            .class_allocations
            .saturating_add(other.class_allocations);
        self.collection_storage_allocations = self
            .collection_storage_allocations
            .saturating_add(other.collection_storage_allocations);
        self.map_slot_allocations = self
            .map_slot_allocations
            .saturating_add(other.map_slot_allocations);
        self.struct_materializations = self
            .struct_materializations
            .saturating_add(other.struct_materializations);
        self.enum_materializations = self
            .enum_materializations
            .saturating_add(other.enum_materializations);
        self.allocated_bytes = self.allocated_bytes.saturating_add(other.allocated_bytes);
        self.collection_relocation_bytes = self
            .collection_relocation_bytes
            .saturating_add(other.collection_relocation_bytes);
        self.string_copy_bytes = self
            .string_copy_bytes
            .saturating_add(other.string_copy_bytes);
        self.host_codec_copy_bytes = self
            .host_codec_copy_bytes
            .saturating_add(other.host_codec_copy_bytes);
    }

    /// Work performed since an `earlier` snapshot of the same counters.
    #[must_use]
    pub const fn delta_since(self, earlier: Self) -> Self {
        Self {
            object_allocations: self
                .object_allocations
                .saturating_sub(earlier.object_allocations),
            string_allocations: self
                .string_allocations
                .saturating_sub(earlier.string_allocations),
            class_allocations: self
                .class_allocations
                .saturating_sub(earlier.class_allocations),
            collection_storage_allocations: self
                .collection_storage_allocations
                .saturating_sub(earlier.collection_storage_allocations),
            map_slot_allocations: self
                .map_slot_allocations
                .saturating_sub(earlier.map_slot_allocations),
            struct_materializations: self
                .struct_materializations
                .saturating_sub(earlier.struct_materializations),
            enum_materializations: self
                .enum_materializations
                .saturating_sub(earlier.enum_materializations),
            allocated_bytes: self.allocated_bytes.saturating_sub(earlier.allocated_bytes),
            collection_relocation_bytes: self
                .collection_relocation_bytes
                .saturating_sub(earlier.collection_relocation_bytes),
            string_copy_bytes: self
                .string_copy_bytes
                .saturating_sub(earlier.string_copy_bytes),
            host_codec_copy_bytes: self
                .host_codec_copy_bytes
                .saturating_sub(earlier.host_codec_copy_bytes),
        }
    }
}

/// Safe-Rust stop-the-world mark/sweep heap with generation-protected references.
#[derive(Debug)]
pub struct Heap {
    slots: Vec<ObjectSlot>,
    free: Vec<u32>,
    /// WP72 typed payload arena: capacity is reserved with the heap, so map
    /// creation/recycling never grows this index vector on the hot path.
    maps: Vec<Option<VmMap>>,
    free_maps: Vec<u32>,
    /// `LANGUAGE_V3`: dedicated key-only set header arena, mirroring the map
    /// arena indirection (the object slot carries only the arena index).
    sets: Vec<Option<VmSet>>,
    free_sets: Vec<u32>,
    map_slots: MapSlotArena,
    max_objects: u32,
    max_string_bytes: usize,
    max_collection_length: usize,
    max_collection_elements: usize,
    collection_elements_used: usize,
    collections: CollectionArena,
    /// K29/WP47: eight compact typed regions share one aligned allocator
    /// backing while retaining their native contiguous element widths.
    scalar_collections: ScalarArenaSet,
    host_staging: Vec<GcRef>,
    host_transaction_active: bool,
    failure_injector: RuntimeFailureInjector,
    counters: VmAllocationCounters,
    /// WP56 literal memoization: content-keyed cache of previously loaded
    /// string constants, each carrying its content hash so hot literal
    /// loads are O(1) instead of rehashing per load (WP69). Entries are
    /// NOT roots; a hit revalidates the generation-protected reference,
    /// and a collected slot simply falls back to a fresh allocation. The
    /// two paths that recycle a slot *without* bumping its generation -
    /// host-transaction rollback and checkpoint restore - clear the cache
    /// instead, so a generation match always implies content identity.
    string_literal_cache: BTreeMap<String, CachedStringLiteral>,
    /// Allocation-free runtime index for module-owned literal pools. The
    /// vector is reserved to the heap object limit and stale generation
    /// entries are replaced in place across GC/reload.
    pooled_string_cache: Vec<CachedPooledString>,
    /// WP74: reusable mark-phase work queue. Capacity converges to the
    /// high-water mark of prior collections instead of reallocating on
    /// every `collect` call. Pure scratch space, never heap state. During
    /// an incremental cycle (G1) it holds the persistent gray set.
    mark_scratch: VecDeque<GcRef>,
    /// WP75/WP81 incremental cycle state. `gc_cycle` is monotonic for the
    /// lifetime of the heap; the remaining counters describe the currently
    /// active or latched-complete cycle.
    gc_phase: GcPhase,
    gc_cycle: u64,
    gc_sweep_cursor: usize,
    gc_roots_scanned: usize,
    gc_marked: usize,
    gc_bytes_marked: u64,
    gc_slots_swept: usize,
    gc_reclaimed: usize,
    gc_barrier_writes: u64,
    gc_barrier_shades: u64,
    gc_reported_barrier_writes: u64,
    gc_reported_barrier_shades: u64,
    gc_incremental_work_time: std::time::Duration,
    gc_max_pause_time: std::time::Duration,
    /// G4: payload bytes released by the current cycle's sweep slices;
    /// latched into `last_cycle_bytes_reclaimed` when the cycle completes.
    gc_bytes_reclaimed: u64,
    last_cycle_bytes_reclaimed: u64,
    /// G6 live gauge: out-of-slot payload bytes owned by live objects,
    /// maintained incrementally at every footprint transition (commit,
    /// sweep, host rollback, array regrow, map rehash). Full collection
    /// re-derives it in debug builds to pin the gauge against drift.
    live_payload_bytes: u64,
    /// WP71 live bytes held in the collection/map payload arenas. Class and
    /// Struct field extents share that allocator and therefore participate
    /// in the same resource ceiling.
    live_collection_bytes: u64,
    /// Exact O(1) object population. Empty slots whose generation is
    /// exhausted cannot be inferred from `slots.len() - free.len()`, so every
    /// object commit and release updates this gauge directly.
    live_objects: usize,
    /// WP71 admission ceiling over total live VM bytes: occupied object/map
    /// headers plus out-of-slot payload.
    max_heap_bytes: u64,
    /// WP71 admission ceiling over collection/map arena bytes.
    max_collection_bytes: u64,
}

/// Exact heap state owned by one staged transactional Cell.
///
/// Runtime limits and the failure-control plane remain Realm authority and are
/// intentionally not snapshotted. Every mutable VM storage surface is: object
/// slots/generations, free lists, collection storage, and Host return staging.
#[derive(Debug)]
pub(crate) struct HeapCheckpoint {
    slots: Vec<ObjectSlot>,
    free: Vec<u32>,
    maps: Vec<Option<VmMap>>,
    free_maps: Vec<u32>,
    sets: Vec<Option<VmSet>>,
    free_sets: Vec<u32>,
    map_slots: MapSlotArena,
    collections: CollectionArena,
    scalar_collections: ScalarArenaSet,
    host_staging: Vec<GcRef>,
    host_transaction_active: bool,
    collection_elements_used: usize,
    live_objects: usize,
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
        let arena_elements = max_collection_length
            .saturating_mul((max_objects as usize).min(64))
            .max(max_collection_length);
        Self::new_with_arena_limits(
            max_objects,
            max_string_bytes,
            max_collection_length,
            arena_elements,
            max_objects as usize + 1,
        )
    }

    #[must_use]
    pub(crate) fn checkpoint(&self) -> HeapCheckpoint {
        HeapCheckpoint {
            slots: self.slots.clone(),
            free: self.free.clone(),
            maps: self.maps.clone(),
            free_maps: self.free_maps.clone(),
            sets: self.sets.clone(),
            free_sets: self.free_sets.clone(),
            map_slots: self.map_slots.clone(),
            collections: self.collections.checkpoint_clone(),
            scalar_collections: self.scalar_collections.checkpoint_clone(),
            host_staging: self.host_staging.clone(),
            host_transaction_active: self.host_transaction_active,
            collection_elements_used: self.collection_elements_used,
            live_objects: self.live_objects,
        }
    }

    pub(crate) fn restore_checkpoint(&mut self, checkpoint: HeapCheckpoint) {
        // The snapshot predates any in-flight incremental cycle state; the
        // gray queue and sweep cursor would reference rolled-back slots.
        self.reset_incremental_cycle();
        // The restored slots may pair a cached literal's generation with
        // different content; the cache trades that ambiguity for a rebuild.
        self.string_literal_cache.clear();
        self.pooled_string_cache.clear();
        restore_reserved_vec(&mut self.slots, checkpoint.slots);
        restore_reserved_vec(&mut self.free, checkpoint.free);
        restore_reserved_vec(&mut self.maps, checkpoint.maps);
        restore_reserved_vec(&mut self.free_maps, checkpoint.free_maps);
        restore_reserved_vec(&mut self.sets, checkpoint.sets);
        restore_reserved_vec(&mut self.free_sets, checkpoint.free_sets);
        self.map_slots.restore_checkpoint(checkpoint.map_slots);
        self.collections.restore_checkpoint(checkpoint.collections);
        self.scalar_collections
            .restore_checkpoint(&checkpoint.scalar_collections);
        restore_reserved_vec(&mut self.host_staging, checkpoint.host_staging);
        self.host_transaction_active = checkpoint.host_transaction_active;
        self.collection_elements_used = checkpoint.collection_elements_used;
        self.live_objects = checkpoint.live_objects;
        debug_assert_eq!(
            self.live_objects,
            self.recompute_live_objects(),
            "checkpoint live object gauge drifted from restored slots"
        );
        // G6: the restored object population owns a different footprint;
        // re-derive the gauge from the ground truth walk.
        self.live_payload_bytes = self.recompute_live_payload_bytes();
        self.live_collection_bytes = self.recompute_live_collection_bytes();
    }

    #[must_use]
    pub fn new_with_arena_limits(
        max_objects: u32,
        max_string_bytes: usize,
        max_collection_length: usize,
        max_collection_elements: usize,
        max_collection_ranges: usize,
    ) -> Self {
        Self {
            slots: Vec::with_capacity(max_objects as usize),
            free: Vec::with_capacity(max_objects as usize),
            maps: Vec::with_capacity(max_objects as usize),
            free_maps: Vec::with_capacity(max_objects as usize),
            sets: Vec::with_capacity(max_objects as usize),
            free_sets: Vec::with_capacity(max_objects as usize),
            map_slots: MapSlotArena::new(
                max_collection_elements,
                max_collection_ranges
                    .max((max_objects as usize).saturating_mul(3).saturating_add(1)),
            ),
            max_objects,
            max_string_bytes,
            max_collection_length: max_collection_length.min(i32::MAX as usize),
            max_collection_elements,
            collection_elements_used: 0,
            collections: CollectionArena::new(
                max_collection_elements,
                max_collection_ranges.max(max_objects as usize + 1),
            ),
            scalar_collections: ScalarArenaSet::new(max_collection_elements),
            host_staging: Vec::with_capacity(max_objects as usize),
            host_transaction_active: false,
            failure_injector: RuntimeFailureInjector::default(),
            counters: VmAllocationCounters::default(),
            string_literal_cache: BTreeMap::new(),
            pooled_string_cache: Vec::with_capacity(max_objects as usize),
            mark_scratch: VecDeque::with_capacity(max_objects as usize),
            gc_phase: GcPhase::Idle,
            gc_cycle: 0,
            gc_sweep_cursor: 0,
            gc_roots_scanned: 0,
            gc_marked: 0,
            gc_bytes_marked: 0,
            gc_slots_swept: 0,
            gc_reclaimed: 0,
            gc_barrier_writes: 0,
            gc_barrier_shades: 0,
            gc_reported_barrier_writes: 0,
            gc_reported_barrier_shades: 0,
            gc_incremental_work_time: std::time::Duration::ZERO,
            gc_max_pause_time: std::time::Duration::ZERO,
            gc_bytes_reclaimed: 0,
            last_cycle_bytes_reclaimed: 0,
            live_payload_bytes: 0,
            live_collection_bytes: 0,
            live_objects: 0,
            max_heap_bytes: u64::MAX,
            max_collection_bytes: u64::MAX,
        }
    }

    /// Cumulative allocation/copy work performed by this heap (WP13).
    #[must_use]
    pub const fn vm_allocation_counters(&self) -> VmAllocationCounters {
        self.counters
    }

    pub fn allocate_string(&mut self, value: &str) -> Result<GcRef, HeapError> {
        self.validate_string_length(value.len())?;
        self.ensure_new_object_headroom(value.len() as u64, false)?;
        let mut reservation = self.preflight(1)?;
        let value = value.to_owned();
        Ok(self.commit(&mut reservation, Object::String(value)))
    }

    /// WP56 literal load: returns the cached live copy of a string constant
    /// when its slot generation still matches, otherwise allocates and
    /// re-caches. Hot literal loads therefore create no new String objects.
    pub fn load_string_literal(&mut self, value: &str) -> Result<GcRef, HeapError> {
        self.load_string_literal_with_hash(value)
            .map(|(reference, _)| reference)
    }

    /// WP56/WP69 hot path: the cached reference *and* its content hash in
    /// one O(1) lookup - no per-load content rehash, no content compare.
    /// A generation match implies content identity because every path
    /// that recycles a slot without bumping its generation (host
    /// rollback, checkpoint restore) clears this cache instead.
    pub fn load_string_literal_with_hash(
        &mut self,
        value: &str,
    ) -> Result<(GcRef, u64), HeapError> {
        if let Some(cached) = self.string_literal_cache.get(value).copied() {
            let slot = self
                .slots
                .get(cached.reference.index as usize)
                .filter(|slot| slot.generation == cached.reference.generation);
            if let Some(slot) = slot
                && matches!(slot.object.as_ref(), Some(Object::String(_)))
            {
                debug_assert!(
                    matches!(
                        slot.object.as_ref(),
                        Some(Object::String(cached_value)) if cached_value == value
                    ),
                    "a generation-valid literal cache entry must keep its content"
                );
                return Ok((cached.reference, cached.hash));
            }
        }
        let reference = self.allocate_string(value)?;
        let hash = fnv_content_hash(value);
        self.string_literal_cache
            .insert(value.to_owned(), CachedStringLiteral { reference, hash });
        Ok((reference, hash))
    }

    pub(crate) fn load_pooled_string(
        &mut self,
        pool: u64,
        literal: u32,
        value: Arc<str>,
        hash: u64,
    ) -> Result<(GcRef, u64), HeapError> {
        let mut reusable = None;
        for (index, cached) in self.pooled_string_cache.iter().copied().enumerate() {
            let live = self
                .slots
                .get(cached.reference.index as usize)
                .filter(|slot| slot.generation == cached.reference.generation)
                .and_then(|slot| slot.object.as_ref());
            if cached.pool == pool && cached.literal == literal {
                if let Some(Object::SharedString(current)) = live {
                    debug_assert_eq!(&**current, &*value);
                    return Ok((cached.reference, cached.hash));
                }
                reusable = Some(index);
                break;
            }
            if live.is_none() && reusable.is_none() {
                reusable = Some(index);
            }
        }
        self.validate_string_length(value.len())?;
        self.ensure_new_object_headroom(0, false)?;
        let mut reservation = self.preflight(1)?;
        let reference = self.commit(&mut reservation, Object::SharedString(value));
        let cached = CachedPooledString {
            pool,
            literal,
            reference,
            hash,
        };
        if let Some(index) = reusable {
            self.pooled_string_cache[index] = cached;
        } else {
            debug_assert!(
                self.pooled_string_cache.len() < self.pooled_string_cache.capacity(),
                "every pooled literal consumes one bounded heap object"
            );
            self.pooled_string_cache.push(cached);
        }
        Ok((reference, hash))
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
        self.ensure_new_object_headroom(length as u64, false)?;
        let mut reservation = self.preflight(1)?;
        let mut value = String::with_capacity(length);
        value.push_str(self.string(lhs)?);
        value.push_str(self.string(rhs)?);
        Ok(self.commit(&mut reservation, Object::String(value)))
    }

    pub(crate) fn copy_string_range(
        &mut self,
        source: GcRef,
        start: usize,
        end: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let length = {
            let value = self.string(source)?;
            value
                .get(start..end)
                .ok_or(HeapError::IndexOutOfBounds {
                    index: end,
                    length: value.len(),
                })?
                .len()
        };
        self.validate_string_length(length)?;
        let mut reservation = self.preflight(1)?;
        // The source object cannot move while this instruction executes. Make
        // the only owned copy after every fallible VM capacity check.
        let value = self
            .string(source)?
            .get(start..end)
            .expect("validated string range remains valid")
            .to_owned();
        self.commit_owned_string(&mut reservation, value)
    }

    pub(crate) fn trim_string(&mut self, source: GcRef) -> Result<RuntimeValue, HeapError> {
        let (start, end) = {
            let value = self.string(source)?;
            let start = value.len() - value.trim_start().len();
            let end = value.trim_end().len();
            (start, end.max(start))
        };
        self.copy_string_range(source, start, end)
    }

    pub fn string(&self, reference: GcRef) -> Result<&str, HeapError> {
        match self.resolve(reference)? {
            Object::String(value) => Ok(value),
            Object::SharedString(value) => Ok(value),
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
        Ok(fnv_content_hash(self.string(reference)?))
    }

    pub(crate) fn split_string(
        &mut self,
        value: GcRef,
        delimiter: GcRef,
    ) -> Result<RuntimeValue, HeapError> {
        let part_count = {
            let value = self.string(value)?;
            let delimiter = self.string(delimiter)?;
            let mut count = 0_usize;
            for part in value.split(delimiter) {
                if count == self.max_collection_length {
                    return Err(HeapError::CollectionTooLarge {
                        length: self.max_collection_length.saturating_add(1),
                        max_length: self.max_collection_length,
                    });
                }
                self.validate_string_length(part.len())?;
                count += 1;
            }
            count
        };
        self.validate_collection_length(part_count)?;

        let object_count = part_count
            .checked_add(1)
            .ok_or(HeapError::CapacityExhausted)?;
        let storage = CollectionStorage::String;
        let mut parts = Vec::new();
        if parts.try_reserve_exact(part_count).is_err() {
            return Err(HeapError::CapacityExhausted);
        }
        {
            let value = self.string(value)?;
            let delimiter = self.string(delimiter)?;
            parts.extend(value.split(delimiter).map(str::to_owned));
        }
        debug_assert_eq!(parts.len(), part_count);

        // Admit the whole compound result before publishing its first VM
        // object. Per-object checks alone would allow early part strings to
        // commit before the final Array extent discovered a byte-ceiling
        // failure.
        let header_bytes = u64::try_from(object_count)
            .unwrap_or(u64::MAX)
            .saturating_mul(Self::new_object_header_bytes(false));
        let string_bytes = parts
            .iter()
            .map(|part| u64::try_from(part.capacity()).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add);
        let collection_bytes = u64::try_from(part_count)
            .unwrap_or(u64::MAX)
            .saturating_mul(storage.cell_size() as u64);
        self.ensure_payload_headroom(
            header_bytes
                .saturating_add(string_bytes)
                .saturating_add(collection_bytes),
        )?;
        self.ensure_collection_headroom(collection_bytes)?;
        let mut objects = self.preflight(object_count)?;
        let range = self.claim_typed_collection(storage, part_count)?;
        for (index, part) in parts.into_iter().enumerate() {
            let value = match self.commit_owned_string(&mut objects, part) {
                Ok(value) => value,
                Err(error) => {
                    self.release_typed_collection(storage, range);
                    return Err(error);
                }
            };
            if let Err(error) = self.typed_collection_set(
                storage,
                nexa_bytecode::ValueType::String,
                range,
                index,
                value,
            ) {
                self.release_typed_collection(storage, range);
                return Err(error);
            }
        }
        self.commit_array_reserved(
            &mut objects,
            nexa_bytecode::array_type(nexa_bytecode::ValueType::String),
            nexa_bytecode::ValueType::String,
            storage,
            range,
        )
    }

    pub(crate) fn split_fuel_shape(
        &self,
        value: GcRef,
        delimiter: GcRef,
    ) -> Result<StringSplitFuelShape, HeapError> {
        let value = self.string(value)?;
        let delimiter = self.string(delimiter)?;
        // Use only O(1) metadata before fuel settlement. Counting actual
        // matches here would let an underfunded task repeatedly scan an
        // arbitrarily large string for free.
        let upper_bound = if delimiter.is_empty() {
            value.len().checked_add(2)
        } else {
            value
                .len()
                .checked_div(delimiter.len())
                .and_then(|parts| parts.checked_add(1))
        }
        .ok_or(HeapError::CollectionTooLarge {
            length: usize::MAX,
            max_length: self.max_collection_length,
        })?;
        let charged_parts = upper_bound.min(self.max_collection_length.saturating_add(1));
        Ok(StringSplitFuelShape {
            source_bytes: value.len(),
            delimiter_bytes: delimiter.len(),
            parts: charged_parts,
        })
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

    pub(crate) fn preflight_host_string_bytes(&self, bytes: usize) -> Result<(), HeapError> {
        if self.failure_trigger(RuntimeFailurePoint::HostReturnStringReservation) {
            return Err(HeapError::InjectedFailure(
                RuntimeFailurePoint::HostReturnStringReservation,
            ));
        }
        self.validate_string_length(bytes)
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
        // Typed payload handles are created only by their dedicated
        // allocation funnels; accepting a caller-forged map index would
        // break the slot/arena ownership invariant.
        if matches!(object, Object::Map { .. } | Object::Set { .. }) {
            return Err(invalid_value_reference());
        }
        let payload_bytes = self.object_payload_bytes(&object);
        self.ensure_new_object_headroom(payload_bytes, false)?;
        self.ensure_collection_headroom(self.object_collection_bytes(&object))?;
        let mut reservation = self.preflight(1)?;
        Ok(self.commit(&mut reservation, object))
    }

    fn object_payload_bytes(&self, object: &Object) -> u64 {
        match object {
            Object::Map { storage } => self
                .maps
                .get(*storage as usize)
                .and_then(Option::as_ref)
                .map_or(0, |map| {
                    u64::try_from(map.storage_bytes()).unwrap_or(u64::MAX)
                }),
            Object::Set { storage } => self
                .sets
                .get(*storage as usize)
                .and_then(Option::as_ref)
                .map_or(0, |set| {
                    u64::try_from(set.storage_bytes()).unwrap_or(u64::MAX)
                }),
            _ => object.payload_bytes(),
        }
    }

    fn object_collection_bytes(&self, object: &Object) -> u64 {
        match object {
            Object::Array { .. }
            | Object::Buffer { .. }
            | Object::Map { .. }
            | Object::Set { .. }
            | Object::Struct { .. }
            | Object::Class { .. } => self.object_payload_bytes(object),
            Object::String(_) | Object::SharedString(_) | Object::Enum { .. } => 0,
        }
    }

    /// Releases the exclusively owned payload behind a condemned slot and
    /// returns the exact G4/G6 byte count that left the live heap.
    fn release_object_storage(&mut self, object: &Object) -> u64 {
        let payload = self.object_payload_bytes(object);
        match object {
            Object::Array { range, storage, .. }
            | Object::Buffer { range, storage, .. }
            | Object::Struct { storage, range, .. }
            | Object::Class { storage, range, .. } => {
                self.release_typed_collection(*storage, *range);
            }
            Object::Map { storage } => {
                let storage = *storage as usize;
                if let Some(map) = self.maps.get_mut(storage).and_then(Option::take) {
                    self.release_map_value_extents(&map);
                    self.map_slots.release(map.slots);
                    if let Some(rehash) = map.rehash {
                        self.map_slots.release(rehash.old_slots);
                        self.map_slots.release(rehash.new_slots);
                    }
                    self.free_maps
                        .push(u32::try_from(storage).expect("map arena index originates as u32"));
                }
            }
            Object::Set { storage } => {
                let storage = *storage as usize;
                if let Some(set) = self.sets.get_mut(storage).and_then(Option::take) {
                    self.map_slots.release(set.slots);
                    if let Some(rehash) = set.rehash {
                        self.map_slots.release(rehash.old_slots);
                        self.map_slots.release(rehash.new_slots);
                    }
                    self.free_sets
                        .push(u32::try_from(storage).expect("set arena index originates as u32"));
                }
            }
            Object::String(_) | Object::SharedString(_) | Object::Enum { .. } => {}
        }
        payload
    }

    /// Releases every persistent physical value range owned by a map.
    ///
    /// Entries can reside in the current table or either side of an
    /// incremental rehash, but never in more than one of them. The returned
    /// byte count is used by mutation paths; object sweep/rollback already
    /// release the map's complete payload as one aggregate.
    fn release_map_value_extents(&mut self, map: &VmMap) -> u64 {
        let mut released = 0_u64;
        let ranges = [
            map.values,
            map.rehash
                .as_ref()
                .map_or(CollectionRange::default(), |rehash| rehash.old_values),
            map.rehash
                .as_ref()
                .map_or(CollectionRange::default(), |rehash| rehash.new_values),
        ];
        for values in ranges {
            if values.length != 0 {
                released = released.saturating_add(
                    u64::try_from(values.length)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(map.value_storage.cell_size() as u64),
                );
                self.release_typed_collection(map.value_storage, values);
            }
        }
        released
    }

    pub(crate) fn preflight(&mut self, count: usize) -> Result<HeapReservation, HeapError> {
        if self.failure_injector.trigger(RuntimeFailurePoint::HeapSlot) {
            return Err(HeapError::InjectedFailure(RuntimeFailurePoint::HeapSlot));
        }
        // Every reserved slot will become one live object header. Pin the
        // minimum byte admission here so mutation paths (notably MapRemove)
        // cannot succeed and then discover that their result Enum has no
        // byte headroom. Payload/map-header admission remains at the typed
        // constructor where its exact footprint is known.
        self.ensure_payload_headroom(
            u64::try_from(count)
                .unwrap_or(u64::MAX)
                .saturating_mul(Self::new_object_header_bytes(false)),
        )?;
        let unused = usize::try_from(self.max_objects)
            .unwrap_or(usize::MAX)
            .saturating_sub(self.slots.len());
        if self.free.len().saturating_add(unused) < count {
            return Err(HeapError::CapacityExhausted);
        }
        Ok(HeapReservation { remaining: count })
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn commit(&mut self, reservation: &mut HeapReservation, object: Object) -> GcRef {
        reservation.remaining = reservation
            .remaining
            .checked_sub(1)
            .expect("heap allocation was preflighted");
        self.live_objects = self
            .live_objects
            .checked_add(1)
            .expect("live objects cannot exceed the u32 heap limit");
        self.counters.object_allocations = self.counters.object_allocations.saturating_add(1);
        let header_bytes = size_of::<ObjectSlot>() as u64
            + match &object {
                Object::Map { .. } => size_of::<Option<VmMap>>() as u64,
                Object::Set { .. } => size_of::<Option<VmSet>>() as u64,
                _ => 0,
            };
        self.counters.allocated_bytes = self.counters.allocated_bytes.saturating_add(header_bytes);
        // WP71: the commit funnel charges both the total live payload and
        // the collection-arena subset. Every caller preflights the complete
        // object footprint before this infallible publication.
        let payload_bytes = self.object_payload_bytes(&object);
        let collection_bytes = self.object_collection_bytes(&object);
        self.charge_live_payload(payload_bytes);
        self.live_collection_bytes = self.live_collection_bytes.saturating_add(collection_bytes);
        debug_assert!(self.live_vm_bytes() <= self.max_heap_bytes);
        debug_assert!(self.live_collection_bytes <= self.max_collection_bytes);
        match &object {
            Object::String(value) => {
                self.counters.string_allocations =
                    self.counters.string_allocations.saturating_add(1);
                self.counters.string_copy_bytes = self
                    .counters
                    .string_copy_bytes
                    .saturating_add(value.len() as u64);
            }
            Object::SharedString(_) => {}
            Object::Class { .. } => {
                self.counters.class_allocations = self.counters.class_allocations.saturating_add(1);
            }
            Object::Array { .. } | Object::Buffer { .. } => {
                self.counters.collection_storage_allocations = self
                    .counters
                    .collection_storage_allocations
                    .saturating_add(1);
            }
            Object::Map { .. } | Object::Set { .. } => {
                self.counters.collection_storage_allocations = self
                    .counters
                    .collection_storage_allocations
                    .saturating_add(1);
                self.counters.map_slot_allocations =
                    self.counters.map_slot_allocations.saturating_add(1);
            }
            Object::Struct { .. } => {
                self.counters.struct_materializations =
                    self.counters.struct_materializations.saturating_add(1);
            }
            Object::Enum { .. } => {
                self.counters.enum_materializations =
                    self.counters.enum_materializations.saturating_add(1);
            }
        }
        // G1: objects allocated while a cycle is active are born marked so
        // an in-flight sweep never reclaims them; during Mark their inline
        // children are shaded because the newborn is already black. During
        // Sweep every nameable child is necessarily marked, so no shading
        // is needed. Array/Buffer extents shade through
        // `commit_collection_value`.
        let born_marked = self.gc_phase.is_active();
        if self.gc_phase == GcPhase::Mark {
            object.trace_references(&mut |child| {
                self.gc_barrier_writes = self.gc_barrier_writes.saturating_add(1);
                if Self::enqueue_gray(&mut self.slots, &mut self.mark_scratch, child) {
                    self.gc_marked += 1;
                    self.gc_barrier_shades = self.gc_barrier_shades.saturating_add(1);
                }
            });
        }
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            debug_assert!(slot.object.is_none());
            slot.object = Some(object);
            slot.marked = born_marked;
            let reference = GcRef {
                index,
                generation: slot.generation,
            };
            if self.host_transaction_active {
                self.host_staging.push(reference);
            }
            return reference;
        }
        let index = u32::try_from(self.slots.len()).expect("heap capacity was preflighted");
        debug_assert!(index < self.max_objects);
        self.slots.push(ObjectSlot {
            generation: 0,
            marked: born_marked,
            object: Some(object),
        });
        let reference = GcRef {
            index,
            generation: 0,
        };
        if self.host_transaction_active {
            self.host_staging.push(reference);
        }
        reference
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
        self.ensure_new_object_headroom(
            u64::try_from(value.capacity()).unwrap_or(u64::MAX),
            false,
        )?;
        let hash = fnv_content_hash(&value);
        let reference = self.commit(reservation, Object::String(value));
        Ok(RuntimeValue::String { reference, hash })
    }

    /// Validates every bounded VM resource needed by one owned string before
    /// its backing allocation is attempted.
    pub(crate) fn preflight_string_build(
        &mut self,
        bytes: usize,
    ) -> Result<HeapReservation, HeapError> {
        self.validate_string_length(bytes)?;
        self.ensure_new_object_headroom(u64::try_from(bytes).unwrap_or(u64::MAX), false)?;
        self.preflight(1)
    }

    pub fn preflight_collection(
        &mut self,
        element_count: usize,
    ) -> Result<CollectionReservation, HeapError> {
        // G6 admission: extent bytes count toward the heap byte ceiling.
        // For regrow this is conservative - the old extent is still held -
        // which is exactly the safe direction.
        let range = self.claim_global_collection_range(element_count, size_of::<RuntimeValue>())?;
        if let Err(error) = self.collections.initialize(range) {
            self.collections.release(range);
            self.release_collection_quota(range.length);
            return Err(error);
        }
        Ok(CollectionReservation {
            range,
            written: 0,
            claimed: true,
        })
    }

    fn claim_global_collection_range(
        &mut self,
        element_count: usize,
        element_bytes: usize,
    ) -> Result<CollectionRange, HeapError> {
        let bytes = (element_count as u64).saturating_mul(element_bytes as u64);
        self.ensure_payload_headroom(bytes)?;
        self.ensure_collection_headroom(bytes)?;
        self.claim_collection_quota(element_count)?;
        let range = self.collections.find_free(element_count).ok_or_else(|| {
            self.release_collection_quota(element_count);
            HeapError::CapacityExhausted
        })?;
        if let Err(error) = self.collections.claim(range) {
            self.release_collection_quota(element_count);
            return Err(error);
        }
        Ok(range)
    }

    fn claim_collection_quota(&mut self, count: usize) -> Result<(), HeapError> {
        let claimed = self
            .collection_elements_used
            .checked_add(count)
            .ok_or(HeapError::CapacityExhausted)?;
        if claimed > self.max_collection_elements {
            return Err(HeapError::CapacityExhausted);
        }
        self.collection_elements_used += count;
        Ok(())
    }

    fn release_collection_quota(&mut self, count: usize) {
        self.collection_elements_used = self
            .collection_elements_used
            .checked_sub(count)
            .expect("released collection extent was charged");
    }

    pub(crate) fn preflight_collection_quota(
        &mut self,
        count: usize,
    ) -> Result<CollectionQuotaReservation, HeapError> {
        let range = self.claim_global_collection_range(count, size_of::<RuntimeValue>())?;
        Ok(CollectionQuotaReservation {
            range,
            written: 0,
            remaining: count,
        })
    }

    pub(crate) fn claim_reserved_typed_collection(
        &mut self,
        reservation: &mut CollectionQuotaReservation,
        storage: CollectionStorage,
        count: usize,
    ) -> Result<CollectionRange, HeapError> {
        if count > reservation.remaining {
            return Err(HeapError::CapacityExhausted);
        }
        let range = CollectionRange {
            start: reservation.range.start + reservation.written,
            length: count,
        };
        let claimed = self.claim_physical_collection(storage, range);
        match claimed {
            Ok(()) => {
                reservation.written += count;
                reservation.remaining -= count;
                Ok(range)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn release_collection_quota_reservation(
        &mut self,
        reservation: &mut CollectionQuotaReservation,
    ) {
        if reservation.remaining != 0 {
            let tail = CollectionRange {
                start: reservation.range.start + reservation.written,
                length: reservation.remaining,
            };
            self.collections.release(tail);
            self.release_collection_quota(tail.length);
        }
        reservation.remaining = 0;
    }

    pub(crate) const fn collection_quota_complete(
        reservation: &CollectionQuotaReservation,
    ) -> bool {
        reservation.remaining == 0
    }

    fn claim_typed_collection(
        &mut self,
        storage: CollectionStorage,
        element_count: usize,
    ) -> Result<CollectionRange, HeapError> {
        let range = self.claim_global_collection_range(element_count, storage.cell_size())?;
        let claimed = self.claim_physical_collection(storage, range);
        if let Err(error) = claimed {
            self.collections.release(range);
            self.release_collection_quota(element_count);
            return Err(error);
        }
        Ok(range)
    }

    pub(crate) fn claim_physical_collection(
        &mut self,
        storage: CollectionStorage,
        range: CollectionRange,
    ) -> Result<(), HeapError> {
        match storage {
            CollectionStorage::Values => self.collections.initialize(range),
            CollectionStorage::I32 => self.scalar_collections.i32_mut().claim_exact(range),
            CollectionStorage::I64 => self.scalar_collections.i64_mut().claim_exact(range),
            CollectionStorage::F32 => self.scalar_collections.f32_mut().claim_exact(range),
            CollectionStorage::F64 => self.scalar_collections.f64_mut().claim_exact(range),
            CollectionStorage::Bool => self.scalar_collections.bools_mut().claim_exact(range),
            CollectionStorage::Rune => self.scalar_collections.runes_mut().claim_exact(range),
            CollectionStorage::String => self.scalar_collections.strings_mut().claim_exact(range),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                self.scalar_collections.refs_mut().claim_exact(range)
            }
        }
    }

    pub(crate) fn release_typed_collection(
        &mut self,
        storage: CollectionStorage,
        range: CollectionRange,
    ) {
        match storage {
            CollectionStorage::Values => {}
            CollectionStorage::I32 => self.scalar_collections.i32_mut().release(range),
            CollectionStorage::I64 => self.scalar_collections.i64_mut().release(range),
            CollectionStorage::F32 => self.scalar_collections.f32_mut().release(range),
            CollectionStorage::F64 => self.scalar_collections.f64_mut().release(range),
            CollectionStorage::Bool => self.scalar_collections.bools_mut().release(range),
            CollectionStorage::Rune => self.scalar_collections.runes_mut().release(range),
            CollectionStorage::String => self.scalar_collections.strings_mut().release(range),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                self.scalar_collections.refs_mut().release(range);
            }
        }
        self.collections.release(range);
        self.release_collection_quota(range.length);
    }

    fn typed_collection_view(
        &self,
        storage: CollectionStorage,
        element_type: nexa_bytecode::ValueType,
        range: CollectionRange,
    ) -> Result<CollectionView<'_>, HeapError> {
        typed_collection_view_from_arenas(
            &self.collections,
            &self.scalar_collections,
            storage,
            element_type,
            range,
        )
    }

    fn field_view(
        &self,
        storage: CollectionStorage,
        range: CollectionRange,
        field_count: usize,
    ) -> Result<CollectionView<'_>, HeapError> {
        let element_type = field_type_for_storage(storage).unwrap_or(nexa_bytecode::ValueType::Ref);
        self.typed_collection_view(storage, element_type, range)?
            .prefix(field_count)
            .ok_or(HeapError::IndexOutOfBounds {
                index: field_count,
                length: range.length,
            })
    }

    fn typed_collection_get(
        &self,
        storage: CollectionStorage,
        element_type: nexa_bytecode::ValueType,
        range: CollectionRange,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let view = self.typed_collection_view(storage, element_type, range)?;
        view.get(index).ok_or(HeapError::IndexOutOfBounds {
            index,
            length: view.len(),
        })
    }

    fn typed_collection_get_absolute(
        &self,
        storage: CollectionStorage,
        element_type: nexa_bytecode::ValueType,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        macro_rules! scalar {
            ($arena:ident, $constructor:expr) => {{
                let arena = self.scalar_collections.$arena();
                let values = arena.values(CollectionRange {
                    start: 0,
                    length: arena.length(),
                })?;
                values
                    .get(index)
                    .copied()
                    .map($constructor)
                    .ok_or(HeapError::IndexOutOfBounds {
                        index,
                        length: values.len(),
                    })
            }};
        }
        match storage {
            CollectionStorage::I32 => scalar!(i32, RuntimeValue::I32),
            CollectionStorage::I64 => scalar!(i64, RuntimeValue::I64),
            CollectionStorage::F32 => scalar!(f32, RuntimeValue::F32),
            CollectionStorage::F64 => scalar!(f64, RuntimeValue::F64),
            CollectionStorage::Bool => {
                scalar!(bools, |value| RuntimeValue::Bool(value != 0))
            }
            CollectionStorage::Rune => scalar!(runes, RuntimeValue::Rune),
            CollectionStorage::String => scalar!(strings, |(reference, hash)| {
                RuntimeValue::String { reference, hash }
            }),
            CollectionStorage::Ref => scalar!(refs, RuntimeValue::Ref),
            CollectionStorage::NamedRef => {
                let nexa_bytecode::ValueType::Named(type_id) = element_type else {
                    return Err(invalid_value_reference());
                };
                scalar!(refs, |reference| RuntimeValue::NamedRef {
                    reference,
                    type_id,
                })
            }
            CollectionStorage::Values => {
                let values = &self.collections.values;
                values
                    .get(index)
                    .copied()
                    .ok_or(HeapError::IndexOutOfBounds {
                        index,
                        length: values.len(),
                    })
            }
        }
    }

    pub(crate) fn typed_collection_set(
        &mut self,
        storage: CollectionStorage,
        element_type: nexa_bytecode::ValueType,
        range: CollectionRange,
        index: usize,
        value: RuntimeValue,
    ) -> Result<(), HeapError> {
        macro_rules! set_scalar {
            ($arena:ident, $pattern:pat => $stored:expr) => {{
                let $pattern = value else {
                    return Err(invalid_value_reference());
                };
                let mut arena = self.scalar_collections.$arena();
                let values = arena.values_mut(range)?;
                let length = values.len();
                let slot = values
                    .get_mut(index)
                    .ok_or(HeapError::IndexOutOfBounds { index, length })?;
                *slot = $stored;
                Ok(())
            }};
        }
        match storage {
            CollectionStorage::I32 => {
                set_scalar!(i32_mut, RuntimeValue::I32(value) => value)
            }
            CollectionStorage::I64 => {
                set_scalar!(i64_mut, RuntimeValue::I64(value) => value)
            }
            CollectionStorage::F32 => {
                set_scalar!(f32_mut, RuntimeValue::F32(value) => value)
            }
            CollectionStorage::F64 => {
                set_scalar!(f64_mut, RuntimeValue::F64(value) => value)
            }
            CollectionStorage::Bool => {
                set_scalar!(bools_mut, RuntimeValue::Bool(value) => u8::from(value))
            }
            CollectionStorage::Rune => {
                set_scalar!(runes_mut, RuntimeValue::Rune(value) => value)
            }
            CollectionStorage::String => {
                self.shade_on_write(value);
                set_scalar!(
                    strings_mut,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                )
            }
            CollectionStorage::Ref => {
                self.shade_on_write(value);
                set_scalar!(refs_mut, RuntimeValue::Ref(reference) => reference)
            }
            CollectionStorage::NamedRef => {
                let nexa_bytecode::ValueType::Named(expected) = element_type else {
                    return Err(invalid_value_reference());
                };
                let RuntimeValue::NamedRef { reference, type_id } = value else {
                    return Err(invalid_value_reference());
                };
                if type_id != expected {
                    return Err(invalid_value_reference());
                }
                self.shade_on_write(value);
                let mut arena = self.scalar_collections.refs_mut();
                let values = arena.values_mut(range)?;
                let length = values.len();
                let slot = values
                    .get_mut(index)
                    .ok_or(HeapError::IndexOutOfBounds { index, length })?;
                *slot = reference;
                Ok(())
            }
            CollectionStorage::Values => {
                self.shade_on_write(value);
                let values = self.collections.values_mut(range)?;
                let length = values.len();
                let slot = values
                    .get_mut(index)
                    .ok_or(HeapError::IndexOutOfBounds { index, length })?;
                *slot = value;
                Ok(())
            }
        }
    }

    fn typed_collection_copy_within(
        &mut self,
        storage: CollectionStorage,
        range: CollectionRange,
        source: std::ops::Range<usize>,
        destination: usize,
    ) -> Result<(), HeapError> {
        typed_collection_copy_within_arenas(
            &mut self.collections,
            &mut self.scalar_collections,
            storage,
            range,
            source,
            destination,
        )
    }

    fn typed_collection_clear(
        &mut self,
        storage: CollectionStorage,
        range: CollectionRange,
        cells: std::ops::Range<usize>,
    ) -> Result<(), HeapError> {
        typed_collection_clear_in_arenas(
            &mut self.collections,
            &mut self.scalar_collections,
            storage,
            range,
            cells,
        )
    }

    #[must_use]
    pub(crate) fn collection_arena_fuel_shape(&self) -> CollectionArenaFuelShape {
        CollectionArenaFuelShape {
            free_ranges: self.collections.free_ranges.len(),
        }
    }

    pub fn commit_collection_value(
        &mut self,
        reservation: &mut CollectionReservation,
        value: RuntimeValue,
    ) -> Result<(), HeapError> {
        if !reservation.claimed || reservation.written >= reservation.range.length {
            return Err(HeapError::IndexOutOfBounds {
                index: reservation.written,
                length: reservation.range.length,
            });
        }
        let index = reservation.range.start + reservation.written;
        // G1 barrier: initial collection elements are published into an
        // extent owned by a born-black object while marking runs.
        self.shade_on_write(value);
        self.collections.values[index] = value;
        reservation.written += 1;
        Ok(())
    }

    pub(crate) fn release_collection_reservation(
        &mut self,
        reservation: &mut CollectionReservation,
    ) {
        if reservation.claimed {
            self.collections.release(reservation.range);
            self.release_collection_quota(reservation.range.length);
            reservation.claimed = false;
            reservation.written = 0;
        }
    }

    pub(crate) fn complete_collection_reservation(
        reservation: &mut CollectionReservation,
    ) -> Result<(), HeapError> {
        if reservation.written != reservation.range.length {
            return Err(HeapError::IndexOutOfBounds {
                index: reservation.written,
                length: reservation.range.length,
            });
        }
        reservation.claimed = false;
        Ok(())
    }

    pub(crate) fn begin_host_transaction(&mut self) -> Result<(), HeapError> {
        if self.host_transaction_active {
            return Err(HeapError::CapacityExhausted);
        }
        self.host_staging.clear();
        self.host_transaction_active = true;
        Ok(())
    }

    pub(crate) fn host_staging_roots(&self) -> &[GcRef] {
        &self.host_staging
    }

    pub(crate) fn commit_host_transaction(&mut self) {
        self.host_transaction_active = false;
        self.host_staging.clear();
    }

    pub(crate) fn rollback_host_transaction(&mut self) {
        self.host_transaction_active = false;
        let mut released = 0_u64;
        let mut collection_released = 0_u64;
        let mut recycled = false;
        while let Some(reference) = self.host_staging.pop() {
            let object = self
                .slots
                .get_mut(reference.index as usize)
                .filter(|slot| slot.generation == reference.generation)
                .and_then(|slot| slot.object.take());
            if let Some(object) = object {
                self.live_objects = self
                    .live_objects
                    .checked_sub(1)
                    .expect("a staged object was counted as live");
                // G6: staged objects vanish outside the sweep, so their
                // footprint leaves the gauge here. Every collection now owns
                // one typed extent; committed staged objects release that
                // extent here, while unfinished builders are released by the
                // transaction drop path.
                released = released.saturating_add(self.object_payload_bytes(&object));
                collection_released =
                    collection_released.saturating_add(self.object_collection_bytes(&object));
                match object {
                    Object::Array { storage, range, .. }
                    | Object::Buffer { storage, range, .. }
                    | Object::Struct { storage, range, .. }
                    | Object::Class { storage, range, .. } => {
                        self.release_typed_collection(storage, range);
                    }
                    Object::Map { storage } => {
                        if let Some(map) = self.maps[storage as usize].take() {
                            self.release_map_value_extents(&map);
                            self.map_slots.release(map.slots);
                            if let Some(rehash) = map.rehash {
                                self.map_slots.release(rehash.old_slots);
                                self.map_slots.release(rehash.new_slots);
                            }
                            self.free_maps.push(storage);
                        }
                    }
                    Object::Set { storage } => {
                        if let Some(set) = self.sets[storage as usize].take() {
                            self.map_slots.release(set.slots);
                            if let Some(rehash) = set.rehash {
                                self.map_slots.release(rehash.old_slots);
                                self.map_slots.release(rehash.new_slots);
                            }
                            self.free_sets.push(storage);
                        }
                    }
                    _ => {}
                }
                self.free.push(reference.index);
                recycled = true;
            }
        }
        if recycled {
            // Rollback frees slots without bumping their generation, so a
            // literal cached at the same (index, generation) could later
            // alias different content; drop the cache instead.
            self.string_literal_cache.clear();
            self.pooled_string_cache.clear();
        }
        self.release_live_payload(released);
        self.live_collection_bytes = self
            .live_collection_bytes
            .saturating_sub(collection_released);
    }

    pub(crate) fn commit_array_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        storage: CollectionStorage,
        range: CollectionRange,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::array_type(element_type) {
            return Err(invalid_value_reference());
        }
        self.validate_collection_length(range.length)?;
        let payload_bytes = (range.length as u64).saturating_mul(storage.cell_size() as u64);
        self.ensure_new_object_headroom(payload_bytes, false)?;
        self.ensure_collection_headroom(payload_bytes)?;
        let length = range.length;
        let reference = self.commit(
            reservation,
            Object::Array {
                type_id,
                element_type,
                storage,
                range,
                length,
                row_stride: None,
                mutation_epoch: 0,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub(crate) fn commit_buffer_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        storage: CollectionStorage,
        range: CollectionRange,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::buffer_type(element_type) {
            return Err(invalid_value_reference());
        }
        self.validate_collection_length(range.length)?;
        let payload_bytes = (range.length as u64).saturating_mul(storage.cell_size() as u64);
        self.ensure_new_object_headroom(payload_bytes, false)?;
        self.ensure_collection_headroom(payload_bytes)?;
        let reference = self.commit(
            reservation,
            Object::Buffer {
                type_id,
                element_type,
                storage,
                range,
                mutation_epoch: 0,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub(crate) fn commit_array_values_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        let storage = collection_storage_for_values(element_type, values)?;
        let range = self.claim_typed_collection(storage, values.len())?;
        for (index, value) in values.iter().copied().enumerate() {
            if let Err(error) =
                self.typed_collection_set(storage, element_type, range, index, value)
            {
                self.release_typed_collection(storage, range);
                return Err(error);
            }
        }
        match self.commit_array_reserved(reservation, type_id, element_type, storage, range) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.release_typed_collection(storage, range);
                Err(error)
            }
        }
    }

    pub(crate) fn commit_buffer_values_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        let storage = collection_storage_for_values(element_type, values)?;
        let range = self.claim_typed_collection(storage, values.len())?;
        for (index, value) in values.iter().copied().enumerate() {
            if let Err(error) =
                self.typed_collection_set(storage, element_type, range, index, value)
            {
                self.release_typed_collection(storage, range);
                return Err(error);
            }
        }
        match self.commit_buffer_reserved(reservation, type_id, element_type, storage, range) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.release_typed_collection(storage, range);
                Err(error)
            }
        }
    }

    pub fn allocate_enum(
        &mut self,
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<RuntimeValue>,
    ) -> Result<RuntimeValue, HeapError> {
        self.ensure_new_object_headroom(0, false)?;
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

    pub fn enum_equal(&self, lhs: RuntimeValue, rhs: RuntimeValue) -> Result<bool, HeapError> {
        let (lhs_type, lhs_variant, lhs_tag, lhs_payload) = self.enum_parts(lhs)?;
        let (rhs_type, rhs_variant, rhs_tag, rhs_payload) = self.enum_parts(rhs)?;
        if lhs_type != rhs_type || lhs_variant != rhs_variant || lhs_tag != rhs_tag {
            return Ok(false);
        }
        match (lhs_payload, rhs_payload) {
            (Some(lhs), Some(rhs)) => self.runtime_value_equal(lhs, rhs),
            (None, None) => Ok(true),
            _ => Ok(false),
        }
    }

    pub fn allocate_struct(
        &mut self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if u16::try_from(fields.len()).is_err() {
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
        let field_bytes = (fields.len() as u64)
            .saturating_mul(homogeneous_field_storage(fields).cell_size() as u64);
        self.ensure_new_object_headroom(field_bytes, false)?;
        self.ensure_collection_headroom(field_bytes)?;
        let hash = self.structural_hash(type_id, fields)?;
        let (storage, range) = self.claim_field_extent(fields)?;
        let reference = self.commit(
            reservation,
            Object::Struct {
                type_id,
                storage,
                range,
                field_count: u16::try_from(fields.len())
                    .map_err(|_| HeapError::CapacityExhausted)?,
                hash,
            },
        );
        Ok(RuntimeValue::Struct {
            reference,
            type_id,
            hash,
        })
    }

    /// K5: claims one collection-arena extent for a struct/class field
    /// row, writes the fields, and settles G1 bookkeeping. The caller
    /// commits the owning object immediately afterwards (no fallible step
    /// in between), so a claimed extent can never leak on an error path.
    /// G6 is charged once by the owning object's commit funnel.
    fn claim_field_extent(
        &mut self,
        fields: &[RuntimeValue],
    ) -> Result<(CollectionStorage, CollectionRange), HeapError> {
        let storage = homogeneous_field_storage(fields);
        let range = self.claim_typed_collection(storage, fields.len())?;
        let element_type = field_type_for_storage(storage);
        for (index, field) in fields.iter().copied().enumerate() {
            let result = if let Some(element_type) = element_type {
                self.typed_collection_set(storage, element_type, range, index, field)
            } else {
                self.typed_collection_set(
                    CollectionStorage::Values,
                    nexa_bytecode::ValueType::Ref,
                    range,
                    index,
                    field,
                )
            };
            if let Err(error) = result {
                self.release_typed_collection(storage, range);
                return Err(error);
            }
        }
        // G1: the owner is born marked while a cycle is active, so its
        // children shade in `typed_collection_set`, exactly like Array.
        Ok((storage, range))
    }

    pub fn struct_fields(&self, value: RuntimeValue) -> Result<CollectionView<'_>, HeapError> {
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
                storage,
                range,
                field_count,
                hash: actual_hash,
            } if *actual == type_id && *actual_hash == hash => {
                self.field_view(*storage, *range, usize::from(*field_count))
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
        let field_count = fields.len();
        let mut updated = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        for (destination, field) in updated[..field_count].iter_mut().zip(fields.iter()) {
            *destination = field;
        }
        updated[index] = replacement;
        self.allocate_struct(type_id, &updated[..field_count])
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
        lhs.iter()
            .zip(rhs.iter())
            .try_fold(true, |equal, (lhs, rhs)| {
                Ok(equal && self.runtime_value_equal(lhs, rhs)?)
            })
    }

    pub fn allocate_class(
        &mut self,
        type_id: StableId,
        fields: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        if u16::try_from(fields.len()).is_err() {
            return Err(HeapError::CapacityExhausted);
        }
        let field_bytes = (fields.len() as u64)
            .saturating_mul(homogeneous_field_storage(fields).cell_size() as u64);
        self.ensure_new_object_headroom(field_bytes, false)?;
        self.ensure_collection_headroom(field_bytes)?;
        // Slot preflight before the extent claim keeps the pair atomic:
        // once the extent is claimed, the commit cannot fail.
        let mut reservation = self.preflight(1)?;
        let (storage, range) = self.claim_field_extent(fields)?;
        let reference = self.commit(
            &mut reservation,
            Object::Class {
                type_id,
                storage,
                range,
                field_count: u16::try_from(fields.len())
                    .map_err(|_| HeapError::CapacityExhausted)?,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn class_field(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let RuntimeValue::NamedRef { reference, .. } = value else {
            return Err(invalid_value_reference());
        };
        self.class_fields(value)?
            .get(index)
            .ok_or(HeapError::InvalidReference(reference))
    }

    pub(crate) fn class_field_range(
        &self,
        value: RuntimeValue,
        offset: usize,
        slots: usize,
    ) -> Result<CollectionView<'_>, HeapError> {
        let RuntimeValue::NamedRef { reference, .. } = value else {
            return Err(invalid_value_reference());
        };
        self.class_fields(value)?
            .slice(offset, slots)
            .ok_or(HeapError::InvalidReference(reference))
    }

    pub(crate) fn class_fields(
        &self,
        value: RuntimeValue,
    ) -> Result<CollectionView<'_>, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Class {
                type_id: actual,
                storage,
                range,
                field_count,
            } if *actual == type_id => self.field_view(*storage, *range, usize::from(*field_count)),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// K5 inspection: the field cells of a struct or class resolved by
    /// reference alone (no value-side type or hash re-validation).
    /// Read-only tooling and tests use this where they previously
    /// destructured the inline field array out of [`Object`].
    pub fn object_fields(&self, reference: GcRef) -> Result<CollectionView<'_>, HeapError> {
        match self.resolve(reference)? {
            Object::Struct {
                storage,
                range,
                field_count,
                ..
            }
            | Object::Class {
                storage,
                range,
                field_count,
                ..
            } => self.field_view(*storage, *range, usize::from(*field_count)),
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
        self.write_barrier(reference, replacement)?;
        let range = match self.resolve(reference)? {
            Object::Class {
                type_id: actual,
                storage,
                range,
                field_count,
            } if *actual == type_id && index < usize::from(*field_count) => (*storage, *range),
            _ => return Err(HeapError::InvalidReference(reference)),
        };
        let element_type = field_type_for_storage(range.0).unwrap_or(nexa_bytecode::ValueType::Ref);
        self.typed_collection_set(range.0, element_type, range.1, index, replacement)
    }

    pub(crate) fn set_class_field_range(
        &mut self,
        value: RuntimeValue,
        offset: usize,
        replacement: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        let (storage, range, field_count) = match self.resolve(reference)? {
            Object::Class {
                type_id: actual,
                storage,
                range,
                field_count,
            } if *actual == type_id => (*storage, *range, usize::from(*field_count)),
            _ => return Err(HeapError::InvalidReference(reference)),
        };
        let _end = offset
            .checked_add(replacement.len())
            .filter(|end| *end <= field_count)
            .ok_or(HeapError::IndexOutOfBounds {
                index: offset.saturating_add(replacement.len()),
                length: field_count,
            })?;
        self.validate_reference(reference)?;
        for value in replacement {
            if let Some(child) = value_reference(*value) {
                self.validate_reference(child)?;
            }
        }
        let element_type = field_type_for_storage(storage).unwrap_or(nexa_bytecode::ValueType::Ref);
        for (index, replacement) in replacement.iter().copied().enumerate() {
            self.typed_collection_set(storage, element_type, range, offset + index, replacement)?;
        }
        Ok(())
    }

    /// Publishes a value into an already allocated GC object.
    ///
    /// Validates both sides before mutation: a forged or stale child
    /// reference must never become reachable through a live object. While
    /// an incremental mark phase is active (G1), the published child is
    /// also shaded gray to preserve the tri-color invariant.
    fn write_barrier(&mut self, owner: GcRef, replacement: RuntimeValue) -> Result<(), HeapError> {
        self.validate_reference(owner)?;
        if let Some(child) = value_reference(replacement) {
            self.validate_reference(child)?;
        }
        self.shade_on_write(replacement);
        Ok(())
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
            storage: CollectionStorage::for_type(element_type),
            range: CollectionRange::default(),
            length: 0,
            row_stride: None,
            mutation_epoch: 0,
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    /// WP52: allocates an array whose aggregate elements live flattened in
    /// the collection arena - `row_slots` physical cells per element, zero
    /// heap objects per element.
    pub fn allocate_value_row_array(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        row_slots: std::num::NonZeroU16,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::array_type(element_type)
            || !matches!(element_type, nexa_bytecode::ValueType::Named(_))
        {
            return Err(invalid_value_reference());
        }
        let reference = self.allocate(Object::Array {
            type_id,
            element_type,
            storage: CollectionStorage::Values,
            range: CollectionRange::default(),
            length: 0,
            row_stride: Some(row_slots),
            mutation_epoch: 0,
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn array_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.array_parts(value)?.length)
    }

    /// Logical element capacity retained by the WP48 array header.
    ///
    /// Flattened struct rows store multiple arena cells per element, so the
    /// physical extent must always be divided by its row stride at this API
    /// boundary.
    pub fn array_capacity(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        let parts = self.array_parts(value)?;
        Ok(parts.range.length / parts.stride())
    }

    /// Ensures room for `additional` elements without changing the logical
    /// length. Growth first attempts to extend the existing typed-arena
    /// extent in place; relocation is the bounded fallback.
    pub fn array_reserve(
        &mut self,
        value: RuntimeValue,
        additional: usize,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let needed = parts
            .length
            .checked_add(additional)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(needed)?;
        let current = parts.range.length / parts.stride();
        if needed <= current {
            return Ok(());
        }
        let target = grown_array_capacity(current, needed, self.max_collection_length);
        self.resize_array_capacity(parts, target)
    }

    /// Releases every unused capacity cell while preserving the live prefix.
    ///
    /// Arena ranges are tail-splittable, so shrinking never copies elements
    /// and never asks the system allocator for temporary storage.
    pub fn array_shrink_to_fit(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        self.resize_array_capacity(parts, parts.length)
    }

    pub fn array_get(
        &mut self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        let Some(stride) = parts.rows() else {
            return self.typed_collection_get(
                parts.storage,
                parts.element_type,
                parts.range,
                index,
            );
        };
        // WP52 rows: reading a logical element materializes one transient
        // struct value from the flattened cells; the storage itself never
        // holds a per-element object.
        let struct_type = parts.element_struct_type()?;
        let mut fields = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        let cells = self.collections.values(parts.range)?;
        fields[..stride].copy_from_slice(&cells[index * stride..(index + 1) * stride]);
        let mut reservation = self.preflight(1)?;
        self.commit_struct(&mut reservation, struct_type, &fields[..stride])
    }

    /// Borrows one element in its physical storage layout. Aggregate rows
    /// expose their complete flattened slot range; scalar/reference arrays
    /// expose a one-cell typed view.
    pub fn array_value_range(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<CollectionView<'_>, HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        if let Some(stride) = parts.rows() {
            return self
                .collections
                .values(parts.range)?
                .get(index * stride..(index + 1) * stride)
                .map(CollectionView::Values)
                .ok_or(HeapError::IndexOutOfBounds {
                    index: (index + 1) * stride,
                    length: parts.range.length,
                });
        }
        self.typed_collection_view(parts.storage, parts.element_type, parts.range)?
            .slice(index, 1)
            .ok_or(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            })
    }

    /// Borrows a physical field subrange from one flattened aggregate row.
    pub fn array_field_range(
        &self,
        value: RuntimeValue,
        index: usize,
        offset: usize,
        slots: usize,
    ) -> Result<CollectionView<'_>, HeapError> {
        let row = self.array_value_range(value, index)?;
        row.slice(offset, slots).ok_or(HeapError::IndexOutOfBounds {
            index: offset.saturating_add(slots),
            length: row.len(),
        })
    }

    pub fn array_set_row(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        let stride = parts.rows().ok_or_else(invalid_value_reference)?;
        if replacement.len() != stride {
            return Err(invalid_value_reference());
        }
        for value in replacement {
            self.shade_on_write(*value);
        }
        // `LANGUAGE_V3` 4.3 epoch discipline: reserve the next epoch (traps
        // before any write when exhausted), then commit after success.
        let epoch = self.next_array_epoch(parts.reference)?;
        self.collections.values_mut(parts.range)?[index * stride..(index + 1) * stride]
            .copy_from_slice(replacement);
        self.commit_array_epoch(parts.reference, epoch)
    }

    pub fn array_set(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        self.write_array_element_no_epoch(parts, index, replacement)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    /// Element write without epoch accounting, shared by `array_set` and
    /// the multi-write `ArraySwap`/`ArrayReverse` primitives (which reserve
    /// and commit exactly one epoch per public operation).
    fn write_array_element_no_epoch(
        &mut self,
        parts: ArrayParts,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let Some(stride) = parts.rows() else {
            return self.typed_collection_set(
                parts.storage,
                parts.element_type,
                parts.range,
                index,
                replacement,
            );
        };
        let row = self.struct_row(parts.element_struct_type()?, stride, replacement)?;
        for field in &row[..stride] {
            self.shade_on_write(*field);
        }
        self.collections.values_mut(parts.range)?[index * stride..(index + 1) * stride]
            .copy_from_slice(&row[..stride]);
        Ok(())
    }

    /// Infallible after the caller validated both indices: swaps the
    /// flattened cells of two elements without materializing or touching
    /// the epoch.
    fn swap_array_elements_no_epoch(
        &mut self,
        parts: ArrayParts,
        lhs: usize,
        rhs: usize,
    ) -> Result<(), HeapError> {
        let Some(stride) = parts.rows() else {
            let left =
                self.typed_collection_get(parts.storage, parts.element_type, parts.range, lhs)?;
            let right =
                self.typed_collection_get(parts.storage, parts.element_type, parts.range, rhs)?;
            self.typed_collection_set(parts.storage, parts.element_type, parts.range, lhs, right)?;
            return self.typed_collection_set(
                parts.storage,
                parts.element_type,
                parts.range,
                rhs,
                left,
            );
        };
        let values = self.collections.values_mut(parts.range)?;
        let left_row = lhs * stride..(lhs + 1) * stride;
        let right_row = rhs * stride..(rhs + 1) * stride;
        let mut scratch = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        scratch[..stride].copy_from_slice(&values[left_row.clone()]);
        values.copy_within(right_row.clone(), left_row.start);
        values[right_row].copy_from_slice(&scratch[..stride]);
        Ok(())
    }

    /// `LANGUAGE_V3` `ArraySwap`: swaps the elements at `lhs` and `rhs` in
    /// place. Bounds are validated before any write; the mutation advances
    /// the epoch exactly once (never per element), so an exhausted epoch
    /// traps before the first write.
    pub fn array_swap(
        &mut self,
        value: RuntimeValue,
        lhs: usize,
        rhs: usize,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if lhs >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index: lhs,
                length: parts.length,
            });
        }
        if rhs >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index: rhs,
                length: parts.length,
            });
        }
        if lhs == rhs {
            return Ok(());
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        self.swap_array_elements_no_epoch(parts, lhs, rhs)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    /// `LANGUAGE_V3` `ArrayReverse`: reverses the live prefix in place with
    /// one epoch increment per public operation.
    pub fn array_reverse(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if parts.length < 2 {
            return Ok(());
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let mut left = 0;
        let mut right = parts.length - 1;
        while left < right {
            self.swap_array_elements_no_epoch(parts, left, right)?;
            left += 1;
            right -= 1;
        }
        self.commit_array_epoch(parts.reference, epoch)
    }

    #[allow(clippy::too_many_lines)]
    pub fn array_push(
        &mut self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let current = parts.length;
        let length = current
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(length)?;
        macro_rules! push_scalar {
            ($arena:ident, $pattern:pat => $stored:expr) => {{
                let $pattern = element else {
                    return Err(invalid_value_reference());
                };
                let stored = $stored;
                let range = if current == parts.range.length {
                    let capacity = grown_array_capacity(
                        parts.range.length,
                        length,
                        self.max_collection_length,
                    );
                    self.resize_array_capacity(parts, capacity)?;
                    self.array_range(parts.reference)?
                } else {
                    parts.range
                };
                let epoch = self.next_array_epoch(parts.reference)?;
                self.scalar_collections.$arena().values_mut(range)?[current] = stored;
                self.set_array_length(parts.reference, length)?;
                self.commit_array_epoch(parts.reference, epoch)
            }};
        }
        match parts.storage {
            CollectionStorage::I32 => {
                return push_scalar!(i32_mut, RuntimeValue::I32(value) => value);
            }
            CollectionStorage::I64 => {
                return push_scalar!(i64_mut, RuntimeValue::I64(value) => value);
            }
            CollectionStorage::F32 => {
                return push_scalar!(f32_mut, RuntimeValue::F32(value) => value);
            }
            CollectionStorage::F64 => {
                return push_scalar!(f64_mut, RuntimeValue::F64(value) => value);
            }
            CollectionStorage::Bool => {
                return push_scalar!(
                    bools_mut,
                    RuntimeValue::Bool(value) => u8::from(value)
                );
            }
            CollectionStorage::Rune => {
                return push_scalar!(runes_mut, RuntimeValue::Rune(value) => value);
            }
            CollectionStorage::String => {
                self.shade_on_write(element);
                return push_scalar!(
                    strings_mut,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                );
            }
            CollectionStorage::Ref => {
                self.shade_on_write(element);
                return push_scalar!(refs_mut, RuntimeValue::Ref(reference) => reference);
            }
            CollectionStorage::NamedRef => {
                let nexa_bytecode::ValueType::Named(expected) = parts.element_type else {
                    return Err(invalid_value_reference());
                };
                let RuntimeValue::NamedRef { reference, type_id } = element else {
                    return Err(invalid_value_reference());
                };
                if type_id != expected {
                    return Err(invalid_value_reference());
                }
                self.shade_on_write(element);
                return push_scalar!(refs_mut, RuntimeValue::NamedRef { .. } => reference);
            }
            CollectionStorage::Values => {}
        }
        let Some(stride) = parts.rows() else {
            self.shade_on_write(element);
            let range = if current == parts.range.length {
                let capacity =
                    grown_array_capacity(parts.range.length, length, self.max_collection_length);
                self.resize_array_capacity(parts, capacity)?;
                self.array_range(parts.reference)?
            } else {
                parts.range
            };
            let epoch = self.next_array_epoch(parts.reference)?;
            self.collections.values_mut(range)?[current] = element;
            self.set_array_length(parts.reference, length)?;
            return self.commit_array_epoch(parts.reference, epoch);
        };
        let row = self.struct_row(parts.element_struct_type()?, stride, element)?;
        self.push_row_cells(parts, length, &row[..stride])
    }

    /// WP52 push-side fusion: pushes one struct element built directly
    /// from `fields` (declared order). Flattened rows receive the fields
    /// with no source object; cell-layout arrays (host-decoded) fall back
    /// to exactly the unfused materialize-then-push path.
    pub fn array_push_row(
        &mut self,
        value: RuntimeValue,
        fields: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let length = parts
            .length
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(length)?;
        let stride = parts.rows().ok_or_else(invalid_value_reference)?;
        if fields.len() != stride {
            return Err(invalid_value_reference());
        }
        self.push_row_cells(parts, length, fields)
    }

    /// Appends one verifier-proven physical element range.
    ///
    /// Aggregate arrays consume the complete flattened row. Scalar and
    /// reference arrays consume the single value slot. This is the
    /// materialization boundary used by the physical standard-library ABI;
    /// it never constructs an intermediate Struct or Enum heap object.
    pub(crate) fn array_push_value_range(
        &mut self,
        value: RuntimeValue,
        element: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if let Some(stride) = parts.rows() {
            let length = parts
                .length
                .checked_add(1)
                .ok_or(HeapError::CollectionTooLarge {
                    length: usize::MAX,
                    max_length: self.max_collection_length,
                })?;
            self.validate_collection_length(length)?;
            if element.len() != stride {
                return Err(invalid_value_reference());
            }
            return self.push_row_cells(parts, length, element);
        }
        let [element] = element else {
            return Err(invalid_value_reference());
        };
        self.array_push(value, *element)
    }

    pub fn array_insert_row(
        &mut self,
        value: RuntimeValue,
        index: usize,
        row: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let current = parts.length;
        if index > current {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: current,
            });
        }
        let stride = parts.rows().ok_or_else(invalid_value_reference)?;
        if row.len() != stride {
            return Err(invalid_value_reference());
        }
        let length = current
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(length)?;
        for value in row {
            self.shade_on_write(*value);
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let needed_cells = length
            .checked_mul(stride)
            .ok_or(HeapError::CapacityExhausted)?;
        if needed_cells <= parts.range.length {
            let values = self.collections.values_mut(parts.range)?;
            values.copy_within(index * stride..current * stride, (index + 1) * stride);
            values[index * stride..(index + 1) * stride].copy_from_slice(row);
            self.counters.collection_relocation_bytes =
                self.counters.collection_relocation_bytes.saturating_add(
                    ((current - index) * stride * std::mem::size_of::<RuntimeValue>()) as u64,
                );
            self.set_array_length(parts.reference, length)?;
            return self.commit_array_epoch(parts.reference, epoch);
        }
        let capacity_cells = grown_array_capacity(
            parts.range.length / stride,
            length,
            self.max_collection_length,
        )
        .saturating_mul(stride);
        self.regrow_array(
            parts.reference,
            parts.range,
            current * stride,
            capacity_cells,
            |values| {
                values.copy_within(index * stride..current * stride, (index + 1) * stride);
                values[index * stride..(index + 1) * stride].copy_from_slice(row);
            },
        )?;
        self.set_array_length(parts.reference, length)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    pub fn array_pop_row_discard(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let stride = parts.rows().ok_or_else(invalid_value_reference)?;
        if parts.length == 0 {
            return Err(HeapError::IndexOutOfBounds {
                index: 0,
                length: 0,
            });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        self.collections.values_mut(parts.range)?
            [(parts.length - 1) * stride..parts.length * stride]
            .fill(RuntimeValue::Unit);
        self.set_array_length(parts.reference, parts.length - 1)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    pub fn array_remove_row_discard(
        &mut self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let stride = parts.rows().ok_or_else(invalid_value_reference)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let values = self.collections.values_mut(parts.range)?;
        values.copy_within((index + 1) * stride..parts.length * stride, index * stride);
        values[(parts.length - 1) * stride..parts.length * stride].fill(RuntimeValue::Unit);
        self.counters.collection_relocation_bytes =
            self.counters.collection_relocation_bytes.saturating_add(
                ((parts.length - 1 - index) * stride * std::mem::size_of::<RuntimeValue>()) as u64,
            );
        self.set_array_length(parts.reference, parts.length - 1)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    /// Shared row-append tail for [`Self::array_push`] and
    /// [`Self::array_push_row`]: shades every stored field, then writes in
    /// place or grows row-aligned (WP49 amortized).
    fn push_row_cells(
        &mut self,
        parts: ArrayParts,
        length: usize,
        row: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let stride = row.len();
        let current = parts.length;
        for field in row {
            self.shade_on_write(*field);
        }
        let needed_cells = length
            .checked_mul(stride)
            .ok_or(HeapError::CapacityExhausted)?;
        let range = if needed_cells > parts.range.length {
            // Growth is computed in logical elements so the new extent stays
            // row-aligned; `resize_array_capacity` first extends the arena
            // tail in place and relocates only when another extent blocks it.
            let capacity = grown_array_capacity(
                parts.range.length / stride,
                length,
                self.max_collection_length,
            );
            self.resize_array_capacity(parts, capacity)?;
            self.array_range(parts.reference)?
        } else {
            parts.range
        };
        let epoch = self.next_array_epoch(parts.reference)?;
        self.collections.values_mut(range)?[current * stride..needed_cells].copy_from_slice(row);
        self.set_array_length(parts.reference, length)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    pub fn array_pop(&mut self, value: RuntimeValue) -> Result<RuntimeValue, HeapError> {
        let parts = self.array_parts(value)?;
        let length = parts.length;
        if length == 0 {
            return Err(HeapError::IndexOutOfBounds { index: 0, length });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let Some(stride) = parts.rows() else {
            let result = self.typed_collection_get(
                parts.storage,
                parts.element_type,
                parts.range,
                length - 1,
            )?;
            self.typed_collection_clear(parts.storage, parts.range, length - 1..length)?;
            self.set_array_length(parts.reference, length - 1)?;
            self.commit_array_epoch(parts.reference, epoch)?;
            return Ok(result);
        };
        // Materialize the row before mutating anything: a failed struct
        // allocation must leave the array untouched (failure atomicity).
        let result = self.array_get(value, length - 1)?;
        let values = self.collections.values_mut(parts.range)?;
        values[(length - 1) * stride..length * stride].fill(RuntimeValue::Unit);
        self.set_array_length(parts.reference, length - 1)?;
        self.commit_array_epoch(parts.reference, epoch)?;
        Ok(result)
    }

    /// Removes the final element after its physical range has been copied to
    /// the caller-owned result slots.
    pub(crate) fn array_pop_value_discard(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        if parts.length == 0 {
            return Err(HeapError::IndexOutOfBounds {
                index: 0,
                length: 0,
            });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let index = parts.length - 1;
        if let Some(stride) = parts.rows() {
            self.collections.values_mut(parts.range)?[index * stride..parts.length * stride]
                .fill(RuntimeValue::Unit);
        } else {
            self.typed_collection_clear(parts.storage, parts.range, index..parts.length)?;
        }
        self.set_array_length(parts.reference, index)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    #[allow(clippy::too_many_lines)]
    pub fn array_insert(
        &mut self,
        value: RuntimeValue,
        index: usize,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let parts = self.array_parts(value)?;
        let current = parts.length;
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
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(length)?;
        if parts.rows().is_none() && current < parts.range.length {
            self.typed_collection_copy_within(
                parts.storage,
                parts.range,
                index..current,
                index + 1,
            )?;
            self.typed_collection_set(
                parts.storage,
                parts.element_type,
                parts.range,
                index,
                element,
            )?;
            let element_bytes = parts.storage.cell_size();
            self.counters.collection_relocation_bytes = self
                .counters
                .collection_relocation_bytes
                .saturating_add(((current - index) * element_bytes) as u64);
            self.set_array_length(parts.reference, length)?;
            return Ok(());
        }
        macro_rules! grow_insert_scalar {
            ($arena:ident, $pattern:pat => $stored:expr) => {{
                let $pattern = element else {
                    return Err(invalid_value_reference());
                };
                let stored = $stored;
                let capacity =
                    grown_array_capacity(parts.range.length, length, self.max_collection_length);
                let global_range =
                    self.claim_global_collection_range(capacity, size_of_val(&stored))?;
                let new_range = match {
                    let mut arena = self.scalar_collections.$arena();
                    claim_scalar_regrow(&mut arena, global_range, parts.range, current, |values| {
                        values.copy_within(index..current, index + 1);
                        values[index] = stored;
                    })
                } {
                    Ok(range) => range,
                    Err(error) => {
                        self.collections.release(global_range);
                        self.release_collection_quota(capacity);
                        return Err(error);
                    }
                };
                if let Err(error) = self.set_array_range(parts.reference, new_range) {
                    self.scalar_collections.$arena().release(new_range);
                    self.collections.release(new_range);
                    self.release_collection_quota(new_range.length);
                    return Err(error);
                }
                self.scalar_collections.$arena().release(parts.range);
                self.collections.release(parts.range);
                self.release_collection_quota(parts.range.length);
                let element_bytes = size_of_val(&stored) as u64;
                self.charge_collection_payload(
                    (new_range.length as u64).saturating_mul(element_bytes),
                );
                self.release_collection_payload(
                    (parts.range.length as u64).saturating_mul(element_bytes),
                );
                self.counters.collection_relocation_bytes = self
                    .counters
                    .collection_relocation_bytes
                    .saturating_add((current as u64).saturating_mul(element_bytes));
                return self.set_array_length(parts.reference, length);
            }};
        }
        match parts.storage {
            CollectionStorage::I32 => {
                grow_insert_scalar!(i32_mut, RuntimeValue::I32(value) => value);
            }
            CollectionStorage::I64 => {
                grow_insert_scalar!(i64_mut, RuntimeValue::I64(value) => value);
            }
            CollectionStorage::F32 => {
                grow_insert_scalar!(f32_mut, RuntimeValue::F32(value) => value);
            }
            CollectionStorage::F64 => {
                grow_insert_scalar!(f64_mut, RuntimeValue::F64(value) => value);
            }
            CollectionStorage::Bool => {
                grow_insert_scalar!(
                    bools_mut,
                    RuntimeValue::Bool(value) => u8::from(value)
                );
            }
            CollectionStorage::Rune => {
                grow_insert_scalar!(runes_mut, RuntimeValue::Rune(value) => value);
            }
            CollectionStorage::String => {
                self.shade_on_write(element);
                grow_insert_scalar!(
                    strings_mut,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                );
            }
            CollectionStorage::Ref => {
                self.shade_on_write(element);
                grow_insert_scalar!(refs_mut, RuntimeValue::Ref(reference) => reference);
            }
            CollectionStorage::NamedRef => {
                let nexa_bytecode::ValueType::Named(expected) = parts.element_type else {
                    return Err(invalid_value_reference());
                };
                let RuntimeValue::NamedRef { reference, type_id } = element else {
                    return Err(invalid_value_reference());
                };
                if type_id != expected {
                    return Err(invalid_value_reference());
                }
                self.shade_on_write(element);
                grow_insert_scalar!(
                    refs_mut,
                    RuntimeValue::NamedRef { .. } => reference
                );
            }
            CollectionStorage::Values => {}
        }
        let Some(stride) = parts.rows() else {
            self.shade_on_write(element);
            let capacity =
                grown_array_capacity(parts.range.length, length, self.max_collection_length);
            let epoch = self.next_array_epoch(parts.reference)?;
            self.regrow_array(parts.reference, parts.range, current, capacity, |values| {
                values.copy_within(index..current, index + 1);
                values[index] = element;
            })?;
            self.set_array_length(parts.reference, length)?;
            return self.commit_array_epoch(parts.reference, epoch);
        };
        let row = self.struct_row(parts.element_struct_type()?, stride, element)?;
        for field in &row[..stride] {
            self.shade_on_write(*field);
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let needed_cells = length
            .checked_mul(stride)
            .ok_or(HeapError::CapacityExhausted)?;
        if needed_cells <= parts.range.length {
            let values = self.collections.values_mut(parts.range)?;
            values.copy_within(index * stride..current * stride, (index + 1) * stride);
            values[index * stride..(index + 1) * stride].copy_from_slice(&row[..stride]);
            self.counters.collection_relocation_bytes =
                self.counters.collection_relocation_bytes.saturating_add(
                    ((current - index) * stride * std::mem::size_of::<RuntimeValue>()) as u64,
                );
            self.set_array_length(parts.reference, length)?;
            return self.commit_array_epoch(parts.reference, epoch);
        }
        let capacity_cells = grown_array_capacity(
            parts.range.length / stride,
            length,
            self.max_collection_length,
        )
        .saturating_mul(stride);
        self.regrow_array(
            parts.reference,
            parts.range,
            current * stride,
            capacity_cells,
            |values| {
                values.copy_within(index * stride..current * stride, (index + 1) * stride);
                values[index * stride..(index + 1) * stride].copy_from_slice(&row[..stride]);
            },
        )?;
        self.set_array_length(parts.reference, length)?;
        self.commit_array_epoch(parts.reference, epoch)
    }

    pub fn array_remove(
        &mut self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let parts = self.array_parts(value)?;
        let length = parts.length;
        if index >= length {
            return Err(HeapError::IndexOutOfBounds { index, length });
        }
        let epoch = self.next_array_epoch(parts.reference)?;
        let Some(stride) = parts.rows() else {
            let removed =
                self.typed_collection_get(parts.storage, parts.element_type, parts.range, index)?;
            self.typed_collection_copy_within(
                parts.storage,
                parts.range,
                index + 1..length,
                index,
            )?;
            self.typed_collection_clear(parts.storage, parts.range, length - 1..length)?;
            let element_bytes = parts.storage.cell_size();
            self.counters.collection_relocation_bytes = self
                .counters
                .collection_relocation_bytes
                .saturating_add(((length - 1 - index) * element_bytes) as u64);
            self.set_array_length(parts.reference, length - 1)?;
            self.commit_array_epoch(parts.reference, epoch)?;
            return Ok(removed);
        };
        // Materialize the row before mutating anything: a failed struct
        // allocation must leave the array untouched (failure atomicity).
        let removed = self.array_get(value, index)?;
        self.commit_array_epoch(parts.reference, epoch)?;
        let values = self.collections.values_mut(parts.range)?;
        values.copy_within((index + 1) * stride..length * stride, index * stride);
        values[(length - 1) * stride..length * stride].fill(RuntimeValue::Unit);
        self.counters.collection_relocation_bytes =
            self.counters.collection_relocation_bytes.saturating_add(
                ((length - 1 - index) * stride * std::mem::size_of::<RuntimeValue>()) as u64,
            );
        self.set_array_length(parts.reference, length - 1)?;
        Ok(removed)
    }

    pub fn array_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        // WP50: clear retains capacity; live cells reset so no stale
        // references survive in the extent.
        let parts = self.array_parts(value)?;
        let epoch = if parts.length != 0 {
            Some(self.next_array_epoch(parts.reference)?)
        } else {
            None
        };
        let live_cells = parts.length * parts.stride();
        self.typed_collection_clear(parts.storage, parts.range, 0..live_cells)?;
        self.set_array_length(parts.reference, 0)?;
        if let Some(epoch) = epoch {
            self.commit_array_epoch(parts.reference, epoch)?;
        }
        Ok(())
    }

    /// Changes only the retained capacity of an array. `target` is expressed
    /// in logical elements; the arena bookkeeping below operates in cells.
    fn resize_array_capacity(&mut self, parts: ArrayParts, target: usize) -> Result<(), HeapError> {
        if target < parts.length {
            return Err(HeapError::CollectionTooLarge {
                length: parts.length,
                max_length: target,
            });
        }
        self.validate_collection_length(target)?;
        let stride = parts.stride();
        let target_cells = target
            .checked_mul(stride)
            .ok_or(HeapError::CapacityExhausted)?;
        let current_cells = parts.range.length;
        if target_cells == current_cells {
            return Ok(());
        }
        if target_cells < current_cells {
            return self.shrink_array_capacity(parts, target_cells);
        }
        if self.try_extend_array_capacity(parts, target_cells)? {
            return Ok(());
        }
        self.relocate_array_capacity(parts, target_cells)
    }

    fn shrink_array_capacity(
        &mut self,
        parts: ArrayParts,
        target_cells: usize,
    ) -> Result<(), HeapError> {
        let tail = CollectionRange {
            start: parts.range.start + target_cells,
            length: parts.range.length - target_cells,
        };
        self.set_array_range(
            parts.reference,
            CollectionRange {
                start: parts.range.start,
                length: target_cells,
            },
        )?;
        self.release_typed_collection(parts.storage, tail);
        self.release_collection_payload(
            (tail.length as u64).saturating_mul(parts.storage.cell_size() as u64),
        );
        Ok(())
    }

    fn try_extend_array_capacity(
        &mut self,
        parts: ArrayParts,
        target_cells: usize,
    ) -> Result<bool, HeapError> {
        let additional_cells = target_cells - parts.range.length;
        let additional_bytes =
            (additional_cells as u64).saturating_mul(parts.storage.cell_size() as u64);
        self.ensure_payload_headroom(additional_bytes)?;
        self.ensure_collection_headroom(additional_bytes)?;
        self.claim_collection_quota(additional_cells)?;
        let tail = CollectionRange {
            start: parts.range.end(),
            length: additional_cells,
        };
        if self.collections.claim(tail).is_err() {
            self.release_collection_quota(additional_cells);
            return Ok(false);
        }
        if self.claim_physical_collection(parts.storage, tail).is_err() {
            self.collections.release(tail);
            self.release_collection_quota(additional_cells);
            return Ok(false);
        }
        if let Err(error) = self.set_array_range(
            parts.reference,
            CollectionRange {
                start: parts.range.start,
                length: target_cells,
            },
        ) {
            self.release_typed_collection(parts.storage, tail);
            return Err(error);
        }
        self.charge_collection_payload(additional_bytes);
        Ok(true)
    }

    fn relocate_array_capacity(
        &mut self,
        parts: ArrayParts,
        target_cells: usize,
    ) -> Result<(), HeapError> {
        let live_cells = parts
            .length
            .checked_mul(parts.stride())
            .ok_or(HeapError::CapacityExhausted)?;
        if parts.storage == CollectionStorage::Values {
            return self.regrow_array(
                parts.reference,
                parts.range,
                live_cells,
                target_cells,
                |_| {},
            );
        }

        macro_rules! relocate_scalar {
            ($arena:ident) => {{
                let element_bytes = parts.storage.cell_size();
                let global_range =
                    self.claim_global_collection_range(target_cells, element_bytes)?;
                let new_range = match {
                    let mut arena = self.scalar_collections.$arena();
                    claim_scalar_regrow(&mut arena, global_range, parts.range, live_cells, |_| {})
                } {
                    Ok(range) => range,
                    Err(error) => {
                        self.collections.release(global_range);
                        self.release_collection_quota(target_cells);
                        return Err(error);
                    }
                };
                if let Err(error) = self.set_array_range(parts.reference, new_range) {
                    self.scalar_collections.$arena().release(new_range);
                    self.collections.release(new_range);
                    self.release_collection_quota(new_range.length);
                    return Err(error);
                }
                self.scalar_collections.$arena().release(parts.range);
                self.collections.release(parts.range);
                self.release_collection_quota(parts.range.length);
                let new_bytes = (new_range.length as u64).saturating_mul(element_bytes as u64);
                let old_bytes = (parts.range.length as u64).saturating_mul(element_bytes as u64);
                self.charge_collection_payload(new_bytes);
                self.release_collection_payload(old_bytes);
                self.counters.collection_relocation_bytes = self
                    .counters
                    .collection_relocation_bytes
                    .saturating_add((live_cells as u64).saturating_mul(element_bytes as u64));
                return Ok(());
            }};
        }

        match parts.storage {
            CollectionStorage::I32 => relocate_scalar!(i32_mut),
            CollectionStorage::I64 => relocate_scalar!(i64_mut),
            CollectionStorage::F32 => relocate_scalar!(f32_mut),
            CollectionStorage::F64 => relocate_scalar!(f64_mut),
            CollectionStorage::Bool => relocate_scalar!(bools_mut),
            CollectionStorage::Rune => relocate_scalar!(runes_mut),
            CollectionStorage::String => relocate_scalar!(strings_mut),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                relocate_scalar!(refs_mut)
            }
            CollectionStorage::Values => unreachable!("wide storage returned above"),
        }
    }

    fn set_array_length(&mut self, reference: GcRef, new_length: usize) -> Result<(), HeapError> {
        match self.resolve_mut(reference)? {
            Object::Array { length, .. } => {
                *length = new_length;
                Ok(())
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// `LANGUAGE_V3` 4.3 epoch discipline: mutations first *reserve* the next
    /// epoch (non-mutating `checked_add`, so an exhausted epoch traps before
    /// any data write) and *commit* it only after the mutation fully
    /// succeeded. A failed mutation therefore leaves both data and epoch
    /// unchanged.
    fn next_array_epoch(&self, reference: GcRef) -> Result<u64, HeapError> {
        match self.resolve(reference)? {
            Object::Array { mutation_epoch, .. } => mutation_epoch
                .checked_add(1)
                .ok_or(HeapError::MutationEpochExhausted),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn commit_array_epoch(&mut self, reference: GcRef, epoch: u64) -> Result<(), HeapError> {
        match self.resolve_mut(reference)? {
            Object::Array { mutation_epoch, .. } => {
                *mutation_epoch = epoch;
                Ok(())
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn next_buffer_epoch(&self, reference: GcRef) -> Result<u64, HeapError> {
        match self.resolve(reference)? {
            Object::Buffer { mutation_epoch, .. } => mutation_epoch
                .checked_add(1)
                .ok_or(HeapError::MutationEpochExhausted),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn commit_buffer_epoch(&mut self, reference: GcRef, epoch: u64) -> Result<(), HeapError> {
        match self.resolve_mut(reference)? {
            Object::Buffer { mutation_epoch, .. } => {
                *mutation_epoch = epoch;
                Ok(())
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn array_range(&self, reference: GcRef) -> Result<CollectionRange, HeapError> {
        match self.resolve(reference)? {
            Object::Array { range, .. } => Ok(*range),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn set_array_range(
        &mut self,
        reference: GcRef,
        new_range: CollectionRange,
    ) -> Result<(), HeapError> {
        match self.resolve_mut(reference)? {
            Object::Array { range, .. } => {
                *range = new_range;
                Ok(())
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// Deterministic (live, capacity) shape used by fuel settlement before
    /// an array mutation runs (WP49). Both sides are logical elements on
    /// purpose: fuel inputs stay identical across the WP52 row-layout
    /// change until the frozen v7 cost-table boundary.
    pub fn array_fuel_shape(&self, value: RuntimeValue) -> Result<(usize, usize), HeapError> {
        let parts = self.array_parts(value)?;
        Ok((parts.length, parts.range.length / parts.stride()))
    }

    pub(crate) fn array_physical_fuel_shape(
        &self,
        value: RuntimeValue,
    ) -> Result<(usize, usize, usize), HeapError> {
        let parts = self.array_parts(value)?;
        Ok((
            parts.length,
            parts.range.length / parts.stride(),
            parts.stride(),
        ))
    }

    /// Moves the live prefix into a larger extent, applies `write`, and
    /// releases the old extent. Relocation copies exactly `live` elements.
    fn regrow_array(
        &mut self,
        reference: GcRef,
        old_range: CollectionRange,
        live: usize,
        capacity: usize,
        write: impl FnOnce(&mut [RuntimeValue]),
    ) -> Result<(), HeapError> {
        let mut reservation = self.preflight_collection(capacity)?;
        let new_range = reservation.range;
        self.counters.collection_relocation_bytes = self
            .counters
            .collection_relocation_bytes
            .saturating_add((live * std::mem::size_of::<RuntimeValue>()) as u64);
        let old_start = old_range.start;
        let old_end = old_range.end();
        let new_start = new_range.start;
        let new_end = new_range.end();
        if old_range.length == 0 {
            write(&mut self.collections.values[new_start..new_end]);
        } else if new_end <= old_start || old_end <= new_start {
            let (destination, source) = if new_end <= old_start {
                let (left, right) = self.collections.values.split_at_mut(old_start);
                (&mut left[new_start..new_end], &right[..old_range.length])
            } else {
                let (left, right) = self.collections.values.split_at_mut(new_start);
                (&mut right[..capacity], &left[old_start..old_end])
            };
            destination[..live].copy_from_slice(&source[..live]);
            write(destination);
        } else {
            self.release_collection_reservation(&mut reservation);
            return Err(HeapError::CapacityExhausted);
        }
        reservation.written = capacity;
        Self::complete_collection_reservation(&mut reservation)?;
        match self.resolve_mut(reference)? {
            Object::Array { range, .. } => *range = new_range,
            _ => return Err(HeapError::InvalidReference(reference)),
        }
        self.collections.release(old_range);
        self.release_collection_quota(old_range.length);
        // G6: the live object traded extents; adjust the gauge by the
        // actual swap instead of re-deriving the whole footprint.
        let value_bytes = size_of::<RuntimeValue>() as u64;
        self.charge_collection_payload((new_range.length as u64).saturating_mul(value_bytes));
        self.release_collection_payload((old_range.length as u64).saturating_mul(value_bytes));
        Ok(())
    }

    /// One-cell-per-element live view. Flattened struct-row arrays have no
    /// such view and are read through [`Self::array_rows`] instead (WP52).
    pub fn array_values(&self, value: RuntimeValue) -> Result<CollectionView<'_>, HeapError> {
        let parts = self.array_parts(value)?;
        if parts.rows().is_some() {
            return Err(HeapError::InvalidReference(parts.reference));
        }
        self.typed_collection_view(parts.storage, parts.element_type, parts.range)?
            .prefix(parts.length)
            .ok_or(HeapError::IndexOutOfBounds {
                index: parts.length,
                length: parts.range.length,
            })
    }

    /// WP52 borrowed row view: `Some((cells, stride, struct_type))` exposes
    /// the live flattened rows of a struct-element array without
    /// materializing anything; `None` means the plain cell layout.
    pub fn array_rows(&self, value: RuntimeValue) -> Result<Option<ArrayRowsView<'_>>, HeapError> {
        let parts = self.array_parts(value)?;
        let Some(stride) = parts.rows() else {
            return Ok(None);
        };
        let struct_type = parts.element_struct_type()?;
        let live = parts.length.saturating_mul(stride);
        let values = self.collections.values(parts.range)?;
        let cells = values.get(..live).ok_or(HeapError::IndexOutOfBounds {
            index: live,
            length: values.len(),
        })?;
        Ok(Some(ArrayRowsView {
            cells,
            stride,
            struct_type,
        }))
    }

    /// Borrowed field view of one struct element, independent of the array
    /// layout (WP52): flattened rows come straight from the arena, cell
    /// layouts resolve the stored struct value. Read-only tooling and
    /// validation paths use this instead of materializing elements.
    pub fn array_element_fields(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<CollectionView<'_>, HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        if let Some(stride) = parts.rows() {
            let cells = self.collections.values(parts.range)?;
            return Ok(CollectionView::Values(
                &cells[index * stride..(index + 1) * stride],
            ));
        }
        let element = self.collections.values(parts.range)?[index];
        self.struct_fields(element)
    }

    /// WP52 fused projection: one field of one struct element, zero
    /// allocation on both layouts. Backs the `ArrayFieldGet` instruction.
    pub fn array_field_get(
        &self,
        value: RuntimeValue,
        index: usize,
        field: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let fields = self.array_element_fields(value, index)?;
        fields.get(field).ok_or(HeapError::IndexOutOfBounds {
            index: field,
            length: fields.len(),
        })
    }

    /// Copies `element`'s fields into a stack row after checking that it is
    /// a struct value of the array's element type with exactly `stride`
    /// fields (WP52).
    fn struct_row(
        &self,
        expected: StableId,
        stride: usize,
        element: RuntimeValue,
    ) -> Result<[RuntimeValue; nexa_bytecode::MAX_STRUCT_FIELDS], HeapError> {
        let RuntimeValue::Struct { type_id, .. } = element else {
            return Err(invalid_value_reference());
        };
        if type_id != expected {
            return Err(invalid_value_reference());
        }
        let fields = self.struct_fields(element)?;
        if fields.len() != stride {
            return Err(invalid_value_reference());
        }
        let mut row = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        for (destination, field) in row[..stride].iter_mut().zip(fields.iter()) {
            *destination = field;
        }
        Ok(row)
    }

    fn array_parts(&self, value: RuntimeValue) -> Result<ArrayParts, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Array {
                type_id: actual,
                element_type,
                range,
                length,
                row_stride,
                storage,
                ..
            } if *actual == type_id && type_id == nexa_bytecode::array_type(*element_type) => {
                Ok(ArrayParts {
                    reference,
                    range: *range,
                    length: *length,
                    row_stride: *row_stride,
                    element_type: *element_type,
                    storage: *storage,
                })
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
        let mut heap = self.preflight(1)?;
        let storage = collection_storage_for_values(element_type, source)?;
        let range = self.claim_typed_collection(storage, source.len())?;
        for (index, value) in source.iter().copied().enumerate() {
            if let Err(error) =
                self.typed_collection_set(storage, element_type, range, index, value)
            {
                self.release_typed_collection(storage, range);
                return Err(error);
            }
        }
        match self.commit_buffer_reserved(&mut heap, type_id, element_type, storage, range) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.release_typed_collection(storage, range);
                Err(error)
            }
        }
    }

    pub fn buffer_values(&self, value: RuntimeValue) -> Result<CollectionView<'_>, HeapError> {
        let parts = self.buffer_parts(value)?;
        self.typed_collection_view(parts.storage, parts.element_type, parts.range)
    }

    pub fn buffer_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.buffer_parts(value)?.range.length)
    }

    pub fn buffer_get(&self, value: RuntimeValue, index: usize) -> Result<RuntimeValue, HeapError> {
        let parts = self.buffer_parts(value)?;
        self.typed_collection_get(parts.storage, parts.element_type, parts.range, index)
    }

    pub(crate) fn prepare_buffer_get(
        &self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<PreparedBufferGet, HeapError> {
        let parts = self.buffer_parts(value)?;
        if index >= parts.range.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.range.length,
            });
        }
        let absolute_index = parts
            .range
            .start
            .checked_add(index)
            .ok_or_else(invalid_value_reference)?;
        Ok(PreparedBufferGet {
            storage: parts.storage,
            element_type: parts.element_type,
            absolute_index,
        })
    }

    pub(crate) fn execute_prepared_buffer_get(
        &self,
        prepared: PreparedBufferGet,
    ) -> Result<RuntimeValue, HeapError> {
        self.typed_collection_get_absolute(
            prepared.storage,
            prepared.element_type,
            prepared.absolute_index,
        )
    }

    pub fn buffer_set(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let parts = self.buffer_parts(value)?;
        let epoch = self.next_buffer_epoch(parts.reference)?;
        self.typed_collection_set(
            parts.storage,
            parts.element_type,
            parts.range,
            index,
            replacement,
        )?;
        self.commit_buffer_epoch(parts.reference, epoch)
    }

    /// `LANGUAGE_V3` `BufferFill`: overwrites `length` elements starting at
    /// `index` with `replacement`; bounds are validated before any write.
    pub fn buffer_fill(
        &mut self,
        value: RuntimeValue,
        index: usize,
        length: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let parts = self.buffer_parts(value)?;
        let end = checked_collection_end(index, length, parts.range.length)?;
        if length == 0 {
            // A zero-length fill writes nothing and is not a mutation.
            return Ok(());
        }
        let epoch = self.next_buffer_epoch(parts.reference)?;
        for destination in index..end {
            self.typed_collection_set(
                parts.storage,
                parts.element_type,
                parts.range,
                destination,
                replacement,
            )?;
        }
        self.commit_buffer_epoch(parts.reference, epoch)
    }

    pub fn buffer_slice(
        &mut self,
        value: RuntimeValue,
        start: usize,
        length: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let source = self.buffer_parts(value)?;
        let end = checked_collection_end(start, length, source.range.length)?;
        // Reserve the object slot before claiming/copying collection storage,
        // so a full heap cannot strand an otherwise unreachable arena range.
        let mut heap = self.preflight(1)?;
        let range = self.claim_typed_collection(source.storage, length)?;
        for (destination, source_index) in (start..end).enumerate() {
            let item = self.typed_collection_get(
                source.storage,
                source.element_type,
                source.range,
                source_index,
            )?;
            if let Err(error) = self.typed_collection_set(
                source.storage,
                source.element_type,
                range,
                destination,
                item,
            ) {
                self.release_typed_collection(source.storage, range);
                return Err(error);
            }
        }
        match self.commit_buffer_reserved(
            &mut heap,
            source.type_id,
            source.element_type,
            source.storage,
            range,
        ) {
            Ok(value) => Ok(value),
            Err(error) => {
                self.release_typed_collection(source.storage, range);
                Err(error)
            }
        }
    }

    pub fn buffer_copy(
        &mut self,
        destination: RuntimeValue,
        source: RuntimeValue,
        source_start: usize,
        destination_start: usize,
        length: usize,
    ) -> Result<(), HeapError> {
        let prepared =
            self.prepare_buffer_copy(destination, source, source_start, destination_start, length)?;
        self.execute_prepared_buffer_copy(prepared)
    }

    pub(crate) fn prepare_buffer_copy(
        &self,
        destination: RuntimeValue,
        source: RuntimeValue,
        source_start: usize,
        destination_start: usize,
        length: usize,
    ) -> Result<PreparedBufferCopy, HeapError> {
        let destination = self.buffer_parts(destination)?;
        let source = self.buffer_parts(source)?;
        let (source_end, destination_end) = validate_buffer_parts_copy(
            destination,
            source,
            source_start,
            destination_start,
            length,
        )?;
        let source_absolute = source
            .range
            .start
            .checked_add(source_start)
            .ok_or_else(invalid_value_reference)?;
        let destination_absolute = destination
            .range
            .start
            .checked_add(destination_start)
            .ok_or_else(invalid_value_reference)?;
        debug_assert_eq!(source_end - source_start, length);
        debug_assert_eq!(destination_end - destination_start, length);
        Ok(PreparedBufferCopy {
            destination,
            source_absolute,
            destination_absolute,
            destination_start,
            length,
        })
    }

    pub(crate) fn execute_prepared_buffer_copy(
        &mut self,
        prepared: PreparedBufferCopy,
    ) -> Result<(), HeapError> {
        let storage = prepared.destination.storage;
        let element_bytes = storage.cell_size();
        self.counters.collection_relocation_bytes = self
            .counters
            .collection_relocation_bytes
            .saturating_add((prepared.length * element_bytes) as u64);
        let epoch = self.next_buffer_epoch(prepared.destination.reference)?;
        macro_rules! copy_typed {
            ($arena:ident) => {
                self.scalar_collections
                    .$arena()
                    .values_mut(CollectionRange {
                        start: 0,
                        length: prepared
                            .source_absolute
                            .saturating_add(prepared.length)
                            .max(
                                prepared
                                    .destination_absolute
                                    .saturating_add(prepared.length),
                            ),
                    })?
                    .copy_within(
                        prepared.source_absolute..prepared.source_absolute + prepared.length,
                        prepared.destination_absolute,
                    )
            };
        }
        match storage {
            CollectionStorage::I32 => copy_typed!(i32_mut),
            CollectionStorage::I64 => copy_typed!(i64_mut),
            CollectionStorage::F32 => copy_typed!(f32_mut),
            CollectionStorage::F64 => copy_typed!(f64_mut),
            CollectionStorage::Bool => copy_typed!(bools_mut),
            CollectionStorage::Rune => copy_typed!(runes_mut),
            CollectionStorage::String => copy_typed!(strings_mut),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                copy_typed!(refs_mut);
            }
            CollectionStorage::Values => self.collections.values.copy_within(
                prepared.source_absolute..prepared.source_absolute + prepared.length,
                prepared.destination_absolute,
            ),
        }
        // G1 barrier: every reference just published into the destination
        // extent is shaded; the gray queue tolerates duplicates.
        if self.gc_phase == GcPhase::Mark {
            for offset in 0..prepared.length {
                if let Some(value) = self
                    .typed_collection_view(
                        storage,
                        prepared.destination.element_type,
                        prepared.destination.range,
                    )?
                    .get(prepared.destination_start + offset)
                {
                    self.shade_on_write(value);
                }
            }
        }
        self.commit_buffer_epoch(prepared.destination.reference, epoch)
    }

    fn buffer_parts(&self, value: RuntimeValue) -> Result<BufferParts, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                storage,
                range,
                ..
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok(BufferParts {
                    type_id,
                    reference,
                    element_type: *element_type,
                    storage: *storage,
                    range: *range,
                })
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
        self.allocate_physical_map(type_id, key_type, value_type, 1)
    }

    pub(crate) fn allocate_physical_map(
        &mut self,
        type_id: StableId,
        key_type: nexa_bytecode::ValueType,
        value_type: nexa_bytecode::ValueType,
        value_slots: u16,
    ) -> Result<RuntimeValue, HeapError> {
        let value_storage = default_map_value_storage(value_type, value_slots);
        self.allocate_physical_map_with_storage(
            type_id,
            key_type,
            value_type,
            value_slots,
            value_storage,
        )
    }

    pub(crate) fn allocate_physical_map_with_layout(
        &mut self,
        type_id: StableId,
        key_type: nexa_bytecode::ValueType,
        value_type: nexa_bytecode::ValueType,
        value_slots: u16,
        value_layout: &nexa_bytecode::layout::ValueLayout,
    ) -> Result<RuntimeValue, HeapError> {
        if value_layout.logical_type != value_type
            || value_layout.physical_slots != value_slots
            || value_layout.slot_kinds.len() != usize::from(value_slots)
        {
            return Err(invalid_value_reference());
        }
        let value_storage = map_value_storage_for_layout(value_type, value_layout);
        self.allocate_physical_map_with_storage(
            type_id,
            key_type,
            value_type,
            value_slots,
            value_storage,
        )
    }

    fn allocate_physical_map_with_storage(
        &mut self,
        type_id: StableId,
        key_type: nexa_bytecode::ValueType,
        value_type: nexa_bytecode::ValueType,
        value_slots: u16,
        value_storage: CollectionStorage,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::map_type(key_type, value_type) {
            return Err(invalid_value_reference());
        }
        if value_slots == 0 || (value_storage.is_compact() && value_slots != 1) {
            return Err(invalid_value_reference());
        }
        let initial_capacity = self.empty_map_capacity();
        let value_cells = initial_capacity
            .checked_mul(usize::from(value_slots))
            .ok_or(HeapError::CapacityExhausted)?;
        let payload_bytes = (initial_capacity as u64)
            .saturating_mul(size_of::<Option<MapEntry>>() as u64)
            .saturating_add((value_cells as u64).saturating_mul(value_storage.cell_size() as u64));
        self.ensure_new_object_headroom(payload_bytes, true)?;
        self.ensure_collection_headroom(payload_bytes)?;
        let mut reservation = self.preflight(1)?;
        let slots = self.map_slots.claim(initial_capacity)?;
        let values = match self.claim_typed_collection(value_storage, value_cells) {
            Ok(values) => values,
            Err(error) => {
                self.map_slots.release(slots);
                return Err(error);
            }
        };
        let storage = self.claim_map_storage(VmMap {
            type_id,
            key_type,
            value_type,
            value_slots,
            value_storage,
            slots,
            values,
            length: 0,
            rehash: None,
            mutation_epoch: 0,
        });
        let reference = self.commit(&mut reservation, Object::Map { storage });
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    fn claim_map_storage(&mut self, map: VmMap) -> u32 {
        if let Some(index) = self.free_maps.pop() {
            self.maps[index as usize] = Some(map);
            return index;
        }
        let index = u32::try_from(self.maps.len()).expect("map arena is bounded by heap slots");
        debug_assert!(index < self.max_objects);
        debug_assert!(
            self.maps.len() < self.maps.capacity(),
            "map arena capacity is reserved with the heap"
        );
        self.maps.push(Some(map));
        index
    }

    pub fn map_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.map(value)?.length)
    }

    /// Iterates entries in deterministic backing-slot order without allocating
    /// or recomputing hashes. A completed map visits its current table from
    /// lowest to highest slot; an in-progress rehash then visits remaining old
    /// slots followed by populated new slots in the same order.
    pub(crate) fn map_entries(&self, value: RuntimeValue) -> Result<MapEntries<'_>, HeapError> {
        let map = self.map(value)?;
        if map.value_slots != 1 {
            return Err(invalid_value_reference());
        }
        let empty: &[Option<MapEntry>] = &[];
        let (old, new, old_values, new_values) = map.rehash.as_ref().map_or(
            (
                empty,
                empty,
                CollectionRange::default(),
                CollectionRange::default(),
            ),
            |rehash| {
                (
                    self.map_slots.slots(rehash.old_slots),
                    self.map_slots.slots(rehash.new_slots),
                    rehash.old_values,
                    rehash.new_values,
                )
            },
        );
        Ok(MapEntries {
            current: self.map_slots.slots(map.slots),
            old,
            new,
            values: &self.collections,
            scalar_values: &self.scalar_collections,
            current_values: map.values,
            old_values,
            new_values,
            value_type: map.value_type,
            value_slots: map.value_slots,
            value_storage: map.value_storage,
            phase: 0,
            index: 0,
            remaining: map.length,
        })
    }

    pub(crate) fn map_fuel_shape(&self, value: RuntimeValue) -> Result<MapFuelShape, HeapError> {
        const REHASH_CHUNK: usize = 8;
        let map = self.map(value)?;
        let (old_slots, new_slots, rehash_remaining) =
            map.rehash.as_ref().map_or((0, 0, 0), |rehash| {
                (
                    rehash.old_slots.length,
                    rehash.new_slots.length,
                    rehash
                        .old_slots
                        .length
                        .saturating_sub(rehash.cursor)
                        .min(REHASH_CHUNK),
                )
            });
        let next_rehash_slots = if map.rehash.is_none() {
            next_map_capacity(map, self.max_collection_length)
                .filter(|capacity| *capacity > map.slots.length)
                .unwrap_or(0)
        } else {
            0
        };
        Ok(MapFuelShape {
            current_slots: map.slots.length,
            old_slots,
            new_slots,
            rehash_remaining,
            next_rehash_slots,
        })
    }

    #[must_use]
    pub(crate) const fn empty_map_capacity(&self) -> usize {
        if self.max_collection_length < 8 {
            self.max_collection_length
        } else {
            8
        }
    }

    pub(crate) fn map_key_fuel_shape(
        &self,
        key: RuntimeValue,
    ) -> Result<MapKeyFuelShape, HeapError> {
        let shape = match key {
            RuntimeValue::String { reference, .. } => MapKeyFuelShape {
                string_bytes: self.string(reference)?.len(),
                string_objects: 1,
                structural_objects: 0,
                fields_per_object: 0,
                hash_structural_objects: 0,
            },
            RuntimeValue::Struct { .. } => MapKeyFuelShape {
                string_bytes: self.max_string_bytes,
                string_objects: self.max_objects as usize,
                structural_objects: self.max_objects as usize,
                fields_per_object: nexa_bytecode::MAX_STRUCT_FIELDS,
                hash_structural_objects: 0,
            },
            RuntimeValue::NamedRef { reference, .. }
                if matches!(self.resolve(reference)?, Object::Enum { .. }) =>
            {
                MapKeyFuelShape {
                    string_bytes: self.max_string_bytes,
                    string_objects: self.max_objects as usize,
                    structural_objects: self.max_objects as usize,
                    fields_per_object: nexa_bytecode::MAX_STRUCT_FIELDS,
                    hash_structural_objects: self.max_objects as usize,
                }
            }
            _ => MapKeyFuelShape {
                string_bytes: 0,
                string_objects: 0,
                structural_objects: 0,
                fields_per_object: 0,
                hash_structural_objects: 0,
            },
        };
        Ok(shape)
    }

    pub fn map_get(
        &self,
        value: RuntimeValue,
        key: RuntimeValue,
    ) -> Result<Option<RuntimeValue>, HeapError> {
        let mut result = [RuntimeValue::Unit];
        Ok(self
            .map_get_value_into(value, key, &mut result)?
            .then_some(result[0]))
    }

    pub(crate) fn map_get_value_into(
        &self,
        value: RuntimeValue,
        key: RuntimeValue,
        destination: &mut [RuntimeValue],
    ) -> Result<bool, HeapError> {
        let Some(value) = self.map_get_value_range(value, key)? else {
            return Ok(false);
        };
        if destination.len() != value.len() {
            return Err(invalid_value_reference());
        }
        for (destination, value) in destination.iter_mut().zip(value.iter()) {
            *destination = value;
        }
        Ok(true)
    }

    pub(crate) fn map_get_value_range(
        &self,
        value: RuntimeValue,
        key: RuntimeValue,
    ) -> Result<Option<CollectionView<'_>>, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let map = self.map(value)?;
        let Some(location) = self.find_map_entry(map, key, hash)? else {
            return Ok(None);
        };
        let range = map_entry_value_range(map, location)?;
        Ok(Some(self.typed_collection_view(
            map.value_storage,
            map.value_type,
            range,
        )?))
    }

    pub fn map_contains(&self, value: RuntimeValue, key: RuntimeValue) -> Result<bool, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let map = self.map(value)?;
        Ok(self.find_map_entry(map, key, hash)?.is_some())
    }

    pub fn map_set(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
        replacement: RuntimeValue,
    ) -> Result<MapSetOutcome, HeapError> {
        self.map_set_value_range(value, key, std::slice::from_ref(&replacement))
    }

    fn write_map_value_range(
        &mut self,
        storage: CollectionStorage,
        value_type: nexa_bytecode::ValueType,
        range: CollectionRange,
        replacement: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        if replacement.len() != range.length {
            return Err(invalid_value_reference());
        }
        if storage == CollectionStorage::Values {
            self.collections
                .values_mut(range)?
                .copy_from_slice(replacement);
            return Ok(());
        }
        if replacement.len() != 1 {
            return Err(invalid_value_reference());
        }
        self.typed_collection_set(storage, value_type, range, 0, replacement[0])
    }

    fn begin_map_rehash(&mut self, storage: usize, new_capacity: usize) -> Result<(), HeapError> {
        // `LANGUAGE_V3` 4.3 epoch discipline: reserve the next epoch first
        // (read-only), so an exhausted epoch traps before any claim,
        // charge, or header mutation; the reserved value is committed only
        // after the fallible claims succeeded and the switch completed.
        let epoch = self.next_map_epoch(storage)?;
        let map = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists");
        let value_cells = new_capacity
            .checked_mul(usize::from(map.value_slots))
            .ok_or(HeapError::CapacityExhausted)?;
        let value_storage = map.value_storage;
        let entry_bytes = size_of::<Option<MapEntry>>() as u64;
        let new_bytes = (new_capacity as u64)
            .saturating_mul(entry_bytes)
            .saturating_add((value_cells as u64).saturating_mul(value_storage.cell_size() as u64));
        self.ensure_payload_headroom(new_bytes)?;
        self.ensure_collection_headroom(new_bytes)?;

        let new_slots = self.map_slots.claim(new_capacity)?;
        let new_values = match self.claim_typed_collection(value_storage, value_cells) {
            Ok(values) => values,
            Err(error) => {
                self.map_slots.release(new_slots);
                return Err(error);
            }
        };
        self.charge_collection_payload(new_bytes);
        self.counters.map_slot_allocations = self
            .counters
            .map_slot_allocations
            .saturating_add(new_capacity as u64);

        let map = self.maps[storage]
            .as_mut()
            .expect("validated map storage exists");
        let old_slots = map.slots;
        let old_values = map.values;
        map.slots = CollectionRange::default();
        map.values = CollectionRange::default();
        map.rehash = Some(MapRehash {
            old_slots,
            new_slots,
            old_values,
            new_values,
            cursor: 0,
        });
        // Switching to an in-flight rehash is itself an observable
        // structural mutation (an old iterator must trap even while the
        // incremental rehash is paused between chunks).
        self.commit_map_epoch(storage, epoch);
        Ok(())
    }

    fn begin_set_rehash(&mut self, storage: usize, new_capacity: usize) -> Result<(), HeapError> {
        // `LANGUAGE_V3` 4.3 epoch discipline: reserve the next epoch first
        // (read-only) so an exhausted epoch traps before any claim,
        // charge, or header mutation; commit only after the switch.
        let epoch = self.next_set_epoch(storage)?;
        let entry_bytes = size_of::<Option<MapEntry>>() as u64;
        let new_bytes = (new_capacity as u64).saturating_mul(entry_bytes);
        self.ensure_payload_headroom(new_bytes)?;
        self.ensure_collection_headroom(new_bytes)?;

        let new_slots = self.map_slots.claim(new_capacity)?;
        self.charge_collection_payload(new_bytes);
        self.counters.map_slot_allocations = self
            .counters
            .map_slot_allocations
            .saturating_add(new_capacity as u64);

        let set = self.sets[storage]
            .as_mut()
            .expect("validated set storage exists");
        let old_slots = set.slots;
        set.slots = CollectionRange::default();
        set.rehash = Some(SetRehash {
            old_slots,
            new_slots,
            cursor: 0,
        });
        // Entering an in-flight rehash is an observable structural
        // mutation; a paused rehash still traps old iterators.
        self.commit_set_epoch(storage, epoch);
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn map_set_value_range(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
        replacement: &[RuntimeValue],
    ) -> Result<MapSetOutcome, HeapError> {
        // Resolve and validate the public map handle once. The previous
        // implementation repeatedly walked heap slot -> map arena for every
        // branch below, even though neither identity can change during one
        // synchronous insertion.
        let storage = self.map_storage_index(value)?;
        if replacement.len()
            != usize::from(
                self.maps[storage]
                    .as_ref()
                    .expect("validated map storage exists")
                    .value_slots,
            )
        {
            return Err(invalid_value_reference());
        }
        // A retry resumes only the bounded rehash chunk. Looking up the key
        // again here would repeat an entire map scan on every retry and make
        // deterministic attempt-based fuel either free or overcharged.
        if self.maps[storage]
            .as_ref()
            .expect("validated map storage exists")
            .rehash
            .is_some()
        {
            let value_storage = self.maps[storage]
                .as_ref()
                .expect("validated map storage exists")
                .value_storage;
            // `LANGUAGE_V3` 4.3: every rehash step changes the phase
            // topology a mid-rehash iterator walks - the cursor advances
            // and the final step switches back to a single current table -
            // so the epoch advances unconditionally after each progress
            // step, even when the chunk contained no entries. Reserve
            // before the step, commit after it succeeded.
            let epoch = self.next_map_epoch(storage)?;
            let (_migrated, released) = progress_map_rehash(
                self.maps[storage]
                    .as_mut()
                    .expect("validated map storage exists"),
                &mut self.map_slots,
                &mut self.collections,
                &mut self.scalar_collections,
            )?;
            self.commit_map_epoch(storage, epoch);
            if let Some((old_values, released_bytes)) = released {
                self.release_typed_collection(value_storage, old_values);
                // G6: rehash completion released the old slot/value table.
                self.release_collection_payload(released_bytes);
            }
            return Ok(MapSetOutcome::RehashPending);
        }

        let hash = self.runtime_value_hash(key)?;
        let location = {
            let map = self.maps[storage]
                .as_ref()
                .expect("validated map storage exists");
            self.find_map_entry(map, key, hash)?
        };
        if let Some(location) = location {
            // `LANGUAGE_V3` 4.3: an existing-key MapInsert overwrites the
            // value and is an observable write, so the epoch advances
            // exactly like a new-key insert - reserved before the write,
            // committed after success, so an exhausted epoch traps without
            // mutating the map.
            let epoch = self.next_map_epoch(storage)?;
            self.shade_on_write(key);
            for replacement in replacement {
                self.shade_on_write(*replacement);
            }
            let map = self.maps[storage]
                .as_ref()
                .expect("validated map storage exists");
            let value_range = map_entry_value_range(map, location)?;
            let value_storage = map.value_storage;
            let value_type = map.value_type;
            self.write_map_value_range(value_storage, value_type, value_range, replacement)?;
            self.commit_map_epoch(storage, epoch);
            return Ok(MapSetOutcome::Complete);
        }

        let map = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists");
        if map.length >= self.max_collection_length {
            return Err(HeapError::CollectionTooLarge {
                length: map.length.saturating_add(1),
                max_length: self.max_collection_length,
            });
        }
        if map_needs_rehash(map) {
            let old_capacity = map.slots.length;
            let new_capacity =
                next_map_capacity(map, self.max_collection_length).expect("map needs rehash");
            if new_capacity > old_capacity {
                // G6 admission and charge: the new slot vector joins the
                // map's footprint with its companion value rows. Both old
                // extents are released when the bounded rehash completes.
                self.begin_map_rehash(storage, new_capacity)?;
                return Ok(MapSetOutcome::RehashPending);
            }
        }

        let entry = MapEntry { key, hash };
        self.counters.map_slot_allocations = self.counters.map_slot_allocations.saturating_add(1);
        let epoch = self.next_map_epoch(storage)?;
        // The insertion barrier runs only on the actual publication path,
        // after every fallible check and the epoch preflight.
        self.shade_on_write(key);
        for replacement in replacement {
            self.shade_on_write(*replacement);
        }
        let map = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists");
        let range = map.slots;
        let values = map.values;
        let value_slots = map.value_slots;
        let value_storage = map.value_storage;
        let value_type = map.value_type;
        let index = insert_map_entry(self.map_slots.slots_mut(range), entry)?;
        let value_range =
            map_value_row(values, value_slots, index).ok_or_else(invalid_value_reference)?;
        self.write_map_value_range(value_storage, value_type, value_range, replacement)?;
        self.maps[storage]
            .as_mut()
            .expect("validated map storage exists")
            .length += 1;
        self.commit_map_epoch(storage, epoch);
        Ok(MapSetOutcome::Complete)
    }

    pub fn map_remove(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
    ) -> Result<Option<RuntimeValue>, HeapError> {
        let mut result = [RuntimeValue::Unit];
        Ok(self
            .map_remove_value_into(value, key, &mut result)?
            .then_some(result[0]))
    }

    pub(crate) fn map_remove_value_into(
        &mut self,
        value: RuntimeValue,
        key: RuntimeValue,
        destination: &mut [RuntimeValue],
    ) -> Result<bool, HeapError> {
        let hash = self.runtime_value_hash(key)?;
        let location = {
            let map = self.map(value)?;
            if destination.len() != usize::from(map.value_slots) {
                return Err(invalid_value_reference());
            }
            self.find_map_entry(map, key, hash)?
        };
        let Some(location) = location else {
            return Ok(false);
        };
        let storage = self.map_storage_index(value)?;
        // `LANGUAGE_V3` 4.3: successful removal is observable; the epoch is
        // reserved before the entry disappears and committed after success.
        let epoch = self.next_map_epoch(storage)?;
        let map = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists");
        let range = map_location_range(map, location);
        let values = map_location_values(map, location);
        let value_slots = map.value_slots;
        let value_storage = map.value_storage;
        let value_type = map.value_type;
        let value_range = map_entry_value_range(map, location)?;
        let view = self.typed_collection_view(value_storage, value_type, value_range)?;
        for (destination, value) in destination.iter_mut().zip(view.iter()) {
            *destination = value;
        }
        match location {
            MapLocation::RehashOld(_) => {
                self.map_slots.slots_mut(range)[map_location_index(location)]
                    .take()
                    .expect("located map entry exists");
                self.typed_collection_clear(value_storage, value_range, 0..value_range.length)?;
            }
            MapLocation::Current(_) | MapLocation::RehashNew(_) => {
                remove_probed_entry_with_values(
                    self.map_slots.slots_mut(range),
                    &mut self.collections,
                    &mut self.scalar_collections,
                    value_storage,
                    values,
                    value_slots,
                    map_location_index(location),
                );
            }
        }
        self.maps[storage]
            .as_mut()
            .expect("validated map storage exists")
            .length -= 1;
        self.commit_map_epoch(storage, epoch);
        Ok(true)
    }

    pub fn map_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let storage = self.map_storage_index(value)?;
        let snapshot = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists")
            .clone();
        let nonempty = snapshot.length != 0 || snapshot.rehash.is_some();
        let epoch = if nonempty {
            Some(self.next_map_epoch(storage)?)
        } else {
            None
        };
        if let Some(rehash) = snapshot.rehash {
            // G6: an in-flight map has no primary table. Clear releases both
            // sides and lets the next insertion choose a fresh capacity.
            self.map_slots.release(rehash.old_slots);
            self.map_slots.release(rehash.new_slots);
            self.release_typed_collection(snapshot.value_storage, rehash.old_values);
            self.release_typed_collection(snapshot.value_storage, rehash.new_values);
            let bytes = (rehash
                .old_slots
                .length
                .saturating_add(rehash.new_slots.length) as u64)
                .saturating_mul(size_of::<Option<MapEntry>>() as u64)
                .saturating_add(
                    (rehash
                        .old_values
                        .length
                        .saturating_add(rehash.new_values.length) as u64)
                        .saturating_mul(snapshot.value_storage.cell_size() as u64),
                );
            let map = self.maps[storage]
                .as_mut()
                .expect("validated map storage exists");
            map.slots = CollectionRange::default();
            map.values = CollectionRange::default();
            map.rehash = None;
            map.length = 0;
            self.release_collection_payload(bytes);
        } else {
            self.map_slots.slots_mut(snapshot.slots).fill(None);
            self.typed_collection_clear(
                snapshot.value_storage,
                snapshot.values,
                0..snapshot.values.length,
            )?;
            self.maps[storage]
                .as_mut()
                .expect("validated map storage exists")
                .length = 0;
        }
        if let Some(epoch) = epoch {
            self.commit_map_epoch(storage, epoch);
        }
        Ok(())
    }

    fn next_map_epoch(&self, storage: usize) -> Result<u64, HeapError> {
        self.maps[storage]
            .as_ref()
            .expect("validated map storage exists")
            .mutation_epoch
            .checked_add(1)
            .ok_or(HeapError::MutationEpochExhausted)
    }

    fn commit_map_epoch(&mut self, storage: usize, epoch: u64) {
        self.maps[storage]
            .as_mut()
            .expect("validated map storage exists")
            .mutation_epoch = epoch;
    }

    fn next_set_epoch(&self, storage: usize) -> Result<u64, HeapError> {
        self.sets[storage]
            .as_ref()
            .expect("validated set storage exists")
            .mutation_epoch
            .checked_add(1)
            .ok_or(HeapError::MutationEpochExhausted)
    }

    fn commit_set_epoch(&mut self, storage: usize, epoch: u64) {
        self.sets[storage]
            .as_mut()
            .expect("validated set storage exists")
            .mutation_epoch = epoch;
    }

    /// `LANGUAGE_V3` 4.3: live mutation epoch of a map, snapshot by
    /// `IterNew` and revalidated by every `IterNext`.
    pub fn map_mutation_epoch(&self, value: RuntimeValue) -> Result<u64, HeapError> {
        Ok(self.map(value)?.mutation_epoch)
    }

    /// `LANGUAGE_V3` 4.3: live mutation epoch of a set.
    pub fn set_mutation_epoch(&self, value: RuntimeValue) -> Result<u64, HeapError> {
        Ok(self.set(value)?.mutation_epoch)
    }

    /// `LANGUAGE_V3` 4.3: live mutation epoch of an array.
    pub fn array_mutation_epoch(&self, value: RuntimeValue) -> Result<u64, HeapError> {
        let RuntimeValue::NamedRef { reference, .. } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Array { mutation_epoch, .. } => Ok(*mutation_epoch),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// `LANGUAGE_V3` 4.3: live mutation epoch of a buffer.
    pub fn buffer_mutation_epoch(&self, value: RuntimeValue) -> Result<u64, HeapError> {
        let RuntimeValue::NamedRef { reference, .. } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer { mutation_epoch, .. } => Ok(*mutation_epoch),
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// Allocates an empty `Set<T>` with the canonical set type identity.
    pub fn allocate_set(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::set_type(element_type) {
            return Err(invalid_value_reference());
        }
        let initial_capacity = self.empty_map_capacity();
        let payload_bytes =
            (initial_capacity as u64).saturating_mul(size_of::<Option<MapEntry>>() as u64);
        self.ensure_new_set_headroom(payload_bytes)?;
        self.ensure_collection_headroom(payload_bytes)?;
        let mut reservation = self.preflight(1)?;
        let slots = self.map_slots.claim(initial_capacity)?;
        let storage = self.claim_set_storage(VmSet {
            type_id,
            element_type,
            slots,
            length: 0,
            rehash: None,
            mutation_epoch: 0,
        });
        let reference = self.commit(&mut reservation, Object::Set { storage });
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    fn claim_set_storage(&mut self, set: VmSet) -> u32 {
        if let Some(index) = self.free_sets.pop() {
            self.sets[index as usize] = Some(set);
            return index;
        }
        let index = u32::try_from(self.sets.len()).expect("set arena is bounded by heap slots");
        debug_assert!(index < self.max_objects);
        debug_assert!(
            self.sets.len() < self.sets.capacity(),
            "set arena capacity is reserved with the heap"
        );
        self.sets.push(Some(set));
        index
    }

    pub fn set_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.set(value)?.length)
    }

    pub fn set_contains(
        &self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<bool, HeapError> {
        let hash = self.runtime_value_hash(element)?;
        let set = self.set(value)?;
        Ok(self.find_set_entry(set, element, hash)?.is_some())
    }

    /// Attempts one insertion; `RehashPending` retries without re-hashing
    /// (the same deterministic attempt contract as `MapSet`).
    pub fn set_insert(
        &mut self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<SetInsertOutcome, HeapError> {
        let storage = self.set_storage_index(value)?;
        if self.sets[storage]
            .as_ref()
            .expect("validated set storage exists")
            .rehash
            .is_some()
        {
            // `LANGUAGE_V3` 4.3: every rehash step changes the phase
            // topology a mid-rehash iterator walks, so the epoch advances
            // unconditionally after each progress step. Reserve before
            // the step, commit after it succeeded.
            let epoch = self.next_set_epoch(storage)?;
            let (_migrated, released) = progress_set_rehash(
                self.sets[storage]
                    .as_mut()
                    .expect("validated set storage exists"),
                &mut self.map_slots,
            )?;
            self.commit_set_epoch(storage, epoch);
            if let Some(released_bytes) = released {
                self.release_collection_payload(released_bytes);
            }
            return Ok(SetInsertOutcome::RehashPending);
        }

        let hash = self.runtime_value_hash(element)?;
        let location = {
            let set = self.sets[storage]
                .as_ref()
                .expect("validated set storage exists");
            self.find_set_entry(set, element, hash)?
        };
        if location.is_some() {
            // A duplicate insert leaves the set unchanged: no epoch bump.
            return Ok(SetInsertOutcome::Complete(false));
        }

        let set = self.sets[storage]
            .as_ref()
            .expect("validated set storage exists");
        if set.length >= self.max_collection_length {
            return Err(HeapError::CollectionTooLarge {
                length: set.length.saturating_add(1),
                max_length: self.max_collection_length,
            });
        }
        if table_needs_rehash(set.slots.length, set.length) {
            let old_capacity = set.slots.length;
            let new_capacity =
                next_table_capacity(set.slots.length, set.length, self.max_collection_length)
                    .expect("set needs rehash");
            if new_capacity > old_capacity {
                self.begin_set_rehash(storage, new_capacity)?;
                return Ok(SetInsertOutcome::RehashPending);
            }
        }

        let entry = MapEntry { key: element, hash };
        self.counters.map_slot_allocations = self.counters.map_slot_allocations.saturating_add(1);
        let epoch = self.next_set_epoch(storage)?;
        // The insertion barrier runs only on the actual publication path,
        // after every fallible check and the epoch preflight.
        self.shade_on_write(element);
        let set = self.sets[storage]
            .as_ref()
            .expect("validated set storage exists");
        let range = set.slots;
        insert_map_entry(self.map_slots.slots_mut(range), entry)?;
        self.sets[storage]
            .as_mut()
            .expect("validated set storage exists")
            .length += 1;
        self.commit_set_epoch(storage, epoch);
        Ok(SetInsertOutcome::Complete(true))
    }

    /// Removes one element; returns whether it was present. A successful
    /// removal is an observable structural mutation and advances the epoch.
    pub fn set_remove(
        &mut self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<bool, HeapError> {
        let hash = self.runtime_value_hash(element)?;
        let location = {
            let set = self.set(value)?;
            self.find_set_entry(set, element, hash)?
        };
        let Some(location) = location else {
            return Ok(false);
        };
        let storage = self.set_storage_index(value)?;
        // `LANGUAGE_V3` 4.3: successful removal is observable; the epoch is
        // reserved before the entry disappears and committed after success.
        let epoch = self.next_set_epoch(storage)?;
        let set = self.sets[storage]
            .as_ref()
            .expect("validated set storage exists");
        let range = set_location_range(set, location);
        match location {
            SetLocation::RehashOld(_) => {
                self.map_slots.slots_mut(range)[set_location_index(location)]
                    .take()
                    .expect("located set entry exists");
            }
            SetLocation::Current(_) | SetLocation::RehashNew(_) => {
                remove_probed_entry_with_moves(
                    self.map_slots.slots_mut(range),
                    set_location_index(location),
                    |_, _| {},
                );
            }
        }
        self.sets[storage]
            .as_mut()
            .expect("validated set storage exists")
            .length -= 1;
        self.commit_set_epoch(storage, epoch);
        Ok(true)
    }

    pub fn set_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let storage = self.set_storage_index(value)?;
        let snapshot = self.sets[storage]
            .as_ref()
            .expect("validated set storage exists")
            .clone();
        let nonempty = snapshot.length != 0 || snapshot.rehash.is_some();
        let epoch = if nonempty {
            Some(self.next_set_epoch(storage)?)
        } else {
            None
        };
        if let Some(rehash) = snapshot.rehash {
            self.map_slots.release(rehash.old_slots);
            self.map_slots.release(rehash.new_slots);
            let bytes = (rehash
                .old_slots
                .length
                .saturating_add(rehash.new_slots.length) as u64)
                .saturating_mul(size_of::<Option<MapEntry>>() as u64);
            let set = self.sets[storage]
                .as_mut()
                .expect("validated set storage exists");
            set.slots = CollectionRange::default();
            set.rehash = None;
            set.length = 0;
            self.release_collection_payload(bytes);
        } else {
            self.map_slots.slots_mut(snapshot.slots).fill(None);
            self.sets[storage]
                .as_mut()
                .expect("validated set storage exists")
                .length = 0;
        }
        if let Some(epoch) = epoch {
            self.commit_set_epoch(storage, epoch);
        }
        Ok(())
    }

    pub(crate) fn set_fuel_shape(&self, value: RuntimeValue) -> Result<SetFuelShape, HeapError> {
        const REHASH_CHUNK: usize = 8;
        let set = self.set(value)?;
        let (old_slots, new_slots, rehash_remaining) =
            set.rehash.as_ref().map_or((0, 0, 0), |rehash| {
                (
                    rehash.old_slots.length,
                    rehash.new_slots.length,
                    rehash
                        .old_slots
                        .length
                        .saturating_sub(rehash.cursor)
                        .min(REHASH_CHUNK),
                )
            });
        let next_rehash_slots = if set.rehash.is_none() {
            next_table_capacity(set.slots.length, set.length, self.max_collection_length)
                .filter(|capacity| *capacity > set.slots.length)
                .unwrap_or(0)
        } else {
            0
        };
        Ok(SetFuelShape {
            current_slots: set.slots.length,
            old_slots,
            new_slots,
            rehash_remaining,
            next_rehash_slots,
        })
    }

    /// Iterates set elements in deterministic backing-slot order without
    /// allocating or recomputing hashes (current, then old, then new sides
    /// of an in-flight rehash, lowest slot first).
    #[cfg(test)]
    pub(crate) fn set_entries(&self, value: RuntimeValue) -> Result<SetEntries<'_>, HeapError> {
        let set = self.set(value)?;
        let empty: &[Option<MapEntry>] = &[];
        let (old, new) = set.rehash.as_ref().map_or((empty, empty), |rehash| {
            (
                self.map_slots.slots(rehash.old_slots),
                self.map_slots.slots(rehash.new_slots),
            )
        });
        Ok(SetEntries {
            current: self.map_slots.slots(set.slots),
            old,
            new,
            phase: 0,
            index: 0,
            remaining: set.length,
        })
    }

    /// Advances a Map iteration cursor; returns the next key and the
    /// cursor position *after* the yielded entry. The value row is copied
    /// into `destination` (exactly `value_slots` cells). `phase`/`slot`
    /// follow the `IteratorStateRegisters` wire contract (0=current,
    /// 1=rehash-old, 2=rehash-new).
    pub(crate) fn map_iter_advance_into(
        &self,
        value: RuntimeValue,
        phase: u8,
        slot: usize,
        destination: &mut [RuntimeValue],
    ) -> Result<Option<(u8, usize, RuntimeValue)>, HeapError> {
        let map = self.map(value)?;
        if destination.len() != usize::from(map.value_slots) {
            return Err(invalid_value_reference());
        }
        let Some((phase, slot, entry)) = self.advance_slot_cursor(map, phase, slot)? else {
            return Ok(None);
        };
        let location = map_slot_location(phase, slot - 1);
        let value_range = map_entry_value_range(map, location)?;
        let view = self.typed_collection_view(map.value_storage, map.value_type, value_range)?;
        for (destination, value) in destination.iter_mut().zip(view.iter()) {
            *destination = value;
        }
        Ok(Some((phase, slot, entry.key)))
    }

    /// Advances a Set iteration cursor; returns the next element and the
    /// cursor position after the yielded entry.
    pub(crate) fn set_iter_advance(
        &self,
        value: RuntimeValue,
        phase: u8,
        slot: usize,
    ) -> Result<Option<(u8, usize, RuntimeValue)>, HeapError> {
        let set = self.set(value)?;
        let Some((phase, slot, entry)) = self.advance_slot_cursor(set, phase, slot)? else {
            return Ok(None);
        };
        Ok(Some((phase, slot, entry.key)))
    }

    fn advance_slot_cursor(
        &self,
        table: &impl SlotTable,
        phase: u8,
        slot: usize,
    ) -> Result<Option<(u8, usize, MapEntry)>, HeapError> {
        let mut phase = phase;
        let mut slot = slot;
        loop {
            let Some(entries) = table.slots_for(self, phase) else {
                return Ok(None);
            };
            if slot >= entries.len() {
                phase += 1;
                slot = 0;
                continue;
            }
            let entry = entries[slot];
            slot += 1;
            if let Some(entry) = entry {
                return Ok(Some((phase, slot, entry)));
            }
        }
    }

    fn set_storage_index(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        let Object::Set { storage } = self.resolve(reference)? else {
            return Err(HeapError::InvalidReference(reference));
        };
        let storage = *storage as usize;
        let set = self
            .sets
            .get(storage)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference(reference))?;
        if set.type_id == type_id && type_id == nexa_bytecode::set_type(set.element_type) {
            Ok(storage)
        } else {
            Err(HeapError::InvalidReference(reference))
        }
    }

    fn set(&self, value: RuntimeValue) -> Result<&VmSet, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        let Object::Set { storage } = self.resolve(reference)? else {
            return Err(HeapError::InvalidReference(reference));
        };
        let set = self
            .sets
            .get(*storage as usize)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference(reference))?;
        if set.type_id == type_id && type_id == nexa_bytecode::set_type(set.element_type) {
            Ok(set)
        } else {
            Err(HeapError::InvalidReference(reference))
        }
    }

    fn find_set_entry(
        &self,
        set: &VmSet,
        element: RuntimeValue,
        hash: u64,
    ) -> Result<Option<SetLocation>, HeapError> {
        if let Some(index) = self.probe_map_slots(self.map_slots.slots(set.slots), element, hash)? {
            return Ok(Some(SetLocation::Current(index)));
        }
        if let Some(rehash) = &set.rehash {
            if let Some(index) =
                self.probe_map_slots(self.map_slots.slots(rehash.new_slots), element, hash)?
            {
                return Ok(Some(SetLocation::RehashNew(index)));
            }
            for (offset, entry) in self.map_slots.slots(rehash.old_slots)[rehash.cursor..]
                .iter()
                .enumerate()
            {
                if entry.is_some_and(|entry| entry.hash == hash)
                    && self.runtime_value_equal(entry.expect("checked entry").key, element)?
                {
                    return Ok(Some(SetLocation::RehashOld(rehash.cursor + offset)));
                }
            }
        }
        Ok(None)
    }

    fn map_storage_index(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        let Object::Map { storage } = self.resolve(reference)? else {
            return Err(HeapError::InvalidReference(reference));
        };
        let storage = *storage as usize;
        let map = self
            .maps
            .get(storage)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference(reference))?;
        if map.type_id == type_id
            && type_id == nexa_bytecode::map_type(map.key_type, map.value_type)
        {
            Ok(storage)
        } else {
            Err(HeapError::InvalidReference(reference))
        }
    }

    fn map(&self, value: RuntimeValue) -> Result<&VmMap, HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        let Object::Map { storage } = self.resolve(reference)? else {
            return Err(HeapError::InvalidReference(reference));
        };
        let map = self
            .maps
            .get(*storage as usize)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference(reference))?;
        if map.type_id == type_id
            && type_id == nexa_bytecode::map_type(map.key_type, map.value_type)
        {
            Ok(map)
        } else {
            Err(HeapError::InvalidReference(reference))
        }
    }

    /// K3: lookups follow the same linear probe chain the insert side
    /// writes, so a hit or miss costs the probe distance instead of a
    /// full-capacity scan. The primary and rehash-new sides keep the
    /// probe invariant through backshift deletion; the rehash-old side
    /// has migration holes anywhere, so its un-migrated suffix is the
    /// only part that is scanned linearly.
    fn find_map_entry(
        &self,
        map: &VmMap,
        key: RuntimeValue,
        hash: u64,
    ) -> Result<Option<MapLocation>, HeapError> {
        if let Some(index) = self.probe_map_slots(self.map_slots.slots(map.slots), key, hash)? {
            return Ok(Some(MapLocation::Current(index)));
        }
        if let Some(rehash) = &map.rehash {
            if let Some(index) =
                self.probe_map_slots(self.map_slots.slots(rehash.new_slots), key, hash)?
            {
                return Ok(Some(MapLocation::RehashNew(index)));
            }
            for (offset, entry) in self.map_slots.slots(rehash.old_slots)[rehash.cursor..]
                .iter()
                .enumerate()
            {
                if entry.is_some_and(|entry| entry.hash == hash)
                    && self.runtime_value_equal(entry.expect("checked entry").key, key)?
                {
                    return Ok(Some(MapLocation::RehashOld(rehash.cursor + offset)));
                }
            }
        }
        Ok(None)
    }

    /// One probe-chain walk: starts at the key's home slot and stops at
    /// the first empty slot (backshift deletion guarantees no holes
    /// inside a chain) or after a full cycle (tables pinned at the
    /// capacity ceiling can run without an empty slot).
    fn probe_map_slots(
        &self,
        slots: &[Option<MapEntry>],
        key: RuntimeValue,
        hash: u64,
    ) -> Result<Option<usize>, HeapError> {
        if slots.is_empty() {
            return Ok(None);
        }
        let start =
            usize::try_from(hash % slots.len() as u64).expect("hash modulo slot count fits usize");
        for offset in 0..slots.len() {
            let index = (start + offset) % slots.len();
            let Some(entry) = slots[index] else {
                return Ok(None);
            };
            if entry.hash == hash && self.runtime_value_equal(entry.key, key)? {
                return Ok(Some(index));
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

    #[allow(clippy::too_many_lines)]
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
                let value = if value.trailing_zeros() >= 31 {
                    0
                } else {
                    value
                };
                write_hash(&mut hash, &value.to_le_bytes());
            }
            RuntimeValue::F64(value) => {
                write_hash(&mut hash, &[4]);
                let value = if value.trailing_zeros() >= 63 {
                    0
                } else {
                    value
                };
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
                write_hash(&mut hash, &value.content_type().0.to_le_bytes());
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
            RuntimeValue::MigrationOldObject(object) => {
                write_migration_object_hash(&mut hash, 16, object.parts());
            }
            RuntimeValue::MigrationStagingObject(object) => {
                write_migration_object_hash(&mut hash, 17, object.parts());
            }
        }
        Ok(hash)
    }

    #[allow(clippy::float_cmp)]
    pub(crate) fn runtime_value_equal(
        &self,
        lhs: RuntimeValue,
        rhs: RuntimeValue,
    ) -> Result<bool, HeapError> {
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
                    (Object::Enum { .. }, Object::Enum { .. }) => self.enum_equal(lhs, rhs)?,
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

    /// Mark phase (WP73/WP74): seeds the reusable queue with the validated
    /// roots and drains it breadth-first via [`Self::mark_step`]; child
    /// references stream straight into the queue, so no per-object
    /// temporary `Vec` is materialized.
    fn mark_reachable(
        &mut self,
        roots: &GcRoots,
        queue: &mut VecDeque<GcRef>,
    ) -> Result<usize, HeapError> {
        let mut marked = 0;
        for root in roots.iter() {
            self.validate_reference(root)?;
            if Self::enqueue_gray(&mut self.slots, queue, root) {
                marked += 1;
            }
        }
        let mut budget = StepBudget::new(GcBudget::objects(usize::MAX));
        marked += self.mark_step(queue, &mut budget)?;
        Ok(marked)
    }

    pub fn collect(&mut self, roots: &GcRoots) -> Result<CollectionStats, HeapError> {
        // Explicit full collection cancels any in-flight incremental cycle:
        // the mark bits and gray queue are rebuilt from scratch below.
        self.reset_incremental_cycle();
        for slot in &mut self.slots {
            slot.marked = false;
        }
        // WP74: the scratch queue is taken for the duration of the mark
        // phase and returned on every path, so its capacity converges to
        // the reachable-set high-water mark instead of reallocating per
        // collection.
        let mut queue = std::mem::take(&mut self.mark_scratch);
        queue.clear();
        let marked = self.mark_reachable(roots, &mut queue);
        self.mark_scratch = queue;
        let marked = marked?;
        let mut reclaimed = 0;
        let mut bytes_reclaimed = 0_u64;
        let mut collection_bytes_reclaimed = 0_u64;
        for index in 0..self.slots.len() {
            let condemned = {
                let slot = &mut self.slots[index];
                (slot.object.is_some() && !slot.marked)
                    .then(|| slot.object.take().expect("presence checked"))
            };
            if let Some(object) = condemned {
                self.live_objects = self
                    .live_objects
                    .checked_sub(1)
                    .expect("a condemned object was counted as live");
                // G4: payload bytes are measured before the drop; the slot
                // header stays pool-owned and is not "released".
                collection_bytes_reclaimed = collection_bytes_reclaimed
                    .saturating_add(self.object_collection_bytes(&object));
                bytes_reclaimed =
                    bytes_reclaimed.saturating_add(self.release_object_storage(&object));
                let slot = &mut self.slots[index];
                if let Some(generation) = slot.generation.checked_add(1) {
                    slot.generation = generation;
                    self.free
                        .push(u32::try_from(index).expect("slot indices originate as u32"));
                }
                reclaimed += 1;
            }
        }
        self.last_cycle_bytes_reclaimed = bytes_reclaimed;
        self.release_live_payload(bytes_reclaimed);
        self.live_collection_bytes = self
            .live_collection_bytes
            .saturating_sub(collection_bytes_reclaimed);
        // G6 drift pin: the incremental gauge must agree with a full
        // re-derivation at every full-collection boundary.
        debug_assert_eq!(
            self.live_payload_bytes,
            self.recompute_live_payload_bytes(),
            "the live payload gauge drifted from ground truth"
        );
        debug_assert_eq!(
            self.live_objects,
            self.recompute_live_objects(),
            "the live object gauge drifted from ground truth"
        );
        debug_assert_eq!(
            self.live_collection_bytes,
            self.recompute_live_collection_bytes(),
            "the live collection byte gauge drifted from ground truth"
        );
        Ok(CollectionStats {
            marked,
            reclaimed,
            live: self.live_len(),
        })
    }

    pub fn failure_injector(&mut self) -> &mut RuntimeFailureInjector {
        &mut self.failure_injector
    }

    fn reset_incremental_cycle(&mut self) {
        self.gc_phase = GcPhase::Idle;
        self.gc_sweep_cursor = 0;
        self.gc_roots_scanned = 0;
        self.gc_marked = 0;
        self.gc_bytes_marked = 0;
        self.gc_slots_swept = 0;
        self.gc_reclaimed = 0;
        self.gc_barrier_writes = 0;
        self.gc_barrier_shades = 0;
        self.gc_reported_barrier_writes = 0;
        self.gc_reported_barrier_shades = 0;
        self.gc_incremental_work_time = std::time::Duration::ZERO;
        self.gc_max_pause_time = std::time::Duration::ZERO;
        self.gc_bytes_reclaimed = 0;
        self.mark_scratch.clear();
    }

    /// G4: payload bytes released by the most recently *completed*
    /// collection (full or incremental). Cycle-boundary telemetry; the
    /// per-step figure lives on [`IncrementalGcReport::bytes_reclaimed`].
    #[must_use]
    pub const fn last_cycle_bytes_reclaimed(&self) -> u64 {
        self.last_cycle_bytes_reclaimed
    }

    /// G6: out-of-slot payload bytes owned by live objects right now.
    #[must_use]
    pub const fn live_payload_bytes(&self) -> u64 {
        self.live_payload_bytes
    }

    #[must_use]
    pub const fn live_collection_bytes(&self) -> u64 {
        self.live_collection_bytes
    }

    /// O(1) exact bytes owned by live VM objects: occupied object/map/set
    /// headers plus their out-of-slot payloads. Reserved allocator slack and
    /// profiler storage remain separate `GC_V1` inspection categories.
    #[must_use]
    pub fn live_vm_bytes(&self) -> u64 {
        let object_headers =
            (self.live_objects as u64).saturating_mul(size_of::<ObjectSlot>() as u64);
        let live_maps = self.maps.len().saturating_sub(self.free_maps.len()) as u64;
        let map_headers = live_maps.saturating_mul(size_of::<Option<VmMap>>() as u64);
        let live_sets = self.sets.len().saturating_sub(self.free_sets.len()) as u64;
        let set_headers = live_sets.saturating_mul(size_of::<Option<VmSet>>() as u64);
        object_headers
            .saturating_add(map_headers)
            .saturating_add(set_headers)
            .saturating_add(self.live_payload_bytes)
    }

    /// WP71 total live-heap byte ceiling.
    pub const fn set_max_heap_bytes(&mut self, limit: u64) {
        self.max_heap_bytes = limit;
    }

    /// WP71 collection/map arena byte ceiling.
    pub const fn set_max_collection_bytes(&mut self, limit: u64) {
        self.max_collection_bytes = limit;
    }

    fn new_object_header_bytes(map: bool) -> u64 {
        (size_of::<ObjectSlot>() as u64).saturating_add(if map {
            size_of::<Option<VmMap>>() as u64
        } else {
            0
        })
    }

    fn ensure_new_set_headroom(&self, payload: u64) -> Result<(), HeapError> {
        self.ensure_payload_headroom(
            (size_of::<ObjectSlot>() as u64 + size_of::<Option<VmSet>>() as u64)
                .saturating_add(payload),
        )
    }

    fn ensure_new_object_headroom(&self, payload: u64, map: bool) -> Result<(), HeapError> {
        self.ensure_payload_headroom(Self::new_object_header_bytes(map).saturating_add(payload))
    }

    /// Admission check for bytes about to join the total live heap.
    fn ensure_payload_headroom(&self, additional: u64) -> Result<(), HeapError> {
        if self.live_vm_bytes().saturating_add(additional) > self.max_heap_bytes {
            return Err(HeapError::CapacityExhausted);
        }
        Ok(())
    }

    fn ensure_collection_headroom(&self, additional: u64) -> Result<(), HeapError> {
        if self.live_collection_bytes.saturating_add(additional) > self.max_collection_bytes {
            return Err(HeapError::CapacityExhausted);
        }
        Ok(())
    }

    /// WP71 gauge maintenance; saturating on both edges so accounting can
    /// never panic even if a footprint model bug under-releases.
    fn charge_live_payload(&mut self, bytes: u64) {
        self.counters.allocated_bytes = self.counters.allocated_bytes.saturating_add(bytes);
        self.live_payload_bytes = self.live_payload_bytes.saturating_add(bytes);
    }

    fn charge_collection_payload(&mut self, bytes: u64) {
        self.charge_live_payload(bytes);
        self.live_collection_bytes = self.live_collection_bytes.saturating_add(bytes);
    }

    /// WP13: records bytes physically copied by Host/Script codecs into VM
    /// object or collection storage. The counter is a monotonic work total,
    /// so a later transactional rollback intentionally does not rewind it.
    pub(crate) fn record_host_codec_copy(&mut self, bytes: u64) {
        self.counters.host_codec_copy_bytes =
            self.counters.host_codec_copy_bytes.saturating_add(bytes);
    }

    pub(crate) fn record_host_codec_storage_copy(
        &mut self,
        storage: CollectionStorage,
        elements: usize,
    ) {
        self.record_host_codec_copy((elements as u64).saturating_mul(storage.cell_size() as u64));
    }

    pub(crate) fn record_host_codec_field_copy(&mut self, fields: &[RuntimeValue]) {
        self.record_host_codec_storage_copy(homogeneous_field_storage(fields), fields.len());
    }

    pub(crate) fn record_host_codec_collection_copy(
        &mut self,
        element_type: nexa_bytecode::ValueType,
        values: &[RuntimeValue],
    ) -> Result<(), HeapError> {
        let storage = collection_storage_for_values(element_type, values)?;
        self.record_host_codec_storage_copy(storage, values.len());
        Ok(())
    }

    fn release_live_payload(&mut self, bytes: u64) {
        self.live_payload_bytes = self.live_payload_bytes.saturating_sub(bytes);
    }

    fn release_collection_payload(&mut self, bytes: u64) {
        self.release_live_payload(bytes);
        self.live_collection_bytes = self.live_collection_bytes.saturating_sub(bytes);
    }

    /// Ground truth for the G6 gauge: one full walk. Used on checkpoint
    /// restore and by the drift assertion inside full collection.
    fn recompute_live_payload_bytes(&self) -> u64 {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_ref())
            .map(|object| self.object_payload_bytes(object))
            .fold(0, u64::saturating_add)
    }

    fn recompute_live_collection_bytes(&self) -> u64 {
        self.slots
            .iter()
            .filter_map(|slot| slot.object.as_ref())
            .map(|object| self.object_collection_bytes(object))
            .fold(0, u64::saturating_add)
    }

    fn recompute_live_objects(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.object.is_some())
            .count()
    }

    /// Current incremental phase (G1); `Idle` outside an active cycle.
    #[must_use]
    pub const fn gc_phase(&self) -> GcPhase {
        self.gc_phase
    }

    /// Configured object-slot ceiling (G2 trigger input).
    #[must_use]
    pub const fn max_objects(&self) -> u32 {
        self.max_objects
    }

    #[must_use]
    pub const fn max_heap_bytes(&self) -> u64 {
        self.max_heap_bytes
    }

    #[must_use]
    pub const fn max_collection_bytes(&self) -> u64 {
        self.max_collection_bytes
    }

    #[must_use]
    pub const fn collection_elements_used(&self) -> usize {
        self.collection_elements_used
    }

    #[must_use]
    pub const fn max_collection_elements(&self) -> usize {
        self.max_collection_elements
    }

    /// G3 gray-enqueue: marks on push (classic BFS deduplication), so every
    /// object enters the queue at most once per cycle and the preallocated
    /// queue capacity is a hard bound - Mark never allocates. Stale or
    /// vacant references are ignored here; the mutator-facing write paths
    /// validate references before they ever reach the collector.
    fn enqueue_gray(
        slots: &mut [ObjectSlot],
        queue: &mut VecDeque<GcRef>,
        reference: GcRef,
    ) -> bool {
        let Some(slot) = slots
            .get_mut(reference.index as usize)
            .filter(|slot| slot.generation == reference.generation && slot.object.is_some())
        else {
            return false;
        };
        if slot.marked {
            return false;
        }
        slot.marked = true;
        queue.push_back(reference);
        true
    }

    /// G1 insertion barrier: while a mark phase is active, a reference
    /// value being published into a live object is shaded gray so the
    /// tri-color invariant holds under mutation.
    fn shade_on_write(&mut self, value: RuntimeValue) {
        if self.gc_phase != GcPhase::Mark {
            return;
        }
        if let Some(child) = value_reference(value) {
            self.gc_barrier_writes = self.gc_barrier_writes.saturating_add(1);
            if Self::enqueue_gray(&mut self.slots, &mut self.mark_scratch, child) {
                self.gc_marked += 1;
                self.gc_barrier_shades = self.gc_barrier_shades.saturating_add(1);
            }
        }
    }

    fn open_incremental_cycle(&mut self) {
        if !matches!(self.gc_phase, GcPhase::Idle | GcPhase::Complete) {
            return;
        }
        self.gc_cycle = self.gc_cycle.saturating_add(1);
        for slot in &mut self.slots {
            slot.marked = false;
        }
        self.gc_roots_scanned = 0;
        self.gc_marked = 0;
        self.gc_bytes_marked = 0;
        self.gc_slots_swept = 0;
        self.gc_reclaimed = 0;
        self.gc_barrier_writes = 0;
        self.gc_barrier_shades = 0;
        self.gc_reported_barrier_writes = 0;
        self.gc_reported_barrier_shades = 0;
        self.gc_incremental_work_time = std::time::Duration::ZERO;
        self.gc_max_pause_time = std::time::Duration::ZERO;
        self.gc_bytes_reclaimed = 0;
        self.gc_sweep_cursor = 0;
        self.mark_scratch.clear();
        self.gc_phase = GcPhase::RootSnapshot;
    }

    fn snapshot_incremental_roots(
        &mut self,
        roots: &GcRoots,
        report: &mut IncrementalGcReport,
    ) -> Result<(), HeapError> {
        if self.gc_phase != GcPhase::RootSnapshot {
            return Ok(());
        }
        // Validate the entire snapshot before changing a mark bit so a stale
        // root cannot leave a half-seeded active cycle behind.
        for root in roots.iter() {
            if let Err(error) = self.validate_reference(root) {
                self.reset_incremental_cycle();
                return Err(error);
            }
            report.roots_scanned = report.roots_scanned.saturating_add(1);
        }
        let mut queue = std::mem::take(&mut self.mark_scratch);
        for root in roots.iter() {
            if Self::enqueue_gray(&mut self.slots, &mut queue, root) {
                report.roots_seeded += 1;
            }
        }
        self.mark_scratch = queue;
        self.gc_roots_scanned = report.roots_scanned;
        self.gc_marked = report.roots_seeded;
        self.gc_phase = GcPhase::Mark;
        Ok(())
    }

    fn run_incremental_mark(
        &mut self,
        budget: &mut StepBudget,
        report: &mut IncrementalGcReport,
    ) -> Result<(), HeapError> {
        if self.gc_phase != GcPhase::Mark {
            return Ok(());
        }
        let mut queue = std::mem::take(&mut self.mark_scratch);
        let mark_bytes_before = budget.work_bytes;
        let grayed = self.mark_step(&mut queue, budget);
        self.mark_scratch = queue;
        let grayed = grayed?;
        report.bytes_marked = budget.work_bytes.saturating_sub(mark_bytes_before);
        self.gc_bytes_marked = self.gc_bytes_marked.saturating_add(report.bytes_marked);
        self.gc_marked += grayed;
        report.objects_marked = grayed + report.roots_seeded;
        // A budget-exhausting final pop transitions on the next slice,
        // preserving the strict object-work bound.
        if self.mark_scratch.is_empty() && budget.available() {
            self.gc_phase = GcPhase::Sweep;
            self.gc_sweep_cursor = 0;
        }
        Ok(())
    }

    fn run_incremental_sweep(&mut self, budget: &mut StepBudget, report: &mut IncrementalGcReport) {
        if self.gc_phase != GcPhase::Sweep {
            return;
        }
        let mut collection_bytes_reclaimed = 0_u64;
        while budget.available() && self.gc_sweep_cursor < self.slots.len() {
            let index = self.gc_sweep_cursor;
            self.gc_sweep_cursor += 1;
            report.slots_swept += 1;
            self.gc_slots_swept += 1;
            let mut payload = 0;
            let condemned = {
                let slot = &mut self.slots[index];
                (slot.object.is_some() && !slot.marked)
                    .then(|| slot.object.take().expect("presence checked"))
            };
            if let Some(object) = condemned {
                self.live_objects = self
                    .live_objects
                    .checked_sub(1)
                    .expect("a condemned object was counted as live");
                collection_bytes_reclaimed = collection_bytes_reclaimed
                    .saturating_add(self.object_collection_bytes(&object));
                payload = self.release_object_storage(&object);
                report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(payload);
                self.gc_bytes_reclaimed = self.gc_bytes_reclaimed.saturating_add(payload);
                let slot = &mut self.slots[index];
                if let Some(generation) = slot.generation.checked_add(1) {
                    slot.generation = generation;
                    self.free
                        .push(u32::try_from(index).expect("slot indices originate as u32"));
                }
                self.gc_reclaimed += 1;
            }
            budget.charge(payload);
        }
        self.release_live_payload(report.bytes_reclaimed);
        self.live_collection_bytes = self
            .live_collection_bytes
            .saturating_sub(collection_bytes_reclaimed);
        if self.gc_sweep_cursor >= self.slots.len() {
            debug_assert_eq!(
                self.live_objects,
                self.recompute_live_objects(),
                "the incremental live object gauge drifted from ground truth"
            );
            debug_assert_eq!(
                self.live_collection_bytes,
                self.recompute_live_collection_bytes(),
                "the incremental collection byte gauge drifted from ground truth"
            );
            report.completed = Some(CollectionStats {
                marked: self.gc_marked,
                reclaimed: self.gc_reclaimed,
                live: self.live_len(),
            });
            self.last_cycle_bytes_reclaimed = self.gc_bytes_reclaimed;
            self.gc_phase = GcPhase::Complete;
        }
    }

    fn finish_incremental_report(&mut self, budget: StepBudget, report: &mut IncrementalGcReport) {
        report.barrier_writes = self
            .gc_barrier_writes
            .saturating_sub(self.gc_reported_barrier_writes);
        report.barrier_shades = self
            .gc_barrier_shades
            .saturating_sub(self.gc_reported_barrier_shades);
        self.gc_reported_barrier_writes = self.gc_barrier_writes;
        self.gc_reported_barrier_shades = self.gc_barrier_shades;
        report.phase = self.gc_phase;
        report.live_bytes = self.live_vm_bytes();
        report.fragmentation_per_mille = self.collection_fragmentation_per_mille();
        budget.finish(report);
        self.gc_incremental_work_time = self
            .gc_incremental_work_time
            .saturating_add(report.pause_time);
        self.gc_max_pause_time = self.gc_max_pause_time.max(report.pause_time);
        report.telemetry = GcCycleTelemetry {
            cycle: self.gc_cycle,
            phase: self.gc_phase,
            roots: self.gc_roots_scanned,
            objects_marked: self.gc_marked,
            bytes_marked: self.gc_bytes_marked,
            objects_swept: self.gc_slots_swept,
            bytes_reclaimed: self.gc_bytes_reclaimed,
            live_bytes: report.live_bytes,
            pause_time: self.gc_max_pause_time,
            incremental_work_time: self.gc_incremental_work_time,
            barrier_count: self.gc_barrier_writes,
            remembered_writes: self.gc_barrier_shades,
            fragmentation_per_mille: report.fragmentation_per_mille,
        };
    }

    /// One budgeted step of the WP75 incremental state machine:
    /// `Idle -> RootSnapshot -> Mark -> Sweep -> Complete`.
    ///
    /// `RootSnapshot` is atomic and allocation-free: it validates the precise
    /// root set, then shades it into the preallocated gray queue exactly once
    /// per cycle. Removing a root later is conservative; every operation
    /// capable of publishing a new reference while Mark is active goes
    /// through the insertion barrier. Objects allocated during any active
    /// phase are born marked and survive to the next cycle.
    pub fn collect_incremental(
        &mut self,
        roots: &GcRoots,
        budget: GcBudget,
    ) -> Result<IncrementalGcReport, HeapError> {
        let mut report = IncrementalGcReport::default();
        if budget.max_objects == 0 {
            return Ok(report);
        }
        let mut budget = StepBudget::new(budget);
        // G3 bound: marks land at enqueue time, so every object enters the
        // gray queue at most once per cycle and the preallocated capacity
        // is never outgrown - Mark performs zero system allocations.
        let queue_capacity_before = self.mark_scratch.capacity();
        self.open_incremental_cycle();
        report.cycle = self.gc_cycle;
        self.snapshot_incremental_roots(roots, &mut report)?;
        self.run_incremental_mark(&mut budget, &mut report)?;
        self.run_incremental_sweep(&mut budget, &mut report);
        self.finish_incremental_report(budget, &mut report);
        debug_assert_eq!(
            self.mark_scratch.capacity(),
            queue_capacity_before,
            "the bounded gray queue must never reallocate"
        );
        Ok(report)
    }

    fn enqueue_collection_children(
        &mut self,
        queue: &mut VecDeque<GcRef>,
        storage: CollectionStorage,
        range: CollectionRange,
        live: usize,
    ) -> Result<usize, HeapError> {
        let mut grayed = 0;
        let mut enqueue = |child| {
            if Self::enqueue_gray(&mut self.slots, queue, child) {
                grayed += 1;
            }
        };
        match storage {
            CollectionStorage::Values => {
                for value in self.collections.values(range)?[..live].iter().copied() {
                    if let Some(child) = value_reference(value) {
                        enqueue(child);
                    }
                }
            }
            CollectionStorage::String => {
                for (child, _) in self.scalar_collections.strings().values(range)?[..live]
                    .iter()
                    .copied()
                {
                    enqueue(child);
                }
            }
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                for child in self.scalar_collections.refs().values(range)?[..live]
                    .iter()
                    .copied()
                {
                    enqueue(child);
                }
            }
            CollectionStorage::I32
            | CollectionStorage::I64
            | CollectionStorage::F32
            | CollectionStorage::F64
            | CollectionStorage::Bool
            | CollectionStorage::Rune => {}
        }
        Ok(grayed)
    }

    /// Drains gray references within the step budget; every pop was already
    /// marked at enqueue time, so this only scans children, streaming them
    /// back through [`Self::enqueue_gray`] with no temporary allocation
    /// (WP73) and no queue growth past its preallocated bound (G3). Each
    /// pop charges one work unit plus the popped object's payload bytes
    /// (G5). Returns the number of newly grayed children.
    fn mark_step(
        &mut self,
        queue: &mut VecDeque<GcRef>,
        budget: &mut StepBudget,
    ) -> Result<usize, HeapError> {
        let mut grayed = 0;
        while budget.available() {
            let Some(reference) = queue.pop_front() else {
                break;
            };
            let slot = self
                .slots
                .get(reference.index as usize)
                .filter(|slot| slot.generation == reference.generation)
                .and_then(|slot| slot.object.as_ref())
                .ok_or(HeapError::InvalidReference(reference))?;
            debug_assert!(
                self.slots[reference.index as usize].marked,
                "gray queue entries are marked at enqueue time"
            );
            let payload = self.object_payload_bytes(slot);
            match slot {
                Object::Array {
                    range,
                    length,
                    row_stride,
                    storage,
                    ..
                } => {
                    let range = *range;
                    // Live cells cover the row stride (WP52); dead capacity
                    // beyond the live prefix never enters the mark queue.
                    let live =
                        length.saturating_mul(row_stride.map_or(1, |s| usize::from(s.get())));
                    let live = live.min(range.length);
                    grayed += self.enqueue_collection_children(queue, *storage, range, live)?;
                }
                // Buffer and exact Struct/Class extents contain one live
                // RuntimeValue per cell.
                Object::Buffer { storage, range, .. }
                | Object::Struct { storage, range, .. }
                | Object::Class { storage, range, .. } => {
                    let range = *range;
                    grayed +=
                        self.enqueue_collection_children(queue, *storage, range, range.length)?;
                }
                Object::Map { storage } => {
                    let map = self
                        .maps
                        .get(*storage as usize)
                        .and_then(Option::as_ref)
                        .ok_or(HeapError::InvalidReference(reference))?;
                    let slots = &mut self.slots;
                    map.trace_references(
                        &self.map_slots,
                        &self.collections,
                        &self.scalar_collections,
                        &mut |child| {
                            if Self::enqueue_gray(slots, queue, child) {
                                grayed += 1;
                            }
                        },
                    );
                }
                Object::Set { storage } => {
                    let set = self
                        .sets
                        .get(*storage as usize)
                        .and_then(Option::as_ref)
                        .ok_or(HeapError::InvalidReference(reference))?;
                    let slots = &mut self.slots;
                    set.trace_references(&self.map_slots, &mut |child| {
                        if Self::enqueue_gray(slots, queue, child) {
                            grayed += 1;
                        }
                    });
                }
                _ => {
                    // The object is briefly taken out of its slot so the
                    // visitor can enqueue children against `self.slots`
                    // without aliasing; a self-reference is already marked
                    // (marks land at enqueue time), so the momentarily
                    // vacant slot cannot lose edges. No allocation occurs:
                    // the object moves by value, and the queue is bounded.
                    let index = reference.index as usize;
                    let taken = self.slots[index]
                        .object
                        .take()
                        .expect("presence checked above");
                    taken.trace_references(&mut |child| {
                        if Self::enqueue_gray(&mut self.slots, queue, child) {
                            grayed += 1;
                        }
                    });
                    self.slots[index].object = Some(taken);
                }
            }
            budget.charge(payload);
        }
        Ok(grayed)
    }

    #[must_use]
    pub fn collection_inspection(&self) -> CollectionArenaInspection {
        CollectionArenaInspection {
            capacity: self.max_collection_elements,
            free_elements: self
                .max_collection_elements
                .saturating_sub(self.collection_elements_used),
            free_ranges: self.collections.free_ranges.len(),
        }
    }

    /// WP78 trigger/telemetry input. The score is the fraction of free
    /// collection and map cells that are unavailable in the largest
    /// contiguous extent of their respective arenas. It is computed without
    /// allocation and is bounded by the preallocated free-range indexes.
    #[must_use]
    pub fn collection_fragmentation_per_mille(&self) -> u16 {
        fn arena_fragmentation(ranges: &[CollectionRange]) -> u16 {
            let total = ranges
                .iter()
                .map(|range| range.length)
                .fold(0_usize, usize::saturating_add);
            if total == 0 {
                return 0;
            }
            let largest = ranges.iter().map(|range| range.length).max().unwrap_or(0);
            u16::try_from(total.saturating_sub(largest).saturating_mul(1_000) / total)
                .unwrap_or(1_000)
                .min(1_000)
        }

        // The arenas serve different allocation classes, so the worst
        // individual score is authoritative; combining their free cells
        // would let an unfragmented map arena hide an unusable array arena.
        arena_fragmentation(&self.collections.free_ranges)
            .max(arena_fragmentation(&self.map_slots.free_ranges))
    }

    fn scalar_arena_reserved_bytes(&self) -> u64 {
        self.scalar_collections.reserved_bytes()
    }

    /// `GC_V1` heap byte accounting by category (G4). One full walk over the
    /// slot pool plus O(1) arena metadata - inspection-grade, never called
    /// from the hot path or from inside a bounded GC step.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn byte_inspection(&self) -> HeapByteInspection {
        let slot_bytes = size_of::<ObjectSlot>() as u64;
        let map_header_bytes = size_of::<Option<VmMap>>() as u64;
        let value_bytes = size_of::<RuntimeValue>() as u64;
        let mut inspection = HeapByteInspection::default();
        let mut occupied = 0_u64;
        let mut generic_arena_live = 0_u64;
        let mut scalar_arena_live = 0_u64;
        for slot in &self.slots {
            let Some(object) = slot.object.as_ref() else {
                continue;
            };
            occupied += 1;
            match object {
                Object::String(_) | Object::SharedString(_) => {
                    inspection.string_bytes = inspection
                        .string_bytes
                        .saturating_add(object.payload_bytes());
                }
                Object::Array { storage, .. } => {
                    inspection.array_bytes = inspection
                        .array_bytes
                        .saturating_add(object.payload_bytes());
                    if storage.is_compact() {
                        scalar_arena_live =
                            scalar_arena_live.saturating_add(object.payload_bytes());
                    } else {
                        generic_arena_live =
                            generic_arena_live.saturating_add(object.payload_bytes());
                    }
                }
                Object::Buffer { storage, .. } => {
                    inspection.buffer_bytes = inspection
                        .buffer_bytes
                        .saturating_add(object.payload_bytes());
                    if storage.is_compact() {
                        scalar_arena_live =
                            scalar_arena_live.saturating_add(object.payload_bytes());
                    } else {
                        generic_arena_live =
                            generic_arena_live.saturating_add(object.payload_bytes());
                    }
                }
                Object::Map { .. } | Object::Set { .. } => {
                    inspection.map_bytes = inspection
                        .map_bytes
                        .saturating_add(self.object_payload_bytes(object));
                }
                Object::Class { storage, .. } | Object::Struct { storage, .. } => {
                    inspection.class_payload_bytes = inspection
                        .class_payload_bytes
                        .saturating_add(object.payload_bytes());
                    if storage.is_compact() {
                        scalar_arena_live =
                            scalar_arena_live.saturating_add(object.payload_bytes());
                    } else {
                        generic_arena_live =
                            generic_arena_live.saturating_add(object.payload_bytes());
                    }
                }
                Object::Enum { .. } => {}
            }
        }
        let occupied_map_headers = self.maps.iter().filter(|map| map.is_some()).count() as u64;
        let occupied_set_headers = self.sets.iter().filter(|set| set.is_some()).count() as u64;
        let set_header_bytes = size_of::<Option<VmSet>>() as u64;
        inspection.object_header_bytes = occupied
            .saturating_mul(slot_bytes)
            .saturating_add(occupied_map_headers.saturating_mul(map_header_bytes))
            .saturating_add(occupied_set_headers.saturating_mul(set_header_bytes));
        let pool_slots = self.slots.capacity().max(self.slots.len()) as u64;
        let vacant_pool_bytes = pool_slots
            .saturating_sub(occupied)
            .saturating_mul(slot_bytes);
        let map_pool_slots = self.maps.capacity().max(self.maps.len()) as u64;
        let vacant_map_pool_bytes = map_pool_slots
            .saturating_sub(occupied_map_headers)
            .saturating_mul(map_header_bytes);
        let set_pool_slots = self.sets.capacity().max(self.sets.len()) as u64;
        let vacant_set_pool_bytes = set_pool_slots
            .saturating_sub(occupied_set_headers)
            .saturating_mul(set_header_bytes);
        let generic_arena_reserved =
            (self.collections.values.capacity() as u64).saturating_mul(value_bytes);
        let scalar_arena_reserved = self.scalar_arena_reserved_bytes();
        let arena_free_bytes = generic_arena_reserved
            .saturating_sub(generic_arena_live)
            .saturating_add(scalar_arena_reserved.saturating_sub(scalar_arena_live));
        let map_arena_free_bytes = (self
            .map_slots
            .free_ranges
            .iter()
            .map(|range| range.length)
            .sum::<usize>() as u64)
            .saturating_mul(size_of::<Option<MapEntry>>() as u64);
        inspection.allocator_slack_bytes = vacant_pool_bytes
            .saturating_add(vacant_map_pool_bytes)
            .saturating_add(vacant_set_pool_bytes)
            .saturating_add(arena_free_bytes)
            .saturating_add(map_arena_free_bytes);
        inspection.profiler_bytes = crate::profiler::thread_storage_bytes();
        self.debug_assert_byte_gauges(inspection);
        inspection
    }

    fn debug_assert_byte_gauges(&self, inspection: HeapByteInspection) {
        debug_assert_eq!(
            inspection.live_total(),
            self.live_vm_bytes(),
            "O(1) live byte gauge drifted from GC_V1 inspection"
        );
        debug_assert_eq!(
            inspection.collection_total(),
            self.live_collection_bytes,
            "O(1) collection byte gauge drifted from GC_V1 inspection"
        );
    }

    pub(crate) fn failure_trigger(&self, point: RuntimeFailurePoint) -> bool {
        self.failure_injector.trigger(point)
    }

    pub(crate) fn set_failure_injector(&mut self, injector: RuntimeFailureInjector) {
        self.failure_injector = injector;
    }

    #[must_use]
    pub const fn live_len(&self) -> usize {
        self.live_objects
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

fn validate_buffer_parts_copy(
    destination: BufferParts,
    source: BufferParts,
    source_start: usize,
    destination_start: usize,
    length: usize,
) -> Result<(usize, usize), HeapError> {
    if (source.type_id, source.element_type, source.storage)
        != (
            destination.type_id,
            destination.element_type,
            destination.storage,
        )
    {
        return Err(invalid_value_reference());
    }
    let source_end = checked_collection_end(source_start, length, source.range.length)?;
    let destination_end =
        checked_collection_end(destination_start, length, destination.range.length)?;
    Ok((source_end, destination_end))
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

fn table_needs_rehash(slots: usize, length: usize) -> bool {
    slots == 0 || length.saturating_add(1).saturating_mul(4) > slots.saturating_mul(3)
}

fn next_table_capacity(slots: usize, length: usize, max_collection_length: usize) -> Option<usize> {
    if !table_needs_rehash(slots, length) {
        return None;
    }
    let maximum_capacity = max_collection_length
        .saturating_mul(2)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    Some(slots.saturating_mul(2).max(1).min(maximum_capacity))
}

fn map_needs_rehash(map: &VmMap) -> bool {
    table_needs_rehash(map.slots.length, map.length)
}

fn next_map_capacity(map: &VmMap, max_collection_length: usize) -> Option<usize> {
    next_table_capacity(map.slots.length, map.length, max_collection_length)
}

fn insert_map_entry(slots: &mut [Option<MapEntry>], entry: MapEntry) -> Result<usize, HeapError> {
    if slots.is_empty() {
        return Err(HeapError::CapacityExhausted);
    }
    let start = usize::try_from(entry.hash % slots.len() as u64)
        .expect("hash modulo slot count fits usize");
    for offset in 0..slots.len() {
        let index = (start + offset) % slots.len();
        if slots[index].is_none() {
            slots[index] = Some(entry);
            return Ok(index);
        }
    }
    Err(HeapError::CapacityExhausted)
}

fn typed_collection_copy_within_arenas(
    values: &mut CollectionArena,
    scalar_values: &mut ScalarArenaSet,
    storage: CollectionStorage,
    range: CollectionRange,
    source: std::ops::Range<usize>,
    destination: usize,
) -> Result<(), HeapError> {
    match storage {
        CollectionStorage::I32 => scalar_values
            .i32_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::I64 => scalar_values
            .i64_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::F32 => scalar_values
            .f32_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::F64 => scalar_values
            .f64_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::Bool => scalar_values
            .bools_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::Rune => scalar_values
            .runes_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::String => scalar_values
            .strings_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::Ref | CollectionStorage::NamedRef => scalar_values
            .refs_mut()
            .values_mut(range)?
            .copy_within(source, destination),
        CollectionStorage::Values => values.values_mut(range)?.copy_within(source, destination),
    }
    Ok(())
}

fn typed_collection_copy_absolute(
    values: &mut CollectionArena,
    scalar_values: &mut ScalarArenaSet,
    storage: CollectionStorage,
    source: std::ops::Range<usize>,
    destination: usize,
) -> Result<(), HeapError> {
    let length = source.end.saturating_sub(source.start);
    let start = source.start.min(destination);
    let end = source.end.max(
        destination
            .checked_add(length)
            .ok_or(HeapError::CapacityExhausted)?,
    );
    let encompassing = CollectionRange {
        start,
        length: end.checked_sub(start).ok_or(HeapError::CapacityExhausted)?,
    };
    typed_collection_copy_within_arenas(
        values,
        scalar_values,
        storage,
        encompassing,
        source.start - start..source.end - start,
        destination - start,
    )
}

fn typed_collection_clear_in_arenas(
    values: &mut CollectionArena,
    scalar_values: &mut ScalarArenaSet,
    storage: CollectionStorage,
    range: CollectionRange,
    cells: std::ops::Range<usize>,
) -> Result<(), HeapError> {
    match storage {
        CollectionStorage::I32 => {
            scalar_values.i32_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::I64 => {
            scalar_values.i64_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::F32 => {
            scalar_values.f32_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::F64 => {
            scalar_values.f64_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::Bool => {
            scalar_values.bools_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::Rune => {
            scalar_values.runes_mut().values_mut(range)?[cells].fill(0);
        }
        CollectionStorage::String => {
            scalar_values.strings_mut().values_mut(range)?[cells].fill((
                GcRef {
                    index: u32::MAX,
                    generation: u32::MAX,
                },
                0,
            ));
        }
        CollectionStorage::Ref | CollectionStorage::NamedRef => {
            scalar_values.refs_mut().values_mut(range)?[cells].fill(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            });
        }
        CollectionStorage::Values => {
            values.values_mut(range)?[cells].fill(RuntimeValue::Unit);
        }
    }
    Ok(())
}

fn map_value_row(
    values: CollectionRange,
    value_slots: u16,
    index: usize,
) -> Option<CollectionRange> {
    let slots = usize::from(value_slots);
    let offset = index.checked_mul(slots)?;
    let start = values.start.checked_add(offset)?;
    let end = offset.checked_add(slots)?;
    (end <= values.length).then_some(CollectionRange {
        start,
        length: slots,
    })
}

fn map_location_values(map: &VmMap, location: MapLocation) -> CollectionRange {
    match location {
        MapLocation::Current(_) => map.values,
        MapLocation::RehashOld(_) => {
            map.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .old_values
        }
        MapLocation::RehashNew(_) => {
            map.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .new_values
        }
    }
}

fn map_entry_value_range(map: &VmMap, location: MapLocation) -> Result<CollectionRange, HeapError> {
    map_value_row(
        map_location_values(map, location),
        map.value_slots,
        map_location_index(location),
    )
    .ok_or_else(invalid_value_reference)
}

/// Migrates one bounded chunk of old slots into new slots (shared by map
/// and set incremental rehash). Returns the next cursor; `moved` observes
/// each migration (the map side copies companion value rows there).
fn migrate_rehash_chunk(
    arena: &mut MapSlotArena,
    old_slots: CollectionRange,
    new_slots: CollectionRange,
    cursor: usize,
    mut moved: impl FnMut(usize, usize) -> Result<(), HeapError>,
) -> Result<(usize, usize), HeapError> {
    const REHASH_CHUNK: usize = 8;
    let end = cursor.saturating_add(REHASH_CHUNK).min(old_slots.length);
    let mut migrated = 0;
    for index in cursor..end {
        if let Some(entry) = arena.slots_mut(old_slots)[index].take() {
            let destination = insert_map_entry(arena.slots_mut(new_slots), entry)?;
            migrated += 1;
            moved(index, destination)?;
        }
    }
    Ok((end, migrated))
}

/// Advances one bounded rehash chunk for a map. Completion returns the old
/// companion value extent and the exact slot+value bytes leaving the live
/// footprint.
/// Advances one bounded rehash chunk for a map. Returns the number of
/// entries actually migrated plus, on completion, the old companion value
/// extent and the exact slot+value bytes leaving the live footprint.
/// Callers advance the mutation epoch on every progress step.
fn progress_map_rehash(
    map: &mut VmMap,
    arena: &mut MapSlotArena,
    values: &mut CollectionArena,
    scalar_values: &mut ScalarArenaSet,
) -> Result<(usize, Option<(CollectionRange, u64)>), HeapError> {
    let rehash = map.rehash.as_mut().expect("rehash state checked by caller");
    let old_slots = rehash.old_slots;
    let new_slots = rehash.new_slots;
    let value_slots = map.value_slots;
    let value_storage = map.value_storage;
    let old_values = rehash.old_values;
    let new_values = rehash.new_values;
    let (cursor, migrated) = migrate_rehash_chunk(
        arena,
        old_slots,
        new_slots,
        rehash.cursor,
        |source, destination| {
            let source = map_value_row(old_values, value_slots, source)
                .ok_or_else(invalid_value_reference)?;
            let destination = map_value_row(new_values, value_slots, destination)
                .ok_or_else(invalid_value_reference)?;
            typed_collection_copy_absolute(
                values,
                scalar_values,
                value_storage,
                source.start..source.end(),
                destination.start,
            )?;
            let source_length = source.length;
            typed_collection_clear_in_arenas(
                values,
                scalar_values,
                value_storage,
                source,
                0..source_length,
            )
        },
    )?;
    rehash.cursor = cursor;
    if cursor == old_slots.length {
        let released = (old_slots.length as u64)
            .saturating_mul(size_of::<Option<MapEntry>>() as u64)
            .saturating_add(
                (old_values.length as u64).saturating_mul(map.value_storage.cell_size() as u64),
            );
        arena.release(old_slots);
        map.slots = new_slots;
        map.values = new_values;
        map.rehash = None;
        return Ok((migrated, Some((old_values, released))));
    }
    Ok((migrated, None))
}

/// Advances one bounded rehash chunk for a set; completion returns the
/// exact slot bytes leaving the live footprint.
/// Advances one bounded rehash chunk for a set; returns the number of
/// entries actually migrated plus, on completion, the exact slot bytes
/// leaving the live footprint. Callers advance the mutation epoch on
/// every progress step.
fn progress_set_rehash(
    set: &mut VmSet,
    arena: &mut MapSlotArena,
) -> Result<(usize, Option<u64>), HeapError> {
    let rehash = set.rehash.as_mut().expect("rehash state checked by caller");
    let old_slots = rehash.old_slots;
    let new_slots = rehash.new_slots;
    let (cursor, migrated) =
        migrate_rehash_chunk(arena, old_slots, new_slots, rehash.cursor, |_, _| Ok(()))?;
    rehash.cursor = cursor;
    if cursor == old_slots.length {
        let released =
            (old_slots.length as u64).saturating_mul(size_of::<Option<MapEntry>>() as u64);
        arena.release(old_slots);
        set.slots = new_slots;
        set.rehash = None;
        return Ok((migrated, Some(released)));
    }
    Ok((migrated, None))
}

fn set_location_range(set: &VmSet, location: SetLocation) -> CollectionRange {
    match location {
        SetLocation::Current(_) => set.slots,
        SetLocation::RehashOld(_) => {
            set.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .old_slots
        }
        SetLocation::RehashNew(_) => {
            set.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .new_slots
        }
    }
}

const fn set_location_index(location: SetLocation) -> usize {
    match location {
        SetLocation::Current(index)
        | SetLocation::RehashOld(index)
        | SetLocation::RehashNew(index) => index,
    }
}

fn map_slot_location(phase: u8, index: usize) -> MapLocation {
    match phase {
        0 => MapLocation::Current(index),
        1 => MapLocation::RehashOld(index),
        2 => MapLocation::RehashNew(index),
        _ => unreachable!("iterator phases are bounded to current/old/new"),
    }
}

fn map_location_range(map: &VmMap, location: MapLocation) -> CollectionRange {
    match location {
        MapLocation::Current(_) => map.slots,
        MapLocation::RehashOld(_) => {
            map.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .old_slots
        }
        MapLocation::RehashNew(_) => {
            map.rehash
                .as_ref()
                .expect("located rehash entry has state")
                .new_slots
        }
    }
}

const fn map_location_index(location: MapLocation) -> usize {
    match location {
        MapLocation::Current(index)
        | MapLocation::RehashOld(index)
        | MapLocation::RehashNew(index) => index,
    }
}

/// K3: removes one entry from a linear-probe table and restores the
/// probe invariant by shifting displaced successors back over the hole
/// (no tombstones). An entry moves back exactly when the hole lies on
/// its probe path, i.e. its cyclic displacement from home reaches past
/// the hole; entries sitting at or after their home stay put. The walk
/// stops at the first empty slot, or after one full cycle for tables
/// running without an empty slot at the capacity ceiling.
#[cfg(test)]
fn remove_probed_entry(slots: &mut [Option<MapEntry>], index: usize) -> MapEntry {
    remove_probed_entry_with_moves(slots, index, |_, _| {}).0
}

fn remove_probed_entry_with_values(
    slots: &mut [Option<MapEntry>],
    values: &mut CollectionArena,
    scalar_values: &mut ScalarArenaSet,
    storage: CollectionStorage,
    value_table: CollectionRange,
    value_slots: u16,
    index: usize,
) -> MapEntry {
    let (removed, hole) = remove_probed_entry_with_moves(slots, index, |source, destination| {
        let source = map_value_row(value_table, value_slots, source)
            .expect("map slot source has a companion value row");
        let destination = map_value_row(value_table, value_slots, destination)
            .expect("map slot destination has a companion value row");
        typed_collection_copy_absolute(
            values,
            scalar_values,
            storage,
            source.start..source.end(),
            destination.start,
        )
        .expect("map companion value rows share a live typed arena");
    });
    let hole = map_value_row(value_table, value_slots, hole)
        .expect("map deletion hole has a companion value row");
    let hole_length = hole.length;
    typed_collection_clear_in_arenas(values, scalar_values, storage, hole, 0..hole_length)
        .expect("map deletion hole remains inside its typed arena");
    removed
}

fn remove_probed_entry_with_moves(
    slots: &mut [Option<MapEntry>],
    index: usize,
    mut moved: impl FnMut(usize, usize),
) -> (MapEntry, usize) {
    let removed = slots[index].take().expect("located map entry exists");
    let len = slots.len();
    let mut hole = index;
    let mut cursor = (index + 1) % len;
    while cursor != index {
        let Some(entry) = slots[cursor] else {
            break;
        };
        let origin =
            usize::try_from(entry.hash % len as u64).expect("hash modulo slot count fits usize");
        let hole_distance = (cursor + len - hole) % len;
        let origin_distance = (cursor + len - origin) % len;
        if origin_distance >= hole_distance {
            slots[hole] = slots[cursor].take();
            moved(cursor, hole);
            hole = cursor;
        }
        cursor = (cursor + 1) % len;
    }
    (removed, hole)
}

fn write_hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn write_migration_object_hash(
    hash: &mut u64,
    tag: u8,
    (_, stable_id, type_id, generation): (u64, StableId, StableId, u32),
) {
    write_hash(hash, &[tag]);
    write_hash(hash, &stable_id.0.to_le_bytes());
    write_hash(hash, &type_id.0.to_le_bytes());
    write_hash(hash, &generation.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use nexa_core::{CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS, StableId};

    use super::{
        CollectionStorage, CollectionView, GcBudget, GcRoots, Heap, HeapError, MapEntry,
        MapSetOutcome, Object, SetInsertOutcome, fnv_content_hash, insert_map_entry,
        remove_probed_entry,
    };
    use crate::{RuntimeFailurePoint, RuntimeValue};

    fn probe_entry(key: i32, hash: u64) -> MapEntry {
        MapEntry {
            key: RuntimeValue::I32(key),
            hash,
        }
    }

    #[test]
    fn probe_backshift_removal_shifts_wrapping_clusters_back() {
        // Five slots, three entries all homed at slot 3: the cluster wraps
        // through (3, 4, 0). Removing the head must pull both successors
        // back so a probe from home still reaches them before a hole.
        let mut slots = vec![None; 5];
        insert_map_entry(&mut slots, probe_entry(1, 3)).unwrap();
        insert_map_entry(&mut slots, probe_entry(2, 3)).unwrap();
        insert_map_entry(&mut slots, probe_entry(3, 3)).unwrap();
        assert_eq!(remove_probed_entry(&mut slots, 3).key, RuntimeValue::I32(1));
        assert_eq!(slots[3].unwrap().key, RuntimeValue::I32(2));
        assert_eq!(slots[4].unwrap().key, RuntimeValue::I32(3));
        assert!(slots[0].is_none());
    }

    #[test]
    fn probe_backshift_removal_never_moves_entries_before_their_home() {
        // The home-4 entry already sits at its home slot; removing the
        // home-3 entry must not drag it backwards off its own chain.
        let mut slots = vec![None; 5];
        insert_map_entry(&mut slots, probe_entry(1, 3)).unwrap();
        insert_map_entry(&mut slots, probe_entry(2, 4)).unwrap();
        assert_eq!(remove_probed_entry(&mut slots, 3).key, RuntimeValue::I32(1));
        assert!(slots[3].is_none());
        assert_eq!(slots[4].unwrap().key, RuntimeValue::I32(2));
    }

    #[test]
    fn probe_backshift_removal_terminates_on_full_tables() {
        // A table pinned at the capacity ceiling can run with zero empty
        // slots; removal must stop after one full cycle and keep every
        // survivor reachable from its home.
        let mut slots = vec![None; 3];
        insert_map_entry(&mut slots, probe_entry(1, 0)).unwrap();
        insert_map_entry(&mut slots, probe_entry(2, 0)).unwrap();
        insert_map_entry(&mut slots, probe_entry(3, 0)).unwrap();
        assert_eq!(remove_probed_entry(&mut slots, 0).key, RuntimeValue::I32(1));
        assert_eq!(slots[0].unwrap().key, RuntimeValue::I32(2));
        assert_eq!(slots[1].unwrap().key, RuntimeValue::I32(3));
        assert!(slots[2].is_none());
    }

    #[test]
    fn map_lookups_stay_correct_across_removal_churn() {
        // K3 integration: interleaved removals and re-inserts must keep
        // every surviving key reachable through the probe chains the
        // lookup side now walks (a broken chain shows up as a false miss).
        let mut heap = Heap::new_with_limits(8, usize::MAX, 512);
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        let set = |heap: &mut Heap, key: i32, value: i32| {
            while heap
                .map_set(map, RuntimeValue::I32(key), RuntimeValue::I32(value))
                .unwrap()
                == MapSetOutcome::RehashPending
            {}
        };
        for key in 0..64 {
            set(&mut heap, key, key * 10);
        }
        for key in (0..64).step_by(2) {
            assert_eq!(
                heap.map_remove(map, RuntimeValue::I32(key)).unwrap(),
                Some(RuntimeValue::I32(key * 10))
            );
        }
        for key in 0..64 {
            let expected = (key % 2 == 1).then_some(RuntimeValue::I32(key * 10));
            assert_eq!(heap.map_get(map, RuntimeValue::I32(key)).unwrap(), expected);
        }
        // Re-inserting into the holes must keep both generations of
        // entries reachable, including across any rehash this triggers.
        for key in (0..64).step_by(2) {
            set(&mut heap, key, key + 1000);
        }
        for key in 0..64 {
            let expected = if key % 2 == 0 { key + 1000 } else { key * 10 };
            assert_eq!(
                heap.map_get(map, RuntimeValue::I32(key)).unwrap(),
                Some(RuntimeValue::I32(expected))
            );
        }
        assert_eq!(heap.map_len(map).unwrap(), 64);
    }

    #[test]
    fn scalar_map_values_use_exact_width_companion_storage() {
        let mut heap = Heap::new_with_limits(32, usize::MAX, 64);
        let map_type = nexa_bytecode::map_type(
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::Bool,
        );
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::Bool,
            )
            .unwrap();

        let storage = heap.map_storage_index(map).unwrap();
        let header = heap.maps[storage].as_ref().unwrap();
        assert_eq!(header.value_storage, CollectionStorage::Bool);
        assert_eq!(heap.live_collection_bytes(), header.storage_bytes() as u64);
        assert_eq!(
            header.values.length * header.value_storage.cell_size(),
            header.slots.length,
            "bool companions occupy one byte per map slot"
        );

        for key in 0..20 {
            while heap
                .map_set(
                    map,
                    RuntimeValue::I32(key),
                    RuntimeValue::Bool(key % 2 == 0),
                )
                .unwrap()
                == MapSetOutcome::RehashPending
            {}
        }
        for key in 0..20 {
            assert_eq!(
                heap.map_get(map, RuntimeValue::I32(key)).unwrap(),
                Some(RuntimeValue::Bool(key % 2 == 0))
            );
        }
        for key in (0..20).step_by(3) {
            assert_eq!(
                heap.map_remove(map, RuntimeValue::I32(key)).unwrap(),
                Some(RuntimeValue::Bool(key % 2 == 0))
            );
        }

        let header = heap.maps[storage].as_ref().unwrap();
        assert_eq!(header.value_storage, CollectionStorage::Bool);
        assert_eq!(
            heap.live_collection_bytes(),
            header.storage_bytes() as u64,
            "rehash completion keeps exact compact G6 accounting"
        );
    }

    #[test]
    fn compact_string_map_values_are_precisely_traced_across_rehash() {
        let mut heap = Heap::new_with_limits(64, usize::MAX, 64);
        let map_type = nexa_bytecode::map_type(
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::String,
        );
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::String,
            )
            .unwrap();
        let RuntimeValue::NamedRef {
            reference: map_root,
            ..
        } = map
        else {
            panic!("map allocation returns a named reference");
        };
        let mut strings = Vec::new();
        for key in 0..20 {
            let text = format!("value-{key}");
            let reference = heap.allocate_string(&text).unwrap();
            strings.push(reference);
            let value = RuntimeValue::String {
                reference,
                hash: heap.string_hash(reference).unwrap(),
            };
            while heap.map_set(map, RuntimeValue::I32(key), value).unwrap()
                == MapSetOutcome::RehashPending
            {}
        }
        let storage = heap.map_storage_index(map).unwrap();
        assert_eq!(
            heap.maps[storage].as_ref().unwrap().value_storage,
            CollectionStorage::String
        );

        let roots = GcRoots {
            running_frames: vec![map_root],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 21);
        for reference in &strings {
            assert!(heap.resolve(*reference).is_ok());
        }

        let removed = strings[7];
        assert!(
            heap.map_remove(map, RuntimeValue::I32(7))
                .unwrap()
                .is_some()
        );
        assert_eq!(heap.collect(&roots).unwrap().reclaimed, 1);
        assert!(heap.resolve(removed).is_err());
        for (index, reference) in strings.iter().copied().enumerate() {
            if index != 7 {
                assert!(heap.resolve(reference).is_ok());
            }
        }
    }

    #[test]
    fn verified_named_reference_map_values_use_compact_reference_storage() {
        let class_type = StableId::from_name("test.CompactMapClass");
        let mut module = nexa_bytecode::ModuleBuilder::new();
        module.metadata(
            StableId::from_name("test.compact-map-host"),
            nexa_bytecode::StateSchema::default().fingerprint(),
        );
        module.class_type(nexa_bytecode::ClassType {
            type_id: class_type,
            fields: Vec::new(),
        });
        let module = module.finish();
        let layout = nexa_bytecode::layout::LayoutTable::for_module(&module)
            .unwrap()
            .layout_of(nexa_bytecode::ValueType::Named(class_type))
            .unwrap();

        let mut heap = Heap::new_with_limits(16, usize::MAX, 32);
        let map_type = nexa_bytecode::map_type(
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::Named(class_type),
        );
        let map = heap
            .allocate_physical_map_with_layout(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::Named(class_type),
                1,
                &layout,
            )
            .unwrap();
        let class = heap.allocate_class(class_type, &[]).unwrap();
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(1), class),
            Ok(MapSetOutcome::Complete)
        );
        assert_eq!(heap.map_get(map, RuntimeValue::I32(1)), Ok(Some(class)));

        let storage = heap.map_storage_index(map).unwrap();
        assert_eq!(
            heap.maps[storage].as_ref().unwrap().value_storage,
            CollectionStorage::NamedRef
        );
        let RuntimeValue::NamedRef {
            reference: map_root,
            ..
        } = map
        else {
            panic!("map allocation returns a named reference");
        };
        let RuntimeValue::NamedRef {
            reference: class_reference,
            ..
        } = class
        else {
            panic!("class allocation returns a named reference");
        };
        let roots = GcRoots {
            running_frames: vec![map_root],
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 2);
        assert!(heap.resolve(class_reference).is_ok());
    }

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
    fn live_object_gauge_excludes_generation_exhausted_slots() {
        let mut heap = Heap::new(1);
        let value = heap.allocate(Object::String("retired".into())).unwrap();
        heap.slots[value.index as usize].generation = u32::MAX;
        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.live, 0);
        assert_eq!(heap.live_len(), 0);
        assert!(
            heap.free.is_empty(),
            "an exhausted slot cannot return to the reusable free list"
        );
        assert_eq!(
            heap.allocate(Object::String("replacement".into())),
            Err(HeapError::CapacityExhausted)
        );
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
    fn string_copy_capacity_is_checked_before_owned_result_allocation() {
        let mut heap = Heap::new_with_string_limit(1, 64);
        let source = heap.allocate_string("  rooted  ").unwrap();
        assert_eq!(
            heap.copy_string_range(source, 2, 8),
            Err(HeapError::CapacityExhausted)
        );
        assert_eq!(heap.trim_string(source), Err(HeapError::CapacityExhausted));
        assert_eq!(heap.live_len(), 1);
        assert_eq!(heap.string(source), Ok("  rooted  "));
    }

    #[test]
    fn string_split_byte_limit_is_preflighted_before_any_vm_publication() {
        let mut heap = Heap::new(16);
        let source = heap.allocate_string("a,b,c").unwrap();
        let delimiter = heap.allocate_string(",").unwrap();
        let before_objects = heap.live_len();
        let before_bytes = heap.live_vm_bytes();
        let before_payload = heap.live_payload_bytes();
        let header_bytes = heap.byte_inspection().object_header_bytes / 2;
        heap.set_max_heap_bytes(before_bytes.saturating_add(header_bytes).saturating_add(1));

        assert_eq!(
            heap.split_string(source, delimiter),
            Err(HeapError::CapacityExhausted)
        );
        assert_eq!(heap.live_len(), before_objects);
        assert_eq!(heap.live_vm_bytes(), before_bytes);
        assert_eq!(heap.live_payload_bytes(), before_payload);
        assert_eq!(heap.live_collection_bytes(), 0);
        assert_eq!(heap.string(source), Ok("a,b,c"));
        assert_eq!(heap.string(delimiter), Ok(","));
    }

    #[test]
    fn allocation_failure_does_not_drop_live_objects() {
        let mut heap = Heap::new(2);
        let live = heap.allocate(Object::String("live".into())).unwrap();
        let _probe = heap
            .failure_injector()
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
    fn string_literal_cache_shares_live_copies_and_survives_collection() {
        let mut heap = Heap::new(8);
        let (first, first_hash) = heap.load_string_literal_with_hash("pooled").unwrap();
        let (second, second_hash) = heap.load_string_literal_with_hash("pooled").unwrap();
        assert_eq!(first, second, "hot literal loads share one object");
        assert_eq!(heap.vm_allocation_counters().string_allocations, 1);
        // WP69: the cached hash is the interning-time content hash.
        assert_eq!(first_hash, heap.string_hash(first).unwrap());
        assert_eq!(first_hash, second_hash);

        // Cache entries are not roots: an unrooted literal is collected,
        // and the next load safely re-allocates instead of resurrecting.
        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.reclaimed, 1);
        let third = heap.load_string_literal("pooled").unwrap();
        assert_ne!(first, third, "collected entries fall back to allocation");
        assert_eq!(heap.string(third), Ok("pooled"));
    }

    #[test]
    fn executable_string_pool_reuses_module_bytes_and_cache_slots() {
        let mut heap = Heap::new_with_limits(4, 64, 4);
        let shared = std::sync::Arc::<str>::from("module-owned");
        let hash = super::fnv_content_hash(&shared);
        let first = heap
            .load_pooled_string(7, 3, std::sync::Arc::clone(&shared), hash)
            .unwrap();
        let second = heap
            .load_pooled_string(7, 3, std::sync::Arc::clone(&shared), hash)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(heap.string(first.0), Ok("module-owned"));
        assert_eq!(heap.vm_allocation_counters().string_allocations, 0);
        assert_eq!(heap.pooled_string_cache.len(), 1);

        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 1);
        let reloaded = heap
            .load_pooled_string(7, 3, shared, hash)
            .expect("stale cache entry is replaced in place");
        assert_ne!(first.0.generation, reloaded.0.generation);
        assert_eq!(heap.pooled_string_cache.len(), 1);
    }

    #[test]
    fn string_literal_cache_survives_generation_reuse_after_host_rollback() {
        // Host rollback frees slots without bumping their generation; a
        // literal interned inside the transaction must not alias whatever
        // is committed into the recycled slot afterwards.
        let mut heap = Heap::new(8);
        heap.begin_host_transaction().unwrap();
        let staged = heap.load_string_literal("staged-literal").unwrap();
        heap.rollback_host_transaction();
        let replacement = heap.allocate_string("different-content").unwrap();
        assert_eq!(
            (staged.index, staged.generation),
            (replacement.index, replacement.generation),
            "the rollback recycles the staged slot at the same generation"
        );
        let reloaded = heap.load_string_literal("staged-literal").unwrap();
        assert_ne!(
            reloaded, replacement,
            "a stale cache entry must not alias the recycled slot"
        );
        assert_eq!(heap.string(reloaded), Ok("staged-literal"));
        assert_eq!(heap.string(replacement), Ok("different-content"));
    }

    #[test]
    fn host_transaction_staging_is_an_incremental_gc_root() {
        let mut heap = Heap::new(8);
        heap.begin_host_transaction().unwrap();
        let staged = heap.allocate_string("staged-host-result").unwrap();
        let roots = GcRoots {
            staging_heap: heap.host_staging_roots().to_vec(),
            ..GcRoots::default()
        };
        for _ in 0..32 {
            if heap
                .collect_incremental(&roots, GcBudget::objects(1))
                .unwrap()
                .completed
                .is_some()
            {
                break;
            }
        }

        assert!(heap.resolve(staged).is_ok());

        heap.commit_host_transaction();
        let mut reclaimed = None;
        for _ in 0..32 {
            if let Some(stats) = heap
                .collect_incremental(&GcRoots::default(), GcBudget::objects(1))
                .unwrap()
                .completed
            {
                reclaimed = Some(stats.reclaimed);
                break;
            }
        }
        assert_eq!(reclaimed, Some(1));
        assert!(heap.resolve(staged).is_err());
    }

    #[test]
    fn map_values_use_contiguous_physical_rows_across_rehash_and_removal() {
        let mut heap = Heap::new_with_limits(32, usize::MAX, 32);
        let pair = nexa_core::StableId::from_name("test.Pair");
        let map_type = nexa_bytecode::map_type(
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::Named(pair),
        );
        let map = heap
            .allocate_physical_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::Named(pair),
                2,
            )
            .unwrap();
        for key in 0..13 {
            let row = [
                RuntimeValue::I32(key),
                RuntimeValue::I64(i64::from(key) * 10),
            ];
            while heap
                .map_set_value_range(map, RuntimeValue::I32(key), &row)
                .unwrap()
                == MapSetOutcome::RehashPending
            {}
        }
        let mut row = [RuntimeValue::Unit; 2];
        assert!(
            heap.map_get_value_into(map, RuntimeValue::I32(9), &mut row)
                .unwrap()
        );
        assert_eq!(row, [RuntimeValue::I32(9), RuntimeValue::I64(90)]);

        let replacement = [RuntimeValue::I32(90), RuntimeValue::I64(900)];
        assert_eq!(
            heap.map_set_value_range(map, RuntimeValue::I32(9), &replacement),
            Ok(MapSetOutcome::Complete)
        );
        assert!(
            heap.map_remove_value_into(map, RuntimeValue::I32(9), &mut row)
                .unwrap()
        );
        assert_eq!(row, replacement);
        assert!(
            !heap
                .map_get_value_into(map, RuntimeValue::I32(9), &mut row)
                .unwrap()
        );
        assert_eq!(heap.map_len(map), Ok(12));
    }

    #[test]
    fn string_literal_cache_survives_checkpoint_restore() {
        // Restore replaces the slot population wholesale; entries cached
        // after the checkpoint may match a restored generation with
        // different content and must be dropped with it.
        let mut heap = Heap::new(8);
        let checkpoint = heap.checkpoint();
        let cached = heap.load_string_literal("transient").unwrap();
        heap.restore_checkpoint(checkpoint);
        let replacement = heap.allocate_string("occupies-the-slot").unwrap();
        assert_eq!(
            (cached.index, cached.generation),
            (replacement.index, replacement.generation),
            "the restored heap hands out the same slot and generation"
        );
        let reloaded = heap.load_string_literal("transient").unwrap();
        assert_ne!(
            reloaded, replacement,
            "a post-checkpoint cache entry must not survive the restore"
        );
        assert_eq!(heap.string(reloaded), Ok("transient"));
    }

    #[test]
    fn vm_allocation_counters_track_kind_relocation_and_survive_restore() {
        let mut heap = Heap::new_with_limits(16, usize::MAX, 8);
        let checkpoint = heap.checkpoint();
        heap.allocate_string("hello").unwrap();
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        heap.array_push(array, RuntimeValue::I32(1)).unwrap();
        heap.array_push(array, RuntimeValue::I32(2)).unwrap();

        let counters = heap.vm_allocation_counters();
        assert_eq!(counters.string_allocations, 1);
        assert_eq!(counters.string_copy_bytes, 5);
        assert_eq!(counters.collection_storage_allocations, 1);
        assert_eq!(counters.object_allocations, 2);
        assert!(counters.allocated_bytes > counters.string_copy_bytes);
        assert_eq!(counters.host_codec_copy_bytes, 0);
        // WP49 amortized growth: the first push grows an empty extent
        // (zero live elements copied) and the second lands in spare
        // capacity, so no relocation bytes accrue at all.
        assert_eq!(counters.collection_relocation_bytes, 0);

        // Counters are monotonic work totals: rollback keeps them.
        heap.restore_checkpoint(checkpoint);
        assert_eq!(heap.vm_allocation_counters(), counters);
    }

    #[test]
    fn checkpoints_copy_live_prefixes_and_restore_into_reserved_arenas() {
        let mut heap = Heap::new_with_arena_limits(32, 4_096, 16, 1_024, 64);
        let array = heap
            .allocate_array(
                nexa_bytecode::array_type(nexa_bytecode::ValueType::I32),
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        heap.array_push(array, RuntimeValue::I32(7)).unwrap();
        let reserved = heap.scalar_collections.i32().capacity();
        assert_eq!(
            heap.scalar_collections.backing_allocations(),
            1,
            "all compact scalar types share one allocator backing"
        );
        let checkpoint = heap.checkpoint();
        assert_eq!(reserved, 1_024);
        assert!(
            checkpoint.scalar_collections.i32().capacity() < reserved,
            "checkpoint owns only the initialized prefix, not the reserved arena"
        );
        assert!(
            checkpoint.collections.values.capacity() < heap.collections.values.capacity(),
            "generic arena slack is not cloned into a transactional snapshot"
        );

        heap.array_push(array, RuntimeValue::I32(8)).unwrap();
        heap.restore_checkpoint(checkpoint);
        assert_eq!(heap.array_len(array), Ok(1));
        assert_eq!(heap.array_get(array, 0), Ok(RuntimeValue::I32(7)));
        assert_eq!(
            heap.scalar_collections.i32().capacity(),
            reserved,
            "restore reuses the heap's constructor-reserved backing"
        );
    }

    #[test]
    fn struct_row_arrays_store_zero_objects_per_element() {
        // WP52 structural gate: N pushed elements leave exactly one heap
        // object (the array itself) after the transient sources die.
        let mut heap = Heap::new_with_limits(64, usize::MAX, 64);
        let record = StableId::from_name("heap-test::RowRecord");
        let element = nexa_bytecode::ValueType::Named(record);
        let array_type = nexa_bytecode::array_type(element);
        let array = heap
            .allocate_value_row_array(
                array_type,
                element,
                std::num::NonZeroU16::new(2).expect("non-zero"),
            )
            .unwrap();
        for index in 0..8_i32 {
            let label = heap.allocate_string("row-label").unwrap();
            let hash = heap.string_hash(label).unwrap();
            let source = heap
                .allocate_struct(
                    record,
                    &[
                        RuntimeValue::I32(index),
                        RuntimeValue::String {
                            reference: label,
                            hash,
                        },
                    ],
                )
                .unwrap();
            heap.array_push(array, source).unwrap();
        }
        assert_eq!(heap.array_len(array), Ok(8));

        // Only the array is rooted: every pushed struct source dies, the
        // row storage and its string field references survive.
        let RuntimeValue::NamedRef { reference, .. } = array else {
            panic!("arrays are named references");
        };
        let mut roots = GcRoots::default();
        roots.running_frames.push(reference);
        heap.collect(&roots).unwrap();
        assert_eq!(
            heap.live_len(),
            1 + 8,
            "one array object plus eight row label strings"
        );

        // Reads materialize equal transient values from the rows.
        let first = heap.array_get(array, 0).unwrap();
        let fields = heap.struct_fields(first).unwrap();
        assert_eq!(fields.get(0), Some(RuntimeValue::I32(0)));
        let RuntimeValue::String {
            reference: label, ..
        } = fields.get(1).expect("second row field")
        else {
            panic!("label field stays a string reference");
        };
        assert_eq!(heap.string(label), Ok("row-label"));

        // The borrowed views agree with the materialized read.
        assert_eq!(
            heap.array_element_fields(array, 0).unwrap().get(0),
            Some(RuntimeValue::I32(0))
        );
        let view = heap.array_rows(array).unwrap().expect("row layout");
        assert_eq!(
            (view.cells.len(), view.stride, view.struct_type),
            (16, 2, record)
        );
        assert!(
            heap.array_values(array).is_err(),
            "row arrays have no one-cell-per-element view"
        );
    }

    #[test]
    fn struct_row_arrays_keep_logical_semantics_across_mutations() {
        fn make(heap: &mut Heap, record: StableId, value: i32) -> RuntimeValue {
            heap.allocate_struct(record, &[RuntimeValue::I32(value)])
                .unwrap()
        }
        let mut heap = Heap::new_with_limits(64, usize::MAX, 16);
        let record = StableId::from_name("heap-test::RowMutation");
        let element = nexa_bytecode::ValueType::Named(record);
        let array_type = nexa_bytecode::array_type(element);
        let array = heap
            .allocate_value_row_array(
                array_type,
                element,
                std::num::NonZeroU16::new(1).expect("non-zero"),
            )
            .unwrap();
        let first = make(&mut heap, record, 10);
        heap.array_push(array, first).unwrap();
        let second = make(&mut heap, record, 20);
        heap.array_push(array, second).unwrap();
        let inserted = make(&mut heap, record, 5);
        heap.array_insert(array, 0, inserted).unwrap();
        // [5, 10, 20]
        let replacement = make(&mut heap, record, 11);
        heap.array_set(array, 1, replacement).unwrap();
        // [5, 11, 20]
        let removed = heap.array_remove(array, 0).unwrap();
        assert_eq!(
            heap.struct_fields(removed).unwrap().get(0),
            Some(RuntimeValue::I32(5))
        );
        let popped = heap.array_pop(array).unwrap();
        assert_eq!(
            heap.struct_fields(popped).unwrap().get(0),
            Some(RuntimeValue::I32(20))
        );
        assert_eq!(heap.array_len(array), Ok(1));
        assert_eq!(
            heap.array_element_fields(array, 0).unwrap().get(0),
            Some(RuntimeValue::I32(11))
        );
        // Type confusion is rejected: a struct of another type cannot
        // enter the rows.
        let alien_type = StableId::from_name("heap-test::OtherRecord");
        let alien = heap
            .allocate_struct(alien_type, &[RuntimeValue::I32(1)])
            .unwrap();
        assert!(heap.array_push(array, alien).is_err());
        // A materialized element equals what an identical construction
        // produces (structural equality).
        let expected = make(&mut heap, record, 11);
        let read = heap.array_get(array, 0).unwrap();
        assert_eq!(heap.struct_equal(read, expected), Ok(true));
        heap.array_clear(array).unwrap();
        assert_eq!(heap.array_len(array), Ok(0));
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
    fn array_capacity_management_preserves_typed_storage_and_live_prefix() {
        let mut heap = Heap::new_with_arena_limits(8, 4_096, 64, 64, 16);
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();

        heap.array_reserve(array, 3).unwrap();
        assert_eq!(heap.array_capacity(array), Ok(4));
        for value in [10, 20, 30] {
            heap.array_push(array, RuntimeValue::I32(value)).unwrap();
        }

        // Occupy the adjacent arena range so the next reserve exercises the
        // bounded relocation fallback instead of the in-place extension.
        let blocker = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        heap.array_reserve(blocker, 4).unwrap();
        let relocation_before = heap.vm_allocation_counters().collection_relocation_bytes;
        heap.array_reserve(array, 6).unwrap();
        assert_eq!(heap.array_capacity(array), Ok(9));
        assert_eq!(
            heap.vm_allocation_counters().collection_relocation_bytes - relocation_before,
            3 * std::mem::size_of::<i32>() as u64
        );
        assert_eq!(heap.array_get(array, 0), Ok(RuntimeValue::I32(10)));
        assert_eq!(heap.array_get(array, 1), Ok(RuntimeValue::I32(20)));
        assert_eq!(heap.array_get(array, 2), Ok(RuntimeValue::I32(30)));

        heap.array_clear(array).unwrap();
        assert_eq!(heap.array_len(array), Ok(0));
        assert_eq!(heap.array_capacity(array), Ok(9));
        let relocation_before = heap.vm_allocation_counters().collection_relocation_bytes;
        heap.array_shrink_to_fit(array).unwrap();
        assert_eq!(heap.array_capacity(array), Ok(0));
        assert_eq!(
            heap.vm_allocation_counters().collection_relocation_bytes,
            relocation_before,
            "tail-split shrink must not copy elements"
        );
    }

    #[test]
    fn scalar_arrays_use_typed_cells_and_share_one_exact_quota() {
        let mut heap = Heap::new_with_arena_limits(16, 4_096, 8, 24, 32);
        let cases = [
            (nexa_bytecode::ValueType::I32, RuntimeValue::I32(-7)),
            (nexa_bytecode::ValueType::I64, RuntimeValue::I64(-9)),
            (
                nexa_bytecode::ValueType::F32,
                RuntimeValue::F32(1.5_f32.to_bits()),
            ),
            (
                nexa_bytecode::ValueType::F64,
                RuntimeValue::F64((-2.25_f64).to_bits()),
            ),
            (nexa_bytecode::ValueType::Bool, RuntimeValue::Bool(true)),
            (
                nexa_bytecode::ValueType::Rune,
                RuntimeValue::Rune('界' as u32),
            ),
        ];
        let mut arrays = Vec::new();
        for (element_type, value) in cases {
            let array = heap
                .allocate_array(nexa_bytecode::array_type(element_type), element_type)
                .unwrap();
            heap.array_push(array, value).unwrap();
            assert_eq!(heap.array_get(array, 0), Ok(value));
            let view = heap.array_values(array).unwrap();
            assert_eq!(view.len(), 1);
            assert_eq!(view.get(0), Some(value));
            arrays.push(array);
        }
        // Each first push claims the bounded geometric minimum of four
        // cells. The global quota is shared across physical arenas, so a
        // seventh independent scalar array cannot silently consume another
        // per-type capacity pool.
        let extra = heap
            .allocate_array(
                nexa_bytecode::array_type(nexa_bytecode::ValueType::I32),
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        assert_eq!(
            heap.array_push(extra, RuntimeValue::I32(1)),
            Err(HeapError::CapacityExhausted)
        );

        let roots = GcRoots {
            running_frames: arrays
                .iter()
                .chain(std::iter::once(&extra))
                .filter_map(|value| match value {
                    RuntimeValue::NamedRef { reference, .. } => Some(*reference),
                    _ => None,
                })
                .collect(),
            ..GcRoots::default()
        };
        assert_eq!(heap.collect(&roots).unwrap().live, 7);
    }

    #[test]
    fn homogeneous_object_fields_use_exact_width_cells_and_trace_references() {
        let mut heap = Heap::new_with_limits(16, 128, 16);
        let scalar_type = StableId::from_name("heap-test::CompactScalarClass");
        let scalar = heap
            .allocate_class(scalar_type, &[RuntimeValue::I32(7), RuntimeValue::I32(9)])
            .unwrap();
        assert!(matches!(
            heap.class_fields(scalar).unwrap(),
            CollectionView::I32(&[7, 9])
        ));
        assert_eq!(
            heap.byte_inspection().class_payload_bytes,
            2 * size_of::<i32>() as u64
        );

        let text = heap.allocate_string("compact-root").unwrap();
        let text_value = RuntimeValue::String {
            reference: text,
            hash: fnv_content_hash("compact-root"),
        };
        let string_type = StableId::from_name("heap-test::CompactStringClass");
        let string_owner = heap.allocate_class(string_type, &[text_value]).unwrap();
        assert!(matches!(
            heap.class_fields(string_owner).unwrap(),
            CollectionView::String(values) if values == [(text, fnv_content_hash("compact-root"))]
        ));
        let RuntimeValue::NamedRef {
            reference: owner, ..
        } = string_owner
        else {
            panic!("class is a named reference");
        };
        let mut roots = GcRoots::default();
        roots.running_frames.push(owner);
        heap.collect(&roots).unwrap();
        assert_eq!(heap.string(text), Ok("compact-root"));
    }

    #[test]
    fn named_reference_arrays_use_eight_byte_cells_and_trace_enum_graphs() {
        let mut heap = Heap::new_with_arena_limits(16, 4_096, 8, 16, 32);
        let enum_type = StableId::from_name("heap-test::CompactEnum");
        let variant = StableId::from_name("heap-test::CompactEnum::Some");
        let payload = heap.allocate_string("payload").unwrap();
        let payload = RuntimeValue::String {
            reference: payload,
            hash: heap.string_hash(payload).unwrap(),
        };
        let value = heap
            .allocate_enum(enum_type, variant, 1, Some(payload))
            .unwrap();
        let array = heap
            .allocate_array(
                nexa_bytecode::array_type(nexa_bytecode::ValueType::Named(enum_type)),
                nexa_bytecode::ValueType::Named(enum_type),
            )
            .unwrap();
        heap.array_push(array, value).unwrap();
        assert_eq!(heap.array_get(array, 0), Ok(value));
        assert_eq!(
            heap.byte_inspection().array_bytes,
            4 * size_of::<super::GcRef>() as u64,
            "geometric capacity is physically eight bytes per enum reference"
        );

        let RuntimeValue::NamedRef {
            reference: array_root,
            ..
        } = array
        else {
            panic!("array is a named reference")
        };
        let roots = GcRoots {
            running_frames: vec![array_root],
            ..GcRoots::default()
        };
        assert_eq!(
            heap.collect(&roots).unwrap().live,
            3,
            "array -> enum -> string payload remains precisely traced"
        );
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
    #[allow(clippy::too_many_lines)]
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
            heap.buffer_values(destination)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            vec![
                RuntimeValue::I32(6),
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
                RuntimeValue::I32(4),
            ]
        );
        assert_eq!(
            heap.buffer_values(source)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            vec![
                RuntimeValue::I32(9),
                RuntimeValue::I32(8),
                RuntimeValue::I32(7),
            ]
        );

        let slice = heap.buffer_slice(destination, 1, 2).unwrap();
        heap.buffer_set(slice, 0, RuntimeValue::I32(5)).unwrap();
        assert_eq!(heap.buffer_get(slice, 0), Ok(RuntimeValue::I32(5)));
        assert_eq!(heap.buffer_get(destination, 1), Ok(RuntimeValue::I32(9)));

        let before = heap
            .buffer_values(destination)
            .unwrap()
            .iter()
            .collect::<Vec<_>>();
        assert_eq!(
            heap.buffer_copy(destination, source, 2, 0, 2),
            Err(HeapError::IndexOutOfBounds {
                index: 4,
                length: 3,
            })
        );
        assert_eq!(
            heap.buffer_values(destination)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            before
        );
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

        let mut full_heap = Heap::new_with_arena_limits(2, 64, 4, 16, 4);
        let source = full_heap
            .allocate_buffer(
                type_id,
                element,
                &[RuntimeValue::I32(1), RuntimeValue::I32(2)],
            )
            .unwrap();
        full_heap.allocate_string("full").unwrap();
        let before = full_heap.collection_inspection();
        assert_eq!(
            full_heap.buffer_slice(source, 0, 1),
            Err(HeapError::CapacityExhausted)
        );
        assert_eq!(full_heap.collection_inspection(), before);
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
    fn map_slot_arena_rehashes_and_recycles_without_growing() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 32);
        let arena_pointer = heap.map_slots.values.as_ptr();
        let arena_capacity = heap.map_slots.values.capacity();
        let arena_limit = heap
            .map_slots
            .free_ranges
            .iter()
            .map(|range| range.length)
            .sum::<usize>();
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        for key in 0..24 {
            while heap
                .map_set(map, RuntimeValue::I32(key), RuntimeValue::I32(key))
                .unwrap()
                == MapSetOutcome::RehashPending
            {}
        }
        assert_eq!(heap.map_slots.values.as_ptr(), arena_pointer);
        assert_eq!(heap.map_slots.values.capacity(), arena_capacity);
        let RuntimeValue::NamedRef { reference, .. } = map else {
            unreachable!("map allocations are named references")
        };
        assert_eq!(heap.collect(&GcRoots::default()).unwrap().reclaimed, 1);
        assert_eq!(
            heap.resolve(reference),
            Err(HeapError::InvalidReference(reference))
        );
        assert_eq!(
            heap.map_slots
                .free_ranges
                .iter()
                .map(|range| range.length)
                .sum::<usize>(),
            arena_limit
        );
    }

    #[test]
    fn host_rollback_releases_map_slot_extents() {
        let mut heap = Heap::new_with_limits(4, usize::MAX, 16);
        let arena_limit = heap
            .map_slots
            .free_ranges
            .iter()
            .map(|range| range.length)
            .sum::<usize>();
        heap.begin_host_transaction().unwrap();
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        heap.allocate_map(
            map_type,
            nexa_bytecode::ValueType::I32,
            nexa_bytecode::ValueType::I32,
        )
        .unwrap();
        assert!(heap.live_collection_bytes() > 0);
        heap.rollback_host_transaction();
        assert!(heap.maps.iter().all(Option::is_none));
        assert_eq!(heap.live_collection_bytes(), 0);
        assert_eq!(heap.live_payload_bytes(), 0);
        assert_eq!(
            heap.map_slots
                .free_ranges
                .iter()
                .map(|range| range.length)
                .sum::<usize>(),
            arena_limit
        );
    }

    #[test]
    fn float_signed_zero_preserves_struct_and_map_hash_contracts() {
        let mut heap = Heap::new_with_limits(4, usize::MAX, 4);
        let struct_type = StableId::from_name("FloatPair");
        let positive_zero = heap
            .allocate_struct(
                struct_type,
                &[
                    RuntimeValue::F32(0.0_f32.to_bits()),
                    RuntimeValue::F64(0.0_f64.to_bits()),
                ],
            )
            .unwrap();
        let negative_zero = heap
            .allocate_struct(
                struct_type,
                &[
                    RuntimeValue::F32((-0.0_f32).to_bits()),
                    RuntimeValue::F64((-0.0_f64).to_bits()),
                ],
            )
            .unwrap();

        assert_eq!(heap.struct_equal(positive_zero, negative_zero), Ok(true));

        let key_type = nexa_bytecode::ValueType::Named(struct_type);
        let map_type = nexa_bytecode::map_type(key_type, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(map_type, key_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        assert_eq!(
            heap.map_set(map, positive_zero, RuntimeValue::I32(7)),
            Ok(MapSetOutcome::Complete)
        );
        assert_eq!(
            heap.map_get(map, negative_zero),
            Ok(Some(RuntimeValue::I32(7)))
        );

        assert_eq!(
            heap.runtime_value_equal(
                RuntimeValue::F32(CANONICAL_NAN_F32_BITS),
                RuntimeValue::F32(CANONICAL_NAN_F32_BITS),
            ),
            Ok(false)
        );
        assert_eq!(
            heap.runtime_value_equal(
                RuntimeValue::F64(CANONICAL_NAN_F64_BITS),
                RuntimeValue::F64(CANONICAL_NAN_F64_BITS),
            ),
            Ok(false)
        );
    }

    #[test]
    fn enum_equality_recurses_into_payloads() {
        let mut heap = Heap::new(6);
        let inner_type = nexa_bytecode::option_type(nexa_bytecode::ValueType::F32);
        let some = StableId::from_parts(&["Option", "::Some"]);
        let lhs_inner = heap
            .allocate_enum(
                inner_type.type_id,
                some,
                1,
                Some(RuntimeValue::F32(0.0_f32.to_bits())),
            )
            .unwrap();
        let rhs_inner = heap
            .allocate_enum(
                inner_type.type_id,
                some,
                1,
                Some(RuntimeValue::F32((-0.0_f32).to_bits())),
            )
            .unwrap();
        let outer_type =
            nexa_bytecode::option_type(nexa_bytecode::ValueType::Named(inner_type.type_id));
        let lhs = heap
            .allocate_enum(outer_type.type_id, some, 1, Some(lhs_inner))
            .unwrap();
        let rhs = heap
            .allocate_enum(outer_type.type_id, some, 1, Some(rhs_inner))
            .unwrap();
        assert_eq!(heap.enum_equal(lhs, rhs), Ok(true));

        let nan = heap
            .allocate_enum(
                inner_type.type_id,
                some,
                1,
                Some(RuntimeValue::F32(CANONICAL_NAN_F32_BITS)),
            )
            .unwrap();
        assert_eq!(heap.enum_equal(nan, nan), Ok(false));
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

    fn i32_set(heap: &mut Heap) -> RuntimeValue {
        heap.allocate_set(
            nexa_bytecode::set_type(nexa_bytecode::ValueType::I32),
            nexa_bytecode::ValueType::I32,
        )
        .unwrap()
    }

    fn insert_all(heap: &mut Heap, set: RuntimeValue, keys: std::ops::Range<i32>) {
        for key in keys {
            while heap.set_insert(set, RuntimeValue::I32(key)).unwrap()
                == SetInsertOutcome::RehashPending
            {}
        }
    }

    #[test]
    fn set_insert_remove_reinsert_keeps_probe_chains_correct() {
        // `LANGUAGE_V3` Set: delete/reinsert churn must keep every surviving
        // element reachable through the shared backshift probe invariant.
        let mut heap = Heap::new_with_limits(8, usize::MAX, 512);
        let set = i32_set(&mut heap);
        insert_all(&mut heap, set, 0..64);
        for key in (0..64).step_by(2) {
            assert!(heap.set_remove(set, RuntimeValue::I32(key)).unwrap());
        }
        for key in 0..64 {
            assert_eq!(
                heap.set_contains(set, RuntimeValue::I32(key)).unwrap(),
                key % 2 == 1
            );
        }
        for key in (0..64).step_by(2) {
            assert_eq!(
                heap.set_insert(set, RuntimeValue::I32(key)).unwrap(),
                SetInsertOutcome::Complete(true)
            );
        }
        for key in 0..64 {
            assert!(heap.set_contains(set, RuntimeValue::I32(key)).unwrap());
        }
        assert_eq!(heap.set_len(set).unwrap(), 64);
    }

    #[test]
    fn set_duplicate_insert_leaves_set_and_epoch_unchanged() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(7)).unwrap(),
            SetInsertOutcome::Complete(true)
        );
        let epoch = heap.set_mutation_epoch(set).unwrap();
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(7)).unwrap(),
            SetInsertOutcome::Complete(false)
        );
        assert_eq!(heap.set_mutation_epoch(set).unwrap(), epoch);
        assert_eq!(heap.set_len(set).unwrap(), 1);
    }

    #[test]
    fn set_rehash_phases_preserve_elements_and_advance_epoch() {
        // Rehash begins once the load factor crosses 3/4; each attempt
        // migrates one bounded chunk, and the epoch advances when the
        // rehash starts (paused rehash still traps old iterators) and
        // again when the final insert lands.
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        let mut epochs = Vec::new();
        for key in 0..10_i32 {
            loop {
                match heap.set_insert(set, RuntimeValue::I32(key)).unwrap() {
                    SetInsertOutcome::Complete(_) => break,
                    SetInsertOutcome::RehashPending => {
                        epochs.push(heap.set_mutation_epoch(set).unwrap());
                    }
                }
            }
        }
        assert_eq!(heap.set_len(set).unwrap(), 10);
        assert!(!epochs.is_empty(), "rehash was exercised");
        for key in 0..10_i32 {
            assert!(heap.set_contains(set, RuntimeValue::I32(key)).unwrap());
        }
        let final_epoch = heap.set_mutation_epoch(set).unwrap();
        assert!(
            epochs.iter().all(|epoch| *epoch < final_epoch),
            "rehash initiation advances the epoch before the final insert"
        );
        assert!(
            epochs.windows(2).all(|pair| pair[0] < pair[1]),
            "every rehash step advances the epoch"
        );
    }

    #[test]
    fn set_rehash_chunk_progress_advances_epoch_for_mid_rehash_iterators() {
        // An iterator snapshotted after the rehash began must trap once a
        // later attempt migrates entries between tables: the chunk itself
        // advances the epoch.
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        insert_all(&mut heap, set, 0..6);
        // Attempt 1 begins the rehash (epoch bump before any claim).
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(6)).unwrap(),
            SetInsertOutcome::RehashPending
        );
        let after_begin = heap.set_mutation_epoch(set).unwrap();
        // Attempt 2 progresses the chunk; every progress step advances
        // the epoch because the phase topology changes.
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(6)).unwrap(),
            SetInsertOutcome::RehashPending
        );
        let after_chunk = heap.set_mutation_epoch(set).unwrap();
        assert!(
            after_chunk > after_begin,
            "chunk progress advances the epoch"
        );
        // Attempt 3 completes the insert.
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(6)).unwrap(),
            SetInsertOutcome::Complete(true)
        );
        assert!(heap.set_mutation_epoch(set).unwrap() > after_chunk);
        assert_eq!(heap.set_len(set).unwrap(), 7);
    }

    #[test]
    fn map_rehash_chunk_progress_advances_epoch_for_mid_rehash_iterators() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        let set = |heap: &mut Heap, key: i32| {
            while heap
                .map_set(map, RuntimeValue::I32(key), RuntimeValue::I32(key))
                .unwrap()
                == MapSetOutcome::RehashPending
            {}
        };
        for key in 0..6 {
            set(&mut heap, key);
        }
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(6), RuntimeValue::I32(6))
                .unwrap(),
            MapSetOutcome::RehashPending
        );
        let after_begin = heap.map_mutation_epoch(map).unwrap();
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(6), RuntimeValue::I32(6))
                .unwrap(),
            MapSetOutcome::RehashPending
        );
        assert!(
            heap.map_mutation_epoch(map).unwrap() > after_begin,
            "map chunk migration advances the epoch"
        );
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(6), RuntimeValue::I32(6))
                .unwrap(),
            MapSetOutcome::Complete
        );
        assert_eq!(heap.map_len(map).unwrap(), 7);
    }

    #[test]
    fn set_rehash_empty_trailing_chunk_completion_still_advances_epoch() {
        // Drive the set into a 16->32 rehash, delete every entry still
        // sitting in the high half of the old table, then let the final
        // chunk run over only empty slots: the completion that switches
        // the header back to a single current table must still advance
        // the epoch (a phase-1 iterator would otherwise end and miss the
        // new table).
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        insert_all(&mut heap, set, 0..12);
        let mut began_16_to_32 = false;
        while !began_16_to_32 {
            match heap.set_insert(set, RuntimeValue::I32(12)).unwrap() {
                SetInsertOutcome::RehashPending => began_16_to_32 = true,
                SetInsertOutcome::Complete(_) => break,
            }
        }
        assert!(began_16_to_32, "the 13th insert enters the 16->32 rehash");
        // Progress the first chunk (old slots 0..8).
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(12)).unwrap(),
            SetInsertOutcome::RehashPending
        );
        // Delete everything still in old slots 8..16 so the trailing
        // chunk migrates zero entries. The removed keys leave the set
        // for good; the surviving keys must all remain reachable.
        let mut removed_keys = std::collections::BTreeSet::new();
        loop {
            let mut removed_any = false;
            let mut cursor = (1_u8, 8_usize);
            loop {
                let Some((phase, slot, value)) =
                    heap.set_iter_advance(set, cursor.0, cursor.1).unwrap()
                else {
                    break;
                };
                if phase == 1 {
                    if let RuntimeValue::I32(key) = value {
                        assert!(heap.set_remove(set, RuntimeValue::I32(key)).unwrap());
                        removed_keys.insert(key);
                        removed_any = true;
                    }
                }
                cursor = (phase, slot);
                if phase > 1 {
                    break;
                }
            }
            if !removed_any {
                break;
            }
        }
        assert!(!removed_keys.is_empty(), "high-half entries were removed");
        let before_final_chunk = heap.set_mutation_epoch(set).unwrap();
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(12)).unwrap(),
            SetInsertOutcome::RehashPending,
            "the trailing empty chunk still returns RehashPending"
        );
        assert!(
            heap.set_mutation_epoch(set).unwrap() > before_final_chunk,
            "completing the rehash over an empty chunk advances the epoch"
        );
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(12)).unwrap(),
            SetInsertOutcome::Complete(true)
        );
        for key in 0..12_i32 {
            assert_eq!(
                heap.set_contains(set, RuntimeValue::I32(key)).unwrap(),
                !removed_keys.contains(&key),
                "surviving keys remain reachable after the rehash completed"
            );
        }
        // The completing insert adds key 12 on top of the survivors.
        assert_eq!(heap.set_len(set).unwrap(), 12 - removed_keys.len() + 1);
    }

    #[test]
    fn set_clear_with_pending_rehash_bumps_epoch() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        insert_all(&mut heap, set, 0..6);
        // The seventh insert crosses the 3/4 load factor and must enter
        // the incremental rehash (RehashPending) before completing.
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(6)).unwrap(),
            SetInsertOutcome::RehashPending
        );
        let epoch = heap.set_mutation_epoch(set).unwrap();
        heap.set_clear(set).unwrap();
        assert_eq!(heap.set_len(set).unwrap(), 0);
        assert!(
            heap.set_mutation_epoch(set).unwrap() > epoch,
            "clearing a set with a pending rehash advances the epoch"
        );
    }

    #[test]
    fn map_epoch_advances_on_insert_overwrite_remove_and_clear() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        let epoch = |heap: &Heap| heap.map_mutation_epoch(map).unwrap();
        assert_eq!(epoch(&heap), 0);
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(1), RuntimeValue::I32(10))
                .unwrap(),
            MapSetOutcome::Complete
        );
        assert_eq!(epoch(&heap), 1);
        // `LANGUAGE_V3`: an existing-key insert overwrites and advances.
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(1), RuntimeValue::I32(11))
                .unwrap(),
            MapSetOutcome::Complete
        );
        assert_eq!(epoch(&heap), 2);
        assert_eq!(
            heap.map_remove(map, RuntimeValue::I32(1)).unwrap(),
            Some(RuntimeValue::I32(11))
        );
        assert_eq!(epoch(&heap), 3);
        // Removing a missing key is not a mutation.
        assert_eq!(heap.map_remove(map, RuntimeValue::I32(9)).unwrap(), None);
        assert_eq!(epoch(&heap), 3);
        // Clearing an empty map is not a mutation; clearing a populated
        // map advances the epoch.
        heap.map_clear(map).unwrap();
        assert_eq!(epoch(&heap), 3);
        heap.map_set(map, RuntimeValue::I32(2), RuntimeValue::I32(20))
            .unwrap();
        heap.map_clear(map).unwrap();
        assert_eq!(epoch(&heap), 5);
    }

    #[test]
    fn epoch_exhaustion_traps_before_any_data_write() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        let set_storage = match heap.resolve_mut(named_ref(&set)) {
            Ok(Object::Set { storage }) => *storage as usize,
            _ => panic!("set reference resolves to set storage"),
        };
        heap.sets[set_storage]
            .as_mut()
            .expect("set storage exists")
            .mutation_epoch = u64::MAX;
        assert_eq!(
            heap.set_insert(set, RuntimeValue::I32(1)),
            Err(HeapError::MutationEpochExhausted)
        );
        assert_eq!(
            heap.set_contains(set, RuntimeValue::I32(1)).unwrap(),
            false,
            "the failed insert left the set unchanged"
        );

        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        let map_storage = match heap.resolve_mut(named_ref(&map)) {
            Ok(Object::Map { storage }) => *storage as usize,
            _ => panic!("map reference resolves to map storage"),
        };
        heap.maps[map_storage]
            .as_mut()
            .expect("map storage exists")
            .mutation_epoch = u64::MAX;
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(1), RuntimeValue::I32(10)),
            Err(HeapError::MutationEpochExhausted)
        );
        assert_eq!(heap.map_len(map).unwrap(), 0);
    }

    #[test]
    fn array_and_buffer_epochs_advance_on_writes_and_structural_ops() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        let epoch = |heap: &Heap| heap.array_mutation_epoch(array).unwrap();
        heap.array_push(array, RuntimeValue::I32(1)).unwrap();
        assert_eq!(epoch(&heap), 1);
        heap.array_push(array, RuntimeValue::I32(2)).unwrap();
        heap.array_set(array, 0, RuntimeValue::I32(9)).unwrap();
        assert_eq!(epoch(&heap), 3);
        // Swap and reverse advance the epoch exactly once per operation.
        heap.array_swap(array, 0, 1).unwrap();
        assert_eq!(epoch(&heap), 4);
        heap.array_reverse(array).unwrap();
        assert_eq!(epoch(&heap), 5);
        // A no-op swap (lhs == rhs) is not a mutation.
        heap.array_swap(array, 1, 1).unwrap();
        assert_eq!(epoch(&heap), 5);
        heap.array_pop(array).unwrap();
        assert_eq!(epoch(&heap), 6);
        heap.array_clear(array).unwrap();
        assert_eq!(epoch(&heap), 7);
        // Clearing an already-empty array is not a mutation.
        heap.array_clear(array).unwrap();
        assert_eq!(epoch(&heap), 7);

        let buffer_type = nexa_bytecode::buffer_type(nexa_bytecode::ValueType::I32);
        let buffer = heap
            .allocate_buffer(
                buffer_type,
                nexa_bytecode::ValueType::I32,
                &[RuntimeValue::I32(0); 4],
            )
            .unwrap();
        let epoch = |heap: &Heap| heap.buffer_mutation_epoch(buffer).unwrap();
        heap.buffer_set(buffer, 0, RuntimeValue::I32(5)).unwrap();
        assert_eq!(epoch(&heap), 1);
        heap.buffer_fill(buffer, 0, 4, RuntimeValue::I32(7))
            .unwrap();
        assert_eq!(epoch(&heap), 2);
    }

    #[test]
    fn swap_and_reverse_at_max_epoch_trap_before_any_write() {
        // At epoch u64::MAX the swap must fail atomically: the epoch
        // reserve traps before the first element write, so neither side
        // of the swap lands.
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        heap.array_push(array, RuntimeValue::I32(1)).unwrap();
        heap.array_push(array, RuntimeValue::I32(2)).unwrap();
        heap.array_push(array, RuntimeValue::I32(3)).unwrap();
        {
            let reference = named_ref(&array);
            match heap.resolve_mut(reference).unwrap() {
                Object::Array { mutation_epoch, .. } => *mutation_epoch = u64::MAX,
                _ => panic!("array reference resolves to array storage"),
            }
        }
        assert_eq!(
            heap.array_swap(array, 0, 2),
            Err(HeapError::MutationEpochExhausted)
        );
        assert_eq!(
            heap.array_get(array, 0).unwrap(),
            RuntimeValue::I32(1),
            "the failed swap left the array untouched"
        );
        assert_eq!(
            heap.array_get(array, 2).unwrap(),
            RuntimeValue::I32(3),
            "neither swap write landed"
        );
        assert_eq!(
            heap.array_reverse(array),
            Err(HeapError::MutationEpochExhausted)
        );
        assert_eq!(
            heap.array_get(array, 0).unwrap(),
            RuntimeValue::I32(1),
            "the failed reverse left the array untouched"
        );
        assert_eq!(
            heap.array_push(array, RuntimeValue::I32(4)),
            Err(HeapError::MutationEpochExhausted),
            "every mutation traps once the epoch is exhausted"
        );
        assert_eq!(heap.array_len(array).unwrap(), 3);
    }

    #[test]
    fn set_iteration_visits_backing_slots_deterministically() {
        let mut heap = Heap::new_with_limits(8, usize::MAX, 64);
        let set = i32_set(&mut heap);
        insert_all(&mut heap, set, 0..6);
        let mut entries: Vec<i32> = heap
            .set_entries(set)
            .unwrap()
            .map(|value| match value {
                RuntimeValue::I32(value) => value,
                _ => panic!("i32 set elements are i32"),
            })
            .collect();
        entries.sort_unstable();
        assert_eq!(entries, vec![0, 1, 2, 3, 4, 5]);
        // Exhausted advance keeps returning None without error.
        assert!(heap.set_iter_advance(set, 0, 0).unwrap().is_some());
        assert_eq!(heap.set_iter_advance(set, 0, 1024).unwrap(), None);
        assert_eq!(heap.set_iter_advance(set, 3, 0).unwrap(), None);
    }

    #[test]
    fn set_keys_are_precisely_traced_across_rehash() {
        // `LANGUAGE_V3` GC: set element references are exact roots; a
        // rehash-migrated string key must survive collection.
        let mut heap = Heap::new_with_limits(64, usize::MAX, 64);
        let string_type = nexa_bytecode::ValueType::String;
        let set = heap
            .allocate_set(nexa_bytecode::set_type(string_type), string_type)
            .unwrap();
        let mut references = Vec::new();
        for index in 0..12 {
            let reference = heap.allocate_string(&format!("set-key-{index}")).unwrap();
            references.push(reference);
            let hash = heap.string_hash(reference).unwrap();
            let value = RuntimeValue::String { reference, hash };
            while heap.set_insert(set, value).unwrap() == SetInsertOutcome::RehashPending {}
        }
        let RuntimeValue::NamedRef {
            reference: set_reference,
            ..
        } = set
        else {
            panic!("set allocations are named references")
        };
        let roots = GcRoots {
            running_frames: vec![set_reference],
            ..GcRoots::default()
        };
        // The set plus its 12 string keys survive collection: set element
        // references are exact GC roots across the rehash.
        assert_eq!(heap.collect(&roots).unwrap().live, 13);
        assert!(
            references
                .iter()
                .all(|reference| heap.string(*reference).is_ok())
        );
        for index in 0..12 {
            let reference = references[index];
            let hash = heap.string_hash(reference).unwrap();
            assert!(
                heap.set_contains(set, RuntimeValue::String { reference, hash },)
                    .unwrap()
            );
        }
    }

    fn named_ref(value: &RuntimeValue) -> crate::GcRef {
        match value {
            RuntimeValue::NamedRef { reference, .. } => *reference,
            _ => panic!("named reference expected"),
        }
    }
}
