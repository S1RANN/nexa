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
    CheckedInterpreter, ContinuationReservation, ExecutableModule, ExecutionCharge, FrameLimits,
    FuelState, GcBudget, Heap, HostCallOutcome, HostFunctionAuthority, HostPayload, HostRegistry,
    HostTrap, InterpreterOutcome, Object, OpcodeCostTable, PendingHostRequest, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostArgs, RuntimeResourceLedger,
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
static SYSTEM_REALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static SYSTEM_ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static SYSTEM_REALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static SYSTEM_OUTSTANDING_BYTES: AtomicU64 = AtomicU64::new(0);
static SYSTEM_PEAK_OUTSTANDING_BYTES: AtomicU64 = AtomicU64::new(0);

fn record_outstanding_growth(bytes: usize) {
    let outstanding = SYSTEM_OUTSTANDING_BYTES
        .fetch_add(bytes as u64, Ordering::Relaxed)
        .saturating_add(bytes as u64);
    SYSTEM_PEAK_OUTSTANDING_BYTES.fetch_max(outstanding, Ordering::Relaxed);
}

fn record_outstanding_shrink(bytes: usize) {
    let _ = SYSTEM_OUTSTANDING_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(bytes as u64))
    });
}

/// Snapshot of the WP12 allocator counters delimiting one measured region.
#[derive(Clone, Copy, Debug, Default)]
struct AllocationRegion {
    allocations: u64,
    reallocations: u64,
    allocated_bytes: u64,
    reallocated_bytes: u64,
    outstanding_bytes: u64,
}

impl AllocationRegion {
    fn begin() -> Self {
        // The benchmark harness is single-threaded; re-anchoring the peak at
        // the current outstanding level scopes it to the measured region.
        let outstanding = SYSTEM_OUTSTANDING_BYTES.load(Ordering::Relaxed);
        SYSTEM_PEAK_OUTSTANDING_BYTES.store(outstanding, Ordering::Relaxed);
        Self {
            allocations: SYSTEM_ALLOCATIONS.load(Ordering::Relaxed),
            reallocations: SYSTEM_REALLOCATIONS.load(Ordering::Relaxed),
            allocated_bytes: SYSTEM_ALLOCATED_BYTES.load(Ordering::Relaxed),
            reallocated_bytes: SYSTEM_REALLOCATED_BYTES.load(Ordering::Relaxed),
            outstanding_bytes: outstanding,
        }
    }

