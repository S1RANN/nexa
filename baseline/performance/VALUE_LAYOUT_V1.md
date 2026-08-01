# Value Layout v1

Version: **1.0.0**

`ValueLayout` is the authoritative physical representation table:

```text
ValueLayout {
    logical_type,
    physical_slots,
    alignment,
    gc_bitmap,
    field_offsets,
    enum_layout,
    copy_strategy,
    equality_strategy,
    hash_strategy,
}
```

Every module carries a deterministic layout table for scalar, Struct, Enum,
tuple, Option/Result, Class reference, collection reference, and handle
layouts, ordered by stable type ID.

## Physical slots

A logical value lowers to one or more contiguous physical slots. A physical
slot only carries `i32`, `i64`, `f32`, `f64`, `bool`, `rune`, a GC
reference, a Host handle/token/snapshot, or an opaque scalar. Struct and
Enum locals are not single heap-reference slots.

## Calling convention

Function signatures keep logical types. Module load derives per-parameter
logical type, starting physical slot, slot count, and root bitmap. Calls do
not construct temporary heap structs for parameters. Multi-slot returns use
a caller-allocated return range. Frames reserve the physical slot arena,
call metadata, return range, root bitmap, and defer captures; call, return,
and tail positions perform no system allocation.

## Inlined aggregates

Struct construction, copy, field access, update construction
(`Cell { x: 10, ..old }`), parameters, returns, equality, and hash all
operate on physical layout without creating heap objects. Enum uses a tag
slot plus the maximum payload slot range with a payload root bitmap;
inactive payloads never participate in equality, hash, or root scanning.
Aggregate nesting (Struct in Struct, Enum in Struct, Struct in Enum,
Option/Result of aggregates, tuples mixing Class references) preserves
precise roots.

## Materialization boundary

Copying a logical value into persistent storage happens only at Class
fields, state classes, Array/Buffer element storage, Map key/value storage,
Host ABI, snapshots, migration staging, and Task frames. Materialization
never creates an identity-bearing Struct heap object.

## Root maps

Root maps are expressed in physical-slot units and mark only slots that can
contain GC references.

## Verification

The verifier validates physical slot ranges, non-overlapping return ranges,
layout IDs, field offsets, enum payload widths, root bitmap widths, call
argument slot counts, result slot counts, and safepoint roots. Bytecode v7
is the only accepted wire version once this layout lands; no compatibility
decoder remains on product paths.

## Differential gate

Randomly generated legal programs execute on the pre-layout semantic oracle
and the ValueLayout interpreter; results, traps, instruction semantics,
Class identity, and GC liveness must match exactly. Fuel comparison follows
the version-boundary ruling in `BENCHMARK_PROTOCOL_V1.md`.
