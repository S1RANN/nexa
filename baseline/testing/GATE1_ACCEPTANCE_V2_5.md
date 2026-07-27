# Gate 1 Acceptance v2.5

Version: **2.5.0**

Status: **frozen before Gate 1 v2.5 formal execution**.

Gate 1 v2.5 retains every H1, H2, and H3 product-outcome threshold from v2.4. It changes only
semantic projection, cross-run comparison, structural-state propagation, phase-aware contract
evaluation, deterministic finalization, and Raw-derived receipt verification.

## Product outcomes

- H1 retains the same 20 API, 20 mutation, edit-point, maintained-line, and early-rejection rules.
- H2 retains the same 32 configurations, 12 cleanup triggers, zero-allocation hard conditions,
  three-process benchmark, 100 warmups, 1,000 samples, p95, and frame-time limits.
- H3 retains the same 11 migration, 10 completion, and 9 transaction scenarios and thresholds.
- A legitimate FAIL remains a product outcome and does not make the apparatus invalid.

## Stable comparison

- H2 semantic comparison hashes only the typed whitelist in
  `experiments/gate1-v2.5/h2_semantic_projection.json`.
- Performance and allocation are projected and compared independently.
- Performance uses the frozen formula and numeric tolerances in
  `experiments/gate1-v2.5/h2_performance_policy.json`.
- A semantic or allocation difference is a comparison FAIL; performance-only excess is
  INCONCLUSIVE; missing or malformed evidence is INVALID.

## State and finalization

- Structural status, evidence progress, product outcomes, product decision, and finalization are
  separate fields.
- A structural gap always yields INCOMPLETE / NOT_TRUSTWORTHY and no product decision.
- Stable product FAIL yields STOP after structural closure.
- I2.5, E2.5, D2.5, and F2.5 have disjoint path sets.
- F2.5 is generated as a complete candidate tree while HEAD is D2.5, committed once, and then
  verified against its actual parent and tree.
- WP-048 means PUSH_READY. Push is authorized by 48/48 plus verify-final and is not itself a
  prerequisite contract.
