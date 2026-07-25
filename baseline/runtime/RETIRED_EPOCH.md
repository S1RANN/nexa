# Retired Epoch Registry Specification 1.0

Every published old `ModuleEpochRoot` enters the Realm-owned Retired Epoch Registry:

```text
Retired → Draining → Drained
```

The registry records module handle, epoch, task/request/token/snapshot counts, GC-root count,
Realm release backlog, and pending completion reservations. An epoch may become `Drained` only
when:

```text
tasks = requests = tokens = snapshots = 0
pending Realm releases = 0
pending or queued completions = 0
```

Task, request, token, snapshot, release, and completion ownership is maintained incrementally in
Epoch-keyed reverse indexes at lifecycle transitions. The per-tick drain check reads those indexes;
it must not discover ownership by scanning the corresponding slot pools. Realm release ownership
uses a transfer generation, so splicing all domain lists into `RuntimeHost` also clears the
Realm-side Epoch view in O(1).

At that point module globals, state roots, and staging roots are cleared and the module slot is
released. Release records already transferred to `RuntimeHost` may still await their required
host-domain drain. An outstanding detached completion ticket keeps the epoch Retired; consuming
that ticket as a late completion allows deterministic final drain without touching a reused slot.
