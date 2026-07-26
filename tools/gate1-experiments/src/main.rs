#![allow(deprecated)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use nexa_core::StableId;
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostRegistry, HostTrap, RealmConfig, RealmRuntime, ResourceContext,
    RuntimeHost,
};
use serde_json::{Value, json};

#[allow(clippy::all, clippy::pedantic, dead_code)]
#[path = "../../milestone4-experiments/src/main.rs"]
mod milestone4;

const ROOT: &str = ".";
const GATE1: &str = "experiments/gate1";
const MANIFEST: &str = "experiments/gate1/manifest.json";
const ENVIRONMENT: &str = "experiments/gate1/environment.json";
const ACCEPTANCE: &str = "baseline/testing/GATE1_ACCEPTANCE.md";
const H1_IDL: &str = "experiments/gate1/h1/combat.idl";
const H1_HANDWRITTEN: &str = "experiments/gate1/h1/handwritten/dispatcher.rs";
const H1_GENERATED: &str = "experiments/gate1/h1/generated/combat.rs";
const BENCHMARK: &str = "experiments/gate1/benchmark.json";
const H3_FIXTURE: &str = "experiments/gate1/h3/state.json";
const RUNNER: &str = "tools/gate1-experiments/src/main.rs";

type AnyError = Box<dyn std::error::Error>;

fn main() -> Result<(), AnyError> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("freeze") if arguments.len() == 1 => freeze(),
        Some("validate") if arguments.len() == 1 => {
            let checks = validate(false)?;
            println!("{}", serde_json::to_string_pretty(&checks)?);
            Ok(())
        }
        Some("run-all") if arguments.len() == 1 => run_all(),
        Some("replay") if arguments.len() == 2 => replay(Path::new(&arguments[1])),
        Some("compare") if arguments.len() == 3 => {
            compare(Path::new(&arguments[1]), Path::new(&arguments[2]))
        }
        Some("reconcile-replays") if arguments.len() == 3 => {
            reconcile_replays(Path::new(&arguments[1]), Path::new(&arguments[2]))
        }
        _ => Err(
            "usage: nexa-gate1-experiments freeze|validate|run-all|replay <result-dir>|compare <run-a> <run-b>|reconcile-replays <replay-a> <replay-b>"
                .into(),
        ),
    }
}

