# Resource Machine Specification 1.0

Any resource that will require a host release action reserves its release record before becoming
script-visible. This applies to host requests, host-resource tokens, immutable snapshots, and all
future releasable resources.

```text
Reserve resource slot
→ reserve release record
→ host acquire
→ publish resource
```

Release enqueue is constant-time, allocation-free, and cannot fail. The release queue belongs to
the runtime host-resource domain and outlives realms.

Host resource state:

```text
Healthy → ReleaseBacklog → ResourceStalled
   ↑                             │
   └──────── explicit recover ───┘
```

Thresholds reject new external-resource creation but never reject release enqueue. Existing tasks
may run; resource acquisition returns `ResourceStalled` at the operation point.
