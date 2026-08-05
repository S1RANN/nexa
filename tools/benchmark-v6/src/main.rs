use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder, RootMap,
    ScriptExport, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_migrate::{
    MigrateCheckConfig, StateFixture, StateFixtureField, StateFixtureObject, StateFixtureValue,
    run_migrate_check,
};
use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, ExecutionCharge, FrameLimits, FuelState, Heap,
    HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostPayload, HostRegistry, HostTrap,
    InterpreterOutcome, OpcodeCostTable, PendingHostRequest, RealmConfig, RealmRuntime,
    ResolvedHostFunction, ResourceContext, RuntimeHost, RuntimeHostArgs, RuntimeResourceLedger,
    RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits, TaskPoll, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};
use serde::Serialize;

const DEFAULT_SAMPLES: usize = 1_000;
const SMOKE_SAMPLES: usize = 20;
const WARMUP: usize = 100;
const HOST: StableId = StableId(0x4245_4e43_4848_4f53);
const BENCH_TASK_EXPORT: StableId = StableId(0x4245_4e43_4854_4153);

struct CountingAllocator;

static SYSTEM_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        SYSTEM_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the layout and allocation contract are delegated unchanged to System.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        SYSTEM_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the layout and allocation contract are delegated unchanged to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer/layout pair came from the delegated System allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        SYSTEM_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the pointer/layout pair and requested size are delegated to System.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Observation {
    fuel: u64,
    instructions: u64,
    heap_slots: u64,
    resources: PeakResources,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct PeakResources {
    tasks: u64,
    requests: u64,
    tokens: u64,
    snapshots: u64,
    state_objects: u64,
    retired_modules: u64,
    total: u64,
}

impl PeakResources {
    fn from_ledger(ledger: RuntimeResourceLedger) -> Self {
        Self {
            tasks: ledger.tasks,
            requests: ledger.requests,
            tokens: ledger.tokens,
            snapshots: ledger.snapshots,
            state_objects: ledger.state_objects,
            retired_modules: ledger.retired_modules,
            total: ledger
                .tasks
                .saturating_add(ledger.scopes)
                .saturating_add(ledger.continuations)
                .saturating_add(ledger.scheduler_tokens)
                .saturating_add(ledger.requests)
                .saturating_add(ledger.completion_reservations)
                .saturating_add(ledger.tokens)
                .saturating_add(ledger.snapshots)
                .saturating_add(ledger.release_reservations)
                .saturating_add(ledger.queued_releases)
                .saturating_add(ledger.heap_objects)
                .saturating_add(ledger.state_objects)
                .saturating_add(ledger.retired_modules),
        }
    }

    fn merge(&mut self, other: Self) {
        self.tasks = self.tasks.max(other.tasks);
        self.requests = self.requests.max(other.requests);
        self.tokens = self.tokens.max(other.tokens);
        self.snapshots = self.snapshots.max(other.snapshots);
        self.state_objects = self.state_objects.max(other.state_objects);
        self.retired_modules = self.retired_modules.max(other.retired_modules);
        self.total = self.total.max(other.total);
    }
}

#[derive(Debug, Serialize)]
struct CaseStats {
    case: &'static str,
    samples: usize,
    throughput_ops_per_second: u64,
    mean_ns: u128,
    p50_ns: u128,
    p90_ns: u128,
    p95_ns: u128,
    p99_ns: u128,
    min_ns: u128,
    max_ns: u128,
    standard_deviation_ns: f64,
    coefficient_of_variation: f64,
    frame_1000_calls_ns: u128,
    system_allocations: u64,
    heap_slots_peak: u64,
    fuel_total: u64,
    fuel_per_operation: u64,
    instructions_total: u64,
    instructions_per_operation: u64,
    peak_resources: PeakResources,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    benchmark_version: u32,
    implementation_commit: String,
    toolchain: String,
    os: &'static str,
    arch: &'static str,
    samples: usize,
    allocation_scope: &'static str,
    cases: Vec<CaseStats>,
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let smoke = arguments.iter().any(|argument| argument == "--smoke");
    let samples = argument_value(&arguments, "--samples")
        .map(str::parse)
        .transpose()?
        .unwrap_or(if smoke {
            SMOKE_SAMPLES
        } else {
            DEFAULT_SAMPLES
        });
    if samples == 0 {
        return Err("benchmark samples must be positive".into());
    }

    let language = nexa_compiler::compile(LANGUAGE_SOURCE)?;
    let mut cases = Vec::new();

    cases.push(bench(
        "immediate_call",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 0, &[RuntimeValue::I32(41)], &mut heap, 256),
    ));
    cases.push(bench(
        "result_ok_err",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| {
            let first = run_returned(&language, 1, &[RuntimeValue::I32(7)], &mut heap, 256);
            let second = run_returned(&language, 2, &[], &mut heap, 256);
            combine(first, second, heap.live_len())
        },
    ));
    cases.push(bench(
        "fuel_resume",
        samples,
        || (),
        |()| run_two_slices(&language, 3, false),
    ));
    let explicit = explicit_resume_module();
    cases.push(bench(
        "explicit_resume",
        samples,
        || (),
        |()| run_two_slices(&explicit, 0, true),
    ));
    cases.push(bench(
        "string_concat",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 4, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "struct_construction",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 5, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "class_allocation",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 6, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "enum_construction_match",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 7, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "array_operations",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 8, &[], &mut heap, 512),
    ));
    cases.push(bench(
        "map_operations",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, 9, &[], &mut heap, 512),
    ));
    let buffer_type = language.module().buffer_types[0];
    cases.push(bench(
        "buffer_copy",
        samples,
        || {
            let mut heap = Heap::new_with_limits(64, 4_096, 64);
            let destination = heap
                .allocate_buffer(
                    buffer_type.type_id,
                    buffer_type.element,
                    &[
                        RuntimeValue::I32(1),
                        RuntimeValue::I32(2),
                        RuntimeValue::I32(3),
                    ],
                )
                .expect("destination buffer");
            let source = heap
                .allocate_buffer(
                    buffer_type.type_id,
                    buffer_type.element,
                    &[
                        RuntimeValue::I32(7),
                        RuntimeValue::I32(8),
                        RuntimeValue::I32(9),
                    ],
                )
                .expect("source buffer");
            (heap, destination, source)
        },
        |(mut heap, destination, source)| {
            run_returned(&language, 10, &[destination, source], &mut heap, 512)
        },
    ));

    let fast = fast_module();
    let snapshot_host = RuntimeHost::new(4_096);
    let mut snapshot_realm = RealmRuntime::hosted(
        RealmConfig::default(),
        snapshot_host.clone(),
        Box::new(NullRegistry),
    )?;
    let snapshot_module =
        snapshot_realm.load_module(fast.clone(), HOST, fast.module().state_schema_fingerprint)?;
    let snapshot_scope = snapshot_realm.create_scope(None)?;
    let snapshot_task = call(&mut snapshot_realm, snapshot_module, snapshot_scope, 1)?;
    let snapshot = snapshot_realm.create_typed_snapshot(
        snapshot_task,
        nexa_runtime::EncodedSnapshot::copy_i32_slice(
            StableId::from_name("BenchSnapshot"),
            StableId::from_name("BenchSnapshot::snapshot-schema"),
            &[1_i32, 2, 3, 4],
        )
        .expect("benchmark snapshot encoding is fixed"),
    )?;
    cases.push(bench(
        "snapshot_access",
        samples,
        || (),
        |()| {
            black_box(
                snapshot_realm
                    .snapshot_payload(snapshot)
                    .expect("snapshot data"),
            );
            Observation {
                resources: PeakResources::from_ledger(snapshot_realm.resource_ledger()),
                ..Observation::default()
            }
        },
    ));
    drop(snapshot_realm);
    close_host(&snapshot_host)?;

    let pending = Arc::new(Mutex::new(None));
    let async_host = RuntimeHost::new(8_192);
    let mut async_config = RealmConfig::default();
    async_config.runtime_limits.max_tasks = u32::try_from(samples.max(64))
        .unwrap_or(u32::MAX)
        .saturating_add(8);
    async_config.runtime_limits.max_scheduler_tokens = async_config.runtime_limits.max_tasks;
    async_config.tombstone_capacity = samples.max(64).saturating_add(8);
    let mut async_realm = RealmRuntime::hosted(
        async_config,
        async_host.clone(),
        Box::new(AsyncRegistry {
            pending: Arc::clone(&pending),
        }),
    )?;
    let async_verified = async_module();
    let async_schema = async_verified.module().state_schema_fingerprint;
    let async_module = async_realm.load_module(async_verified, HOST, async_schema)?;
    let async_scope = async_realm.create_scope(None)?;
    let async_instructions = 3;
    cases.push(bench(
        "async_admission",
        samples,
        || (),
        |()| {
            let task =
                call(&mut async_realm, async_module, async_scope, 7).expect("async task admission");
            assert!(matches!(
                async_realm.poll_task(task, 64),
                Ok(TaskPoll::Waiting(_))
            ));
            let peak = PeakResources::from_ledger(async_realm.resource_ledger());
            let mut request = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("pending request");
            request
                .ticket
                .complete(HostPayload::I32(7))
                .expect("host completion");
            async_realm
                .tick(TickBudget::default())
                .expect("async completion tick");
            Observation {
                fuel: async_instructions,
                instructions: async_instructions,
                heap_slots: async_realm.resource_ledger().heap_objects,
                resources: peak,
            }
        },
    ));
    drop(async_realm);
    close_host(&async_host)?;

    let migration = migration_inputs()?;
    cases.push(bench(
        "migration",
        samples,
        || (),
        |()| {
            let result = run_migrate_check(
                &migration.old_bytes,
                &migration.new_bytes,
                &migration.fixture_bytes,
                MigrateCheckConfig::default(),
            )
            .expect("offline migration");
            black_box(result.final_state_hash);
            Observation {
                fuel: result.usage.fuel_used,
                instructions: result.usage.fuel_used,
                heap_slots: result.usage.object_peak as u64,
                resources: PeakResources {
                    state_objects: result.usage.object_peak as u64,
                    total: result.usage.object_peak as u64,
                    ..PeakResources::default()
                },
            }
        },
    ));

    let old_module = migration.old_module.clone();
    let new_module = migration.new_module.clone();
    cases.push(bench(
        "reload_commit",
        samples,
        || prepared_reload(&old_module, &new_module),
        |mut prepared| {
            let before = PeakResources::from_ledger(prepared.realm.resource_ledger());
            let outcome = prepared
                .realm
                .restart_reload(
                    prepared.old,
                    prepared.candidate,
                    nexa_runtime::RestartReloadPolicy::default(),
                )
                .expect("restart reload");
            assert!(matches!(
                outcome,
                nexa_runtime::RestartReloadOutcome::Committed(_)
            ));
            let after = prepared.realm.resource_ledger();
            let mut resources = before;
            resources.merge(PeakResources::from_ledger(after));
            Observation {
                fuel: 1,
                instructions: 1,
                heap_slots: after.heap_objects,
                resources,
            }
        },
    ));

    cases.push(bench(
        "realm_drop",
        samples,
        || {
            let mut realm = RealmRuntime::isolated(RealmConfig::default());
            let module = realm
                .load_module(fast.clone(), HOST, fast.module().state_schema_fingerprint)
                .expect("drop module");
            let scope = realm.create_scope(None).expect("drop scope");
            let task = call(&mut realm, module, scope, 1).expect("drop task");
            assert!(matches!(realm.poll_task(task, 0), Ok(TaskPoll::Yielded(_))));
            let peak = PeakResources::from_ledger(realm.resource_ledger());
            (realm, peak)
        },
        |(realm, peak)| {
            drop(realm);
            Observation {
                resources: peak,
                ..Observation::default()
            }
        },
    ));

    let report = BenchmarkReport {
        benchmark_version: 6,
        implementation_commit: git_commit(),
        toolchain: rustc_version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        samples,
        allocation_scope: "timed operation only; per-sample setup and result storage excluded",
        cases,
    };
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = argument_value(&arguments, "--output") {
        std::fs::write(path, format!("{rendered}\n"))?;
    }
    println!("{rendered}");
    Ok(())
}

