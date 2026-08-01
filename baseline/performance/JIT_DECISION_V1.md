# JIT Decision v1

Version: **1.0.0**

Status: **PENDING M5 FINAL PROFILE**

M5 ends with exactly one data-backed decision for M6:

```text
M6 LLVM JIT = GO
or
M6 LLVM JIT = DEFER
```

## GO conditions (all must hold)

1. Interpreter dispatch/execution accounts for at least 40% of CPU in at
   least two product workloads.
2. Host boundary and GC are no longer the first bottleneck.
3. Hot spots concentrate in a small set of long-lived functions.
4. Warm V8 still leads Nexa by at least 1.5x on at least three comparable
   pure-computation workloads.
5. LLVM compilation cost amortizes within the frozen call-count or
   frame-count budget.
6. ValueLayout, ExecutableModule, safepoints, root maps, and the Host ABI
   are frozen.

Otherwise the decision is `DEFER`, and if GC or the Host boundary remains
the first bottleneck, the next milestone is a targeted M5R1 rather than a
JIT.

## V8 comparison discipline

The V8 comparison is a decision input, not a completion gate. The
comparison report pins the V8/Node version, warmup protocol, and workload
mapping; it acknowledges that warm V8 is JIT-compiled code measured against
the Nexa interpreter. Absence of a working V8 environment on the
qualification machine blocks the decision report, not the rest of M5.

## Decision artifacts

```text
target/nexa-artifacts/m5/final/performance-report.json
target/nexa-artifacts/m5/final/performance-report.md
target/nexa-artifacts/m5/final/jit-decision.json
```

This document is updated in place with the final decision, its numeric
evidence, and the decision date; M5 completion requires the decision to
exist, not for it to be `GO`.
