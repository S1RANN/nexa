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
cargo xtask test-engine-api
cargo xtask test-diagnostics
cargo xtask test-dev-loop
cargo xtask test-cli
cargo xtask test-lsp
cargo xtask editor-check
cargo xtask dev-loop-stress
cargo xtask test-generation-accounting
cargo xtask check
cargo xtask finalize-m1
cargo xtask finalize-m2
cargo xtask finalize-m3-r1
cargo xtask finalize-m3-r2
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

`finalize-m3-r1` independently reruns workspace fmt/check/clippy/test/doc,
M1/M2 regressions, real Engine diagnostic evidence, Worker queue and Result
backpressure races, Reload stress, metric consistency, CLI Source Policy, NIDL
Span and URI/LSP coverage, editor checks, repository audits, clean-worktree
validation, and annotated-tag validation. Its report is written to
`target/nexa-artifacts/m3r1-finalize/final-report.json`.

`test-generation-accounting` executes five real Engine scenarios covering
pre-queue hash replacement, revert to active content, source removal, disable,
and shutdown. It writes a machine report to
`target/nexa-artifacts/m3r2-generation-accounting/report.json`.
`finalize-m3-r2` reads that report and rejects any mismatch between created and
terminal Generations, any duplicate terminal, or any unterminated Generation.
Its final report is written to
`target/nexa-artifacts/m3r2-finalize/final-report.json`.

`test-binding` includes all 20 textual IDL mutation crates against the
handwritten `BusinessHostV1`. Every changed crate compiles its changed script
against the changed NIDL and executes `heartbeat(41)` through
`GeneratedHostRegistry<PatchedBusinessHost>`. It also runs the generated Combat
Host binding lifecycle test. `test-task` runs the public lifecycle/resource
suite and the external compile-fail test that closes Request/Waiting
construction bypasses.
`test-reload` runs 16 restart outcomes, including pre-commit rollback without
Task revival, late-result discard, immediate old-module release, and
post-publication activation failure.

`test-model` executes all 7,381 Realm event sequences of length 0 through 4,
plus 30 high-risk sequences and four current-handle semantic regressions,
against a real `RealmRuntime`. Re-polling a Waiting Task must reuse the current
Task and Request without changing state. Re-completion after completion or
cancellation must report `AlreadyCompleted`, while re-completion after Restart
Reload must report `DetachedByReload`. Every callable step must advance its
corresponding Runtime API attempt counter, and every rejected step compares the
real inspection snapshot, resource ledger, and Host queues before and after.
The Realm fuzz target uses the same real adapter.
`fuzz-smoke` also replays the committed deterministic Realm corpus with an
ordinary test runner.

Combat validates generated Binding-only Request, Token, and Typed Snapshot
ownership for normal completion, explicit cancellation, and Restart Reload.
Each release batch is exact, and its second and third drains are empty.
