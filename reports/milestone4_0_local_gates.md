# Milestone 4.0 Local Gate Results

The complete local release gate is executable with:

```text
./scripts/milestone4-local-gates.sh
```

The script runs the five required Rust workspace commands, then validates the normative baseline,
machine specifications, generated machine code, a compiled/decoded/verified bytecode corpus,
Realm v3/v4/v5 models, v4/v5 differential suites, Combat Runtime, Benchmark v6 smoke, the global
allocator observer, a checked-in migration fixture, ten deterministic runtime diagnostic
snapshots, and all 13 MVR fuzz targets against their checked-in seed once.

The fuzz smoke uses the libFuzzer executables built directly by Cargo because `cargo-fuzz` is not
installed on this machine. This still executes every harness on its seed corpus with `-runs=1`;
it is a smoke gate, not a timed fuzz campaign.

The full gate completed successfully on macOS aarch64 with Rust 1.97.1. Machine-readable results
are stored in `reports/raw/milestone4_gate_results.json`; they record 17 normative files, 8
machines, 929/16/27,715 Realm v3/v4/v5 worlds, 16 benchmark cases, three zero-allocation observer
repetitions, one end-to-end migration fixture, ten diagnostic snapshots, and 13 fuzz targets.
