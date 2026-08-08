# Nexa Internal Language Bytecode 9

Version: **9.0.0**

Status: **COMPLETE**

Bytecode v9 is Nexa's only portable execution artifact. It is a
little-endian, sectioned, typed-register format whose logical values are
lowered through the physical ABI defined by
[`VALUE_LAYOUT_V1.md`](../performance/VALUE_LAYOUT_V1.md). Runtime execution
accepts only a verifier-produced `VerifiedModule`; portable bytes are never
executed directly.

## Frozen versions

```text
BYTECODE_VERSION = 9
OPCODE_COST_TABLE_VERSION = 8
MANDATORY_SECTION_COUNT = 17
```

The decoder rejects every wire version other than 9 from the envelope before
section payload decoding. There is no v5/v6/v7 compatibility decoder, feature
negotiation, or product fallback path. The opcode cost-table version is
validated when an `ExecutableModule` is built and is part of deterministic
fuel semantics.

## Wire envelope

Every integer is encoded little-endian. The envelope is:

```text
offset  width  field
0       4      magic = "NXBC"
4       2      bytecode version = 9
6       2      section-directory entry count
8       20*N   section-directory entries
...            contiguous section payloads
```

Each 20-byte directory entry contains:

```text
kind:u16
flags:u16
offset:u32
length:u32
count:u32
checksum:u32
```

The mandatory flag is bit 0 and no other flag bit is valid. `count` must equal
the first little-endian `u32` in the section payload. `checksum` is FNV-1a
32-bit over the complete payload. Directory entries must have unique kinds;
payloads must be contiguous, non-overlapping, in bounds, checksum-valid, and
consume the artifact exactly. Unknown mandatory sections fail closed.

## Mandatory sections

The v9 encoder emits, and the decoder requires, these 17 sections:

| Kind | Name | Authority |
| ---: | --- | --- |
| 1 | Strings | UTF-8 module string pool |
| 2 | Types | StateHandle, Array, Map, Buffer, Set, Snapshot, resource token, and Host opaque nominal types |
| 3 | Constants | Reserved in v7; count must be zero |
| 4 | Enums | Stable type/variant IDs, tags, and optional logical payload types |
| 5 | Structs | Stable type/field IDs and logical field types |
| 6 | Classes | Stable type/field IDs and logical field types |
| 7 | HostImports | Stable IDs, declaration fingerprints, capabilities, mode, fuel, signatures, and async policy |
| 8 | StateSchemas | Stable IDs, versions, fields, and logical field types |
| 9 | Exports | Stable IDs, function indices, effects, and logical signatures |
| 10 | Functions | Logical signatures plus physical frame metadata and canonical function metadata |
| 11 | Code | Per-function instruction streams |
| 12 | RootMaps | Initial and per-PC physical-slot root bitmaps |
| 13 | Safepoints | Per-function safepoint PCs |
| 14 | LoopBounds | Immediate-function back edges and static iteration bounds |
| 15 | SourceMap | Function/PC ranges mapped to exact source spans |
| 16 | ReloadMetadata | Contract/schema fingerprints, migration/activation entries, and minimum migration limits |
| 17 | DisplayTypes | Source-facing nominal type and field-name string indices for deterministic structural interpolation |

`Code`, `RootMaps`, `Safepoints`, and `LoopBounds` duplicate the canonical
copies carried with function metadata. The decoder requires byte-for-byte
semantic agreement between both representations; disagreement is an invalid
artifact, not a preference rule.

## Logical type closure and ValueLayout

Function, export, Host, field, collection, and enum metadata retain logical
types. At verification/load time, the complete nominal type closure
deterministically derives a module-local `LayoutTable`, ordered by stable type
ID. No compiler-authored offset table is trusted and no runtime pointer is
serialized.

The only physical slot categories are:

```text
i32 | i64 | f32 | f64 | bool | rune
GC reference | Host handle | Host opaque scalar
```

Scalar values, Class/collection references, handles, and Host opaque values
occupy one slot. Struct values flatten recursively into contiguous field
slots. Enum values occupy one tag slot followed by the widest variant payload
range; inactive payload slots are non-semantic and never become roots.
Recursive value types, unknown nominal types, slot-count overflow, or
inconsistent field/variant metadata are verifier errors.

Host opaque nominal types are explicit entries in `Types`; they never degrade
to Unit or a GC reference. State handles, snapshots, resource tokens, and
Host-request handles are rooted by their registries rather than by frame root
maps.

## Physical function ABI

Signatures retain logical parameter and result types. The verifier derives
for every function:

```text
parameter logical type -> starting physical slot + exact slot width
result logical type    -> caller-owned starting slot + exact slot width
frame                  -> exact physical register count + byte quota
```

