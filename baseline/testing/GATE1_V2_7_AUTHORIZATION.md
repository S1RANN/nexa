# Gate 1 v2.7 Authorization

Status: **AUTHORIZED**

Approved scope:

- repair H2 Gate inputs to the actual frozen Raw schema;
- compare every derived H1/H2/H3 component outcome with its recorded value per run;
- directly assert 32 configurations, 12 cleanup cases, zero invariant violations, three
  allocator-observer repeats, absolute zero allocator deltas, and three benchmark processes;
- invalidate the v2.6 recorded STOP and seal v2.6 as structurally incomplete;
- use the `I2.7 → E2.7 → D2.7 → R2.7 → F2.7` commit protocol;
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
`experiments/gate1-v2.7/authorization.json`.
