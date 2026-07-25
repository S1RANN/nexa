# Milestone 3.3A — Correctness and Capacity Closure

Status: immediate six-item implementation complete.

## Register planning

The compiler now calculates a `RegisterPlan` per function from the actual local register high-water
mark and peak expression, call argument, match, and migration windows. Bind expressions evaluate in
the dedicated temporary window and move their final value into the local slot, so nested evaluation
cannot overwrite adjacent locals. Nested eight-argument calls compile, verify, and execute, while a
simple identity function uses two registers instead of receiving eight unconditional spare slots.

The verifier retains an independent checked range calculation and rejects an eight-argument call
whose final argument would cross the declared register count.

Coverage includes zero through eight HostCall arguments, an executable nested eight-argument
Script Call, and eight-argument calls inside `match`, `?`, and `defer`.

## Recursive isolated capability admission

Isolated admission now walks a cycle-safe module type graph. Function and export signatures, enum
payloads, and state-schema fields recursively resolve through named enum and state types. Direct or
wrapped `HostRequest`, `ResourceToken`, `Snapshot`, and `Buffer` types require Hosted mode.

## Four release domains

`RELEASE_DOMAIN_COUNT` is the single compile-time source for Realm and RuntimeHost release-list
arrays. `RuntimeHostDomain::bucket` maps exactly VM thread, render, audio, and IO; the unreachable
fifth Realm bucket has been removed.

## Migration limits

`RealmConfig::migration_limits` exposes:

```text
max_objects
max_fields
max_forwarding_entries
max_state_bytes
max_gc_roots
max_fuel
max_call_depth
```

Migration staging clones and validates the projected state before publishing each mutation.
Object, field, forwarding, byte, and root violations leave staging unchanged. Fuel and call-depth
limits are enforced by the restricted interpreter. All failures occur before root publication, so
the old module remains rollback-capable.

## Reload completion buffer

Reload no longer leaves completions sitting in the global queue. While a transaction is active,
accepted deliveries move into a preallocated `ReloadCompletionBuffer`. Rollback restores task and
scheduler checkpoints and then replays the buffered deliveries. Commit consumes the buffer as the
old tasks are cancelled and records those results as terminal or late outcomes.

## Realm Model v4

The initial v4 bounded model covers all eleven task states:

```text
Ready, Running, FuelYielded, ExplicitYielded, Waiting, ReloadPaused,
Cancelling, Cleanup, Completed, Cancelled, Trapped
```

It explores fuel and explicit resume, request completion, reload pause/rollback,
ordinary-cancellation cleanup, reload-commit cancellation, completion, and trap. Its invariants
require continuations for yielded tasks, exactly one request for waiting tasks, no scheduler token
for terminal tasks, and no user defer execution after reload-commit cancellation.

## Deferred follow-up

RuntimeHost `Open → Closing → Closed`, allocation reductions, fixed-size HostTrap diagnostics,
three-epoch/GC-root v4 expansion, offline migration tooling, and SourceMap diagnostics remain in the
subsequent supplied execution order.

## Verification

The workspace format, check, strict Clippy, unit/integration/doc tests, combat runtime, benchmark
smoke, real allocator observer, baseline, machine specification, generated-code, and bounded-model
gates all pass. Realm v4 explores 37 bounded worlds without invariant failures or truncation.
