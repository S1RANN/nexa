//! The trusted execution kernel (M5 K1) - the single sanctioned unsafe
//! module in the runtime.
//!
//! # Policy
//!
//! The workspace forbids `unsafe_code`; `nexa-runtime` alone lowers that
//! to `deny` so this one module can opt in explicitly, and `cargo xtask
//! repo-audit` fails if `unsafe` appears in any other `crates/` source.
//! Every `unsafe` block carries a `SAFETY:` justification grounded in an
//! invariant the verifier or the arena constructor has already proven -
//! the kernel removes *redundant re-checks* of proven facts, never a
//! semantic check. Fuel settlement, traps, GC reference validation, user
//! index bounds, write barriers, and reload epochs all stay exactly where
//! they were.
//!
//! # Trust boundary
//!
//! [`CheckedInterpreter`](crate::CheckedInterpreter) only executes
//! [`VerifiedModule`](nexa_verifier::VerifiedModule)s, and the verifier
//! proves for every instruction that its register operands are smaller
//! than the declared register count of the function it belongs to. Frames
//! are only pushed through [`FrameArena::push_call_at`], always with the
//! callee's declared register count, and that constructor enforces the
//! arena layout invariant this module relies on:
//!
//! > The top frame's register window is exactly the tail of the register
//! > store: `register_start + register_count == registers.len()`.
//!
//! (`push_call_at` resizes the store to precisely the new frame's end and
//! `pop` truncates back to `register_start`, so the invariant holds at
//! every instruction boundary.)
//!
//! Given a verified operand `register < register_count`, the access
//! `registers[register_start + register]` is therefore always in bounds.
//! Debug builds re-assert both facts on every access, and the WP36
//! differential gates, executable parity gate, and fuzz smoke corpus run
//! against debug assertions in CI.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::marker::PhantomData;
use std::mem::{MaybeUninit, align_of, size_of};

use crate::RuntimeValue;
use crate::frame::{FrameArena, FrameError};
use crate::heap::{CollectionRange, GcRef, HeapError};
use crate::interpreter::InterpreterError;

pub(crate) const STATIC_LEAF_REGISTER_CAPACITY: usize = 24;
pub(crate) type StaticLeafRegisters = [MaybeUninit<RuntimeValue>; STATIC_LEAF_REGISTER_CAPACITY];

#[inline]
pub(crate) const fn new_static_leaf_registers() -> StaticLeafRegisters {
    [MaybeUninit::uninit(); STATIC_LEAF_REGISTER_CAPACITY]
}

#[inline]
pub(crate) fn read_static_leaf(registers: &StaticLeafRegisters, register: u16) -> RuntimeValue {
    debug_assert!(usize::from(register) < registers.len());
    // SAFETY: `ExecutableModule::build` admits a static leaf only when its
    // verified register count fits this fixed array. The verifier proves
    // every operand is below that declared count and definite-initialization
    // proves every read is preceded by an argument copy or instruction
    // write. `RuntimeValue` is `Copy` and has no destructor.
    unsafe {
        registers
            .get_unchecked(usize::from(register))
            .assume_init_read()
    }
}

#[inline]
pub(crate) fn write_static_leaf(
    registers: &mut StaticLeafRegisters,
    register: u16,
    value: RuntimeValue,
) {
    debug_assert!(usize::from(register) < registers.len());
    // SAFETY: identical bound proof to `read_static_leaf`; writing initializes
    // the selected `MaybeUninit` cell and never drops stale bits.
    unsafe {
        registers
            .get_unchecked_mut(usize::from(register))
            .write(value);
    }
}

#[inline]
pub(crate) fn read_static_leaf_window(
    registers: &StaticLeafRegisters,
    base: u16,
    count: u16,
) -> &[RuntimeValue] {
    let start = usize::from(base);
    let count = usize::from(count);
    debug_assert!(start.saturating_add(count) <= registers.len());
    // SAFETY: the verifier proves both the range bound and definite
    // initialization of every class-constructor field. `MaybeUninit<T>` has
    // the same layout as `T`, and this immutable view cannot outlive or
    // overlap a mutable access to the register bank.
    unsafe { std::slice::from_raw_parts(registers.as_ptr().add(start).cast(), count) }
}

