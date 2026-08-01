# Performance Targets v1

Version: **1.0.0**

All targets are measured on the qualification machine with the frozen
Benchmark v7 harness and multi-process protocol, comparing HEAD against the
`performance-m5-baseline` tag live in a temporary worktree.

## Throughput targets (geometric mean over corpus)

```text
Product CPU corpus          >= 1.50x baseline
Value/collection corpus     >= 2.00x baseline
Host/Task/Engine corpus     >= 1.30x baseline
Cold-start corpus           >= 1.20x baseline
```

The value/collection multiplier is expected to come from representation
work (ValueLayout, typed collections, ExecutableModule), not interpreter
dispatch tricks; stable Rust offers no computed-goto or guaranteed
tail-call dispatch, and no target here assumes one.

## Latency and regression discipline

No mandatory case may regress p95 or p99 by more than 10% without a
written explanation in the final report. Snake 50-package tick p99 stays
within the frozen script frame budget. GC per-tick work never exceeds 110%
of the frozen GC budget.

## Structural gates (absolute, not averaged away)

```text
Local pure-scalar Struct construction: 0 GC objects
Local pure-scalar Enum construction:   0 GC objects
Struct parameters and returns:         0 heap materializations
Array<Struct>:                         no per-element objects
Hot string literal load:               0 new String objects
N consecutive pushes:                  relocation copies <= N elements
GC Mark/Sweep:                         0 system allocations
Stable IDs:                            fully dense-resolved before execution
Immediate entrypoints:                 no full scheduler Task
Steady-state Engine dispatch:          0 system allocations
Profiler disabled overhead:            <= 2%
Profiler enabled overhead:             <= 15%
```

## Semantic gates

Fuel (per the protocol's version-boundary ruling), traps, source spans,
Task/Request lifecycles, Class identity, Struct/Enum value semantics, GC
roots, reload rollback, and Last Known Good behavior are identical between
the reference and optimized pipelines.
