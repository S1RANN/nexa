# Testing

Tests protect current product behavior, not historical evidence formats.

Use the single repository task runner:

```sh
cargo xtask test-core
cargo xtask test-binding
cargo xtask test-task
cargo xtask test-reload
cargo xtask test-model
cargo xtask fuzz-smoke
cargo xtask bench-smoke
cargo xtask repo-audit
cargo xtask check
cargo xtask finalize-m1
```

Unit tests cover parser, type checking, bytecode, verifier, migration, and
runtime internals. Product integration tests enter through generated bindings,
`spawn_task`, `poll_task`, request completion/abandon, and `restart_reload`.
Only Model Differential, fuzz targets, and state-machine-internal tests may
construct low-level Realm events.

Every failure-injection test asserts preconditions and requires its probe to be
consumed. Every terminal Task test checks the resource ledger. Benchmark smoke
runs three independent 1000-sample processes and enforces only the absolute
budgets: p95 at most 100 microseconds and 1000 calls at most 100 milliseconds.

Generated artifacts belong in `target/nexa-artifacts/`.

`test-binding` includes all 20 textual IDL mutation crates against the
handwritten `BusinessHostV1` and runs the generated Combat Host binding
lifecycle test. `test-task` runs the public lifecycle/resource suite and the
external compile-fail test that closes Request/Waiting construction bypasses.
`test-reload` runs 16 restart outcomes, including pre-commit rollback without
Task revival, late-result discard, immediate old-module release, and
post-publication activation failure.

`test-model` executes all 7,381 Realm event sequences of length 0 through 4,
plus at least 20 high-risk sequences, against a real `RealmRuntime`; every step
compares acceptance, normalized snapshots, rejected-state immutability, and
resource invariants. The Realm fuzz target uses the same real adapter.

Combat validates generated Binding-only Request, Token, and Typed Snapshot
ownership for normal completion, explicit cancellation, and Restart Reload.
Each release batch is exact, and its second and third drains are empty.
