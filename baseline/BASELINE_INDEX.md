# Nexa Architecture Baseline Index

Version: **1.2.0**

This is the only entry point for normative Nexa specifications. Historical design documents are
non-normative rationale. When texts conflict, precedence is:

```text
MVR Scope 1.0
> Gate 1 Acceptance
> current Architecture Baseline snapshot
> Roadmap
> historical rationale
```

## Current gate

Milestone 4.0R3 is complete at receipt
`reports/contracts/milestone4r3_verification_receipt.json`.

<!-- gate1-v2.8-status:start -->
Gate 1 v1 and Gate 1 v2 are **INVALID_APPARATUS**. Gate 1 v2.1 is **INVALID**, Gate 1 v2.2 is
**NOT TRUSTWORTHY**, and Gate 1 v2.3 is **SEMANTICALLY_INSUFFICIENT**. Gate 1 v2.4 is
**STRUCTURAL_CLOSURE_FAILED**. Gate 1 v2.5 is **STRUCTURAL_CLOSURE_FAILED** and not
decision-usable. Gate 1 v2.6 is **STRUCTURAL_CLOSURE_FAILED / INCOMPLETE** and its recorded STOP
is not authorized. Gate 1 v2.7 is **INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE** and not
decision-usable. Gate 1 v2.8 is **FROZEN / INCOMPLETE**, its decision is **NOT_COMPUTED**, and
Milestone 5.0R8 is **INCOMPLETE**.
<!-- gate1-v2.8-status:end -->

The current normative experiment entry points are:

- `testing/GATE1_ACCEPTANCE_V2_8.md` — unchanged outcome thresholds plus repaired Raw-to-Gate and direct Contract rules;
- `testing/GATE1_V2_8_AUTHORIZATION.md` — qualified-host execution authority and fixed zero-retry budget;
- `testing/GATE1_MACHINE.md` — experiment lifecycle;
- `../experiments/gate1-v2.8/manifest.json` — qualified frozen input binding after authorization;
- `../reports/gate1_v1_invalidation.md` — v1 historical invalidation;
- `../reports/gate1_v2_invalidation.md` — v2 historical invalidation;
- `../reports/gate1_v2_1_final_decision.md` — v2.1 historical INVALID decision;
- `../reports/gate1_v2_3_semantic_invalidation.md` — v2.3 historical semantic invalidation.

`../ROADMAP.md` provides navigation only and cannot change normative semantics.

## Active decisions

| ID | Status | Scope | Normative location | Supersedes |
|---|---|---|---|---|
| D44 | Active | MVR | `reload/RELOAD_TRANSACTION.md` | reload-time user defer |
| D45 | Active | MVR | `reload/RELOAD_TRANSACTION.md` | multi-field epoch publication |
| D46 | Active | MVR | `runtime/TASK_MACHINE.md` | ownerless fast-task admission |
| D47 | Active | MVR | `runtime/MODULE_MACHINE.md` | conflated active/retired faults |
| D48 | Active | MVR | `runtime/RESOURCE_MACHINE.md` | isolate-owned release queue |
| D49 | Active | MVR | `testing/EXPERIMENT_PROTOCOL.md` | separately handwritten models |
| D51 | Active | Governance | this file | historical-chain normativity |
| D52 | Active | MVR | `reload/RELOAD_TRANSACTION.md` | cross-thread epoch-root refcount |
| D53 | Active | MVR | `runtime/RESOURCE_MACHINE.md` | untracked host business resources |
| D54 | Active | MVR | `runtime/MODULE_MACHINE.md` | post-commit rollback claim |
| D55 | Active | MVR | `runtime/RESOURCE_MACHINE.md` | fallible release enqueue |
| D56 | Active | Experiment | `testing/EXPERIMENT_PROTOCOL.md` | binary experiment outcomes |
| D57 | Active | Experiment | `testing/GATE0_KILL_CRITERIA.md` | unbounded inconclusive retesting |
| D58 | Active | MVR | `runtime/HOST_REQUEST_MACHINE.md` | sender-wide completion reservation |
| D59 | Active | MVR | `runtime/RESOURCE_MACHINE.md` | allocating Realm release transfer |
| D60 | Active | MVR | `runtime/RETIRED_EPOCH.md` | per-frame retired-module scan |
| D61 | Active | Gate 1 | `testing/GATE1_ACCEPTANCE.md` | post-result threshold selection |
| D62 | Active | Gate 1 | `testing/GATE1_MACHINE.md` | informal experiment lifecycle |

## Deferred decisions

| Capability | Status | Earliest review |
|---|---|---|
| Dynamic values | Deferred | Gate 2 RFC |
| Interfaces | Deferred | Gate 2 RFC |
| User-defined generics | Deferred | 1.x RFC |
| Cross-module state handles | Deferred | Gate 2 RFC |
| Reload groups | Deferred | Gate 2 RFC |
| Read/write leases | Deferred | Experimental RFC |
| Compatible ABI adapters | Deferred | Gate 2 RFC |
| Strict deterministic scheduling | Deferred | Gate 2 RFC |
| Untrusted bytecode security verifier | Deferred | Gate 2 RFC |
| AOT/JIT | Deferred | Data-driven RFC |

## Superseded decisions

| Item | Status | Replacement |
|---|---|---|
| Gate 1 read lease | Superseded | Copy + immutable snapshot only |
| Cross-thread root acquire + refcount | Superseded | VM-thread-only root access |
| Cancel old tasks before reload staging | Superseded | Pause, stage, then commit |
| Run user defer after reload commit | Superseded | VM-managed cleanup only |
