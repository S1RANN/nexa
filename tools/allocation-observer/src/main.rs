use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nexa_bytecode::{
    AsyncResultType, CancelPolicy, Function, FunctionBuilder, FunctionEffect, HostCallMode,
    HostImport, Instruction, ModuleBuilder, RootMap, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostErrorPayload, HostPayload, HostRegistry, HostTrap, HostValue,
    PendingHostRequest, PendingReason, PollResult, RealmConfig, RealmRuntime, ResourceContext,
    RuntimeHost, RuntimeHostDomain, RuntimeValue, StepConfig, TaskLimits, TickBudget,
};
use nexa_verifier::{VerifierLimits, verify};

struct CountingAllocator;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn observed(operation: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    operation();
    ENABLED.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst)
}

fn main() {
    let mut runs = Vec::new();
    for repeat in 1..=3 {
        let (mut realm, module) = make_realm(vec![
            Instruction::Safepoint,
            Instruction::Yield,
            Instruction::Return { source: 0 },
        ]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let promotion = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Pending(PendingReason::ExplicitYield)
            ));
        });
        let resume = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![Instruction::Return { source: 0 }]);
        realm.set_trace_enabled(false);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let trace_off = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_immediate_host_realm(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let immediate_host_call = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(Some(RuntimeValue::I32(8)))
            ));
        });
        drop(realm);
        host.close().unwrap();

        let host = RuntimeHost::new(16);
        let pending = Arc::new(Mutex::new(None));
        let (mut realm, module) = make_async_host_realm(host.clone(), Arc::clone(&pending));
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let async_admission = observed(|| {
            assert_eq!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Pending(PendingReason::HostRequest)
            );
        });
        pending
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .ticket
            .complete(HostPayload::I32(8))
            .unwrap();
        let success_result_writeback = observed(|| {
            realm
                .tick(TickBudget {
                    max_tasks: 1,
                    frame_fuel_budget: 64,
                    collect_garbage: false,
                })
                .unwrap();
        });
        let failed = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(9)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(failed, 64).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        pending
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .ticket
            .fail(HostErrorPayload { code: 9 })
            .unwrap();
        let error_result_writeback = observed(|| {
            realm
                .tick(TickBudget {
                    max_tasks: 1,
                    frame_fuel_budget: 64,
                    collect_garbage: false,
                })
                .unwrap();
        });
        drop(realm);
        let _releases = host.drain_releases();
        host.close().unwrap();

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        realm
            .create_resource_token(task, RuntimeHostDomain::Render)
            .unwrap();
        let pending = realm.create_host_request(task).unwrap();
        realm.wait_for_request(task, pending.request).unwrap();
        let realm_drop_transfer = observed(|| drop(realm));
        assert_eq!(host.pending_releases(), 2);
        drop(pending);
        assert_eq!(host.drain_releases().len(), 2);
        host.close().unwrap();
        runs.push((
            repeat,
            promotion,
            resume,
            trace_off,
            immediate_host_call,
            async_admission,
            success_result_writeback,
            error_result_writeback,
            realm_drop_transfer,
        ));
    }

    let required_paths_zero = runs
        .iter()
        .all(
            |(
                _,
                promotion,
                resume,
                trace_off,
                immediate_host_call,
                _,
                _,
                _,
                realm_drop_transfer,
            )| {
                *promotion
                    + *resume
                    + *trace_off
                    + *immediate_host_call
                    + *realm_drop_transfer
                    == 0
            },
        );
    let all_measured_paths_zero = runs.iter().all(
        |(
            _,
            promotion,
            resume,
            trace_off,
            immediate_host_call,
            async_admission,
            success_result_writeback,
            error_result_writeback,
            realm_drop_transfer,
        )| {
            *promotion
                + *resume
                + *trace_off
                + *immediate_host_call
                + *async_admission
                + *success_result_writeback
                + *error_result_writeback
                + *realm_drop_transfer
                == 0
        },
    );
    println!(
        "{{\"observer\":\"global_allocator\",\"toolchain\":\"rustc-1.97.1\",\"runs\":[{}],\"allocation_free_contract_paths_zero\":{required_paths_zero},\"all_measured_paths_zero\":{all_measured_paths_zero}}}",
        runs.iter()
            .map(|(repeat, promotion, resume, trace_off, immediate_host_call, async_admission, success_result_writeback, error_result_writeback, realm_drop_transfer)| format!(
                "{{\"repeat\":{repeat},\"promotion\":{promotion},\"resume\":{resume},\"trace_off\":{trace_off},\"immediate_host_call\":{immediate_host_call},\"async_admission\":{async_admission},\"success_result_writeback\":{success_result_writeback},\"error_result_writeback\":{error_result_writeback},\"realm_drop_transfer\":{realm_drop_transfer}}}"
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(
        required_paths_zero,
        "an allocation-free contract path allocated"
    );
}

fn make_async_host_realm(
    host: RuntimeHost,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-async-host");
    let schema = StableId::from_name("allocation-observer-async-schema");
    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let function = Function {
        signature: Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::Named(result.type_id)),
        },
        registers: 2,
        frame_bytes: 16,
        root_bitmap: vec![false, true],
        root_maps: vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
        ],
        safepoints: vec![0, 1],
        loop_bounds: Vec::new(),
        effect: FunctionEffect::Task,
        max_static_call_depth: 1,
        code: vec![
            Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 1,
                dst: 1,
            },
            Instruction::Return { source: 1 },
        ],
    };
    let mut builder = ModuleBuilder::new();
    builder.metadata(host_hash, schema);
    builder.enum_type(result.clone());
    builder.host_import(HostImport {
        stable_id: StableId::from_name("Observer::async_increment"),
        parameters: vec![ValueType::I32],
        result: Some(ValueType::Named(result.type_id)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(AsyncResultType {
            result_type: result.type_id,
            success: ValueType::I32,
            error: ValueType::I32,
            cancel_policy: CancelPolicy::CancelTask,
            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
            cancel_error: None,
            abandon_error: None,
        }),
    });
    builder.function(function);
    let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host,
        Box::new(AsyncHost { host_hash, pending }),
    )
    .unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_immediate_host_realm(
    host: RuntimeHost,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-immediate-host");
    let schema = StableId::from_name("allocation-observer-immediate-schema");
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::Return { source: 1 });
    let mut builder = ModuleBuilder::new();
    builder.metadata(host_hash, schema);
    builder.host_import(HostImport {
        stable_id: StableId::from_name("Observer::increment"),
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    builder.function(function.finish().unwrap());
    let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host,
        Box::new(ImmediateHost(host_hash)),
    )
    .unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_realm_with_host(
    host: RuntimeHost,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host_hash, schema, vec![Instruction::Return { source: 0 }]);
    let mut realm =
        RealmRuntime::hosted(RealmConfig::default(), host, Box::new(NoHost(host_hash))).unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_realm(code: Vec<Instruction>) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host, schema, code);
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    (realm, module)
}

fn build_module(
    host: StableId,
    schema: StableId,
    code: Vec<Instruction>,
) -> nexa_verifier::VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function.effect(FunctionEffect::Task);
    for instruction in code {
        function.emit(instruction);
    }
    let mut builder = ModuleBuilder::new();
    builder
        .metadata(host, schema)
        .function(function.finish().unwrap());
    verify(builder.finish(), VerifierLimits::default()).unwrap()
}

struct NoHost(StableId);

struct ImmediateHost(StableId);

struct AsyncHost {
    host_hash: StableId,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for AsyncHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.host_hash)
    }

    fn call(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 1 || !matches!(args.get(0)?, HostValue::I32(_)) {
            return Err(HostTrap::Type);
        }
        let pending = context
            .create_request()
            .map_err(|error| HostTrap::Host(error.to_string()))?;
        let request = pending.request;
        *self.pending.lock().unwrap() = Some(pending);
        Ok(HostCallOutcome::Pending(request))
    }
}

impl HostRegistry for ImmediateHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let HostValue::I32(value) = args.get(0)? else {
            return Err(HostTrap::Type);
        };
        Ok(HostCallOutcome::Immediate(HostValue::I32(value + 1)))
    }
}

impl HostRegistry for NoHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        _: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
    }
}