const LANGUAGE_SOURCE: &str = r#"
enum BenchError { Failed, }
struct BenchStruct { value: i32, wide: i64, label: string, }
class BenchClass { mut value: i32, next: Option<BenchClass>, }
enum BenchEvent { Idle, Value(i32), }

@immediate
fn immediate_call(value: i32) -> i32 { return value + 1; }
fn result_ok(value: i32) -> Result<i32, BenchError> { return Result::Ok(value); }
fn result_err() -> Result<i32, BenchError> { return Result::Err(BenchError::Failed); }
async fn fuel_work(value: i32) -> i32 {
    let first: i32 = value + 1;
    let second: i32 = first + 1;
    let third: i32 = second + 1;
    return third;
}
fn string_concat() -> i32 {
    let value: string = "nexa" + "-benchmark";
    return value.byte_len();
}
fn struct_construction() -> i32 {
    let value: BenchStruct = BenchStruct { value: 7, wide: 9, label: "bench" };
    return value.value;
}
fn class_allocation() -> i32 {
    let value: BenchClass = new BenchClass { value: 7, next: Option::None };
    value.value = value.value + 1;
    return value.value;
}
fn enum_match() -> i32 {
    let event: BenchEvent = BenchEvent::Value(7);
    return match event {
        BenchEvent::Idle => 0,
        BenchEvent::Value(value) => value,
    };
}
fn array_operations() -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    values.push(2);
    values.set(0, 3);
    return values.get(0) + values.len();
}
fn map_operations() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(1) {
        Option::Some(value) => value.byte_len(),
        Option::None => 0,
    };
}
fn buffer_copy(destination: Buffer<i32>, source: Buffer<i32>) -> i32 {
    destination.copy(source, 0, 0, 3);
    return destination.get(2);
}
"#;