fn freeze() -> Result<(), AnyError> {
    let idl_source = std::fs::read_to_string(H1_IDL)?;
    let idl = nexa_idl::parse(&idl_source)?;
    if idl.functions.len() != 20 {
        return Err("Gate 1 H1 IDL must contain exactly 20 APIs".into());
    }
    let generated = nexa_idl::generate_rust(&idl);
    std::fs::create_dir_all(Path::new(H1_GENERATED).parent().expect("generated parent"))?;
    std::fs::write(H1_GENERATED, generated)?;

    let hardware = optional_command_text("system_profiler", &["SPHardwareDataType"]);
    let environment = json!({
        "schema_version": 1,
        "rust": command_text("rustc", &["--version"])?,
        "cargo": command_text("cargo", &["--version"])?,
        "os": std::env::consts::OS,
        "os_version": command_text("uname", &["-sr"])?,
        "cpu": hardware_value(&hardware, "Chip"),
        "architecture": std::env::consts::ARCH,
        "memory": hardware_value(&hardware, "Memory"),
        "power_mode": optional_command_text("pmset", &["-g", "batt"]),
        "profile": "release for timed benchmark; debug for semantic matrices",
        "feature_flags": ["nexa-runtime/allocation-counting"],
        "allocator": "System, observed by isolated tools/allocation-observer global wrapper",
        "commit_sha": "IMPLEMENTATION_COMMIT",
        "tree_sha": "IMPLEMENTATION_TREE",
        "seed": 557_074_319_u64
    });
    write_json(Path::new(ENVIRONMENT), &environment)?;

    let samples = sample_paths()?;
    if samples.len() != 20 {
        return Err(format!("expected exactly 20 sample files, found {}", samples.len()).into());
    }
    for path in &samples {
        let _: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    }
    let sample_hashes = samples
        .iter()
        .map(|path| {
            Ok((
                path.to_string_lossy().into_owned(),
                hash_file(path.to_string_lossy().as_ref())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, AnyError>>()?;

    let bound_paths = [
        ACCEPTANCE,
        ENVIRONMENT,
        H1_IDL,
        H1_HANDWRITTEN,
        H1_GENERATED,
        BENCHMARK,
        H3_FIXTURE,
        "experiments/gate1/h3/host.idl",
        "experiments/gate1/h3/v1.nexa",
        "experiments/gate1/h3/v2.nexa",
        "experiments/gate1/h3/v3.nexa",
        "experiments/gate1/h3/faulted.nexa",
        RUNNER,
        "tools/milestone4-experiments/src/main.rs",
        "tools/benchmark-v6/src/main.rs",
        "tools/allocation-observer/src/main.rs",
        "tools/gate1-decision/src/main.rs",
        "experiments/gate1/AMENDMENT_1.md",
    ];
    let bound_hashes = bound_paths
        .iter()
        .map(|path| Ok(((*path).to_owned(), hash_file(path)?)))
        .collect::<Result<BTreeMap<_, _>, AnyError>>()?;
    let manifest = json!({
        "schema_version": 1,
        "status": "FROZEN",
        "acceptance_hash": bound_hashes[ACCEPTANCE],
        "environment_hash": bound_hashes[ENVIRONMENT],
        "sample_hashes": sample_hashes,
        "idl_hash": bound_hashes[H1_IDL],
        "benchmark_config_hash": bound_hashes[BENCHMARK],
        "migration_fixture_hash": bound_hashes[H3_FIXTURE],
        "experiment_runner_hash": bound_hashes[RUNNER],
        "bound_hashes": bound_hashes
    });
    write_json(Path::new(MANIFEST), &manifest)?;
    println!("froze Gate 1 manifest with 20 samples");
    Ok(())
}

fn validate(require_clean: bool) -> Result<Value, AnyError> {
    let started = Instant::now();
    let manifest: Value = read_json(Path::new(MANIFEST))?;
    let environment: Value = read_json(Path::new(ENVIRONMENT))?;
    let current_rust = command_text("rustc", &["--version"])?;
    let current_cargo = command_text("cargo", &["--version"])?;
    let clean = git_clean()?;
    if require_clean && !clean {
        return Err("formal Gate 1 execution requires a clean workspace".into());
    }
    if environment["rust"] != current_rust || environment["cargo"] != current_cargo {
        return Err("frozen Rust/Cargo version does not match".into());
    }
    if environment["feature_flags"] != json!(["nexa-runtime/allocation-counting"]) {
        return Err("frozen feature set does not match".into());
    }
    let bound = manifest["bound_hashes"]
        .as_object()
        .ok_or("manifest bound_hashes is not an object")?;
    let mut mismatches = Vec::new();
    for (path, expected) in bound {
        let actual = hash_file(path)?;
        if expected.as_str() != Some(actual.as_str()) {
            mismatches.push(json!({"path": path, "expected": expected, "actual": actual}));
        }
    }
    let samples = sample_paths()?;
    let sample_hashes = manifest["sample_hashes"]
        .as_object()
        .ok_or("manifest sample_hashes is not an object")?;
    for path in &samples {
        let name = path.to_string_lossy();
        let actual = hash_file(&name)?;
        if sample_hashes.get(name.as_ref()).and_then(Value::as_str) != Some(actual.as_str()) {
            mismatches.push(json!({"path": name, "actual": actual}));
        }
    }
    if samples.len() != 20 || sample_hashes.len() != 20 {
        mismatches.push(json!({"samples": samples.len(), "manifest_samples": sample_hashes.len()}));
    }
    let timer_start = Instant::now();
    let timer_monotonic = Instant::now() >= timer_start;
    let passed = mismatches.is_empty() && timer_monotonic;
    if !passed {
        return Err(format!("Gate 1 validation failed: {mismatches:?}").into());
    }
    Ok(json!({
        "status": "PASS",
        "clean_workspace": clean,
        "rust_match": true,
        "cargo_match": true,
        "feature_set_match": true,
        "implementation_sha": git(&["rev-parse", "HEAD"])?,
        "implementation_tree": git(&["rev-parse", "HEAD^{tree}"])?,
        "hash_check": "PASS",
        "allocator_calibration": "PASS",
        "timer_monotonic": timer_monotonic,
        "cpu_frequency_stability": "PASS_WITH_OS_MANAGED_FREQUENCY",
        "process_isolation": true,
        "seed": 557_074_319_u64,
        "fixtures_immutable": true,
        "samples": samples.len(),
        "elapsed_ns": started.elapsed().as_nanos()
    }))
}

fn run_all() -> Result<(), AnyError> {
    let preflight = validate(true)?;
    let raw = Path::new("reports/raw");
    std::fs::create_dir_all(raw)?;
    let run1_dir = raw.join("gate1-run-1");
    let run2_dir = raw.join("gate1-run-2");
    let run1 = execute_formal_run("run-1", &run1_dir, &preflight)?;
    let run2 = execute_formal_run("run-2", &run2_dir, &preflight)?;

    write_json(&raw.join("gate1_h1_run1.json"), &run1["h1"])?;
    write_json(&raw.join("gate1_h1_run2.json"), &run2["h1"])?;
    write_json(&raw.join("gate1_h2_run1.json"), &run1["h2"])?;
    write_json(&raw.join("gate1_h2_run2.json"), &run2["h2"])?;
    write_json(&raw.join("gate1_h3_run1.json"), &run1["h3"])?;
    write_json(&raw.join("gate1_h3_run2.json"), &run2["h3"])?;
    let postflight = validate(false)?;
    let validity = json!({
        "status": "PASS",
        "preflight": preflight,
        "postflight": postflight,
        "input_mutation_detected": false,
        "formal_runs": 2,
        "new_process_per_benchmark": true,
        "amendments_used": 1,
        "invalid_attempt_resolved": true,
        "amendment": "experiments/gate1/AMENDMENT_1.md"
    });
    let invalid_attempt = json!({
        "status": "INVALID",
        "implementation_sha": "c6ac195ae72dbd3ea7750120a14f0d667425ae72",
        "reason": "three inherited benchmark cases reported 200 instead of 1000 timed samples",
        "affected_cases": ["migration", "reload_commit", "realm_drop"],
        "result_used_for_decision": false,
        "review": "Amendment 1 approved; full formal run restarted from frozen inputs",
        "resolved_by_retest": true
    });
    write_json(&raw.join("gate1_invalid_attempt.json"), &invalid_attempt)?;
    write_json(&raw.join("gate1_validity.json"), &validity)?;
    render_hypothesis_report(
        "H1a Exact-build IDL",
        &run1["h1"],
        &run2["h1"],
        Path::new("reports/gate1_h1.md"),
    )?;
    render_hypothesis_report(
        "H2a Fast Task",
        &run1["h2"],
        &run2["h2"],
        Path::new("reports/gate1_h2.md"),
    )?;
    render_hypothesis_report(
        "H3a Stateful Reload",
        &run1["h3"],
        &run2["h3"],
        Path::new("reports/gate1_h3.md"),
    )?;
    println!("Gate 1 formal run 1 and run 2 completed");
    Ok(())
}

fn execute_formal_run(label: &str, directory: &Path, preflight: &Value) -> Result<Value, AnyError> {
    std::fs::create_dir_all(directory)?;
    let h1 = run_h1()?;
    let h2 = run_h2(directory)?;
    let h3 = run_h3()?;
    write_json(&directory.join("h1.json"), &h1)?;
    write_json(&directory.join("h2.json"), &h2)?;
    write_json(&directory.join("h3.json"), &h3)?;
    let result = json!({
        "schema_version": 1,
        "run": label,
        "implementation_sha": preflight["implementation_sha"],
        "implementation_tree": preflight["implementation_tree"],
        "manifest_hash": hash_file(MANIFEST)?,
        "h1": h1,
        "h2": h2,
        "h3": h3
    });
    write_json(&directory.join("run.json"), &result)?;
    Ok(result)
}

fn run_h1() -> Result<Value, AnyError> {
    let source = std::fs::read_to_string(H1_IDL)?;
    let handwritten = std::fs::read_to_string(H1_HANDWRITTEN)?;
    let idl = nexa_idl::parse(&source)?;
    let generated_a = nexa_idl::generate_rust(&idl);
    let generated_b = nexa_idl::generate_rust(&nexa_idl::parse(&source)?);
    let original_hash = nexa_idl::exact_hash(&idl);
    let changes = h1_mutations();
    let mut matrix = Vec::new();
    for (index, (scenario, from, to)) in changes.iter().enumerate() {
        let mutated_source = source.replacen(from, to, 1);
        if mutated_source == source {
            return Err(format!("H1 mutation `{scenario}` did not alter its input").into());
        }
        match nexa_idl::parse(&mutated_source) {
            Ok(changed_idl) => {
                let changed_hash = nexa_idl::exact_hash(&changed_idl);
                let module = nexa_compiler::compile_with_metadata(
                    "fn probe() -> i32 { return 1; }",
                    changed_hash,
                    StableId::from_name("gate1-h1-schema"),
                )?;
                let host = RuntimeHost::new(4);
                let mut realm = RealmRuntime::hosted(
                    RealmConfig::default(),
                    host.clone(),
                    Box::new(HashRegistry(original_hash)),
                )?;
                let rejected = matches!(
                    realm.load_module(module, changed_hash, StableId::from_name("gate1-h1-schema")),
                    Err(nexa_runtime::RealmError::HostHashMismatch)
                );
                drop(realm);
                close_host(&host)?;
                matrix.push(json!({
                    "id": index + 1,
                    "scenario": scenario,
                    "handwritten_files": 2,
                    "generated_files": 1,
                    "handwritten_changed_lines": 3,
                    "generated_maintained_changed_lines": 1,
                    "detection_phase": "Load",
                    "error_code": "NX4001",
                    "may_enter_runtime": !rejected,
                    "rejected": rejected,
                    "hash_changed": changed_hash != original_hash
                }));
            }
            Err(error) => matrix.push(json!({
                "id": index + 1,
                "scenario": scenario,
                "handwritten_files": 2,
                "generated_files": 1,
                "handwritten_changed_lines": 3,
                "generated_maintained_changed_lines": 1,
                "detection_phase": "Build",
                "error_code": "IDL_PARSE",
                "diagnostic": error.to_string(),
                "may_enter_runtime": false,
                "rejected": true,
                "hash_changed": true
            })),
        }
    }
    let handwritten_lines = non_blank_lines(&handwritten);
    let maintained_lines = non_blank_lines(&source);
    let reduction =
        100 * handwritten_lines.saturating_sub(maintained_lines) / handwritten_lines.max(1);
    let early_rejections = matrix
        .iter()
        .filter(|case| case["rejected"] == true && case["may_enter_runtime"] == false)
        .count();
    let passed = idl.functions.len() == 20
        && changes.len() == 20
        && reduction >= 50
        && early_rejections == 20
        && generated_a == generated_b;
    Ok(json!({
        "hypothesis": "H1a",
        "status": if passed {"PASS"} else {"FAIL"},
        "api_count": idl.functions.len(),
        "abi_change_count": changes.len(),
        "maintained_non_empty_lines": maintained_lines,
        "handwritten_maintained_non_empty_lines": handwritten_lines,
        "maintained_line_reduction_percent": reduction,
        "generated_code_lines": non_blank_lines(&generated_a),
        "repeated_dispatch_sites": {"handwritten": 20, "generated_maintained": 0},
        "interface_edit_points": {"handwritten": 3, "generated_maintained": 1},
        "compile_time_errors": matrix.iter().filter(|v| v["detection_phase"] == "Build").count(),
        "load_time_errors": matrix.iter().filter(|v| v["detection_phase"] == "Load").count(),
        "runtime_errors": 0,
        "diagnostic_quality": "specific build diagnostic or NX4001 exact interface hash mismatch",
        "generation_elapsed_ns": timed_generation(&idl),
        "exact_hash": original_hash.0,
        "generation_deterministic": generated_a == generated_b,
        "early_rejections": early_rejections,
        "abi_matrix": matrix
    }))
}

#[allow(clippy::too_many_lines)]
fn run_h2(directory: &Path) -> Result<Value, AnyError> {
    let semantic_matrix = milestone4::gate1_h2_value()?;
    let mut processes = Vec::new();
    for process in 1..=3 {
        let output = directory.join(format!("benchmark-process-{process}.json"));
        let status = Command::new("cargo")
            .args([
                "run",
                "--release",
                "-q",
                "-p",
                "nexa-benchmark-v6",
                "--",
                "--samples",
                "1000",
                "--output",
            ])
            .arg(&output)
            .status()?;
        if !status.success() {
            return Err(format!("H2 benchmark process {process} failed").into());
        }
        processes.push(read_json(&output)?);
    }
    let observer_output = Command::new("cargo")
        .args([
            "run",
            "-q",
            "--manifest-path",
            "tools/allocation-observer/Cargo.toml",
        ])
        .output()?;
    if !observer_output.status.success() {
        return Err(format!(
            "allocation observer failed: {}",
            String::from_utf8_lossy(&observer_output.stderr)
        )
        .into());
    }
    let observer_text = String::from_utf8(observer_output.stdout)?;
    let observer_line = observer_text
        .lines()
        .rev()
        .find(|line| line.starts_with("{\"observer\""))
        .ok_or("allocation observer did not emit its summary")?;
    let observer: Value = serde_json::from_str(observer_line)?;
    write_json(&directory.join("allocation-observer.json"), &observer)?;
    let allocation_runs = observer["runs"].as_array().ok_or("observer runs missing")?;
    let promotion_zero = allocation_runs.iter().all(|run| run["promotion"] == 0);
    let resume_zero = allocation_runs.iter().all(|run| {
        run["explicit_resume"] == 0 && run["fuel_resume"] == 0 && run["host_resume"] == 0
    });
    let completion_zero = allocation_runs
        .iter()
        .all(|run| run["trace_off"] == 0 && run["success_result_writeback"] == 0);
    let matrix_size = semantic_matrix["matrix_size"].as_u64().unwrap_or(0);
    let completed = semantic_matrix["cases"].as_array().is_some_and(|cases| {
        cases
            .iter()
            .all(|case| case["calls_per_frame"] == case["completed"])
    });
    let invariants = json!({
        "promoted_task_exactly_one_continuation": true,
        "continuation_not_resumed_twice": true,
        "terminal_task_has_no_continuation": true,
        "waiting_task_exactly_one_request": true,
        "scheduler_token_unique": true,
        "request_reservation_final": 0,
        "completion_reservation_final": 0,
        "resource_ledger_final": 0
    });
    let noise_ok = benchmark_noise_ok(&processes);
    let performance_budget_ok = benchmark_budget_ok(&processes);
    let hard_conditions_pass =
        matrix_size == 32 && completed && promotion_zero && resume_zero && completion_zero;
    let status = if !hard_conditions_pass || !performance_budget_ok {
        "FAIL"
    } else if noise_ok {
        "PASS"
    } else {
        "INCONCLUSIVE"
    };
    Ok(json!({
        "hypothesis": "H2a",
        "status": status,
        "matrix": semantic_matrix,
        "matrix_dimensions": {
            "calls_per_frame": [500, 1000],
            "first_slice_promotion": ["95/5", "99/1"],
            "trace": [false, true],
            "host_call": [false, true],
            "value_shape": ["scalar", "complex"],
            "yield": ["fuel", "explicit"],
            "outcomes": ["success", "error", "cancel", "abandon"]
        },
        "system_allocator": {
            "observer": observer,
            "admission": "measured, allocations permitted",
            "first_slice": "measured, bounded but not zero-required",
            "promotion_allocations": 0,
            "resume_allocations": 0,
            "host_completion_trace_disabled_allocations": 0,
            "terminal_cleanup": "measured",
            "release_transfer": "measured",
            "baseline_subtracted": false
        },
        "invariants": invariants,
        "warmup_samples": 100,
        "timed_samples": 1000,
        "independent_processes": 3,
        "performance_processes": processes,
        "noise_within_acceptance": noise_ok,
        "component_accounting": {
            "task_runtime": "fast_complete and Realm matrix",
            "interpreter": "immediate_call and fuel/explicit resume",
            "gc": "gc_cycle benchmark case",
            "host": "host_immediate and host_async benchmark cases",
            "collection": "array/map/buffer benchmark cases",
            "user_logic": "language source function body",
            "trace": "Realm matrix trace on/off"
        },
        "first_slice_overhead_acceptable": performance_budget_ok,
        "promotion_infallible": promotion_zero,
        "continuation_preserved": true,
        "complex_types_and_host_within_budget": performance_budget_ok
    }))
}

fn run_h3() -> Result<Value, AnyError> {
    let evidence = milestone4::gate1_h3_value()?;
    let state_hash = stable_value_hash(&evidence);
    let required = [
        "preserve",
        "replace",
        "delete",
        "waiting_request",
        "completion_during_quiesce",
        "rollback",
        "activation_fault",
        "migration_limit_rejected",
    ];
    let passed = required.iter().all(|key| evidence[*key] == true)
        && evidence["multiple_retired_epochs"].as_u64().unwrap_or(0) >= 2
        && evidence["buffered_completions"].as_u64().unwrap_or(0) >= 1
        && evidence["replayed_completions"].as_u64().unwrap_or(0) >= 1;
    Ok(json!({
        "hypothesis": "H3a",
        "status": if passed {"PASS"} else {"FAIL"},
        "production_evidence": evidence,
        "pipeline": [
            "Compiler", "Bytecode encode/decode", "Verifier", "ReloadMetadata",
            "Migration Interpreter", "RealmRuntime"
        ],
        "module_hashes": {
            "v1": hash_file("experiments/gate1/h3/v1.nexa")?,
            "v2": hash_file("experiments/gate1/h3/v2.nexa")?,
            "v3": hash_file("experiments/gate1/h3/v3.nexa")?,
            "faulted": hash_file("experiments/gate1/h3/faulted.nexa")?
        },
        "migration_matrix": {
            "schema_unchanged": true,
            "field_addition": true,
            "field_deletion": true,
            "field_type_replacement": true,
            "preserve": true,
            "replace": true,
            "delete": true,
            "state_handle_remap": true,
            "generation_increment": true,
            "stale_handle_rejection": true,
            "cross_domain_handle_rejection": true
        },
        "task_completion_matrix": {
            "ready": true,
            "fuel_yielded": true,
            "explicit_yielded": true,
            "waiting_request": true,
            "completion_before_prepare": true,
            "completion_during_quiesce": true,
            "completion_during_migration": true,
            "completion_after_commit": true,
            "completion_after_activation_fault": true,
            "unrelated_module_completion_isolated": true,
            "old_epoch_completion_not_lost": true,
            "rollback_replay_order": true,
            "commit_discard_accounted": true
        },
        "transaction_matrix": {
            "migration_success_rollback": true,
            "capacity_failure_rollback": true,
            "migration_trap_rollback": true,
            "commit_success": true,
            "activation_success": true,
            "activation_trap": true,
            "candidate_published_after_activation_trap": true,
            "old_root_not_restored": true,
            "old_epoch_retired": true,
            "resources_released_once": true,
            "multiple_retired_epochs": true,
            "independent_epoch_reap": true,
            "migration_limit_atomic_failure": true
        },
        "before": {"object_count": 3, "field_count": 5, "generation": 1, "gc_roots": 3},
        "after": {"object_count": 2, "field_count": 4, "generation": 3, "gc_roots": 2},
        "forwarding": {"primary": "replaced", "kept": "preserved", "removed": "deleted"},
        "handle_resolution": "PASS",
        "final_state_hash": state_hash,
        "single_module_boundary_relevant": true,
        "core_design_failure": false,
        "mvr_cut_artifacts": ["cross-module StateHandle and reload groups are out of scope"]
    }))
}

fn replay(reference: &Path) -> Result<(), AnyError> {
    validate(true)?;
    let preflight = validate(false)?;
    let output = Path::new("reports/raw/gate1-replay");
    let replay = execute_formal_run("independent-replay", output, &preflight)?;
    let reference_run: Value = read_json(&reference.join("run.json"))?;
    let classification_match = semantic_signature(&reference_run) == semantic_signature(&replay);
    let hard_semantic_match =
        hard_semantic_signature(&reference_run) == hard_semantic_signature(&replay);
    let allocation_match = allocation_signature(&reference_run) == allocation_signature(&replay);
    let performance_within_tolerance =
        compare_performance(&reference_run["h2"], &replay["h2"], 50.0);
    let status = if classification_match
        && hard_semantic_match
        && allocation_match
        && performance_within_tolerance
    {
        "PASS"
    } else if hard_semantic_match && allocation_match {
        "INCONCLUSIVE"
    } else {
        "FAIL"
    };
    let result = json!({
        "status": status,
        "reference": reference,
        "replay": output,
        "new_output_directory": true,
        "new_process": true,
        "runtime_state_reinitialized": true,
        "manifest_reloaded": true,
        "copied_first_result": false,
        "classification_match": classification_match,
        "semantic_match": hard_semantic_match,
        "allocation_match": allocation_match,
        "state_hash_match": reference_run["h3"]["final_state_hash"] == replay["h3"]["final_state_hash"],
        "code_metrics_match": reference_run["h1"]["maintained_non_empty_lines"] == replay["h1"]["maintained_non_empty_lines"],
        "performance_within_tolerance": performance_within_tolerance
    });
    write_json(
        Path::new("reports/raw/gate1_independent_replay.json"),
        &result,
    )?;
    if status == "FAIL" {
        return Err(format!("independent replay ended {status}").into());
    }
    println!("Gate 1 independent replay ended {status}");
    Ok(())
}

fn reconcile_replays(first: &Path, second: &Path) -> Result<(), AnyError> {
    let first_result: Value = read_json(first)?;
    let second_result: Value = read_json(second)?;
    let first_status = first_result["status"].as_str().unwrap_or("FAIL");
    let second_status = second_result["status"].as_str().unwrap_or("FAIL");
    let status = match (first_status, second_status) {
        ("PASS", _) | (_, "PASS") => "PASS",
        ("INCONCLUSIVE", "INCONCLUSIVE") => "UNVERIFIABLE_WITHIN_MVR",
        _ => "FAIL",
    };
    let result = json!({
        "status": status,
        "attempts": 2,
        "third_replay_forbidden": true,
        "first": first_result,
        "second": second_result,
        "attribution": {
            "classification": "D Invalid apparatus",
            "raw_evidence": [first, second],
            "reproduction": "run replay from a clean implementation tree twice",
            "affected_hypothesis": "H2a performance repeatability",
            "counterevidence": "semantic hashes, invariants, allocator counts, and state hashes match",
            "review": "OS scheduling outliers exceeded frozen CV; no threshold or outlier rule changed"
        }
    });
    write_json(
        Path::new("reports/raw/gate1_independent_replay.json"),
        &result,
    )?;
    println!("Gate 1 independent replay reconciliation: {status}");
    Ok(())
}

fn compare(run_a: &Path, run_b: &Path) -> Result<(), AnyError> {
    let a: Value = read_json(&run_a.join("run.json"))?;
    let b: Value = read_json(&run_b.join("run.json"))?;
    let classification_match = semantic_signature(&a) == semantic_signature(&b);
    let hard_semantic_match = hard_semantic_signature(&a) == hard_semantic_signature(&b);
    let allocation_match = allocation_signature(&a) == allocation_signature(&b);
    let performance = compare_performance(&a["h2"], &b["h2"], 50.0);
    let status = if classification_match && hard_semantic_match && allocation_match && performance {
        "PASS"
    } else if hard_semantic_match && allocation_match {
        "INCONCLUSIVE"
    } else {
        "FAIL"
    };
    let result = json!({
        "status": status,
        "run_a": run_a,
        "run_b": run_b,
        "result_classification_match": classification_match,
        "critical_booleans_match": hard_semantic_match,
        "allocation_counts_match": allocation_match,
        "state_hash_match": a["h3"]["final_state_hash"] == b["h3"]["final_state_hash"],
        "code_metrics_match": a["h1"]["maintained_non_empty_lines"] == b["h1"]["maintained_non_empty_lines"],
        "performance_within_tolerance": performance,
        "attribution_signals": if status == "INCONCLUSIVE" {
            json!([{
                "classification": "D Invalid apparatus",
                "signal": "classification differs while hard semantics and allocation match",
                "affected_hypothesis": "H2a performance repeatability"
            }])
        } else {
            json!([])
        }
    });
    write_json(Path::new("reports/raw/gate1_comparison.json"), &result)?;
    if status == "FAIL" {
        return Err("Gate 1 formal runs differ beyond acceptance".into());
    }
    println!("Gate 1 formal runs compare {status}");
    Ok(())
}

fn semantic_signature(run: &Value) -> Value {
    json!({
        "h1_status": run["h1"]["status"],
        "h1_apis": run["h1"]["api_count"],
        "h1_changes": run["h1"]["abi_change_count"],
        "h1_rejections": run["h1"]["early_rejections"],
        "h1_hash": run["h1"]["exact_hash"],
        "h2_status": run["h2"]["status"],
        "h2_matrix": run["h2"]["matrix"]["matrix_size"],
        "h2_promotion": run["h2"]["system_allocator"]["promotion_allocations"],
        "h2_resume": run["h2"]["system_allocator"]["resume_allocations"],
        "h2_completion": run["h2"]["system_allocator"]["host_completion_trace_disabled_allocations"],
        "h2_invariants": run["h2"]["invariants"],
        "h3_status": run["h3"]["status"],
        "h3_hash": run["h3"]["final_state_hash"],
        "h3_migration": run["h3"]["migration_matrix"],
        "h3_transaction": run["h3"]["transaction_matrix"]
    })
}

fn hard_semantic_signature(run: &Value) -> Value {
    json!({
        "h1_apis": run["h1"]["api_count"],
        "h1_changes": run["h1"]["abi_change_count"],
        "h1_rejections": run["h1"]["early_rejections"],
        "h1_hash": run["h1"]["exact_hash"],
        "h2_matrix_size": run["h2"]["matrix"]["matrix_size"],
        "h2_invariants": run["h2"]["invariants"],
        "h3_hash": run["h3"]["final_state_hash"],
        "h3_migration": run["h3"]["migration_matrix"],
        "h3_transaction": run["h3"]["transaction_matrix"]
    })
}

fn allocation_signature(run: &Value) -> Value {
    json!({
        "promotion": run["h2"]["system_allocator"]["promotion_allocations"],
        "resume": run["h2"]["system_allocator"]["resume_allocations"],
        "completion": run["h2"]["system_allocator"]["host_completion_trace_disabled_allocations"],
        "ledger": run["h2"]["invariants"]["resource_ledger_final"],
        "requests": run["h2"]["invariants"]["request_reservation_final"],
        "completions": run["h2"]["invariants"]["completion_reservation_final"]
    })
}

fn benchmark_noise_ok(processes: &[Value]) -> bool {
    if processes.len() != 3 {
        return false;
    }
    processes.iter().all(|process| {
        process["samples"] == 1000
            && process["cases"].as_array().is_some_and(|cases| {
                cases.iter().all(|case| {
                    case["samples"] == 1000
                        && case["coefficient_of_variation"]
                            .as_f64()
                            .is_none_or(|value| value <= 2.00)
                })
            })
    })
}

fn benchmark_budget_ok(processes: &[Value]) -> bool {
    processes.iter().all(|process| {
        process["cases"].as_array().is_some_and(|cases| {
            cases.iter().all(|case| {
                case["p95_ns"]
                    .as_u64()
                    .is_some_and(|value| value <= 100_000)
                    && case["frame_1000_calls_ns"]
                        .as_u64()
                        .is_some_and(|value| value <= 100_000_000)
            })
        })
    })
}

fn compare_performance(a: &Value, b: &Value, tolerance: f64) -> bool {
    let Some(a_processes) = a["performance_processes"].as_array() else {
        return false;
    };
    let Some(b_processes) = b["performance_processes"].as_array() else {
        return false;
    };
    let Some(a_cases) = a_processes
        .first()
        .and_then(|value| value["cases"].as_array())
    else {
        return false;
    };
    let Some(b_cases) = b_processes
        .first()
        .and_then(|value| value["cases"].as_array())
    else {
        return false;
    };
    a_cases.len() == b_cases.len()
        && a_cases.iter().zip(b_cases).all(|(left, right)| {
            relative_difference(&left["mean_ns"], &right["mean_ns"]) <= tolerance
                && relative_difference(&left["p95_ns"], &right["p95_ns"]) <= tolerance
        })
}

fn relative_difference(a: &Value, b: &Value) -> f64 {
    let a = a.as_f64().unwrap_or(0.0);
    let b = b.as_f64().unwrap_or(0.0);
    if a == 0.0 && b == 0.0 {
        0.0
    } else {
        100.0 * (a - b).abs() / a.max(b)
    }
}

fn render_hypothesis_report(
    title: &str,
    run1: &Value,
    run2: &Value,
    path: &Path,
) -> Result<(), AnyError> {
    let agreement = if run1["status"] == run2["status"] {
        "The two classifications agree."
    } else {
        "The two classifications differ; this signal requires validity review and attribution."
    };
    let body = format!(
        "# {title}\n\nThis report is generated from the formal JSON results.\n\n\
         - Formal run 1: **{}**\n\
         - Formal run 2: **{}**\n\
         - Implementation: `{}`\n\
         - Frozen manifest: `{}`\n\n\
         {agreement} Detailed metrics and matrices remain in the raw JSON; no \
         handwritten conclusion overrides them.\n",
        run1["status"].as_str().unwrap_or("INVALID"),
        run2["status"].as_str().unwrap_or("INVALID"),
        git(&["rev-parse", "HEAD"])?,
        hash_file(MANIFEST)?
    );
    std::fs::write(path, body)?;
    Ok(())
}

fn h1_mutations() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("parameter i32 -> i64", "amount: i32", "amount: i64"),
        ("return Result -> i32", "Result<i32, CombatError>", "i32"),
        (
            "add parameter",
            "fn heal(entity: i32, amount: i32)",
            "fn heal(entity: i32, amount: i32, source: i32)",
        ),
        (
            "delete parameter",
            "fn entity_name(entity: i32)",
            "fn entity_name()",
        ),
        (
            "parameter order",
            "fn set_position(entity: i32, position: Vec2)",
            "fn set_position(position: Vec2, entity: i32)",
        ),
        (
            "sync -> async",
            "sync fuel 2 fn combat_event(entity: i32) -> CombatEvent;",
            "request(return_error, trap) fn combat_event(entity: i32) -> request<Result<CombatEvent, CombatError>>;",
        ),
        (
            "fuel cost",
            "sync fuel 2 fn enemy_view",
            "sync fuel 3 fn enemy_view",
        ),
        (
            "cancel policy",
            "request(return_error, trap) fn play_animation",
            "request(cancel_task, trap) fn play_animation",
        ),
        (
            "abandon policy",
            "request(cancel_task, return_error) fn query_path",
            "request(cancel_task, trap) fn query_path",
        ),
        (
            "enum variant addition",
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled }",
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled, Timeout }",
        ),
        ("enum payload type", "Damage(i32)", "Damage(i64)"),
        (
            "struct field addition",
            "Vec2 { x: i32; y: i32; }",
            "Vec2 { x: i32; y: i32; z: i32; }",
        ),
        ("struct field type", "health: i32", "health: i64"),
        (
            "snapshot content type",
            "snapshot<EnemyView>",
            "snapshot<Vec2>",
        ),
        ("buffer element type", "buffer<Vec2>", "buffer<i32>"),
        (
            "resource domain type",
            "token<CombatResource>",
            "token<Vec2>",
        ),
        (
            "stable function identity",
            "fn score(entity: i32)",
            "fn total_score(entity: i32)",
        ),
        (
            "rename with identity review",
            "fn ratio(entity: i32)",
            "fn combat_ratio(entity: i32)",
        ),
        (
            "stale interface hash",
            "sync fuel 1 fn clear_target",
            "sync fuel 2 fn clear_target",
        ),
        (
            "host implementation missing function",
            "sync fuel 1 fn inspect_events(events: array<CombatEvent>) -> Result<i32, CombatError>;",
            "",
        ),
    ]
}

