# Task Machine Specification 1.0

## Core states

```text
Created → Ready → Running
Running ⇄ FuelYielded
Running ⇄ Waiting
Running/Waiting/FuelYielded → ReloadPauseRequested → ReloadPaused
Running/Waiting/FuelYielded → CancelRequested → Cancelling → Cancelled
Running → Completed
Running/Cancelling → Trapped
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