#[inline]
pub(crate) fn clear_static_leaf_range(registers: &mut StaticLeafRegisters, base: u16, count: u16) {
    let start = usize::from(base);
    let count = usize::from(count);
    debug_assert!(count != 0);
    debug_assert!(start.saturating_add(count) <= registers.len());
    for register in &mut registers[start..start + count] {
        register.write(RuntimeValue::Unit);
    }
}

#[inline]
pub(crate) fn copy_static_leaf_range(
    registers: &mut StaticLeafRegisters,
    source: u16,
    destination: u16,
    count: u16,
) {
    let source = usize::from(source);
    let destination = usize::from(destination);
    let count = usize::from(count);
    debug_assert!(count != 0);
    debug_assert!(source.saturating_add(count) <= registers.len());
    debug_assert!(destination.saturating_add(count) <= registers.len());
    // SAFETY: static-leaf certification proves both ranges fit the fixed
    // register bank and definite initialization proves every source slot is
    // initialized. `ptr::copy` intentionally gives snapshot semantics for
    // overlapping physical-value assignments.
    unsafe {
        std::ptr::copy(
            registers.as_ptr().add(source),
            registers.as_mut_ptr().add(destination),
            count,
        );
    }
}

#[inline]
pub(crate) fn write_static_leaf_values(
    registers: &mut StaticLeafRegisters,
    destination: u16,
    count: u16,
    values: impl ExactSizeIterator<Item = RuntimeValue>,
) -> Result<(), InterpreterError> {
    if values.len() != usize::from(count) {
        return Err(InterpreterError::TypeMismatch);
    }
    let start = usize::from(destination);
    let count = usize::from(count);
    if start
        .checked_add(count)
        .is_none_or(|end| end > registers.len())
    {
        return Err(InterpreterError::RegisterOutOfRange(destination));
    }
    for (target, value) in registers[start..start + count].iter_mut().zip(values) {
        target.write(value);
    }
    Ok(())
}

/// Reads one register of the current frame with the bounds re-checks the
/// verifier already discharged removed. Error mapping matches the checked
/// path exactly: any failure is `RegisterOutOfRange(register)`.
#[inline]
pub(crate) fn read_register(
    arena: &FrameArena,
    register: u16,
) -> Result<RuntimeValue, InterpreterError> {
    let (frames, registers) = arena.trusted_parts();
    let Some(frame) = frames.last() else {
        return Err(InterpreterError::RegisterOutOfRange(register));
    };
    debug_assert!(
        usize::from(register) < usize::from(frame.register_count),
        "verifier bounds every register operand below the function's register count"
    );
    debug_assert!(
        frame.register_start as usize + usize::from(frame.register_count) == registers.len(),
        "FrameArena keeps the top frame's window as the register-store tail"
    );
    // SAFETY: `register < frame.register_count` is proven by the verifier
    // for every operand of the executing function, and `push_call_at`
    // guarantees `register_start + register_count == registers.len()` for
    // the top frame, so the index is strictly below `registers.len()`.
    Ok(unsafe { *registers.get_unchecked(frame.register_start as usize + usize::from(register)) })
}

/// Borrows one contiguous register window from the current frame.
///
/// Struct/class construction and fused row pushes consume fields in
/// declared register order. Returning the verified window directly avoids
/// initializing and copying a maximum-width `[RuntimeValue; 16]` scratch
/// row for every operation while retaining one safe slice bounds check.
#[inline]
pub(crate) fn read_register_window(
    arena: &FrameArena,
    base: u16,
    count: u16,
) -> Result<&[RuntimeValue], InterpreterError> {
    let (frames, registers) = arena.trusted_parts();
    let Some(frame) = frames.last() else {
        return Err(InterpreterError::RegisterOutOfRange(base));
    };
    let end = base
        .checked_add(count)
        .ok_or(InterpreterError::RegisterOutOfRange(u16::MAX))?;
    debug_assert!(
        end <= frame.register_count,
        "verifier bounds every contiguous register window inside the function"
    );
    let start = frame.register_start as usize + usize::from(base);
    let end = frame.register_start as usize + usize::from(end);
    registers
        .get(start..end)
        .ok_or(InterpreterError::RegisterOutOfRange(base))
}

