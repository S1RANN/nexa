# Nexa development loop

Status: M3R1 COMPLETE

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

The scanner uses modification metadata only as a possible prefilter. The
commit identity is a deterministic hash of the package manifest and entry
source. A change must produce the same content for `stable_scan_count`
consecutive scans before compilation is queued.

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
and shutdown explicitly cancel pending, in-flight, and ready work. Source
removal keeps the active Runtime running and permits a new Generation after
the source reappears. Only the newest verified Candidate can enter Restart
Reload.

Development identity is split into `observed_hash`, `stable_hash`,
`queued_hash`, `in_flight_hash`, `terminal_hash`, and `active_hash`.
Backpressure does not advance queued or terminal identity, so a stable version
cannot be forgotten before compilation.

## Safety and Last Known Good

The active Runtime, registered package contribution, and `LastKnownGood` are
unchanged when discovery, parse, type checking, compilation, verification,
export checking, migration, or pre-commit Reload fails. Successful commit
records the artifact, epoch, source hash, schema hash, Host interface hash, and
generation in memory.

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
