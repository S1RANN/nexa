# Fast Task Runtime Benchmark v1

Version: Milestone 2B Unified Executable Runtime

## Method

- Command: `cargo +1.97.0 run --release -p nexa-runtime --example fast_task_bench --features allocation-counting`
- Warmup: 100 iterations per case
- Timed samples: 1,000 for hot paths; 200 for host/resource/reload/GC paths
- Platform: macOS aarch64
- Toolchain: rustc 1.97.0
- Raw result: `reports/raw/fast_task_benchmark_v1.json`
- Every Nexa case uses verified bytecode and the real interpreter or `RealmRuntime`; no “Nexa compute” case is a Rust loop.
- GC has its own case and is not folded into task latency.
- Trace-enabled and trace-disabled fast-complete paths are measured separately.

## Results

| Case | Mean ns | P50 ns | P95 ns | P99 ns |
|---|---:|---:|---:|---:|
| Rust direct | 22 | 41 | 42 | 42 |
| Verified Nexa immediate | 601 | 542 | 542 | 625 |
| Nexa fast complete | 4,397 | 4,333 | 5,917 | 11,625 |
| Nexa fast complete, trace off | 3,489 | 3,208 | 4,959 | 5,541 |
| Nexa fuel promotion/resume | 5,742 | 5,750 | 6,750 | 9,708 |
| Nexa nested calls | 3,606 | 3,583 | 3,917 | 4,625 |
| Nexa sync host thunk | 33 | 42 | 42 | 42 |
| Nexa async host completion | 9,039 | 8,625 | 9,666 | 11,833 |
| Nexa resource token | 6,053 | 5,958 | 7,167 | 7,458 |
| Nexa snapshot read | 4,384 | 4,375 | 4,541 | 5,458 |
| Nexa state handle | 27 | 41 | 42 | 42 |
| Nexa reload | 8,226 | 8,292 | 8,584 | 9,208 |
| Nexa GC collection | 2,387 | 2,375 | 2,459 | 2,500 |

## Fuel and allocation signals

- Fast complete: 1 fuel.
- Fuel-yield/resume: 1 fuel across the measured protocol.
- Nested call: 3 fuel.
- VM-managed allocation events: admission 21,003; first slice 0; promotion 0; resume 0; terminal cleanup 0.
- The H2a hard assertion `promotion allocations = 0` runs in the benchmark executable.

Allocation counting records VM-managed capacity/allocation events, not every allocation performed by the process allocator. The workspace forbids unsafe code, so this gate deliberately avoids a custom global allocator.