/// Writes one register of the current frame; the mirror of
/// [`read_register`] with the same invariants and error mapping.
#[inline]
pub(crate) fn write_register(
    arena: &mut FrameArena,
    register: u16,
    value: RuntimeValue,
) -> Result<(), InterpreterError> {
    let (frames, registers) = arena.trusted_parts_mut();
    let Some(frame) = frames.last() else {
        return Err(InterpreterError::RegisterOutOfRange(register));
    };
    debug_assert!(
        usize::from(register) < usize::from(frame.register_count),
        "verifier bounds every register operand below the function's register count"
    );
    debug_assert!(
        frame.register_start as usize + usize::from(frame.register_count) == registers.len(),
        "FrameArena keeps the top frame's window as the register-store tail"
    );
    let index = frame.register_start as usize + usize::from(register);
    // SAFETY: identical to `read_register` - verifier operand bound plus
    // the top-frame tail invariant put `index` strictly below
    // `registers.len()`.
    unsafe {
        *registers.get_unchecked_mut(index) = value;
    }
    Ok(())
}

/// Advances the current frame's program counter. The overflow check of
/// the safe path is dropped: the verifier bounds function code length far
/// below `u32::MAX` (the whole module must decode inside the wire byte
/// budget), and a hypothetical wrap would still be caught by the next
/// instruction fetch, which bounds `pc` against the function's code
/// before executing anything.
#[inline]
pub(crate) fn advance_pc(arena: &mut FrameArena) -> Result<(), InterpreterError> {
    let (frames, _) = arena.trusted_parts_mut();
    let Some(frame) = frames.last_mut() else {
        return Err(InterpreterError::ContinuationLimit(FrameError::NoFrame));
    };
    frame.pc = frame.pc.wrapping_add(1);
    Ok(())
}

/// One allocator-owned backing for every compact scalar collection arena.
///
/// The eight physical element types occupy disjoint, correctly aligned
/// regions inside one allocation. Region lengths track initialized prefixes,
/// so safe views never expose spare `Vec` capacity as initialized values.
/// This preserves the old typed contiguous slices while replacing eight
/// allocator calls (and eight timed frees) with one deterministic slab.
pub(crate) struct ScalarArenaSet {
    storage: ScalarStorage,
    i32_values: ScalarRegion<i32>,
    i64_values: ScalarRegion<i64>,
    f32_values: ScalarRegion<u32>,
    f64_values: ScalarRegion<u64>,
    bool_values: ScalarRegion<u8>,
    rune_values: ScalarRegion<u32>,
    string_values: ScalarRegion<(GcRef, u64)>,
    ref_values: ScalarRegion<GcRef>,
}

#[derive(Debug)]
#[repr(C, align(16))]
struct AlignedScalarBlock([MaybeUninit<u8>; 16]);

#[derive(Debug)]
struct ScalarStorage {
    blocks: Vec<AlignedScalarBlock>,
}

#[derive(Clone, Copy)]
struct ScalarRegion<T> {
    byte_offset: usize,
    capacity: usize,
    length: usize,
    empty: T,
    marker: PhantomData<T>,
}

#[derive(Clone, Copy)]
struct ScalarCapacities {
    i32: usize,
    i64: usize,
    f32: usize,
    f64: usize,
    bools: usize,
    runes: usize,
    strings: usize,
    refs: usize,
}

