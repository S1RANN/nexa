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

The repository now closes the planned model-checked vertical slice:

```text
machine.spec
→ validated machine model
→ generated Rust transition code
→ versioned trace records
→ bounded Task/Scope exploration and exhaustive runtime differential replay
→ pre-reserved Fast Task + safe FrameArena
→ verified register bytecode + checked interpreter
→ safe-Rust mark/sweep heap
→ Lexer / Parser / type check / HIR / bytecode
→ exact-hash IDL + typed Rust binding
→ HostRequest / ResourceToken / Snapshot
→ single-module Stateful pause / stage / commit reload
```

`MicroProgram` remains test-only. Public execution uses verified bytecode. The compiler accepts
typed functions and `task fn`, bindings, arithmetic, calls, `if`, bounded `while`/`for` lowering,
`await`, non-suspending `defer` validation, and nominal struct/enum/class/stateful-class
declarations.

Try the end-to-end and IDL entry points:

```sh
cargo run -p nexa-cli -- compile examples/add.nexa
cargo run -p nexa-cli -- idl check examples/game.idl
cargo run -p nexa-cli -- idl generate examples/game.idl
cargo run --release -p nexa-runtime --example fast_task_bench
```

The reproducible Task-runtime-only benchmark result is recorded in
[`reports/fast_task_benchmark_v0.md`](reports/fast_task_benchmark_v0.md).

Run the workspace checks:

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

The repository pins Rust 1.97.1. Linux CI is required; Windows and macOS currently run the same
suite as non-blocking portability checks. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the
specification-first workflow.
