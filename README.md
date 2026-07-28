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

The normative specification lives under [`baseline/`](baseline/BASELINE_INDEX.md). The single
global stage map is [`ROADMAP.md`](ROADMAP.md). Historical design discussions are rationale only
and have no normative force.

The current minimum supported Rust toolchain is **1.97.1**.

## Current status

```text
Current implementation milestone: 4.0R3 complete
<!-- gate1-v2.8-status:start -->
Gate 1 v2.4: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION
Gate 1 v2.5: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION
Gate 1 v2.6: STRUCTURAL_CLOSURE_FAILED / INCOMPLETE / RECORDED STOP NOT AUTHORIZED
Gate 1 v2.7: INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE / NOT AUTHORIZED FOR DECISION
Gate 1 v2.8: VERIFIED_TERMINAL_DECISION
Current decision: UNVERIFIABLE_WITHIN_MVR
Milestone 5.0R8: COMPLETE
Push: AUTHORIZED
<!-- gate1-v2.8-status:end -->
```

The implemented MVR execution path is:

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

Gate 1 v2.5 is governed by the
[`MVR Scope`](baseline/mvr/MVR_SCOPE.md),
the Gate 1 v2.5 acceptance criteria and authorization produced after prefreeze closure, and the
qualified frozen manifest under `experiments/gate1-v2.5/`. Gate 1 v1 through v2.4 remain available through the
unified history graph under `reports/history/gate1/` and their
[`v1 invalidation record`](reports/gate1_v1_invalidation.md) and
[`v2 invalidation record`](reports/gate1_v2_invalidation.md), plus the
[`v2.1 invalid decision`](reports/gate1_v2_1_final_decision.md) and
[`v2.3 semantic invalidation`](reports/gate1_v2_3_semantic_invalidation.md); none is current. The
[`Baseline Index`](baseline/BASELINE_INDEX.md) defines their precedence. `ROADMAP.md` is the only
roadmap; this README deliberately does not duplicate it.

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
