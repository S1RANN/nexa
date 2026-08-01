# Host Request Machine Specification 1.1

```text
Created → Submitted → InFlight → CompletionQueued
                         │              ├→ Completed → Released
                         │              ├→ Failed → Released
                         │              ├→ Cancelled → Released
                         │              └→ Abandoned → Released
                         └→ CancelRequested → Detached → Released
```

Every request owns exactly one `HostCompletionTicket`. The ticket carries realm, module, epoch,
request identity, and one pre-reserved completion slot. It is not cloneable and supports exactly
one of `complete`, `fail`, `cancelled`, or `abandon`; Drop submits `Abandoned`. A repeated terminal
operation is rejected without consuming another slot.

Worker threads only consume tickets. Only the VM thread resolves tasks or epoch state. Success
writes the destination and resumes the task. A declared Host error resumes with `Result::Err`;
an ABI, transport, or other undeclared failure traps. Host cancellation cancels the task, and
abandonment traps. Detached physical work retains the ticket but cannot deliver to the released
request generation.
