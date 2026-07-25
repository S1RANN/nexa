# Task Machine Specification 1.0

## Core states

```text
Created → Ready → Running
Running ⇄ FuelYielded
Running ⇄ ExplicitYielded
Running ⇄ Waiting
Running/Waiting/FuelYielded/ExplicitYielded → ReloadPauseRequested → ReloadPaused
Running/Waiting/FuelYielded/ExplicitYielded → CancelRequested → Cancelling → Cleanup → Cancelled
Running → Completed
Running/Cancelling/Cleanup → Trapped
```

## Admission invariants

- Before the first instruction, a task has a valid owner scope and captured module epoch.
- Before the first instruction, the runtime reserves task identity, initial frame storage, root
  metadata, and scheduler token.
- Promotion performs no allocation and cannot fail.
- Operations that can enlarge the continuation check and reserve quota before observable effects.

## Terminal invariant

A terminal task owns no VM resource and no host request capable of delivering a result to it.
Detached physical host operations belong to the host-resource domain.

## Reload commit cancellation

`ReloadCommitCancel` does not run user script `defer`. It releases only VM-managed resources and
registered host-resource tokens. Ordinary cancellation may run non-suspending user `defer`.

Realm Model v4 independently explores Fuel and Explicit Yield resume paths, waiting-request
ownership, reload pause/rollback, ordinary bounded cleanup, reload-commit cancellation, and
terminal scheduler-token cleanup.
