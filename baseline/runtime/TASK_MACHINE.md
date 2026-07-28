# Task Machine Specification 1.0

## Core states

```text
Created → Ready → Running
Running --YieldFuel--> FuelYielded --Resume--> Running
Running --YieldExplicit--> ExplicitYielded --ResumeExplicit--> Running
Running ⇄ Waiting
Running/Waiting/FuelYielded/ExplicitYielded → CancelRequested → Cancelling
Cancelling --BeginCleanup--> Cleanup --Clean--> Cancelled
Cancelling --Clean--> Cancelled
Running → Completed
Running/Cancelling/Cleanup → Trapped
```

`TaskExecution` mirrors non-terminal ownership with distinct `FuelYielded`,
`ExplicitYielded`, `Waiting`, `Cancelling`, and `Cleanup` variants.

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

Restart cancellation takes the direct `Cancelling --Clean--> Cancelled` path
and does not run user script `defer`. It releases only VM-managed resources and
registered Host resources.
Ordinary cancellation with user `defer` must enter `Cleanup`; cleanup success ends in `Cancelled`
and cleanup trap ends in `Trapped`.

The current Realm model independently explores Fuel and Explicit Yield resume
paths, waiting-request ownership, restart cancellation, pre-commit rollback,
late-result discard, ordinary bounded cleanup, and terminal scheduler-token
cleanup.
