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

## Commit

Publish one immutable `ModuleEpochRoot` on the VM thread. No worker thread may acquire or
dereference an epoch root. After publication:

- old completions become detached/discard-on-completion;
- old paused tasks receive `ReloadCommitCancel` and do not run user defer;
- registered host-resource tokens enqueue their preallocated release records;
- the new module enters `Activating`.

Activation failure produces `ActivationFaulted`; it does not roll back old behavior.
