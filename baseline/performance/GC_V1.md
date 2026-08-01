# GC v1

Version: **1.0.0**

M5 GC is a non-moving, precise, incremental, budgeted Mark/Sweep collector
with stable `GcRef` identity.

## Heap accounting

The heap accounts bytes precisely by category: object header bytes, class
payload bytes, string bytes, array bytes, buffer bytes, map bytes, allocator
slack, and profiler bytes. Resource limits cover `max_heap_objects`,
`max_heap_bytes`, `max_string_bytes`, and `max_collection_bytes`.

## Compact storage

With Struct/Enum out of the ordinary heap, remaining heap objects are Class,
String, Array, Buffer, and Map. Each uses a compact header and a suitable
payload arena; slots do not carry the maximum object-enum footprint.

## Incremental cycle

```text
Idle -> RootSnapshot -> Mark -> Sweep -> Complete
```

The cycle spans multiple engine ticks. Reference scanning uses
`trace_references(&self, visitor)` and never returns a temporary vector.
The mark stack is preallocated from the heap limit; during Mark and Sweep,
system allocation count and bytes are both zero.

## Write barrier

During Mark, every reference-writing operation maintains the tri-color
invariant: ClassSet, Array set/push/insert, Buffer writes, MapSet, state
writes, host-return publication, migration staging, and reload commit.

## Budgets and triggering

Sweep runs in slices bounded by slot count or time budget. Triggering
considers used heap bytes, used object slots, allocation rate, last survival
rate, collection arena fragmentation, and whether an incremental cycle is
already active. Engine ticks configure a `GcBudget { max_objects, max_bytes,
max_duration }`; the runtime reports actual work and budget overrun.
Explicit full collection remains available to tests, the inspector, REPL
`:gc`, and shutdown, but ordinary gameplay never depends on it.

## Telemetry

Each cycle reports phase, roots, objects/bytes marked, objects swept, bytes
reclaimed, live bytes, pause time, incremental work time, barrier count,
remembered writes, and fragmentation.

## Stress gates

Coverage includes 100,000 short-lived Class objects, rooted and unrooted
cycles, Class references inside Array/Map, Task suspend roots, host staging
roots, reload staging roots, REPL transaction rollback, object-graph
mutation during incremental Mark, and GC completion during shutdown.
