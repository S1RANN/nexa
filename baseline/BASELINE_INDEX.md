# Nexa Baseline Index

Version: **2.0.0**

```text
Nexa Internal Pivot M1 = COMPLETE
Nexa M2 Embedding & Snake Pilot = COMPLETE
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
```

This is the only normative entry point for the active Internal Language Pivot.
Precedence is:

```text
Internal Language Scope
> Host Binding
> Task Runtime
> Restart Reload
> Embedding and package lifecycle
> ABI and machine details
> Roadmap
```

M1 conformance requires real handwritten Business Host mutation checks executed
through each changed generated Registry, a real-`RealmRuntime` differential/fuzz
adapter with observable public API attempts, no public Request or manual Waiting
constructor, exactly-once Request/Token/Snapshot release, and Restart Reload
rollback without old-Task revival.

## Active specifications

- [`internal/INTERNAL_LANGUAGE_SCOPE.md`](internal/INTERNAL_LANGUAGE_SCOPE.md)
- [`internal/HOST_BINDING.md`](internal/HOST_BINDING.md)
- [`internal/TASK_RUNTIME.md`](internal/TASK_RUNTIME.md)
- [`internal/RESTART_RELOAD.md`](internal/RESTART_RELOAD.md)
- [`embed/EMBED_API.md`](embed/EMBED_API.md)
- [`embed/PACKAGE_SOURCE.md`](embed/PACKAGE_SOURCE.md)
- [`embed/PACKAGE_POLICY.md`](embed/PACKAGE_POLICY.md)
- [`embed/PACKAGE_LIFECYCLE.md`](embed/PACKAGE_LIFECYCLE.md)
- [`embed/SNAKE_PILOT.md`](embed/SNAKE_PILOT.md)
- [`abi/IDL.md`](abi/IDL.md)
- [`abi/BYTECODE.md`](abi/BYTECODE.md)
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
