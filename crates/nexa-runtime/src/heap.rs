use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::Arc;

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
    old_slots: CollectionRange,
    new_slots: CollectionRange,
    cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmMap {
    type_id: StableId,
    key_type: nexa_bytecode::ValueType,
    value_type: nexa_bytecode::ValueType,
    slots: CollectionRange,
    length: usize,
    rehash: Option<MapRehash>,
}

impl VmMap {
    // WP73: stream child references into the mark queue instead of
    // materializing a temporary Vec per object during GC.
    fn trace_references(&self, arena: &MapSlotArena, visit: &mut impl FnMut(GcRef)) {
        let current = arena
            .slots(self.slots)
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|entry| [entry.key, entry.value]);
        let rehash = self.rehash.iter().flat_map(|rehash| {
            arena
                .slots(rehash.old_slots)
                .iter()
                .chain(arena.slots(rehash.new_slots))
                .filter_map(Option::as_ref)
                .flat_map(|entry| [entry.key, entry.value])
        });
        for reference in current.chain(rehash).filter_map(value_reference) {
            visit(reference);
        }
    }

    /// G4 byte accounting: system bytes held by the slot vectors,
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
                return Some((entry.key, entry.value));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapSetOutcome {
    Complete,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectionArena {
    values: Vec<RuntimeValue>,
    free_ranges: Vec<CollectionRange>,
    capacity: usize,
}

/// Preallocated scalar collection storage. Unlike `CollectionArena`, each
/// element occupies exactly `size_of::<T>()`; extents share the same bounded
/// first-fit/release discipline and never allocate after heap construction.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScalarArena<T> {
    values: Vec<T>,
    capacity: usize,
    empty: T,
}

impl<T: Copy> ScalarArena<T> {
    fn new(capacity: usize, empty: T) -> Self {
        Self {
            values: Vec::with_capacity(capacity),
            capacity,
            empty,
        }
    }

    fn claim_exact(&mut self, range: CollectionRange) -> Result<(), HeapError> {
        if range.end() > self.capacity {
            return Err(HeapError::CapacityExhausted);
        }
        if self.values.len() < range.end() {
            self.values.resize(range.end(), self.empty);
        }
        Ok(())
    }

    fn release(&mut self, range: CollectionRange) {
        if range.length == 0 {
            return;
        }
        if let Some(values) = self.values.get_mut(range.start..range.end()) {
            values.fill(self.empty);
        }
    }

    fn values(&self, range: CollectionRange) -> Result<&[T], HeapError> {
        self.values
            .get(range.start..range.end())
            .ok_or(HeapError::IndexOutOfBounds {
                index: range.end(),
                length: self.capacity,
            })
    }

    fn values_mut(&mut self, range: CollectionRange) -> Result<&mut [T], HeapError> {
        self.values
            .get_mut(range.start..range.end())
            .ok_or(HeapError::IndexOutOfBounds {
                index: range.end(),
                length: self.capacity,
            })
    }

    fn checkpoint_clone(&self) -> Self {
        let mut values = Vec::with_capacity(self.values.capacity());
        values.extend_from_slice(&self.values);
        Self {
            values,
            capacity: self.capacity,
            empty: self.empty,
        }
    }
}

