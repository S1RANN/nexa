# Development Worker

Status: M3R3 COMPLETE

The development Worker compiles immutable Package Candidate snapshots outside
the Runtime thread. It is a bounded producer whose capacity creates
backpressure, never data loss.

## Generation contract

1. No accepted Job may be silently discarded.
2. No completed Result may be silently discarded.
3. Every Candidate Generation has exactly one terminal outcome.
4. Queue capacity may create backpressure; it may not erase state.
5. The Worker may not modify a Realm, Package lifecycle, or Last Known Good.
6. Only `NexaEngine::tick()` may commit a Candidate.
7. Disable, source removal, and shutdown terminate every affected Generation.
8. A stale Generation is `Superseded`; it can never commit.
9. Commit requires the latest Generation and an exact refreshed
   `desired_hash`.
10. Missing or unreadable source clears `desired_hash` and closes the commit
    gate.

Each Package has at most one in-flight Generation and one newer pending
Generation. Replacing a pending Generation preserves that Package's stable
FIFO position and terminates the replaced Generation as
`SupersededBeforeCompile`. A full cross-Package queue returns backpressure to
the Engine, which retains and retries the Job.

Observing a new content hash creates a Generation before stable-write
detection. The Engine records that pre-queue Generation explicitly. If another
hash replaces it, or the source returns to the active hash, it terminates as
`SupersededBeforeCompile`. Disable, source removal, and shutdown terminate it
with their corresponding cancellation outcome even though no `CompileJob`
exists yet.

Completed terminals use a bounded Result queue. When it is full the Worker
waits for Result space or shutdown; it never removes an older Result. Worker
start events and terminal results are drained by `NexaEngine::tick()`.

Freshness supersession is one pipeline-wide operation. It terminates stale
unqueued and awaiting-queue work before compilation, removes stale pending
Jobs, marks stale in-flight Jobs for supersession, rewrites stale queued Results
as superseded terminals, and discards stale ready Candidates. Cancellation for
disable, removal, or shutdown remains stronger than supersession.

## Hash states

- `observed_hash`: newest content observed by scanning.
- `desired_hash`: freshly discoverable content the Engine is allowed to
  commit, or `None` when source discovery fails.
- `stable_hash`: content that passed stable-write detection.
- `queued_hash`: content accepted by the Worker.
- `in_flight_hash`: content currently compiling.
- `terminal_hash`: content with an observed terminal.
- `active_hash`: content currently running in the Runtime.

Queued and in-flight hashes are paired with their exact Candidate Generation.
An older terminal with the same hash cannot clear a newer Generation's Worker
identity during an A → B → C → B sequence.

Only `terminal_hash` or `active_hash` suppresses re-queuing identical content.
A backpressured Job does not update `queued_hash` or `terminal_hash`.
`active_hash` is initialized when a Package is enabled, not only after its
first Reload.

Before Runtime mutation, `NexaEngine::tick()` reads the source again without
advancing the scan state. Candidate identity is current only when its
Generation is the latest and its hash equals the refreshed `desired_hash`.
This final check covers Results and manually retained ready Candidates that
outlive the scan which created them. A mismatch is terminal supersession; it
does not update the active Runtime, Last Known Good artifact, or
`terminal_hash`.

Inspection reports cumulative created, terminal, duplicate, and unterminated
Generation counts. Finalization reads those real Engine counts; it may not
derive them from whether Worker tests passed.

Shutdown stops admission, terminates pending work, accounts for in-flight work,
drains terminals, joins the Worker, closes Package runtimes, drains releases,
and closes `RuntimeHost`.

The immutable M3, M3R1, and M3R2 completion tags remain unchanged. M4 is
complete at `language-scale-m4-complete`; completed M4R1 Language v2 and NIDL
v2 do not change the Worker freshness contract.
