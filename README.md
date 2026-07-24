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

The repository implements **Milestone 2B — Unified Executable Runtime**:

```text
Nexa source
→ compiler and verifier
→ RealmRuntime
→ Task-owned InterpreterContinuation
→ FrameArena and automatic GC roots
→ scheduler, Host resources and bounded completion delivery
→ single-module pause / migrate / commit reload
→ explicit Completed / Cancelled / Trapped terminal records
```

`MicroProgram` remains test-only. Normal callers retain `TaskHandle`, never a continuation, and
drive execution through `RealmRuntime::poll_task` or `RealmRuntime::tick`. The compiler preserves
function effects, performs return-flow and lexical-scope checks, lowers non-suspending `defer`,
and keeps nominal reference types distinct. Bytecode carries effect, frame, root-map, safepoint,
call-depth, call-range and static loop-bound metadata.

Try the end-to-end and IDL entry points:

```sh
cargo run -p nexa-cli -- compile examples/add.nexa
cargo run -p nexa-cli -- idl check examples/game.idl
cargo run -p nexa-cli -- idl generate examples/game.idl
cargo run -p combat-runtime
cargo run --release -p nexa-runtime --example fast_task_bench --features allocation-counting
```

The reproducible real-path benchmark result is recorded in
[`reports/fast_task_benchmark_v1.md`](reports/fast_task_benchmark_v1.md).

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
