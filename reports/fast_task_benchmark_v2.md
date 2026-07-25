# Fast Task Runtime Benchmark v2

Version: Milestone 3 Host-Integrated Stateful Runtime

## Method

- Command: `cargo +1.97.1 run --release -p nexa-runtime --example fast_task_bench --features allocation-counting`
- Warmup: 100 iterations per case
- Timed samples: 1,000 for hot paths; 200 for host/resource/reload/GC paths
- Platform: macOS aarch64
- Implementation commit: `0c2c0ee7e008bb334a15ab712d4a5b8b07bd98d7`
- Raw result: `reports/raw/fast_task_benchmark_v2.json`
- `Generated Rust thunk direct call` is the direct host control. The immediate and async cases both execute verified `HOST_CALL` bytecode through the Realm-owned registry; async includes pending request, completion delivery, destination-register writeback, and resume.
- GC and trace-disabled execution remain separately measured.

## Results

| Case | Mean ns | P50 ns | P95 ns | P99 ns |
|---|---:|---:|---:|---:|
| Rust direct | 24 | 41 | 42 | 42 |
| Verified immediate | 577 | 542 | 584 | 625 |
| Nexa fast complete | 2,118 | 1,875 | 2,334 | 5,958 |
| Nexa fast complete, trace off | 793 | 792 | 1,041 | 1,125 |
| Nexa fuel promotion/resume | 2,247 | 2,292 | 3,333 | 3,541 |
| Nexa nested calls | 1,470 | 1,416 | 2,167 | 2,375 |
| Generated Rust thunk direct call | 31 | 42 | 42 | 42 |
| Nexa `HOST_CALL`, immediate | 1,531 | 1,542 | 1,917 | 2,458 |
| Nexa `HOST_CALL`, async completion | 5,108 | 4,875 | 5,708 | 6,916 |
| Nexa resource token | 3,740 | 3,709 | 3,875 | 4,541 |
| Nexa snapshot read | 3,542 | 3,542 | 3,917 | 4,542 |
| Nexa state handle | 32 | 42 | 42 | 42 |
| Nexa reload | 5,650 | 5,458 | 6,250 | 6,500 |
| Nexa GC collection | 2,497 | 2,541 | 2,792 | 3,208 |

## Signal status

- H1: measured.
- H2: inconclusive from the VM-managed counter alone. The separate global allocator observer supplies the zero-allocation evidence.
- H3: measured.
- Real allocator evidence: `reports/raw/allocation_observer_v2.json`, three repetitions with zero promotion, resume, and trace-off allocations.
