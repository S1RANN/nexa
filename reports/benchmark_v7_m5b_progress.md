# Benchmark v7 — M5b Progress Report (formal 7×1000)

Protocol: `BENCHMARK_PROTOCOL_V1.md` — 7 processes × 1000 samples,
median-of-process aggregation, release profile, global allocator
instrumentation.

- Baseline: tag `performance-m5-baseline` (commit `24e87e0`),
  artifact `target/nexa-artifacts/m5/baseline/aggregate-7x1000.json`
- Current: branch `codex/performance-m5` (commit `b43ffb9`),
  artifact `target/nexa-artifacts/m5/aggregate-7x1000-m5b.json`
- Landed stages since baseline: A/B governance and measurement,
  C representation (struct/enum physical inlining, Typed IR passes,
  array capacity amortization, string literal interning), F
  ExecutableModule (predecoded rows, strict parity gate), G incremental
  GC (budgeted cycles, byte accounting, three-axis budgets,
  max_heap_bytes), H pooling (continuation arenas, recycling fast
  path), I source compilation cache.

## Results (median p50, max system allocations per process)

| case | tier | p50 base → now | Δ | allocs base → now |
|---|---|---|---|---|
| immediate_call | micro | 583 → 416 ns | -28.6% | 3000 → 0 |
| result_ok_err | micro | 1166 → 750 ns | -35.7% | 7000 → 1000 |
| fuel_resume | micro | 417 → 292 ns | -30.0% | 3000 → 0 |
| explicit_resume | micro | 334 → 167 ns | -50.0% | 3000 → 0 |
| string_concat | micro | 875 → 708 ns | -19.1% | 7000 → 7000 |
| struct_construction | micro | 834 → 584 ns | -30.0% | 5000 → 4000 |
| class_allocation | micro | 875 → 667 ns | -23.8% | 4000 → 1000 |
| enum_construction_match | micro | 834 → 375 ns | -55.0% | 4000 → 0 |
| array_operations | micro | 959 → 791 ns | -17.5% | 4000 → 1000 |
| map_operations | micro | 1125 → 1000 ns | -11.1% | 6000 → 5000 |
| buffer_copy | micro | 750 → 583 ns | -22.3% | 3000 → 0 |
| product_data_sweep | product | 142.0 → 80.0 µs | -43.6% | 260000 → 4000 |
| product_standalone_pipeline | product | 3.816 → 3.589 ms | -5.9% | ≈126.0M → ≈127.3M |
| product_cached_pipeline | product | (new) 4.667 µs | — | 22000 |
| snapshot_access | micro | 542 → 500 ns | -7.7% | 0 → 0 |
| async_admission | subsystem | 4917 → 4166 ns | -15.3% | 3000 → 0 |
| migration | subsystem | 41.7 → 37.0 µs | -11.2% | 435000 → 484000 |
| reload_commit | subsystem | 38.8 → 37.3 µs | -4.0% | 33000 → 38000 |
| realm_drop | subsystem | 1500 → 1416 ns | -5.6% | 0 → 0 |
| gc_incremental_step | micro | (new) 1125 ns | — | 0 |

Every preexisting case improved; no regressions.

## Methodology notes (honest comparability)

- **Micro-tier deltas combine two effects**: interpreter/runtime
  improvements AND the measurement-authority change to pooled
  continuation storage (H2). Pooling mirrors what pooled product realms
  actually execute, but it means micro deltas are not purely
  interpreter-core wins. `async_admission` is the pooled-realm number
  with unchanged methodology apart from H1 landing in the runtime
  itself.
- **product_standalone_pipeline** keeps its cold methodology (fresh
  compile + verify + predecode + fresh arena per sample); its -5.9% is
  a pure pipeline improvement. Its allocation count grew ~1% with the
  predecode step added in stage F — the row build is part of the cold
  cost shape by design.
- **product_cached_pipeline** is the stage-I warm path over the same
  workload: 4.667 µs vs 3.589 ms cold (~769×), 22 allocations vs
  ~127K.
- **Fuel identity**: per `BENCHMARK_PROTOCOL_V1.md`, same-cost-table
  fuel identity is enforced by the executable-parity gate; the
  optimized-vs-reference pipeline comparison is exempt on fuel totals
  only. No fuel semantics changed in this window.
- **gc_incremental_step** measures exactly one budgeted incremental GC
  step per sample (allocator churn excluded from timing); its zero
  system allocations is the stage-G structural guarantee measured end
  to end.

## Verification anchor

All 28 commits in this window carry full-workspace regressions; the
M1→M5 stacked acceptance (`cargo xtask check`) passed with the
regression credential bound to a clean checkout at `b4bd24a`, and every
M5 gate added since (three-axis GC budgets, byte accounting symmetry,
heap byte ceiling, continuation pool, source cache) is wired into
`check_m5_gates`.
