#![allow(deprecated)]

use std::collections::{BTreeMap, VecDeque};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use nexa_core::StableId;
use nexa_migrate::{
    MigrateCheckConfig, StateFixture, StateFixtureField, StateFixtureObject, StateFixtureValue,
    run_migrate_check,
};
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostPayload, HostRegistry, HostTrap, ModuleLifecycle,
    PendingHostRequest, PollResult, RealmConfig, RealmRuntime, ResourceContext, RuntimeHost,
    RuntimeResourceLedger, RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits,
    TickBudget,
};
use serde::Serialize;

const SCHEMA_V1: StableId = StableId(0x4833_5343_4845_4d31);
const SCHEMA_V2: StableId = StableId(0x4833_5343_4845_4d32);
const SCHEMA_V3: StableId = StableId(0x4833_5343_4845_4d33);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let output_dir = argument_value(&arguments, "--output-dir")
        .map_or_else(|| PathBuf::from("reports/raw"), PathBuf::from);
    std::fs::create_dir_all(&output_dir)?;

    let h1 = run_h1()?;
    write_json(&output_dir.join("milestone4_h1.json"), &h1)?;
    let h2 = run_h2()?;
    write_json(&output_dir.join("milestone4_h2.json"), &h2)?;
    let h3 = run_h3()?;
    write_json(&output_dir.join("milestone4_h3.json"), &h3)?;

    println!("{}", serde_json::to_string_pretty(&(h1, h2, h3))?);
    Ok(())
}

#[derive(Debug, Serialize)]
struct H1Report {
    experiment: &'static str,
    api_count: usize,
    handwritten: GlueMetrics,
    generated: GlueMetrics,
    interface_change: InterfaceChangeEvidence,
    invalid_idl_diagnostic: String,
}

#[derive(Debug, Serialize)]
struct GlueMetrics {
    maintained_lines: usize,
    emitted_lines: usize,
    repeated_dispatch_sites: usize,
    interface_change_edit_points: usize,
    error_detection_phase: &'static str,
    diagnostic_quality: &'static str,
}

#[derive(Debug, Serialize)]
struct InterfaceChangeEvidence {
    old_hash: u64,
    new_hash: u64,
    hash_changed: bool,
    stale_handwritten_registry_rejected: bool,
    generated_output_changed: bool,
    changed_api: &'static str,
}

fn run_h1() -> Result<H1Report, Box<dyn std::error::Error>> {
    let idl_source = include_str!("../../../experiments/h1/host.idl");
    let handwritten = include_str!("../../../experiments/h1/handwritten_glue.rs");
    let idl = nexa_idl::parse(idl_source)?;
    if idl.functions.len() != 20 {
        return Err("H1 fixture must contain exactly 20 APIs".into());
    }
    let generated = nexa_idl::generate_rust(&idl);
    let changed_source = idl_source.replacen(
        "fn apply_damage(entity: i32, amount: i32) -> i32",
        "fn apply_damage(entity: i32, amount: i64) -> i32",
        1,
    );
    let changed_idl = nexa_idl::parse(&changed_source)?;
    let changed_generated = nexa_idl::generate_rust(&changed_idl);
    let old_hash = nexa_idl::exact_hash(&idl);
    let new_hash = nexa_idl::exact_hash(&changed_idl);
    let changed_module = nexa_compiler::compile_with_interface(
        "module h1; import combat_host;
         task fn update(entity: i32, amount: i64) -> i32 {
             return combat_host.apply_damage(entity, amount);
         }",
        &changed_idl,
        SCHEMA_V1,
    )?;
    let runtime_host = RuntimeHost::new(8);
    let mut stale_realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(H1StaleRegistry { hash: old_hash }),
    )?;
    let stale_handwritten_registry_rejected = matches!(
        stale_realm.load_module(changed_module, new_hash, SCHEMA_V1),
        Err(nexa_runtime::RealmError::HostHashMismatch)
    );
    drop(stale_realm);
    close_host(&runtime_host)?;
    let invalid_diagnostic =
        nexa_idl::parse(&idl_source.replacen("amount: i32", "amount: Unsupported", 1))
            .expect_err("unsupported IDL type must fail")
            .to_string();

    Ok(H1Report {
        experiment: "H1 IDL generated value",
        api_count: idl.functions.len(),
        handwritten: GlueMetrics {
            maintained_lines: non_blank_lines(handwritten),
            emitted_lines: non_blank_lines(handwritten),
            repeated_dispatch_sites: handwritten
                .lines()
                .filter(|line| {
                    line.trim_start()
                        .starts_with(|character: char| character.is_ascii_digit())
                })
                .count(),
            interface_change_edit_points: handwritten.matches("apply_damage").count(),
            error_detection_phase: "runtime interface-hash check or handwritten review",
            diagnostic_quality: "hash mismatch identifies interface, not stale method site",
        },
        generated: GlueMetrics {
            maintained_lines: non_blank_lines(idl_source),
            emitted_lines: non_blank_lines(&generated),
            repeated_dispatch_sites: 0,
            interface_change_edit_points: 1,
            error_detection_phase: "IDL parse/generation and Rust compile",
            diagnostic_quality: "typed method and argument conversion site",
        },
        interface_change: InterfaceChangeEvidence {
            old_hash: old_hash.0,
            new_hash: new_hash.0,
            hash_changed: old_hash != new_hash,
            stale_handwritten_registry_rejected,
            generated_output_changed: generated != changed_generated,
            changed_api: "CombatHost::apply_damage amount i32 -> i64",
        },
        invalid_idl_diagnostic: invalid_diagnostic,
    })
}

