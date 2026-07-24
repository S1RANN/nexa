# Nexa Architecture Baseline Index

Version: **1.0.1**

This is the only entry point for normative Nexa specifications. Historical design documents are
non-normative rationale. When texts conflict, precedence is:

```text
MVR Scope 1.0
> current Architecture Baseline snapshot
> historical rationale
```

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
