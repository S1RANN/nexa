# Reload Transaction Specification 1.0

## Prepare

Load and verify the candidate module, validate exact host IDL and state schema, validate migration
purity, and reserve staging capacity. The active system continues running.

## Quiesce

Stop new task admission for the module. Tasks enter `ReloadPaused` at safepoints. Completion
delivery is drained from the global queue into a capacity-bounded, transaction-owned completion
buffer only when its immutable module/epoch identity matches the transaction's old root.
Completions for every other module or epoch continue through normal delivery. Tasks, frames,
requests, and resources remain intact. Rollback restores scheduler and task checkpoints before
replaying buffered deliveries in terminal-sequence order; commit explicitly accounts for and
discards old-epoch buffered deliveries while cancelling the old tasks. Activation failure occurs
after publication and follows the same discard rule.

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

`RealmConfig::migration_limits` is a hard arena contract for objects, fields, forwarding entries,
payload bytes, GC roots, fuel, and call depth. `MigrationContext` reserves all object, field,
forwarding, payload-byte, and root storage once during construction. From the first migration
opcode through `STATE_FINISH`, the sorted slot vectors do not grow and no system allocation is
permitted. Each mutation preflights its exact object, field, payload-byte, and root delta before
changing the arena. `max_state_bytes` means available staging payload bytes; fixed object, field,
and forwarding metadata is reported separately by `MigrationCapacityReport`.

Successful staging moves the arena's object, field, payload, and root vectors directly into the
candidate `StatefulRegistry`; it does not rebuild a map or copy the completed graph. A limit
failure occurs before root publication, performs no partial opcode mutation, and leaves the old
root available for rollback.

## Commit

Publish one immutable `ModuleEpochRoot` on the VM thread. No worker thread may acquire or
dereference an epoch root. After publication:

- old completions become detached/discard-on-completion;
- old paused tasks receive `ReloadCommitCancel` and do not run user defer;
- registered host-resource tokens enqueue their preallocated release records;
- the new module enters `Activating`.

Activation failure produces `ActivationFaulted`; it does not roll back old behavior.
