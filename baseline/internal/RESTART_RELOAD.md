# Restart Reload

Version: **1.0.0**

Restart Reload is the only supported reload protocol:

```text
stop new Task admission
→ cancel all old-module Tasks
→ detach all old Requests
→ drain VM-owned resources
→ snapshot @state
→ load and verify the candidate
→ run pure migration on staging state
→ commit the new root
→ invoke on_reload_start / activation
→ resume admission
```

`RealmRuntime::restart_reload` returns one of:

- `Committed`: migration, root commit, and activation succeeded;
- `RolledBackBeforeCommit`: migration failed; the old root and state remain;
  Tasks and Requests cancelled during quiesce do not return;
- `ActivationFaulted`: the new root committed, activation failed, and the Host
  may reload again or reset.

Old Requests become detached. Physical completion may arrive later, but its
module epoch is stale: the result is discarded and its Host resource is
released exactly once. It never enters an intermediate replay queue and never
resumes an old Task.

Nexa does not preserve old continuations, resume old Tasks, run old and new
business code concurrently, route work through released modules, or support
seamless multi-version reload.

Inspection reports the count of cancelled Tasks, detached Requests, and
discarded late completions. Released-module records are fixed-capacity,
read-only diagnostics and never participate in Task, Request, completion, or
resource routing.