impl ScalarCapacities {
    const fn uniform(capacity: usize) -> Self {
        Self {
            i32: capacity,
            i64: capacity,
            f32: capacity,
            f64: capacity,
            bools: capacity,
            runes: capacity,
            strings: capacity,
            refs: capacity,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ScalarArena<'a, T> {
    storage: &'a ScalarStorage,
    region: &'a ScalarRegion<T>,
}

pub(crate) struct ScalarArenaMut<'a, T> {
    storage: &'a mut ScalarStorage,
    region: &'a mut ScalarRegion<T>,
}

impl<T: Copy> ScalarRegion<T> {
    fn reserve(cursor: &mut usize, capacity: usize, empty: T) -> Self {
        let aligned = cursor
            .checked_add(align_of::<T>() - 1)
            .map(|value| value & !(align_of::<T>() - 1))
            .expect("scalar arena alignment must fit usize");
        let bytes = capacity
            .checked_mul(size_of::<T>())
            .expect("scalar arena byte capacity must fit usize");
        *cursor = aligned
            .checked_add(bytes)
            .expect("scalar arena layout must fit usize");
        Self {
            byte_offset: aligned,
            capacity,
            length: 0,
            empty,
            marker: PhantomData,
        }
    }
}

impl ScalarStorage {
    fn new(bytes: usize) -> Self {
        let blocks = bytes.div_ceil(size_of::<AlignedScalarBlock>());
        Self {
            blocks: Vec::with_capacity(blocks),
        }
    }

    fn base(&self) -> *const u8 {
        self.blocks.as_ptr().cast()
    }

    fn base_mut(&mut self) -> *mut u8 {
        self.blocks.as_mut_ptr().cast()
    }

    fn values<T: Copy>(&self, region: &ScalarRegion<T>) -> &[T] {
        debug_assert_eq!(
            (self.base() as usize + region.byte_offset) % align_of::<T>(),
            0
        );
        // SAFETY: `ScalarArenaSet::with_capacities` assigns this region a
        // disjoint, `T`-aligned byte interval inside the allocation.
        // `ScalarArenaMut::claim_exact` initializes every cell below
        // `region.length` before increasing that length. The returned view is
        // tied to the immutable borrow of the complete storage.
        unsafe {
            std::slice::from_raw_parts(
                self.base().add(region.byte_offset).cast::<T>(),
                region.length,
            )
        }
    }

    fn values_mut<T: Copy>(&mut self, region: &ScalarRegion<T>) -> &mut [T] {
        debug_assert_eq!(
            (self.base() as usize + region.byte_offset) % align_of::<T>(),
            0
        );
        // SAFETY: identical layout and initialization proof to `values`.
        // The mutable borrow covers the entire slab, so no other typed region
        // can be borrowed concurrently through the safe arena API.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.base_mut().add(region.byte_offset).cast::<T>(),
                region.length,
            )
        }
    }

    fn initialize<T: Copy>(&mut self, region: &ScalarRegion<T>, start: usize, end: usize) {
        debug_assert!(start <= end);
        debug_assert!(end <= region.capacity);
        // SAFETY: the constructor reserves `capacity * size_of::<T>()`
        // aligned bytes for this region. `[start, end)` is within that
        // interval and not exposed as initialized until this fill completes.
        let values = unsafe {
            std::slice::from_raw_parts_mut(
                self.base_mut()
                    .add(region.byte_offset)
                    .cast::<T>()
                    .add(start),
                end - start,
            )
        };
        values.fill(region.empty);
    }
}

impl<'a, T: Copy> ScalarArena<'a, T> {
    pub(crate) fn values(self, range: CollectionRange) -> Result<&'a [T], HeapError> {
        let end = range
            .start
            .checked_add(range.length)
            .ok_or(HeapError::CapacityExhausted)?;
        self.storage
            .values(self.region)
            .get(range.start..end)
            .ok_or(HeapError::IndexOutOfBounds {
                index: end,
                length: self.region.capacity,
            })
    }

    pub(crate) const fn capacity(self) -> usize {
        self.region.capacity
    }

    pub(crate) const fn length(self) -> usize {
        self.region.length
    }
}