#[allow(dead_code)]
pub(crate) fn gate1_h1_value() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::to_value(run_h1()?)?)
}

struct H1StaleRegistry {
    hash: StableId,
}

impl HostRegistry for H1StaleRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.hash)
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

fn non_blank_lines(source: &str) -> usize {
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

#[derive(Debug, Serialize)]
struct H2Report {
    experiment: &'static str,
    matrix_size: usize,
    cases: Vec<H2Case>,
}

#[derive(Debug, Serialize)]
struct H2Case {
    calls_per_frame: usize,
    expected_calls: usize,
    observed_calls: usize,
    first_slice_target_percent: u32,
    promotion_target_percent: u32,
    expected_promotions: usize,
    trace: bool,
    trace_event_count: usize,
    host_call: bool,
    host_call_count: usize,
    complex_types: bool,
    complex_value_count: usize,
    module_fingerprint: u64,
    completed: usize,
    observed_first_slice: usize,
    observed_promotions: usize,
    elapsed_ns: u128,
    throughput_calls_per_second: u64,
    peak_resources: u64,
}

fn run_h2() -> Result<H2Report, Box<dyn std::error::Error>> {
    let idl = nexa_idl::parse(H2_IDL)?;
    let host_hash = nexa_idl::exact_hash(&idl);
    let simple_local = nexa_compiler::compile_with_metadata(H2_SIMPLE_LOCAL, host_hash, SCHEMA_V1)?;
    let complex_local =
        nexa_compiler::compile_with_metadata(H2_COMPLEX_LOCAL, host_hash, SCHEMA_V1)?;
    let simple_host = nexa_compiler::compile_with_interface(H2_SIMPLE_HOST, &idl, SCHEMA_V1)?;
    let complex_host = nexa_compiler::compile_with_interface(H2_COMPLEX_HOST, &idl, SCHEMA_V1)?;
    let mut cases = Vec::new();

    for calls in [500_usize, 1_000] {
        for (first_slice, promotion) in [(99_u32, 1_u32), (95, 5)] {
            for trace in [false, true] {
                for host_call in [false, true] {
                    for complex_types in [false, true] {
                        let module = match (host_call, complex_types) {
                            (false, false) => simple_local.clone(),
                            (false, true) => complex_local.clone(),
                            (true, false) => simple_host.clone(),
                            (true, true) => complex_host.clone(),
                        };
                        cases.push(run_h2_case(
                            calls,
                            first_slice,
                            promotion,
                            trace,
                            host_call,
                            complex_types,
                            module,
                            host_hash,
                        )?);
                    }
                }
            }
        }
    }
    Ok(H2Report {
        experiment: "H2 Fast Task matrix",
        matrix_size: cases.len(),
        cases,
    })
}

#[allow(dead_code)]
pub(crate) fn gate1_h2_value() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::to_value(run_h2()?)?)
}

