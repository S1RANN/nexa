# Gate 1 v2.4 Authorization

Version: **2.4.0**

Gate 1 v2.4 is authorized as a new experiment after Gate 1 v2.3 was found
`SEMANTICALLY_INSUFFICIENT`. The v2.3 I/E/R history remains immutable and structurally valid, but
its Raw Run, Gates, Contracts, Decision, and Receipt are not decision evidence for v2.4.

Authorization is effective only because the complete v2.4 prefreeze closure passed, the selected
host produced a newly bound `QUALIFIED` environment proof, and the v2.4 threshold-equivalence
record proves that H1/H2/H3 outcome thresholds are unchanged.

The only authorized formal execution budget is:

- one `formal-run-1` supervisor execution;
- one `formal-run-2` supervisor execution;
- one `replay` supervisor execution;
- one top worker and one H1, H2, and H3 child worker in each execution;
- three benchmark subprocesses inside each H2 worker;
- zero retry.

Formal output is isolated under `target/gate1-v2.4/`. Build output remains under `target/` and is
never copied into Evidence. The Gate packager must regenerate all 21 Gates from Raw Run, and the
Receipt must regenerate them again in a temporary directory and compare every Gate byte-for-byte.

This authorization permits only experiment apparatus, read-only runtime inspection, declarative
fixtures, evidence generation, governance status generation, and verification. It does not permit
new language, compiler, bytecode, Runtime product behavior, Gate 2 implementation, threshold
changes, CI substitution, or additional experimental runs.

The machine-readable authority at `experiments/gate1-v2.4/authorization.json` binds the prefreeze
closure, environment qualification, threshold equivalence, all scenario manifests and the H1
fixture, the outcome-transport protocol, the exact execution budget, and zero retry.
