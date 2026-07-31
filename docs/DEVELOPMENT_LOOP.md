# Nexa development loop

Status: M3R3 COMPLETE; M4 COMPLETE

`NexaEngine` owns the package development loop. Applications opt in with
`DevelopmentConfig`; they do not create compiler threads or mutate a Realm
from callbacks.

```rust
let mut engine = NexaEngine::builder(generated::contract())
    .host_factory(host_factory)
    .package_source(source)
    .development(DevelopmentConfig::default())
    .build()?;

engine.discover()?;
engine.enable_defaults()?;

loop {
    let report = engine.tick()?;
    consume(report.development_events, report.diagnostics, report.reloads);
}
```

## Candidate state machine

```text
Idle
→ ChangeObserved
→ WaitingForStableWrite
→ CompileQueued
→ Compiling
→ CandidateReady
→ ReloadPending
→ Reloading
→ Reloaded
```

Failure states are `CompileFailed`, `VerifyFailed`, `MigrationFailed`,
`ActivationFaulted`, `HostRebuildRequired`, and `SourceMissing`.
`AwaitingQueue` is an explicit backpressure state. Every transition produces
a bounded `DevelopmentEvent`.

The scanner uses modification metadata only as a possible prefilter. M4
observes one immutable resolved build input containing the Manifest, Lockfile,
all Source Modules, the full local Library closure, and Host Contract. Its
commit identity is a 256-bit `BuildFingerprint`. A change must produce the same
complete input for `stable_scan_count` consecutive scans before compilation is
queued.

One bounded worker reads an immutable Candidate snapshot and performs parse,
type checking, bytecode generation, verification, required-export checks, and
artifact construction. It cannot access a Realm, call Host code, process
releases, or change package state. Queue capacity applies backpressure: the
Engine retains the Job in `AwaitingQueue` and retries on a later Tick.

Each Package has at most one in-flight Generation and one newer pending
Generation. Replacing pending work emits `SupersededBeforeCompile` without
changing its FIFO position. An older in-flight result emits
`SupersededAfterCompile`. A full Result queue blocks the Worker until
`NexaEngine::tick()` drains space; completed results are never evicted.

Every Generation has exactly one terminal outcome. Disable, source removal,
and shutdown explicitly cancel pre-queue, pending, in-flight, and ready work.
A hash observed before it is stable is held in an explicit
`unqueued_generation` ledger entry. A newer hash or a return to active content
terminates that entry as `SupersededBeforeCompile`. Source removal keeps the
active Runtime running and permits a new Generation after the source
reappears. Only the newest verified Candidate can enter Restart Reload.

Every stage carries one `CandidateIdentity`:

```text
Package ID + Generation + Build Fingerprint
```

Observed, stable, queued, in-flight, terminal, active, and fail-closed desired
states compare that complete identity. Desired identity becomes absent when
source, dependency, lock, or contract discovery fails. Backpressure does not
advance queued or terminal identity, so a stable version cannot be forgotten
before compilation.

Changing or reverting source triggers unified supersession across
`unqueued_generation`, `AwaitingQueue`, Worker pending and in-flight Jobs,
queued Results, and retained ready Candidates. Stale work receives exactly one
terminal outcome and cannot become the active Runtime. Disable, removal, and
shutdown cancellation still take precedence over supersession.

Queued and in-flight bookkeeping compares the complete Candidate identity.
Reusing identical source content in a later Generation cannot let an older
Worker event or terminal clear the newer Candidate identity.

`NexaEngine::tick()` refreshes the source immediately before Runtime mutation.
Commit requires both the latest Candidate Generation and equality between the
Candidate Build Fingerprint and the newly resolved desired build input. This
check is independent of ordinary scan cadence and therefore rejects stale
Initial Enable, Manual Reload, automatic Results, and CLI retained Candidates
after add/delete/rename/ABA, dependency, Lockfile, Manifest, or Host Contract
changes. A refresh failure closes the gate.

`EngineInspection` reports real cumulative `created_generations`,
`terminal_generations`, `duplicate_terminals`, and
`generations_without_terminal` values. The completion gate requires created
and terminal counts to match with zero duplicates and zero missing terminals.

## Safety and Last Known Good

The active Runtime, registered package contribution, and `LastKnownGood` are
unchanged when discovery, parse, type checking, compilation, verification,
export checking, migration, or pre-commit Reload fails. Successful commit
records the Artifact, Epoch, Source Set/Public API/State Schema/Build
Fingerprints, dependency closure, Host interface hash, and Generation in
memory.

An activation failure is post-commit and faults the package. A later valid
source generation is allowed to construct a fresh Runtime and restore the
package to `Enabled`.

A changed Host contract cannot be committed as script-only Reload. `nexa dev`
emits `HostRebuildRequired`, keeps the last successful Candidate, and instructs
the developer to rebuild the Rust Host binding.

Shutdown stops admission, terminates every queued and in-flight Generation,
drains terminals, signals and joins the worker, closes packages, drains
releases, and closes `RuntimeHost`.

## Headless development

The repository `nexa.dev.toml` uses schema 2. Each source declares its ID,
root, trust, allowed activation modes, capability ceiling, entitlement rule,
package count, and Runtime limits. Package policy is selected from the source
root; it is never inferred from a Package Manifest.

Validate the full project once or watch continuously:

```sh
cargo run -p nexa-cli -- check --project nexa.dev.toml
cargo run -p nexa-cli -- dev --project nexa.dev.toml
```

`nexa dev` is a headless compiler watcher. It never owns an application
Runtime; Runtime Reload remains the responsibility of the host's
`NexaEngine::tick()`.

Single-Package checks have three explicit validation levels:

```sh
cargo run -p nexa-cli -- check path/to/package --manifest-only
cargo run -p nexa-cli -- check path/to/package --contract app_api.nidl
cargo run -p nexa-cli -- check path/to/package \
  --contract app_api.nidl --policy source-policy.toml
```

Their structured output reports `manifest-only`, `contract`, or
`full-policy`. Project checks always report `full-policy`.

The immutable M3, M3R1, M3R2, and M3R3 completion tags remain unchanged. M4 is
complete at the annotated `language-scale-m4-complete` tag and does not rewrite
those historical commits.