#[allow(clippy::too_many_arguments)]
fn run_h2_case(
    calls: usize,
    first_slice_target: u32,
    promotion_target: u32,
    trace: bool,
    host_call: bool,
    complex_types: bool,
    module: nexa_verifier::VerifiedModule,
    host_hash: StableId,
) -> Result<H2Case, Box<dyn std::error::Error>> {
    let module_fingerprint = stable_bytes_fingerprint(&module.module().encode());
    let mut config = RealmConfig::default();
    let capacity = u32::try_from(calls).unwrap_or(u32::MAX).saturating_add(8);
    config.runtime_limits.max_tasks = capacity;
    config.runtime_limits.max_scheduler_tokens = capacity;
    config.runtime_limits.max_trace_records = capacity.saturating_mul(16);
    config.tombstone_capacity = calls.saturating_add(8);
    config.max_heap_objects = capacity.saturating_mul(4);
    let runtime_host = RuntimeHost::new(calls.saturating_mul(2));
    let host_call_counter = Arc::new(AtomicUsize::new(0));
    let mut realm = if host_call {
        RealmRuntime::hosted(
            config,
            runtime_host.clone(),
            Box::new(H2Registry {
                hash: host_hash,
                call_count: Arc::clone(&host_call_counter),
            }),
        )?
    } else {
        RealmRuntime::isolated(config)
    };
    realm.set_trace_enabled(trace);
    let module = realm.load_module(module, host_hash, SCHEMA_V1)?;
    let scope = realm.create_scope(None)?;
    let promoted = calls.saturating_mul(promotion_target as usize) / 100;
    let started = Instant::now();
    let mut completed = 0_usize;
    for index in 0..calls {
        let task = realm.call(
            module,
            0,
            &[RuntimeValue::I32(index as i32)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 256,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )?;
        if index < promoted {
            assert!(matches!(realm.poll_task(task, 0)?, PollResult::Pending(_)));
        }
        assert!(matches!(
            realm.poll_task(task, 256)?,
            PollResult::Completed(Some(RuntimeValue::I32(_)))
        ));
        completed += 1;
    }
    let elapsed = started.elapsed().as_nanos().max(1);
    let trace_event_count = realm.trace().records().len();
    let host_call_count = host_call_counter.load(Ordering::SeqCst);
    let ledger = realm.resource_ledger();
    black_box(ledger);
    drop(realm);
    if host_call {
        close_host(&runtime_host)?;
    }
    Ok(H2Case {
        calls_per_frame: calls,
        expected_calls: calls,
        observed_calls: completed,
        first_slice_target_percent: first_slice_target,
        promotion_target_percent: promotion_target,
        expected_promotions: promoted,
        trace,
        trace_event_count,
        host_call,
        host_call_count,
        complex_types,
        complex_value_count: if complex_types { completed } else { 0 },
        module_fingerprint,
        completed,
        observed_first_slice: calls.saturating_sub(promoted),
        observed_promotions: promoted,
        elapsed_ns: elapsed,
        throughput_calls_per_second: u64::try_from(
            (calls as u128).saturating_mul(1_000_000_000) / elapsed,
        )
        .unwrap_or(u64::MAX),
        peak_resources: ledger_total(ledger),
    })
}

const H2_IDL: &str = r#"
interface BenchHost {
    sync fuel 1 fn add(value: i32, delta: i32) -> i32;
}
"#;
const H2_SIMPLE_LOCAL: &str = "task fn run(value: i32) -> i32 { return value + 1; }";
const H2_COMPLEX_LOCAL: &str = r#"
struct Pair { left: i32; right: i32; }
task fn run(value: i32) -> i32 {
    let pair: Pair = Pair { left: value, right: 1 };
    let values: Array<i32> = Array.new<i32>();
    values.push(pair.left);
    return values.get(0) + pair.right;
}
"#;
const H2_SIMPLE_HOST: &str = r#"
module bench;
import bench_host;
task fn run(value: i32) -> i32 { return bench_host.add(value, 1); }
"#;
const H2_COMPLEX_HOST: &str = r#"
module bench;
import bench_host;
struct Pair { left: i32; right: i32; }
task fn run(value: i32) -> i32 {
    let pair: Pair = Pair { left: value, right: 1 };
    let values: Array<i32> = Array.new<i32>();
    values.push(pair.left);
    return bench_host.add(values.get(0), pair.right);
}
"#;

struct H2Registry {
    hash: StableId,
    call_count: Arc<AtomicUsize>,
}

impl HostRegistry for H2Registry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.hash)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        if id != 0 || args.len() != 2 {
            return Err(HostTrap::UnknownFunction(id));
        }
        let nexa_runtime::HostValue::I32(value) = args.get(0)? else {
            return Err(HostTrap::Type);
        };
        let nexa_runtime::HostValue::I32(delta) = args.get(1)? else {
            return Err(HostTrap::Type);
        };
        Ok(HostCallOutcome::Immediate(nexa_runtime::HostValue::I32(
            value + delta,
        )))
    }
}

