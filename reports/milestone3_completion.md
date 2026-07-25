# Milestone 3 — Host-Integrated Stateful Runtime

Status: completed locally on Rust 1.97.1.

Published implementation: `0c2c0ee7e008bb334a15ab712d4a5b8b07bd98d7`. The corresponding [GitHub Actions run](https://github.com/S1RANN/nexa/actions/runs/30141408369) passed on Linux, macOS, and Windows.

## Closed execution loop

The combat integration now executes this path without application-side host dispatch or request waiting:

```text
Nexa source
→ typed IDL host import
→ verified HOST_CALL
→ Realm-owned generated HostRegistry
→ immediate result or automatic Waiting
→ cross-thread-safe completion queue
→ typed destination-register writeback
→ continuation resume
```

`RuntimeHost` owns deferred releases outside Realm lifetime. Host requests, resource tokens, release queues, and reload transitions use generated machine implementations. Poisoned completion/release locks recover their guards instead of panicking.

## Stateful reload

State metadata carries stable type/field IDs and explicit schema versions. Schema changes require a Migration entry. The restricted Migration VM supports:

```text
STATE_OLD_GET
STATE_NEW_CREATE
STATE_NEW_SET
STATE_HANDLE_REMAP
STATE_DELETE
```

Verifier effect and call-graph rules reject these capabilities outside Migration code and reject HostCall/Yield/Await inside Migration. Staging validates field types, versions, target existence, module ownership, generations, and recursive GC roots before publication.

The combat scenario migrates `EnemyBrain` v1 to v2, preserves `phase`, adds defaulted `aggression`, removes `legacy_target`, commits, handles a late completion, and exercises deterministic ActivationFaulted behavior.

## Safety and evidence

- Exact per-PC RootMaps are independently reconstructed by verifier dataflow.
- Reload rollback restores continuation, fuel, scheduler state, waiting destination/type, request association, and buffered completions.
- Bytecode v2 uses a checked section directory and bounded decoding for bytes, sections, functions, instructions, registers, RootMaps, loop bounds, imports, state schemas, and exports.
- WCET analysis is memoized, bounded, and includes immediate host costs.
- Dedicated fuzz harnesses cover bytecode decode, verifier, RootMaps, WCET, host imports, and state schemas.
- The isolated global allocator observer reports zero allocations for promotion, resume, and trace-off paths across three repetitions.
- Benchmark v2 labels direct generated thunk, immediate HOST_CALL, and async HOST_CALL as distinct real paths.
- Realm composite model exploration visits 32 worlds; differential replay matches Runtime request/token reservations and releases.

## Local merge gates

All passed:

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
cargo test -p combat-runtime
cargo run -p combat-runtime
cargo run -p nexa-runtime --example fast_task_bench --features allocation-counting -- --smoke
cargo run --manifest-path tools/allocation-observer/Cargo.toml --quiet
```

Performance evidence remains platform-specific. H2 stays `INCONCLUSIVE` in the VM counter report and is supported separately by the allocator-observer result.
