# Pilot Operations

## Version and upgrade policy

Pin the Nexa Git SHA, Rust toolchain, IDL hash, feature flags, generated bindings, module bytecode,
capacity file, and state fixture. An upgrade is a coordinated exact-build release.

1. Stop new admissions and collect diagnostics.
2. Back up the typed state fixture and current bytecode.
3. Build and verify the candidate using the pinned toolchain.
4. Run migration dry-run with `nexa migrate-check --dump-state --diff-state`.
5. Confirm capacity headroom and rollback eligibility.
6. Prepare, quiesce, stage, inspect, then commit.
7. If failure precedes commit, rollback and replay buffered completions.
8. If activation fails after commit, stop admissions and deploy a new candidate; never claim
   rollback to the old root.

## Monitoring and artifacts

Monitor every Runtime resource-ledger field, completion-buffer depth, release-queue depth, task
terminal reasons, retired epochs, migration consumption, and trace overflow. On fault collect the
exact module bytes, IDL and hash, state fixture, capacity configuration, diagnostic JSON, trace,
runtime inspection snapshot, host log, implementation SHA/tree, and reproduction steps.

To stop using Nexa, stop admission, cancel/finish scopes by policy, drain requests and releases,
persist the last fixture and diagnostics, close `RuntimeHost`, and revert the engine to its
pre-Pilot integration. Do not discard evidence needed for state recovery.
