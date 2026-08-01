# Nexa Reload workflow

Status: M3R1 COMPLETE

The M3 workflow preserves Restart Reload semantics and adds a guarded Candidate
pipeline:

```text
stable source snapshot
→ parse
→ type check
→ compile
→ verify
→ required entrypoint check
→ Host contract check
→ CandidateReady
→ engine.tick() safe point
→ Restart Reload
```

Before commit, admission stops, old Tasks are cancelled, Requests are
detached, and migration runs on staging. Failure rolls back without replacing
the active package or Last Known Good. Commit publishes the new epoch and
artifact provisionally while retaining the old epoch until activation
finishes. Activation failure is reported as `ActivationFaulted`, restores the
old root, heap, and Last Known Good, and leaves the package enabled; a
successful activation retires the old epoch.

`ReloadReport` records package and generation identity, old/new epochs, source
hash, cancelled Tasks, detached Requests, and independently measured timing:

```text
change-to-stable
queue
compile
verify
ready-to-commit
quiesce
migration
commit
activation
total change-to-visible
```

`reload_duration` is the sum of quiesce, migration, commit, and activation; no
phase receives an invented whole-operation duration. Compile and Verify use
separate calls and clocks. Execution inspection also records instruction count
and charged Fuel independently.

The report outcome is one of:

```text
Committed
CompileFailed
VerifyFailed
RolledBackBeforeCommit
ActivationFaulted
Superseded
HostRebuildRequired
```

Manifest priority, handler-fuel, and activation-preference changes are applied
with a complete Candidate at a safe point. Policy, capability, and entitlement
changes are reevaluated. A Package ID change is rediscovery, not an in-place
replacement. A NIDL v2 Contract Descriptor or effective Contract fingerprint
change requires rebuilding the Rust Host binding and cannot be reported as an
ordinary script compile error.

Old Tasks are never restored and completion replay is not part of M3.