fn run_returned(
    module: &VerifiedModule,
    function: u32,
    arguments: &[RuntimeValue],
    heap: &mut Heap,
    fuel: u64,
) -> Observation {
    let outcome = CheckedInterpreter::run_with_heap(module, function, arguments, fuel, heap)
        .expect("verified benchmark function");
    let InterpreterOutcome::Returned { charge, value, .. } = outcome else {
        panic!("benchmark function did not return");
    };
    black_box(value);
    observation(charge, heap.live_len())
}

fn run_two_slices(module: &VerifiedModule, function: u32, explicit: bool) -> Observation {
    let limits = FrameLimits::default();
    let continuation = CheckedInterpreter::start(
        module,
        function,
        &[RuntimeValue::I32(7)],
        limits,
        ContinuationReservation::for_limits(limits),
    )
    .expect("benchmark continuation");
    let first_fuel = if explicit { 64 } else { 1 };
    let first = CheckedInterpreter::poll(
        module,
        continuation,
        FuelState::new(first_fuel, 0, 1_024),
        &OpcodeCostTable::default(),
    )
    .expect("first benchmark slice");
    let InterpreterOutcome::Suspended {
        continuation,
        charge: first_charge,
        fuel,
        ..
    } = first
    else {
        panic!("benchmark function did not suspend");
    };
    let second = CheckedInterpreter::poll(
        module,
        continuation,
        FuelState::new(64, fuel.cumulative_used, 1_024),
        &OpcodeCostTable::default(),
    )
    .expect("second benchmark slice");
    let InterpreterOutcome::Returned {
        charge: second_charge,
        value,
        ..
    } = second
    else {
        panic!("benchmark function did not return after resume");
    };
    black_box(value);
    Observation {
        fuel: first_charge
            .fuel_used
            .saturating_add(second_charge.fuel_used),
        instructions: first_charge
            .instructions
            .saturating_add(second_charge.instructions),
        ..Observation::default()
    }
}

