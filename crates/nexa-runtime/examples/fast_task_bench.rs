use std::hint::black_box;
use std::time::Instant;

use nexa_runtime::{FrameArena, FrameLimits, RuntimeLimits, TaskRuntime};

const SAMPLES: usize = 10_000;

fn main() {
    let mut runtime = TaskRuntime::new(1, RuntimeLimits::default());
    let scope = runtime.create_scope(None).unwrap();
    bench("empty complete", || {
        let task = runtime.admit_task(scope, 1, true).unwrap();
        runtime.poll_task(task).unwrap();
        runtime.finish_task(task).unwrap();
    });

    bench("10 ops complete", || compute(10));
    bench("100 ops complete", || compute(100));

    let mut promoted = TaskRuntime::new(2, RuntimeLimits::default());
    let promoted_scope = promoted.create_scope(None).unwrap();
    bench("fuel promotion", || {
        let task = promoted.admit_task(promoted_scope, 1, true).unwrap();
        promoted.poll_task(task).unwrap();
        promoted.yield_task(task).unwrap();
        promoted.resume_task(task).unwrap();
        promoted.finish_task(task).unwrap();
    });

    bench("call depth 32", || {
        let mut arena = FrameArena::new(FrameLimits::default());
        for function in 0..32 {
            arena.push(function, 2).unwrap();
        }
        for _ in 0..32 {
            black_box(arena.pop().unwrap());
        }
    });

    let mut cancellation = TaskRuntime::new(3, RuntimeLimits::default());
    bench("scope cancellation", || {
        let scope = cancellation.create_scope(None).unwrap();
        cancellation.cancel_scope(scope).unwrap();
        cancellation.begin_scope_cancellation(scope).unwrap();
        cancellation.finish_scope_cancellation(scope).unwrap();
        cancellation.destroy_scope(scope).unwrap();
    });

    let mut disabled = TaskRuntime::new(4, RuntimeLimits::default());
    disabled.set_trace_enabled(false);
    let disabled_scope = disabled.create_scope(None).unwrap();
    bench("trace disabled empty", || {
        let task = disabled.admit_task(disabled_scope, 1, true).unwrap();
        disabled.poll_task(task).unwrap();
        disabled.finish_task(task).unwrap();
    });
}

fn compute(count: u32) {
    let mut value = 0_u32;
    for item in 0..count {
        value = black_box(value.wrapping_add(item));
    }
    black_box(value);
}

fn bench(name: &str, mut operation: impl FnMut()) {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        operation();
        samples.push(started.elapsed().as_nanos());
    }
    samples.sort_unstable();
    let total = samples.iter().sum::<u128>();
    println!(
        "{name}: mean={}ns p50={}ns p95={}ns p99={}ns",
        total / SAMPLES as u128,
        percentile(&samples, 50),
        percentile(&samples, 95),
        percentile(&samples, 99),
    );
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    samples[(samples.len() - 1) * percentile / 100]
}
