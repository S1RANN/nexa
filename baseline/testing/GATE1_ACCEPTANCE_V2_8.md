# Gate 1 Acceptance v2.8

Version: **2.8.0**

Status: **frozen before Gate 1 v2.8 formal execution**.

Gate 1 v2.8 retains every H1, H2, and H3 product-outcome threshold and every performance
tolerance from v2.7. It preserves the v2.7 Raw-to-Gate repair, per-run derived-to-recorded
component equality, direct H2 32/12/0/3 assertions, and absolute allocator semantics. It also
reads warmup count from the formal metric schema and requires Formal Run invocations to use the
qualified unrestricted subprocess environment.

## Product outcomes

- H1 retains the same 20 API, 20 mutation, edit-point, maintained-line, and early-rejection rules.
- H2 retains the same 32 configurations, 12 cleanup triggers, zero-allocation hard conditions,
  three-process benchmark, 100 warmups, 1,000 samples, p95, and frame-time limits.
- H3 retains the same 11 migration, 10 completion, and 9 transaction scenarios and thresholds.
- A legitimate FAIL remains a product outcome and does not make the apparatus invalid.
- Every H1/H2/H3 Gate compares its derived outcome with the matching recorded component outcome
  for each of Formal Run 1, Formal Run 2, and Replay.
- H2 Gate contracts read `/semantic/snapshot_scenarios`, `/semantic/cleanup_matrix`,
  `/metrics/allocation_counts/value`, `allocation_observer.runs`, and
  `performance_processes` directly from each Raw result.
- Allocation counts are absolute global-allocator deltas; no baseline is subtracted.

## Stable comparison

- H2 semantic comparison hashes only the typed whitelist in
  `experiments/gate1-v2.8/h2_semantic_projection.json`.
- Performance and allocation are projected and compared independently.
- Performance uses the frozen formula and numeric tolerances in
  `experiments/gate1-v2.8/h2_performance_policy.json`.
- A semantic or allocation difference is a comparison FAIL; performance-only excess is
  INCONCLUSIVE; missing or malformed evidence is INVALID. Every correctly derived comparison
  outcome—including FAIL, INCONCLUSIVE, or INVALID—has Contract Status PASS.

## State and finalization

- Structural status, evidence progress, product outcomes, product decision, and finalization are
  separate fields.
- A structural gap always yields INCOMPLETE / NOT_TRUSTWORTHY and no product decision.
- A stable core FAIL yields STOP before performance-only INCONCLUSIVE is considered.
- I2.8, E2.8, D2.8, R2.8, and F2.8 have disjoint path sets.
- R2.8 contains only the Receipt and precomputes all eight final-file hashes.
- F2.8 is generated deterministically from the R2.8 Receipt and verified against its parent.
- The recorded v2.6 STOP remains unauthorized because the v2.6 Gate/Contract chain was
  structurally unsound.
- v2.7 is an invalid environment execution with no product decision. Its failed Formal Run 1 is
  sealed and is not retried or imported into v2.8.
- WP-050 means PUSH_READY. Push is authorized by 50/50 plus verify-final and is not itself a
  prerequisite contract.