fn observation(charge: ExecutionCharge, heap_slots: usize) -> Observation {
    Observation {
        fuel: charge.fuel_used,
        instructions: charge.instructions,
        heap_slots: u64::try_from(heap_slots).unwrap_or(u64::MAX),
        ..Observation::default()
    }
}

fn combine(first: Observation, second: Observation, heap_slots: usize) -> Observation {
    let mut resources = first.resources;
    resources.merge(second.resources);
    Observation {
        fuel: first.fuel.saturating_add(second.fuel),
        instructions: first.instructions.saturating_add(second.instructions),
        heap_slots: u64::try_from(heap_slots).unwrap_or(u64::MAX),
        resources,
    }
}

fn explicit_resume_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, nexa_bytecode::StateSchema::default().fingerprint());
    let function = module.function(function.finish().expect("explicit resume function"));
    module.script_export(ScriptExport {
        stable_id: BENCH_TASK_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("explicit resume module")
}

fn fast_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, nexa_bytecode::StateSchema::default().fingerprint());
    let function = module.function(function.finish().expect("fast function"));
    module.script_export(ScriptExport {
        stable_id: BENCH_TASK_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("fast module")
}

fn async_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    // Register 0 is the scalar parameter, 1..3 is the two-slot
    // `Result<i32, i32>`, and register 3 receives its extracted payload.
    // The payload destination must not overlap the verified aggregate range.
    let mut function = FunctionBuilder::new(signature.clone(), 4);
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
            dst: 3,
        })
        .emit(Instruction::Return { source: 3 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, nexa_bytecode::StateSchema::default().fingerprint());
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
        stable_id: StableId::from_name("BenchHost::value"),
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: vec![ValueType::I32],
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut function = function.finish().expect("async function");
    function.safepoints = vec![0, 1, 2];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false, false, false, false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![false, false, false, false],
        },
        RootMap {
            pc: 2,
            bitmap: vec![false, false, false, false],
        },
    ];
    let function = module.function(function);
    module.script_export(ScriptExport {
        stable_id: BENCH_TASK_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("async module")
}

struct NullRegistry;

impl HostRegistry for NullRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn resolve_function(&self, _: StableId) -> Option<ResolvedHostFunction<'_>> {
        None
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::InvalidFunctionSlot(slot))
    }
}

