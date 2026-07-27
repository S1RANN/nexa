# Gate 1 v2.8 Authorization

Status: **AUTHORIZED**

Approved scope:

- preserve the v2.7 H2 Raw pointer, per-run outcome, direct-count, and absolute-allocation repairs;
- read benchmark warmup from `metrics.warmup_samples.value` and per-process samples from
  `performance_processes[*].samples`;
- seal the v2.7 invalid environment execution without retry or evidence import;
- require all v2.8 Formal Run invocations to use the qualified unrestricted subprocess environment;
- use the `I2.8 → E2.8 → D2.8 → R2.8 → F2.8` commit protocol.

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
`experiments/gate1-v2.8/authorization.json`.