`parameter_slots` must equal the packed physical width of all parameters.
`Call`, `HostCall`, and `DeferPush` carry a base plus an exact physical-slot
count. `dst` and `Return.source` are the starts of caller-owned result ranges;
Unit has no result range. Multi-slot values are copied with `CopyValue`, whose
encoded width must equal the authoritative layout. Call, return, aggregate
construction, field access, and value copy do not require temporary Struct or
Enum heap objects.

Persistent storage boundaries—Class/state fields, Array/Buffer rows, Map
keys/values, snapshots, migration staging, Task continuations, and the Host
ABI—materialize or reconstruct logical aggregate values as required while
preserving Struct/Enum value semantics and Class identity.

## Aggregate and collection instructions

v8 instructions operate on physical ranges:

- `StructNew`, `StructGet`, `StructWith`, `StructEqual`, `EnumNew`,
  `EnumTag`, `EnumPayload`, and `EnumEqual` use verifier-derived layout widths
  and offsets.
- `ArrayPushRow` and `ArrayFieldGet` address typed row storage without
  per-element Struct objects.
- `CopyValue` copies one complete logical value across contiguous slots.
- `StringBuild` converts and concatenates a bounded physical argument window
  into one published String.
- `SetNew`, `SetLen`, `SetContains`, `SetInsert`, `SetRemove`, and `SetClear`
  carry the Language v3 minimal `Set<T>` surface; the Types section kind 8
  entry supplies the element type.
- `IterNew` and `IterNext` carry dynamic iteration over
  Range/Array/Buffer/Map/Set through `CollectionIteratorKind`. Iterator state
  is heap-allocation-free: four hidden scalar registers hold the collection
  reference, the phase/slot cursor, and the mutation epoch snapshot. Every
  `IterNext` revalidates the epoch; mutation during iteration traps.
  `IterNext` uses explicit `has_value_dst`, `first_dst`, and optional
  `second_dst` registers (Map uses `second_dst`; the verifier enforces
  correctness).
- `StandardIntrinsic` encodes the fully instantiated logical types of generic
  standard-library operations; runtime name or generic resolution is
  forbidden.

All remaining scalar, control-flow, Class, collection, state, migration,
defer, Task, and Host instructions retain their v8 typed-register contracts.
An unknown opcode or intrinsic is rejected during decoding.

## Precise roots and safepoints

Function root bitmaps and every per-PC root map are expressed in physical-slot
units and must have the exact frame width. Only slots whose derived
`PhysicalSlotKind` is `GcReference` may be set. In particular, state handles,
snapshots, resource tokens, Host-request handles, and Host opaque scalars are
not forged GC roots.

Every allocating, yielding, Host, Task, collection, migration, and explicit
safepoint operation is verified against a valid safepoint and exact root map.
Struct nesting and Enum active variants preserve precise roots; inactive enum
payload slots are cleared or ignored and never scanned.

## Effects, control flow, and limits

Each function is exactly one of `Ordinary`, `Task`, `Immediate`, `Migration`,
or `Cleanup`. The verifier independently validates instruction boundaries,
CFG edges, definite initialization, logical and physical register types, call
signatures, result ranges, field/variant identities, effect restrictions,
defer captures, Host signatures, source ranges, and frame/resource ceilings.

Immediate loops require a matching static `LoopBounds` entry. Recursive
immediate call graphs and any immediate WCET that exceeds its declared limits
are rejected. Migration code is restricted to the state protocol and must
finish with an explicit Preserve/Replace/Delete decision plus `StateFinish`;
its minimum object, field, forwarding, byte, root, fuel, and call-depth limits
are committed in `ReloadMetadata`.

Decoding is fail-closed under explicit limits for total bytes, sections,
strings and string bytes, types, functions, instructions, registers, root-map
bytes, safepoints, loop bounds, Host imports, state/enum/struct/class metadata,
fields, exports, source-map entries, and reload metadata.

## Fuel and failure atomicity

`OPCODE_COST_TABLE_VERSION = 8` binds the static base cost of all 119 v8
opcodes and every standard intrinsic (56 wire variants). Value-dependent
string, collection, map, set, buffer, hashing, conversion, and copy work reads
bounded metadata and precharges its deterministic worst-case or exact dynamic
surcharge before mutation or allocation. Fuel failure therefore cannot publish
a partial String, collection update, Host result, or migration write.

Portable and `ExecutableModule` interpreters must produce identical results,
traps, charge settlement, suspend points, Task/Host lifecycles, identities,
and source stacks for the same verified v8 artifact and cost table.

## Version transition

Bytecode 7 is retired. v8 changed the wire version exactly once to introduce
the Language v3 `Set<T>` type metadata, Set operations, and dynamic
collection iteration with epoch-trapped, heap-allocation-free iterator state.
Caches and persisted artifacts bind their build identity to the bytecode
version and are rebuilt rather than decoded through a historical compatibility
path.
