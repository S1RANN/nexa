# Reload Transaction Specification 1.0

## Prepare

Load and verify the candidate module, validate exact host IDL and state schema, validate migration
purity, and reserve staging capacity. The active system continues running.

## Quiesce

Stop new task admission for the module. Tasks enter `ReloadPaused` at safepoints. Completion
delivery is buffered. Tasks, frames, requests, and resources remain intact. Timeout restores
delivery and resumes old tasks.

## Stage

Snapshot registered stateful objects, build the new graph in a staging heap, validate state
handles, and compute a migration hash. Migration uses VM pure operations only and cannot call the
host. Failure discards staging and resumes the old system.

If the state schema is unchanged, an untouched migration performs an identity clone and preserves
the Stateful Domain and generations. If the schema changed, migration must explicitly account for
every old identity with `STATE_PRESERVE`, `STATE_REPLACE`, or `STATE_DELETE`, construct the staging
graph, and execute `STATE_FINISH`. An untouched changed-schema migration returns
`MigrationNoOutput`; incomplete or duplicate forwarding is rejected before publication.

Migration functions use typed source intrinsics: `old.get<T>`, `old.field<T>`, `new.create<T>`,
`new.set`, `preserve`, `replace`, `delete`, and `finish_migration`. These are available only to
`migration fn` and lower directly to the restricted migration instruction set.

## Commit

Publish one immutable `ModuleEpochRoot` on the VM thread. No worker thread may acquire or
dereference an epoch root. After publication:

- old completions become detached/discard-on-completion;
- old paused tasks receive `ReloadCommitCancel` and do not run user defer;
- registered host-resource tokens enqueue their preallocated release records;
- the new module enters `Activating`.

Activation failure produces `ActivationFaulted`; it does not roll back old behavior.
