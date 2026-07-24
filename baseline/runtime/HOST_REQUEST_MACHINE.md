# Host Request Machine Specification 1.0

```text
Created → Submitted → InFlight → Completed → Released
                         ├──────→ Discarded → Released
                         └──────→ CancelRequested → Detached → Released
```

Worker threads only enqueue completions tagged with realm, module, epoch, and request identity.
Only the VM thread may resolve tasks or epoch state. A terminal task has no request that can still
deliver to it; detached physical work is owned by the host-resource domain.