impl<T: Copy> ScalarArenaMut<'_, T> {
    pub(crate) fn claim_exact(&mut self, range: CollectionRange) -> Result<(), HeapError> {
        let end = range
            .start
            .checked_add(range.length)
            .ok_or(HeapError::CapacityExhausted)?;
        if end > self.region.capacity {
            return Err(HeapError::CapacityExhausted);
        }
        if self.region.length < end {
            self.storage
                .initialize(self.region, self.region.length, end);
            self.region.length = end;
        }
        Ok(())
    }

    pub(crate) fn release(&mut self, range: CollectionRange) {
        if range.length == 0 {
            return;
        }
        let empty = self.region.empty;
        if let Ok(values) = self.values_mut(range) {
            values.fill(empty);
        }
    }

    pub(crate) fn values_mut(&mut self, range: CollectionRange) -> Result<&mut [T], HeapError> {
        let end = range
            .start
            .checked_add(range.length)
            .ok_or(HeapError::CapacityExhausted)?;
        let capacity = self.region.capacity;
        self.storage
            .values_mut(self.region)
            .get_mut(range.start..end)
            .ok_or(HeapError::IndexOutOfBounds {
                index: end,
                length: capacity,
            })
    }

    pub(crate) const fn length(&self) -> usize {
        self.region.length
    }

    fn restore(&mut self, source: &[T]) {
        debug_assert!(source.len() <= self.region.capacity);
        let previous_length = self.region.length;
        let restored_length = source.len();
        if previous_length < restored_length {
            self.storage
                .initialize(self.region, previous_length, restored_length);
        }
        self.region.length = previous_length.max(restored_length);
        let empty = self.region.empty;
        let all = self
            .values_mut(CollectionRange {
                start: 0,
                length: self.region.length,
            })
            .expect("restored scalar prefix fits reserved storage");
        all[..restored_length].copy_from_slice(source);
        all[restored_length..].fill(empty);
        self.region.length = restored_length;
    }
}

impl ScalarArenaSet {
    pub(crate) fn new(capacity: usize) -> Self {
        Self::with_capacities(ScalarCapacities::uniform(capacity))
    }

