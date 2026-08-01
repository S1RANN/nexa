use std::hint::black_box;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder, RootMap,
    ScriptExport, Signature, ValueType,
};
use nexa_core::{StableId, StateSchemaFingerprint};
use nexa_runtime::{
    HostCallOutcome, HostFunctionAuthority, HostPayload, HostRegistry, HostTrap,
    PendingHostRequest, RealmConfig, RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostArgs,
    RuntimeHostDomain, RuntimeValue, StepConfig, TaskLimits, TaskPoll, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const DEFAULT_SAMPLES: usize = 1_000;
const WARMUP: usize = 100;
const HOST: StableId = StableId(11);
const UNARY_TASK_EXPORT: StableId = StableId(0x4641_5354_554e_4152);
const BINARY_TASK_EXPORT: StableId = StableId(0x4641_5354_4249_4e41);
const BINARY_IMMEDIATE_EXPORT: StableId = StableId(0x4641_5354_494d_4d45);

fn schema() -> StateSchemaFingerprint {
    nexa_bytecode::StateSchema::default().fingerprint()
}

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
    let smoke = std::env::args().any(|argument| argument == "--smoke");
    let samples = if smoke {
        10
    } else {
        std::env::var("NEXA_BENCH_SAMPLES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_SAMPLES)
    };
    let mut results = Vec::new();
    results.push(bench("rust_direct", samples, || {
        black_box(20_i32 + 22);
    }));

    let immediate = build_module(
        BINARY_IMMEDIATE_EXPORT,
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
        UNARY_TASK_EXPORT,
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
        UNARY_TASK_EXPORT,
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
            TaskPoll::Yielded(_)
        ));
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    let nested = nested_module();
    let (mut realm, module, scope) = loaded(nested);
    results.push(bench("nexa_nested_calls", samples, || {
        let task = call(&mut realm, module, scope, 7);
        black_box(realm.poll_task(task, 64).unwrap());
    }));

    let (mut realm, module, scope, generated_host) = loaded_hosted(yielded.clone());
    let task = call(&mut realm, module, scope, 1);
    let mut registry = AddRegistry;
    results.push(bench("generated_rust_thunk_direct_call", samples, || {
        let outcome = realm
            .with_resource_context(task, |context| {
                registry.call_runtime(
                    StableId::from_name("BenchHost::add"),
                    context,
                    RuntimeHostArgs::new(&[RuntimeValue::I32(20), RuntimeValue::I32(22)], None)
                        .unwrap(),
                )
            })
            .unwrap()
            .unwrap();
        black_box(outcome);
    }));
    drop(realm);
    let _ = generated_host.drain_releases();
    let _ = generated_host.begin_close();
    generated_host.try_finish_close().unwrap();

    let host_module = host_call_module();
    let immediate_host = RuntimeHost::new(1_024);
    let mut host_realm = RealmRuntime::hosted(
        RealmConfig::default(),
        immediate_host.clone(),
        Box::new(AddRegistry),
    )
    .unwrap();
    let host_module = host_realm.load_module(host_module, HOST, schema()).unwrap();
    let host_scope = host_realm.create_scope(None).unwrap();
    results.push(bench("nexa_host_call_opcode_immediate", samples, || {
        let task = host_realm
            .spawn_task(
                host_module,
                BINARY_TASK_EXPORT,
                &[RuntimeValue::I32(20), RuntimeValue::I32(22)],
                StepConfig {
                    owner: host_scope,
                    priority: 1,
                    fuel_slice: 64,
                    cumulative_budget: 1_024,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        black_box(host_realm.poll_task(task, 64).unwrap());
    }));
    drop(host_realm);
    let _ = immediate_host.drain_releases();
    let _ = immediate_host.begin_close();
    immediate_host.try_finish_close().unwrap();

    let host_samples = samples.min(200);
    let async_module = async_host_call_module();
    results.push(bench("nexa_host_call_opcode_async", host_samples, || {
        let pending = Arc::new(Mutex::new(None));
        let host = RuntimeHost::new(1_024);
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            host.clone(),
            Box::new(AsyncRegistry {
                pending: Arc::clone(&pending),
            }),
        )
        .unwrap();
        let module = realm
            .load_module(async_module.clone(), HOST, schema())
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = call(&mut realm, module, scope, 1);
        assert!(matches!(
            realm.poll_task(task, 64).unwrap(),
            TaskPoll::Waiting(_)
        ));
        let mut pending = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap();
        pending.ticket.complete(HostPayload::I32(1)).unwrap();
        black_box(realm.tick(TickBudget::default()).unwrap());
        assert!(realm.terminal_record(task).is_some());
        drop(realm);
        let _ = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();
    }));

    results.push(bench("nexa_resource_token", host_samples, || {
        let (mut realm, module, scope, host) = loaded_hosted(fast.clone());
        let task = call(&mut realm, module, scope, 1);
        black_box(
            realm
                .create_resource_token(
                    task,
                    StableId::from_name("FastTaskBenchToken"),
                    RuntimeHostDomain::Render,
                )
                .unwrap(),
        );
        black_box(realm.poll_task(task, 64).unwrap());
        drop(realm);
        let _ = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();
    }));

    results.push(bench("nexa_snapshot_read", host_samples, || {
        let (mut realm, module, scope, host) = loaded_hosted(fast.clone());
        let task = call(&mut realm, module, scope, 1);
        let snapshot = realm
            .create_typed_snapshot(
                task,
                nexa_runtime::EncodedSnapshot::copy_i32_slice(
                    nexa_core::StableId::from_name("BenchSnapshot"),
                    nexa_core::StableId::from_name("BenchSnapshot::snapshot-schema"),
                    &[1, 2, 3],
                )
                .unwrap(),
            )
            .unwrap();
        black_box(realm.snapshot_payload(snapshot).unwrap());
        drop(realm);
        let _ = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();
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
        let outcome = black_box(
            realm
                .restart_reload(
                    old,
                    reload_module(),
                    nexa_runtime::RestartReloadPolicy {
                        migration_arguments: vec![RuntimeValue::I32(1)],
                        activation_arguments: Vec::new(),
                        activation_fuel: 64,
                    },
                )
                .unwrap(),
        );
        let nexa_runtime::RestartReloadOutcome::Committed(candidate) = outcome else {
            panic!("benchmark reload must commit");
        };
        assert_eq!(realm.active_root(), Some(candidate));
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
        "{{\"benchmark_version\":2,\"toolchain\":\"{}\",\"os\":\"{}\",\"arch\":\"{}\",\"samples\":{},\
         \"allocation_events\":{{\"admission\":{},\"first_slice\":{},\"promotion\":{},\"resume\":{},\"terminal_cleanup\":{}}},\
         \"fuel_costs\":{{\"fast_complete\":1,\"yield_resume\":1,\"nested_call\":3}},\
         \"signals\":{{\"H1\":\"measured\",\"H2\":\"inconclusive\",\"H3\":\"measured\"}},\
         \"promotion_rate\":1.0,\"gc_measured_separately\":true,\"trace_comparison\":true,\"results\":[{}]}}",
        option_env!("RUSTC_VERSION").unwrap_or("rustc-1.97.1"),
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

fn host_call_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32, ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 3);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 2,
            dst: 2,
        })
        .emit(Instruction::Return { source: 2 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    module.host_import(HostImport {
        stable_id: StableId::from_name("BenchHost::add"),
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: vec![ValueType::I32, ValueType::I32],
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    let function = module.function(function.finish().unwrap());
    module.script_export(ScriptExport {
        stable_id: BINARY_TASK_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn async_host_call_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 3);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::EnumPayload {
            source: 1,
            variant: StableId::from_parts(&["Result", "::Ok"]),
            dst: 2,
        })
        .emit(Instruction::Return { source: 2 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    let async_enum = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let async_result = nexa_bytecode::AsyncResultType {
        result_type: async_enum.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: Some(u32::MAX - 1),
        abandon_error: None,
    };
    module.enum_type(async_enum);
    module.host_import(HostImport {
        stable_id: StableId::from_name("BenchHost::async"),
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: vec![ValueType::I32],
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut function = function.finish().unwrap();
    function.root_bitmap[1] = true;
    function.safepoints = vec![0, 1, 2];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false, false, false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![false, true, false],
        },
        RootMap {
            pc: 2,
            bitmap: vec![false, true, false],
        },
    ];
    let function = module.function(function);
    module.script_export(ScriptExport {
        stable_id: UNARY_TASK_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn build_module(
    export_id: StableId,
    effect: FunctionEffect,
    code: Vec<Instruction>,
    registers: u16,
    parameters: usize,
) -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32; parameters],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), registers);
    function.effect(effect);
    for instruction in code {
        function.emit(instruction);
    }
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    let function = module.function(function.finish().unwrap());
    module.script_export(ScriptExport {
        stable_id: export_id,
        function,
        signature,
        effect,
    });
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn nested_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut caller = FunctionBuilder::new(signature.clone(), 2);
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
    module.metadata(HOST, schema());
    let caller = module.function(caller.finish().unwrap());
    module.script_export(ScriptExport {
        stable_id: UNARY_TASK_EXPORT,
        function: caller,
        signature,
        effect: FunctionEffect::Task,
    });
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
    let mut activation = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    activation
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, schema())
        .function(migration.finish().unwrap());
    module.function(task.finish().unwrap());
    module.function(activation.finish().unwrap());
    verify(module.finish(), VerifierLimits::default()).unwrap()
}

fn loaded(
    verified: VerifiedModule,
) -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    nexa_runtime::ScopeHandle,
) {
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, HOST, schema()).unwrap();
    let scope = realm.create_scope(None).unwrap();
    (realm, module, scope)
}

fn loaded_hosted(
    verified: VerifiedModule,
) -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    nexa_runtime::ScopeHandle,
    RuntimeHost,
) {
    let host = RuntimeHost::new(1_024);
    let mut realm =
        RealmRuntime::hosted(RealmConfig::default(), host.clone(), Box::new(AddRegistry)).unwrap();
    let module = realm.load_module(verified, HOST, schema()).unwrap();
    let scope = realm.create_scope(None).unwrap();
    (realm, module, scope, host)
}

fn call(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    scope: nexa_runtime::ScopeHandle,
    value: i32,
) -> nexa_runtime::TaskHandle {
    realm
        .spawn_task(
            module,
            UNARY_TASK_EXPORT,
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
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn function_authority(&self, id: StableId) -> Option<&HostFunctionAuthority> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            HostFunctionAuthority::new(
                StableId::from_name("BenchHost::add"),
                [0; 32],
                &[ValueType::I32, ValueType::I32],
                Some(ValueType::I32),
                HostCallMode::Immediate,
                1,
                None,
                &[],
            )
        });
        (id == authority.stable_id()).then_some(authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != StableId::from_name("BenchHost::add") {
            return Err(HostTrap::UnknownFunction(id));
        }
        if args.len() != 2 {
            return Err(HostTrap::Arity);
        }
        let (lhs, rhs) = (args.i32(0)?, args.i32(1)?);
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(
            lhs + rhs,
        )))
    }
}

struct AsyncRegistry {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for AsyncRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn function_authority(&self, id: StableId) -> Option<&HostFunctionAuthority> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
            HostFunctionAuthority::new(
                StableId::from_name("BenchHost::async"),
                [0; 32],
                &[ValueType::I32],
                Some(ValueType::Named(result.type_id)),
                HostCallMode::Async,
                1,
                Some(nexa_bytecode::AsyncResultType {
                    result_type: result.type_id,
                    success: ValueType::I32,
                    error: ValueType::I32,
                    cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
                    abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
                    cancel_error: Some(u32::MAX - 1),
                    abandon_error: None,
                }),
                &[],
            )
        });
        (id == authority.stable_id()).then_some(authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != StableId::from_name("BenchHost::async") {
            return Err(HostTrap::UnknownFunction(id));
        }
        if args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::Host("host request admission failed".into()))?;
        let request = pending.request;
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
        Ok(HostCallOutcome::Pending(request))
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
