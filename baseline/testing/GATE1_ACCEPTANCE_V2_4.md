# Gate 1 Acceptance v2.4

Version: **2.4.0**

Status: **frozen before Gate 1 v2.4 formal execution**.

Gate 1 v2.4 retains every Gate 1 v2.3 outcome threshold. It adds only apparatus conditions for
mutation-semantic equivalence, dimension effectiveness, scenario execution independence, outcome
transport, Raw-to-Gate regeneration, and artifact hygiene. It does not weaken, remove, or
reinterpret an outcome condition.

## H1 — exact-build IDL

- exactly 20 real Host APIs and 20 independent ABI mutations;
- developer-maintained non-empty lines reduced by at least 50%;
- developer edit points reduced by at least 50%;
- 20/20 mismatches rejected by a real build or load command before interpreter execution;
- generated output deterministic across all valid runs;
- each IDL mutation and handwritten mutation has the same semantic signature and changed symbols.

## H2 — Fast Task

- all 32 configurations execute with their declared task count, promotion ratio, trace mode, host
  call mode, and scalar or complex value shape;
- promotion counts use integer floor division;
- all eight lifecycle cohorts and twelve distinct cleanup/failure triggers execute independently;
- promotion, fuel resume, explicit resume, host resume, and trace-off completion have zero system
  allocator calls without baseline subtraction;
- 100 warmups and 1,000 timed samples in each of three independent benchmark processes;
- p95 is at most 100 microseconds per call and a 1,000-call frame is at most 100 milliseconds;
- observed Runtime snapshots contain no continuation, scheduler-token, request, terminal-record,
  completion-accounting, or resource-ledger invariant violation.

## H3 — stateful reload

- preserve, replace, delete, rollback, commit-before-activation, retired-epoch, and migration-limit
  requirements remain mandatory;
- 11 migration, 10 task/completion, and 9 transaction scenarios execute with unique specifications,
  executors, fixtures, operation traces, and observations;
- each scenario creates a fresh Runtime host and Realm;
- all conclusions derive from production API results and real Runtime snapshots, never an aggregate
  experiment result or a name-to-boolean mapping.

## v2.4 apparatus

- formal run 1, formal run 2, and replay use independent supervisors and child workers;
- legitimate PASS, FAIL, INCONCLUSIVE, and NOT_RUN outcomes exit successfully and retain their
  exact classification; apparatus and transport faults become INVALID;
- preflight and postflight bind the same frozen source, environment, authorization, manifests,
  executors, gate source, decision source, and receipt source;
- the Gate packager regenerates 21 Gates directly from Raw Run evidence;
- the Receipt regenerates Gates in a temporary directory and compares them byte-for-byte;
- all 44 contracts are recomputed, and the decision and full current-status documentation are
  regenerated;
- Evidence excludes build directories, compiler intermediates, lock files, and executables;
- a trustworthy terminal INVALID, STOP, HOLD, or UNVERIFIABLE result may complete Milestone 5.0R4.
