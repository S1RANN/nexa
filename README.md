# Nexa

Nexa is a typed, Rust-hosted internal gameplay language with generated Host
bindings, bounded Tasks, explicit Host resources, typed state, and Restart
Reload.

## Status

```text
Gate 1 v2.9 old MVR = STOP
Nexa Internal Language Pivot = ACTIVE
Nexa Internal Pivot M1 = FINALIZING
Current target = Rust-only dogfood Gameplay Language
Seamless advanced Reload = REMOVED
```

The STOP decision ends the former general-product route. It does not end Nexa
as an internal language. The immutable experiment history is available at the
annotated tag `gate1-v2.9-stop`; the active branch keeps only the compact
[history index](docs/history/GATE1_V2_9_STOP.md).

## Product path

```text
Nexa source
→ parser and type checker
→ bytecode compiler and verifier
→ RealmRuntime
→ spawn_task / poll_task
→ generated Rust Host binding
→ typed @state
→ Restart Reload
```

`examples/combat-runtime` is the dogfood loop. Its `combat_api.nidl` is the only
Host API source and generates the Rust Trait, Dispatcher, codecs, stable
function IDs, Exact Interface Hash, Nexa declaration, and test Stub into
`OUT_DIR`. The Host implements only that Trait.

Restart Reload stops admission, cancels old Tasks, detaches old Requests,
migrates state on staging, commits the new root, then activates it. Migration
failure rolls back before commit; activation failure remains observable after
commit. Old continuations and Completion Buffers are not supported.

## Run

The repository pins Rust `1.97.1` in `rust-toolchain.toml`.

```sh
cargo run -p nexa-cli -- compile examples/add.nexa
cargo run -p combat-runtime
cargo xtask check
```

Focused commands are documented in [Testing](docs/TESTING.md). Generated
outputs go to `target/nexa-artifacts/` under the
[Artifact Policy](docs/ARTIFACT_POLICY.md).

The normative entry point is [Baseline Index](baseline/BASELINE_INDEX.md), and
the current direction is [Roadmap](ROADMAP.md).
