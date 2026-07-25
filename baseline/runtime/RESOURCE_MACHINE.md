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

`RuntimeHost::new(capacity)` owns a fixed `ReleaseNodePool`. Resource creation reserves one node.
Realm queues and host queues are per-domain intrusive lists over that shared pool:

```text
VM Thread | Render | Audio | IO | Custom(0)
```

Realm-to-host transfer splices the five list pairs in O(1), without a temporary `Vec`, per-record
`push_back`, allocation, or fallible enqueue. Capacity exhaustion is observable only while
creating a new resource. Host-side domain drains may allocate their returned collection.

Host resource state:

```text
Healthy → ReleaseBacklog → ResourceStalled
   ↑                             │
   └──────── explicit recover ───┘
```

Thresholds reject new external-resource creation but never reject release enqueue. Existing tasks
may run; resource acquisition returns `ResourceStalled` at the operation point.
