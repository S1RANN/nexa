# Nexa Baseline Index

Version: **5.0.0**

```text
Nexa Internal Pivot M1 = COMPLETE
Nexa M2 Embedding & Snake Pilot = COMPLETE
Nexa M3 Developer Loop & Diagnostics = COMPLETE
NexaEngine API = COMPLETE
Automatic Candidate Compilation = COMPLETE
Candidate Generation Terminal Accounting = COMPLETE
Candidate Freshness Commit Guard = COMPLETE
Last Known Good Reload = COMPLETE
Unified Diagnostics = COMPLETE
Source-level Runtime Stack Traces = COMPLETE
Package-aware CLI = COMPLETE
Editor Diagnostics = COMPLETE
nexa-embed v1 = COMPLETE
Typed Script Export = COMPLETE
Snake Core = COMPLETE
Built-in Package Pilot = COMPLETE
Official DLC Pilot = COMPLETE
Trusted Local Mod Pilot = COMPLETE
Repository Slimming = COMPLETE
Rust Host Binding v1 = COMPLETE
Task Runtime Stabilization = COMPLETE
Restart Reload v1 = COMPLETE
Combat Dogfood Loop = COMPLETE
Nexa M4 Language Scale Foundation = COMPLETE
Nexa M4R1 Language Surface Reset = COMPLETE
Nexa Language v2 = COMPLETE
NIDL v2 = COMPLETE
Structured Codegen v2 = COMPLETE
Standalone Profile v1 = COMPLETE
REPL v1 = COMPLETE
Multiple Entrypoint Model = COMPLETE
Nexa M5 Deep Performance Optimization = COMPLETE
Performance Measurement Authority v1 = COMPLETE
Value Layout v1 = COMPLETE
ExecutableModule v1 = COMPLETE
Incremental GC v1 = COMPLETE
Runtime Fast Paths v1 = COMPLETE
M6 LLVM JIT = DEFER
```

This is the only normative entry point for the active Internal Language Pivot.
Precedence is:

```text
Internal Language Scope
> Language v2 and Object Model v2
> Async v2, Standalone v1, and REPL v1
> NIDL v2, Contract Descriptor v2, and Binding Codegen v2
> Host Binding
> Task Runtime
> Restart Reload
> Embedding and package lifecycle
> Developer loop and diagnostics
> ABI and machine details
> Roadmap
```

M1 conformance requires real handwritten Business Host mutation checks executed
through each changed generated Registry, a real-`RealmRuntime` differential/fuzz
adapter with observable public API attempts, no public Request or manual Waiting
constructor, exactly-once Request/Token/Snapshot release, and Restart Reload
rollback without old-Task revival.

M3R3 conformance requires a fail-closed desired source identity, a commit-time
source refresh, and unified supersession of stale Candidate work before Worker
admission, inside the Worker, in the Result queue, and after verification. The
immutable M3, M3R1, and M3R2 completion tags remain unchanged. M4 has not
changed those historical baselines. M4 is complete from the immutable M3R3
completion commit and is marked by the annotated
`language-scale-m4-complete` tag.

M4R1 is the completed breaking reset of the source language, NIDL, generated
Rust bindings, Standalone entry model, and REPL. No M4 source or NIDL
compatibility surface is normative. The specifications below define the only
accepted v2 surface.

## Frozen active versions

```text
NEXA_LANGUAGE_VERSION = 2
NIDL_SYNTAX_VERSION = 2
HOST_CONTRACT_SCHEMA_VERSION = 2
ABI_DESCRIPTOR_VERSION = 2
BYTECODE_VERSION = 7
```

These are structured protocol values, not display strings. Active products
reject every other source, Contract, Descriptor, or Bytecode version instead
of selecting a compatibility parser or decoder.

## Active specifications