    fn with_capacities(capacities: ScalarCapacities) -> Self {
        let mut cursor = 0_usize;
        let i32_values = ScalarRegion::reserve(&mut cursor, capacities.i32, 0_i32);
        let i64_values = ScalarRegion::reserve(&mut cursor, capacities.i64, 0_i64);
        let f32_values = ScalarRegion::reserve(&mut cursor, capacities.f32, 0_u32);
        let f64_values = ScalarRegion::reserve(&mut cursor, capacities.f64, 0_u64);
        let bool_values = ScalarRegion::reserve(&mut cursor, capacities.bools, 0_u8);
        let rune_values = ScalarRegion::reserve(&mut cursor, capacities.runes, 0_u32);
        let string_values = ScalarRegion::reserve(
            &mut cursor,
            capacities.strings,
            (
                GcRef {
                    index: u32::MAX,
                    generation: u32::MAX,
                },
                0,
            ),
        );
        let ref_values = ScalarRegion::reserve(
            &mut cursor,
            capacities.refs,
            GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            },
        );
        Self {
            storage: ScalarStorage::new(cursor),
            i32_values,
            i64_values,
            f32_values,
            f64_values,
            bool_values,
            rune_values,
            string_values,
            ref_values,
        }
    }

    pub(crate) fn checkpoint_clone(&self) -> Self {
        let mut snapshot = Self::with_capacities(ScalarCapacities {
            i32: self.i32_values.length,
            i64: self.i64_values.length,
            f32: self.f32_values.length,
            f64: self.f64_values.length,
            bools: self.bool_values.length,
            runes: self.rune_values.length,
            strings: self.string_values.length,
            refs: self.ref_values.length,
        });
        macro_rules! copy_prefix {
            ($read:ident, $write:ident) => {{
                let source = self.$read();
                let range = CollectionRange {
                    start: 0,
                    length: source.length(),
                };
                let values = source
                    .values(range)
                    .expect("snapshot range is the initialized prefix");
                let mut destination = snapshot.$write();
                destination
                    .claim_exact(range)
                    .expect("snapshot reserves the exact initialized prefix");
                destination
                    .values_mut(range)
                    .expect("snapshot range was initialized")
                    .copy_from_slice(values);
            }};
        }
        copy_prefix!(i32, i32_mut);
        copy_prefix!(i64, i64_mut);
        copy_prefix!(f32, f32_mut);
        copy_prefix!(f64, f64_mut);
        copy_prefix!(bools, bools_mut);
        copy_prefix!(runes, runes_mut);
        copy_prefix!(strings, strings_mut);
        copy_prefix!(refs, refs_mut);
        snapshot
    }

    pub(crate) fn restore_checkpoint(&mut self, snapshot: &Self) {
        macro_rules! restore_prefix {
            ($read:ident, $write:ident) => {{
                let source = snapshot.$read();
                let range = CollectionRange {
                    start: 0,
                    length: source.length(),
                };
                self.$write().restore(
                    source
                        .values(range)
                        .expect("snapshot exposes its initialized prefix"),
                );
            }};
        }
        restore_prefix!(i32, i32_mut);
        restore_prefix!(i64, i64_mut);
        restore_prefix!(f32, f32_mut);
        restore_prefix!(f64, f64_mut);
        restore_prefix!(bools, bools_mut);
        restore_prefix!(runes, runes_mut);
        restore_prefix!(strings, strings_mut);
        restore_prefix!(refs, refs_mut);
    }

    pub(crate) fn reserved_bytes(&self) -> u64 {
        [
            self.i32().capacity().saturating_mul(size_of::<i32>()),
            self.i64().capacity().saturating_mul(size_of::<i64>()),
            self.f32().capacity().saturating_mul(size_of::<u32>()),
            self.f64().capacity().saturating_mul(size_of::<u64>()),
            self.bools().capacity(),
            self.runes().capacity().saturating_mul(size_of::<u32>()),
            self.strings()
                .capacity()
                .saturating_mul(size_of::<(GcRef, u64)>()),
            self.refs().capacity().saturating_mul(size_of::<GcRef>()),
        ]
        .into_iter()
        .fold(0_u64, |total, bytes| total.saturating_add(bytes as u64))
    }

    pub(crate) fn backing_allocations(&self) -> usize {
        usize::from(self.storage.blocks.capacity() != 0)
    }
}

macro_rules! scalar_arena_accessors {
    ($(($read:ident, $write:ident, $field:ident, $element:ty)),+ $(,)?) => {
        impl ScalarArenaSet {
            $(
                pub(crate) fn $read(&self) -> ScalarArena<'_, $element> {
                    ScalarArena {
                        storage: &self.storage,
                        region: &self.$field,
                    }
                }

                pub(crate) fn $write(&mut self) -> ScalarArenaMut<'_, $element> {
                    ScalarArenaMut {
                        storage: &mut self.storage,
                        region: &mut self.$field,
                    }
                }
            )+
        }
    };
}

scalar_arena_accessors!(
    (i32, i32_mut, i32_values, i32),
    (i64, i64_mut, i64_values, i64),
    (f32, f32_mut, f32_values, u32),
    (f64, f64_mut, f64_values, u64),
    (bools, bools_mut, bool_values, u8),
    (runes, runes_mut, rune_values, u32),
    (strings, strings_mut, string_values, (GcRef, u64)),
    (refs, refs_mut, ref_values, GcRef),
);