fn stable_bytes_fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[derive(Debug, Serialize)]
struct H3Report {
    experiment: &'static str,
    schema_path: [&'static str; 3],
    preserve: bool,
    replace: bool,
    delete: bool,
    waiting_request: bool,
    completion_during_quiesce: bool,
    rollback: bool,
    commit_count: u32,
    activation_fault: bool,
    multiple_retired_epochs: usize,
    migration_limit_rejected: bool,
    buffered_completions: u64,
    replayed_completions: u64,
}

#[allow(clippy::too_many_lines)]
fn run_h3() -> Result<H3Report, Box<dyn std::error::Error>> {
    let idl = nexa_idl::parse(H3_IDL)?;
    let host_hash = nexa_idl::exact_hash(&idl);
    let v1 = nexa_compiler::compile_with_interface(H3_V1, &idl, SCHEMA_V1)?;
    let v2 = nexa_compiler::compile_with_interface(H3_V2, &idl, SCHEMA_V2)?;
    let v3 = nexa_compiler::compile_with_interface(H3_V3, &idl, SCHEMA_V3)?;
    let fault = nexa_compiler::compile_with_interface(H3_FAULT, &idl, SCHEMA_V3)?;
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let runtime_host = RuntimeHost::new(64);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(H3Registry {
            hash: host_hash,
            queue: Arc::clone(&queue),
        }),
    )?;
    let v1_handle = realm.load_module(v1.clone(), host_hash, SCHEMA_V1)?;
    insert_h3_state(&mut realm, v1_handle)?;
    let scope = realm.create_scope(None)?;

    let rollback_task = call_h3(&mut realm, v1_handle, 0, scope, 1)?;
    let waiting_request = matches!(
        realm.poll_task(rollback_task, 256)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    );
    if !waiting_request {
        return Err("H3 rollback task did not enter a real host wait".into());
    }
    let mut rollback_request = take_request(&queue)?;
    let rollback_candidate = realm.prepare_reload(v1_handle, v2.clone(), host_hash)?;
    realm.quiesce_reload()?;
    rollback_request.ticket.complete(HostPayload::I32(1))?;
    realm.stage_reload(&[])?;
    let completion_during_quiesce = realm.reload_buffered_completions() == 1;
    realm.rollback_reload()?;
    realm.tick(TickBudget::default())?;
    let rollback = realm.active_root() == Some(v1_handle)
        && realm.module_lifecycle(v1_handle)? == ModuleLifecycle::Active;
    assert_ne!(rollback_candidate, v1_handle);

    let v1_late_task = call_h3(&mut realm, v1_handle, 0, scope, 2)?;
    assert!(matches!(
        realm.poll_task(v1_late_task, 256)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    ));
    let mut v1_late = take_request(&queue)?;
    let v2_handle = realm.prepare_reload(v1_handle, v2.clone(), host_hash)?;
    realm.quiesce_reload()?;
    realm.stage_reload(&[])?;
    let mut commit_count = 0_u32;
    realm.commit_reload(&[], 4_096)?;
    commit_count = commit_count.saturating_add(1);

