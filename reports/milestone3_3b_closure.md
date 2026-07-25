# Milestone 3.3B Closure

Status: **PASS — Milestone 3.3B scope only**

## Immutable implementation identity

The four Milestone 3.3B implementation changes end at
`57672ffe879bc47894bd5c6e8dbbf3991bc5c7cb`. The final test commit is
`ef3e59e4057220c6c94675edd94791e77893e481`; in addition to adding this report, that commit changes
`crates/nexa-runtime/src/stateful.rs` to add the generation boundary matrix
`MAX-2 → MAX-1`, `MAX-1 → MAX`, and `MAX → overflow`.

| Ordered change | Commit |
| --- | --- |
| PR 1 — Reload completion identity and routing | `4dac0cf02df87325ac53aec28ef4e2607d64d2fb` |
| PR 2 — Task state-machine/runtime alignment | `f1dc2c3b8932bceaf8525feba3e72594f5a41d9b` |
| PR 3 — Realm v4 runtime differential | `83b2f010b137aefc7f7e9b66b0e2960b14b128a0` |
| PR 4 — Hard-capacity migration arena | `57672ffe879bc47894bd5c6e8dbbf3991bc5c7cb` |
| Final generation boundary tests and closure evidence | `ef3e59e4057220c6c94675edd94791e77893e481` |

The CI workflow matrix was added separately by
`a5fb367e296fdc1e1fda62cffae16896715466c8`. CI results below are supplemental evidence, not a
condition used to assign this report's PASS status.

## Completion identity and routing evidence

There are 8 end-to-end Rust tests covering 10 required scenarios:

1. Module B success while Module A reloads.
2. Module B error while Module A reloads.
3. Module B cancellation while Module A reloads.
4. Module B abandonment while Module A reloads.
5. Module A rollback replays its buffered completion.
6. Module A commit explicitly discards and accounts for its buffered completion.
7. Module A activation fault uses the same explicit discard accounting.
8. A and B completions arrive in one tick.
9. Terminal-sequence ordering is preserved.
10. Buffer capacity counts only Module A's old module/epoch.

The tests assert exactly-once completion handling, destination/type restoration, scheduler wakeup,
terminal accounting, and final reservation/release counts.

## Task machine and Realm v4 evidence

The machine specification, generated machine, baseline, `TaskState`, `TaskExecution`,
`TaskRuntime`, and `RealmRuntime` all contain distinct `FuelYielded`, `ExplicitYielded`, and
`Cleanup` semantics.

Realm v4 exploration and runtime replay results:

| Model | Visited worlds | Saved shortest paths | Runtime-replayed paths |
| --- | ---: | ---: | ---: |
| Task/runtime state model | 16 | 16 | 16 |
| Dual-module reload completion routing | 18 | 18 | 18 |
| Total | 34 | 34 | 34 |

Every path creates a fresh real runtime adapter and compares task state, execution variant,
scheduler tokens, request and continuation ownership, reload checkpoint, cancellation kind, user
defer, terminal reason, VM resources, and dual-module completion routing after every event.
Failures write valid JSON to `target/model-artifacts/realm-v4-failure.json`.

## Migration hard-capacity matrix

`MigrationContext` reserves object, field, forwarding, payload-byte, and GC-root storage during
construction. All mutation paths use sorted slots with atomic preflight; the completed vectors
move directly into `StatefulRegistry`.

| Boundary | Below limit | At limit | Above limit |
| --- | --- | --- | --- |
| Objects, capacity 2 | 1 succeeds | 2 succeeds | 3 rejects atomically |
| Fields, capacity 2 | 1 succeeds | 2 succeeds | 3 rejects atomically |
| Forwarding decisions, capacity 2 | 1 succeeds | 2 succeeds | 3 rejects atomically |
| Payload bytes | one field unit below succeeds | exact byte capacity succeeds | one field unit above rejects atomically |
| GC roots, capacity 2 | 1 succeeds | 2 succeeds | 3 rejects atomically |
| Fuel, budget 2 | 1-cost program succeeds | 2-cost program succeeds | 3-cost program yields fuel limit |
| Call depth, capacity 2 | depth 1 succeeds | depth 2 succeeds | depth 3 rejects |
| Generation | `MAX-2 → MAX-1` succeeds | `MAX-1 → MAX` succeeds | `MAX → overflow` rejects atomically |

The end-to-end failure case additionally proves that the old registry and Active Root remain
unchanged, the Candidate stays Staging, root publication remains empty, and the failed opcode has
no partial object, field, forwarding, payload, root, or generation mutation.

`MigrationCapacityReport` separately reports object, field, forwarding, payload-byte, and fixed
metadata capacities.

## Allocation observer

The real global allocator observer ran three repetitions. Each result was identical:

| Phase | System allocations |
| --- | ---: |
| `MigrationContext` construction | 5 |
| First migration opcode | 0 |
| `old_get` | 0 |
| `new_create` | 0 |
| `new_set` | 0 |
| `preserve` | 0 |
| `replace` | 0 |
| `delete` | 0 |
| `STATE_FINISH` | 0 |
| Arena-to-registry finish | 0 |

Result flags: `allocation_free_contract_paths_zero=true` and
`migration_hot_paths_zero=true`.

## Local verification

All commands passed on the final implementation plus verification workflow:

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --doc --workspace
cargo run -p nexa-cli -- baseline check
cargo run -p nexa-cli -- machine check
cargo run -p nexa-machine -- check-generated
cargo run -p nexa-cli -- model check
cargo run -p combat-runtime --quiet
cargo run -p nexa-runtime --example fast_task_bench --features allocation-counting -- --smoke
cargo run --manifest-path tools/allocation-observer/Cargo.toml --quiet
```

The model gate reported 929 Realm v3 worlds, 16 Realm v4 task worlds, and 18 Realm v4 routing
worlds. Combat integration, benchmark smoke, and allocator observation all completed.

## Supplemental cross-platform CI evidence

GitHub Actions Run
[30171654681](https://github.com/S1RANN/nexa/actions/runs/30171654681) executed verification commit
`a5fb367e296fdc1e1fda62cffae16896715466c8`, which contains the four implementation changes through
`57672ffe879bc47894bd5c6e8dbbf3991bc5c7cb`. It predates the final generation boundary tests in
`ef3e59e4057220c6c94675edd94791e77893e481` and therefore is not evidence for those added tests.

| Platform | Runner | Result | Run |
| --- | --- | --- | --- |
| Linux x86_64 | `ubuntu-24.04` | Success | [Job 89713718306](https://github.com/S1RANN/nexa/actions/runs/30171654681/job/89713718306) |
| Windows x86_64 | `windows-2025` | Success | [Job 89713718265](https://github.com/S1RANN/nexa/actions/runs/30171654681/job/89713718265) |
| macOS arm64 | `macos-15` | Success | [Job 89713718288](https://github.com/S1RANN/nexa/actions/runs/30171654681/job/89713718288) |

None of the three jobs uses `continue-on-error`. The runner architecture is recorded by
`rustc -vV` in each job.

## Known remaining issues

There are no known remaining contract gaps inside the four-item Milestone 3.3B scope documented
above. This statement does not claim completion of any later Nexa milestone. GitHub emits a
non-blocking deprecation annotation for the Node.js runtime embedded in `actions/checkout@v4` and
`actions/upload-artifact@v4`; GitHub ran those actions on Node.js 24 and all three supplemental
jobs succeeded.