- [`internal/INTERNAL_LANGUAGE_SCOPE.md`](internal/INTERNAL_LANGUAGE_SCOPE.md)
- [`language/LANGUAGE_V2.md`](language/LANGUAGE_V2.md)
- [`language/OBJECT_MODEL_V2.md`](language/OBJECT_MODEL_V2.md)
- [`language/ASYNC_V2.md`](language/ASYNC_V2.md)
- [`language/STANDALONE_V2.md`](language/STANDALONE_V2.md)
- [`language/REPL_V1.md`](language/REPL_V1.md)
- [`abi/NIDL_V2.md`](abi/NIDL_V2.md)
- [`abi/CONTRACT_DESCRIPTOR_V2.md`](abi/CONTRACT_DESCRIPTOR_V2.md)
- [`abi/BINDING_CODEGEN_V2.md`](abi/BINDING_CODEGEN_V2.md)
- [`internal/HOST_BINDING.md`](internal/HOST_BINDING.md)
- [`internal/TASK_RUNTIME.md`](internal/TASK_RUNTIME.md)
- [`internal/RESTART_RELOAD.md`](internal/RESTART_RELOAD.md)
- [`embed/EMBED_API.md`](embed/EMBED_API.md)
- [`embed/PACKAGE_SOURCE.md`](embed/PACKAGE_SOURCE.md)
- [`embed/PACKAGE_POLICY.md`](embed/PACKAGE_POLICY.md)
- [`embed/PACKAGE_LIFECYCLE.md`](embed/PACKAGE_LIFECYCLE.md)
- [`embed/SNAKE_PILOT.md`](embed/SNAKE_PILOT.md)
- [`embed/DEVELOPMENT_WORKER.md`](embed/DEVELOPMENT_WORKER.md)
- [`embed/ENGINE_DIAGNOSTICS.md`](embed/ENGINE_DIAGNOSTICS.md)
- [`../docs/DEVELOPMENT_LOOP.md`](../docs/DEVELOPMENT_LOOP.md)
- [`../docs/DIAGNOSTICS.md`](../docs/DIAGNOSTICS.md)
- [`../docs/RELOAD_WORKFLOW.md`](../docs/RELOAD_WORKFLOW.md)
- [`../docs/EDITOR_SUPPORT.md`](../docs/EDITOR_SUPPORT.md)
- [`../docs/MODULES.md`](../docs/MODULES.md)
- [`../docs/LOCAL_LIBRARIES.md`](../docs/LOCAL_LIBRARIES.md)
- [`../docs/PACKAGE_TESTS.md`](../docs/PACKAGE_TESTS.md)
- [`../docs/INCREMENTAL_ANALYSIS.md`](../docs/INCREMENTAL_ANALYSIS.md)
- [`../docs/MIGRATING_TO_M4.md`](../docs/MIGRATING_TO_M4.md)
- [`../docs/NIDL.md`](../docs/NIDL.md)
- [`../docs/STANDALONE.md`](../docs/STANDALONE.md)
- [`../docs/REPL.md`](../docs/REPL.md)
- [`abi/IDL.md`](abi/IDL.md)
- [`abi/BYTECODE.md`](abi/BYTECODE.md)
- [`performance/M5_SCOPE.md`](performance/M5_SCOPE.md)
- [`performance/BENCHMARK_PROTOCOL_V1.md`](performance/BENCHMARK_PROTOCOL_V1.md)
- [`performance/PERFORMANCE_COUNTERS_V1.md`](performance/PERFORMANCE_COUNTERS_V1.md)
- [`performance/VALUE_LAYOUT_V1.md`](performance/VALUE_LAYOUT_V1.md)
- [`performance/EXECUTABLE_MODULE_V1.md`](performance/EXECUTABLE_MODULE_V1.md)
- [`performance/GC_V1.md`](performance/GC_V1.md)
- [`performance/PERFORMANCE_TARGETS_V1.md`](performance/PERFORMANCE_TARGETS_V1.md)
- [`performance/JIT_DECISION_V1.md`](performance/JIT_DECISION_V1.md)
- [`performance/M5_RELEASE_SUMMARY.json`](performance/M5_RELEASE_SUMMARY.json)
- [`runtime/HANDLES.md`](runtime/HANDLES.md)
- [`runtime/RESOURCE_MACHINE.md`](runtime/RESOURCE_MACHINE.md)
- [`runtime/SCOPE_MACHINE.md`](runtime/SCOPE_MACHINE.md)
- [`runtime/TASK_MACHINE.md`](runtime/TASK_MACHINE.md)

## Active decisions

| ID | Status | Scope | Normative location | Supersedes |
|---|---|---|---|---|
| D63 | Active | Internal language | `internal/INTERNAL_LANGUAGE_SCOPE.md` | old general-product MVR |
| D64 | Active | Host binding | `internal/HOST_BINDING.md` | handwritten host dispatch and ABI tables |
| D65 | Active | Task runtime | `internal/TASK_RUNTIME.md` | caller-authored low-level task events |
| D66 | Active | Reload | `internal/RESTART_RELOAD.md` | seamless multi-epoch reload |
| D67 | Active | Language and object model v2 | `language/LANGUAGE_V2.md`, `language/OBJECT_MODEL_V2.md` | M4 source surface |
| D68 | Active | NIDL and generated bindings v2 | `abi/NIDL_V2.md`, `abi/CONTRACT_DESCRIPTOR_V2.md`, `abi/BINDING_CODEGEN_V2.md` | NIDL v1 surface and legacy index generation |
| D69 | Active | Standalone and REPL | `language/STANDALONE_V2.md`, `language/REPL_V1.md` | low-level function-index-only execution |
| D70 | Active | M5 performance authority | `performance/M5_SCOPE.md` | benchmark-v6-only performance evidence |

## Historical boundary

The old general-product MVR is not normative. Its final decision is:

```text
Gate 1 v2.9 = STOP
H1/H2/H3 = FAIL
```

The active tree stores only
[`docs/history/GATE1_V2_9_STOP.md`](../docs/history/GATE1_V2_9_STOP.md).
All old experiment inputs, Raw evidence, contracts, receipts, tools, and
acceptance/authorization documents are recoverable from the immutable
annotated tag `gate1-v2.9-stop`.

`ROADMAP.md` provides navigation and cannot override these specifications.
