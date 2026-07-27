# Gate 1 Acceptance v2.3

Version: **2.3.0**

Status: **frozen before Gate 1 v2.3 formal execution**.

Gate 1 v2.3 retains every Gate 1 v2.2, Gate 1 v2.1, and Gate 1 v2 outcome threshold. It adds only
the supersession graph, dual-layer Contract semantics, mandatory prefreeze closure, and
terminal-short-circuit Receipt semantics. It does not weaken, remove, or reinterpret an outcome
condition.

## H1 — exact-build IDL

- exactly 20 real Host APIs and 20 independent ABI mutations;
- developer-maintained non-empty lines reduced by at least 50%;
- developer edit points reduced by at least 50%;
- 20/20 mismatches rejected by a real build or load command before interpreter execution;
- generated output deterministic across all valid runs.

## H2 — Fast Task

- all 32 real configurations complete according to policy;
- promotion, fuel resume, explicit resume, host resume, and trace-off completion have zero system
  allocator calls without baseline subtraction;
- 100 warmups and 1,000 timed samples in each of three independent benchmark processes;
- p95 is at most 100 microseconds per call and a 1,000-call frame is at most 100 milliseconds;
- observed Runtime snapshots contain no continuation, scheduler-token, request, terminal-record,
  completion-accounting, or resource-ledger invariant violation.

## H3 — stateful reload

- preserve, replace, delete, rollback, commit-before-activation, retired-epoch, and migration-limit
  requirements remain mandatory;
- 11 migration, 10 task/completion, and 9 transaction scenarios execute independently;
- all conclusions derive from real Runtime snapshots, registry state, accounting, and API results.

## v2.3 apparatus

- formal run 1, formal run 2, and replay use independent top-level workers;
- H1, H2, and H3 use independent child workers inside each top-level run;
- the selected environment must have a bound `QUALIFIED` environment proof;
- every parent/child edge uses `gate1-process-handshake-v1` with Child::id, nonce, one-time token,
  executable hash, role, run ID, and output-path verification;
- mandatory provenance never reads the system process table or another process's executable path;
- output remains under `target/gate1-v2.3` until a separate verified packaging step;
- preflight and postflight use the same strict clean-tree, SHA, tree, manifest, input, toolchain,
  timer, allocator, and device checks;
- every critical metric carries provenance;
- contracts and the final decision are recomputed from raw artifacts;
- Apparatus Contracts prove evidence-chain completeness independently of hypothesis outcomes;
- Outcome Contracts accept correctly derived terminal PASS, FAIL, INVALID, INCONCLUSIVE, and
  structured NOT_RUN states;
- the supersession graph is validated by reachability without rewriting historical records;
- Governance negative tests, Contract satisfiability, every Decision branch, terminal short
  circuit, and synthetic I/E/R all pass before Authorization;
- a trustworthy terminal INVALID, STOP, PIVOT, HOLD, or UNVERIFIABLE result may still complete the
  milestone when its Receipt is fully recomputable;
- a second inconclusive result terminates at `UNVERIFIABLE_WITHIN_MVR`.
