# Nexa Benchmark v6

Benchmark v6 measures the complete Milestone 4.0 MVR surface with real verified bytecode,
runtime host resources, offline migration, reload publication, and Realm teardown.

The reproducible command is:

```text
cargo run --release -p nexa-benchmark-v6 -- --samples 1000 --output reports/raw/benchmark_v6.json
```

For every case the JSON records throughput, mean/P50/P95/P99 latency, system allocator calls
inside the timed operation, peak live heap slots, fuel, executed instructions, and peak runtime
resources. Per-sample setup and result-vector storage are deliberately outside the timed and
allocation-counted region.

The covered cases are immediate call, async admission, `Result` Ok/Err, fuel resume, explicit
resume, string concatenation, struct construction, class allocation, enum construction/match,
array operations, map operations, buffer copy, snapshot access, migration, reload commit, and
Realm drop.

Raw machine-readable evidence is stored in `reports/raw/benchmark_v6.json`.

## Measured environment

- Target implementation: `a021d7c12cb91b94680f75152c00e121c5aee92c`
- Toolchain: `rustc 1.97.1 (8bab26f4f 2026-07-14)`
- Platform: macOS aarch64
- Timed samples: 1,000 for the regular cases; 200 for migration, reload commit, and Realm drop
- Build profile: release

## Results

System allocations are totals over the case's timed samples. Fuel and instructions are per
operation. Peak resources is the maximum sum of all authoritative Realm ledger classes.

| Case | ops/s | P50 ns | P95 ns | P99 ns | System allocations | Peak heap slots | Fuel/op | Instructions/op | Peak resources |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| immediate call | 1,183,902 | 833 | 875 | 917 | 3,000 | 0 | 4 | 4 | 0 |
| Result Ok/Err | 965,608 | 834 | 1,833 | 1,958 | 6,000 | 3 | 6 | 6 | 0 |
| fuel resume | 1,984,083 | 500 | 584 | 625 | 3,000 | 0 | 14 | 14 | 0 |
| explicit resume | 1,640,072 | 500 | 875 | 1,125 | 3,000 | 0 | 2 | 2 | 0 |
| string concat | 1,193,600 | 625 | 1,333 | 1,459 | 6,000 | 3 | 6 | 6 | 0 |
| struct construction | 1,330,872 | 584 | 1,083 | 1,250 | 4,000 | 2 | 8 | 8 | 0 |
| class allocation | 1,404,107 | 625 | 1,125 | 1,167 | 3,000 | 2 | 13 | 13 | 0 |
| enum construction/match | 1,388,083 | 584 | 1,083 | 1,125 | 3,000 | 1 | 16 | 16 | 0 |
| array operations | 893,329 | 917 | 1,667 | 2,000 | 16,000 | 1 | 17 | 17 | 0 |
| map operations | 639,474 | 1,208 | 2,292 | 2,916 | 26,000 | 3 | 17 | 17 | 0 |
| buffer copy | 1,021,625 | 833 | 1,541 | 1,667 | 16,000 | 2 | 9 | 9 | 0 |
| snapshot access | 1,323,348 | 667 | 1,042 | 1,084 | 0 | 0 | 0 | 0 | 6 |
| async admission/completion | 178,439 | 5,292 | 9,125 | 10,458 | 3,000 | 1,025 | 3 | 3 | 1,031 |
| migration | 73,833 | 12,417 | 16,459 | 16,709 | 42,200 | 1 | 22 | 22 | 1 |
| reload commit | 218,499 | 4,250 | 5,542 | 6,708 | 600 | 0 | 1 | 1 | 3 |
| Realm drop | 568,204 | 1,708 | 2,042 | 2,334 | 0 | 0 | 0 | 0 | 4 |

The async case deliberately retains terminal result values across the run, so its peak heap and
ledger values expose the configured tombstone retention cost instead of hiding it with
per-iteration Realm construction.
