#!/bin/sh
set -eu

run_gate() {
    name=$1
    shift
    echo "gate: $name"
    "$@"
}

gate_tmp=$(mktemp -d "${TMPDIR:-/tmp}/nexa-milestone4-gates.XXXXXX")

run_gate format cargo fmt --all -- --check
run_gate check cargo check --workspace --all-targets
run_gate clippy cargo clippy --workspace --all-targets -- -D warnings
run_gate tests cargo test --workspace --all-targets
run_gate doc-tests cargo test --doc --workspace

run_gate baseline cargo run -p nexa-cli -- baseline check
run_gate machines cargo run -p nexa-cli -- machine check
run_gate generated-code cargo run -p nexa-machine -- check-generated

run_gate bytecode-build cargo run -p nexa-cli -- build examples/add.nexa -o "$gate_tmp/add.nxb"
run_gate bytecode-verify cargo run -p nexa-cli -- verify "$gate_tmp/add.nxb"
run_gate bytecode-corpus cargo run -p nexa-cli -- dump --section code "$gate_tmp/add.nxb"

run_gate model-v3-v4-v5 cargo run -p nexa-cli -- model-check
run_gate v4-differential cargo test -p nexa-model --test realm_v4_differential
run_gate v5-differential cargo test -p nexa-model --test realm_v5_differential
run_gate v5-failure-differential cargo test -p nexa-model --test realm_v5_failure_differential

run_gate combat-runtime cargo run -p combat-runtime
run_gate benchmark-smoke cargo run -p nexa-benchmark-v6 -- --smoke
run_gate allocator-observer cargo run --manifest-path tools/allocation-observer/Cargo.toml

run_gate migration-v1 cargo run -p nexa-cli -- build fixtures/migration/v1.nexa -o "$gate_tmp/v1.nxb"
run_gate migration-v2 cargo run -p nexa-cli -- build fixtures/migration/v2.nexa -o "$gate_tmp/v2.nxb"
run_gate migration-fixture cargo run -p nexa-cli -- fixture-check fixtures/migration
run_gate migrate-check cargo run -p nexa-cli -- migrate-check \
    --old-module "$gate_tmp/v1.nxb" \
    --new-module "$gate_tmp/v2.nxb" \
    --state fixtures/migration/state.json \
    --format json \
    --dump-state \
    --diff-state

run_gate diagnostic-snapshots cargo test -p nexa-runtime --test runtime_baseline
snapshot_count=$(find crates/nexa-runtime/tests/snapshots/runtime -name '*.snap' | wc -l | tr -d ' ')
test "$snapshot_count" -eq 10

run_gate fuzz-build cargo build --manifest-path fuzz/mvr/Cargo.toml --bins
for target in \
    bytecode_decode \
    verifier \
    register_planner \
    enum_match_lowering \
    try_operator_lowering \
    completion_routing \
    completion_ticket_terminal_race \
    release_intrusive_list \
    stateful_registry \
    migration_arena \
    migration_fixture_parser \
    source_map_decoder \
    realm_event_sequence
do
    run_gate "fuzz-$target" \
        "fuzz/mvr/target/debug/$target" \
        "fuzz/mvr/corpus/$target/seed" \
        -runs=1
done

rm -rf tools/allocation-observer/target
echo "Milestone 4.0 local gates passed"
