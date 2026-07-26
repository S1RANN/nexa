# Pilot Integration Guide

## RuntimeHost and Realm lifecycle

Create one `RuntimeHost` with a bounded completion/release capacity, register the generated IDL
host implementation, then create a hosted `RealmRuntime`. Load only verified modules whose exact
IDL hash matches the host. Retain `TaskHandle`; poll or tick tasks through Realm. During shutdown,
drop realms, drain release records, call `begin_close`, and finish only after reservations reach
zero.

## IDL workflow

Edit the IDL first, run `nexa idl check`, regenerate Rust bindings, rebuild the host, then rebuild
the module. Exact hash changes are deployment boundaries; MVR has no compatibility adapter.

## Task and request workflow

Call Realm functions with a scope, priority, fuel slice, cumulative budget, and task limits.
First-slice completion is the fast path. A suspended task keeps its Realm-owned continuation.
Async host functions create a bounded request; complete, fail, cancel, or abandon its ticket
exactly once. Never retain borrowed argument views past the host call.

## Resources and snapshots

Resource tokens transfer release responsibility through `RuntimeHost`; configure release capacity
for worst-case teardown. Typed snapshots are immutable copies bound to content and schema IDs.
Neither is a general shared-memory lease.

## Stateful reload

Compile and verify the candidate, back up the state fixture, run `migrate-check`, prepare, quiesce,
stage, and commit. Before commit, rollback restores the old root and replays buffered completions.
After commit, activation failure leaves the candidate published as `ActivationFaulted`; it does not
restore the old root.

## Capacity planning

Set explicit maxima for modules, tasks, scopes, continuations, scheduler tokens, host requests,
completion reservations, host resources, snapshots, release reservations, heap/state objects,
reload buffers, retired epochs, trace records, and migration work. Start from observed Pilot peak
plus a documented safety margin; capacity failure is a normal explicit error.

## Limits

The Pilot is single-module for stateful reload. It has no stable compatible ABI, reload group,
cross-module StateHandle, strict deterministic scheduler, hostile-bytecode sandbox, AOT/JIT,
package manager, or seamless online patching. See `baseline/mvr/MVR_NON_GOALS.md`.
