//! WP97 reload peak-memory authority.
//!
//! Unlike the hot comparison, this report deliberately measures the whole
//! overlapping lifetime: both portable artifacts are retained while the old
//! execution image, candidate image, migration arena, active incremental GC
//! state, and rooted String/collection storage coexist.

use std::hint::black_box;
use std::time::Instant;

use nexa_bytecode::ValueType;
use nexa_core::StableId;
use nexa_runtime::{
    GcBudget, GcPhase, Object, RealmConfig, RealmRuntime, RestartReloadOutcome,
    RestartReloadPolicy, RuntimeValue, StateObject, StateValue,
};
use serde::Serialize;

use super::{AllocationRegion, HOST};

const ROOTED_STRINGS: usize = 64;
const BUFFER_ELEMENTS: usize = 256;

const RELOAD_PEAK_V1: &str = r#"
fn stable_helper(x: i32) -> i32 { return x + 1; }
@state(version = 1)
class BenchState { mut value: i32, mut legacy: i32, }
async fn update(value: i32) -> i32 { return value; }
"#;

const RELOAD_PEAK_V2: &str = r#"
fn stable_helper(x: i32) -> i32 { return x + 1; }
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
struct Sample {
    duration_ns: u128,
    system_allocations: u64,
    system_allocated_bytes: u64,
    system_peak_outstanding_bytes: u64,
    old_artifact_bytes: u64,
    candidate_artifact_bytes: u64,
    executable_entries: usize,
    logical_executable_payload_bytes: u64,
    unique_executable_payload_bytes: u64,
    shared_executable_payload_bytes: u64,
    layout_reuses: u64,
    module_abi_reuses: u64,
    function_reuses: u64,
    string_pool_reuses: u64,
    host_plan_reuses: u64,
    migration_object_peak: usize,
    migration_field_peak: usize,
    migration_forwarding_peak: usize,
    migration_payload_byte_peak: usize,
    gc_root_peak: usize,
    gc_before: GcPhase,
    gc_after: GcPhase,
    string_bytes: u64,
    collection_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ReloadPeakReport {
    schema: u32,
    benchmark_version: u32,
    report: &'static str,
    implementation_commit: String,
    benchmark_source_hash: String,
    toolchain: String,
    os: &'static str,
    arch: &'static str,
    cpu_model: String,
    logical_cpu_count: usize,
    build_profile: &'static str,
    samples: usize,
    started_at_unix_ms: u128,
    protocol: &'static str,
    measurement_boundary: &'static str,
    duration: DurationSummary,
    system_allocator: SystemAllocatorSummary,
    simultaneous_surfaces: SimultaneousSurfaces,
    portable_artifacts: PortableArtifactSummary,
    executable_images: ExecutableImageSummary,
    migration_staging: MigrationStagingSummary,
    incremental_gc: IncrementalGcSummary,
    vm_storage: VmStorageSummary,
    reuse: ReuseSummary,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct DurationSummary {
    p50_ns: u128,
    p95_ns: u128,
    p99_ns: u128,
}

#[derive(Debug, Serialize)]
struct SystemAllocatorSummary {
    allocations_max: u64,
    allocated_bytes_max: u64,
    peak_outstanding_bytes_max: u64,
}

#[derive(Debug, Serialize)]
struct SimultaneousSurfaces {
    old_artifact: bool,
    candidate_artifact: bool,
    old_executable_module: bool,
    candidate_executable_module: bool,
    migration_staging: bool,
    incremental_gc_state: bool,
    string_and_collection_storage: bool,
}

#[derive(Debug, Serialize)]
struct PortableArtifactSummary {
    old_bytes: u64,
    candidate_bytes: u64,
    simultaneous_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ExecutableImageSummary {
    entries: usize,
    logical_payload_bytes: u64,
    unique_payload_bytes: u64,
    shared_payload_bytes: u64,
}

#[derive(Debug, Serialize)]
struct MigrationStagingSummary {
    object_peak: usize,
    field_peak: usize,
    forwarding_peak: usize,
    payload_byte_peak: usize,
    gc_root_peak: usize,
}

#[derive(Debug, Serialize)]
struct IncrementalGcSummary {
    phase_before_reload: &'static str,
    phase_after_reload: &'static str,
    active_before_reload: bool,
    active_after_reload: bool,
}

#[derive(Debug, Serialize)]
struct VmStorageSummary {
    rooted_strings: usize,
    buffer_elements: usize,
    string_bytes: u64,
    collection_bytes: u64,
}

#[derive(Debug, Serialize)]
struct ReuseSummary {
    layout_tables: u64,
    module_abis: u64,
    unchanged_functions: u64,
    string_pools: u64,
    host_import_plans: u64,
}

pub(super) fn run(samples: usize, output: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let started_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    // One unrecorded pass absorbs allocator initialization and page faults.
    black_box(measure_one()?);
    let mut measured = Vec::with_capacity(samples);
    for _ in 0..samples {
        measured.push(measure_one()?);
    }

    let mut durations = measured
        .iter()
        .map(|sample| sample.duration_ns)
        .collect::<Vec<_>>();
    durations.sort_unstable();
    let representative = measured[0];
    let max = |field: fn(&Sample) -> u64| measured.iter().map(field).max().unwrap_or(0);
    let max_usize = |field: fn(&Sample) -> usize| measured.iter().map(field).max().unwrap_or(0);

    let old_artifact_bytes = max(|sample| sample.old_artifact_bytes);
    let candidate_artifact_bytes = max(|sample| sample.candidate_artifact_bytes);
    let executable_entries = max_usize(|sample| sample.executable_entries);
    let logical_executable_payload_bytes = max(|sample| sample.logical_executable_payload_bytes);
    let unique_executable_payload_bytes = max(|sample| sample.unique_executable_payload_bytes);
    let shared_executable_payload_bytes = max(|sample| sample.shared_executable_payload_bytes);
    let migration_object_peak = max_usize(|sample| sample.migration_object_peak);
    let migration_field_peak = max_usize(|sample| sample.migration_field_peak);
    let migration_forwarding_peak = max_usize(|sample| sample.migration_forwarding_peak);
    let migration_payload_byte_peak = max_usize(|sample| sample.migration_payload_byte_peak);
    let gc_root_peak = max_usize(|sample| sample.gc_root_peak);
    let string_bytes = max(|sample| sample.string_bytes);
    let collection_bytes = max(|sample| sample.collection_bytes);
    let layout_reuses = max(|sample| sample.layout_reuses);
    let module_abi_reuses = max(|sample| sample.module_abi_reuses);
    let function_reuses = max(|sample| sample.function_reuses);
    let string_pool_reuses = max(|sample| sample.string_pool_reuses);
    let host_plan_reuses = max(|sample| sample.host_plan_reuses);

    // Every recorded sample must independently prove the overlap. Taking
    // maxima is useful for the summary, but must never synthesize PASS from
    // different samples that each missed a different required surface.
    let surfaces = SimultaneousSurfaces {
        old_artifact: measured.iter().all(|sample| sample.old_artifact_bytes != 0),
        candidate_artifact: measured
            .iter()
            .all(|sample| sample.candidate_artifact_bytes != 0),
        old_executable_module: measured.iter().all(|sample| sample.executable_entries >= 2),
        candidate_executable_module: measured.iter().all(|sample| sample.executable_entries >= 2),
        migration_staging: measured.iter().all(|sample| {
            sample.migration_object_peak != 0 && sample.migration_payload_byte_peak != 0
        }),
        incremental_gc_state: measured
            .iter()
            .all(|sample| sample.gc_before.is_active() && sample.gc_after.is_active()),
        string_and_collection_storage: measured
            .iter()
            .all(|sample| sample.string_bytes != 0 && sample.collection_bytes != 0),
    };
    let sharing_valid = measured.iter().all(|sample| {
        sample.shared_executable_payload_bytes != 0
            && sample.unique_executable_payload_bytes < sample.logical_executable_payload_bytes
            && sample.layout_reuses != 0
            && sample.function_reuses != 0
            && sample.string_pool_reuses != 0
            && sample.host_plan_reuses != 0
    });
    let pass = surfaces.old_artifact
        && surfaces.candidate_artifact
        && surfaces.old_executable_module
        && surfaces.candidate_executable_module
        && surfaces.migration_staging
        && surfaces.incremental_gc_state
        && surfaces.string_and_collection_storage
        && sharing_valid;

    let report = ReloadPeakReport {
        schema: 1,
        benchmark_version: 7,
        report: "Nexa M5 WP97 Reload Peak Memory",
        implementation_commit: super::git_commit(),
        benchmark_source_hash: super::benchmark_source_hash(),
        toolchain: super::rustc_version(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        cpu_model: super::cpu_model(),
        logical_cpu_count: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        samples,
        started_at_unix_ms,
        protocol: "one warmup plus bounded whole-lifetime samples in one isolated process",
        measurement_boundary: "before compiling both candidates through committed reload while encoded artifacts, both execution images, migration staging, active incremental GC, and rooted VM storage overlap",
        duration: DurationSummary {
            p50_ns: percentile(&durations, 50),
            p95_ns: percentile(&durations, 95),
            p99_ns: percentile(&durations, 99),
        },
        system_allocator: SystemAllocatorSummary {
            allocations_max: max(|sample| sample.system_allocations),
            allocated_bytes_max: max(|sample| sample.system_allocated_bytes),
            peak_outstanding_bytes_max: max(|sample| sample.system_peak_outstanding_bytes),
        },
        simultaneous_surfaces: surfaces,
        portable_artifacts: PortableArtifactSummary {
            old_bytes: old_artifact_bytes,
            candidate_bytes: candidate_artifact_bytes,
            simultaneous_bytes: old_artifact_bytes.saturating_add(candidate_artifact_bytes),
        },
        executable_images: ExecutableImageSummary {
            entries: executable_entries,
            logical_payload_bytes: logical_executable_payload_bytes,
            unique_payload_bytes: unique_executable_payload_bytes,
            shared_payload_bytes: shared_executable_payload_bytes,
        },
        migration_staging: MigrationStagingSummary {
            object_peak: migration_object_peak,
            field_peak: migration_field_peak,
            forwarding_peak: migration_forwarding_peak,
            payload_byte_peak: migration_payload_byte_peak,
            gc_root_peak,
        },
        incremental_gc: IncrementalGcSummary {
            phase_before_reload: gc_phase_name(representative.gc_before),
            phase_after_reload: gc_phase_name(representative.gc_after),
            active_before_reload: representative.gc_before.is_active(),
            active_after_reload: representative.gc_after.is_active(),
        },
        vm_storage: VmStorageSummary {
            rooted_strings: ROOTED_STRINGS,
            buffer_elements: BUFFER_ELEMENTS,
            string_bytes,
            collection_bytes,
        },
        reuse: ReuseSummary {
            layout_tables: layout_reuses,
            module_abis: module_abi_reuses,
            unchanged_functions: function_reuses,
            string_pools: string_pool_reuses,
            host_import_plans: host_plan_reuses,
        },
        status: if pass { "PASS" } else { "FAIL" },
    };
    let encoded = format!("{}\n", serde_json::to_string_pretty(&report)?);
    if let Some(path) = output {
        let path = std::path::Path::new(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, &encoded)?;
    } else {
        print!("{encoded}");
    }
    if pass {
        Ok(())
    } else {
        Err("WP97 reload peak-memory invariants failed".into())
    }
}

fn measure_one() -> Result<Sample, Box<dyn std::error::Error>> {
    let region = AllocationRegion::begin();
    let started = Instant::now();

    let old_module = nexa_compiler::compile_with_contract_id(RELOAD_PEAK_V1, HOST)?;
    let candidate_module = nexa_compiler::compile_with_contract_id(RELOAD_PEAK_V2, HOST)?;
    let old_artifact = old_module.module().encode();
    let candidate_artifact = candidate_module.module().encode();
    let state_ids = super::bench_state_ids(&old_module, &candidate_module);
    let old_schema = old_module.module().state_schema_fingerprint;

    let mut realm = RealmRuntime::isolated(RealmConfig {
        max_modules: 4,
        execution_image_cache_capacity: 4,
        max_heap_objects: 256,
        max_collection_elements: 4_096,
        max_collection_ranges: 512,
        ..RealmConfig::default()
    });
    let old = realm.load_module(old_module, HOST, old_schema)?;
    realm.insert_state(
        old,
        StableId::from_name("bench"),
        StateValue::Object(StateObject {
            type_id: state_ids.ty,
            version: 1,
            fields: std::collections::BTreeMap::from([
                (state_ids.value_field, StateValue::I32(7)),
                (state_ids.legacy_field, StateValue::I32(9)),
            ]),
        }),
    )?;

    for index in 0..ROOTED_STRINGS {
        let reference = realm.allocate(Object::String(format!(
            "wp97-rooted-string-{index:04}-{}",
            "x".repeat(64)
        )))?;
        realm.attach_module_root(old, reference)?;
    }
    let buffer = realm.allocate_buffer(
        nexa_bytecode::buffer_type(ValueType::I32),
        ValueType::I32,
        &vec![RuntimeValue::I32(7); BUFFER_ELEMENTS],
    )?;
    realm.attach_module_root(old, value_reference(buffer)?)?;
    let before_heap = realm.heap_byte_inspection();
    let before_gc = realm.collect_garbage_incremental(GcBudget::objects(1))?;
    if !before_gc.phase.is_active() {
        return Err("WP97 fixture failed to retain active GC state".into());
    }

    let outcome = realm.restart_reload(old, candidate_module, RestartReloadPolicy::default())?;
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        return Err(format!("WP97 reload did not commit: {outcome:?}").into());
    };
    if realm.active_root() != Some(candidate) {
        return Err("WP97 candidate was not published".into());
    }
    let after_gc = realm.collect_garbage_incremental(GcBudget::objects(1))?;
    let after_heap = realm.heap_byte_inspection();
    let images = realm.execution_image_cache_inspection();
    let host_plans = realm.host_import_plan_cache_inspection();
    let migration = realm
        .last_migration_usage_report()
        .ok_or("WP97 migration usage report missing")?;

    // Keep both encoded artifacts and the fully populated Realm live through
    // the allocation peak observation.
    black_box((&old_artifact, &candidate_artifact, &realm));
    let elapsed = started.elapsed().as_nanos();
    let allocation = region.end();
    Ok(Sample {
        duration_ns: elapsed,
        system_allocations: allocation.allocations,
        system_allocated_bytes: allocation.allocated_bytes,
        system_peak_outstanding_bytes: allocation.peak_outstanding_bytes,
        old_artifact_bytes: u64::try_from(old_artifact.len()).unwrap_or(u64::MAX),
        candidate_artifact_bytes: u64::try_from(candidate_artifact.len()).unwrap_or(u64::MAX),
        executable_entries: images.entries,
        logical_executable_payload_bytes: images.logical_executable_payload_bytes,
        unique_executable_payload_bytes: images.unique_executable_payload_bytes,
        shared_executable_payload_bytes: images.shared_executable_payload_bytes,
        layout_reuses: images.layout_reuses,
        module_abi_reuses: images.module_abi_reuses,
        function_reuses: images.function_reuses,
        string_pool_reuses: images.string_pool_reuses,
        host_plan_reuses: host_plans.hits,
        migration_object_peak: migration.object_peak,
        migration_field_peak: migration.field_peak,
        migration_forwarding_peak: migration.forwarding_peak,
        migration_payload_byte_peak: migration.payload_byte_peak,
        gc_root_peak: migration.gc_root_peak,
        gc_before: before_gc.phase,
        gc_after: after_gc.phase,
        string_bytes: before_heap.string_bytes.max(after_heap.string_bytes),
        collection_bytes: before_heap
            .collection_total()
            .max(after_heap.collection_total()),
    })
}

fn value_reference(value: RuntimeValue) -> Result<nexa_runtime::GcRef, &'static str> {
    match value {
        RuntimeValue::Ref(reference)
        | RuntimeValue::NamedRef { reference, .. }
        | RuntimeValue::String { reference, .. }
        | RuntimeValue::Struct { reference, .. } => Ok(reference),
        _ => Err("WP97 fixture value is not GC-backed"),
    }
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    samples[(samples.len() - 1) * percentile / 100]
}

const fn gc_phase_name(phase: GcPhase) -> &'static str {
    match phase {
        GcPhase::Idle => "idle",
        GcPhase::RootSnapshot => "root-snapshot",
        GcPhase::Mark => "mark",
        GcPhase::Sweep => "sweep",
        GcPhase::Complete => "complete",
    }
}