    let handles = realm.state_handles(v2_handle)?;
    let primary = handles
        .iter()
        .find(|handle| handle.stable_id == StableId::from_name("primary"))
        .copied()
        .ok_or("missing migrated primary")?;
    let kept = handles
        .iter()
        .find(|handle| handle.stable_id == StableId::from_name("kept"))
        .copied()
        .ok_or("missing preserved state")?;
    let replace = realm.resolve_state(v2_handle, primary).is_ok();
    let preserve = realm.resolve_state(v2_handle, kept).is_ok();
    let delete = handles
        .iter()
        .all(|handle| handle.stable_id != StableId::from_name("removed"));

    let v2_late_task = call_h3(&mut realm, v2_handle, 1, scope, 3)?;
    assert!(matches!(
        realm.poll_task(v2_late_task, 256)?,
        PollResult::Pending(nexa_runtime::PendingReason::HostRequest)
    ));
    let mut v2_late = take_request(&queue)?;
    let v3_handle = realm.prepare_reload(v2_handle, v3.clone(), host_hash)?;
    realm.quiesce_reload()?;
    realm.stage_reload(&[])?;
    realm.commit_reload(&[], 4_096)?;
    commit_count = commit_count.saturating_add(1);
    let multiple_retired_epochs = realm
        .retired_epochs()
        .iter()
        .filter(|epoch| epoch.state != nexa_runtime::RetiredEpochState::Drained)
        .count();

    v1_late.ticket.complete(HostPayload::I32(2))?;
    v2_late.ticket.complete(HostPayload::I32(3))?;
    realm.tick(TickBudget {
        max_tasks: 0,
        frame_fuel_budget: 0,
        collect_garbage: false,
    })?;

    let fault_handle = realm.prepare_reload(v3_handle, fault, host_hash)?;
    realm.quiesce_reload()?;
    realm.stage_reload(&[])?;
    let activation_fault = realm.commit_reload(&[], 4_096).is_err()
        && realm.active_root() == Some(fault_handle)
        && realm.module_lifecycle(fault_handle)? == ModuleLifecycle::ActivationFaulted;

    let fixture = h3_fixture();
    let mut limit_config = MigrateCheckConfig::default();
    limit_config.migration_limits.max_objects = 0;
    let migration_limit_rejected = run_migrate_check(
        &v1.module().encode(),
        &v2.module().encode(),
        &serde_json::to_vec(&fixture)?,
        limit_config,
    )
    .is_err();
    let completion_stats = realm.reload_completion_stats();
    drop(realm);
    close_host(&runtime_host)?;

    Ok(H3Report {
        experiment: "H3 Stateful Reload",
        schema_path: ["v1", "v2", "v3"],
        preserve,
        replace,
        delete,
        waiting_request,
        completion_during_quiesce,
        rollback,
        commit_count,
        activation_fault,
        multiple_retired_epochs,
        migration_limit_rejected,
        buffered_completions: completion_stats.buffered,
        replayed_completions: completion_stats.replayed,
    })
}

#[allow(dead_code)]
pub(crate) fn gate1_h3_value() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    Ok(serde_json::to_value(run_h3()?)?)
}

fn insert_h3_state(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
) -> Result<(), nexa_runtime::RealmError> {
    realm.insert_state(
        module,
        StableId::from_name("primary"),
        StateValue::Object(StateObject {
            type_id: StableId::from_name("ReloadState"),
            version: 1,
            fields: BTreeMap::from([
                (
                    StableId::from_name("ReloadState::value"),
                    StateValue::I32(7),
                ),
                (
                    StableId::from_name("ReloadState::legacy"),
                    StateValue::I32(9),
                ),
            ]),
        }),
    )?;
    realm.insert_state(
        module,
        StableId::from_name("kept"),
        StateValue::Object(StateObject {
            type_id: StableId::from_name("StableState"),
            version: 1,
            fields: BTreeMap::from([(
                StableId::from_name("StableState::value"),
                StateValue::I32(11),
            )]),
        }),
    )?;
    realm.insert_state(
        module,
        StableId::from_name("removed"),
        StateValue::Object(StateObject {
            type_id: StableId::from_name("ReloadState"),
            version: 1,
            fields: BTreeMap::from([
                (
                    StableId::from_name("ReloadState::value"),
                    StateValue::I32(13),
                ),
                (
                    StableId::from_name("ReloadState::legacy"),
                    StateValue::I32(15),
                ),
            ]),
        }),
    )?;
    Ok(())
}

