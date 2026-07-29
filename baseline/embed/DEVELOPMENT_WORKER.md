# Development Worker

Status: M3R1 COMPLETE

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

Each Package has at most one in-flight Generation and one newer pending
Generation. Replacing a pending Generation preserves that Package's stable
FIFO position and terminates the replaced Generation as
`SupersededBeforeCompile`. A full cross-Package queue returns backpressure to
the Engine, which retains and retries the Job.

Completed terminals use a bounded Result queue. When it is full the Worker
waits for Result space or shutdown; it never removes an older Result. Worker
start events and terminal results are drained by `NexaEngine::tick()`.

## Hash states

- `observed_hash`: newest content observed by scanning.
- `stable_hash`: content that passed stable-write detection.
- `queued_hash`: content accepted by the Worker.
- `in_flight_hash`: content currently compiling.
- `terminal_hash`: content with an observed terminal.
- `active_hash`: content currently running in the Runtime.

Only `terminal_hash` or `active_hash` suppresses re-queuing identical content.
A backpressured Job does not update `queued_hash` or `terminal_hash`.

Shutdown stops admission, terminates pending work, accounts for in-flight work,
drains terminals, joins the Worker, closes Package runtimes, drains releases,
and closes `RuntimeHost`.
