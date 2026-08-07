# Nexa Language v3

Version: **3**

Status: **SPEC+WIRE FOUNDATION** (compiler/runtime implementation pending)

The structured language version carried in build inputs, fingerprints, and
artifacts is:

```text
NEXA_LANGUAGE_VERSION: u16 = 3
```

It is not a display string or a SemVer compatibility range.

This document is the normative source-language specification for the Nexa v3
surface: the `Set<T>` collection, dynamic iteration over
Range/Array/Buffer/Map/Set, unspecified HashMap-style slot order, the
mutation-epoch trap, and the minimal collection API set. It layers on the
frozen Language v2 and Object Model v2 rules; every rule those documents state
remains in force unless this document replaces it. The terms **MUST**,
**MUST NOT**, **SHOULD**, and **MAY** are normative.

## 1. Frozen boundaries

```text
NEXA_LANGUAGE_VERSION = 3
CONTRACT_SYNTAX_VERSION = 3      (unchanged)
HOST_CONTRACT_SCHEMA_VERSION = 2 (unchanged)
ABI_DESCRIPTOR_VERSION = 2       (unchanged)
BYTECODE_VERSION = 8
OPCODE_COST_TABLE_VERSION = 8
```

Contract Syntax v3, Host schema 2, Descriptor 2, and the Contract/Host ABI are
unchanged by Language v3. Equality between these values is neither required nor
implied. Active products reject every other source, Contract, Descriptor, or
Bytecode version instead of selecting a compatibility parser or decoder.

### 1.1 Aggregate initializer shorthand

Struct and Class values use the same `Type { fields }` initializer. A field
whose value is a same-named binding MAY omit the colon and value:

```nexa
let x = 1;
let point = Point { x, y: 2 };
let object = Boxed { value };
```

`Type { field }` is exactly equivalent to `Type { field: field }`. Shorthand
fields MAY precede `..base`; duplicate, unknown, missing, and type-mismatched
fields are checked exactly as explicit field initializers.

## 2. `Set<T>`

A `Set<T>` is a non-null GC reference type holding distinct elements of one
element type `T`, mirroring the reference semantics of `Map<K, V>`.

- `Set<T>` elements are unique by the same equality rules that key a
  `Map<K, V>`; an element equal to an existing element replaces no value and
  leaves the set unchanged.
- Insertion, removal, and containment require `T` to define the same equality
  and hashing contract as a Map key.
- `Set<T>` is never const-safe: construction requires GC allocation.
- A `Set<T>` reference in a Struct/Enum/Class field is an exact GC root and
  passes through the same write barriers as other collection references.
- The `Set` element parameter participates in the type's stable ABI identity
  through the canonical parameterized type ID (`Set<T>`), exactly as `Array<T>`
  and `Map<K, V>` do.

The minimal `Set<T>` surface is exactly:

```text
Set::new() -> Set<T>             // empty set
len() -> i32                     // element count
contains(value) -> bool          // membership
insert(value) -> bool            // true when the element was not present
remove(value) -> bool            // true when the element was present
clear()
```

No other Set method, iterator type, or collection combinator is added by
Language v3. `Set<T>` participates in dynamic iteration exactly like the other
collections (section 4). The extended minimal collection surface added by the
v8 wire is exactly: Array `first`/`last`/`swap`/`reverse`, Map
`is_empty`/`get_or`/`insert_if_absent`, and Buffer `is_empty`/`fill`; each
returns the value documented by its intrinsic contract (`first`/`last` return
`Option<T>`, `get_or` returns the value or the provided default, and the
mutating forms return whether the operation applied).

## 3. Minimal collection API

Language v3 adds no collection API beyond the `Set<T>` surface in section 2
and the exact extended surface listed in this section. The complete list of
collection APIs added by Language v3 is:

```text
Set<T>          new, len, contains, insert, remove, clear      (section 2)
Array<T>        first, last, swap, reverse
Map<K, V>       is_empty, get_or, insert_if_absent
Buffer<T>       is_empty, fill
```

Beyond that exact list, Language v3 adds no collection API. In particular it
adds:

```text
no new integer widths
no StringBuilder or string-builder API
no fixed-size arrays
no sorted/ordered Map or Set variants
no Map/Set iteration-order guarantees
```

