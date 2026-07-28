# Pilot Integration Guide

## RuntimeHost and Realm lifecycle

Create one `RuntimeHost` with a bounded completion/release capacity, register the generated IDL
host implementation, then create a hosted `RealmRuntime`. Load only verified modules whose exact
IDL hash matches the host. Retain `TaskHandle`; poll or tick tasks through Realm. During shutdown,
drop realms, drain release records, call `begin_close`, and finish only after reservations reach
zero.

## IDL workflow

Edit the `.nidl` file first, regenerate Rust bindings into `OUT_DIR`, rebuild the host, then rebuild
the module. Exact hash changes are deployment boundaries; there is no compatibility adapter.

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

Compile and verify the candidate, back up the state fixture, then call `restart_reload`. The runtime
cancels old tasks, detaches old requests, drains managed resources, migrates staging state, commits,
and invokes activation. Migration failure rolls back before commit. Activation failure leaves the
candidate published as `ActivationFaulted`.

## Capacity planning

Set explicit maxima for modules, tasks, scopes, continuations, scheduler tokens, host requests,
completion reservations, host resources, snapshots, release reservations, heap/state objects,
trace records, and migration work. Start from observed Pilot peak
plus a documented safety margin; capacity failure is a normal explicit error.

## Limits

The Pilot is single-module for stateful reload. It has no stable compatible ABI, reload group,
cross-module StateHandle, strict deterministic scheduler, hostile-bytecode sandbox, AOT/JIT,
package manager, or seamless online patching. See
`baseline/internal/INTERNAL_LANGUAGE_SCOPE.md`.
