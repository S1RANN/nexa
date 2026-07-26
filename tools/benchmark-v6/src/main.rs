#![allow(deprecated)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder, RootMap,
    Signature, ValueType,
};
use nexa_core::StableId;
use nexa_migrate::{
    MigrateCheckConfig, StateFixture, StateFixtureField, StateFixtureObject, StateFixtureValue,
    run_migrate_check,
};
use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, ExecutionCharge, FrameLimits, FuelState, Heap,
    HostArgs, HostCallOutcome, HostPayload, HostRegistry, HostTrap, InterpreterOutcome,
    OpcodeCostTable, PendingHostRequest, PollResult, RealmConfig, RealmRuntime, ResourceContext,
    RuntimeHost, RuntimeResourceLedger, RuntimeValue, StateObject, StateValue, StepConfig,
    TaskLimits, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};
use serde::Serialize;

const DEFAULT_SAMPLES: usize = 1_000;
const SMOKE_SAMPLES: usize = 20;
const WARMUP: usize = 25;
const HOST: StableId = StableId(0x4245_4e43_4848_4f53);
const SCHEMA_V1: StableId = StableId(0x4245_4e43_4853_4331);
const SCHEMA_V2: StableId = StableId(0x4245_4e43_4853_4332);

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
    retired_epochs: u64,
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
            retired_epochs: ledger.retired_epochs,
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
                .saturating_add(ledger.retired_epochs),
        }
    }

    fn merge(&mut self, other: Self) {
        self.tasks = self.tasks.max(other.tasks);
        self.requests = self.requests.max(other.requests);
        self.tokens = self.tokens.max(other.tokens);
        self.snapshots = self.snapshots.max(other.snapshots);
        self.state_objects = self.state_objects.max(other.state_objects);
        self.retired_epochs = self.retired_epochs.max(other.retired_epochs);
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
    p95_ns: u128,
    p99_ns: u128,
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
    let snapshot_module = snapshot_realm.load_module(fast.clone(), HOST, SCHEMA_V1)?;
    let snapshot_scope = snapshot_realm.create_scope(None)?;
    let snapshot_task = call(&mut snapshot_realm, snapshot_module, snapshot_scope, 1)?;
    let snapshot = snapshot_realm.create_snapshot(
        snapshot_task,
        StableId::from_name("BenchSnapshot"),
        Arc::from([1_i32, 2, 3, 4]),
    )?;
    cases.push(bench(
        "snapshot_access",
        samples,
        || (),
        |()| {
            black_box(
                snapshot_realm
                    .snapshot_data(snapshot)
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
    let async_module = async_realm.load_module(async_module(), HOST, SCHEMA_V1)?;
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
                Ok(PollResult::Pending(
                    nexa_runtime::PendingReason::HostRequest
                ))
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
        samples.min(200),
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
        samples.min(200),
        || prepared_reload(&old_module, &new_module),
        |mut prepared| {
            let before = PeakResources::from_ledger(prepared.realm.resource_ledger());
            let active = prepared
                .realm
                .commit_reload(&[], 4_096)
                .expect("reload commit");
            assert_eq!(active, prepared.candidate);
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
        samples.min(200),
        || {
            let mut realm = RealmRuntime::isolated(RealmConfig::default());
            let module = realm
                .load_module(fast.clone(), HOST, SCHEMA_V1)
                .expect("drop module");
            let scope = realm.create_scope(None).expect("drop scope");
            let task = call(&mut realm, module, scope, 1).expect("drop task");
            assert!(matches!(
                realm.poll_task(task, 0),
                Ok(PollResult::Pending(_))
            ));
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
enum BenchError { Failed }
struct BenchStruct { value: i32; wide: i64; label: string; }
class BenchClass { value: i32; next: Option<BenchClass>; }
enum BenchEvent { Idle, Value(i32) }

immediate fn immediate_call(value: i32) -> i32 { return value + 1; }
fn result_ok(value: i32) -> Result<i32, BenchError> { return Ok(value); }
fn result_err() -> Result<i32, BenchError> { return Err(Failed); }
task fn fuel_work(value: i32) -> i32 {
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
    let value: BenchClass = new BenchClass { value: 7, next: None };
    value.value = value.value + 1;
    return value.value;
}
fn enum_match() -> i32 {
    let event: BenchEvent = Value(7);
    return match event { Idle => 0, Value(value) => value };
}
fn array_operations() -> i32 {
    let values: Array<i32> = Array.new<i32>();
    values.push(1);
    values.push(2);
    values.set(0, 3);
    return values.get(0) + values.len();
}
fn map_operations() -> i32 {
    let values: Map<i32, string> = Map.new<i32, string>();
    values.set(1, "one");
    return match values.get(1) {
        Some(value) => value.byte_len(),
        None => 0,
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
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, SCHEMA_V1);
    module.function(function.finish().expect("explicit resume function"));
    verify(module.finish(), VerifierLimits::default()).expect("explicit resume module")
}

fn fast_module() -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, SCHEMA_V1);
    module.function(function.finish().expect("fast function"));
    verify(module.finish(), VerifierLimits::default()).expect("fast module")
}

fn async_module() -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        3,
    );
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
    module.metadata(HOST, SCHEMA_V1);
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
        parameters: vec![ValueType::I32],
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut function = function.finish().expect("async function");
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
    module.function(function);
    verify(module.finish(), VerifierLimits::default()).expect("async module")
}

struct NullRegistry;

impl HostRegistry for NullRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn call(
        &mut self,
        _: u32,
        _: &mut ResourceContext<'_>,
        _: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(u32::MAX))
    }
}

struct AsyncRegistry {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for AsyncRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn call(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 1 {
            return Err(HostTrap::UnknownFunction(id));
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
    realm.call(
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
    let old_module = nexa_compiler::compile_with_metadata(MIGRATION_V1, HOST, SCHEMA_V1)?;
    let new_module = nexa_compiler::compile_with_metadata(MIGRATION_V2, HOST, SCHEMA_V2)?;
    let fixture = StateFixture {
        format_version: nexa_migrate::STATE_FIXTURE_FORMAT_VERSION,
        stateful_domain: 1,
        objects: vec![StateFixtureObject {
            stable_id: StableId::from_name("bench").0,
            type_id: StableId::from_name("BenchState").0,
            generation: 1,
            fields: vec![
                StateFixtureField {
                    stable_id: StableId::from_name("BenchState::value").0,
                    value: StateFixtureValue::I32 { value: 7 },
                },
                StateFixtureField {
                    stable_id: StableId::from_name("BenchState::legacy").0,
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
@stateful(1) class BenchState { value: i32; legacy: i32; }
task fn update(value: i32) -> i32 { return value; }
"#;

const MIGRATION_V2: &str = r#"
@stateful(2) class BenchState { value: i32; total: i32; }
migration fn migrate() -> bool {
    let old_state: BenchState = old.get<BenchState>(bench);
    let old_value: i32 = old.field<i32>(old_state, BenchState.value);
    let state: BenchState = new.create<BenchState>(bench);
    new.set(state, BenchState.value, old_value);
    new.set(state, BenchState.total, 1);
    replace(bench, state);
    finish_migration();
    return true;
}
task fn update(value: i32) -> i32 { return value + 1; }
@activation fn activate() -> bool { return true; }
"#;

struct PreparedReload {
    realm: RealmRuntime,
    candidate: nexa_runtime::ModuleHandle,
}

fn prepared_reload(old: &VerifiedModule, new: &VerifiedModule) -> PreparedReload {
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old_handle = realm
        .load_module(old.clone(), HOST, SCHEMA_V1)
        .expect("old reload module");
    realm
        .insert_state(
            old_handle,
            StableId::from_name("bench"),
            StateValue::Object(StateObject {
                type_id: StableId::from_name("BenchState"),
                version: 1,
                fields: std::collections::BTreeMap::from([
                    (StableId::from_name("BenchState::value"), StateValue::I32(7)),
                    (
                        StableId::from_name("BenchState::legacy"),
                        StateValue::I32(9),
                    ),
                ]),
            }),
        )
        .expect("old reload state");
    let candidate = realm
        .prepare_reload(old_handle, new.clone(), HOST)
        .expect("prepare reload");
    realm.quiesce_reload().expect("quiesce reload");
    realm.stage_reload(&[]).expect("stage reload");
    PreparedReload { realm, candidate }
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
    let stats = CaseStats {
        case: name,
        samples,
        throughput_ops_per_second: u64::try_from(
            (samples as u128).saturating_mul(1_000_000_000) / total,
        )
        .unwrap_or(u64::MAX),
        mean_ns: total / samples as u128,
        p50_ns: percentile(&durations, 50),
        p95_ns: percentile(&durations, 95),
        p99_ns: percentile(&durations, 99),
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
