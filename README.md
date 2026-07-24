# Nexa

Nexa is an experimental typed scripting language and controlled runtime for embedding gameplay
logic in game engines.

The current repository implements **MVR Scope 1.0**. Its purpose is to answer three deliberately
narrow questions:

1. Is an exact-build typed IDL materially better than hand-written Rust host bindings?
2. Can a fast-task execution path support high-frequency host-to-script calls without losing a
   resumable continuation?
3. Can single-module stateful reload preserve gameplay state with commit-before-activation
   semantics?

The normative specification lives under [`baseline/`](baseline/BASELINE_INDEX.md). Historical
design discussions are rationale only and have no normative force.

The current minimum supported Rust toolchain is **1.97.1**.

## Current milestone

The first implementation milestone is the executable state-machine foundation:

```text
machine.spec
→ validated machine model
→ generated Rust transition code
→ versioned trace records
→ bounded state exploration
```

Run the workspace checks:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo run -p nexa-cli -- baseline check
cargo run -p nexa-cli -- machine check
cargo run -p nexa-cli -- model check
```
