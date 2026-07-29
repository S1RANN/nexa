# Nexa Reload workflow

Status: M3 COMPLETE

The M3 workflow preserves Restart Reload semantics and adds a guarded Candidate
pipeline:

```text
stable source snapshot
→ parse
→ type check
→ compile
→ verify
→ required export check
→ Host contract check
→ CandidateReady
→ engine.tick() safe point
→ Restart Reload
```

Before commit, admission stops, old Tasks are cancelled, Requests are
detached, and migration runs on staging. Failure rolls back without replacing
the active package or Last Known Good. Commit publishes the new epoch and
artifact. Activation then runs; its failure is explicitly post-commit and
faults the package.

`ReloadReport` records package and generation identity, old/new epochs, source
hash, compile/verify/migration/activation durations, cancelled Tasks, detached
Requests, and one of:

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
replacement. A NIDL/interface-hash change requires rebuilding the Rust Host
binding and cannot be reported as an ordinary script compile error.

Old Tasks are never restored and completion replay is not part of M3.
