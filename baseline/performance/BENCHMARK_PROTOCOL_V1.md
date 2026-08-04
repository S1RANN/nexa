# Benchmark Protocol v1

Version: **1.0.1**

Benchmark v7 (`tools/benchmark-v7`) is the only performance measurement
authority for M5. Benchmark v6 remains in the tree, frozen, until every v7
output, gate, and semantic is complete; final active gates use v7 only.

## Corpus tiers

```text
Micro      scalar ops, branches, loops, calls, Struct, Enum, Class, String,
           Array, Map, Buffer, Host call, Task, GC
Subsystem  Engine dispatch, Task admission, Task resume, async completion,
           Migration, Reload, artifact load, REPL cell
Product    Snake, Combat, Standalone, REPL, data-intensive scripts
```

## Execution protocol

Every case performs isolated initialization, fixed warmup, a fixed sample
count, black-boxed results, a correctness assertion on the computed result,
and resource cleanup. First-compile and initialization costs never enter hot
metrics; cold-start cases measure them explicitly and separately.

Formal comparisons use at least 7 independent processes, each with its own
warmup, and at least 1,000 formal samples per case per process. The reported
aggregate is the median across process medians; selecting the fastest process
is forbidden.

## Qualification machine

Formal reports are valid only from the pinned qualification machine
configuration recorded in each report: machine model, CPU model, logical CPU
count, OS version, power source, and thermal policy. Reports from other
machines are exploratory and cannot satisfy gates. CI is not part of M5
completion evidence.

Per-process Benchmark v7 reports use schema 1. The current multi-process
aggregate uses schema 2 so it retains those qualification fields, the exact
toolchain, build profile, bytecode hash, profiler mode, process count, sample
count, warmup count, and validated case inventory instead of discarding child
provenance. This is an evidence-envelope correction only; it does not change
the frozen timing, warmup, corpus, or median-of-medians protocol.

The immutable baseline tag predates that aggregate envelope and emits schema
1. It is accepted only when the finalizer has synchronously rebuilt and run
the tag in a temporary worktree and bound it to the immediately preceding
live HEAD qualification from that same finalizer call. Prerecorded baseline
aggregates are never accepted.

## Baseline discipline

The `performance-m5-baseline` annotated tag is created only after the v7
harness, report schema, counters, and this protocol are final and before any
runtime hot-path optimization lands. The finalizer re-runs baseline and HEAD
live on the same machine in a temporary worktree; prerecorded reports are not
comparison inputs.

## Fuel ruling

The fuel accounting mechanism is frozen: charging order, slice exhaustion
into `FuelYielded`, cumulative budgets, and terminal settlement do not
change. Per-program fuel totals are version-scoped: they may change only at
the single frozen `BYTECODE_VERSION`/`OPCODE_COST_TABLE_VERSION` upgrade
boundary of M5, after which they are deterministic again.

Differential gates therefore compare fuel exactly when both sides execute
the same compiled artifact under the same cost-table version (Portable
reference interpreter versus ExecutableModule interpreter). Comparisons that
cross the version boundary (old pipeline versus new pipeline) require
identical results, traps, task lifecycles, and identities, but not identical
fuel totals. A report that relies on the cross-version exemption must say so
explicitly.

## Report location

Raw reports are written under `target/nexa-artifacts/m5/` and are not
committed. The harness hash, protocol, and baseline tag are committed.
