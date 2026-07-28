# Gate 1 v2.9 Authorization

Status: **AUTHORIZED**

Approved scope:

- preserve the v2.8 H2 Raw pointer, per-run outcome, direct-count, and absolute-allocation repairs;
- read benchmark warmup from `metrics.warmup_samples.value` and per-process samples from
  `performance_processes[*].samples`;
- seal v2.8 as structurally valid evidence with semantically insufficient decision derivation;
- derive Stable Core Failure from all component outcomes aggregated independently for each run;
- require H1/H3 semantic comparison PASS and H2 semantic/allocation PASS with performance
  restricted to PASS or INCONCLUSIVE;
- require formal-data end-to-end contracts to derive `stable_core_failures = [H1,H2,H3]` and
  `Product Decision = STOP`;
- require all v2.9 Formal Run invocations to use the qualified unrestricted subprocess environment;
- prohibit importing v2.8 Raw artifacts as v2.9 formal executions;
- prohibit I2.9 from modifying any F2.9 output path;
- use the `I2.9 → E2.9 → D2.9 → R2.9 → F2.9` commit protocol.

No product feature, product-outcome threshold, absolute performance budget, cross-run tolerance,
scenario count, or execution budget is changed.

Execution budget:

```text
Formal Run 1 × 1
Formal Run 2 × 1
Replay × 1
Retry × 0
```

The authorization is valid only with the bound prefreeze closure and qualified environment in
`experiments/gate1-v2.9/authorization.json`.