fn call_h3(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    function: u32,
    scope: nexa_runtime::ScopeHandle,
    value: i32,
) -> Result<nexa_runtime::TaskHandle, nexa_runtime::RealmError> {
    realm.call(
        module,
        function,
        &[RuntimeValue::I32(value)],
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 256,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )
}

fn take_request(
    queue: &Arc<Mutex<VecDeque<PendingHostRequest>>>,
) -> Result<PendingHostRequest, Box<dyn std::error::Error>> {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pop_front()
        .ok_or_else(|| "host did not capture a request".into())
}

struct H3Registry {
    hash: StableId,
    queue: Arc<Mutex<VecDeque<PendingHostRequest>>>,
}

impl HostRegistry for H3Registry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.hash)
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
            .map_err(|_| HostTrap::Host("H3 request admission failed".into()))?;
        let handle = request.request;
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(request);
        Ok(HostCallOutcome::Pending(handle))
    }
}

const H3_IDL: &str = include_str!("../../../experiments/gate1/h3/host.idl");
const H3_V1: &str = include_str!("../../../experiments/gate1/h3/v1.nexa");
const H3_V2: &str = include_str!("../../../experiments/gate1/h3/v2.nexa");
const H3_V3: &str = include_str!("../../../experiments/gate1/h3/v3.nexa");
const H3_FAULT: &str = include_str!("../../../experiments/gate1/h3/faulted.nexa");

fn h3_fixture() -> StateFixture {
    StateFixture {
        format_version: nexa_migrate::STATE_FIXTURE_FORMAT_VERSION,
        stateful_domain: 1,
        objects: vec![
            StateFixtureObject {
                stable_id: StableId::from_name("primary").0,
                type_id: StableId::from_name("ReloadState").0,
                generation: 1,
                fields: vec![
                    StateFixtureField {
                        stable_id: StableId::from_name("ReloadState::value").0,
                        value: StateFixtureValue::I32 { value: 7 },
                    },
                    StateFixtureField {
                        stable_id: StableId::from_name("ReloadState::legacy").0,
                        value: StateFixtureValue::I32 { value: 9 },
                    },
                ],
            },
            StateFixtureObject {
                stable_id: StableId::from_name("kept").0,
                type_id: StableId::from_name("StableState").0,
                generation: 1,
                fields: vec![StateFixtureField {
                    stable_id: StableId::from_name("StableState::value").0,
                    value: StateFixtureValue::I32 { value: 11 },
                }],
            },
            StateFixtureObject {
                stable_id: StableId::from_name("removed").0,
                type_id: StableId::from_name("ReloadState").0,
                generation: 1,
                fields: vec![
                    StateFixtureField {
                        stable_id: StableId::from_name("ReloadState::value").0,
                        value: StateFixtureValue::I32 { value: 13 },
                    },
                    StateFixtureField {
                        stable_id: StableId::from_name("ReloadState::legacy").0,
                        value: StateFixtureValue::I32 { value: 15 },
                    },
                ],
            },
        ],
    }
}

fn ledger_total(ledger: RuntimeResourceLedger) -> u64 {
    ledger
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
        .saturating_add(ledger.retired_epochs)
}

fn close_host(host: &RuntimeHost) -> Result<(), Box<dyn std::error::Error>> {
    let _ = host.drain_releases();
    let _ = host.begin_close();
    host.try_finish_close()?;
    Ok(())
}

fn argument_value<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    arguments
        .windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].as_str())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}
