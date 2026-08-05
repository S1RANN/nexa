# Contributing to Nexa

Nexa is developed specification-first. The current normative entry point is
[`baseline/BASELINE_INDEX.md`](baseline/BASELINE_INDEX.md); historical rationale does not override
the active baseline.

## Toolchain

The repository pins Rust 1.97.1 with Clippy and rustfmt in `rust-toolchain.toml`. A standard
rustup installation selects and installs that toolchain automatically when commands run from the
repository.

## Development gate

Run the fast development gate while iterating:

```sh
cargo fmt --check
cargo xtask check
```

`xtask check` is intended to finish in under five minutes. It may run focused
checks directly, but it does not manufacture milestone evidence.

## Finalization gate

Milestone finalization runs once on the clean candidate commit:

```sh
cargo xtask finalize-m5
```

The finalizer performs the workspace build/test/doc suite once, writes a
HEAD/tree/toolchain-bound receipt, and lets downstream M4, M4R1, and M5 gates
validate that receipt instead of rerunning the same tests. Formal benchmark
artifacts are likewise reused only when their provenance matches exactly.
Use `cargo xtask finalize-m5 --dry-run` to inspect the step plan,
`--force-bench` to refresh current-HEAD performance evidence, and
`--refresh-baseline` to rebuild the immutable baseline cache.

The underlying commands remain available for focused diagnosis:

```sh
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo run -p nexa-cli -- qa baseline
cargo run -p nexa-cli -- qa machines
cargo run -p nexa-machine -- check-generated
cargo run -p nexa-cli -- qa models
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
