# Gate 1 Acceptance 1.0

Status: **frozen before formal results**. One amendment is permitted only after an `INVALID` or
first `INCONCLUSIVE` result and must be reviewed before a retest. A second inconclusive result is
`UNVERIFIABLE_WITHIN_MVR`. Results are valid for this exact implementation tree, toolchain,
feature set, and frozen input manifest.

## Common validity rules

- Two formal runs and one independent replay use new processes and output directories.
- Each benchmark process performs 100 warmups followed by 1,000 timed samples; H2 uses three
  independent processes per formal run.
- Semantic booleans, hashes, code metrics, and allocation counts must match exactly across runs.
- Timing distributions may differ by at most 50% for mean and p95; coefficient of variation must
  be at most 2.00. Timing is advisory when semantic and allocation hard conditions pass.
- No result is discarded as an outlier. A device self-check failure or input mutation makes that
  run `INVALID`.
- Fixture hashes, toolchain, feature set, implementation commit/tree, monotonic timer, fixed seed,
  and clean workspace are mandatory.

## H1a — exact-build IDL

PASS requires exactly 20 real host APIs, at least 50% fewer developer-maintained non-empty lines
and interface-change edit points than the maintained handwritten dispatcher, all 20 ABI mutations
rejected at build or load, zero gameplay-stage ABI errors, distinct generated/maintained line
counts, and byte-for-byte deterministic generation.

FAIL is any hard condition violation. INCONCLUSIVE means the code/change accounting cannot be
reproduced despite valid inputs. INVALID means common validity failed. Minimum sample: 20 APIs and
20 ABI mutations, repeated in both formal runs.

## H2a — Fast Task

PASS requires the real matrix to cover 500 and 1,000 calls/frame, 95/5 and 99/1
first-slice/promotion ratios, trace and HostCall on/off, scalar and complex values, fuel and
explicit yield, and success/error/cancel/abandon. All tasks must terminate by policy;
promotion, resume, and trace-disabled host completion must each perform exactly zero system
allocator calls without subtracting a baseline.

Every promoted task owns exactly one continuation, no continuation is resumed twice, terminal
tasks own none, waiting tasks own exactly one request, scheduler tokens are unique, request and
completion reservations return to zero, and the resource ledger is zero after teardown.

The advisory first-slice budget is p95 ≤ 100 microseconds per call and one 1,000-call frame ≤
100 milliseconds on the frozen machine. A hard invariant failure is FAIL; apparatus failure is
INVALID; stable semantic results with excessive timing noise are INCONCLUSIVE.

## H3a — stateful reload

PASS requires real compiler → bytecode encode/decode → verifier → reload metadata → migration
interpreter → RealmRuntime execution for v1/v2/v3 and the faulted candidate. Preserve, replace,
delete, waiting request routing, quiesce buffering/replay, exact rollback, commit publication,
commit-before-activation fault, multiple retired epochs, independent reap, and atomic migration
limit failure must all pass. Final state hashes must match across runs.

A core semantic failure is FAIL. A limitation caused only by the documented single-module MVR cut
is INCONCLUSIVE with attribution B. Apparatus or mutation failure is INVALID.