Every Array, Buffer, and Map method not listed above keeps exactly its
Language v2 API. `StringBuild` remains the only bounded scalar-to-string
lowering surface.

## 4. Dynamic iteration

`for` iteration is dynamic over exactly these shapes:

```text
Range(start..end)   scalar i32 values
Array<T>            element values
Buffer<T>           element values
Map<K, V>           (key, value) pairs
Set<T>              element values
```

### 4.1 Order

Map and Set iterate in unspecified HashMap-style slot order. An implementation
MUST NOT promise insertion order, sorted order, or stable order across
mutation, reload, or distinct runs. Array and Buffer iterate in index order;
Range iterates from `start` upward in ascending scalar order.

Programs MUST NOT depend on Map or Set iteration order; the verifier or
compiler MAY reject a construct whose observable result depends on it only
through documented deterministic-analysis rules, never at runtime.

### 4.2 Iterator state

Iteration state is heap-allocation-free and lives entirely in hidden scalar
registers:

```text
collection register  reference to the collection (or range start scalar)
phase register       HashMap-style bucket phase cursor
slot register        slot cursor inside the current bucket phase
epoch register       mutation epoch observed at IterNew
```

No iterator object is allocated. `IterNew` initializes the cursor and snapshots
the epoch; each `IterNext` sets `has_value` to true (element yielded) or false
(iteration exhausted), writes the element into `first_dst`, and for Map
iteration writes the value into `second_dst`.

### 4.3 Mutation-epoch trap

Every collection keeps a mutation epoch that counts applied content changes.
`IterNew` snapshots it; `IterNext` revalidates the live epoch against the
snapshot. A mutating operation between `IterNew` and the final `IterNext` on
the same collection MUST trap with a deterministic
collection-mutated-during-iteration error instead of iterating stale storage.
Resuming iteration after the trap is impossible: the iterator state is
consumed by the trap.

The operations that increment the epoch exactly when they apply a content
change are:

```text
Set<T>      insert (element absent), remove (element present), clear (non-empty)
Map<K, V>   set / insert (stored value changes), insert_if_absent (key absent),
            remove (key present), clear (non-empty)
Array<T>    set (stored element changes), push, pop, insert, remove,
            clear (non-empty), swap (distinct indexes), reverse (length > 1)
Buffer<T>   set (stored element changes), copy (copied span changes),
            fill (filled span changes)
```

Operations that leave the collection content unchanged MUST NOT increment the
epoch: `Set::insert` of an element already present, `Set::remove` of an absent
element, `Set::clear`/`Map::clear`/`Array::clear` on an empty collection,
`Array::swap(a, a)` with equal indexes, `Array::reverse` on one element,
`Map::insert_if_absent` on a present key, and any store of a value equal to
the value already stored. "Changes" is judged by the same builtin value
equality the collection uses for its elements and keys; a call whose content
does not change is a no-op for the epoch even when its intrinsic returns
`true`. Where an intrinsic reports applied-ness (`set_insert`, `set_remove`,
`map_insert_if_absent`) its result and the epoch decision agree; where it does
not (`map_set`, `map_insert`, `array_swap`, `array_reverse`, `buffer_fill`
always return `true` on success), the epoch follows the content-change rules
above, not the return value.

Array and Buffer iteration may use any mechanism whose bumps exactly match
the rules above (for example a monotonically counted epoch or a content
fingerprint); the observable contract is the same trap.

## 5. Wire surface (bytecode v8)

Bytecode v8 carries the Language v3 surface:

- Types section kind `8` = `Set` nominal type metadata (`type_id`, `element`).
- Instructions: `SetNew`, `SetLen`, `SetContains`, `SetInsert` (returns
  `bool`: whether the element was newly inserted), `SetRemove`, `SetClear`,
  `IterNew`, `IterNext`.
- Standard intrinsics: `SetLen`, `SetContains`, `SetInsert`, `SetRemove`
  (wire tags 43-46) plus the extended minimal collection surface
  `ArrayFirst`/`ArrayLast`/`ArraySwap`/`ArrayReverse`,
  `MapIsEmpty`/`MapGetOr`/`MapInsertIfAbsent`, and
  `BufferIsEmpty`/`BufferFill` (wire tags 47-55; 56 reserved variants total).
