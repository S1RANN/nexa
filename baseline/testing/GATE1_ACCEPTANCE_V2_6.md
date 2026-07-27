# Gate 1 Acceptance v2.6

Version: **2.6.0**

Status: **frozen before Gate 1 v2.6 formal execution**.

Gate 1 v2.6 retains every H1, H2, and H3 product-outcome threshold and every performance
tolerance from v2.5. It changes only the outcome-to-contract mapping, structured comparison
schema, stable-core-failure decision priority, five-commit finalization, and Receipt generation.

## Product outcomes

- H1 retains the same 20 API, 20 mutation, edit-point, maintained-line, and early-rejection rules.
- H2 retains the same 32 configurations, 12 cleanup triggers, zero-allocation hard conditions,
  three-process benchmark, 100 warmups, 1,000 samples, p95, and frame-time limits.
- H3 retains the same 11 migration, 10 completion, and 9 transaction scenarios and thresholds.
- A legitimate FAIL remains a product outcome and does not make the apparatus invalid.

## Stable comparison

- H2 semantic comparison hashes only the typed whitelist in
  `experiments/gate1-v2.6/h2_semantic_projection.json`.
- Performance and allocation are projected and compared independently.
- Performance uses the frozen formula and numeric tolerances in
  `experiments/gate1-v2.6/h2_performance_policy.json`.
- A semantic or allocation difference is a comparison FAIL; performance-only excess is
  INCONCLUSIVE; missing or malformed evidence is INVALID. Every correctly derived comparison
  outcome—including FAIL, INCONCLUSIVE, or INVALID—has Contract Status PASS.

## State and finalization

- Structural status, evidence progress, product outcomes, product decision, and finalization are
  separate fields.
- A structural gap always yields INCOMPLETE / NOT_TRUSTWORTHY and no product decision.
- A stable core FAIL yields STOP before performance-only INCONCLUSIVE is considered.
- I2.6, E2.6, D2.6, R2.6, and F2.6 have disjoint path sets.
- R2.6 contains only the Receipt and precomputes all eight final-file hashes.
- F2.6 is generated deterministically from the R2.6 Receipt and verified against its parent.
- WP-050 means PUSH_READY. Push is authorized by 50/50 plus verify-final and is not itself a
  prerequisite contract.