fn claim_scalar_regrow<T: Copy>(
    arena: &mut ScalarArena<T>,
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
    if old_range.length == 0 {
        write(&mut arena.values[new_start..new_end]);
    } else if new_end <= old_start || old_end <= new_start {
        let (destination, source) = if new_end <= old_start {
            let (left, right) = arena.values.split_at_mut(old_start);
            (&mut left[new_start..new_end], &right[..old_range.length])
        } else {
            let (left, right) = arena.values.split_at_mut(new_start);
            (&mut right[..capacity], &left[old_start..old_end])
        };
        destination[..live].copy_from_slice(&source[..live]);
        write(destination);
    } else {
        arena.release(new_range);
        return Err(HeapError::CapacityExhausted);
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
        let mut values = Vec::with_capacity(self.values.capacity());
        values.extend_from_slice(&self.values);
        let mut free_ranges = Vec::with_capacity(self.free_ranges.capacity());
        free_ranges.extend_from_slice(&self.free_ranges);
        Self {
            values,
            free_ranges,
        }
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
    /// Exclusive-category sum: headers, out-of-slot payloads, slack, and
    /// profiler storage.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.object_header_bytes
            .saturating_add(self.class_payload_bytes)
            .saturating_add(self.string_bytes)
            .saturating_add(self.array_bytes)
            .saturating_add(self.buffer_bytes)
            .saturating_add(self.map_bytes)
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
            values: vec![RuntimeValue::Unit; capacity],
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
        self.values[range.start..range.end()].fill(RuntimeValue::Unit);
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

    fn checkpoint_clone(&self) -> Self {
        let mut values = Vec::with_capacity(self.values.capacity());
        values.extend_from_slice(&self.values);
        let mut free_ranges = Vec::with_capacity(self.free_ranges.capacity());
        free_ranges.extend_from_slice(&self.free_ranges);
        Self {
            values,
            free_ranges,
            capacity: self.capacity,
        }
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
        range: CollectionRange,
        field_count: u8,
        hash: u64,
    },
    Class {
        type_id: StableId,
        range: CollectionRange,
        field_count: u8,
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
        row_stride: Option<std::num::NonZeroU8>,
    },
    Buffer {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        storage: CollectionStorage,
        range: CollectionRange,
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
            Self::Array { range, storage, .. } | Self::Buffer { range, storage, .. } => {
                range.length.saturating_mul(storage.cell_size())
            }
            Self::Struct { range, .. } | Self::Class { range, .. } => {
                range.length.saturating_mul(size_of::<RuntimeValue>())
            }
            // Shared literal bytes belong to ExecutableModule; Map payload
            // lives in its typed arena; Enum payload remains inline.
            Self::SharedString(_) | Self::Map { .. } | Self::Enum { .. } => 0,
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

    const fn cell_size(self) -> usize {
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
}

/// Resolved array header shared by every logical array operation (WP52).
#[derive(Clone, Copy)]
struct ArrayParts {
    reference: GcRef,
    range: CollectionRange,
    /// Logical element count.
    length: usize,
    row_stride: Option<std::num::NonZeroU8>,
    element_type: nexa_bytecode::ValueType,
    storage: CollectionStorage,
}

impl ArrayParts {
    /// Arena cells per logical element.
    fn stride(self) -> usize {
        self.row_stride
            .map_or(1, |stride| usize::from(stride.get()))
    }

    /// `Some(stride)` when elements are flattened struct rows.
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

/// Incremental cycle phase (G1): `Idle -> Mark -> Sweep -> Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcPhase {
    Idle,
    Mark,
    Sweep,
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

/// Live tracker for one incremental step (G5). The object axis is a strict
/// pre-check (G1 semantics); bytes and deadline are charged after each
/// completed unit with a first-unit guarantee, so a degenerate budget
/// overruns by at most one unit instead of stalling the cycle.
struct StepBudget {
    objects: usize,
    bytes: u64,
    deadline: Option<std::time::Instant>,
    spent: bool,
}

impl StepBudget {
    fn new(budget: GcBudget) -> Self {
        Self {
            objects: budget.max_objects,
            bytes: budget.max_bytes,
            // `Duration::MAX` disables the clock; adding it to `now` would
            // overflow, and the deterministic path must not read time.
            deadline: (budget.max_duration != std::time::Duration::MAX)
                .then(|| std::time::Instant::now() + budget.max_duration),
            spent: false,
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
    }
}

/// Telemetry for one incremental step: work actually performed, the phase
/// after the step, and the whole-cycle stats when the cycle completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncrementalGcReport {
    pub roots_seeded: usize,
    pub objects_marked: usize,
    pub slots_swept: usize,
    pub barrier_shades: u64,
    /// G4: payload bytes released by this step's sweep slice (String
    /// storage, i32 backing, map slot vectors, collection-arena extents).
    pub bytes_reclaimed: u64,
    pub completed: Option<CollectionStats>,
}

/// Cumulative VM allocation and copy counters (M5 WP13).
///
/// Counters are monotonic work totals, not live-state gauges: checkpoint
/// restores (REPL transaction rollback) intentionally do not rewind them,
/// because the allocation and copy work still happened. Host codec copy
/// accounting lands with the stage-H boundary work and is reported as
/// unavailable until then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmAllocationCounters {
    pub object_allocations: u64,
    pub string_allocations: u64,
    pub class_allocations: u64,
    pub collection_storage_allocations: u64,
    pub map_slot_allocations: u64,
    pub struct_materializations: u64,
    pub enum_materializations: u64,
    pub collection_relocation_bytes: u64,
    pub string_copy_bytes: u64,
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
        self.collection_relocation_bytes = self
            .collection_relocation_bytes
            .saturating_add(other.collection_relocation_bytes);
        self.string_copy_bytes = self
            .string_copy_bytes
            .saturating_add(other.string_copy_bytes);
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
            collection_relocation_bytes: self
                .collection_relocation_bytes
                .saturating_sub(earlier.collection_relocation_bytes),
            string_copy_bytes: self
                .string_copy_bytes
                .saturating_sub(earlier.string_copy_bytes),
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
    map_slots: MapSlotArena,
    max_objects: u32,
    max_string_bytes: usize,
    max_collection_length: usize,
    max_collection_elements: usize,
    collection_elements_used: usize,
    collections: CollectionArena,
    /// K13/WP47: compact 4-byte cells for Array/Buffer<i32>.
    i32_collections: ScalarArena<i32>,
    i64_collections: ScalarArena<i64>,
    f32_collections: ScalarArena<u32>,
    f64_collections: ScalarArena<u64>,
    bool_collections: ScalarArena<u8>,
    rune_collections: ScalarArena<u32>,
    string_collections: ScalarArena<(GcRef, u64)>,
    ref_collections: ScalarArena<GcRef>,
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
    /// G1 incremental cycle state: current phase, the sweep resume
    /// cursor, objects marked so far this cycle, and insertion-barrier
    /// shade count for telemetry.
    gc_phase: GcPhase,
    gc_sweep_cursor: usize,
    gc_marked: usize,
    gc_reclaimed: usize,
    gc_barrier_shades: u64,
    /// G4: payload bytes released by the current cycle's sweep slices;
    /// latched into `last_cycle_bytes_reclaimed` when the cycle completes.
    gc_bytes_reclaimed: u64,
    last_cycle_bytes_reclaimed: u64,
    /// G6 live gauge: out-of-slot payload bytes owned by live objects,
    /// maintained incrementally at every footprint transition (commit,
    /// sweep, host rollback, array regrow, map rehash). Full collection
    /// re-derives it in debug builds to pin the gauge against drift.
    live_payload_bytes: u64,
    /// G6 admission ceiling over `live_payload_bytes`; `u64::MAX` keeps
    /// every existing constructor unlimited.
    max_heap_bytes: u64,
}

/// Exact heap state owned by one staged transactional Cell.
///
/// Runtime limits and the failure-control plane remain Realm authority and are
/// intentionally not snapshotted. Every mutable VM storage surface is: object
/// slots/generations, free lists, collection storage, and Host return staging.
#[derive(Clone, Debug)]
pub(crate) struct HeapCheckpoint {
    slots: Vec<ObjectSlot>,
    free: Vec<u32>,
    maps: Vec<Option<VmMap>>,
    free_maps: Vec<u32>,
    map_slots: MapSlotArena,
    collections: CollectionArena,
    i32_collections: ScalarArena<i32>,
    i64_collections: ScalarArena<i64>,
    f32_collections: ScalarArena<u32>,
    f64_collections: ScalarArena<u64>,
    bool_collections: ScalarArena<u8>,
    rune_collections: ScalarArena<u32>,
    string_collections: ScalarArena<(GcRef, u64)>,
    ref_collections: ScalarArena<GcRef>,
    host_staging: Vec<GcRef>,
    host_transaction_active: bool,
    collection_elements_used: usize,
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
        let mut slots = Vec::with_capacity(self.slots.capacity());
        slots.extend_from_slice(&self.slots);
        let mut free = Vec::with_capacity(self.free.capacity());
        free.extend_from_slice(&self.free);
        let mut maps = Vec::with_capacity(self.maps.capacity());
        maps.extend_from_slice(&self.maps);
        let mut free_maps = Vec::with_capacity(self.free_maps.capacity());
        free_maps.extend_from_slice(&self.free_maps);
        let mut host_staging = Vec::with_capacity(self.host_staging.capacity());
        host_staging.extend_from_slice(&self.host_staging);
        HeapCheckpoint {
            slots,
            free,
            maps,
            free_maps,
            map_slots: self.map_slots.clone(),
            collections: self.collections.checkpoint_clone(),
            i32_collections: self.i32_collections.checkpoint_clone(),
            i64_collections: self.i64_collections.checkpoint_clone(),
            f32_collections: self.f32_collections.checkpoint_clone(),
            f64_collections: self.f64_collections.checkpoint_clone(),
            bool_collections: self.bool_collections.checkpoint_clone(),
            rune_collections: self.rune_collections.checkpoint_clone(),
            string_collections: self.string_collections.checkpoint_clone(),
            ref_collections: self.ref_collections.checkpoint_clone(),
            host_staging,
            host_transaction_active: self.host_transaction_active,
            collection_elements_used: self.collection_elements_used,
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
        self.slots = checkpoint.slots;
        self.free = checkpoint.free;
        self.maps = checkpoint.maps;
        self.free_maps = checkpoint.free_maps;
        self.map_slots = checkpoint.map_slots;
        self.collections = checkpoint.collections;
        self.i32_collections = checkpoint.i32_collections;
        self.i64_collections = checkpoint.i64_collections;
        self.f32_collections = checkpoint.f32_collections;
        self.f64_collections = checkpoint.f64_collections;
        self.bool_collections = checkpoint.bool_collections;
        self.rune_collections = checkpoint.rune_collections;
        self.string_collections = checkpoint.string_collections;
        self.ref_collections = checkpoint.ref_collections;
        self.host_staging = checkpoint.host_staging;
        self.host_transaction_active = checkpoint.host_transaction_active;
        self.collection_elements_used = checkpoint.collection_elements_used;
        // G6: the restored object population owns a different footprint;
        // re-derive the gauge from the ground truth walk.
        self.live_payload_bytes = self.recompute_live_payload_bytes();
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
            i32_collections: ScalarArena::new(max_collection_elements, 0),
            i64_collections: ScalarArena::new(max_collection_elements, 0),
            f32_collections: ScalarArena::new(max_collection_elements, 0),
            f64_collections: ScalarArena::new(max_collection_elements, 0),
            bool_collections: ScalarArena::new(max_collection_elements, 0),
            rune_collections: ScalarArena::new(max_collection_elements, 0),
            string_collections: ScalarArena::new(
                max_collection_elements,
                (
                    GcRef {
                        index: u32::MAX,
                        generation: u32::MAX,
                    },
                    0,
                ),
            ),
            ref_collections: ScalarArena::new(
                max_collection_elements,
                GcRef {
                    index: u32::MAX,
                    generation: u32::MAX,
                },
            ),
            host_staging: Vec::with_capacity(max_objects as usize),
            host_transaction_active: false,
            failure_injector: RuntimeFailureInjector::default(),
            counters: VmAllocationCounters::default(),
            string_literal_cache: BTreeMap::new(),
            pooled_string_cache: Vec::with_capacity(max_objects as usize),
            mark_scratch: VecDeque::with_capacity(max_objects as usize),
            gc_phase: GcPhase::Idle,
            gc_sweep_cursor: 0,
            gc_marked: 0,
            gc_reclaimed: 0,
            gc_barrier_shades: 0,
            gc_bytes_reclaimed: 0,
            last_cycle_bytes_reclaimed: 0,
            live_payload_bytes: 0,
            max_heap_bytes: u64::MAX,
        }
    }

    /// Cumulative allocation/copy work performed by this heap (WP13).
    #[must_use]
    pub const fn vm_allocation_counters(&self) -> VmAllocationCounters {
        self.counters
    }

    pub fn allocate_string(&mut self, value: &str) -> Result<GcRef, HeapError> {
        self.validate_string_length(value.len())?;
        // G6 admission: string storage counts toward the byte ceiling.
        self.ensure_payload_headroom(value.len() as u64)?;
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
        // G6 admission: the concatenated storage counts toward the ceiling.
        self.ensure_payload_headroom(length as u64)?;
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

        // Reserve both arenas before publishing any VM object. In
        // particular, splitting on an empty delimiter cannot partially fill
        // the heap, or even allocate owned part strings, before hitting a
        // resource limit.
        let object_count = part_count
            .checked_add(1)
            .ok_or(HeapError::CapacityExhausted)?;
        let mut objects = self.preflight(object_count)?;
        let storage = CollectionStorage::String;
        let range = self.claim_typed_collection(storage, part_count)?;
        let mut parts = Vec::new();
        if parts.try_reserve_exact(part_count).is_err() {
            self.release_typed_collection(storage, range);
            return Err(HeapError::CapacityExhausted);
        }
        {
            let value = self.string(value)?;
            let delimiter = self.string(delimiter)?;
            parts.extend(value.split(delimiter).map(str::to_owned));
        }
        debug_assert_eq!(parts.len(), part_count);
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
        if matches!(object, Object::Map { .. }) {
            return Err(invalid_value_reference());
        }
        // G6 admission: the whole out-of-slot footprint is known here.
        self.ensure_payload_headroom(self.object_payload_bytes(&object))?;
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
            _ => object.payload_bytes(),
        }
    }

    /// Releases the exclusively owned payload behind a condemned slot and
    /// returns the exact G4/G6 byte count that left the live heap.
    fn release_object_storage(&mut self, object: &Object) -> u64 {
        let payload = self.object_payload_bytes(object);
        match object {
            Object::Array { range, storage, .. } | Object::Buffer { range, storage, .. } => {
                self.release_typed_collection(*storage, *range);
            }
            Object::Struct { range, .. } | Object::Class { range, .. } => {
                self.collections.release(*range);
                self.release_collection_quota(range.length);
            }
            Object::Map { storage } => {
                let storage = *storage as usize;
                if let Some(map) = self.maps.get_mut(storage).and_then(Option::take) {
                    self.map_slots.release(map.slots);
                    if let Some(rehash) = map.rehash {
                        self.map_slots.release(rehash.old_slots);
                        self.map_slots.release(rehash.new_slots);
                    }
                    self.free_maps
                        .push(u32::try_from(storage).expect("map arena index originates as u32"));
                }
            }
            Object::String(_) | Object::SharedString(_) | Object::Enum { .. } => {}
        }
        payload
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
        self.counters.object_allocations = self.counters.object_allocations.saturating_add(1);
        // G6: the commit funnel is the single charge point for every fresh
        // object's out-of-slot footprint; later growth (array regrow, map
        // rehash) adjusts the gauge at its own transition site.
        self.charge_live_payload(self.object_payload_bytes(&object));
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
            Object::Map { .. } => {
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
        let born_marked = self.gc_phase != GcPhase::Idle;
        if self.gc_phase == GcPhase::Mark {
            object.trace_references(&mut |child| {
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
        // G6 admission: string storage counts toward the byte ceiling.
        self.ensure_payload_headroom(u64::try_from(value.capacity()).unwrap_or(u64::MAX))?;
        let reference = self.commit(reservation, Object::String(value));
        let hash = self.string_hash(reference)?;
        Ok(RuntimeValue::String { reference, hash })
    }

    pub fn preflight_collection(
        &mut self,
        element_count: usize,
    ) -> Result<CollectionReservation, HeapError> {
        // G6 admission: extent bytes count toward the heap byte ceiling.
        // For regrow this is conservative - the old extent is still held -
        // which is exactly the safe direction.
        let range = self.claim_global_collection_range(element_count, size_of::<RuntimeValue>())?;
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
        self.ensure_payload_headroom((element_count as u64).saturating_mul(element_bytes as u64))?;
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
            CollectionStorage::Values => Ok(()),
            CollectionStorage::I32 => self.i32_collections.claim_exact(range),
            CollectionStorage::I64 => self.i64_collections.claim_exact(range),
            CollectionStorage::F32 => self.f32_collections.claim_exact(range),
            CollectionStorage::F64 => self.f64_collections.claim_exact(range),
            CollectionStorage::Bool => self.bool_collections.claim_exact(range),
            CollectionStorage::Rune => self.rune_collections.claim_exact(range),
            CollectionStorage::String => self.string_collections.claim_exact(range),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                self.ref_collections.claim_exact(range)
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
            CollectionStorage::I32 => self.i32_collections.release(range),
            CollectionStorage::I64 => self.i64_collections.release(range),
            CollectionStorage::F32 => self.f32_collections.release(range),
            CollectionStorage::F64 => self.f64_collections.release(range),
            CollectionStorage::Bool => self.bool_collections.release(range),
            CollectionStorage::Rune => self.rune_collections.release(range),
            CollectionStorage::String => self.string_collections.release(range),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                self.ref_collections.release(range);
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
        match storage {
            CollectionStorage::Values => {
                Ok(CollectionView::Values(self.collections.values(range)?))
            }
            CollectionStorage::I32 => Ok(CollectionView::I32(self.i32_collections.values(range)?)),
            CollectionStorage::I64 => Ok(CollectionView::I64(self.i64_collections.values(range)?)),
            CollectionStorage::F32 => Ok(CollectionView::F32(self.f32_collections.values(range)?)),
            CollectionStorage::F64 => Ok(CollectionView::F64(self.f64_collections.values(range)?)),
            CollectionStorage::Bool => {
                Ok(CollectionView::Bool(self.bool_collections.values(range)?))
            }
            CollectionStorage::Rune => {
                Ok(CollectionView::Rune(self.rune_collections.values(range)?))
            }
            CollectionStorage::String => Ok(CollectionView::String(
                self.string_collections.values(range)?,
            )),
            CollectionStorage::Ref => Ok(CollectionView::Ref(self.ref_collections.values(range)?)),
            CollectionStorage::NamedRef => {
                let nexa_bytecode::ValueType::Named(type_id) = element_type else {
                    return Err(invalid_value_reference());
                };
                Ok(CollectionView::NamedRef {
                    values: self.ref_collections.values(range)?,
                    type_id,
                })
            }
        }
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
                let values = self.$arena.values_mut(range)?;
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
                set_scalar!(i32_collections, RuntimeValue::I32(value) => value)
            }
            CollectionStorage::I64 => {
                set_scalar!(i64_collections, RuntimeValue::I64(value) => value)
            }
            CollectionStorage::F32 => {
                set_scalar!(f32_collections, RuntimeValue::F32(value) => value)
            }
            CollectionStorage::F64 => {
                set_scalar!(f64_collections, RuntimeValue::F64(value) => value)
            }
            CollectionStorage::Bool => {
                set_scalar!(bool_collections, RuntimeValue::Bool(value) => u8::from(value))
            }
            CollectionStorage::Rune => {
                set_scalar!(rune_collections, RuntimeValue::Rune(value) => value)
            }
            CollectionStorage::String => {
                self.shade_on_write(value);
                set_scalar!(
                    string_collections,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                )
            }
            CollectionStorage::Ref => {
                self.shade_on_write(value);
                set_scalar!(ref_collections, RuntimeValue::Ref(reference) => reference)
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
                let values = self.ref_collections.values_mut(range)?;
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
        match storage {
            CollectionStorage::I32 => self
                .i32_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::I64 => self
                .i64_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::F32 => self
                .f32_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::F64 => self
                .f64_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::Bool => self
                .bool_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::Rune => self
                .rune_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::String => self
                .string_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::Ref | CollectionStorage::NamedRef => self
                .ref_collections
                .values_mut(range)?
                .copy_within(source, destination),
            CollectionStorage::Values => self
                .collections
                .values_mut(range)?
                .copy_within(source, destination),
        }
        Ok(())
    }

    fn typed_collection_clear(
        &mut self,
        storage: CollectionStorage,
        range: CollectionRange,
        cells: std::ops::Range<usize>,
    ) -> Result<(), HeapError> {
        match storage {
            CollectionStorage::I32 => {
                self.i32_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::I64 => {
                self.i64_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::F32 => {
                self.f32_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::F64 => {
                self.f64_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::Bool => {
                self.bool_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::Rune => {
                self.rune_collections.values_mut(range)?[cells].fill(0);
            }
            CollectionStorage::String => {
                self.string_collections.values_mut(range)?[cells].fill((
                    GcRef {
                        index: u32::MAX,
                        generation: u32::MAX,
                    },
                    0,
                ));
            }
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                self.ref_collections.values_mut(range)?[cells].fill(GcRef {
                    index: u32::MAX,
                    generation: u32::MAX,
                });
            }
            CollectionStorage::Values => {
                self.collections.values_mut(range)?[cells].fill(RuntimeValue::Unit);
            }
        }
        Ok(())
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

    pub(crate) fn commit_host_transaction(&mut self) {
        self.host_transaction_active = false;
        self.host_staging.clear();
    }

    pub(crate) fn rollback_host_transaction(&mut self) {
        self.host_transaction_active = false;
        let mut released = 0_u64;
        let mut recycled = false;
        while let Some(reference) = self.host_staging.pop() {
            let object = self
                .slots
                .get_mut(reference.index as usize)
                .filter(|slot| slot.generation == reference.generation)
                .and_then(|slot| slot.object.take());
            if let Some(object) = object {
                // G6: staged objects vanish outside the sweep, so their
                // footprint leaves the gauge here. Every collection now owns
                // one typed extent; committed staged objects release that
                // extent here, while unfinished builders are released by the
                // transaction drop path.
                released = released.saturating_add(self.object_payload_bytes(&object));
                match object {
                    Object::Array { storage, range, .. }
                    | Object::Buffer { storage, range, .. } => {
                        self.release_typed_collection(storage, range);
                    }
                    Object::Struct { range, .. } | Object::Class { range, .. } => {
                        self.collections.release(range);
                        self.release_collection_quota(range.length);
                    }
                    Object::Map { storage } => {
                        if let Some(map) = self.maps[storage as usize].take() {
                            self.map_slots.release(map.slots);
                            if let Some(rehash) = map.rehash {
                                self.map_slots.release(rehash.old_slots);
                                self.map_slots.release(rehash.new_slots);
                            }
                            self.free_maps.push(storage);
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
        let reference = self.commit(
            reservation,
            Object::Buffer {
                type_id,
                element_type,
                storage,
                range,
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
        let range = self.claim_field_extent(fields)?;
        let reference = self.commit(
            reservation,
            Object::Struct {
                type_id,
                range,
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

    /// K5: claims one collection-arena extent for a struct/class field
    /// row, writes the fields, and settles G1 bookkeeping. The caller
    /// commits the owning object immediately afterwards (no fallible step
    /// in between), so a claimed extent can never leak on an error path.
    /// G6 is charged once by the owning object's commit funnel.
    fn claim_field_extent(
        &mut self,
        fields: &[RuntimeValue],
    ) -> Result<CollectionRange, HeapError> {
        let bytes = u64::try_from(fields.len().saturating_mul(size_of::<RuntimeValue>()))
            .unwrap_or(u64::MAX);
        // G6 admission: field extents count toward the byte ceiling.
        self.ensure_payload_headroom(bytes)?;
        self.claim_collection_quota(fields.len())?;
        let range = self.collections.find_free(fields.len()).ok_or_else(|| {
            self.release_collection_quota(fields.len());
            HeapError::CapacityExhausted
        })?;
        if let Err(error) = self.collections.claim(range) {
            self.release_collection_quota(fields.len());
            return Err(error);
        }
        self.collections.values_mut(range)?.copy_from_slice(fields);
        // G1: the owner is born marked while a cycle is active, so its
        // children shade at the write - exactly the array-extent barrier;
        // `Object::trace_references` no longer sees these fields.
        for field in fields {
            self.shade_on_write(*field);
        }
        Ok(range)
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
                range,
                field_count,
                hash: actual_hash,
            } if *actual == type_id && *actual_hash == hash => {
                let cells = self.collections.values(*range)?;
                Ok(&cells[..usize::from(*field_count)])
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
        let field_count = fields.len();
        let mut updated = [RuntimeValue::Unit; nexa_bytecode::MAX_STRUCT_FIELDS];
        updated[..field_count].copy_from_slice(fields);
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
        // Slot preflight before the extent claim keeps the pair atomic:
        // once the extent is claimed, the commit cannot fail.
        let mut reservation = self.preflight(1)?;
        let range = self.claim_field_extent(fields)?;
        let reference = self.commit(
            &mut reservation,
            Object::Class {
                type_id,
                range,
                field_count: u8::try_from(fields.len()).expect("class field limit fits into u8"),
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
            .copied()
            .ok_or(HeapError::InvalidReference(reference))
    }

    pub(crate) fn class_fields(&self, value: RuntimeValue) -> Result<&[RuntimeValue], HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Class {
                type_id: actual,
                range,
                field_count,
            } if *actual == type_id => {
                let cells = self.collections.values(*range)?;
                Ok(&cells[..usize::from(*field_count)])
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    /// K5 inspection: the field cells of a struct or class resolved by
    /// reference alone (no value-side type or hash re-validation).
    /// Read-only tooling and tests use this where they previously
    /// destructured the inline field array out of [`Object`].
    pub fn object_fields(&self, reference: GcRef) -> Result<&[RuntimeValue], HeapError> {
        match self.resolve(reference)? {
            Object::Struct {
                range, field_count, ..
            }
            | Object::Class {
                range, field_count, ..
            } => {
                let cells = self.collections.values(*range)?;
                Ok(&cells[..usize::from(*field_count)])
            }
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
                range,
                field_count,
            } if *actual == type_id && index < usize::from(*field_count) => *range,
            _ => return Err(HeapError::InvalidReference(reference)),
        };
        self.collections.values_mut(range)?[index] = replacement;
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
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    /// WP52: allocates an array whose struct elements live flattened in
    /// the collection arena - `field_count` cells per element, zero heap
    /// objects per element. `element_type` must name the struct type.
    pub fn allocate_struct_row_array(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        field_count: std::num::NonZeroU8,
    ) -> Result<RuntimeValue, HeapError> {
        if type_id != nexa_bytecode::array_type(element_type)
            || !matches!(element_type, nexa_bytecode::ValueType::Named(_))
            || usize::from(field_count.get()) > nexa_bytecode::MAX_STRUCT_FIELDS
        {
            return Err(invalid_value_reference());
        }
        let reference = self.allocate(Object::Array {
            type_id,
            element_type,
            storage: CollectionStorage::Values,
            range: CollectionRange::default(),
            length: 0,
            row_stride: Some(field_count),
        })?;
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub fn array_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.array_parts(value)?.length)
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
                if current < parts.range.length {
                    self.$arena.values_mut(parts.range)?[current] = stored;
                } else {
                    let capacity = grown_array_capacity(
                        parts.range.length,
                        length,
                        self.max_collection_length,
                    );
                    let global_range =
                        self.claim_global_collection_range(capacity, size_of_val(&stored))?;
                    let new_range = match claim_scalar_regrow(
                        &mut self.$arena,
                        global_range,
                        parts.range,
                        current,
                        |values| values[current] = stored,
                    ) {
                        Ok(range) => range,
                        Err(error) => {
                            self.collections.release(global_range);
                            self.release_collection_quota(capacity);
                            return Err(error);
                        }
                    };
                    if let Err(error) = self.set_array_range(parts.reference, new_range) {
                        self.$arena.release(new_range);
                        self.collections.release(new_range);
                        self.release_collection_quota(new_range.length);
                        return Err(error);
                    }
                    self.$arena.release(parts.range);
                    self.collections.release(parts.range);
                    self.release_collection_quota(parts.range.length);
                    let element_bytes = size_of_val(&stored) as u64;
                    self.charge_live_payload(
                        (new_range.length as u64).saturating_mul(element_bytes),
                    );
                    self.release_live_payload(
                        (parts.range.length as u64).saturating_mul(element_bytes),
                    );
                }
                self.set_array_length(parts.reference, length)
            }};
        }
        match parts.storage {
            CollectionStorage::I32 => {
                return push_scalar!(i32_collections, RuntimeValue::I32(value) => value);
            }
            CollectionStorage::I64 => {
                return push_scalar!(i64_collections, RuntimeValue::I64(value) => value);
            }
            CollectionStorage::F32 => {
                return push_scalar!(f32_collections, RuntimeValue::F32(value) => value);
            }
            CollectionStorage::F64 => {
                return push_scalar!(f64_collections, RuntimeValue::F64(value) => value);
            }
            CollectionStorage::Bool => {
                return push_scalar!(
                    bool_collections,
                    RuntimeValue::Bool(value) => u8::from(value)
                );
            }
            CollectionStorage::Rune => {
                return push_scalar!(rune_collections, RuntimeValue::Rune(value) => value);
            }
            CollectionStorage::String => {
                self.shade_on_write(element);
                return push_scalar!(
                    string_collections,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                );
            }
            CollectionStorage::Ref => {
                self.shade_on_write(element);
                return push_scalar!(ref_collections, RuntimeValue::Ref(reference) => reference);
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
                return push_scalar!(ref_collections, RuntimeValue::NamedRef { .. } => reference);
            }
            CollectionStorage::Values => {}
        }
        let Some(stride) = parts.rows() else {
            self.shade_on_write(element);
            if current < parts.range.length {
                // WP49 amortized fast path: spare capacity, write in place.
                self.collections.values_mut(parts.range)?[current] = element;
                self.set_array_length(parts.reference, length)?;
                return Ok(());
            }
            let capacity =
                grown_array_capacity(parts.range.length, length, self.max_collection_length);
            self.regrow_array(parts.reference, parts.range, current, capacity, |values| {
                values[current] = element;
            })?;
            return self.set_array_length(parts.reference, length);
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
        let Some(stride) = parts.rows() else {
            let element = self.allocate_struct(parts.element_struct_type()?, fields)?;
            return self.array_push(value, element);
        };
        if fields.len() != stride {
            return Err(invalid_value_reference());
        }
        self.push_row_cells(parts, length, fields)
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
        if needed_cells <= parts.range.length {
            // WP49 amortized fast path: spare row capacity, write in place.
            self.collections.values_mut(parts.range)?[current * stride..needed_cells]
                .copy_from_slice(row);
            self.set_array_length(parts.reference, length)?;
            return Ok(());
        }
        // Growth is computed in logical elements so the new extent stays
        // row-aligned; the arena works in cells.
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
                values[current * stride..(current + 1) * stride].copy_from_slice(row);
            },
        )?;
        self.set_array_length(parts.reference, length)
    }

    pub fn array_pop(&mut self, value: RuntimeValue) -> Result<RuntimeValue, HeapError> {
        let parts = self.array_parts(value)?;
        let length = parts.length;
        if length == 0 {
            return Err(HeapError::IndexOutOfBounds { index: 0, length });
        }
        let Some(stride) = parts.rows() else {
            let result = self.typed_collection_get(
                parts.storage,
                parts.element_type,
                parts.range,
                length - 1,
            )?;
            self.typed_collection_clear(parts.storage, parts.range, length - 1..length)?;
            self.set_array_length(parts.reference, length - 1)?;
            return Ok(result);
        };
        // Materialize the row before mutating anything: a failed struct
        // allocation must leave the array untouched (failure atomicity).
        let result = self.array_get(value, length - 1)?;
        let values = self.collections.values_mut(parts.range)?;
        values[(length - 1) * stride..length * stride].fill(RuntimeValue::Unit);
        self.set_array_length(parts.reference, length - 1)?;
        Ok(result)
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
                let new_range = match claim_scalar_regrow(
                    &mut self.$arena,
                    global_range,
                    parts.range,
                    current,
                    |values| {
                        values.copy_within(index..current, index + 1);
                        values[index] = stored;
                    },
                ) {
                    Ok(range) => range,
                    Err(error) => {
                        self.collections.release(global_range);
                        self.release_collection_quota(capacity);
                        return Err(error);
                    }
                };
                if let Err(error) = self.set_array_range(parts.reference, new_range) {
                    self.$arena.release(new_range);
                    self.collections.release(new_range);
                    self.release_collection_quota(new_range.length);
                    return Err(error);
                }
                self.$arena.release(parts.range);
                self.collections.release(parts.range);
                self.release_collection_quota(parts.range.length);
                let element_bytes = size_of_val(&stored) as u64;
                self.charge_live_payload((new_range.length as u64).saturating_mul(element_bytes));
                self.release_live_payload(
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
                grow_insert_scalar!(i32_collections, RuntimeValue::I32(value) => value);
            }
            CollectionStorage::I64 => {
                grow_insert_scalar!(i64_collections, RuntimeValue::I64(value) => value);
            }
            CollectionStorage::F32 => {
                grow_insert_scalar!(f32_collections, RuntimeValue::F32(value) => value);
            }
            CollectionStorage::F64 => {
                grow_insert_scalar!(f64_collections, RuntimeValue::F64(value) => value);
            }
            CollectionStorage::Bool => {
                grow_insert_scalar!(
                    bool_collections,
                    RuntimeValue::Bool(value) => u8::from(value)
                );
            }
            CollectionStorage::Rune => {
                grow_insert_scalar!(rune_collections, RuntimeValue::Rune(value) => value);
            }
            CollectionStorage::String => {
                self.shade_on_write(element);
                grow_insert_scalar!(
                    string_collections,
                    RuntimeValue::String { reference, hash } => (reference, hash)
                );
            }
            CollectionStorage::Ref => {
                self.shade_on_write(element);
                grow_insert_scalar!(
                    ref_collections,
                    RuntimeValue::Ref(reference) => reference
                );
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
                    ref_collections,
                    RuntimeValue::NamedRef { .. } => reference
                );
            }
            CollectionStorage::Values => {}
        }
        let Some(stride) = parts.rows() else {
            self.shade_on_write(element);
            let capacity =
                grown_array_capacity(parts.range.length, length, self.max_collection_length);
            self.regrow_array(parts.reference, parts.range, current, capacity, |values| {
                values.copy_within(index..current, index + 1);
                values[index] = element;
            })?;
            return self.set_array_length(parts.reference, length);
        };
        let row = self.struct_row(parts.element_struct_type()?, stride, element)?;
        for field in &row[..stride] {
            self.shade_on_write(*field);
        }
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
            return Ok(());
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
        self.set_array_length(parts.reference, length)
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
            return Ok(removed);
        };
        // Materialize the row before mutating anything: a failed struct
        // allocation must leave the array untouched (failure atomicity).
        let removed = self.array_get(value, index)?;
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
        let live_cells = parts.length * parts.stride();
        self.typed_collection_clear(parts.storage, parts.range, 0..live_cells)?;
        self.set_array_length(parts.reference, 0)
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
        self.charge_live_payload((new_range.length as u64).saturating_mul(value_bytes));
        self.release_live_payload((old_range.length as u64).saturating_mul(value_bytes));
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
    ) -> Result<&[RuntimeValue], HeapError> {
        let parts = self.array_parts(value)?;
        if index >= parts.length {
            return Err(HeapError::IndexOutOfBounds {
                index,
                length: parts.length,
            });
        }
        if let Some(stride) = parts.rows() {
            let cells = self.collections.values(parts.range)?;
            return Ok(&cells[index * stride..(index + 1) * stride]);
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
        fields
            .get(field)
            .copied()
            .ok_or(HeapError::IndexOutOfBounds {
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
        row[..stride].copy_from_slice(fields);
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
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                storage,
                range,
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                self.typed_collection_view(*storage, *element_type, *range)
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    pub fn buffer_len(&self, value: RuntimeValue) -> Result<usize, HeapError> {
        Ok(self.buffer_values(value)?.len())
    }

    pub fn buffer_get(&self, value: RuntimeValue, index: usize) -> Result<RuntimeValue, HeapError> {
        let (_, element_type, storage) = self.buffer_metadata(value)?;
        let (_, range) = self.buffer_range(value)?;
        self.typed_collection_get(storage, element_type, range, index)
    }

    pub fn buffer_set(
        &mut self,
        value: RuntimeValue,
        index: usize,
        replacement: RuntimeValue,
    ) -> Result<(), HeapError> {
        let (_, element_type, storage) = self.buffer_metadata(value)?;
        let (_, range) = self.buffer_range(value)?;
        self.typed_collection_set(storage, element_type, range, index, replacement)
    }

    pub fn buffer_slice(
        &mut self,
        value: RuntimeValue,
        start: usize,
        length: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let (type_id, element_type, storage) = self.buffer_metadata(value)?;
        let (_, source_range) = self.buffer_range(value)?;
        let end = checked_collection_end(start, length, source_range.length)?;
        // Reserve the object slot before claiming/copying collection storage,
        // so a full heap cannot strand an otherwise unreachable arena range.
        let mut heap = self.preflight(1)?;
        let range = self.claim_typed_collection(storage, length)?;
        for (destination, source) in (start..end).enumerate() {
            let item = self.typed_collection_get(storage, element_type, source_range, source)?;
            if let Err(error) =
                self.typed_collection_set(storage, element_type, range, destination, item)
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
        let source_end =
            checked_collection_end(source_start, length, self.buffer_range(source)?.1.length)?;
        let destination_end = checked_collection_end(
            destination_start,
            length,
            self.buffer_range(destination)?.1.length,
        )?;
        let (_, source_range) = self.buffer_range(source)?;
        let (_, destination_range) = self.buffer_range(destination)?;
        let source_absolute = source_range.start + source_start;
        let destination_absolute = destination_range.start + destination_start;
        let copied = source_end - source_start;
        let element_type = destination_metadata.1;
        let storage = destination_metadata.2;
        let element_bytes = storage.cell_size();
        self.counters.collection_relocation_bytes = self
            .counters
            .collection_relocation_bytes
            .saturating_add((copied * element_bytes) as u64);
        macro_rules! copy_typed {
            ($arena:ident) => {
                self.$arena.values.copy_within(
                    source_absolute..source_absolute + copied,
                    destination_absolute,
                )
            };
        }
        match storage {
            CollectionStorage::I32 => copy_typed!(i32_collections),
            CollectionStorage::I64 => copy_typed!(i64_collections),
            CollectionStorage::F32 => copy_typed!(f32_collections),
            CollectionStorage::F64 => copy_typed!(f64_collections),
            CollectionStorage::Bool => copy_typed!(bool_collections),
            CollectionStorage::Rune => copy_typed!(rune_collections),
            CollectionStorage::String => copy_typed!(string_collections),
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                copy_typed!(ref_collections);
            }
            CollectionStorage::Values => copy_typed!(collections),
        }
        // G1 barrier: every reference just published into the destination
        // extent is shaded; the gray queue tolerates duplicates.
        if self.gc_phase == GcPhase::Mark {
            for offset in 0..(source_end - source_start) {
                if let Some(value) = self
                    .typed_collection_view(storage, element_type, destination_range)?
                    .get(destination_start + offset)
                {
                    self.shade_on_write(value);
                }
            }
        }
        debug_assert_eq!(destination_end - destination_start, length);
        Ok(())
    }

    fn buffer_metadata(
        &self,
        value: RuntimeValue,
    ) -> Result<(StableId, nexa_bytecode::ValueType, CollectionStorage), HeapError> {
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
                storage,
                ..
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok((type_id, *element_type, *storage))
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn buffer_range(&self, value: RuntimeValue) -> Result<(GcRef, CollectionRange), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                range,
                ..
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok((reference, *range))
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
        // G6 admission: the initial typed slot extent counts toward the ceiling.
        self.ensure_payload_headroom(
            (initial_capacity as u64).saturating_mul(size_of::<Option<MapEntry>>() as u64),
        )?;
        let mut reservation = self.preflight(1)?;
        let slots = self.map_slots.claim(initial_capacity)?;
        let storage = self.claim_map_storage(VmMap {
            type_id,
            key_type,
            value_type,
            slots,
            length: 0,
            rehash: None,
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
        let empty: &[Option<MapEntry>] = &[];
        let (old, new) = map.rehash.as_ref().map_or((empty, empty), |rehash| {
            (
                self.map_slots.slots(rehash.old_slots),
                self.map_slots.slots(rehash.new_slots),
            )
        });
        Ok(MapEntries {
            current: self.map_slots.slots(map.slots),
            old,
            new,
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
        let hash = self.runtime_value_hash(key)?;
        let map = self.map(value)?;
        Ok(self
            .find_map_entry(map, key, hash)?
            .map(|location| map_entry(map, &self.map_slots, location).value))
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
        // G1 barrier: shading before the outcome branches is conservative
        // (a pending rehash publishes nothing yet) but always safe.
        self.shade_on_write(key);
        self.shade_on_write(replacement);
        // A retry resumes only the bounded rehash chunk. Looking up the key
        // again here would repeat an entire map scan on every retry and make
        // deterministic attempt-based fuel either free or overcharged.
        if self.map(value)?.rehash.is_some() {
            let storage = self.map_storage_index(value)?;
            let released = progress_map_rehash(
                self.maps[storage]
                    .as_mut()
                    .expect("validated map storage exists"),
                &mut self.map_slots,
            )?;
            // G6: rehash completion just released the old typed extent.
            self.release_live_payload(released);
            return Ok(MapSetOutcome::RehashPending);
        }

        let hash = self.runtime_value_hash(key)?;
        let location = {
            let map = self.map(value)?;
            self.find_map_entry(map, key, hash)?
        };
        if let Some(location) = location {
            let storage = self.map_storage_index(value)?;
            let map = self.maps[storage]
                .as_ref()
                .expect("validated map storage exists");
            let range = map_location_range(map, location);
            self.map_slots.slots_mut(range)[map_location_index(location)]
                .as_mut()
                .expect("located map entry exists")
                .value = replacement;
            return Ok(MapSetOutcome::Complete);
        }

        if self.map(value)?.length >= self.max_collection_length {
            return Err(HeapError::CollectionTooLarge {
                length: self.map(value)?.length.saturating_add(1),
                max_length: self.max_collection_length,
            });
        }
        if map_needs_rehash(self.map(value)?) {
            let old_capacity = self.map(value)?.slots.length;
            let new_capacity = next_map_capacity(self.map(value)?, self.max_collection_length)
                .expect("map needs rehash");
            if new_capacity > old_capacity {
                let entry_bytes = size_of::<Option<MapEntry>>() as u64;
                // G6 admission and charge: the new slot vector joins the
                // map's footprint now; the old vector is released when the
                // rehash completes.
                self.ensure_payload_headroom((new_capacity as u64).saturating_mul(entry_bytes))?;
                let new_slots = self.map_slots.claim(new_capacity)?;
                self.charge_live_payload(
                    u64::try_from(new_slots.length)
                        .unwrap_or(u64::MAX)
                        .saturating_mul(entry_bytes),
                );
                self.counters.map_slot_allocations = self
                    .counters
                    .map_slot_allocations
                    .saturating_add(new_capacity as u64);
                let storage = self.map_storage_index(value)?;
                let map = self.maps[storage]
                    .as_mut()
                    .expect("validated map storage exists");
                let old_slots = map.slots;
                map.slots = CollectionRange::default();
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
        self.counters.map_slot_allocations = self.counters.map_slot_allocations.saturating_add(1);
        let storage = self.map_storage_index(value)?;
        let range = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists")
            .slots;
        insert_map_entry(self.map_slots.slots_mut(range), entry)?;
        self.maps[storage]
            .as_mut()
            .expect("validated map storage exists")
            .length += 1;
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
        let storage = self.map_storage_index(value)?;
        let map = self.maps[storage]
            .as_ref()
            .expect("validated map storage exists");
        let range = map_location_range(map, location);
        let entry = match location {
            MapLocation::RehashOld(_) => self.map_slots.slots_mut(range)
                [map_location_index(location)]
            .take()
            .expect("located map entry exists"),
            MapLocation::Current(_) | MapLocation::RehashNew(_) => remove_probed_entry(
                self.map_slots.slots_mut(range),
                map_location_index(location),
            ),
        };
        self.maps[storage]
            .as_mut()
            .expect("validated map storage exists")
            .length -= 1;
        Ok(Some(entry.value))
    }

    pub fn map_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        let storage = self.map_storage_index(value)?;
        let map = self.maps[storage]
            .as_mut()
            .expect("validated map storage exists");
        self.map_slots.slots_mut(map.slots).fill(None);
        // G6: dropping an in-flight rehash releases both side extents;
        // the primary extent keeps its capacity.
        let rehash = map.rehash.take();
        map.length = 0;
        let released = rehash.map_or(0, |rehash| {
            let bytes = (rehash
                .old_slots
                .length
                .saturating_add(rehash.new_slots.length) as u64)
                .saturating_mul(size_of::<Option<MapEntry>>() as u64);
            self.map_slots.release(rehash.old_slots);
            self.map_slots.release(rehash.new_slots);
            bytes
        });
        self.release_live_payload(released);
        Ok(())
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
        for index in 0..self.slots.len() {
            let condemned = {
                let slot = &mut self.slots[index];
                (slot.object.is_some() && !slot.marked)
                    .then(|| slot.object.take().expect("presence checked"))
            };
            if let Some(object) = condemned {
                // G4: payload bytes are measured before the drop; the slot
                // header stays pool-owned and is not "released".
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
        // G6 drift pin: the incremental gauge must agree with a full
        // re-derivation at every full-collection boundary.
        debug_assert_eq!(
            self.live_payload_bytes,
            self.recompute_live_payload_bytes(),
            "the live payload gauge drifted from ground truth"
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
        self.gc_marked = 0;
        self.gc_reclaimed = 0;
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

    /// G6: caps [`Self::live_payload_bytes`]; growth past the ceiling is
    /// refused with `CapacityExhausted` at the fallible allocation and
    /// growth boundaries.
    pub const fn set_max_heap_bytes(&mut self, limit: u64) {
        self.max_heap_bytes = limit;
    }

    /// G6 admission check for `additional` payload bytes about to be
    /// owned by live objects.
    fn ensure_payload_headroom(&self, additional: u64) -> Result<(), HeapError> {
        if self.live_payload_bytes.saturating_add(additional) > self.max_heap_bytes {
            return Err(HeapError::CapacityExhausted);
        }
        Ok(())
    }

    /// G6 gauge maintenance; saturating on both edges so accounting can
    /// never panic even if a footprint model bug under-releases.
    fn charge_live_payload(&mut self, bytes: u64) {
        self.live_payload_bytes = self.live_payload_bytes.saturating_add(bytes);
    }

    fn release_live_payload(&mut self, bytes: u64) {
        self.live_payload_bytes = self.live_payload_bytes.saturating_sub(bytes);
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
        if let Some(child) = value_reference(value)
            && Self::enqueue_gray(&mut self.slots, &mut self.mark_scratch, child)
        {
            self.gc_marked += 1;
            self.gc_barrier_shades = self.gc_barrier_shades.saturating_add(1);
        }
    }

    /// One budgeted step of the incremental cycle (G1):
    /// `Idle -> Mark -> Sweep -> Idle`, spanning as many calls as the
    /// budget requires.
    ///
    /// Every mark step re-seeds the current precise roots before draining
    /// the gray queue, so the root set may change between steps (task
    /// suspend points move); Sweep begins only after a step whose freshly
    /// seeded queue drains completely, which together with the insertion
    /// barrier keeps every reachable object marked. Objects allocated
    /// while a cycle is active are born marked and survive to the next
    /// cycle.
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
        if self.gc_phase == GcPhase::Idle {
            for slot in &mut self.slots {
                slot.marked = false;
            }
            self.gc_marked = 0;
            self.gc_reclaimed = 0;
            self.gc_sweep_cursor = 0;
            self.mark_scratch.clear();
            self.gc_phase = GcPhase::Mark;
        }
        if self.gc_phase == GcPhase::Mark {
            let mut queue = std::mem::take(&mut self.mark_scratch);
            for root in roots.iter() {
                self.validate_reference(root)?;
                if Self::enqueue_gray(&mut self.slots, &mut queue, root) {
                    report.roots_seeded += 1;
                }
            }
            let grayed = self.mark_step(&mut queue, &mut budget);
            self.mark_scratch = queue;
            let grayed = grayed?;
            self.gc_marked += grayed + report.roots_seeded;
            report.objects_marked = grayed + report.roots_seeded;
            // Conservative transition: only a step with leftover budget can
            // prove the freshly seeded queue truly drained.
            if self.mark_scratch.is_empty() && budget.available() {
                self.gc_phase = GcPhase::Sweep;
                self.gc_sweep_cursor = 0;
            }
        }
        if self.gc_phase == GcPhase::Sweep {
            while budget.available() && self.gc_sweep_cursor < self.slots.len() {
                let index = self.gc_sweep_cursor;
                self.gc_sweep_cursor += 1;
                report.slots_swept += 1;
                let mut payload = 0;
                let condemned = {
                    let slot = &mut self.slots[index];
                    (slot.object.is_some() && !slot.marked)
                        .then(|| slot.object.take().expect("presence checked"))
                };
                if let Some(object) = condemned {
                    // G4: measured before the drop, mirroring the full
                    // collection path byte for byte.
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
            // G6: the sweep slice just released exactly the bytes it
            // reported for this step.
            self.release_live_payload(report.bytes_reclaimed);
            if self.gc_sweep_cursor >= self.slots.len() {
                report.completed = Some(CollectionStats {
                    marked: self.gc_marked,
                    reclaimed: self.gc_reclaimed,
                    live: self.live_len(),
                });
                // Latch the cycle total before the reset clears the
                // accumulator; full collection sets the same latch.
                self.last_cycle_bytes_reclaimed = self.gc_bytes_reclaimed;
                self.reset_incremental_cycle();
            }
        }
        report.barrier_shades = self.gc_barrier_shades;
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
                for (child, _) in self.string_collections.values(range)?[..live]
                    .iter()
                    .copied()
                {
                    enqueue(child);
                }
            }
            CollectionStorage::Ref | CollectionStorage::NamedRef => {
                for child in self.ref_collections.values(range)?[..live].iter().copied() {
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
                Object::Buffer { storage, range, .. } => {
                    let range = *range;
                    grayed +=
                        self.enqueue_collection_children(queue, *storage, range, range.length)?;
                }
                Object::Struct { range, .. } | Object::Class { range, .. } => {
                    let range = *range;
                    for index in 0..range.length {
                        let value = self.collections.values(range)?[index];
                        if let Some(child) = value_reference(value)
                            && Self::enqueue_gray(&mut self.slots, queue, child)
                        {
                            grayed += 1;
                        }
                    }
                }
                Object::Map { storage } => {
                    let map = self
                        .maps
                        .get(*storage as usize)
                        .and_then(Option::as_ref)
                        .ok_or(HeapError::InvalidReference(reference))?;
                    let slots = &mut self.slots;
                    map.trace_references(&self.map_slots, &mut |child| {
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

    fn scalar_arena_reserved_bytes(&self) -> u64 {
        (self.i32_collections.values.capacity() as u64)
            .saturating_mul(size_of::<i32>() as u64)
            .saturating_add(
                (self.i64_collections.values.capacity() as u64)
                    .saturating_mul(size_of::<i64>() as u64),
            )
            .saturating_add(
                (self.f32_collections.values.capacity() as u64)
                    .saturating_mul(size_of::<u32>() as u64),
            )
            .saturating_add(
                (self.f64_collections.values.capacity() as u64)
                    .saturating_mul(size_of::<u64>() as u64),
            )
            .saturating_add(self.bool_collections.values.capacity() as u64)
            .saturating_add(
                (self.rune_collections.values.capacity() as u64)
                    .saturating_mul(size_of::<u32>() as u64),
            )
            .saturating_add(
                (self.string_collections.values.capacity() as u64).saturating_mul(size_of::<(
                    GcRef,
                    u64,
                )>(
                )
                    as u64),
            )
            .saturating_add(
                (self.ref_collections.values.capacity() as u64)
                    .saturating_mul(size_of::<GcRef>() as u64),
            )
    }

    /// `GC_V1` heap byte accounting by category (G4). One full walk over the
    /// slot pool plus O(1) arena metadata - inspection-grade, never called
    /// from the hot path or from inside a bounded GC step.
    #[must_use]
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
                Object::Map { .. } => {
                    inspection.map_bytes = inspection
                        .map_bytes
                        .saturating_add(self.object_payload_bytes(object));
                }
                Object::Class { .. } | Object::Struct { .. } => {
                    inspection.class_payload_bytes = inspection
                        .class_payload_bytes
                        .saturating_add(object.payload_bytes());
                    generic_arena_live = generic_arena_live.saturating_add(object.payload_bytes());
                }
                Object::Enum { .. } => {}
            }
        }
        let occupied_map_headers = self.maps.iter().filter(|map| map.is_some()).count() as u64;
        inspection.object_header_bytes = occupied
            .saturating_mul(slot_bytes)
            .saturating_add(occupied_map_headers.saturating_mul(map_header_bytes));
        let pool_slots = self.slots.capacity().max(self.slots.len()) as u64;
        let vacant_pool_bytes = pool_slots
            .saturating_sub(occupied)
            .saturating_mul(slot_bytes);
        let map_pool_slots = self.maps.capacity().max(self.maps.len()) as u64;
        let vacant_map_pool_bytes = map_pool_slots
            .saturating_sub(occupied_map_headers)
            .saturating_mul(map_header_bytes);
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
            .saturating_add(arena_free_bytes)
            .saturating_add(map_arena_free_bytes);
        inspection.profiler_bytes = crate::profiler::thread_storage_bytes();
        inspection
    }

    pub(crate) fn failure_trigger(&self, point: RuntimeFailurePoint) -> bool {
        self.failure_injector.trigger(point)
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

fn map_needs_rehash(map: &VmMap) -> bool {
    map.slots.length == 0
        || map.length.saturating_add(1).saturating_mul(4) > map.slots.length.saturating_mul(3)
}

fn next_map_capacity(map: &VmMap, max_collection_length: usize) -> Option<usize> {
    if !map_needs_rehash(map) {
        return None;
    }
    let maximum_capacity = max_collection_length
        .saturating_mul(2)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    Some(
        map.slots
            .length
            .saturating_mul(2)
            .max(1)
            .min(maximum_capacity),
    )
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

/// Advances one bounded rehash chunk. Returns the payload bytes released
/// by completing the rehash (the dropped old slot vector), zero while the
/// rehash is still in flight (G6).
fn progress_map_rehash(map: &mut VmMap, arena: &mut MapSlotArena) -> Result<u64, HeapError> {
    const REHASH_CHUNK: usize = 8;
    let rehash = map.rehash.as_mut().expect("rehash state checked by caller");
    let end = rehash
        .cursor
        .saturating_add(REHASH_CHUNK)
        .min(rehash.old_slots.length);
    for index in rehash.cursor..end {
        if let Some(entry) = arena.slots_mut(rehash.old_slots)[index].take() {
            insert_map_entry(arena.slots_mut(rehash.new_slots), entry)?;
        }
    }
    rehash.cursor = end;
    if end == rehash.old_slots.length {
        let released =
            (rehash.old_slots.length as u64).saturating_mul(size_of::<Option<MapEntry>>() as u64);
        arena.release(rehash.old_slots);
        map.slots = rehash.new_slots;
        map.rehash = None;
        return Ok(released);
    }
    Ok(0)
}

fn map_entry(map: &VmMap, arena: &MapSlotArena, location: MapLocation) -> MapEntry {
    arena.slots(map_location_range(map, location))[map_location_index(location)]
        .expect("located map entry exists")
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
fn remove_probed_entry(slots: &mut [Option<MapEntry>], index: usize) -> MapEntry {
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
            hole = cursor;
        }
        cursor = (cursor + 1) % len;
    }
    removed
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
        GcRoots, Heap, HeapError, MapEntry, MapSetOutcome, Object, insert_map_entry,
        remove_probed_entry,
    };
    use crate::{RuntimeFailurePoint, RuntimeValue};

    fn probe_entry(key: i32, hash: u64) -> MapEntry {
        MapEntry {
            key: RuntimeValue::I32(key),
            value: RuntimeValue::I32(key),
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
        // WP49 amortized growth: the first push grows an empty extent
        // (zero live elements copied) and the second lands in spare
        // capacity, so no relocation bytes accrue at all.
        assert_eq!(counters.collection_relocation_bytes, 0);

        // Counters are monotonic work totals: rollback keeps them.
        heap.restore_checkpoint(checkpoint);
        assert_eq!(heap.vm_allocation_counters(), counters);
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
            .allocate_struct_row_array(
                array_type,
                element,
                std::num::NonZeroU8::new(2).expect("non-zero"),
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
        assert_eq!(fields[0], RuntimeValue::I32(0));
        let RuntimeValue::String {
            reference: label, ..
        } = fields[1]
        else {
            panic!("label field stays a string reference");
        };
        assert_eq!(heap.string(label), Ok("row-label"));

        // The borrowed views agree with the materialized read.
        assert_eq!(
            heap.array_element_fields(array, 0).unwrap()[0],
            RuntimeValue::I32(0)
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
            .allocate_struct_row_array(
                array_type,
                element,
                std::num::NonZeroU8::new(1).expect("non-zero"),
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
            heap.struct_fields(removed).unwrap()[0],
            RuntimeValue::I32(5)
        );
        let popped = heap.array_pop(array).unwrap();
        assert_eq!(
            heap.struct_fields(popped).unwrap()[0],
            RuntimeValue::I32(20)
        );
        assert_eq!(heap.array_len(array), Ok(1));
        assert_eq!(
            heap.array_element_fields(array, 0).unwrap()[0],
            RuntimeValue::I32(11)
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
        heap.rollback_host_transaction();
        assert!(heap.maps.iter().all(Option::is_none));
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
}
