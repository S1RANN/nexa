# Package Lifecycle

Status: M2 COMPLETE

The only package states are `Discovered`, `Locked`, `Disabled`, `Enabling`,
`Enabled`, `Reloading`, `Disabling`, `Faulted`, and `Incompatible`. Every
transition is checked by `PackageLifecycle`.

Enable revalidates the candidate, compiles with the exact Host interface,
verifies bytecode, validates required export stable IDs and signatures, creates
an independent Host registry, Realm, Module, and root Scope, then publishes
`Enabled`. Temporary state is dropped on failure. A failed package cannot
invalidate another package.

Each enabled package owns exactly one Realm, one loaded Module, one root Scope,
and independent Task/Request/Token/Snapshot ledgers. Realms share one
process-level `RuntimeHost`.

Disable stops dispatch, cancels the root Scope, ticks cancellation and garbage
collection, drops the Realm, drains process releases, and publishes
`Disabled`. Fault follows the same isolation boundary.

Reload compiles and verifies a candidate before touching the active Realm, then
uses Restart Reload. Pre-commit compile, verification, or migration failure
keeps the old module enabled. A committed activation failure faults only that
package. Successful restart keeps compatible typed state and cancels old
Tasks. Old Tasks are never restored.

Development change scan compares manifest and entry hashes at a deterministic
tick interval. It uses no background thread or platform watcher.

Only user enable/disable selections persist across processes. Heap, Tasks, and
script state do not. Persistence uses a temporary file, file sync, atomic
rename, and directory sync.
