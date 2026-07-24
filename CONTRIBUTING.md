# Contributing to Nexa

Nexa is developed specification-first. The current normative entry point is
[`baseline/BASELINE_INDEX.md`](baseline/BASELINE_INDEX.md); historical rationale does not override
the active baseline.

## Toolchain

The repository pins Rust 1.97.1 with Clippy and rustfmt in `rust-toolchain.toml`. A standard
rustup installation selects and installs that toolchain automatically when commands run from the
repository.

## Required checks

Run these commands before proposing a change:

```sh
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo run -p nexa-cli -- baseline check
cargo run -p nexa-cli -- machine check
cargo run -p nexa-machine -- check-generated
cargo run -p nexa-cli -- model check
```

Linux CI is required. Windows and macOS run the same suite as observational jobs until their
results are stable enough to become required.

## State-machine changes

Edit the normative files under `specs/machines/`, then regenerate the checked-in Rust source:

```sh
cargo run -p nexa-machine -- generate
```

Never edit `crates/nexa-runtime/src/generated/machines.rs` manually. CI runs
`nexa-machine check-generated` and rejects drift.

## Scope discipline

Keep changes inside MVR Scope 1.0. Do not add deferred language or runtime capabilities, empty
future-facing interfaces, or compatibility layers without first updating the active baseline.
