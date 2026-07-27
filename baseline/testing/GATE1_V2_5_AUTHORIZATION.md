# Gate 1 v2.5 Authorization

Version: **2.5.0**

Gate 1 v2.5 is authorized as a new experiment after Gate 1 v2.4 completed all three formal
executions but failed structural closure. The archived v2.4 Raw evidence remains non-decision
evidence and may be used only for history and non-formal regression checks.

Authorization is effective because the v2.5 prefreeze closure passed, the selected host produced a
new `QUALIFIED` environment proof, and threshold equivalence confirms that the H1, H2, and H3
product outcome thresholds are unchanged.

The only authorized apparatus changes are:

- an explicit H2 semantic projection and stable semantic signature;
- separation of H2 performance and allocation projections;
- phase-aware comparison and decision state machines;
- the I2.5/E2.5/D2.5/F2.5 finalization protocol;
- Raw-to-Gate, Contract, Decision, document, and Receipt regeneration.

The only authorized formal execution budget is one `formal-run-1`, one `formal-run-2`, one
`replay`, and zero retry. Every execution requires an independent supervisor, top worker, H1
worker, H2 worker, H3 worker, process identities, nonces, and strict preflight/postflight.

Formal output is isolated under `target/gate1-v2.5/`. Evidence may contain only approved Raw JSON,
NDJSON, Markdown, text, and logs. Build output and executables remain under `target/` and must not
enter E2.5.

This authorization does not permit changes to Runtime, Compiler, IDL, Bytecode product semantics;
H1/H2/H3 scenarios or thresholds; production capabilities; the formal run count; or the zero-retry
rule. A legitimate product `FAIL` must remain `FAIL` and may yield `STOP`; it is not an apparatus
failure.

The machine-readable authority at `experiments/gate1-v2.5/authorization.json` binds the prefreeze
closure, qualified environment, threshold equivalence, all scenario manifests, portable provenance
protocol, exact execution budget, and authorized apparatus scope.
