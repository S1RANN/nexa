# Gate 1 v2.3 Authorization

Version: **2.3.0**

Gate 1 v2.3 is a new experiment version after Gate 1 v2.2 ended `NOT_TRUSTWORTHY`. Gate 1 v2.2
completed its three technical executions, but its frozen Governance Contract used an invalid
direct-successor assertion. Its I2.2/E2.2 history is sealed, no R2.2 exists, and its outcomes are
not imported into v2.3.

Authorization was given by the repository owner through the Gate 1 v2.3 plan on 2026-07-27
(Asia/Shanghai), only after the complete prefreeze closure passed and the selected host produced
the bound `QUALIFIED` environment proof with zero stress-test failure. The authorized local
execution budget is exactly:

- one `formal-run-1` supervisor execution;
- one `formal-run-2` supervisor execution;
- one `replay` supervisor execution;
- one H1, H2, and H3 child worker per top-level execution;
- three benchmark subprocesses inside each H2 worker.

No retry, threshold change, CI substitution, Gate 2 implementation, additional experimental run,
or production Runtime/Compiler/IDL/Bytecode behavior change is authorized. Formal output is
isolated under `target/gate1-v2.3/`. The frozen environment uses
`gate1-process-handshake-v1`; mandatory provenance does not use restricted process introspection.
The authorization binds the prefreeze closure, renewed qualification, portable protocol, and
Acceptance equivalence hashes in `experiments/gate1-v2.3/authorization.json`.