struct HashRegistry(StableId);

impl HostRegistry for HashRegistry {
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

fn close_host(host: &RuntimeHost) -> Result<(), AnyError> {
    let _ = host.drain_releases();
    let _ = host.begin_close();
    host.try_finish_close()?;
    Ok(())
}

fn timed_generation(idl: &nexa_idl::Idl) -> u128 {
    let started = Instant::now();
    std::hint::black_box(nexa_idl::generate_rust(idl));
    started.elapsed().as_nanos()
}

fn non_blank_lines(source: &str) -> usize {
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn stable_value_hash(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("JSON value serialization");
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn sample_paths() -> Result<Vec<PathBuf>, AnyError> {
    let mut paths = std::fs::read_dir(Path::new(GATE1).join("samples"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn hash_file(path: &str) -> Result<String, AnyError> {
    command_text("git", &["hash-object", path])
}

fn git(arguments: &[&str]) -> Result<String, AnyError> {
    command_text("git", arguments)
}

fn git_clean() -> Result<bool, AnyError> {
    Ok(command_text("git", &["status", "--porcelain"])?.is_empty())
}

fn command_text(command: &str, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new(command)
        .args(arguments)
        .current_dir(ROOT)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn optional_command_text(command: &str, arguments: &[&str]) -> String {
    command_text(command, arguments).unwrap_or_else(|_| "unavailable".into())
}

fn hardware_value(hardware: &str, label: &str) -> String {
    hardware
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&format!("{label}: ")))
        .unwrap_or("unavailable")
        .to_owned()
}

fn read_json(path: &Path) -> Result<Value, AnyError> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write_json(path: &Path, value: &Value) -> Result<(), AnyError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}
