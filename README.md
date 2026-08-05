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
Nexa M4R1 Language Surface Reset = COMPLETE
Nexa M5 Deep Performance Optimization = COMPLETE
Performance Measurement Authority v1 = COMPLETE
Value Layout v1 = COMPLETE
ExecutableModule v1 = COMPLETE
Incremental GC v1 = COMPLETE
Runtime Fast Paths v1 = COMPLETE
M6 LLVM JIT = DEFER
Nexa Language v2 = COMPLETE
NIDL v2 = COMPLETE
Structured Codegen v2 = COMPLETE
Standalone Profile v1 = COMPLETE
REPL v1 = COMPLETE
Multiple Entrypoint Model = COMPLETE
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
one deterministic Package Artifact, one Realm, and one Epoch. M4R1 freezes the
breaking Language v2 surface: modules are derived from source paths, `use`
binds namespaces, Host access is explicit (`use host::app;`), and
module-private, `pub(package)`, and `pub` visibility remain distinct. There is
no source-level compatibility mode for the M4 syntax.

M4 completion is enforced by the source, semantics, incremental, tooling, and
scale-stress gates in `cargo xtask check`. Publication is finalized from the
annotated `language-scale-m4-complete` tag, with the clean-HEAD report written
to `target/nexa-artifacts/m4-finalize/final-report.json`.

M4R1 completion is enforced by `cargo xtask finalize-m4-r1`; the clean tagged
report is written to
`target/nexa-artifacts/m4r1-finalize/final-report.json`, and the publication
authority is the annotated `language-scale-m4-complete-r1` tag.

M5 completion is enforced by `cargo xtask finalize-m5` from the clean,
published `performance-m5-complete` annotated tag. The finalizer reruns the
workspace and historical gates, live same-machine baseline comparison,
profiler overhead, product corpus, V8 parity, and structural zero-allocation
checks. The committed public attestation is
`baseline/performance/M5_RELEASE_SUMMARY.json`; its BLAKE3 digest and the
regenerated exact results are written to
`target/nexa-artifacts/m5-finalize/final-report.json`. The data-backed M6 LLVM
JIT decision is **DEFER**: M5 met every frozen performance target, but neither
per-workload CPU sampling nor a bounded LLVM compilation-cost prototype proves
the remaining JIT GO conditions.

See [Source Modules](docs/MODULES.md),
[Local Libraries](docs/LOCAL_LIBRARIES.md),
[Incremental Analysis](docs/INCREMENTAL_ANALYSIS.md),
[M4 Language Additions](docs/M4_LANGUAGE.md),
[Standard Library](docs/STANDARD_LIBRARY.md),
[Package Tests](docs/PACKAGE_TESTS.md),
[NIDL v2](docs/NIDL.md),
[Standalone](docs/STANDALONE.md),
[REPL](docs/REPL.md), and the
[schema 2 migration guide](docs/MIGRATING_TO_M4.md).

`examples/hello-runtime` is the minimal high-level onboarding example. It uses
`nexa-embed` to discover one in-memory package, enable it, call a generated
typed entrypoint, and print `hello, world`; it does not manage Realm, Scope,
Task, or release queues. `examples/combat-runtime` remains the low-level
consistency example. Its
`combat_api.nidl` is the only Host API source and generates the Rust Trait,
Dispatcher, codecs, stable function IDs, full ABI Descriptor v2 and Contract
fingerprint, typed Nexa entrypoint markers, and test Stub into `OUT_DIR`. A
Package build derives its smaller effective Contract fingerprint from the
surface actually used by its linked closure. The Host implements only the
generated Trait.

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
cargo run -p nexa-cli -- run examples/hello-runtime/hello.nexa -- Alice
cargo run -p nexa-cli -- repl
cargo run -p hello-runtime
cargo run -p combat-runtime
cargo run -p snake-game
cargo xtask check
cargo xtask finalize-m1
cargo xtask finalize-m3-r1
cargo xtask finalize-m3-r2
cargo xtask finalize-m3-r3
cargo xtask finalize-m4-r1
```

Focused commands are documented in [Testing](docs/TESTING.md). Generated
outputs go to `target/nexa-artifacts/` under the
[Artifact Policy](docs/ARTIFACT_POLICY.md).

The normative entry point is [Baseline Index](baseline/BASELINE_INDEX.md), and
the current direction is [Roadmap](ROADMAP.md).