    fn end(self) -> AllocationDelta {
        AllocationDelta {
            allocations: SYSTEM_ALLOCATIONS
                .load(Ordering::Relaxed)
                .saturating_sub(self.allocations),
            reallocations: SYSTEM_REALLOCATIONS
                .load(Ordering::Relaxed)
                .saturating_sub(self.reallocations),
            allocated_bytes: SYSTEM_ALLOCATED_BYTES
                .load(Ordering::Relaxed)
                .saturating_sub(self.allocated_bytes),
            reallocated_bytes: SYSTEM_REALLOCATED_BYTES
                .load(Ordering::Relaxed)
                .saturating_sub(self.reallocated_bytes),
            peak_outstanding_bytes: SYSTEM_PEAK_OUTSTANDING_BYTES
                .load(Ordering::Relaxed)
                .saturating_sub(self.outstanding_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AllocationDelta {
    allocations: u64,
    reallocations: u64,
    allocated_bytes: u64,
    reallocated_bytes: u64,
    peak_outstanding_bytes: u64,
}

impl AllocationDelta {
    fn accumulate(&mut self, other: Self) {
        self.allocations = self.allocations.saturating_add(other.allocations);
        self.reallocations = self.reallocations.saturating_add(other.reallocations);
        self.allocated_bytes = self.allocated_bytes.saturating_add(other.allocated_bytes);
        self.reallocated_bytes = self
            .reallocated_bytes
            .saturating_add(other.reallocated_bytes);
        self.peak_outstanding_bytes = self
            .peak_outstanding_bytes
            .max(other.peak_outstanding_bytes);
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        SYSTEM_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        SYSTEM_ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        record_outstanding_growth(layout.size());
        // SAFETY: the layout and allocation contract are delegated unchanged to System.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        SYSTEM_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        SYSTEM_ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        record_outstanding_growth(layout.size());
        // SAFETY: the layout and allocation contract are delegated unchanged to System.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_outstanding_shrink(layout.size());
        // SAFETY: the pointer/layout pair came from the delegated System allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        SYSTEM_REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        SYSTEM_REALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
        record_outstanding_shrink(layout.size());
        record_outstanding_growth(size);
        // SAFETY: the pointer/layout pair and requested size are delegated to System.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Observation {
    fuel: u64,
    instructions: u64,
    heap_slots: u64,
    vm: Option<nexa_runtime::VmAllocationCounters>,
    gc: Option<GcObservation>,
    resources: PeakResources,
}

/// Per-sample incremental GC evidence (stage G).
#[derive(Clone, Copy, Debug, Default)]
struct GcObservation {
    completed_cycles: u64,
    objects_reclaimed: u64,
    bytes_reclaimed: u64,
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
    tier: &'static str,
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
    system_reallocations: u64,
    system_allocated_bytes: u64,
    system_reallocated_bytes: u64,
    system_peak_outstanding_bytes: u64,
    vm: VmCounters,
    gc: GcCounters,
    fuel_total: u64,
    fuel_per_operation: u64,
    instructions_total: u64,
    instructions_per_operation: u64,
    peak_resources: PeakResources,
}

/// WP13 runtime counters. Fields the runtime does not expose yet are `null`
/// so reports never fake zeros for unimplemented instrumentation.
#[derive(Debug, Default, Serialize)]
struct VmCounters {
    live_heap_slots_peak: u64,
    allocations: Option<u64>,
    string_allocations: Option<u64>,
    class_allocations: Option<u64>,
    collection_storage_allocations: Option<u64>,
    map_slot_allocations: Option<u64>,
    struct_materializations: Option<u64>,
    enum_materializations: Option<u64>,
    allocated_bytes: Option<u64>,
    live_bytes: Option<u64>,
    bytes_copied: Option<u64>,
}

impl VmCounters {
    fn from_totals(
        live_heap_slots_peak: u64,
        totals: Option<nexa_runtime::VmAllocationCounters>,
    ) -> Self {
        let Some(totals) = totals else {
            return Self {
                live_heap_slots_peak,
                ..Self::default()
            };
        };
        Self {
            live_heap_slots_peak,
            allocations: Some(totals.object_allocations),
            string_allocations: Some(totals.string_allocations),
            class_allocations: Some(totals.class_allocations),
            collection_storage_allocations: Some(totals.collection_storage_allocations),
            map_slot_allocations: Some(totals.map_slot_allocations),
            struct_materializations: Some(totals.struct_materializations),
            enum_materializations: Some(totals.enum_materializations),
            // Precise per-kind heap byte accounting is stage-G work (WP71).
            allocated_bytes: None,
            live_bytes: None,
            bytes_copied: Some(
                totals
                    .collection_relocation_bytes
                    .saturating_add(totals.string_copy_bytes),
            ),
        }
    }
}

/// GC v1 telemetry lands with stage G; until then every field is `null`.
#[derive(Debug, Default, Serialize)]
struct GcCounters {
    cycles: Option<u64>,
    pause_ns_max: Option<u64>,
    /// Object-count reclamation next to the exact payload-byte figure
    /// reported by the G4 sweep accounting.
    objects_reclaimed: Option<u64>,
    bytes_reclaimed: Option<u64>,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    schema: u32,
    benchmark_version: u32,
    implementation_commit: String,
    benchmark_source_hash: String,
    bytecode_hash: String,
    toolchain: String,
    os: &'static str,
    arch: &'static str,
    cpu_model: String,
    logical_cpu_count: usize,
    build_profile: &'static str,
    samples: usize,
    warmup: usize,
    process_index: usize,
    started_at_unix_ms: u128,
    profiler_enabled: bool,
    profiler: Option<ProfilerSummary>,
    allocation_scope: &'static str,
    cases: Vec<CaseStats>,
}

/// WP15/WP16 profiler evidence attached to profiled runs.
#[derive(Debug, Serialize)]
struct ProfilerSummary {
    host_calls: u64,
    function_count: usize,
    allocation_site_count: usize,
    dropped_functions: u64,
    dropped_sites: u64,
    top_opcodes: Vec<(String, u64)>,
    top_allocation_sites: Vec<AllocationSiteSummary>,
}

#[derive(Debug, Serialize)]
struct AllocationSiteSummary {
    function: u32,
    pc: u32,
    opcode: u16,
    type_id: u64,
    count: u64,
}

fn profiler_summary(enabled: bool) -> Option<ProfilerSummary> {
    if !enabled {
        return None;
    }
    nexa_runtime::profiler::disable();
    let report = nexa_runtime::profiler::take_thread_report()?;
    let mut opcodes = report
        .opcodes
        .iter()
        .map(|entry| (entry.opcode.to_owned(), entry.executions))
        .collect::<Vec<_>>();
    opcodes.sort_by_key(|(_, executions)| std::cmp::Reverse(*executions));
    opcodes.truncate(5);
    let mut sites = report.allocation_sites.clone();
    sites.sort_by_key(|site| std::cmp::Reverse(site.count));
    sites.truncate(5);
    Some(ProfilerSummary {
        host_calls: report.host_calls,
        function_count: report.functions.len(),
        allocation_site_count: report.allocation_sites.len(),
        dropped_functions: report.dropped_functions,
        dropped_sites: report.dropped_sites,
        top_opcodes: opcodes,
        top_allocation_sites: sites
            .into_iter()
            .map(|site| AllocationSiteSummary {
                function: site.function,
                pc: site.pc,
                opcode: site.opcode,
                type_id: site.type_id,
                count: site.count,
            })
            .collect(),
    })
}

/// Median-of-process-medians aggregate written by the multi-process driver.
#[derive(Debug, Serialize)]
struct AggregateReport {
    schema: u32,
    benchmark_version: u32,
    protocol: &'static str,
    process_count: usize,
    samples_per_process: usize,
    implementation_commit: String,
    benchmark_source_hash: String,
    cases: Vec<AggregateCase>,
}

#[derive(Debug, Serialize)]
struct AggregateCase {
    case: String,
    tier: String,
    median_throughput_ops_per_second: u64,
    median_p50_ns: u128,
    median_p95_ns: u128,
    median_p99_ns: u128,
    max_system_allocations: u64,
    max_system_allocated_bytes: u64,
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
    let process_index = argument_value(&arguments, "--process-index")
        .map(str::parse)
        .transpose()?
        .unwrap_or(0_usize);
    let processes = argument_value(&arguments, "--processes")
        .map(str::parse)
        .transpose()?
        .unwrap_or(1_usize);
    let profiler_enabled = arguments.iter().any(|argument| argument == "--profile");
    if processes > 1 {
        return run_multi_process(&arguments, processes, samples);
    }
    if profiler_enabled {
        nexa_runtime::profiler::enable();
    }
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();

    let language = nexa_compiler::compile(LANGUAGE_SOURCE)?;
    // Stage F: rows are built once at load, exactly like realm admission.
    let language_rows = ExecutableModule::build(&language, &OpcodeCostTable::default())?;
    let bytecode_hash = blake3::hash(&language.module().encode())
        .to_hex()
        .to_string();
    let mut cases = Vec::new();

    cases.push(bench(
        "immediate_call",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| {
            run_returned(
                &language,
                &language_rows,
                0,
                &[RuntimeValue::I32(41)],
                &mut heap,
                256,
            )
        },
    ));
    cases.push(bench(
        "result_ok_err",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| {
            let first = run_returned(
                &language,
                &language_rows,
                1,
                &[RuntimeValue::I32(7)],
                &mut heap,
                256,
            );
            let second = run_returned(&language, &language_rows, 2, &[], &mut heap, 256);
            combine(first, second, heap.live_len())
        },
    ));
    cases.push(bench(
        "fuel_resume",
        "micro",
        samples,
        || (),
        |()| run_two_slices(&language, 3, false),
    ));
    let explicit = explicit_resume_module();
    cases.push(bench(
        "explicit_resume",
        "micro",
        samples,
        || (),
        |()| run_two_slices(&explicit, 0, true),
    ));
    cases.push(bench(
        "string_concat",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 4, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "struct_construction",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 5, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "class_allocation",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 6, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "enum_construction_match",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 7, &[], &mut heap, 256),
    ));
    cases.push(bench(
        "array_operations",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 8, &[], &mut heap, 512),
    ));
    cases.push(bench(
        "map_operations",
        "micro",
        samples,
        || Heap::new_with_limits(64, 4_096, 64),
        |mut heap| run_returned(&language, &language_rows, 9, &[], &mut heap, 512),
    ));
    let buffer_type = language.module().buffer_types[0];
    cases.push(bench(
        "buffer_copy",
        "micro",
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
            run_returned(
                &language,
                &language_rows,
                10,
                &[destination, source],
                &mut heap,
                512,
            )
        },
    ));
    cases.push(bench(
        "product_data_sweep",
        "product",
        samples,
        || Heap::new_with_limits(1_024, 65_536, 512),
        |mut heap| run_returned(&language, &language_rows, 11, &[], &mut heap, 2_000_000),
    ));
    cases.push(bench(
        "product_standalone_pipeline",
        "product",
        samples,
        || (),
        |()| {
            // Full frontend + verifier + predecode + execution per sample:
            // the cost shape of a standalone script or REPL cell.
            let verified =
                nexa_compiler::compile(LANGUAGE_SOURCE).expect("benchmark language compiles");
            let rows = ExecutableModule::build(&verified, &OpcodeCostTable::default())
                .expect("benchmark language predecodes");
            let mut heap = Heap::new_with_limits(64, 4_096, 64);
            run_returned(
                &verified,
                &rows,
                0,
                &[RuntimeValue::I32(41)],
                &mut heap,
                256,
            )
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
        "micro",
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
        "subsystem",
        samples,
        || (),
        |()| {
            let vm_before = async_realm.vm_allocation_counters();
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
                vm: Some(async_realm.vm_allocation_counters().delta_since(vm_before)),
                gc: None,
                resources: peak,
            }
        },
    ));
    drop(async_realm);
    close_host(&async_host)?;

    let migration = migration_inputs()?;
    cases.push(bench(
        "migration",
        "subsystem",
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
                vm: None,
                gc: None,
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
        "subsystem",
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
                vm: Some(prepared.realm.vm_allocation_counters()),
                gc: None,
                resources,
            }
        },
    ));

