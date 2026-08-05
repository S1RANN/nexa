# Nexa M5 Scope

Version: **1.0.0**

Status: **COMPLETE**

M5 is the deep performance optimization milestone. It optimizes value
representation, the interpreter, collections, GC, Task/Host boundaries, and
product workloads behind frozen observable semantics.

## Staged completion

M5 completes in two independently finalized stages so that every landed layer
is gated before the next layer starts:

```text
M5a  Stages A-F: measurement authority, ValueLayout, Typed IR passes,
     typed collections, ExecutableModule
M5b  Stages G-J: incremental GC v1, Task/Host/Engine fast paths, caches,
     product corpus, JIT decision
```

Stage C (representation), Stage F (ExecutableModule), and Stage G (GC) land
strictly in that order; each must pass its differential gate before the next
stage may change runtime behavior.

## Non-goals

M5 does not include: LLVM JIT, AOT, machine-code caching, new language or
NIDL syntax, user generics, traits, closures, inheritance, dynamic dispatch,
operator overloading, macros, reflection, `dynamic`/`any`, raw pointers,
`Box<T>`, a borrow checker, moving GC, generational nursery GC, public weak
references, user finalizers, shared-memory multithreaded VM, full semantic
LSP, DAP, remote package registries, or Pluie integration.

## GC target

```text
non-moving, precise, incremental, budgeted Mark/Sweep, stable GcRef
```

Whether a generational nursery is added is decided by the final M5 profile,
not during implementation.

## Version boundaries

M5 may upgrade `BYTECODE_VERSION` (6 to 7), `OPCODE_COST_TABLE_VERSION`,
the ExecutableModule schema, the Benchmark schema, and the Profiler schema.
M5 does not change `NEXA_LANGUAGE_VERSION = 2`, `NIDL_SYNTAX_VERSION = 2`,
`HOST_CONTRACT_SCHEMA_VERSION = 2`, or `ABI_DESCRIPTOR_VERSION = 2`.

## Frozen observable semantics

M5 must not change: integer and float results, deterministic math, the fuel
accounting mechanism (see `BENCHMARK_PROTOCOL_V1.md` for the per-program
fuel-total ruling), trap kinds, Task yield/resume, Host requests,
cancel/abandon policies, Class identity, Struct/Enum value semantics, package
reload, Last Known Good, source maps, script call stacks, or permission and
resource limits.

## Correctness precedence

If an optimization conflicts with verifier safety, precise roots, write
barriers, fuel, resource ceilings, failure atomicity, reload rollback,
diagnostic locations, or deterministic results, the optimization is removed
rather than the condition weakened.

## Completion evidence

M5 completed both staged slices under the frozen Benchmark v7 protocol. The
qualification run used 7 independent processes with 1,000 samples per case
and a live rebuild of `performance-m5-baseline` on the same Apple M4 Pro
machine. It met all four geometric-mean targets and reported no unexplained
p95/p99 regression:

```text
Product CPU corpus          2.397x  (target 1.50x)
Value/collection corpus     2.444x  (target 2.00x)
Host/Task/Engine corpus     1.485x  (target 1.30x)
Cold-start corpus           1.650x  (target 1.20x)
```

The committed, independently reviewable release attestation is
[`M5_RELEASE_SUMMARY.json`](M5_RELEASE_SUMMARY.json). The finalizer validates
that summary against the frozen versions, targets, and live receipts and
records its BLAKE3 digest in the terminal tagged-HEAD authority at
`target/nexa-artifacts/m5-finalize/final-report.json`. Raw benchmark receipts
remain uncommitted under `target/nexa-artifacts/m5/`. The resulting M6 LLVM
JIT decision is `DEFER`, as documented in `JIT_DECISION_V1.md`.
