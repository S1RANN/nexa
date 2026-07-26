# Milestone 4.0 H1/H2/H3 Experiments

The experiments are executable and reproducible:

```text
cargo run --release -p nexa-milestone4-experiments -- --output-dir reports/raw
```

## H1: IDL generation value

The experiment compares a checked-in handwritten dispatcher for 20 combat APIs with the Rust
binding generated from the corresponding IDL. It measures maintained and emitted nonblank lines,
repeated dispatch sites, interface-change edit points, error-detection phase, and diagnostic
quality. It also changes one real API from `i32` to `i64`, verifies that the Exact ABI hash and
generated output change, and captures the invalid-IDL diagnostic.

Raw evidence: `reports/raw/milestone4_h1.json`.

Measured result: the handwritten fixture has 78 maintained lines, 20 repeated dispatch sites,
and three edit points for the changed API. The generated path has 22 maintained IDL lines, no
developer-maintained dispatch sites, and one edit point. Changing `apply_damage` from `i32` to
`i64` changed both the Exact ABI hash and generated Rust; loading the changed module against the
stale handwritten hash was rejected by the real Realm boundary.

## H2: Fast Task matrix

The experiment executes 32 real Realm configurations:

- 500 and 1,000 calls per frame;
- paired 99% first-slice/1% promotion and 95% first-slice/5% promotion traffic;
- trace on/off;
- immediate HostCall on/off;
- struct plus array operations on/off.

Every admitted task must complete, and the output records observed first-slice and promotion
counts, frame time, throughput, and authoritative peak resource totals.

Raw evidence: `reports/raw/milestone4_h2.json`.

All 24,000 task calls completed. Across the recorded matrix, throughput ranged from 603,211 to
1,633,768 calls/s. Factor averages were 1,041,414 calls/s with trace off versus 950,803 with trace
on; 1,158,350 for scalar traffic versus 833,867 with complex types; and 993,792 without HostCall
versus 998,424 with HostCall. These are local comparative signals, not cross-machine targets; the
small HostCall aggregate difference is within single-run local noise.

## H3: Stateful Reload

The experiment uses compiled Nexa v1, v2, and v3 state schemas and a real async Host registry. It
executes preserve, replace, delete, a waiting request, completion during quiesce, rollback and
replay, two commits, an activation fault, simultaneous retired epochs, and an intentionally
undersized offline migration limit.

Raw evidence: `reports/raw/milestone4_h3.json`.

Every required boolean closed as true. The quiesce experiment buffered and replayed one
completion, both schema commits succeeded, two retired epochs coexisted, the undersized migration
was rejected, and the final candidate entered `ActivationFaulted` after publication.
