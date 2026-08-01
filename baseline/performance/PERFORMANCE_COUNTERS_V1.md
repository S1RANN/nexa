# Performance Counters v1

Version: **1.0.0**

## System allocation counters

The benchmark counting allocator records, per measured region:

```text
alloc_count
realloc_count
allocated_bytes
reallocated_bytes
peak_outstanding_bytes
```

Measurement regions explicitly exclude benchmark result collection, report
serialization, and harness bookkeeping.

## VM counters

The runtime records:

```text
object_allocations
string_allocations
class_allocations
collection_storage_allocations
map_slot_allocations
struct_materializations
enum_materializations
collection_relocation_bytes
string_copy_bytes
host_codec_copy_bytes
```

## Allocation sites

An allocation site is identified by package ID, module, function stable ID,
source span, allocation kind, and type ID. Profiler output never reports
anonymous totals without a site breakdown.

## Profiler

The bounded runtime profiler exposes `PerformanceProfile`,
`FunctionProfile`, `OpcodeProfile`, `AllocationProfile`, `GcProfile`,
`HostCallProfile`, and `TaskProfile`. When disabled it allocates nothing,
takes no global mutex, and maintains no hash table.

Overhead thresholds, enforced by gate:

```text
disabled: hot-corpus overhead <= 2%
enabled:  hot-corpus overhead <= 15%
storage:  always bounded; overflow increments a dropped counter
```
