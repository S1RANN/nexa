# Gate 1 v2.6 Authorization

Status: **AUTHORIZED**

Approved scope:

- map every correctly derived Comparison Outcome, including `INCONCLUSIVE`, to Contract `PASS`;
- give a stable core failure priority over performance-only comparison noise;
- use the `I2.6 → E2.6 → D2.6 → R2.6 → F2.6` commit protocol;
- let the Receipt precompute hashes for exactly eight deterministic finalization files.

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
`experiments/gate1-v2.6/authorization.json`.
