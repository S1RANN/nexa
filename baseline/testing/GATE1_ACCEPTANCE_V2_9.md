# Gate 1 Acceptance v2.9

Version: **2.9.0**

Status: **frozen before Gate 1 v2.9 formal execution**.

Gate 1 v2.9 retains every H1, H2, and H3 product-outcome threshold and every performance
tolerance from v2.8. It preserves the v2.8 Raw-to-Gate repair, per-run derived-to-recorded
component equality, direct H2 32/12/0/3 assertions, absolute allocator semantics, and qualified
unrestricted execution protocol. It corrects only the post-evidence Stable Core Failure and
decision derivation.

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

- Each hypothesis outcome is aggregated independently for each of Formal Run 1, Formal Run 2,
  and Replay. The precedence is INVALID, FAIL, INCONCLUSIVE, PASS.
- A Stable Core Failure requires all three per-run aggregate outcomes to be FAIL; it must never
  use one representative sub-Gate outcome as a substitute.
- H1 and H3 additionally require formal and replay semantic comparison PASS and stable semantic
  signatures.
- H2 additionally requires formal and replay semantic and allocation comparison PASS, stable
  semantic signatures, and performance comparison in PASS or INCONCLUSIVE.
- Three apparatus-valid aggregate failures with those conditions yield
  `stable_core_failures = ["H1", "H2", "H3"]`.
- H2 semantic comparison hashes only the typed whitelist in
  `experiments/gate1-v2.9/h2_semantic_projection.json`.
- Performance and allocation are projected and compared independently.
- Performance uses the frozen formula and numeric tolerances in
  `experiments/gate1-v2.9/h2_performance_policy.json`.
- A semantic or allocation difference is a comparison FAIL; performance-only excess is
  INCONCLUSIVE; missing or malformed evidence is INVALID. Every correctly derived comparison
  outcome—including FAIL, INCONCLUSIVE, or INVALID—has Contract Status PASS.

## State and finalization

- Structural status, evidence progress, product outcomes, product decision, and finalization are
  separate fields.
- A structural gap always yields INCOMPLETE / NOT_TRUSTWORTHY and no product decision.
- A stable core FAIL yields STOP before performance-only INCONCLUSIVE is considered.
- WP-024 through WP-026, WP-044, and WP-047 directly assert the formal per-run aggregate
  outcomes, semantic evidence, Stable Core Failure set, priority branch, and derived decision.
- I2.9, E2.9, D2.9, R2.9, and F2.9 have disjoint path sets. I2.9 must not modify any of the
  eight F2.9 output paths; E2.9 is exactly the v2.9 Raw package; D2.9, R2.9, and F2.9 use exact
  enumerated path sets.
- R2.9 contains only the Receipt and precomputes all eight final-file hashes.
- F2.9 is generated deterministically from the R2.9 Receipt and verified against its parent.
- The recorded v2.6 STOP remains unauthorized because the v2.6 Gate/Contract chain was
  structurally unsound.
- v2.7 is an invalid environment execution with no product decision. Its failed Formal Run 1 is
  sealed and is not retried or imported into v2.9.
- v2.8 Raw evidence is valid historical evidence, but its recorded decision and finalization are
  semantically insufficient and unauthorized. v2.8 evidence may be used only for prefreeze
  regression; it is not imported as a v2.9 formal result.
- WP-050 means PUSH_READY. Push is authorized by 50/50 plus verify-final and is not itself a
  prerequisite contract.