struct AsyncRegistry {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for AsyncRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
            HostFunctionAuthority::new(
                StableId::from_name("BenchHost::value"),
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
        (id == authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            authority,
        ))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        if args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let request = context
            .create_request()
            .map_err(|_| HostTrap::Host("benchmark request admission failed".into()))?;
        let handle = request.request;
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
        Ok(HostCallOutcome::Pending(handle))
    }
}

fn call(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    scope: nexa_runtime::ScopeHandle,
    value: i32,
) -> Result<nexa_runtime::TaskHandle, nexa_runtime::RealmError> {
    realm
        .spawn_task(
            module,
            BENCH_TASK_EXPORT,
            &[RuntimeValue::I32(value)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )
        .map_err(Into::into)
}

fn close_host(host: &RuntimeHost) -> Result<(), Box<dyn std::error::Error>> {
    let _ = host.drain_releases();
    let _ = host.begin_close();
    host.try_finish_close()?;
    Ok(())
}

struct MigrationInputs {
    old_module: VerifiedModule,
    new_module: VerifiedModule,
    old_bytes: Vec<u8>,
    new_bytes: Vec<u8>,
    fixture_bytes: Vec<u8>,
}

fn migration_inputs() -> Result<MigrationInputs, Box<dyn std::error::Error>> {
    let old_module = nexa_compiler::compile_with_contract_id(MIGRATION_V1, HOST)?;
    let new_module = nexa_compiler::compile_with_contract_id(MIGRATION_V2, HOST)?;
    let state_ids = bench_state_ids(&old_module, &new_module);
    let fixture = StateFixture {
        format_version: nexa_migrate::STATE_FIXTURE_FORMAT_VERSION,
        stateful_domain: 1,
        objects: vec![StateFixtureObject {
            stable_id: StableId::from_name("bench").0,
            type_id: state_ids.ty.0,
            generation: 1,
            fields: vec![
                StateFixtureField {
                    stable_id: state_ids.value_field.0,
                    value: StateFixtureValue::I32 { value: 7 },
                },
                StateFixtureField {
                    stable_id: state_ids.legacy_field.0,
                    value: StateFixtureValue::I32 { value: 9 },
                },
            ],
        }],
    };
    Ok(MigrationInputs {
        old_bytes: old_module.module().encode(),
        new_bytes: new_module.module().encode(),
        fixture_bytes: serde_json::to_vec(&fixture)?,
        old_module,
        new_module,
    })
}

const MIGRATION_V1: &str = r#"
@state(version = 1)
class BenchState { mut value: i32, mut legacy: i32, }
async fn update(value: i32) -> i32 { return value; }
"#;

const MIGRATION_V2: &str = r#"
@state(version = 2)
class BenchState { mut value: i32, mut total: i32, }
@migration
pub fn migrate() -> bool {
    let old_state: BenchState = old.get<BenchState>(bench);
    let old_value: i32 = old.field<i32>(old_state, BenchState::value);
    let state: BenchState = new.create<BenchState>(bench);
    new.set(state, BenchState::value, old_value);
    new.set(state, BenchState::total, 1);
    replace(bench, state);
    finish_migration();
    return true;
}
async fn update(value: i32) -> i32 { return value + 1; }
@activation
pub fn activate() -> bool { return true; }
"#;

#[derive(Clone, Copy)]
struct BenchStateIds {
    ty: StableId,
    value_field: StableId,
    legacy_field: StableId,
}

fn bench_state_ids(old: &VerifiedModule, new: &VerifiedModule) -> BenchStateIds {
    let old_state = old
        .module()
        .state_schema
        .types
        .first()
        .expect("old benchmark state type");
    let new_state = new
        .module()
        .state_schema
        .types
        .first()
        .expect("new benchmark state type");
    let retained = |field: &&nexa_bytecode::StateField| {
        new_state
            .fields
            .iter()
            .any(|candidate| candidate.stable_id == field.stable_id)
    };
    BenchStateIds {
        ty: old_state.stable_id,
        value_field: old_state
            .fields
            .iter()
            .find(retained)
            .expect("retained benchmark value field")
            .stable_id,
        legacy_field: old_state
            .fields
            .iter()
            .find(|field| !retained(field))
            .expect("removed benchmark legacy field")
            .stable_id,
    }
}

struct PreparedReload {
    realm: RealmRuntime,
    old: nexa_runtime::ModuleHandle,
    candidate: VerifiedModule,
}

fn prepared_reload(old: &VerifiedModule, new: &VerifiedModule) -> PreparedReload {
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let state_ids = bench_state_ids(old, new);
    let old_handle = realm
        .load_module(old.clone(), HOST, old.module().state_schema_fingerprint)
        .expect("old reload module");
    realm
        .insert_state(
            old_handle,
            StableId::from_name("bench"),
            StateValue::Object(StateObject {
                type_id: state_ids.ty,
                version: 1,
                fields: std::collections::BTreeMap::from([
                    (state_ids.value_field, StateValue::I32(7)),
                    (state_ids.legacy_field, StateValue::I32(9)),
                ]),
            }),
        )
        .expect("old reload state");
    PreparedReload {
        realm,
        old: old_handle,
        candidate: new.clone(),
    }
}

fn bench<T>(
    name: &'static str,
    samples: usize,
    mut prepare: impl FnMut() -> T,
    mut operation: impl FnMut(T) -> Observation,
) -> CaseStats {
    for _ in 0..WARMUP.min(samples) {
        let input = prepare();
        black_box(operation(input));
    }
    let mut durations = Vec::with_capacity(samples);
    let mut allocations = 0_u64;
    let mut fuel = 0_u64;
    let mut instructions = 0_u64;
    let mut heap_slots = 0_u64;
    let mut resources = PeakResources::default();
    for _ in 0..samples {
        let input = prepare();
        let allocation_start = SYSTEM_ALLOCATIONS.load(Ordering::Relaxed);
        let started = Instant::now();
        let observation = black_box(operation(input));
        let elapsed = started.elapsed();
        let allocation_end = SYSTEM_ALLOCATIONS.load(Ordering::Relaxed);
        allocations = allocations.saturating_add(allocation_end.saturating_sub(allocation_start));
        durations.push(elapsed);
        fuel = fuel.saturating_add(observation.fuel);
        instructions = instructions.saturating_add(observation.instructions);
        heap_slots = heap_slots.max(observation.heap_slots);
        resources.merge(observation.resources);
    }
    durations.sort_unstable();
    let total = durations.iter().sum::<Duration>().as_nanos().max(1);
    let sample_count = u64::try_from(samples).unwrap_or(u64::MAX);
    let mean_ns = total / samples as u128;
    let variance = durations
        .iter()
        .map(|duration| {
            let delta = duration.as_nanos() as f64 - mean_ns as f64;
            delta * delta
        })
        .sum::<f64>()
        / samples as f64;
    let standard_deviation_ns = variance.sqrt();
    let stats = CaseStats {
        case: name,
        samples,
        throughput_ops_per_second: u64::try_from(
            (samples as u128).saturating_mul(1_000_000_000) / total,
        )
        .unwrap_or(u64::MAX),
        mean_ns,
        p50_ns: percentile(&durations, 50),
        p90_ns: percentile(&durations, 90),
        p95_ns: percentile(&durations, 95),
        p99_ns: percentile(&durations, 99),
        min_ns: durations.first().map_or(0, Duration::as_nanos),
        max_ns: durations.last().map_or(0, Duration::as_nanos),
        standard_deviation_ns,
        coefficient_of_variation: standard_deviation_ns / mean_ns.max(1) as f64,
        frame_1000_calls_ns: mean_ns.saturating_mul(1_000),
        system_allocations: allocations,
        heap_slots_peak: heap_slots,
        fuel_total: fuel,
        fuel_per_operation: fuel / sample_count,
        instructions_total: instructions,
        instructions_per_operation: instructions / sample_count,
        peak_resources: resources,
    };
    eprintln!(
        "{}: {} ops/s, p50={}ns, p95={}ns, p99={}ns, allocs={}",
        stats.case,
        stats.throughput_ops_per_second,
        stats.p50_ns,
        stats.p95_ns,
        stats.p99_ns,
        stats.system_allocations
    );
    stats
}

fn percentile(samples: &[Duration], percentile: usize) -> u128 {
    samples[(samples.len() - 1) * percentile / 100].as_nanos()
}

fn argument_value<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].as_str())
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}
