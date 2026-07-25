# Milestone 3.3 — Contract Verification and Developer Semantics

Status: first-six contract closure complete.

## Realm and host lifecycle

`RealmRuntime::isolated` and `RealmRuntime::hosted` remain the only public construction paths.
Isolated admission rejects host imports and host-owned types, and isolated resource APIs return
`HostCapabilitiesUnavailable`.

Hosted realms register with `RuntimeHost`. `RuntimeHost::close` rejects live realms, completion
reservations (including queued completions), and pending release records. A closed host rejects new
realm registration; debug builds diagnose a final `RuntimeHost` drop that was not explicitly
closed. The combat integration drains release domains before closing.

## Result and Option source semantics

The source language now supports:

```nexa
Option<T>
Result<T, E>
None
Some(value)
Ok(value)
Err(error)
match value { ... }
operation()?
```

`match` requires every variant exactly once and consistent arm result types. `?` requires a Result
operand and an enclosing Result with the exact same error payload type. Lowering uses the canonical
builtin enum metadata and enum bytecode instructions.

Async IDL imports are visible to Nexa as their declared Result. The combat source executes both
successful and failing typed completions through `?`, and consumes another async Result with
exhaustive `match`; expected host failures do not become unconditional traps.

## Source-authored explicit migration

Migration source intrinsics cover immutable old lookup/field reads, staging creation/writes,
Preserve, Replace, Delete, and Finish. The combat v1-to-v2 migration is compiled directly from
Nexa source. It preserves an unchanged state identity, replaces a versioned EnemyBrain, deletes a
retired identity, adds `aggression`, removes `legacy_target`, and validates stale/preserved handles.
The prior Rust code that replaced the migration function bytecode has been removed.

## Host argument allocation contract

Runtime HostCall argument conversion uses a fixed eight-element inline `HostArgs` buffer rather
than an intermediate `Vec<HostValue>`.

The global allocation observer reports three reproducible runs on macOS arm64:

```text
Immediate HostCall       0 allocations
Async admission          8 allocations
Success Result writeback 2 allocations
Error Result writeback   2 allocations
Realm drop transfer      0 allocations
```

Only Immediate HostCall and the previously declared allocation-free runtime paths are zero-
allocation contracts. Async admission and Result writeback are measured facts for subsequent
optimization, not reported as zero.

## Remaining Milestone 3.3 work

Realm Model v4, migration limits/configuration, the offline migration CLI, structured diagnostics,
and the broader benchmark matrix remain later items in the supplied execution order.
