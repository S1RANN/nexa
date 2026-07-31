# Nexa

Nexa is a typed, Rust-hosted internal gameplay language with generated Host
bindings, bounded Tasks, explicit Host resources, typed state, and Restart
Reload.

## Status

```text
Gate 1 v2.9 old MVR = STOP
Nexa Internal Language Pivot = ACTIVE
Nexa Internal Pivot M1 = COMPLETE
Nexa M2 Embedding & Snake Pilot = COMPLETE
Nexa M3 Developer Loop & Diagnostics = COMPLETE
NexaEngine API = COMPLETE
Automatic Candidate Compilation = COMPLETE
Candidate Generation Terminal Accounting = COMPLETE
Candidate Freshness Commit Guard = COMPLETE
Last Known Good Reload = COMPLETE
Unified Diagnostics = COMPLETE
Source-level Runtime Stack Traces = COMPLETE
Package-aware CLI = COMPLETE
Editor Diagnostics = COMPLETE
nexa-embed v1 = COMPLETE
Typed Script Export = COMPLETE
Snake Core = COMPLETE
Built-in Package Pilot = COMPLETE
Official DLC Pilot = COMPLETE
Trusted Local Mod Pilot = COMPLETE
Repository Slimming = COMPLETE
Rust Host Binding v1 = COMPLETE
Task Runtime Stabilization = COMPLETE
Restart Reload v1 = COMPLETE
Combat Dogfood Loop = COMPLETE
Nexa M4 Language Scale Foundation = COMPLETE
Multi-file Source Modules = COMPLETE
Static Local Libraries = COMPLETE
Incremental Analysis = COMPLETE
Package Tests = COMPLETE
Seamless advanced Reload = REMOVED
```

The STOP decision ends the former general-product route. It does not end Nexa
as an internal language. The immutable experiment history is available at the
annotated tag `gate1-v2.9-stop`; the active branch keeps only the compact
[history index](docs/history/GATE1_V2_9_STOP.md).

## Product path

```text
Nexa source
→ lossless syntax and shared package analysis
→ Source Module and local Library graph
→ deterministic Typed IR
→ bytecode compiler and verifier
→ RealmRuntime
→ spawn_task / poll_task
→ generated Rust Host binding
→ typed @state
→ Restart Reload
```

M4 keeps Runtime isolation simple while scaling the language frontend. A
schema 2 Application and its lockfile-pinned local Library closure compile to
one deterministic Package Artifact, one Realm, and one Epoch. Imports always
bind namespaces, Host access is explicit (`import host as app;`), and
module-private, `pub(package)`, and `pub` visibility are distinct.

M4 completion is enforced by the source, semantics, incremental, tooling, and
scale-stress gates in `cargo xtask check`. Publication is finalized from the
annotated `language-scale-m4-complete` tag, with the clean-HEAD report written
to `target/nexa-artifacts/m4-finalize/final-report.json`.

See [Source Modules](docs/MODULES.md),
[Local Libraries](docs/LOCAL_LIBRARIES.md),
[Incremental Analysis](docs/INCREMENTAL_ANALYSIS.md),
[M4 Language Additions](docs/M4_LANGUAGE.md),
[Standard Library](docs/STANDARD_LIBRARY.md),
[Package Tests](docs/PACKAGE_TESTS.md), and the
[schema 2 migration guide](docs/MIGRATING_TO_M4.md).

`examples/hello-runtime` is the minimal high-level onboarding example. It uses
`nexa-embed` to discover one in-memory package, enable it, call a generated
typed export, and print `hello, world`; it does not manage Realm, Scope, Task,
or release queues. `examples/combat-runtime` remains the low-level consistency
example. Its
`combat_api.nidl` is the only Host API source and generates the Rust Trait,
Dispatcher, codecs, stable function IDs, Exact Interface Hash, Nexa
declaration, and test Stub into `OUT_DIR`. The Host implements only that
Trait.

Restart Reload stops admission, cancels old Tasks, detaches old Requests,
migrates state on staging, commits the new root, then activates it. Migration
failure rolls back before commit; activation failure remains observable after
commit. Old continuations and intermediate completion queues are not supported.

The binding gate compiles 20 textual `.nidl` mutations against the handwritten
`BusinessHostV1`, applies explicit business-code patches where required, rejects
old bytecode before interpretation, and executes the changed `heartbeat`
contract through `GeneratedHostRegistry<PatchedBusinessHost>`. External callers
cannot create Requests or manually place Tasks into Waiting: those transitions
only come from generated Host bindings and `TaskPoll::Waiting`. Real
`RealmRuntime` differential/fuzz coverage makes invalid events call public
Runtime APIs, while Combat verifies Request, Token, and Snapshot release
exactly once.

## High-level embedding

```rust
let source = MemorySource::new(SourceId::new("app")?, app_policy)
    .package(package_manifest, package_source);
let mut engine = NexaEngine::builder(generated::contract())
    .host_factory(|context| {
        generated::registry(AppHost::new(context))
    })
    .package_source(source)
    .storage_dir("user-data/extensions")
    .require_export::<generated::Main>()
    .development(DevelopmentConfig::default())
    .build()?;
engine.discover()?;
engine.enable_defaults()?;
let report = engine.tick()?;
engine.shutdown()?;
```

Package sources, policies, lifecycle, and diagnostics are documented in
[Embedding](docs/EMBEDDING.md).

## Run

The repository pins Rust `1.97.1` in `rust-toolchain.toml`.

```sh
cargo run -p nexa-cli -- compile examples/add.nexa
cargo run -p nexa-cli -- check --project nexa.dev.toml
cargo run -p nexa-cli -- dev --project nexa.dev.toml
cargo run -p hello-runtime
cargo run -p combat-runtime
cargo run -p snake-game
cargo xtask check
cargo xtask finalize-m1
cargo xtask finalize-m3-r1
cargo xtask finalize-m3-r2
cargo xtask finalize-m3-r3
```

Focused commands are documented in [Testing](docs/TESTING.md). Generated
outputs go to `target/nexa-artifacts/` under the
[Artifact Policy](docs/ARTIFACT_POLICY.md).

The normative entry point is [Baseline Index](baseline/BASELINE_INDEX.md), and
the current direction is [Roadmap](ROADMAP.md).
