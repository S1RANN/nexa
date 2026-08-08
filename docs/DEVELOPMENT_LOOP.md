# Nexa development loop

Status: Nexa M4 COMPLETE; M4R1 Language Surface Reset COMPLETE

`NexaEngine` owns the package development loop. Applications opt in with
`DevelopmentConfig`; they do not create compiler workers or mutate a Realm
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

Nexa Language v2 modules derive their identity from their path. For example,
`src/ui/score_overlay.nexa` is `package::ui::score_overlay`; the source path is
the sole source of module identity. Dependencies and sibling modules use `use`
declarations:

```nexa
use package::ui::commands as commands;
use shared_math::score;

fn emit(value: string) {
    host::log(value);
}
```

Changing a file path or a `use` target is therefore an identity and dependency
graph change, not merely a text edit.

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

The scanner uses modification metadata only as a possible prefilter. It
resolves one immutable build input containing the manifest, lockfile, all
source modules, the full local library closure, the effective Contract v3
contract, and effective compiler options. The commit identity is a 256-bit
`BuildFingerprint`. A change must produce the same complete input for
`stable_scan_count` consecutive scans before compilation is queued.

The effective Contract is accumulated across resolved Host calls and shared
types in the root Package plus every linked local Library. Required and
root-implemented Nexa entrypoints are included; unrelated Optional
declarations are excluded. Its canonical 32-byte fingerprint is embedded as
one field of `BuildFingerprint`, so traversal order and unused Contract
declarations cannot create false invalidation while a newly used dependency
Host call cannot escape freshness accounting.

One bounded worker parses Nexa Language v2, performs analysis and Typed IR
lowering, emits Bytecode v9, verifies the artifact, validates required
entrypoints, and constructs a candidate. It cannot access a Realm, call Host
code, process releases, or change package state. Queue capacity applies
backpressure: the Engine retains the job in `AwaitingQueue` and retries on a
later `tick`.

Each package has at most one in-flight generation and one newer pending
generation. Replacing pending work emits `SupersededBeforeCompile` without
changing its FIFO position. An older in-flight result emits
`SupersededAfterCompile`. A full result queue blocks the worker until
`NexaEngine::tick()` drains space; completed results are never evicted.

Every generation has exactly one terminal outcome. Disable, source removal,
and shutdown explicitly cancel pre-queue, pending, in-flight, and ready work.
A fingerprint observed before it is stable is held in an explicit
`unqueued_generation` ledger entry. A newer fingerprint or a return to active
content terminates that entry as `SupersededBeforeCompile`. Source removal
keeps the active runtime running and permits a new generation after the source
reappears. Only the newest verified candidate can enter Restart Reload.

Every stage carries one `CandidateIdentity`:

```text
Package ID + Generation + Build Fingerprint
```

Observed, stable, queued, in-flight, terminal, active, and fail-closed desired
states compare that complete identity. Desired identity becomes absent when
source, dependency, lock, contract, or compiler-option resolution fails.
Backpressure does not advance queued or terminal identity, so a stable version
cannot be forgotten before compilation.

Changing or reverting source triggers unified supersession across
`unqueued_generation`, `AwaitingQueue`, worker pending and in-flight jobs,
queued results, and retained ready candidates. Stale work receives exactly one
terminal outcome and cannot become the active runtime. Disable, removal, and
shutdown cancellation take precedence over supersession.

Queued and in-flight bookkeeping compares the complete candidate identity.
Reusing identical source content in a later generation cannot let an older
worker event or terminal clear the newer candidate identity.

`NexaEngine::tick()` refreshes the source immediately before runtime mutation.
Commit requires both the latest candidate generation and equality between the
candidate build fingerprint and the newly resolved desired build input. This
check is independent of ordinary scan cadence and rejects stale initial
enable, manual reload, automatic results, and retained CLI candidates after
add/delete/rename/ABA, dependency, lockfile, manifest, compiler-option, or Contract
contract changes. A refresh failure closes the gate.

`EngineInspection` reports cumulative `created_generations`,
`terminal_generations`, `duplicate_terminals`, and
`generations_without_terminal` values. A healthy completed run has equal
created and terminal counts, zero duplicate terminals, and zero generations
without a terminal outcome.

## Safety and Last Known Good

The active runtime, registered package contribution, and `LastKnownGood` are
unchanged when discovery, syntax, analysis, compilation, verification,
entrypoint validation, migration, or pre-commit reload fails. Successful
commit records the artifact, epoch, source-set/public-API/state-schema/build
fingerprints, dependency closure, effective contract fingerprint, and
generation in memory.

An activation failure is post-commit and faults the package. A later valid
source generation may construct a fresh runtime and restore it to `Enabled`.

A changed Contract Host surface cannot be committed as script-only reload.
`nexa dev` emits `HostRebuildRequired`, keeps Last Known Good, and instructs
the developer to rebuild the Rust Host bindings.

Shutdown stops admission, terminates every queued and in-flight generation,
drains terminal outcomes, signals and joins the worker, closes packages,
drains releases, and closes `RuntimeHost`.

## Headless development

The repository `nexa.dev.toml` uses schema 2. Each source declares its ID,
root, trust, allowed activation modes, capability ceiling, entitlement rule,
package count, and runtime limits. Package policy is selected from the source
root; it is never inferred from a package manifest.

The project-level Required surface uses the v2 key and the exact `snake_case`
names declared in the Contract's `nexa {}` block:

```toml
schema = 2
contract = "app_api.contract.nexa"
required_entrypoints = ["on_event"]
```

The list is an exact subset. An empty list makes every Contract entrypoint
Optional for this project; omitting the key selects the complete `nexa {}`
surface as Required. Duplicate, unknown, or wrong-case names are rejected.
The removed `required_exports` key is not an alias and is rejected as legacy
configuration. This TOML rename does not change the typed Rust builder API:
embedded Hosts continue to opt into a Required marker with
`require_export::<E>()`.

Validate the full project once or watch continuously:

```sh
cargo run -p nexa-cli -- check --project nexa.dev.toml
cargo run -p nexa-cli -- dev --project nexa.dev.toml
```

`nexa dev` is a headless compiler watcher. It never owns an application
runtime; reload of an embedded runtime remains the responsibility of the
Host's `NexaEngine::tick()`.

Single-package checks have three explicit validation levels:

```sh
cargo run -p nexa-cli -- check path/to/package --manifest-only
cargo run -p nexa-cli -- check path/to/package --contract app_api.contract.nexa
cargo run -p nexa-cli -- check path/to/package \
  --contract app_api.contract.nexa --policy source-policy.toml
```

Structured output reports `manifest-only`, `contract`, or `full-policy`.
Project checks always report `full-policy`.

Inspect or generate one Contract directly with the Contract command group:

```sh
cargo run -p nexa-cli -- contract check app_api.contract.nexa
cargo run -p nexa-cli -- contract generate app_api.contract.nexa
```

The resolver verifies existence, suffix, project-root containment, symbolic
link and `..` traversal, and the single-current-Contract rule before parsing.
JSON and NDJSON expose `contractPath`, `contractSyntaxVersion`, and
`contractDiagnostic`; there is no dual-write compatibility surface.

For direct execution use `nexa run`, documented in
[STANDALONE.md](STANDALONE.md). For an isolated interactive compiler/runtime
session use naked `nexa` or `nexa repl`, documented in [REPL.md](REPL.md).

The immutable M1, M2, M3, M3R1, M3R2, M3R3, and M4 completion tags remain
unchanged. M4 is recorded by `language-scale-m4-complete`; the M4R1 completion
authority is the annotated `language-scale-m4-complete-r1` tag.
