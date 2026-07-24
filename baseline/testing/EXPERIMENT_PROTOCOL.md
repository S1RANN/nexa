# Experiment Protocol 1.0

Outcomes:

```text
PASS | FAIL | INCONCLUSIVE | INVALID
```

Attribution:

```text
A core design defect
B MVR cut artifact
C immature implementation
D invalid experimental apparatus
```

Only controlled, independently reproduced A signals enter kill criteria directly. B or C
classification requires unanimous review. Disagreement defaults to A pending reproduction.

Each hypothesis permits one minimal amendment and one retest. A second inconclusive result becomes
`UNVERIFIABLE_WITHIN_MVR`.

State machines use `*.machine.spec` as their single transition source. Generated runtime guards,
explorer models, coverage IDs, and emitted traces must differentially replay without divergence.
