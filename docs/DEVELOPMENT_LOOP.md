# Nexa development loop

Status: M3 COMPLETE

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
`ActivationFaulted`, and `HostRebuildRequired`. Every transition produces a
bounded `DevelopmentEvent`.

The scanner uses modification metadata only as a possible prefilter. The
commit identity is a deterministic hash of the package manifest and entry
source. A change must produce the same content for `stable_scan_count`
consecutive scans before compilation is queued.

One bounded worker reads an immutable Candidate snapshot and performs parse,
type checking, bytecode generation, verification, required-export checks, and
artifact construction. It cannot access a Realm, call Host code, process
releases, or change package state.

Queued generations from the same package are superseded by newer generations.
When a result reaches `NexaEngine::tick()`, its generation is checked again.
Only the newest verified Candidate can enter Restart Reload.

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

Shutdown stops admission, clears queued jobs, signals and joins the worker,
closes packages, drains releases, and closes `RuntimeHost`.

## Headless development

The repository `nexa.dev.toml` describes the Snake contract, package roots,
and required export. Validate once or watch continuously:

```sh
cargo run -p nexa-cli -- check --project nexa.dev.toml
cargo run -p nexa-cli -- dev --project nexa.dev.toml
```

`nexa dev` is a headless compiler watcher. It never owns an application
Runtime; Runtime Reload remains the responsibility of the host's
`NexaEngine::tick()`.
