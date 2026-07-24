# Gate 0 Signal Log

## H2a — continuation promotion

- Hypothesis: a pre-reserved executable task can promote after fuel exhaustion without a VM-managed allocation.
- Experiment: Fast Task Runtime Benchmark v1.
- Frozen workload: `nexa_fuel_promotion_resume`.
- Raw data: `reports/raw/fast_task_benchmark_v1.json`.
- Result: pass; `promotion = 0`.
- Attribution: Codex implementation run, 2026-07-25, macOS aarch64, rustc 1.97.0.
- Budget consumed: 1,000 timed samples plus 100 warmup iterations.
- Validated scope: FrameArena continuation, Task Slot promotion, scheduler entry reuse, interpreter resume.
- Unvalidated dimensions: process-global allocator traffic in host code and other platforms.
- Retest status: required by the release gate and CI benchmark command.

## Runtime-path integrity

- Hypothesis: benchmark labels described as Nexa execute verified bytecode or Realm runtime operations.
- Experiment: source audit plus successful benchmark v1 run.
- Result: pass; Rust direct is the only direct-compute case.
- Validated scope: immediate, fast complete, promotion/resume, nested call, host sync/async, token, snapshot, state, reload and GC.
- Retest status: benchmark executable asserts the promotion invariant on every run.