    cases.push(bench(
        "realm_drop",
        "subsystem",
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

    // Stage G: bounded-pause evidence. Each sample times exactly one
    // budgeted incremental step against a realm heap under sustained
    // short-lived churn; the allocator pressure lives in `prepare`, which
    // the sampler never times. The case duration percentiles therefore ARE
    // the single-step pause distribution, and the case-level
    // system_allocations counter doubles as the "GC steps never allocate"
    // witness (G3 bound).
    let churn_type = StableId::from_name("benchmark-v7::GcChurn");
    let gc_realm = std::cell::RefCell::new(RealmRuntime::isolated(RealmConfig {
        max_heap_objects: 1_024,
        ..RealmConfig::default()
    }));
    cases.push(bench(
        "gc_incremental_step",
        "micro",
        samples,
        || {
            // Untimed churn: 32 short-lived objects per sample keeps the
            // in-flight garbage well below the 1024-slot ceiling across a
            // full mark+sweep cycle (~9 steps at this budget). One string
            // per sample gives the sweep real out-of-slot payload bytes to
            // account (G4); class payloads are inline and report zero.
            let mut realm = gc_realm.borrow_mut();
            realm
                .allocate(Object::String(String::from("gc-churn-payload-bytes")))
                .expect("churn string stays below the heap ceiling");
            for index in 0..31_u32 {
                realm
                    .allocate(Object::Class {
                        type_id: churn_type,
                        fields: [RuntimeValue::I32(i32::try_from(index).expect("bounded"));
                            nexa_bytecode::MAX_CLASS_FIELDS],
                        field_count: 1,
                    })
                    .expect("churn stays below the heap ceiling");
            }
        },
        |()| {
            let mut realm = gc_realm.borrow_mut();
            let report = realm
                .collect_garbage_incremental(GcBudget::objects(128))
                .expect("budgeted incremental step");
            let reclaimed = report.completed.map_or(0, |stats| {
                u64::try_from(stats.reclaimed).unwrap_or(u64::MAX)
            });
            Observation {
                heap_slots: realm.resource_ledger().heap_objects,
                gc: Some(GcObservation {
                    completed_cycles: u64::from(report.completed.is_some()),
                    objects_reclaimed: reclaimed,
                    bytes_reclaimed: report.bytes_reclaimed,
                }),
                ..Observation::default()
            }
        },
    ));
    drop(gc_realm);

    let report = BenchmarkReport {
        schema: 1,
        benchmark_version: 7,
        implementation_commit: git_commit(),
        benchmark_source_hash: blake3::hash(include_bytes!("main.rs")).to_hex().to_string(),
        bytecode_hash,
        toolchain: rustc_version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_model: cpu_model(),
        logical_cpu_count: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        samples,
        warmup: WARMUP,
        process_index,
        started_at_unix_ms,
        profiler_enabled,
        profiler: profiler_summary(profiler_enabled),
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

/// WP11 driver: spawn independent child processes, each with its own warmup,
/// and aggregate the median across process medians.
fn run_multi_process(
    arguments: &[String],
    processes: usize,
    samples: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let mut reports = Vec::with_capacity(processes);
    for index in 0..processes {
        let mut child_arguments = vec![
            "--process-index".to_owned(),
            index.to_string(),
            "--samples".to_owned(),
            samples.to_string(),
        ];
        if arguments.iter().any(|argument| argument == "--smoke") {
            child_arguments.push("--smoke".to_owned());
        }
        if arguments.iter().any(|argument| argument == "--profile") {
            child_arguments.push("--profile".to_owned());
        }
        let output = std::process::Command::new(&executable)
            .args(&child_arguments)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "benchmark process {index} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        eprintln!("process {index}/{processes} complete");
        reports.push(report);
    }
    let aggregate = aggregate_reports(&reports, processes, samples)?;
    let rendered = serde_json::to_string_pretty(&aggregate)?;
    if let Some(path) = argument_value(arguments, "--output") {
        std::fs::write(path, format!("{rendered}\n"))?;
    }
    println!("{rendered}");
    Ok(())
}

fn aggregate_reports(
    reports: &[serde_json::Value],
    processes: usize,
    samples: usize,
) -> Result<AggregateReport, Box<dyn std::error::Error>> {
    let first = reports.first().ok_or("no benchmark process reports")?;
    let first_cases = first["cases"].as_array().ok_or("report has no cases")?;
    let mut cases = Vec::with_capacity(first_cases.len());
    for (case_index, case) in first_cases.iter().enumerate() {
        let name = case["case"].as_str().ok_or("case has no name")?.to_owned();
        let tier = case["tier"].as_str().ok_or("case has no tier")?.to_owned();
        let collect = |field: &str| -> Result<Vec<u128>, Box<dyn std::error::Error>> {
            let mut values = Vec::with_capacity(reports.len());
            for report in reports {
                let value = &report["cases"][case_index][field];
                values.push(
                    value
                        .as_u64()
                        .map(u128::from)
                        .or_else(|| value.as_number().and_then(|n| n.as_u128()))
                        .ok_or_else(|| format!("case {name} missing numeric {field}"))?,
                );
            }
            values.sort_unstable();
            Ok(values)
        };
        let median = |values: &[u128]| values[values.len() / 2];
        let throughput = collect("throughput_ops_per_second")?;
        let p50 = collect("p50_ns")?;
        let p95 = collect("p95_ns")?;
        let p99 = collect("p99_ns")?;
        let allocations = collect("system_allocations")?;
        let allocated_bytes = collect("system_allocated_bytes")?;
        cases.push(AggregateCase {
            case: name,
            tier,
            median_throughput_ops_per_second: u64::try_from(median(&throughput))
                .unwrap_or(u64::MAX),
            median_p50_ns: median(&p50),
            median_p95_ns: median(&p95),
            median_p99_ns: median(&p99),
            max_system_allocations: u64::try_from(*allocations.last().unwrap_or(&0))
                .unwrap_or(u64::MAX),
            max_system_allocated_bytes: u64::try_from(*allocated_bytes.last().unwrap_or(&0))
                .unwrap_or(u64::MAX),
        });
    }
    Ok(AggregateReport {
        schema: 1,
        benchmark_version: 7,
        protocol: "median across process medians; each process independently warmed",
        process_count: processes,
        samples_per_process: samples,
        implementation_commit: git_commit(),
        benchmark_source_hash: blake3::hash(include_bytes!("main.rs")).to_hex().to_string(),
        cases,
    })
}

fn cpu_model() -> String {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".into())
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|info| {
                info.lines()
                    .find(|line| line.starts_with("model name"))
                    .and_then(|line| line.split(':').nth(1))
                    .map(|name| name.trim().to_owned())
            })
            .unwrap_or_else(|| "unknown".into())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unknown".into()
    }
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
fn product_data_sweep() -> i32 {
    let values: Array<i32> = Array::new();
    let mut index: i32 = 0;
    while index < 256 {
        let cell: BenchStruct = BenchStruct { value: index, wide: 9, label: "sweep" };
        values.push(cell.value);
        index = index + 1;
    }
    let mut total: i32 = 0;
    let mut cursor: i32 = 0;
    while cursor < 256 {
        total = total + values.get(cursor);
        cursor = cursor + 1;
    }
    return total;
}
"#;

fn run_returned(
    module: &VerifiedModule,
    executable: &ExecutableModule,
    function: u32,
    arguments: &[RuntimeValue],
    heap: &mut Heap,
    fuel: u64,
) -> Observation {
    // Stage F: the measurement authority executes the predecoded-row form,
    // which is what product realms run; fuel parity with the portable
    // interpreter is enforced by the executable_parity gate.
    let outcome = CheckedInterpreter::run_with_heap_and_executable(
        module, function, arguments, fuel, heap, executable,
    )
    .expect("verified benchmark function");
    let InterpreterOutcome::Returned { charge, value, .. } = outcome else {
        panic!("benchmark function did not return");
    };
    black_box(value);
    observation(charge, heap)
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

fn observation(charge: ExecutionCharge, heap: &Heap) -> Observation {
    Observation {
        fuel: charge.fuel_used,
        instructions: charge.instructions,
        heap_slots: u64::try_from(heap.live_len()).unwrap_or(u64::MAX),
        vm: Some(heap.vm_allocation_counters()),
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
        // Counters are cumulative per heap; the later observation subsumes
        // the earlier one taken from the same heap.
        vm: second.vm.or(first.vm),
        gc: second.gc.or(first.gc),
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
            bitmap: vec![false, false, false],
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

    fn call_runtime(
        &mut self,
        id: StableId,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
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
        (id == authority.stable_id()).then_some(authority)
    }

    fn call_runtime(
        &mut self,
        id: StableId,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != StableId::from_name("BenchHost::value") {
            return Err(HostTrap::UnknownFunction(id));
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
    tier: &'static str,
    samples: usize,
    mut prepare: impl FnMut() -> T,
    mut operation: impl FnMut(T) -> Observation,
) -> CaseStats {
    for _ in 0..WARMUP.min(samples) {
        let input = prepare();
        black_box(operation(input));
    }
    let mut durations = Vec::with_capacity(samples);
    let mut allocation_totals = AllocationDelta::default();
    let mut fuel = 0_u64;
    let mut instructions = 0_u64;
    let mut heap_slots = 0_u64;
    let mut vm_totals: Option<nexa_runtime::VmAllocationCounters> = None;
    let mut gc_totals: Option<GcObservation> = None;
    let mut resources = PeakResources::default();
    for _ in 0..samples {
        let input = prepare();
        let region = AllocationRegion::begin();
        let started = Instant::now();
        let observation = black_box(operation(input));
        let elapsed = started.elapsed();
        allocation_totals.accumulate(region.end());
        durations.push(elapsed);
        fuel = fuel.saturating_add(observation.fuel);
        instructions = instructions.saturating_add(observation.instructions);
        heap_slots = heap_slots.max(observation.heap_slots);
        if let Some(sample_vm) = observation.vm {
            vm_totals.get_or_insert_default().accumulate(sample_vm);
        }
        if let Some(sample_gc) = observation.gc {
            let totals = gc_totals.get_or_insert_default();
            totals.completed_cycles += sample_gc.completed_cycles;
            totals.objects_reclaimed += sample_gc.objects_reclaimed;
            totals.bytes_reclaimed += sample_gc.bytes_reclaimed;
        }
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
        tier,
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
        system_allocations: allocation_totals.allocations,
        system_reallocations: allocation_totals.reallocations,
        system_allocated_bytes: allocation_totals.allocated_bytes,
        system_reallocated_bytes: allocation_totals.reallocated_bytes,
        system_peak_outstanding_bytes: allocation_totals.peak_outstanding_bytes,
        vm: VmCounters::from_totals(heap_slots, vm_totals),
        gc: gc_totals.map_or_else(GcCounters::default, |totals| GcCounters {
            cycles: Some(totals.completed_cycles),
            pause_ns_max: Some(
                u64::try_from(durations.last().map_or(0, Duration::as_nanos)).unwrap_or(u64::MAX),
            ),
            objects_reclaimed: Some(totals.objects_reclaimed),
            bytes_reclaimed: Some(totals.bytes_reclaimed),
        }),
        fuel_total: fuel,
        fuel_per_operation: fuel / sample_count,
        instructions_total: instructions,
        instructions_per_operation: instructions / sample_count,
        peak_resources: resources,
    };
    eprintln!(
        "{}: {} ops/s, p50={}ns, p95={}ns, p99={}ns, allocs={}, alloc_bytes={}",
        stats.case,
        stats.throughput_ops_per_second,
        stats.p50_ns,
        stats.p95_ns,
        stats.p99_ns,
        stats.system_allocations,
        stats.system_allocated_bytes
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
