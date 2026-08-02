use std::collections::{BTreeMap, VecDeque};
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
    // WP73: stream child references into the mark queue instead of
    // materializing a temporary Vec per object during GC.
    fn trace_references(&self, visit: &mut impl FnMut(GcRef)) {
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
        for reference in current.chain(rehash).filter_map(value_reference) {
            visit(reference);
        }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CollectionArenaInspection {
    pub capacity: usize,
    pub free_elements: usize,
    pub free_ranges: usize,
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
        /// Capacity extent inside the collection arena (WP48); `length`
        /// tracks the live prefix so pushes grow amortized (WP49).
        range: CollectionRange,
        length: usize,
    },
    Buffer {
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        range: CollectionRange,
    },
}

impl Object {
    // WP73: allocation-free reference traversal for the GC mark phase.
    // Array/Buffer extents live in the collection arena and are traced
    // directly inside `collect`, which owns the arena borrow.
    fn trace_references(&self, visit: &mut impl FnMut(GcRef)) {
        match self {
            // MAX_CLASS_FIELDS == MAX_STRUCT_FIELDS, so both inline field
            // arrays share one arm.
            Self::Class {
                fields,
                field_count,
                ..
            }
            | Self::Struct {
                fields,
                field_count,
                ..
            } => {
                for reference in fields[..usize::from(*field_count)]
                    .iter()
                    .copied()
                    .filter_map(value_reference)
                {
                    visit(reference);
                }
            }
            Self::Map(map) => map.trace_references(visit),
            Self::Enum { payload, .. } => {
                for reference in payload.iter().copied().filter_map(value_reference) {
                    visit(reference);
                }
            }
            Self::Array { .. } | Self::Buffer { .. } | Self::String(_) | Self::I32Array(_) => {}
        }
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

/// Per-step work budget for one incremental collection step (G1 counts
/// slot-shaped work units; byte and duration budgets are later G work).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcBudget {
    pub max_steps: usize,
}

/// Telemetry for one incremental step: work actually performed, the phase
/// after the step, and the whole-cycle stats when the cycle completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncrementalGcReport {
    pub roots_seeded: usize,
    pub objects_marked: usize,
    pub slots_swept: usize,
    pub barrier_shades: u64,
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
    max_objects: u32,
    max_string_bytes: usize,
    max_collection_length: usize,
    collections: CollectionArena,
    host_staging: Vec<GcRef>,
    host_transaction_active: bool,
    failure_injector: RuntimeFailureInjector,
    counters: VmAllocationCounters,
    /// WP56 literal memoization: content-keyed cache of previously loaded
    /// string constants. Entries are NOT roots; a hit revalidates the
    /// generation-protected reference and its content, and a collected or
    /// repurposed slot simply falls back to a fresh allocation. No root
    /// management, no unload bookkeeping, no leak.
    string_literal_cache: BTreeMap<String, GcRef>,
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
    collections: CollectionArena,
    host_staging: Vec<GcRef>,
    host_transaction_active: bool,
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
        let mut host_staging = Vec::with_capacity(self.host_staging.capacity());
        host_staging.extend_from_slice(&self.host_staging);
        HeapCheckpoint {
            slots,
            free,
            collections: self.collections.checkpoint_clone(),
            host_staging,
            host_transaction_active: self.host_transaction_active,
        }
    }

    pub(crate) fn restore_checkpoint(&mut self, checkpoint: HeapCheckpoint) {
        // The snapshot predates any in-flight incremental cycle state; the
        // gray queue and sweep cursor would reference rolled-back slots.
        self.reset_incremental_cycle();
        self.slots = checkpoint.slots;
        self.free = checkpoint.free;
        self.collections = checkpoint.collections;
        self.host_staging = checkpoint.host_staging;
        self.host_transaction_active = checkpoint.host_transaction_active;
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
            max_objects,
            max_string_bytes,
            max_collection_length: max_collection_length.min(i32::MAX as usize),
            collections: CollectionArena::new(
                max_collection_elements,
                max_collection_ranges.max(max_objects as usize + 1),
            ),
            host_staging: Vec::with_capacity(max_objects as usize),
            host_transaction_active: false,
            failure_injector: RuntimeFailureInjector::default(),
            counters: VmAllocationCounters::default(),
            string_literal_cache: BTreeMap::new(),
            mark_scratch: VecDeque::with_capacity(max_objects as usize),
            gc_phase: GcPhase::Idle,
            gc_sweep_cursor: 0,
            gc_marked: 0,
            gc_reclaimed: 0,
            gc_barrier_shades: 0,
        }
    }

    /// Cumulative allocation/copy work performed by this heap (WP13).
    #[must_use]
    pub const fn vm_allocation_counters(&self) -> VmAllocationCounters {
        self.counters
    }

    pub fn allocate_string(&mut self, value: &str) -> Result<GcRef, HeapError> {
        self.validate_string_length(value.len())?;
        let mut reservation = self.preflight(1)?;
        let value = value.to_owned();
        Ok(self.commit(&mut reservation, Object::String(value)))
    }

    /// WP56 literal load: returns the cached live copy of a string constant
    /// when its content still matches, otherwise allocates and re-caches.
    /// Hot literal loads therefore create no new String objects.
    pub fn load_string_literal(&mut self, value: &str) -> Result<GcRef, HeapError> {
        if let Some(reference) = self.string_literal_cache.get(value).copied()
            && matches!(
                self.slots
                    .get(reference.index as usize)
                    .filter(|slot| slot.generation == reference.generation)
                    .and_then(|slot| slot.object.as_ref()),
                Some(Object::String(cached)) if cached == value
            )
        {
            return Ok(reference);
        }
        let reference = self.allocate_string(value)?;
        self.string_literal_cache
            .insert(value.to_owned(), reference);
        Ok(reference)
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
        let mut collection = self.preflight_collection(part_count)?;
        let mut parts = Vec::new();
        if parts.try_reserve_exact(part_count).is_err() {
            self.release_collection_reservation(&mut collection);
            return Err(HeapError::CapacityExhausted);
        }
        {
            let value = self.string(value)?;
            let delimiter = self.string(delimiter)?;
            parts.extend(value.split(delimiter).map(str::to_owned));
        }
        debug_assert_eq!(parts.len(), part_count);
        for part in parts {
            let value = match self.commit_owned_string(&mut objects, part) {
                Ok(value) => value,
                Err(error) => {
                    self.release_collection_reservation(&mut collection);
                    return Err(error);
                }
            };
            if let Err(error) = self.commit_collection_value(&mut collection, value) {
                self.release_collection_reservation(&mut collection);
                return Err(error);
            }
        }
        let range = collection.range;
        Self::complete_collection_reservation(&mut collection)?;
        self.commit_array_reserved(
            &mut objects,
            nexa_bytecode::array_type(nexa_bytecode::ValueType::String),
            nexa_bytecode::ValueType::String,
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
        self.counters.object_allocations = self.counters.object_allocations.saturating_add(1);
        match &object {
            Object::String(value) => {
                self.counters.string_allocations =
                    self.counters.string_allocations.saturating_add(1);
                self.counters.string_copy_bytes = self
                    .counters
                    .string_copy_bytes
                    .saturating_add(value.len() as u64);
            }
            Object::Class { .. } => {
                self.counters.class_allocations = self.counters.class_allocations.saturating_add(1);
            }
            Object::Array { .. } | Object::Buffer { .. } | Object::I32Array(_) => {
                self.counters.collection_storage_allocations = self
                    .counters
                    .collection_storage_allocations
                    .saturating_add(1);
            }
            Object::Map(_) => {
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
                self.mark_scratch.push_back(child);
                self.gc_barrier_shades = self.gc_barrier_shades.saturating_add(1);
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
        let reference = self.commit(reservation, Object::String(value));
        let hash = self.string_hash(reference)?;
        Ok(RuntimeValue::String { reference, hash })
    }

    pub fn preflight_collection(
        &mut self,
        element_count: usize,
    ) -> Result<CollectionReservation, HeapError> {
        let range = self
            .collections
            .find_free(element_count)
            .ok_or(HeapError::CapacityExhausted)?;
        self.collections.claim(range)?;
        Ok(CollectionReservation {
            range,
            written: 0,
            claimed: true,
        })
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

    pub(crate) fn reserve_collection_segment(
        reservation: &mut CollectionReservation,
        length: usize,
    ) -> Result<CollectionRange, HeapError> {
        let start = reservation.written;
        let end = start
            .checked_add(length)
            .ok_or(HeapError::CapacityExhausted)?;
        if end > reservation.range.length {
            return Err(HeapError::IndexOutOfBounds {
                index: end,
                length: reservation.range.length,
            });
        }
        reservation.written = end;
        Ok(CollectionRange {
            start: reservation.range.start + start,
            length,
        })
    }

    pub(crate) fn write_collection_at(
        &mut self,
        range: CollectionRange,
        index: usize,
        value: RuntimeValue,
    ) -> Result<(), HeapError> {
        let values = self.collections.values_mut(range)?;
        let length = values.len();
        let slot = values
            .get_mut(index)
            .ok_or(HeapError::IndexOutOfBounds { index, length })?;
        *slot = value;
        Ok(())
    }

    pub(crate) fn release_collection_reservation(
        &mut self,
        reservation: &mut CollectionReservation,
    ) {
        if reservation.claimed {
            self.collections.release(reservation.range);
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
        while let Some(reference) = self.host_staging.pop() {
            if let Some(slot) = self.slots.get_mut(reference.index as usize)
                && slot.generation == reference.generation
                && slot.object.take().is_some()
            {
                self.free.push(reference.index);
            }
        }
    }

    pub(crate) fn commit_array_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
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
                range,
                length,
            },
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
    }

    pub(crate) fn commit_buffer_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
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
        let mut collection = self.preflight_collection(values.len())?;
        for value in values {
            if let Err(error) = self.commit_collection_value(&mut collection, *value) {
                self.release_collection_reservation(&mut collection);
                return Err(error);
            }
        }
        let range = collection.range;
        Self::complete_collection_reservation(&mut collection)?;
        self.commit_array_reserved(reservation, type_id, element_type, range)
    }

    pub(crate) fn commit_buffer_values_reserved(
        &mut self,
        reservation: &mut HeapReservation,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        values: &[RuntimeValue],
    ) -> Result<RuntimeValue, HeapError> {
        let mut collection = self.preflight_collection(values.len())?;
        for value in values {
            if let Err(error) = self.commit_collection_value(&mut collection, *value) {
                self.release_collection_reservation(&mut collection);
                return Err(error);
            }
        }
        let range = collection.range;
        Self::complete_collection_reservation(&mut collection)?;
        self.commit_buffer_reserved(reservation, type_id, element_type, range)
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
                fields,
                field_count,
            } if *actual == type_id => Ok(&fields[..usize::from(*field_count)]),
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
            range: CollectionRange::default(),
            length: 0,
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
        let (_, range, length) = self.array_range(value)?;
        if index >= length {
            return Err(HeapError::IndexOutOfBounds { index, length });
        }
        self.shade_on_write(replacement);
        let values = self.collections.values_mut(range)?;
        values[index] = replacement;
        Ok(())
    }

    pub fn array_push(
        &mut self,
        value: RuntimeValue,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let (reference, range, current) = self.array_range(value)?;
        self.shade_on_write(element);
        let length = current
            .checked_add(1)
            .ok_or(HeapError::CollectionTooLarge {
                length: usize::MAX,
                max_length: self.max_collection_length,
            })?;
        self.validate_collection_length(length)?;
        if current < range.length {
            // WP49 amortized fast path: spare capacity, write in place.
            self.collections.values_mut(range)?[current] = element;
            self.set_array_length(reference, length)?;
            return Ok(());
        }
        let capacity = grown_array_capacity(range.length, length, self.max_collection_length);
        self.regrow_array(reference, range, current, capacity, |values| {
            values[current] = element;
        })?;
        self.set_array_length(reference, length)
    }

    pub fn array_pop(&mut self, value: RuntimeValue) -> Result<RuntimeValue, HeapError> {
        let (reference, range, length) = self.array_range(value)?;
        if length == 0 {
            return Err(HeapError::IndexOutOfBounds { index: 0, length });
        }
        let values = self.collections.values_mut(range)?;
        let result = values[length - 1];
        // Clear the vacated tail slot so no stale reference lingers in the
        // retained capacity extent.
        values[length - 1] = RuntimeValue::Unit;
        self.set_array_length(reference, length - 1)?;
        Ok(result)
    }

    pub fn array_insert(
        &mut self,
        value: RuntimeValue,
        index: usize,
        element: RuntimeValue,
    ) -> Result<(), HeapError> {
        let (reference, range, current) = self.array_range(value)?;
        self.shade_on_write(element);
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
        if current < range.length {
            let values = self.collections.values_mut(range)?;
            values.copy_within(index..current, index + 1);
            values[index] = element;
            self.counters.collection_relocation_bytes = self
                .counters
                .collection_relocation_bytes
                .saturating_add(((current - index) * std::mem::size_of::<RuntimeValue>()) as u64);
            self.set_array_length(reference, length)?;
            return Ok(());
        }
        let capacity = grown_array_capacity(range.length, length, self.max_collection_length);
        self.regrow_array(reference, range, current, capacity, |values| {
            values.copy_within(index..current, index + 1);
            values[index] = element;
        })?;
        self.set_array_length(reference, length)
    }

    pub fn array_remove(
        &mut self,
        value: RuntimeValue,
        index: usize,
    ) -> Result<RuntimeValue, HeapError> {
        let (reference, range, length) = self.array_range(value)?;
        if index >= length {
            return Err(HeapError::IndexOutOfBounds { index, length });
        }
        let values = self.collections.values_mut(range)?;
        let removed = values[index];
        values.copy_within(index + 1..length, index);
        values[length - 1] = RuntimeValue::Unit;
        self.counters.collection_relocation_bytes = self
            .counters
            .collection_relocation_bytes
            .saturating_add(((length - 1 - index) * std::mem::size_of::<RuntimeValue>()) as u64);
        self.set_array_length(reference, length - 1)?;
        Ok(removed)
    }

    pub fn array_clear(&mut self, value: RuntimeValue) -> Result<(), HeapError> {
        // WP50: clear retains capacity; live slots reset so no stale
        // references survive in the extent.
        let (reference, range, length) = self.array_range(value)?;
        let values = self.collections.values_mut(range)?;
        values[..length].fill(RuntimeValue::Unit);
        self.set_array_length(reference, 0)
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

    /// Deterministic (live, capacity) shape used by fuel settlement before
    /// an array mutation runs (WP49).
    pub fn array_fuel_shape(&self, value: RuntimeValue) -> Result<(usize, usize), HeapError> {
        let (_, range, length) = self.array_range(value)?;
        Ok((length, range.length))
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
                range,
                length,
            } if *actual == type_id && type_id == nexa_bytecode::array_type(*element_type) => {
                let values = self.collections.values(*range)?;
                values.get(..*length).ok_or(HeapError::IndexOutOfBounds {
                    index: *length,
                    length: values.len(),
                })
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn array_range(
        &self,
        value: RuntimeValue,
    ) -> Result<(GcRef, CollectionRange, usize), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Array {
                type_id: actual,
                element_type,
                range,
                length,
            } if *actual == type_id && type_id == nexa_bytecode::array_type(*element_type) => {
                Ok((reference, *range, *length))
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
        let mut collection = self.preflight_collection(source.len())?;
        for value in source {
            self.commit_collection_value(&mut collection, *value)?;
        }
        let range = collection.range;
        Self::complete_collection_reservation(&mut collection)?;
        self.commit_buffer_reserved(&mut heap, type_id, element_type, range)
    }

    pub fn buffer_values(&self, value: RuntimeValue) -> Result<&[RuntimeValue], HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                range,
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                self.collections.values(*range)
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
        self.shade_on_write(replacement);
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
        // Reserve the object slot before claiming/copying collection storage,
        // so a full heap cannot strand an otherwise unreachable arena range.
        let mut heap = self.preflight(1)?;
        let mut collection = self.preflight_collection(length)?;
        for index in start..end {
            let item = self.buffer_values(value)?[index];
            self.commit_collection_value(&mut collection, item)?;
        }
        let range = collection.range;
        Self::complete_collection_reservation(&mut collection)?;
        self.commit_buffer_reserved(&mut heap, type_id, element_type, range)
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
            checked_collection_end(source_start, length, self.buffer_values(source)?.len())?;
        let destination_end = checked_collection_end(
            destination_start,
            length,
            self.buffer_values(destination)?.len(),
        )?;
        let (_, source_range) = self.buffer_range(source)?;
        let (_, destination_range) = self.buffer_range(destination)?;
        let source_absolute = source_range.start + source_start;
        let destination_absolute = destination_range.start + destination_start;
        self.counters.collection_relocation_bytes =
            self.counters.collection_relocation_bytes.saturating_add(
                ((source_end - source_start) * std::mem::size_of::<RuntimeValue>()) as u64,
            );
        self.collections.values.copy_within(
            source_absolute..source_absolute + (source_end - source_start),
            destination_absolute,
        );
        // G1 barrier: every reference just published into the destination
        // extent is shaded; the gray queue tolerates duplicates.
        if self.gc_phase == GcPhase::Mark {
            for offset in 0..(source_end - source_start) {
                let value = self.collections.values[destination_absolute + offset];
                self.shade_on_write(value);
            }
        }
        debug_assert_eq!(destination_end - destination_start, length);
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

    fn buffer_range(&self, value: RuntimeValue) -> Result<(GcRef, CollectionRange), HeapError> {
        let RuntimeValue::NamedRef { reference, type_id } = value else {
            return Err(invalid_value_reference());
        };
        match self.resolve(reference)? {
            Object::Buffer {
                type_id: actual,
                element_type,
                range,
            } if *actual == type_id && type_id == nexa_bytecode::buffer_type(*element_type) => {
                Ok((reference, *range))
            }
            _ => Err(HeapError::InvalidReference(reference)),
        }
    }

    fn buffer_values_mut(&mut self, value: RuntimeValue) -> Result<&mut [RuntimeValue], HeapError> {
        let (_, range) = self.buffer_range(value)?;
        self.collections.values_mut(range)
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
        let mut reservation = self.preflight(1)?;
        let slots = empty_map_slots(initial_capacity)?;
        let reference = self.commit(
            &mut reservation,
            Object::Map(VmMap {
                type_id,
                key_type,
                value_type,
                slots,
                length: 0,
                rehash: None,
            }),
        );
        Ok(RuntimeValue::NamedRef { reference, type_id })
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
            (rehash.old_slots.as_slice(), rehash.new_slots.as_slice())
        });
        Ok(MapEntries {
            current: &map.slots,
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
                    rehash.old_slots.len(),
                    rehash.new_slots.len(),
                    rehash
                        .old_slots
                        .len()
                        .saturating_sub(rehash.cursor)
                        .min(REHASH_CHUNK),
                )
            });
        let next_rehash_slots = if map.rehash.is_none() {
            next_map_capacity(map, self.max_collection_length)
                .filter(|capacity| *capacity > map.slots.len())
                .unwrap_or(0)
        } else {
            0
        };
        Ok(MapFuelShape {
            current_slots: map.slots.len(),
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
        // G1 barrier: shading before the outcome branches is conservative
        // (a pending rehash publishes nothing yet) but always safe.
        self.shade_on_write(key);
        self.shade_on_write(replacement);
        // A retry resumes only the bounded rehash chunk. Looking up the key
        // again here would repeat an entire map scan on every retry and make
        // deterministic attempt-based fuel either free or overcharged.
        if self.map(value)?.rehash.is_some() {
            progress_map_rehash(self.map_mut(value)?)?;
            return Ok(MapSetOutcome::RehashPending);
        }

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
        if map_needs_rehash(self.map(value)?) {
            let old_capacity = self.map(value)?.slots.len();
            let new_capacity = next_map_capacity(self.map(value)?, self.max_collection_length)
                .expect("map needs rehash");
            if new_capacity > old_capacity {
                let new_slots = empty_map_slots(new_capacity)?;
                self.counters.map_slot_allocations = self
                    .counters
                    .map_slot_allocations
                    .saturating_add(new_capacity as u64);
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
        self.counters.map_slot_allocations = self.counters.map_slot_allocations.saturating_add(1);
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
        for root in roots.iter() {
            self.validate_reference(root)?;
            queue.push_back(root);
        }
        let mut steps = usize::MAX;
        self.mark_step(queue, &mut steps)
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
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.object.is_some() && !slot.marked {
                if let Some(Object::Array { range, .. } | Object::Buffer { range, .. }) =
                    slot.object.take()
                {
                    self.collections.release(range);
                }
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

    fn reset_incremental_cycle(&mut self) {
        self.gc_phase = GcPhase::Idle;
        self.gc_sweep_cursor = 0;
        self.gc_marked = 0;
        self.gc_reclaimed = 0;
        self.mark_scratch.clear();
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

    /// G1 insertion barrier: while a mark phase is active, a reference
    /// value being published into a live object is shaded gray so the
    /// tri-color invariant holds under mutation. Duplicate shades are
    /// tolerated; the mark loop skips already-marked pops.
    fn shade_on_write(&mut self, value: RuntimeValue) {
        if self.gc_phase != GcPhase::Mark {
            return;
        }
        if let Some(child) = value_reference(value) {
            let already_marked = self
                .slots
                .get(child.index as usize)
                .is_some_and(|slot| slot.marked);
            if !already_marked {
                self.mark_scratch.push_back(child);
                self.gc_barrier_shades = self.gc_barrier_shades.saturating_add(1);
            }
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
        let mut steps = budget.max_steps;
        if steps == 0 {
            return Ok(report);
        }
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
            for root in roots.iter() {
                self.validate_reference(root)?;
                let already_marked = self.slots[root.index as usize].marked;
                if !already_marked {
                    self.mark_scratch.push_back(root);
                    report.roots_seeded += 1;
                }
            }
            let mut queue = std::mem::take(&mut self.mark_scratch);
            let marked = self.mark_step(&mut queue, &mut steps);
            self.mark_scratch = queue;
            let marked = marked?;
            self.gc_marked += marked;
            report.objects_marked = marked;
            if self.mark_scratch.is_empty() && steps > 0 {
                self.gc_phase = GcPhase::Sweep;
                self.gc_sweep_cursor = 0;
            }
        }
        if self.gc_phase == GcPhase::Sweep {
            while steps > 0 && self.gc_sweep_cursor < self.slots.len() {
                let index = self.gc_sweep_cursor;
                self.gc_sweep_cursor += 1;
                steps -= 1;
                report.slots_swept += 1;
                let slot = &mut self.slots[index];
                if slot.object.is_some() && !slot.marked {
                    if let Some(Object::Array { range, .. } | Object::Buffer { range, .. }) =
                        slot.object.take()
                    {
                        self.collections.release(range);
                    }
                    if let Some(generation) = slot.generation.checked_add(1) {
                        slot.generation = generation;
                        self.free
                            .push(u32::try_from(index).expect("slot indices originate as u32"));
                    }
                    self.gc_reclaimed += 1;
                }
            }
            if self.gc_sweep_cursor >= self.slots.len() {
                report.completed = Some(CollectionStats {
                    marked: self.gc_marked,
                    reclaimed: self.gc_reclaimed,
                    live: self.live_len(),
                });
                self.reset_incremental_cycle();
            }
        }
        report.barrier_shades = self.gc_barrier_shades;
        Ok(report)
    }

    /// Drains up to `steps` gray references; children stream straight
    /// back into the queue with no temporary allocation (WP73).
    fn mark_step(
        &mut self,
        queue: &mut VecDeque<GcRef>,
        steps: &mut usize,
    ) -> Result<usize, HeapError> {
        let mut marked = 0;
        while *steps > 0 {
            let Some(reference) = queue.pop_front() else {
                break;
            };
            *steps -= 1;
            let slot = self
                .slots
                .get_mut(reference.index as usize)
                .filter(|slot| slot.generation == reference.generation && slot.object.is_some())
                .ok_or(HeapError::InvalidReference(reference))?;
            if slot.marked {
                continue;
            }
            slot.marked = true;
            marked += 1;
            let object = slot.object.as_ref().expect("presence checked above");
            match object {
                Object::Array { range, length, .. } => {
                    let live = *length;
                    for child in self
                        .collections
                        .values(*range)?
                        .iter()
                        .take(live)
                        .copied()
                        .filter_map(value_reference)
                    {
                        queue.push_back(child);
                    }
                }
                Object::Buffer { range, .. } => {
                    for child in self
                        .collections
                        .values(*range)?
                        .iter()
                        .copied()
                        .filter_map(value_reference)
                    {
                        queue.push_back(child);
                    }
                }
                _ => object.trace_references(&mut |child| queue.push_back(child)),
            }
        }
        Ok(marked)
    }

    #[must_use]
    pub fn collection_inspection(&self) -> CollectionArenaInspection {
        CollectionArenaInspection {
            capacity: self.collections.capacity,
            free_elements: self
                .collections
                .free_ranges
                .iter()
                .map(|range| range.length)
                .sum(),
            free_ranges: self.collections.free_ranges.len(),
        }
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
            .len()
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
        let live = heap.allocate(Object::I32Array(vec![1, 2])).unwrap();
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
        let first = heap.load_string_literal("pooled").unwrap();
        let second = heap.load_string_literal("pooled").unwrap();
        assert_eq!(first, second, "hot literal loads share one object");
        assert_eq!(heap.vm_allocation_counters().string_allocations, 1);

        // Cache entries are not roots: an unrooted literal is collected,
        // and the next load safely re-allocates instead of resurrecting.
        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.reclaimed, 1);
        let third = heap.load_string_literal("pooled").unwrap();
        assert_ne!(first, third, "collected entries fall back to allocation");
        assert_eq!(heap.string(third), Ok("pooled"));
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
