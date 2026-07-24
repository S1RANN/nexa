# Fast Task Runtime Benchmark v0

Version: 0.1.0

This report measures the Task runtime and safe `FrameArena` only. It does not make claims about
full-language performance, host latency, or GC pause time.

## Method

- Command: `cargo +1.97.0 run --release -p nexa-runtime --example fast_task_bench`
- Samples per case: 10,000
- Platform: Darwin 25.5.0 arm64
- Compiler: rustc 1.97.0 (2d8144b78 2026-07-07)
- Date: 2026-07-24
- Each task admission reserves its task slot, frame segment, scheduler token, trace capacity, and
  scope membership before the first operation.
- Promotion retains these reservations; it does not reserve or allocate a continuation.

## Results

| Case | Mean | P50 | P95 | P99 |
|---|---:|---:|---:|---:|
| empty complete | 2,204 ns | 1,708 ns | 3,500 ns | 4,417 ns |
| 10 compute ops | 15 ns | 0 ns | 42 ns | 42 ns |
| 100 compute ops | 70 ns | 83 ns | 84 ns | 84 ns |
| fuel promotion/resume/complete | 2,730 ns | 2,541 ns | 3,584 ns | 4,250 ns |
| call depth 32 push/pop | 295 ns | 292 ns | 375 ns | 416 ns |
| empty scope cancellation | 2,069 ns | 1,959 ns | 2,583 ns | 3,250 ns |
| trace-disabled empty complete | 1,646 ns | 1,625 ns | 1,750 ns | 2,125 ns |

The benchmark performs no hot-path pool growth after runtime construction. Rust's safe standard
allocator is still used while constructing the runtime, trace storage, and benchmark sample
vectors; this version does not claim a process-wide allocation count.
