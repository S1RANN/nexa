# Gate 0 Benchmarks 1.0

Benchmark inputs and the 20 representative gameplay samples are frozen before results are
observed. Changes create a new experiment version and require affected reruns.

Required suites:

- fast task: empty, arithmetic, 500/1000 calls, completion and promotion ratios;
- state handle: sequential/random/hot-loop access with safepoints and fuel yields;
- host calls: immediate, async request, resource token, errors;
- snapshots: creation, sharing, release, release-queue saturation;
- reload: prepare/quiesce/stage/activation failures and late completions.

Performance reporting separates task runtime, mutator, GC, host, collection, and user-logic time.