impl std::fmt::Debug for ScalarArenaSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScalarArenaSet")
            .field("storage", &self.storage)
            .field("backing_allocations", &self.backing_allocations())
            .field("reserved_bytes", &self.reserved_bytes())
            .field("i32_length", &self.i32_values.length)
            .field("i64_length", &self.i64_values.length)
            .field("f32_length", &self.f32_values.length)
            .field("f64_length", &self.f64_values.length)
            .field("bool_length", &self.bool_values.length)
            .field("rune_length", &self.rune_values.length)
            .field("string_length", &self.string_values.length)
            .field("ref_length", &self.ref_values.length)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::frame::{FrameArena, FrameLimits};
    use crate::{CollectionRange, GcRef, InterpreterError, RuntimeValue};

    #[test]
    fn scalar_arenas_share_one_backing_and_restore_typed_prefixes() {
        let mut arenas = super::ScalarArenaSet::new(8);
        assert_eq!(arenas.backing_allocations(), 1);
        assert_eq!(arenas.i32().capacity(), 8);
        assert_eq!(
            arenas.reserved_bytes(),
            8 * (4 + 8 + 4 + 8 + 1 + 4 + 16 + 8)
        );

        let range = CollectionRange {
            start: 2,
            length: 2,
        };
        {
            let mut values = arenas.i32_mut();
            values.claim_exact(range).unwrap();
            values.values_mut(range).unwrap().copy_from_slice(&[7, 9]);
        }
        let reference = GcRef {
            index: 3,
            generation: 5,
        };
        {
            let mut values = arenas.strings_mut();
            values.claim_exact(range).unwrap();
            values.values_mut(range).unwrap()[0] = (reference, 11);
        }
        assert_eq!(arenas.i32().values(range).unwrap(), &[7, 9]);
        assert_eq!(arenas.strings().values(range).unwrap()[0], (reference, 11));

        let snapshot = arenas.checkpoint_clone();
        assert_eq!(snapshot.i32().capacity(), range.start + range.length);
        assert_eq!(snapshot.strings().capacity(), range.start + range.length);
        arenas.i32_mut().values_mut(range).unwrap().fill(99);
        arenas.restore_checkpoint(&snapshot);
        assert_eq!(arenas.i32().values(range).unwrap(), &[7, 9]);
        assert_eq!(arenas.strings().values(range).unwrap()[0], (reference, 11));
        assert_eq!(arenas.i32().capacity(), 8);
    }

    #[test]
    fn kernel_register_access_matches_the_checked_arena() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(3, 4).unwrap();
        super::write_register(&mut arena, 2, RuntimeValue::I32(41)).unwrap();
        assert_eq!(
            super::read_register(&arena, 2).unwrap(),
            RuntimeValue::I32(41)
        );
        super::write_register(&mut arena, 3, RuntimeValue::I32(42)).unwrap();
        assert_eq!(
            super::read_register_window(&arena, 2, 2).unwrap(),
            &[RuntimeValue::I32(41), RuntimeValue::I32(42)]
        );
        assert_eq!(arena.register(2).unwrap(), RuntimeValue::I32(41));

        // A nested frame re-targets the window to the new tail.
        arena.push(4, 2).unwrap();
        super::write_register(&mut arena, 0, RuntimeValue::Bool(true)).unwrap();
        assert_eq!(
            super::read_register(&arena, 0).unwrap(),
            RuntimeValue::Bool(true)
        );
        assert_eq!(arena.register(0).unwrap(), RuntimeValue::Bool(true));
        // The parent frame's registers are untouched.
        arena.pop().unwrap();
        assert_eq!(
            super::read_register(&arena, 2).unwrap(),
            RuntimeValue::I32(41)
        );
    }

    #[test]
    fn kernel_access_without_a_frame_fails_like_the_checked_path() {
        let mut arena = FrameArena::new(FrameLimits::default());
        assert!(matches!(
            super::read_register(&arena, 0),
            Err(InterpreterError::RegisterOutOfRange(0))
        ));
        assert!(matches!(
            super::write_register(&mut arena, 1, RuntimeValue::Unit),
            Err(InterpreterError::RegisterOutOfRange(1))
        ));
        assert!(super::advance_pc(&mut arena).is_err());
    }

    #[test]
    fn kernel_pc_advance_matches_the_checked_path() {
        let mut arena = FrameArena::new(FrameLimits::default());
        arena.push(0, 1).unwrap();
        super::advance_pc(&mut arena).unwrap();
        super::advance_pc(&mut arena).unwrap();
        assert_eq!(arena.current().unwrap().pc, 2);
    }
}