- `CollectionIteratorKind` instantiates Range/Array/Buffer/Map/Set iteration
  with concrete element types; `IteratorStateRegisters` encodes the four
  hidden scalar registers of section 4.2. `IterNext` carries explicit
  `has_value_dst`, `first_dst`, and optional `second_dst` registers (set for
  Map, `None` for other shapes) instead of a fabricated Tuple result.

The v8 decoder rejects every other wire version. There is no v7 compatibility
decoder. `BYTECODE_VERSION` and `OPCODE_COST_TABLE_VERSION` are frozen at 8;
the new Set/iteration opcodes and intrinsics receive deterministic base costs
in the v8 cost table.

## 6. Exclusions carried from v2

Language v2's explicit exclusions remain: no user-defined generics, traits,
closures, dynamic dispatch, operator overloading, macros, reflection,
`Box<T>`, pointers, borrow checker, threads, or sandbox. `Set<T>` does not
create a generic-programming feature; it is a built-in collection exactly like
`Array<T>` and `Map<K, V>`.

## 7. Acceptance matrix

The foundation commit is accepted when every `PASS` row below has evidence on
one candidate commit. Status values are `PENDING`, `PASS`, or `FAIL`.

| ID | Surface | Acceptance | Evidence / gate | Status |
| --- | --- | --- | --- | --- |
| L01 | Version constants | `NEXA_LANGUAGE_VERSION = 3`, `BYTECODE_VERSION = 8`, `OPCODE_COST_TABLE_VERSION = 8` in every authoritative code constant and baseline doc | baseline docs; `nexa-core` constants; `nexa-analysis` constant | PASS |
| L02 | Canonical Set identity | `Set<T>` derives its ABI type ID from `canonical_set_type_id("Set", [T])` and equals `SetType::new(T).type_id` | nexa-core unit test; nexa-bytecode wire test | PASS |
| L03 | Set module metadata | Types section kind 8 round-trips `SetType` (type_id + element) through encode/decode | wire roundtrip test | PASS |
| L04 | Set instructions | `SetNew`/`SetLen`/`SetContains`/`SetInsert`/`SetRemove`/`SetClear` encode/decode as v8 opcodes 111-116 | wire roundtrip test | PASS |
| L05 | Set + collection intrinsics | `SetLen`/`SetContains`/`SetInsert`/`SetRemove` (tags 43-46) and `ArrayFirst`/`ArrayLast`/`ArraySwap`/`ArrayReverse`, `MapIsEmpty`/`MapGetOr`/`MapInsertIfAbsent`, `BufferIsEmpty`/`BufferFill` (tags 47-55) with correct canonical names, arity, argument/result types, mutation flags, and fuel models | intrinsic metadata + roundtrip tests | PASS |
| L06 | Iteration wire | `IterNew`/`IterNext` (117/118) carry `CollectionIteratorKind` and heap-free `IteratorStateRegisters` (collection/phase/slot/epoch) | wire roundtrip test | PASS |
| L07 | Version rejection | Decoder rejects every wire version other than 8 with `UnsupportedVersion`, no compatibility decoder | version rejection test | PASS |
| L08 | Frozen ABI | Contract Syntax v3, Host schema 2, Descriptor 2, and Contract/Host ABI unchanged | no baseline ABI doc changes; untouched code paths | PASS |
| L09 | No surface creep | No new integers, StringBuilder, fixed arrays, sorted Map/Set, or extra collection APIs | baseline doc freeze; repo scan | PASS |
| L10 | Compiler/runtime | Compiler lowers `Set<T>` and dynamic iteration to the v8 wire; verifier validates the new metadata/instructions; runtime implements epoch-trapped iteration | downstream tasks | PENDING |
| L11 | Docs agreement | BYTECODE.md and Baseline Index agree on v8 and Language v3 boundaries | link check; terminology scan | PASS |

## 8. Conformance boundary

An implementation conforms to Language v3 only when the lexer, parser,
analysis, compiler, verifier, runtime, and editor surfaces agree on the v3
facts: `Set<T>` exists with the minimal API, dynamic iteration covers exactly
Range/Array/Buffer/Map/Set, Map/Set order is unspecified, and mutation during
iteration traps. Conformance does not require source compatibility with any
earlier language revision.
