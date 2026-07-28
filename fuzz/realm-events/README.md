# Nexa Realm and Runtime fuzz targets

This cargo-fuzz package contains the long-lived Realm/runtime targets. Every harness rejects oversized
input and bounds all event, object, field, request, and instruction counts.

Run a target through the wrapper:

```text
./run-target.sh <target>
```

The wrapper supplies the target's maximum input length, a per-input timeout, a memory limit, its
seed corpus, and an artifact directory. When libFuzzer saves a `crash-*` input, the wrapper invokes
`cargo fuzz tmin` and retains the minimized reproducer as `artifacts/<target>/minimized-*`.

Targets:

```text
bytecode_decode
verifier
register_planner
enum_match_lowering
try_operator_lowering
completion_ticket_terminal_race
release_intrusive_list
stateful_registry
migration_arena
migration_fixture_parser
source_map_decoder
realm_event_sequence
```
