# Milestone 3.2 — Resource & Transaction Closure Foundation

Status: the first five ordered implementation items are complete.

## Completion terminal protocol

Request creation returns `PendingHostRequest { request, ticket }`. The non-cloneable ticket owns
one fixed completion reservation and supports Success, Error, Cancelled, and Abandoned. Ticket
Drop submits Abandoned, and repeated termination returns `AlreadyCompleted`. Completion events
carry a monotonic terminal sequence and fixed realm/module/epoch/request identity.

Runtime behavior is explicit:

- Success type-checks, writes the destination register, and resumes;
- Error traps with a typed numeric host error payload and optional owned message;
- Cancelled terminates the task with `HostCancelled`;
- Abandoned traps deterministically.

## Zero-allocation release transfer

`RuntimeHost` owns a preallocated `ReleaseNodePool`. Realm and host release queues are intrusive
lists partitioned by VM, Render, Audio, IO, and Custom domain. Resource creation reserves the node;
terminal cleanup only fills it. Realm flush and Drop splice the lists into RuntimeHost without a
temporary collection or per-record enqueue.

The global allocator observer measures `realm_drop_transfer = 0` across three repetitions while
dropping a Realm that owns both a pending request and a Render token.

## Retired Epoch Registry

Published old roots enter a bounded registry with task, request, token, snapshot, GC-root,
release-backlog, and completion counts. A detached ticket prevents early module-slot reuse.
After its late terminal event is consumed and Realm releases are transferred, the registry clears
old roots, releases the module slot, and records `Drained`.

All runtime-owned counts come from lifecycle-maintained Epoch reverse indexes. Retired drain does
not scan the Task, Request, Token, Snapshot, release-node, or completion-slot pools per frame.
The three-Epoch runtime scenario keeps an Epoch 1 late completion and an Epoch 2 release backlog
while Epoch 3 is active, then drains both retired roots without reusing either slot early.

## Realm Model v3

The bounded object model contains two module epochs, two scope slots, two task slots, two request
slots, one token, one snapshot, reload state, per-request completion reservations, Realm releases,
host releases, and late-result counts. Canonical first-free/first-waiting slot selection removes
independent object permutations. At depth 14 it exhausts 929 distinct worlds below the 4,096-world
bound, including Success/Error/Cancel/Abandon, precommit rollback, activation success/failure,
late completion, retired drain, and host drain.

Every newly discovered world stores its shortest path. The exhaustive differential test creates a
fresh `RealmRuntimeAdapter` for every path and compares task state, request terminal state, active
root/module lifecycle, completion reservations, release counts, retired state, and late-result
count after every event.

## Scope boundary

This closes the ordered foundation batch:

```text
HostCompletionTicket
Complete/Fail/Cancelled/Abandoned
ReleaseNodePool and zero-allocation transfer
RetiredEpoch Registry
Realm Model v3 and explorer-driven differential replay
```

Strict Explicit Migration and language-level `Result` remain the next Milestone 3.2 tranche.
