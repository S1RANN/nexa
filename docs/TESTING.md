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
cargo xtask test-candidate-freshness
cargo xtask test-language-v2
cargo xtask test-object-model-v2
cargo xtask test-async-v2
cargo xtask test-nidl-v2
cargo xtask test-structured-codegen
cargo xtask test-standalone
cargo xtask test-repl
cargo xtask test-entrypoints
cargo xtask m4r1-scale-stress
cargo xtask check
cargo xtask finalize-m1
cargo xtask finalize-m2
cargo xtask finalize-m3-r1
cargo xtask finalize-m3-r2
cargo xtask finalize-m3-r3
cargo xtask finalize-m4-r1
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

`test-candidate-freshness` executes six real Engine scenarios:

1. pending Candidate B followed by a revert to active A;
2. in-flight Candidate B followed by a revert to active A;
3. queued Result B followed by a revert to active A;
4. ready Candidate B with automatic Reload disabled followed by a revert to
   active A;
5. pending Candidate C followed by a return to previously terminal B;
6. queued Result B while the desired source advances to C.

The machine report is written to
`target/nexa-artifacts/m3r3-candidate-freshness/report.json`. Every scenario
must prove the stale Candidate never becomes active, Last Known Good is
unchanged by stale work, terminal accounting remains balanced, and the
aggregate `staleCandidatesCommitted` value is `0`.

Additional unit regressions cover source-refresh failure followed by recovery
and same-hash A → B → C → B Generation overlap. They verify that fail-closed
refresh does not retain a stale observation and that an old terminal cannot
clear a newer queued or in-flight identity.

`finalize-m3-r3` independently reruns the M3R2 generation-accounting gate and
the M3R3 freshness gate, checks the product freshness boundary, validates
status documents and immutable predecessor tags, and requires the annotated
`developer-loop-m3-complete-r3` tag at the final commit. Its report is written
to `target/nexa-artifacts/m3r3-finalize/final-report.json`.

`test-nidl-v2` mutates Contracts, `host`/`nexa` blocks, attributes, types,
names, declaration uniqueness, recursive layouts, async declarations,
comments, and source spans. `test-structured-codegen` sends every Binding Model
through TokenStream, `syn`, `prettyplease`, a second `syn` parse, and
`cargo check`; it also checks identifier-injection rejection and byte-for-byte
determinism. `test-binding` retains the generated Combat Host binding lifecycle
coverage. `test-task` runs the public lifecycle/resource suite and the external
compile-fail test that closes Request/Waiting construction bypasses.
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

`test-language-v2`, `test-object-model-v2`, and `test-async-v2` cover the
frozen source surface, value/reference semantics, precise roots, `.await`
chains, cancellation, Host waits, and Reload. `test-standalone` covers
synchronous and asynchronous `main`, script lowering, arguments, Console Host,
and exit codes. `test-repl` proves Cell persistence, rollback after failure,
async cancellation, commands, and resource ceilings. `test-entrypoints`
validates Required and Optional typed entrypoint dispatch, including the three
Snake strategies. `m4r1-scale-stress` repeats the Package-scale and freshness
thresholds on Language v2.

`finalize-m4-r1` reruns workspace format, check, Clippy, tests, doc tests, the
ten M4R1 gates, the repository check, legacy-surface audit, status and
predecessor-tag checks, and clean annotated-tag validation. Its report is
`target/nexa-artifacts/m4r1-finalize/final-report.json`. It cannot report PASS
until `language-scale-m4-complete-r1` is an annotated tag targeting the same
clean commit as `main`.
