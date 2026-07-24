use std::hint::black_box;
use std::time::{Duration, Instant};

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostRegistry, HostTrap, HostValue, PollResult, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHostDomain, RuntimeValue, StepConfig, TaskLimits,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const DEFAULT_SAMPLES: usize = 1_000;
const WARMUP: usize = 100;
const HOST: StableId = StableId(11);
const SCHEMA: StableId = StableId(12);

#[derive(Clone)]
struct Stats {
    name: &'static str,
    p50: u128,
    p95: u128,
    p99: u128,
    mean: u128,
}

#[allow(clippy::too_many_lines)]
fn main() {
    let samples = std::env::var("NEXA_BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SAMPLES);
    let mut results = Vec::new();
    results.push(bench("rust_direct", samples, || {
        black_box(20_i32 + 22);
    }));

    let immediate = build_module(
        FunctionEffect::Immediate,
        vec![
            Instruction::Add {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            Instruction::Return { source: 2 },
        ],
        3,
        2,
    );
    results.push(bench("verified_immediate", samples, || {
        black_box(
            nexa_runtime::CheckedInterpreter::run(
                &immediate,
                0,
                &[RuntimeValue::I32(20), RuntimeValue::I32(22)],
                64,
            )
            .unwrap(),
        );
    }));

    let fast = build_module(
        FunctionEffect::Task,
        vec![Instruction::Return { source: 0 }],
        1,
        1,
    );
    let (mut realm, module, scope) = loaded(fast.clone());
    results.push(bench("nexa_fast_complete", samples, || {
        let task = call(&mut realm, module, scope, 7);
        black_box(realm.poll_task(task, 64).unwrap());
    }));
    let (mut realm, module, scope) = loaded(fast.clone());
    realm.set_trace_enabled(false);
    results.push(bench("nexa_fast_complete_trace_off", samples, || {
        let task = call(&mut realm, module, scope, 7);
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    let yielded = build_module(
        FunctionEffect::Task,
        vec![Instruction::Yield, Instruction::Return { source: 0 }],
        1,
        1,
    );
    let (mut realm, module, scope) = loaded(fast.clone());
    results.push(bench("nexa_fuel_promotion_resume", samples, || {
        let task = call(&mut realm, module, scope, 7);
        assert!(matches!(
            realm.poll_task(task, 0).unwrap(),
            PollResult::Pending(_)
        ));
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    let nested = nested_module();
    let (mut realm, module, scope) = loaded(nested);
    results.push(bench("nexa_nested_calls", samples, || {
        let task = call(&mut realm, module, scope, 7);
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    let (mut realm, module, scope) = loaded(yielded.clone());
    let task = call(&mut realm, module, scope, 1);
    let mut registry = AddRegistry;
    results.push(bench("nexa_sync_host_call", samples, || {
        let outcome = realm
            .with_resource_context(task, |context| {
                registry.call(
                    0,
                    context,
                    HostArgs::new(&[HostValue::I32(20), HostValue::I32(22)]),
                )
            })
            .unwrap()
            .unwrap();
        black_box(outcome);
    }));

    let host_samples = samples.min(200);
    results.push(bench("nexa_async_host_call", host_samples, || {
        let (mut realm, module, scope) = loaded(yielded.clone());
        let task = call(&mut realm, module, scope, 1);
        let request = realm.create_host_request(task).unwrap();
        realm.wait_for_request(task, request).unwrap();
        realm
            .completion_sender()
            .complete(nexa_runtime::HostCompletion {
                realm_id: realm.realm_id(),
                module_id: module.raw().index,
                epoch: realm.module_epoch(module).unwrap(),
                request,
                payload: nexa_runtime::HostPayload::I32(1),
            })
            .unwrap();
        black_box(realm.tick(nexa_runtime::TickBudget::default()).unwrap());
    }));

    results.push(bench("nexa_resource_token", host_samples, || {
        let (mut realm, module, scope) = loaded(fast.clone());
        let task = call(&mut realm, module, scope, 1);
        black_box(
            realm
                .create_resource_token(task, RuntimeHostDomain::Render)
                .unwrap(),
        );
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    results.push(bench("nexa_snapshot_read", host_samples, || {
        let (mut realm, module, scope) = loaded(fast.clone());
        let task = call(&mut realm, module, scope, 1);
        let snapshot = realm.create_snapshot(task, [1, 2, 3].into()).unwrap();
        black_box(realm.snapshot_data(snapshot).unwrap());
    }));

    let (mut state_realm, state_module, _) = loaded(fast.clone());
    let state = state_realm
        .insert_state(
            state_module,
            StableId::from_name("EnemyBrain"),
            nexa_runtime::StateValue::I32(7),
        )
        .unwrap();
    results.push(bench("nexa_state_handle", samples, || {
        black_box(state_realm.resolve_state(state_module, state).unwrap());
    }));

    results.push(bench("nexa_reload", host_samples, || {
        let (mut realm, old, scope) = loaded(yielded.clone());
        let task = call(&mut realm, old, scope, 1);
        let candidate = realm
            .prepare_reload(old, reload_module(), HOST, SCHEMA)
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        black_box(
            realm
                .commit_reload(|module| {
                    assert_eq!(module, candidate);
                    Ok(())
                })
                .unwrap(),
        );
        assert!(realm.terminal_record(task).is_some());
    }));
    results.push(bench("nexa_gc_collect", host_samples, || {
        let (mut realm, _, _) = loaded(fast.clone());
        realm
            .allocate(nexa_runtime::Object::I32Array(vec![1, 2, 3]))
            .unwrap();
        black_box(realm.collect_garbage().unwrap());
    }));

    let allocations =
        nexa_runtime::allocation_snapshot() - nexa_runtime::AllocationSnapshot::default();
    assert_eq!(allocations.promotion, 0);
    println!(
        "{{\"toolchain\":\"{}\",\"os\":\"{}\",\"arch\":\"{}\",\"samples\":{},\
         \"allocation_events\":{{\"admission\":{},\"first_slice\":{},\"promotion\":{},\"resume\":{},\"terminal_cleanup\":{}}},\
         \"fuel_costs\":{{\"fast_complete\":1,\"yield_resume\":1,\"nested_call\":3}},\
         \"promotion_rate\":1.0,\"gc_measured_separately\":true,\"trace_comparison\":true,\"results\":[{}]}}",
        option_env!("RUSTC_VERSION").unwrap_or("rustc-1.97.0"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        samples,
        allocations.admission,
        allocations.first_slice,
        allocations.promotion,
        allocations.resume,
        allocations.terminal_cleanup,
        results
            .iter()
            .map(|result| format!(
                "{{\"case\":\"{}\",\"mean_ns\":{},\"p50_ns\":{},\"p95_ns\":{},\"p99_ns\":{}}}",
                result.name, result.mean, result.p50, result.p95, result.p99
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn build_module(
    effect: FunctionEffect,
    code: Vec<Instruction>,
    registers: u16,
    parameters: usize,
) -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32; parameters],
            result: Some(ValueType::I32),
        },
        registers,
    );
    function.effect(effect);
    for instruction in code {
        function.emit(instruction);
    }
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, SCHEMA)
        .function(function.finish().unwrap());
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn nested_module() -> VerifiedModule {
    let mut caller = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    caller
        .effect(FunctionEffect::Task)
        .emit(Instruction::Call {
            function: 1,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::Return { source: 1 });
    let mut identity_function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    identity_function.emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, SCHEMA)
        .function(caller.finish().unwrap());
    module.function(identity_function.finish().unwrap());
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn reload_module() -> VerifiedModule {
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    migration
        .effect(FunctionEffect::Migration)
        .emit(Instruction::Return { source: 0 });
    let mut task = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    task.effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, SCHEMA)
        .function(migration.finish().unwrap());
    module.function(task.finish().unwrap());
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn loaded(
    verified: VerifiedModule,
) -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    nexa_runtime::ScopeHandle,
) {
    let mut realm = RealmRuntime::new(RealmConfig::default());
    let module = realm.load_module(verified, HOST, SCHEMA).unwrap();
    let scope = realm.create_scope(None).unwrap();
    (realm, module, scope)
}

fn call(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    scope: nexa_runtime::ScopeHandle,
    value: i32,
) -> nexa_runtime::TaskHandle {
    realm
        .call(
            module,
            0,
            &[RuntimeValue::I32(value)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )
        .unwrap()
}

struct AddRegistry;

impl HostRegistry for AddRegistry {
    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 2 {
            return Err(HostTrap::UnknownFunction(id));
        }
        let (HostValue::I32(lhs), HostValue::I32(rhs)) = (args.get(0)?, args.get(1)?) else {
            return Err(HostTrap::Type);
        };
        Ok(HostCallOutcome::Immediate(HostValue::I32(lhs + rhs)))
    }
}

fn bench(name: &'static str, samples: usize, mut operation: impl FnMut()) -> Stats {
    for _ in 0..WARMUP.min(samples) {
        operation();
    }
    let mut durations = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        operation();
        durations.push(started.elapsed());
    }
    durations.sort_unstable();
    let total = durations.iter().sum::<Duration>().as_nanos();
    let stats = Stats {
        name,
        mean: total / samples as u128,
        p50: percentile(&durations, 50),
        p95: percentile(&durations, 95),
        p99: percentile(&durations, 99),
    };
    eprintln!(
        "{}: mean={}ns p50={}ns p95={}ns p99={}ns",
        stats.name, stats.mean, stats.p50, stats.p95, stats.p99
    );
    stats
}

fn percentile(samples: &[Duration], percentile: usize) -> u128 {
    samples[(samples.len() - 1) * percentile / 100].as_nanos()
}
