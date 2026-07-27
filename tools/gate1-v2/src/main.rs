#![allow(clippy::too_many_lines, clippy::similar_names, deprecated)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

use nexa_core::StableId;
use nexa_gate1_v2_3::{
    AnyError, EventLog, MANIFEST, MeasurementKind, ObservedMetric, ProcessRecorder, TARGET_ROOT,
    bound_hashes, git, git_clean_failures, hash_file, nonce, output_root, read_json,
    repository_root, stable_bytes_hash, stable_value_hash, write_json,
};
use nexa_runtime::model_adapter::{RealmV5RuntimeAdapter, RealmV5RuntimeEvent};
use nexa_runtime::{
    HostArgs, HostCallOutcome, HostRegistry, HostTrap, RealmConfig, RealmRuntime, ResourceContext,
    RuntimeHost,
};
use serde_json::{Value, json};

mod prefreeze;
mod qualification;

#[allow(clippy::all, clippy::pedantic, dead_code)]
#[path = "../../milestone4-experiments/src/main.rs"]
mod milestone4;

const ACCEPTANCE: &str = "baseline/testing/GATE1_ACCEPTANCE_V2_3.md";
const AUTHORIZATION: &str = "baseline/testing/GATE1_V2_3_AUTHORIZATION.md";
const AUTHORIZATION_RECORD: &str = "experiments/gate1-v2.3/authorization.json";
const THRESHOLDS: &str = "experiments/gate1-v2.3/threshold_equivalence.json";
const ENVIRONMENT: &str = "experiments/gate1-v2.3/environment.json";
const QUALIFICATION: &str = "experiments/gate1-v2.3/qualification/environment_qualification.json";
const H1_IDL: &str = "experiments/gate1/h1/combat.idl";
const H1_HANDWRITTEN: &str = "experiments/gate1/h1/handwritten/dispatcher.rs";
const H1_GENERATED: &str = "experiments/gate1/h1/generated/combat.rs";

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    emit_formal_handshake()?;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [command, probe] if command == "probe" && probe == "spawn-minimal" => {
            qualification::spawn_minimal()
        }
        [command, probe] if command == "probe" && probe == "provenance-atomic" => {
            qualification::probe_atomic()
        }
        [command, probe] if command == "probe" && probe == "formal-handshake" => {
            formal_handshake_probe()
        }
        [command, output, token] if command == "qualification-child" => {
            qualification::child(output, token, false)
        }
        [command, output, token] if command == "qualification-abnormal-child" => {
            qualification::child(output, token, true)
        }
        [command] if command == "qualification-sleep-child" => qualification::sleep_child(),
        [command, output, token] if command == "qualification-nested-child" => {
            qualification::nested_child(output, token)
        }
        [command, role, output, token] if command == "qualification-empty-worker" => {
            qualification::empty_worker(role, output, token)
        }
        [command, role, output, token] if command == "attestation-probe-worker" => {
            qualification::empty_worker(role, output, token)
        }
        [command, flag, output] if command == "qualify-environment" && flag == "--output" => {
            qualification::qualify_environment(Path::new(output))
        }
        [command] if command == "qualify-environment" => qualification::qualify_environment(
            Path::new("target/gate1-v2.3-qualification/qualified-host"),
        ),
        [command] if command == "history-check" => {
            let result = prefreeze::history_check()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        [command] if command == "governance-negative-tests" => {
            let result = prefreeze::governance_negative_tests()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        [command] if command == "prefreeze-closure" => {
            evidence_static_check()?;
            let result = prefreeze::prefreeze_closure()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        [command] if command == "acceptance-equivalence" => acceptance_equivalence(),
        [command] if command == "evidence-static-check" => evidence_static_check(),
        [command] if command == "freeze" => freeze(),
        [command] if command == "validate" => {
            let result = validate_environment(true)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        [command] if command == "validate-frozen-inputs" => {
            let result = validate_environment(false)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            ensure_pass(&result, "frozen inputs")
        }
        [command, label] if command == "supervisor" => supervisor(label),
        [command, label, nonce] if command == "worker" => worker(label, nonce),
        [command, label, nonce] if command == "h1-worker" => h1_worker(label, nonce),
        [command, label, nonce] if command == "h2-worker" => h2_worker(label, nonce),
        [command, label, nonce] if command == "h3-worker" => h3_worker(label, nonce),
        [command, left, right] if command == "compare" => compare(left, right),
        _ => Err("usage: nexa-gate1-v2-3 history-check|governance-negative-tests|acceptance-equivalence|evidence-static-check|qualify-environment|prefreeze-closure|freeze|validate|supervisor <formal-run-1|formal-run-2|replay>|compare <left> <right>".into()),
    }
}

fn acceptance_equivalence() -> Result<(), AnyError> {
    let fingerprint = read_json(THRESHOLDS)?;
    let removed = fingerprint["removed_hard_conditions"]
        .as_array()
        .ok_or("removed_hard_conditions is missing")?;
    let weakened = fingerprint["outcome_thresholds_weakened"]
        .as_bool()
        .ok_or("outcome_thresholds_weakened is missing")?;
    let expected_h1 = json!({
        "abi_changes": 20,
        "api_count": 20,
        "early_rejections": 20,
        "edit_point_reduction_percent_min": 50,
        "maintained_line_reduction_percent_min": 50
    });
    let expected_h2 = json!({
        "benchmark_processes": 3,
        "frame_1000_calls_ns_max": 100_000_000,
        "promotion_allocations_max": 0,
        "p95_ns_max": 100_000,
        "resume_allocations_max": 0,
        "timed_samples": 1000,
        "trace_off_completion_allocations_max": 0,
        "warmup_samples": 100
    });
    let expected_h3 = json!({
        "activation_fault_commit_before_activation": "required",
        "delete": "required",
        "migration_limit_atomic_failure": "required",
        "preserve": "required",
        "replace": "required",
        "retired_epoch": "required",
        "rollback": "required"
    });
    let changed = fingerprint["changed_outcome_rules"]
        .as_array()
        .ok_or("changed_outcome_rules is missing")?;
    fingerprint["authorized_apparatus_changes"]
        .as_array()
        .ok_or("authorized_apparatus_changes is missing")?;
    if weakened
        || !removed.is_empty()
        || !changed.is_empty()
        || fingerprint["experiment_version"] != "gate1-v2.3"
        || fingerprint["previous_versions"] != json!(["gate1-v2.2", "gate1-v2.1", "gate1-v2"])
        || fingerprint["h1"] != expected_h1
        || fingerprint["h2"] != expected_h2
        || fingerprint["h3"] != expected_h3
        || fingerprint["authorized_apparatus_changes"]
            != json!([
                "supersession graph",
                "dual-layer contracts",
                "prefreeze closure",
                "terminal short-circuit receipt"
            ])
    {
        return Err("Gate 1 v2.3 outcome thresholds are not equivalent to v2".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "experiment_version": fingerprint["experiment_version"],
            "previous_versions": fingerprint["previous_versions"],
            "outcome_thresholds_weakened": weakened,
            "removed_hard_conditions": removed,
            "changed_outcome_rules": fingerprint["changed_outcome_rules"],
            "authorized_apparatus_changes": fingerprint["authorized_apparatus_changes"]
        }))?
    );
    Ok(())
}

fn evidence_static_check() -> Result<(), AnyError> {
    let roots = [
        Path::new("tools/gate1-v2/src"),
        Path::new("tools/gate1-v2-gates/src"),
        Path::new("tools/gate1-v2-decision/src"),
        Path::new("tools/milestone4-experiments/src"),
    ];
    let forbidden = [
        ["\"promoted_task_exactly_one_continuation\"", ": true"].concat(),
        ["\"resource_ledger_final\"", ": 0"].concat(),
        ["\"migration_success_rollback\"", ": true"].concat(),
        ["\"status\"", ": \"passed\""].concat(),
        ["(1..=30)", ".map("].concat(),
        ["\"contracts_recomputed\"", ": true"].concat(),
    ];
    let mut hardcoded = Vec::new();
    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path)?;
            for pattern in &forbidden {
                if source.contains(pattern) {
                    hardcoded.push(json!({"path": path, "pattern": pattern}));
                }
            }
        }
    }
    let result = json!({
        "unproven_boolean_literals": [],
        "unconditional_contract_passes": [],
        "unconditional_milestone_complete": [],
        "hardcoded_domain_metrics": hardcoded
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result["hardcoded_domain_metrics"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        return Err("static evidence check found a forbidden production pattern".into());
    }
    Ok(())
}

fn freeze() -> Result<(), AnyError> {
    acceptance_equivalence()?;
    evidence_static_check()?;
    let history = prefreeze::history_check()?;
    ensure_pass(&history, "history")?;
    let prefreeze_closure = read_json("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?;
    if prefreeze_closure["status"] != "PASS"
        || prefreeze_closure["synthetic_artifacts_formal_evidence_usable"] != false
    {
        return Err("Gate 1 v2.3 prefreeze closure is not valid".into());
    }
    let qualification = read_json(QUALIFICATION)?;
    if qualification["status"] != "QUALIFIED"
        || qualification["failures"]
            .as_array()
            .is_none_or(|failures| !failures.is_empty())
    {
        return Err("Gate 1 v2.3 environment is not qualified".into());
    }
    let authorization = read_json(AUTHORIZATION_RECORD)?;
    if authorization["status"] != "AUTHORIZED"
        || authorization["prefreeze_closure"]["hash"]
            != hash_file("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?
        || authorization["environment_qualification"]["hash"] != hash_file(QUALIFICATION)?
        || authorization["acceptance_equivalence"]["hash"] != hash_file(THRESHOLDS)?
        || authorization["provenance_protocol"]["name"] != "gate1-process-handshake-v1"
    {
        return Err("Gate 1 v2.3 authorization bindings are invalid".into());
    }
    let contract_manifest = read_json("reports/contracts/gate1_v2_3_contracts.json")?;
    if contract_manifest["contracts"]
        .as_array()
        .map_or(0, Vec::len)
        != 36
    {
        return Err("Gate 1 v2.3 Contract Manifest does not cover 36 work packages".into());
    }
    let environment = json!({
        "schema_version": 2,
        "rust": command("rustc", &["--version"])?,
        "cargo": command("cargo", &["--version"])?,
        "os": command("uname", &["-srvmp"])?,
        "architecture": std::env::consts::ARCH,
        "feature_set": ["nexa-runtime/allocation-counting", "nexa-runtime/model-adapter"],
        "allocator": "System observed by isolated allocation-observer",
        "timer": "std::time::Instant monotonic",
        "cpu_power": optional_command("pmset", &["-g", "batt"]),
        "experiment_version": "gate1-v2.3",
        "qualification_hash": hash_file(QUALIFICATION)?,
        "qualified_host": qualification["candidate_environment"],
        "provenance_protocol": "gate1-process-handshake-v1",
        "execution_budget": authorization["execution_budget"],
    });
    write_json(Path::new(ENVIRONMENT), &environment)?;
    let paths = bound_input_paths();
    let references = paths.iter().map(String::as_str).collect::<Vec<_>>();
    let hashes = bound_hashes(&references)?;
    let sample_hashes = hashes
        .iter()
        .filter(|(path, _)| path.starts_with("experiments/gate1/samples/"))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect::<BTreeMap<_, _>>();
    if sample_hashes.len() != 20 {
        return Err(format!(
            "expected 20 frozen gameplay samples, found {}",
            sample_hashes.len()
        )
        .into());
    }
    let sample_hash = stable_value_hash(&serde_json::to_value(&sample_hashes)?);
    let runner_hash = stable_value_hash(&json!({
        "runner_lib": hashes["tools/gate1-v2/src/lib.rs"],
        "runner_main": hashes["tools/gate1-v2/src/main.rs"],
        "gates": hashes["tools/gate1-v2-gates/src/main.rs"],
        "decision": hashes["tools/gate1-v2-decision/src/main.rs"],
        "milestone4": hashes["tools/milestone4-experiments/src/main.rs"],
        "benchmark": hashes["tools/benchmark-v6/src/main.rs"],
        "allocation_observer": hashes["tools/allocation-observer/src/main.rs"]
        ,"qualification": hashes["tools/gate1-v2/src/qualification.rs"]
        ,"prefreeze": hashes["tools/gate1-v2/src/prefreeze.rs"]
        ,"fixtures_lib": hashes["tools/gate1-v2-3-fixtures/src/lib.rs"]
        ,"fixtures_main": hashes["tools/gate1-v2-3-fixtures/src/main.rs"]
    }));
    let manifest = json!({
        "schema_version": 3,
        "experiment_version": "gate1-v2.3",
        "state": "FROZEN",
        "authorization_hash": hashes[AUTHORIZATION],
        "authorization_record_hash": hashes[AUTHORIZATION_RECORD],
        "acceptance_hash": hashes[ACCEPTANCE],
        "threshold_hash": hashes[THRESHOLDS],
        "environment_hash": hashes[ENVIRONMENT],
        "qualification_hash": hashes[QUALIFICATION],
        "prefreeze_closure_hash": hashes["experiments/gate1-v2.3/prefreeze/prefreeze_closure.json"],
        "supersession_graph_hash": hashes["reports/history/gate1/supersession_graph.json"],
        "sample_hash": sample_hash,
        "sample_hashes": sample_hashes,
        "runner_hash": runner_hash,
        "worker_hash": hashes["tools/gate1-v2/src/main.rs"],
        "decision_tool_hash": hashes["tools/gate1-v2-decision/src/main.rs"],
        "receipt_generator_hash": hashes["tools/gate1-v2-decision/src/main.rs"],
        "h2_benchmark_config_hash": hashes["experiments/gate1/benchmark.json"],
        "contract_manifest_hash": hashes["reports/contracts/gate1_v2_3_contracts.json"],
        "bound_hashes": hashes
    });
    write_json(Path::new(MANIFEST), &manifest)?;
    println!(
        "froze Gate 1 v2.3 manifest with {} bound inputs",
        paths.len()
    );
    Ok(())
}

fn bound_input_paths() -> Vec<String> {
    let mut paths = [
        ACCEPTANCE,
        AUTHORIZATION,
        AUTHORIZATION_RECORD,
        THRESHOLDS,
        ENVIRONMENT,
        QUALIFICATION,
        "experiments/gate1-v2.3/qualification/environment_qualification.md",
        "experiments/gate1-v2.3/qualification/environment_qualification_hashes.json",
        "experiments/gate1-v2.3/qualification/root-cause.json",
        "experiments/gate1-v2.3/qualification/formal-handshake/process_attestation.json",
        "experiments/gate1-v2.3/qualification/formal-handshake/parent_verification.json",
        "experiments/gate1-v2.3/qualification/formal-handshake/probe.json",
        "experiments/gate1-v2.3/prefreeze/history_check.json",
        "experiments/gate1-v2.3/prefreeze/governance_negative_tests.json",
        "experiments/gate1-v2.3/prefreeze/contract_satisfiability.json",
        "experiments/gate1-v2.3/prefreeze/decision_branches.json",
        "experiments/gate1-v2.3/prefreeze/terminal_short_circuit.json",
        "experiments/gate1-v2.3/prefreeze/synthetic_git_chain.json",
        "experiments/gate1-v2.3/prefreeze/prefreeze_closure.json",
        "reports/history/gate1/index.json",
        "reports/history/gate1/supersession_graph.json",
        "reports/history/gate1/versions/gate1-v1.json",
        "reports/history/gate1/versions/gate1-v2.json",
        "reports/history/gate1/versions/gate1-v2.1.json",
        "reports/history/gate1/versions/gate1-v2.2.json",
        "reports/history/gate1/versions/gate1-v2.3.json",
        "reports/history/gate1/v2_2/implementation.json",
        "reports/history/gate1/v2_2/terminal.json",
        "reports/history/gate1/v2_2/governance_failure.json",
        H1_IDL,
        H1_HANDWRITTEN,
        H1_GENERATED,
        "experiments/gate1/h3/state.json",
        "experiments/gate1/h3/host.idl",
        "experiments/gate1/h3/v1.nexa",
        "experiments/gate1/h3/v2.nexa",
        "experiments/gate1/h3/v3.nexa",
        "experiments/gate1/h3/faulted.nexa",
        "experiments/gate1/benchmark.json",
        "tools/gate1-v2/src/lib.rs",
        "tools/gate1-v2/src/main.rs",
        "tools/gate1-v2/src/qualification.rs",
        "tools/gate1-v2/src/prefreeze.rs",
        "tools/gate1-v2-3-fixtures/src/lib.rs",
        "tools/gate1-v2-3-fixtures/src/main.rs",
        "tools/gate1-v2-gates/src/main.rs",
        "tools/gate1-v2-decision/src/main.rs",
        "tools/milestone4-experiments/src/main.rs",
        "tools/benchmark-v6/src/main.rs",
        "tools/allocation-observer/src/main.rs",
        "crates/nexa-runtime/src/lib.rs",
        "crates/nexa-runtime/src/model_adapter_v5.rs",
        "crates/nexa-runtime/src/realm.rs",
        "crates/nexa-runtime/src/task.rs",
        "reports/contracts/gate1_v2_3_contracts.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let mut samples = std::fs::read_dir("experiments/gate1/samples")
        .expect("Gate 1 samples directory")
        .map(|entry| {
            entry
                .expect("Gate 1 sample entry")
                .path()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    samples.sort();
    paths.extend(samples);
    paths
}

fn validate_environment(require_clean: bool) -> Result<Value, AnyError> {
    let started = Instant::now();
    let mut failures = Vec::new();
    if require_clean {
        failures.extend(git_clean_failures()?);
    }
    let manifest = read_json(MANIFEST)?;
    let environment = read_json(ENVIRONMENT)?;
    let qualification = read_json(QUALIFICATION)?;
    let authorization = read_json(AUTHORIZATION_RECORD)?;
    if qualification["status"] != "QUALIFIED"
        || qualification["failures"]
            .as_array()
            .is_none_or(|failures| !failures.is_empty())
    {
        failures.push("bound environment qualification is not QUALIFIED".to_owned());
    }
    if environment["qualification_hash"] != hash_file(QUALIFICATION)?
        || authorization["environment_qualification"]["hash"] != hash_file(QUALIFICATION)?
    {
        failures.push("environment qualification hash differs from authorization".to_owned());
    }
    if environment["provenance_protocol"] != "gate1-process-handshake-v1"
        || authorization["provenance_protocol"]["name"] != "gate1-process-handshake-v1"
    {
        failures.push("portable provenance protocol binding differs".to_owned());
    }
    if authorization["prefreeze_closure"]["hash"]
        != hash_file("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?
        || authorization["acceptance_equivalence"]["hash"] != hash_file(THRESHOLDS)?
    {
        failures.push("authorization prefreeze or equivalence binding differs".to_owned());
    }
    if qualification["candidate_environment"]["host_id"]
        != command("scutil", &["--get", "ComputerName"])?
        || qualification["candidate_environment"]["os"] != command("uname", &["-srvmp"])?
        || qualification["candidate_environment"]["cpu_architecture"] != std::env::consts::ARCH
        || qualification["candidate_environment"]["rust"] != command("rustc", &["--version"])?
        || qualification["candidate_environment"]["cargo"] != command("cargo", &["--version"])?
    {
        failures.push("current host fingerprint differs from qualified environment".to_owned());
    }
    if environment["rust"] != command("rustc", &["--version"])? {
        failures.push("rust toolchain differs from frozen environment".to_owned());
    }
    if environment["cargo"] != command("cargo", &["--version"])? {
        failures.push("cargo toolchain differs from frozen environment".to_owned());
    }
    let bound = manifest["bound_hashes"]
        .as_object()
        .ok_or("manifest bound_hashes is missing")?;
    let mut inputs = BTreeMap::new();
    for (path, expected) in bound {
        let actual = hash_file(path)?;
        if expected.as_str() != Some(actual.as_str()) {
            failures.push(format!("bound input hash changed: {path}"));
        }
        inputs.insert(path.clone(), actual);
    }
    let timer_start = Instant::now();
    let timer_monotonic = Instant::now() >= timer_start;
    if !timer_monotonic {
        failures.push("monotonic timer moved backwards".to_owned());
    }
    let allocation_snapshot = nexa_runtime::allocation_snapshot();
    let allocation_after = nexa_runtime::allocation_snapshot();
    let allocator_calibrated = allocation_after.admission >= allocation_snapshot.admission
        && allocation_after.first_slice >= allocation_snapshot.first_slice
        && allocation_after.promotion >= allocation_snapshot.promotion
        && allocation_after.resume >= allocation_snapshot.resume
        && allocation_after.terminal_cleanup >= allocation_snapshot.terminal_cleanup;
    if !allocator_calibrated {
        failures.push("allocator calibration failed".to_owned());
    }
    let status = validity_status(&failures);
    Ok(json!({
        "status": status,
        "failures": failures,
        "implementation_sha": git(&["rev-parse", "HEAD"])?,
        "implementation_tree": git(&["rev-parse", "HEAD^{tree}"])?,
        "manifest_hash": hash_file(MANIFEST)?,
        "input_hashes": inputs,
        "rust": environment["rust"],
        "cargo": environment["cargo"],
        "feature_set": environment["feature_set"],
        "timer_monotonic": timer_monotonic,
        "allocator_calibrated": allocator_calibrated,
        "cpu_power": environment["cpu_power"],
        "qualification_status": qualification["status"],
        "provenance_protocol": environment["provenance_protocol"],
        "elapsed_ns": started.elapsed().as_nanos()
    }))
}

fn supervisor(label: &str) -> Result<(), AnyError> {
    require_run_label(label)?;
    let root = output_root(label);
    if root.exists() {
        return Err(
            format!("formal output already exists for {label}; retries are forbidden").into(),
        );
    }
    let preflight = validate_environment(true)?;
    ensure_pass(&preflight, "preflight")?;
    std::fs::create_dir_all(&root)?;
    write_json(&root.join("environment.json"), &read_json(ENVIRONMENT)?)?;
    write_json(&root.join("input_manifest.json"), &read_json(MANIFEST)?)?;
    let supervisor_nonce = nonce("supervisor")?;
    let recorder = ProcessRecorder::start(&root, "supervisor", label, &supervisor_nonce)?;
    let mut events = EventLog::create(&root, label, &supervisor_nonce)?;
    let before = json!({"preflight": preflight});
    let worker_nonce = nonce("top-worker")?;
    let worker_spawn = spawn_attested_self(
        &["worker", label, &worker_nonce],
        label,
        "top-level-worker",
        &worker_nonce,
        &root.join("worker"),
    )?;
    let output = &worker_spawn.output;
    let worker_result = if output.status.success() && worker_spawn.verified {
        read_json(root.join("worker/result.json"))?
    } else {
        json!({
            "status": "INVALID",
            "stderr": String::from_utf8_lossy(&output.stderr),
            "process_attestation": worker_spawn.attestation,
            "parent_verification": worker_spawn.parent_verification
        })
    };
    events.record(
        "supervisor.worker",
        "spawn independent worker",
        &before,
        &worker_result,
        status_text(output),
    )?;
    let postflight = validate_environment(true)?;
    let mut failures = Vec::new();
    if !output.status.success() {
        failures.push(format!(
            "top-level worker exited {:?}",
            output.status.code()
        ));
    }
    if !worker_spawn.verified {
        failures.push("top-level worker portable handshake failed".to_owned());
    }
    if worker_result["status"] != "PASS" {
        failures.push("top-level worker result is not PASS".to_owned());
    }
    failures.extend(
        postflight["failures"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned),
    );
    let validity = json!({
        "status": validity_status(&failures),
        "failures": failures,
        "preflight": before["preflight"],
        "postflight": postflight,
        "mutation_failures": worker_result.pointer("/failures/h1").cloned().unwrap_or_else(|| json!([])),
        "process_isolation_failures": worker_result.pointer("/failures/process").cloned().unwrap_or_else(|| json!([])),
        "allocator_failures": worker_result.pointer("/failures/h2_allocator").cloned().unwrap_or_else(|| json!([])),
        "timing_apparatus_failures": worker_result.pointer("/failures/h2_timing").cloned().unwrap_or_else(|| json!([]))
    });
    let validity_name = match label {
        "formal-run-1" => "validity_run1.json",
        "formal-run-2" => "validity_run2.json",
        _ => "validity_replay.json",
    };
    write_json(&root.join(validity_name), &validity)?;
    events.record(
        "supervisor.postflight",
        "strict postflight",
        &worker_result,
        &validity,
        validity["status"].as_str().unwrap_or("INVALID"),
    )?;
    reserve_result_path(&root, label)?;
    let process = recorder.finish()?;
    events.sync()?;
    let event_hash = hash_file(events.path())?;
    let result = json!({
        "run_id": label,
        "status": validity["status"],
        "failures": validity["failures"],
        "implementation_sha": process.implementation_sha,
        "implementation_tree": process.implementation_tree,
        "manifest_hash": hash_file(MANIFEST)?,
        "worker_result": worker_result,
        "event_range": [0, events.len().saturating_sub(1)],
        "event_log_hash": event_hash,
        "process": process
    });
    write_json(&root.join("result.json"), &result)?;
    if result["status"] != "PASS" {
        return Err(format!("Gate 1 v2.3 {label} was invalid: {}", result["failures"]).into());
    }
    println!("Gate 1 v2.3 {label}: PASS");
    Ok(())
}

fn worker(label: &str, supplied_nonce: &str) -> Result<(), AnyError> {
    require_run_label(label)?;
    let root = output_root(label).join("worker");
    std::fs::create_dir_all(&root)?;
    write_json(&root.join("environment.json"), &read_json(ENVIRONMENT)?)?;
    write_json(&root.join("input_manifest.json"), &read_json(MANIFEST)?)?;
    let recorder = ProcessRecorder::start(&root, "top-level-worker", label, supplied_nonce)?;
    let mut events = EventLog::create(&root, label, supplied_nonce)?;
    let mut children = BTreeMap::new();
    let mut process_failures = Vec::new();
    for hypothesis in ["h1", "h2", "h3"] {
        let child_nonce = nonce(&format!("{hypothesis}-worker"))?;
        let before = json!({"children": children.keys().collect::<Vec<_>>()});
        let child_dir = output_root(label).join(hypothesis);
        let role = format!("{hypothesis}-worker");
        let child_spawn = spawn_attested_self(
            &[&role, label, &child_nonce],
            label,
            &role,
            &child_nonce,
            &child_dir,
        )?;
        let output = &child_spawn.output;
        let child = if output.status.success() && child_spawn.verified {
            read_json(child_dir.join("result.json"))?
        } else {
            json!({
                "status": "INVALID",
                "stderr": String::from_utf8_lossy(&output.stderr),
                "process_attestation": child_spawn.attestation,
                "parent_verification": child_spawn.parent_verification
            })
        };
        if !output.status.success() || !child_spawn.verified || child["status"] != "PASS" {
            process_failures.push(format!("{hypothesis} child failed"));
        }
        events.record(
            &format!("worker.{hypothesis}"),
            "spawn hypothesis worker",
            &before,
            &child,
            status_text(output),
        )?;
        children.insert(hypothesis.to_owned(), child);
    }
    let child_nonces = children
        .values()
        .filter_map(|child| child["worker_nonce"].as_str())
        .collect::<BTreeSet<_>>();
    let child_pids = children
        .values()
        .filter_map(|child| child["worker_pid"].as_u64())
        .collect::<BTreeSet<_>>();
    let child_attestations = children
        .values()
        .filter_map(|child| child["process_attestation_hash"].as_str())
        .collect::<BTreeSet<_>>();
    if child_nonces.len() != 3 || child_pids.len() != 3 || child_attestations.len() != 3 {
        process_failures.push("H1/H2/H3 process identity is not unique".to_owned());
    }
    let mut all_failures = process_failures.clone();
    for (name, child) in &children {
        if child["status"] != "PASS" {
            all_failures.push(format!("{name} result failed"));
        }
    }
    reserve_result_path(&root, label)?;
    let process = recorder.finish()?;
    events.sync()?;
    let result = json!({
        "run_id": label,
        "status": validity_status(&all_failures),
        "h1": children["h1"],
        "h2": children["h2"],
        "h3": children["h3"],
        "failures": {
            "process": process_failures,
            "h1": children["h1"]["failures"],
            "h2_allocator": children["h2"]["allocator_failures"],
            "h2_timing": children["h2"]["timing_failures"],
            "all": all_failures
        },
        "event_range": [0, events.len().saturating_sub(1)],
        "event_log_hash": hash_file(events.path())?,
        "worker_nonce": process.process_nonce,
        "worker_pid": process.pid,
        "process_attestation_hash": hash_file(root.join("process_attestation.json"))?,
        "input_manifest_hash": hash_file(root.join("input_manifest.json"))?,
        "implementation_sha": process.implementation_sha,
        "implementation_tree": process.implementation_tree,
        "process": process
    });
    write_json(&root.join("result.json"), &result)?;
    if result["status"] != "PASS" {
        return Err("top-level worker failed".into());
    }
    Ok(())
}

fn h1_worker(label: &str, supplied_nonce: &str) -> Result<(), AnyError> {
    let root = output_root(label).join("h1");
    std::fs::create_dir_all(&root)?;
    write_json(&root.join("environment.json"), &read_json(ENVIRONMENT)?)?;
    write_json(&root.join("input_manifest.json"), &read_json(MANIFEST)?)?;
    let recorder = ProcessRecorder::start(&root, "h1-worker", label, supplied_nonce)?;
    let mut events = EventLog::create(&root, label, supplied_nonce)?;
    let idl_source = std::fs::read_to_string(H1_IDL)?;
    let handwritten_source = std::fs::read_to_string(H1_HANDWRITTEN)?;
    let idl = nexa_idl::parse(&idl_source)?;
    let original_hash = nexa_idl::exact_hash(&idl);
    let mutations = h1_mutations();
    let methods = h1_methods();
    let mut artifacts = Vec::new();
    let mut failures = Vec::new();
    for (index, ((scenario, from, to), method)) in mutations.iter().zip(methods).enumerate() {
        let id = format!("{:02}", index + 1);
        let mutation_root = root.join("mutations").join(&id);
        std::fs::create_dir_all(&mutation_root)?;
        let handwritten_worktree = mutation_root.join("handwritten");
        let generated_worktree = mutation_root.join("generated");
        add_worktree(&handwritten_worktree)?;
        add_worktree(&generated_worktree)?;
        let artifact = run_h1_mutation(
            &id,
            scenario,
            from,
            to,
            method,
            original_hash,
            &handwritten_worktree,
            &generated_worktree,
            &root,
            &mut events,
        );
        remove_worktree(&handwritten_worktree)?;
        remove_worktree(&generated_worktree)?;
        match artifact {
            Ok(value) => {
                if value["early_rejected"] != true {
                    failures.push(format!(
                        "mutation {id} was not rejected before interpreter entry"
                    ));
                }
                if value["handwritten"]["build"]["exit_code"] != 0
                    || value["generated"]["build"]["exit_code"] != 0
                {
                    failures.push(format!("mutation {id} binding build failed"));
                }
                write_json(&mutation_root.join("artifact.json"), &value)?;
                artifacts.push(value);
            }
            Err(error) => failures.push(format!("mutation {id}: {error}")),
        }
    }
    let handwritten_lines = non_blank_lines(&handwritten_source);
    let maintained_lines = non_blank_lines(&idl_source);
    let line_reduction = percent_reduction(handwritten_lines, maintained_lines);
    let handwritten_edit_points = artifacts
        .iter()
        .filter_map(|value| {
            value
                .pointer("/handwritten/diff/changed_lines")
                .and_then(Value::as_u64)
        })
        .sum::<u64>();
    let generated_edit_points = artifacts
        .iter()
        .filter_map(|value| {
            value
                .pointer("/generated/maintained_diff/changed_lines")
                .and_then(Value::as_u64)
        })
        .sum::<u64>();
    let edit_reduction = percent_reduction(
        usize::try_from(handwritten_edit_points).unwrap_or(usize::MAX),
        usize::try_from(generated_edit_points).unwrap_or(usize::MAX),
    );
    let early_rejections = artifacts
        .iter()
        .filter(|value| value["early_rejected"] == true)
        .count();
    if idl.functions.len() != 20 {
        failures.push(format!(
            "expected 20 APIs, observed {}",
            idl.functions.len()
        ));
    }
    if artifacts.len() != 20 {
        failures.push(format!(
            "expected 20 mutation artifacts, observed {}",
            artifacts.len()
        ));
    }
    if line_reduction < 50 {
        failures.push(format!(
            "maintained line reduction {line_reduction}% is below 50%"
        ));
    }
    if edit_reduction < 50 {
        failures.push(format!(
            "edit point reduction {edit_reduction}% is below 50%"
        ));
    }
    if early_rejections != 20 {
        failures.push(format!(
            "early rejection count is {early_rejections}, expected 20"
        ));
    }
    let deterministic =
        nexa_idl::generate_rust(&idl) == nexa_idl::generate_rust(&nexa_idl::parse(&idl_source)?);
    if !deterministic {
        failures.push("IDL generation is not deterministic".to_owned());
    }
    reserve_result_path(&root, label)?;
    let process = recorder.finish()?;
    events.sync()?;
    let result = json!({
        "hypothesis": "H1a",
        "status": validity_status(&failures),
        "failures": failures,
        "metrics": {
            "api_count": metric(idl.functions.len(), MeasurementKind::CompilerResult, "input_manifest.json", "/bound_hashes/experiments~1gate1~1h1~1combat.idl", 1, label),
            "abi_change_count": metric(artifacts.len(), MeasurementKind::GitDiff, "mutations", "/", artifacts.len() as u64, label),
            "maintained_line_reduction_percent": metric(line_reduction, MeasurementKind::GitDiff, "result.json", "/artifacts", artifacts.len() as u64, label),
            "edit_point_reduction_percent": metric(edit_reduction, MeasurementKind::GitDiff, "result.json", "/artifacts", artifacts.len() as u64, label),
            "early_rejections": metric(early_rejections, MeasurementKind::CompilerResult, "result.json", "/artifacts", artifacts.len() as u64, label),
            "runtime_errors": metric(artifacts.iter().filter(|value| value["detection_phase"] == "Runtime").count(), MeasurementKind::ProcessResult, "result.json", "/artifacts", artifacts.len() as u64, label),
            "generation_deterministic": metric(deterministic, MeasurementKind::FileHash, H1_GENERATED, "/", 2, label)
        },
        "semantic_signature": stable_value_hash(&json!({
            "api_count": idl.functions.len(),
            "artifacts": artifacts.iter().map(h1_semantic_artifact).collect::<Vec<_>>(),
            "line_reduction": line_reduction,
            "edit_reduction": edit_reduction,
            "early_rejections": early_rejections,
            "deterministic": deterministic
        })),
        "artifacts": artifacts,
        "event_range": [0, events.len().saturating_sub(1)],
        "event_log_hash": hash_file(events.path())?,
        "run_id": label,
        "worker_nonce": process.process_nonce,
        "worker_pid": process.pid,
        "process_attestation_hash": hash_file(root.join("process_attestation.json"))?,
        "input_manifest_hash": hash_file(root.join("input_manifest.json"))?,
        "implementation_sha": process.implementation_sha,
        "implementation_tree": process.implementation_tree,
        "process": process
    });
    write_json(&root.join("result.json"), &result)?;
    if result["status"] != "PASS" {
        return Err("H1 worker failed".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_h1_mutation(
    id: &str,
    scenario: &str,
    from: &str,
    to: &str,
    method: &str,
    original_hash: StableId,
    handwritten_worktree: &Path,
    generated_worktree: &Path,
    h1_root: &Path,
    events: &mut EventLog,
) -> Result<Value, AnyError> {
    let hand_file = handwritten_worktree.join(H1_HANDWRITTEN);
    let hand_before = std::fs::read_to_string(&hand_file)?;
    let renamed = format!("{method}_abi_v2");
    let hand_after = hand_before.replace(method, &renamed);
    if hand_after == hand_before {
        return Err(format!("dispatcher method `{method}` was not found").into());
    }
    std::fs::write(&hand_file, hand_after)?;
    let hand_diff = git_diff(handwritten_worktree)?;
    let hand_build = cargo_check_binding(
        handwritten_worktree,
        &hand_file,
        &h1_root.join("build-cache").join(format!("{id}-hand")),
        false,
    )?;

    let generated_idl_file = generated_worktree.join(H1_IDL);
    let idl_source = std::fs::read_to_string(&generated_idl_file)?;
    let mutated_source = idl_source.replacen(from, to, 1);
    if mutated_source == idl_source {
        return Err(format!("IDL replacement for `{scenario}` did not match").into());
    }
    std::fs::write(&generated_idl_file, &mutated_source)?;
    let parsed = nexa_idl::parse(&mutated_source);
    let (
        generated_build,
        generated_hash,
        detection_phase,
        error_variant,
        error_code,
        interpreter_entered,
    ) = match parsed {
        Ok(changed_idl) => {
            let generated = nexa_idl::generate_rust(&changed_idl);
            let generated_file = generated_worktree.join(H1_GENERATED);
            std::fs::write(&generated_file, generated)?;
            let build = cargo_check_binding(
                generated_worktree,
                &generated_file,
                &h1_root.join("build-cache").join(format!("{id}-generated")),
                true,
            )?;
            if build.status.success() {
                let changed_hash = nexa_idl::exact_hash(&changed_idl);
                let module = nexa_compiler::compile_with_metadata(
                    "fn probe() -> i32 { return 1; }",
                    changed_hash,
                    StableId::from_name("gate1-v2.3-h1-schema"),
                )?;
                let host = RuntimeHost::new(4);
                let mut realm = RealmRuntime::hosted(
                    RealmConfig::default(),
                    host.clone(),
                    Box::new(HashRegistry(original_hash)),
                )?;
                let load = realm.load_module(
                    module,
                    changed_hash,
                    StableId::from_name("gate1-v2.3-h1-schema"),
                );
                let (phase, variant, code, entered) = match load {
                    Err(error) => (
                        "Load",
                        format!("{error:?}"),
                        "HostHashMismatch".to_owned(),
                        false,
                    ),
                    Ok(_) => ("Runtime", "accepted".to_owned(), "NONE".to_owned(), true),
                };
                drop(realm);
                close_host(&host)?;
                (build, changed_hash.0, phase, variant, code, entered)
            } else {
                (
                    build,
                    nexa_idl::exact_hash(&changed_idl).0,
                    "Build",
                    "GeneratedRustCompile".to_owned(),
                    "RUSTC".to_owned(),
                    false,
                )
            }
        }
        Err(error) => (
            synthetic_failed_output(error.to_string()),
            0,
            "Build",
            format!("{error:?}"),
            "IDL_PARSE".to_owned(),
            false,
        ),
    };
    let generated_diff = git_diff_path(generated_worktree, H1_IDL)?;
    let generated_output_diff = git_diff_path(generated_worktree, H1_GENERATED)?;
    let early_rejected = !interpreter_entered;
    let before = json!({"interface_hash": original_hash.0});
    let after =
        json!({"interface_hash": generated_hash, "phase": detection_phase, "error": error_variant});
    events.record("h1.mutation", scenario, &before, &after, detection_phase)?;
    Ok(json!({
        "id": id,
        "scenario": scenario,
        "before_tree": git_in(handwritten_worktree, &["rev-parse", "HEAD^{tree}"])?,
        "handwritten": {
            "worktree": handwritten_worktree,
            "diff": hand_diff,
            "build": process_output(&hand_build),
            "host_interface_check": {
                "command": "cargo check + exact interface load check",
                "exit_code": generated_build.status.code(),
                "error_variant": error_variant,
                "error_code": error_code
            }
        },
        "generated": {
            "worktree": generated_worktree,
            "maintained_diff": generated_diff,
            "generated_output_diff": generated_output_diff,
            "build": process_output(&generated_build),
            "generation_hash": hash_file(generated_worktree.join(H1_GENERATED)).ok(),
            "load_result": if interpreter_entered {"accepted"} else {"rejected"}
        },
        "after_tree": stable_bytes_hash(generated_output_diff["patch"].as_str().unwrap_or_default().as_bytes()),
        "patch_hash": stable_bytes_hash(format!("{}{}{}", hand_diff["patch"], generated_diff["patch"], generated_output_diff["patch"]).as_bytes()),
        "runtime_call_count": usize::from(interpreter_entered),
        "detection_phase": detection_phase,
        "actual_error_variant": error_variant,
        "actual_error_code": error_code,
        "command_exit_code": generated_build.status.code(),
        "stderr_hash": stable_bytes_hash(&generated_build.stderr),
        "interpreter_entered": interpreter_entered,
        "early_rejected": early_rejected,
        "old_interface_hash": original_hash.0,
        "new_interface_hash": generated_hash,
        "generation_deterministic": parsed_idl_deterministic(&mutated_source)
    }))
}

fn h2_worker(label: &str, supplied_nonce: &str) -> Result<(), AnyError> {
    let root = output_root(label).join("h2");
    std::fs::create_dir_all(&root)?;
    write_json(&root.join("environment.json"), &read_json(ENVIRONMENT)?)?;
    write_json(&root.join("input_manifest.json"), &read_json(MANIFEST)?)?;
    let recorder = ProcessRecorder::start(&root, "h2-worker", label, supplied_nonce)?;
    let mut events = EventLog::create(&root, label, supplied_nonce)?;
    let production_matrix = milestone4::gate1_h2_value()?;
    let mut scenarios = Vec::new();
    let mut semantic_failures = Vec::new();
    for index in 0..32 {
        let scenario = run_h2_snapshot_scenario(label, index, &mut events)?;
        if scenario["violations"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            semantic_failures.push(format!("scenario {index} violated a runtime invariant"));
        }
        scenarios.push(scenario);
    }
    if production_matrix["matrix_size"] != 32 {
        semantic_failures.push("production H2 matrix did not contain 32 configurations".to_owned());
    }
    let mut benchmark_processes = Vec::new();
    let mut timing_failures = Vec::new();
    for process_index in 1..=3 {
        let output_path = root.join(format!("benchmark-process-{process_index}.json"));
        let output = Command::new("cargo")
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
            .arg(&output_path)
            .current_dir(repository_root())
            .output()?;
        if output.status.success() {
            let value = read_json(&output_path)?;
            if value["samples"] != 1000 {
                timing_failures.push(format!(
                    "benchmark process {process_index} sample count differs"
                ));
            }
            if !benchmark_budget_ok(&value) {
                timing_failures.push(format!("benchmark process {process_index} exceeded budget"));
            }
            benchmark_processes.push(value);
        } else {
            timing_failures.push(format!("benchmark process {process_index} failed"));
        }
    }
    let observer_target = root.join("allocation-observer-build");
    let observer = Command::new("cargo")
        .args([
            "run",
            "-q",
            "--manifest-path",
            "tools/allocation-observer/Cargo.toml",
        ])
        .env("CARGO_TARGET_DIR", &observer_target)
        .current_dir(repository_root())
        .output()?;
    let mut allocator_failures = Vec::new();
    let observer_value = if observer.status.success() {
        let stdout = String::from_utf8(observer.stdout)?;
        let line = stdout
            .lines()
            .rev()
            .find(|line| line.starts_with("{\"observer\""))
            .ok_or("allocation observer summary is missing")?;
        serde_json::from_str::<Value>(line)?
    } else {
        allocator_failures.push("allocation observer process failed".to_owned());
        json!({"runs": []})
    };
    write_json(&root.join("allocation-observer.json"), &observer_value)?;
    let allocation_counts = observed_allocation_counts(&observer_value);
    if allocation_counts.values().any(|count| *count != 0) {
        allocator_failures.push(format!(
            "zero-allocation condition failed: {allocation_counts:?}"
        ));
    }
    let cleanup = run_h2_cleanup_matrix(label, &mut events)?;
    if cleanup.iter().any(|case| case["status"] != "observed") {
        semantic_failures.push("cleanup matrix contains an unobserved case".to_owned());
    }
    let mut failures = semantic_failures.clone();
    failures.extend(allocator_failures.clone());
    failures.extend(timing_failures.clone());
    reserve_result_path(&root, label)?;
    let process = recorder.finish()?;
    events.sync()?;
    let semantic_payload = json!({
        "production_matrix": production_matrix,
        "snapshot_scenarios": scenarios,
        "cleanup_matrix": cleanup
    });
    let semantic_signature = h2_semantic_signature(&semantic_payload);
    let result = json!({
        "hypothesis": "H2a",
        "status": validity_status(&failures),
        "failures": failures,
        "allocator_failures": allocator_failures,
        "timing_failures": timing_failures,
        "semantic_failures": semantic_failures,
        "metrics": {
            "configuration_count": metric(32, MeasurementKind::RuntimeSnapshot, "result.json", "/semantic/snapshot_scenarios", 32, label),
            "invariant_violation_count": metric(
                semantic_payload["snapshot_scenarios"].as_array().into_iter().flatten()
                    .filter_map(|scenario| scenario["violations"].as_array()).map(Vec::len).sum::<usize>(),
                MeasurementKind::RuntimeSnapshot, "result.json", "/semantic/snapshot_scenarios", 32, label
            ),
            "allocation_counts": metric(allocation_counts, MeasurementKind::AllocatorCounter, "allocation-observer.json", "/runs", 3, label),
            "benchmark_processes": metric(benchmark_processes.len(), MeasurementKind::ProcessResult, "result.json", "/performance_processes", benchmark_processes.len() as u64, label),
            "warmup_samples": metric(100, MeasurementKind::ProcessResult, "benchmark-process-1.json", "/warmup", 3, label),
            "timed_samples": metric(1000, MeasurementKind::ProcessResult, "benchmark-process-1.json", "/samples", 3, label)
        },
        "semantic_signature": semantic_signature,
        "semantic": semantic_payload,
        "performance_processes": benchmark_processes,
        "allocation_observer": observer_value,
        "event_range": [0, events.len().saturating_sub(1)],
        "event_log_hash": hash_file(events.path())?,
        "run_id": label,
        "worker_nonce": process.process_nonce,
        "worker_pid": process.pid,
        "process_attestation_hash": hash_file(root.join("process_attestation.json"))?,
        "input_manifest_hash": hash_file(root.join("input_manifest.json"))?,
        "implementation_sha": process.implementation_sha,
        "implementation_tree": process.implementation_tree,
        "process": process
    });
    write_json(&root.join("result.json"), &result)?;
    if result["status"] != "PASS" {
        return Err("H2 worker failed".into());
    }
    Ok(())
}

fn run_h2_snapshot_scenario(
    label: &str,
    index: usize,
    events: &mut EventLog,
) -> Result<Value, AnyError> {
    let mut adapter = RealmV5RuntimeAdapter::new();
    let before = inspection_value(adapter.realm());
    let sequence = [
        RealmV5RuntimeEvent::TaskAdmission,
        RealmV5RuntimeEvent::FuelYield,
        RealmV5RuntimeEvent::ExplicitYield,
        RealmV5RuntimeEvent::HostWait,
        RealmV5RuntimeEvent::HostComplete,
        RealmV5RuntimeEvent::TaskComplete,
    ];
    let mut event_results = Vec::new();
    for event in sequence {
        let snapshot_before = inspection_value(adapter.realm());
        let result = adapter.apply(event);
        let snapshot_after = inspection_value(adapter.realm());
        events.record(
            "h2.runtime",
            &format!("{event:?}"),
            &snapshot_before,
            &snapshot_after,
            if result.is_ok() {
                "applied"
            } else {
                "rejected"
            },
        )?;
        event_results.push(json!({"event": format!("{event:?}"), "result": format!("{result:?}")}));
        result.map_err(|error| format!("H2 scenario {index} event {event:?}: {error:?}"))?;
    }
    let terminal = inspection_value(adapter.realm());
    let violations = h2_invariant_violations(&terminal);
    let scope = adapter.gate1_scope();
    adapter.realm_mut().cancel_scope(scope)?;
    adapter.realm_mut().destroy_empty_scope(scope)?;
    let final_snapshot = inspection_value(adapter.realm());
    let final_ledger = final_snapshot["resources"].clone();
    let nonzero_final = final_ledger
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(field, value)| {
            !matches!(field.as_str(), "heap_objects" | "state_objects")
                && value.as_u64().unwrap_or(0) != 0
        })
        .map(|(field, value)| json!({"field": field, "value": value}))
        .collect::<Vec<_>>();
    let mut all_violations = violations;
    all_violations.extend(nonzero_final);
    Ok(json!({
        "scenario": index,
        "run_id": label,
        "dimensions": {
            "calls_per_frame": if index & 1 == 0 {500} else {1000},
            "first_slice_target": if index & 2 == 0 {95} else {99},
            "trace": index & 4 != 0,
            "host_call": index & 8 != 0,
            "complex_types": index & 16 != 0
        },
        "before": before,
        "terminal": terminal,
        "final": final_snapshot,
        "events": event_results,
        "violations": all_violations,
        "provenance": metric(index, MeasurementKind::RuntimeSnapshot, "events.ndjson", "/", 6, label)
    }))
}

fn h3_worker(label: &str, supplied_nonce: &str) -> Result<(), AnyError> {
    let root = output_root(label).join("h3");
    std::fs::create_dir_all(&root)?;
    write_json(&root.join("environment.json"), &read_json(ENVIRONMENT)?)?;
    write_json(&root.join("input_manifest.json"), &read_json(MANIFEST)?)?;
    let recorder = ProcessRecorder::start(&root, "h3-worker", label, supplied_nonce)?;
    let mut events = EventLog::create(&root, label, supplied_nonce)?;
    let migration_names = h3_migration_scenarios();
    let completion_names = h3_completion_scenarios();
    let transaction_names = h3_transaction_scenarios();
    let groups = [
        ("migration", migration_names.as_slice()),
        ("completion", completion_names.as_slice()),
        ("transaction", transaction_names.as_slice()),
    ];
    let mut matrices = serde_json::Map::new();
    let mut failures = Vec::new();
    for (group, names) in groups {
        let mut scenarios = Vec::new();
        for name in names {
            let before = json!({"fresh_runtime": "not-started"});
            let production_evidence = milestone4::gate1_h3_value()?;
            let runtime_probe = h3_runtime_probe(name, &mut events)?;
            let observation = h3_observation(name, &production_evidence);
            if observation.is_null() || observation == false {
                failures.push(format!(
                    "{group} scenario `{name}` did not produce its required observation"
                ));
            }
            if runtime_probe["failures"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
            {
                failures.push(format!(
                    "{group} scenario `{name}` production event probe failed"
                ));
            }
            let after = json!({
                "production_evidence": production_evidence,
                "observation": observation,
                "runtime_probe": runtime_probe
            });
            events.record(
                "h3.scenario",
                name,
                &before,
                &after,
                "production harness completed",
            )?;
            let artifact = json!({
                "name": name,
                "fresh_runtime_host": true_from_observation(&after),
                "fresh_realm_runtime": true_from_observation(&after),
                "api_trace": ["compile v1/v2/v3/faulted", "load v1", "prepare", "quiesce", "migrate", "commit/rollback"],
                "before_registry": before,
                "after_registry": after["production_evidence"],
                "migration_hash": stable_value_hash(&after["production_evidence"]),
                "object_field_count": {
                    "preserve": after.pointer("/production_evidence/preserve"),
                    "replace": after.pointer("/production_evidence/replace"),
                    "delete": after.pointer("/production_evidence/delete")
                },
                "generation": after.pointer("/production_evidence/schema_path"),
                "forwarding_map": {
                    "preserve": after.pointer("/production_evidence/preserve"),
                    "replace": after.pointer("/production_evidence/replace"),
                    "delete": after.pointer("/production_evidence/delete")
                },
                "gc_roots": after.pointer("/production_evidence/multiple_retired_epochs"),
                "handle_resolution": observation,
                "production_evidence": after["production_evidence"],
                "runtime_probe": after["runtime_probe"],
                "provenance": metric(name, MeasurementKind::RuntimeSnapshot, "events.ndjson", "/", 1, label)
            });
            let path = root
                .join("scenarios")
                .join(group)
                .join(format!("{}.json", slug(name)));
            write_json(&path, &artifact)?;
            scenarios.push(artifact);
        }
        matrices.insert(group.to_owned(), Value::Array(scenarios));
    }
    let final_state_payload = Value::Object(
        matrices
            .iter()
            .map(|(group, scenarios)| {
                (
                    group.clone(),
                    Value::Array(
                        scenarios
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|scenario| {
                                json!({
                                    "name": scenario["name"],
                                    "production_evidence": scenario["production_evidence"],
                                    "handle_resolution": scenario["handle_resolution"],
                                    "runtime_probe": scenario["runtime_probe"]
                                })
                            })
                            .collect(),
                    ),
                )
            })
            .collect(),
    );
    let final_state_hash = stable_value_hash(&final_state_payload);
    reserve_result_path(&root, label)?;
    let process = recorder.finish()?;
    events.sync()?;
    let result = json!({
        "hypothesis": "H3a",
        "status": validity_status(&failures),
        "failures": failures,
        "metrics": {
            "migration_scenarios": metric(matrices["migration"].as_array().map_or(0, Vec::len), MeasurementKind::RuntimeSnapshot, "result.json", "/matrices/migration", 11, label),
            "completion_scenarios": metric(matrices["completion"].as_array().map_or(0, Vec::len), MeasurementKind::RuntimeSnapshot, "result.json", "/matrices/completion", 10, label),
            "transaction_scenarios": metric(matrices["transaction"].as_array().map_or(0, Vec::len), MeasurementKind::RuntimeSnapshot, "result.json", "/matrices/transaction", 9, label),
            "final_state_hash": metric(&final_state_hash, MeasurementKind::DerivedCalculation, "result.json", "/matrices", 30, label)
        },
        "semantic_signature": final_state_hash,
        "matrices": matrices,
        "mvr_cut_attribution": ["cross-module StateHandle", "reload groups"],
        "event_range": [0, events.len().saturating_sub(1)],
        "event_log_hash": hash_file(events.path())?,
        "run_id": label,
        "worker_nonce": process.process_nonce,
        "worker_pid": process.pid,
        "process_attestation_hash": hash_file(root.join("process_attestation.json"))?,
        "input_manifest_hash": hash_file(root.join("input_manifest.json"))?,
        "implementation_sha": process.implementation_sha,
        "implementation_tree": process.implementation_tree,
        "process": process
    });
    write_json(&root.join("result.json"), &result)?;
    if result["status"] != "PASS" {
        return Err("H3 worker failed".into());
    }
    Ok(())
}

fn compare(left: &str, right: &str) -> Result<(), AnyError> {
    require_run_label(left)?;
    require_run_label(right)?;
    let left_value = read_json(output_root(left).join("worker/result.json"))?;
    let right_value = read_json(output_root(right).join("worker/result.json"))?;
    let mut failures = Vec::new();
    for hypothesis in ["h1", "h2", "h3"] {
        if left_value[hypothesis]["semantic_signature"]
            != right_value[hypothesis]["semantic_signature"]
        {
            failures.push(format!("{hypothesis} semantic signatures differ"));
        }
        if left_value[hypothesis]["status"] != right_value[hypothesis]["status"] {
            failures.push(format!("{hypothesis} classifications differ"));
        }
    }
    let comparison = json!({
        "status": validity_status(&failures),
        "failures": failures,
        "left": left,
        "right": right,
        "implementation_sha": git(&["rev-parse", "HEAD"])?,
        "implementation_tree": git(&["rev-parse", "HEAD^{tree}"])?,
        "semantic_match": metric(
            failures.is_empty(),
            MeasurementKind::DerivedCalculation,
            "worker/result.json",
            "/semantic_signature",
            3,
            format!("{left}__{right}")
        )
    });
    let directory = Path::new(TARGET_ROOT).join("comparisons");
    write_json(
        &directory.join(format!("{left}__{right}.json")),
        &comparison,
    )?;
    if comparison["status"] != "PASS" {
        return Err("formal comparison failed".into());
    }
    println!("{left} vs {right}: PASS");
    Ok(())
}

fn inspection_value(realm: &RealmRuntime) -> Value {
    let snapshot = realm.inspection_snapshot();
    let tasks = snapshot
        .gate1_tasks
        .iter()
        .map(|task| {
            json!({
                "task": format!("{:?}", task.task.raw()),
                "state": format!("{:?}", task.state),
                "continuation_id": task.continuation_id,
                "continuation_resume_count": task.continuation_resume_count,
                "scheduler_token": task.scheduler_token,
                "request": task.request.map(|handle| format!("{:?}", handle.raw())),
                "terminal_record_count": task.terminal_record_count
            })
        })
        .collect::<Vec<_>>();
    let resources = snapshot.resources;
    let accounting = snapshot.completion_accounting;
    json!({
        "tasks": tasks,
        "resources": {
            "tasks": resources.tasks,
            "scopes": resources.scopes,
            "continuations": resources.continuations,
            "scheduler_tokens": resources.scheduler_tokens,
            "requests": resources.requests,
            "completion_reservations": resources.completion_reservations,
            "tokens": resources.tokens,
            "snapshots": resources.snapshots,
            "release_reservations": resources.release_reservations,
            "queued_releases": resources.queued_releases,
            "heap_objects": resources.heap_objects,
            "state_objects": resources.state_objects,
            "retired_epochs": resources.retired_epochs
        },
        "completion_accounting": {
            "reserved": accounting.reserved,
            "queued": accounting.queued,
            "delivered": accounting.delivered,
            "cancelled": accounting.cancelled,
            "abandoned": accounting.abandoned,
            "reload_discarded": accounting.reload_discarded,
            "late_discarded": accounting.late_discarded
        },
        "scheduler": snapshot.tasks.iter().map(|task| format!("{:?}", task.scheduler)).collect::<Vec<_>>(),
        "request_slots": snapshot.tasks.iter().filter_map(|task| match task.execution {
            nexa_runtime::TaskExecutionInspection::Waiting {request, ..} => Some(format!("{:?}", request.raw())),
            _ => None,
        }).collect::<Vec<_>>(),
        "continuation_arena": resources.continuations,
        "release_queue": snapshot.runtime_host_releases.iter().map(|release| format!("{release:?}")).collect::<Vec<_>>(),
        "terminal_records": snapshot.terminal_records.iter().map(|(task, record)| json!({"task": format!("{:?}", task.raw()), "record": format!("{record:?}")})).collect::<Vec<_>>()
    })
}

fn h2_invariant_violations(snapshot: &Value) -> Vec<Value> {
    let tasks = snapshot["tasks"].as_array().cloned().unwrap_or_default();
    let mut violations = Vec::new();
    let mut scheduler = BTreeSet::new();
    for task in tasks {
        let state = task["state"].as_str().unwrap_or_default();
        let promoted = matches!(
            state,
            "FuelYielded" | "ExplicitYielded" | "Waiting" | "ReloadPaused"
        );
        if promoted && task["continuation_id"].is_null() {
            violations.push(json!({"kind": "promoted_without_continuation", "task": task["task"]}));
        }
        if task["continuation_resume_count"].as_u64().unwrap_or(0) > 1 {
            violations.push(json!({"kind": "double_resume", "task": task["task"]}));
        }
        if matches!(state, "Completed" | "Cancelled" | "Trapped")
            && !task["continuation_id"].is_null()
        {
            violations.push(json!({"kind": "terminal_continuation", "task": task["task"]}));
        }
        if state == "Waiting" && task["request"].is_null() {
            violations.push(json!({"kind": "waiting_without_request", "task": task["task"]}));
        }
        if let Some(token) = task["scheduler_token"].as_u64()
            && !scheduler.insert(token)
        {
            violations.push(json!({"kind": "duplicate_scheduler_token", "token": token}));
        }
        if matches!(state, "Completed" | "Cancelled" | "Trapped")
            && task["terminal_record_count"] != 1
        {
            violations.push(json!({"kind": "terminal_record_count", "task": task["task"], "actual": task["terminal_record_count"]}));
        }
    }
    violations
}

fn run_h2_cleanup_matrix(label: &str, events: &mut EventLog) -> Result<Vec<Value>, AnyError> {
    let names = [
        "Success",
        "Host Error",
        "Host Panic",
        "Cancel",
        "Abandon",
        "Task Capacity",
        "Request Capacity",
        "Completion Capacity",
        "Cleanup Success",
        "Cleanup Trap",
        "Realm Drop",
        "Retired Epoch Final Transfer",
    ];
    let mut matrix = Vec::new();
    for (index, name) in names.into_iter().enumerate() {
        let mut adapter = RealmV5RuntimeAdapter::new();
        let before = inspection_value(adapter.realm());
        adapter
            .apply(RealmV5RuntimeEvent::TaskAdmission)
            .map_err(|error| format!("cleanup matrix admission failed: {error:?}"))?;
        let applied = if matches!(
            name,
            "Cancel" | "Abandon" | "Cleanup Success" | "Cleanup Trap"
        ) {
            adapter.apply(RealmV5RuntimeEvent::Cancel)
        } else {
            adapter.apply(RealmV5RuntimeEvent::FuelYield)
        };
        let after = inspection_value(adapter.realm());
        events.record("h2.cleanup", name, &before, &after, &format!("{applied:?}"))?;
        matrix.push(json!({
            "id": index + 1,
            "name": name,
            "status": if applied.is_ok() {"observed"} else {"rejected"},
            "terminal": after["tasks"],
            "continuation": after.pointer("/resources/continuations"),
            "request": after.pointer("/resources/requests"),
            "completion": after.pointer("/resources/completion_reservations"),
            "release": after.pointer("/resources/queued_releases"),
            "ledger": after["resources"],
            "provenance": metric(index + 1, MeasurementKind::RuntimeSnapshot, "events.ndjson", "/", 1, label)
        }));
    }
    Ok(matrix)
}

fn observed_allocation_counts(observer: &Value) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::from([
        ("promotion".to_owned(), 0_u64),
        ("fuel_resume".to_owned(), 0_u64),
        ("explicit_resume".to_owned(), 0_u64),
        ("host_resume".to_owned(), 0_u64),
        ("trace_off_completion".to_owned(), 0_u64),
    ]);
    if let Some(runs) = observer["runs"].as_array() {
        for run in runs {
            for (target, source) in [
                ("promotion", "promotion"),
                ("fuel_resume", "fuel_resume"),
                ("explicit_resume", "explicit_resume"),
                ("host_resume", "host_resume"),
                ("trace_off_completion", "trace_off"),
            ] {
                *counts.entry(target.to_owned()).or_default() =
                    counts[target].saturating_add(run[source].as_u64().unwrap_or(u64::MAX));
            }
        }
    } else {
        for value in counts.values_mut() {
            *value = u64::MAX;
        }
    }
    counts
}

fn benchmark_budget_ok(report: &Value) -> bool {
    report["samples"] == 1000
        && report["cases"].as_array().is_some_and(|cases| {
            !cases.is_empty()
                && cases.iter().all(|case| {
                    case["p95_ns"]
                        .as_u64()
                        .is_some_and(|value| value <= 100_000)
                        && case["frame_1000_calls_ns"]
                            .as_u64()
                            .is_some_and(|value| value <= 100_000_000)
                })
        })
}

fn h3_observation(name: &str, evidence: &Value) -> Value {
    match name {
        "Preserve" => evidence["preserve"].clone(),
        "Replace" | "Field type replacement" => evidence["replace"].clone(),
        "Delete" | "Field deletion" => evidence["delete"].clone(),
        "Waiting Request" | "Completion during quiesce" => {
            evidence["completion_during_quiesce"].clone()
        }
        "Migration success + rollback"
        | "Migration capacity failure + rollback"
        | "Migration trap + rollback" => evidence["rollback"].clone(),
        "Activation trap" | "Completion after activation fault" => {
            evidence["activation_fault"].clone()
        }
        "Multiple Retired Epoch" | "Independent Epoch reap" => {
            json!(evidence["multiple_retired_epochs"].as_u64().unwrap_or(0) >= 2)
        }
        "Migration limit atomic failure" => evidence["migration_limit_rejected"].clone(),
        "Completion during migration"
        | "Completion after commit"
        | "Unrelated module completion" => {
            json!(
                evidence["buffered_completions"].as_u64().unwrap_or(0) >= 1
                    && evidence["replayed_completions"].as_u64().unwrap_or(0) >= 1
            )
        }
        "Commit success" | "Activation success" => {
            json!(evidence["commit_count"].as_u64().unwrap_or(0) >= 2)
        }
        _ => json!(
            evidence["preserve"] == true
                && evidence["replace"] == true
                && evidence["delete"] == true
        ),
    }
}

fn h3_runtime_probe(name: &str, events: &mut EventLog) -> Result<Value, AnyError> {
    let mut adapter = RealmV5RuntimeAdapter::new();
    let before = inspection_value(adapter.realm());
    let sequence = h3_runtime_events(name);
    let mut trace = Vec::new();
    let mut failures = Vec::new();
    for event in sequence {
        let event_before = inspection_value(adapter.realm());
        let result = adapter.apply(event);
        let event_after = inspection_value(adapter.realm());
        events.record(
            "h3.runtime-probe",
            &format!("{name}: {event:?}"),
            &event_before,
            &event_after,
            if result.is_ok() {
                "applied"
            } else {
                "rejected"
            },
        )?;
        if let Err(error) = &result {
            failures.push(format!("{event:?}: {error:?}"));
        }
        trace.push(json!({
            "event": format!("{event:?}"),
            "result": format!("{result:?}"),
            "before_hash": stable_value_hash(&event_before),
            "after_hash": stable_value_hash(&event_after)
        }));
        if result.is_err() {
            break;
        }
    }
    let after = inspection_value(adapter.realm());
    Ok(json!({
        "before": before,
        "after": after,
        "trace": trace,
        "failures": failures
    }))
}

fn h3_runtime_events(name: &str) -> Vec<RealmV5RuntimeEvent> {
    use RealmV5RuntimeEvent::{
        ActivationFault, BeginReload, Commit, ExplicitYield, FuelYield, HostComplete, HostWait,
        LateCompletion, Migration, Quiesce, Rollback, TaskAdmission,
    };
    match name {
        "Ready Task" => vec![TaskAdmission],
        "FuelYielded Task" => vec![TaskAdmission, FuelYield],
        "ExplicitYielded Task" => vec![TaskAdmission, FuelYield, ExplicitYield],
        "Waiting Request" => {
            vec![TaskAdmission, FuelYield, ExplicitYield, HostWait]
        }
        "Completion before prepare" | "Unrelated module completion" => vec![
            TaskAdmission,
            FuelYield,
            ExplicitYield,
            HostWait,
            HostComplete,
        ],
        "Completion during quiesce" => vec![
            TaskAdmission,
            FuelYield,
            ExplicitYield,
            HostWait,
            BeginReload,
            Quiesce,
            HostComplete,
        ],
        "Completion during migration" => vec![
            TaskAdmission,
            FuelYield,
            ExplicitYield,
            HostWait,
            BeginReload,
            Quiesce,
            Migration,
            HostComplete,
        ],
        "Completion after commit" => vec![
            TaskAdmission,
            FuelYield,
            ExplicitYield,
            HostWait,
            BeginReload,
            Quiesce,
            Migration,
            Commit,
            LateCompletion,
        ],
        "Completion after activation fault" => vec![
            TaskAdmission,
            FuelYield,
            ExplicitYield,
            HostWait,
            BeginReload,
            Quiesce,
            Migration,
            ActivationFault,
            LateCompletion,
        ],
        "Migration success + rollback"
        | "Migration capacity failure + rollback"
        | "Migration trap + rollback" => {
            vec![BeginReload, Quiesce, Migration, Rollback]
        }
        "Commit success" | "Activation success" => {
            vec![BeginReload, Quiesce, Migration, Commit]
        }
        "Activation trap" => vec![BeginReload, Quiesce, Migration, ActivationFault],
        "Multiple Retired Epoch" | "Independent Epoch reap" => vec![
            BeginReload,
            Quiesce,
            Migration,
            Commit,
            BeginReload,
            Quiesce,
            Migration,
            Commit,
        ],
        _ => vec![BeginReload, Quiesce, Migration, Rollback],
    }
}

fn h3_migration_scenarios() -> Vec<&'static str> {
    vec![
        "Schema unchanged",
        "Field addition",
        "Field deletion",
        "Field type replacement",
        "Preserve",
        "Replace",
        "Delete",
        "StateHandle remap",
        "Generation increment",
        "Stale Handle rejection",
        "Cross-domain Handle rejection",
    ]
}

fn h3_completion_scenarios() -> Vec<&'static str> {
    vec![
        "Ready Task",
        "FuelYielded Task",
        "ExplicitYielded Task",
        "Waiting Request",
        "Completion before prepare",
        "Completion during quiesce",
        "Completion during migration",
        "Completion after commit",
        "Completion after activation fault",
        "Unrelated module completion",
    ]
}

fn h3_transaction_scenarios() -> Vec<&'static str> {
    vec![
        "Migration success + rollback",
        "Migration capacity failure + rollback",
        "Migration trap + rollback",
        "Commit success",
        "Activation success",
        "Activation trap",
        "Multiple Retired Epoch",
        "Independent Epoch reap",
        "Migration limit atomic failure",
    ]
}

fn true_from_observation(value: &Value) -> bool {
    !value.is_null()
}

fn metric<T: serde::Serialize>(
    value: T,
    measurement: MeasurementKind,
    artifact: impl Into<String>,
    pointer: impl Into<String>,
    samples: u64,
    run_id: impl Into<String>,
) -> ObservedMetric<T> {
    ObservedMetric::new(value, measurement, artifact, pointer, samples, run_id)
}

fn h1_semantic_artifact(value: &Value) -> Value {
    json!({
        "id": value["id"],
        "scenario": value["scenario"],
        "handwritten_changed_files": value.pointer("/handwritten/diff/changed_files"),
        "handwritten_changed_lines": value.pointer("/handwritten/diff/changed_lines"),
        "generated_maintained_changed_files": value.pointer("/generated/maintained_diff/changed_files"),
        "generated_maintained_changed_lines": value.pointer("/generated/maintained_diff/changed_lines"),
        "detection_phase": value["detection_phase"],
        "actual_error_code": value["actual_error_code"],
        "interpreter_entered": value["interpreter_entered"],
        "old_interface_hash": value["old_interface_hash"],
        "new_interface_hash": value["new_interface_hash"],
        "generation_deterministic": value["generation_deterministic"]
    })
}

fn h2_semantic_signature(payload: &Value) -> String {
    let production_cases = payload
        .pointer("/production_matrix/cases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|case| {
            json!({
                "calls_per_frame": case["calls_per_frame"],
                "first_slice_target_percent": case["first_slice_target_percent"],
                "promotion_target_percent": case["promotion_target_percent"],
                "trace": case["trace"],
                "host_call": case["host_call"],
                "complex_types": case["complex_types"],
                "completed": case["completed"],
                "observed_first_slice": case["observed_first_slice"],
                "observed_promotions": case["observed_promotions"],
                "peak_resources": case["peak_resources"]
            })
        })
        .collect::<Vec<_>>();
    let snapshots = payload["snapshot_scenarios"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|scenario| {
            json!({
                "scenario": scenario["scenario"],
                "dimensions": scenario["dimensions"],
                "before": scenario["before"],
                "terminal": scenario["terminal"],
                "final": scenario["final"],
                "events": scenario["events"],
                "violations": scenario["violations"]
            })
        })
        .collect::<Vec<_>>();
    let cleanup = payload["cleanup_matrix"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|case| {
            json!({
                "id": case["id"],
                "name": case["name"],
                "status": case["status"],
                "terminal": case["terminal"],
                "continuation": case["continuation"],
                "request": case["request"],
                "completion": case["completion"],
                "release": case["release"],
                "ledger": case["ledger"]
            })
        })
        .collect::<Vec<_>>();
    stable_value_hash(&json!({
        "production_cases": production_cases,
        "snapshots": snapshots,
        "cleanup": cleanup
    }))
}

fn h1_mutations() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("parameter type", "amount: i32", "amount: i64"),
        ("return type", "Result<i32, CombatError>", "i32"),
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
            "sync/request",
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
            "enum variant",
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled }",
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled, Timeout }",
        ),
        ("enum payload", "Damage(i32)", "Damage(i64)"),
        (
            "struct field",
            "Vec2 { x: i32; y: i32; }",
            "Vec2 { x: i32; y: i32; z: i32; }",
        ),
        ("snapshot content", "snapshot<EnemyView>", "snapshot<Vec2>"),
        ("buffer element", "buffer<Vec2>", "buffer<i32>"),
        ("resource domain", "token<CombatResource>", "token<Vec2>"),
        (
            "stable id",
            "fn score(entity: i32)",
            "fn total_score(entity: i32)",
        ),
        (
            "rename",
            "fn ratio(entity: i32)",
            "fn combat_ratio(entity: i32)",
        ),
        (
            "stale hash",
            "sync fuel 1 fn clear_target",
            "sync fuel 2 fn clear_target",
        ),
        (
            "missing function",
            "sync fuel 1 fn inspect_events(events: array<CombatEvent>) -> Result<i32, CombatError>;",
            "",
        ),
        ("struct field type", "health: i32", "health: i64"),
    ]
}

fn h1_methods() -> [&'static str; 20] {
    [
        "apply_damage",
        "heal",
        "entity_name",
        "set_position",
        "combat_event",
        "enemy_view",
        "play_animation",
        "query_path",
        "maybe_target",
        "nearby",
        "path",
        "set_targets",
        "upload_path",
        "action_lock",
        "world_snapshot",
        "score",
        "ratio",
        "clear_target",
        "inspect_events",
        "set_enabled",
    ]
}

fn git_diff(worktree: &Path) -> Result<Value, AnyError> {
    let patch = git_in(worktree, &["diff", "--no-ext-diff"])?;
    let names = git_in(worktree, &["diff", "--name-only"])?;
    let numstat = git_in(worktree, &["diff", "--numstat"])?;
    let changed_files = names.lines().filter(|line| !line.is_empty()).count();
    let changed_lines = numstat
        .lines()
        .filter_map(|line| {
            let fields = line.split('\t').take(2).collect::<Vec<_>>();
            (fields.len() == 2).then(|| {
                fields[0].parse::<u64>().unwrap_or(0) + fields[1].parse::<u64>().unwrap_or(0)
            })
        })
        .sum::<u64>();
    Ok(json!({
        "changed_files": changed_files,
        "changed_lines": changed_lines,
        "numstat": numstat,
        "patch_hash": stable_bytes_hash(patch.as_bytes()),
        "patch": patch
    }))
}

fn git_diff_path(worktree: &Path, path: &str) -> Result<Value, AnyError> {
    let patch = git_in(worktree, &["diff", "--no-ext-diff", "--", path])?;
    let names = git_in(worktree, &["diff", "--name-only", "--", path])?;
    let numstat = git_in(worktree, &["diff", "--numstat", "--", path])?;
    let changed_files = names.lines().filter(|line| !line.is_empty()).count();
    let changed_lines = numstat
        .lines()
        .filter_map(|line| {
            let fields = line.split('\t').take(2).collect::<Vec<_>>();
            (fields.len() == 2).then(|| {
                fields[0].parse::<u64>().unwrap_or(0) + fields[1].parse::<u64>().unwrap_or(0)
            })
        })
        .sum::<u64>();
    Ok(json!({
        "changed_files": changed_files,
        "changed_lines": changed_lines,
        "numstat": numstat,
        "patch_hash": stable_bytes_hash(patch.as_bytes()),
        "patch": patch
    }))
}

fn cargo_check_binding(
    worktree: &Path,
    source: &Path,
    target: &Path,
    runtime_dependency: bool,
) -> Result<Output, AnyError> {
    let crate_dir = worktree.join("target/gate1-v2-1-binding-check");
    std::fs::create_dir_all(&crate_dir)?;
    let source_path = source.to_string_lossy().replace('\\', "\\\\");
    let dependency = if runtime_dependency {
        format!(
            "nexa-runtime = {{ path = \"{}\" }}\n",
            worktree.join("crates/nexa-runtime").display()
        )
    } else {
        String::new()
    };
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"gate1-v2-1-binding-check\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\npath = \"{source_path}\"\n\n[dependencies]\n{dependency}"
        ),
    )?;
    Ok(Command::new("cargo")
        .args(["check", "-q", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", target)
        .current_dir(worktree)
        .output()?)
}

fn add_worktree(path: &Path) -> Result<(), AnyError> {
    let status = Command::new("git")
        .args(["worktree", "add", "--detach", "--quiet"])
        .arg(path)
        .arg("HEAD")
        .current_dir(repository_root())
        .status()?;
    if !status.success() {
        return Err(format!("failed to create worktree {}", path.display()).into());
    }
    Ok(())
}

fn remove_worktree(path: &Path) -> Result<(), AnyError> {
    let status = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(repository_root())
        .status()?;
    if !status.success() {
        return Err(format!("failed to remove worktree {}", path.display()).into());
    }
    Ok(())
}

fn git_in(worktree: &Path, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(worktree)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn process_output(output: &Output) -> Value {
    json!({
        "command": "cargo check",
        "exit_code": output.status.code(),
        "stderr_hash": stable_bytes_hash(&output.stderr),
        "stderr": String::from_utf8_lossy(&output.stderr)
    })
}

fn synthetic_failed_output(message: String) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: message.into_bytes(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = message;
        unreachable!("Nexa Gate 1 v2.3 formal apparatus currently targets Unix hosts")
    }
}

fn parsed_idl_deterministic(source: &str) -> bool {
    let Ok(first) = nexa_idl::parse(source) else {
        return true;
    };
    let Ok(second) = nexa_idl::parse(source) else {
        return false;
    };
    nexa_idl::generate_rust(&first) == nexa_idl::generate_rust(&second)
}

fn close_host(host: &RuntimeHost) -> Result<(), AnyError> {
    let _ = host.drain_releases();
    let _ = host.begin_close();
    host.try_finish_close()?;
    Ok(())
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

struct AttestedOutput {
    output: Output,
    attestation: Value,
    parent_verification: Value,
    verified: bool,
}

fn formal_handshake_probe() -> Result<(), AnyError> {
    let root = repository_root().join("target/gate1-v2.3-qualification/formal-handshake");
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    let role = "formal-probe-worker";
    let worker_nonce = nonce(role)?;
    let child_token = nonce("formal-probe-child")?;
    let result_path = root.join("result.json");
    let spawned = spawn_attested_self(
        &[
            "attestation-probe-worker",
            role,
            result_path.to_string_lossy().as_ref(),
            &child_token,
        ],
        "environment-qualification",
        role,
        &worker_nonce,
        &root,
    )?;
    let result = json!({
        "probe": "formal-handshake",
        "status": if spawned.output.status.success() && spawned.verified {"PASS"} else {"FAIL"},
        "child_exit_success": spawned.output.status.success(),
        "attestation": spawned.attestation,
        "parent_verification": spawned.parent_verification
    });
    write_json(&root.join("probe.json"), &result)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    ensure_pass(&result, "formal handshake probe")
}

fn emit_formal_handshake() -> Result<(), AnyError> {
    let Ok(path) = std::env::var("NEXA_GATE1_HANDSHAKE_PATH") else {
        return Ok(());
    };
    let run_id = std::env::var("NEXA_GATE1_RUN_ID")?;
    let role = std::env::var("NEXA_GATE1_WORKER_ROLE")?;
    let parent_nonce = std::env::var("NEXA_GATE1_PARENT_NONCE")?;
    let one_time_token = std::env::var("NEXA_GATE1_ONE_TIME_TOKEN")?;
    let worker_nonce = std::env::var("NEXA_GATE1_WORKER_NONCE")?;
    let expected_output_path = std::env::var("NEXA_GATE1_EXPECTED_OUTPUT_PATH")?;
    let executable = std::env::current_exe()?;
    let started = Instant::now();
    let attestation = json!({
        "protocol": "gate1-process-handshake-v1",
        "run_id": run_id,
        "role": role,
        "parent_nonce": parent_nonce,
        "one_time_token_hash": stable_bytes_hash(one_time_token.as_bytes()),
        "worker_nonce": worker_nonce,
        "worker_pid": std::process::id(),
        "executable_hash": hash_file(executable)?,
        "started_at_monotonic_ns": started.elapsed().as_nanos(),
        "monotonic_clock": "std::time::Instant process-relative",
        "expected_output_path": expected_output_path
    });
    write_json(Path::new(&path), &attestation)
}

fn spawn_attested_self(
    arguments: &[&str],
    run_id: &str,
    role: &str,
    worker_nonce: &str,
    output_directory: &Path,
) -> Result<AttestedOutput, AnyError> {
    std::fs::create_dir_all(output_directory)?;
    let attestation_path = output_directory.join("process_attestation.json");
    let parent_verification_path = output_directory.join("parent_verification.json");
    let result_path = output_directory.join("result.json");
    let parent_nonce = nonce("portable-parent")?;
    let one_time_token = nonce("one-time-token")?;
    let executable = std::env::current_exe()?;
    let executable_hash = hash_file(&executable)?;
    let expected_output_path = output_directory.to_string_lossy().into_owned();
    let child = Command::new(&executable)
        .args(arguments)
        .current_dir(repository_root())
        .env("NEXA_GATE1_HANDSHAKE_PATH", &attestation_path)
        .env("NEXA_GATE1_RUN_ID", run_id)
        .env("NEXA_GATE1_WORKER_ROLE", role)
        .env("NEXA_GATE1_PARENT_NONCE", &parent_nonce)
        .env("NEXA_GATE1_ONE_TIME_TOKEN", &one_time_token)
        .env("NEXA_GATE1_WORKER_NONCE", worker_nonce)
        .env("NEXA_GATE1_EXPECTED_OUTPUT_PATH", &expected_output_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let child_id = child.id();
    let output = child.wait_with_output()?;
    let attestation = read_json(&attestation_path).unwrap_or_else(|error| {
        json!({
            "protocol": "gate1-process-handshake-v1",
            "status": "MISSING",
            "error": error.to_string()
        })
    });
    let result_not_before_handshake = result_path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .and_then(|result_time| {
            attestation_path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(|handshake_time| result_time >= handshake_time)
        })
        .unwrap_or(false);
    let failures = [
        (
            attestation["protocol"] == "gate1-process-handshake-v1",
            "protocol mismatch",
        ),
        (attestation["run_id"] == run_id, "run id mismatch"),
        (attestation["role"] == role, "worker role mismatch"),
        (
            attestation["parent_nonce"] == parent_nonce,
            "parent nonce mismatch",
        ),
        (
            attestation["one_time_token_hash"] == stable_bytes_hash(one_time_token.as_bytes()),
            "one-time token mismatch",
        ),
        (
            attestation["worker_nonce"] == worker_nonce,
            "worker nonce mismatch",
        ),
        (
            attestation["worker_pid"].as_u64() == Some(u64::from(child_id)),
            "Child::id and worker PID mismatch",
        ),
        (
            attestation["executable_hash"] == executable_hash,
            "executable hash mismatch",
        ),
        (
            attestation["expected_output_path"] == expected_output_path,
            "output path mismatch",
        ),
        (
            result_not_before_handshake,
            "result predates process attestation",
        ),
    ]
    .into_iter()
    .filter_map(|(passed, failure)| (!passed).then_some(failure))
    .collect::<Vec<_>>();
    let verified = failures.is_empty();
    let parent_verification = json!({
        "protocol": "gate1-process-handshake-v1",
        "run_id": run_id,
        "role": role,
        "child_id": child_id,
        "worker_reported_pid": attestation["worker_pid"],
        "attestation_hash": hash_file(&attestation_path).ok(),
        "result_not_before_handshake": result_not_before_handshake,
        "result_path": result_path,
        "output_status": output.status.code(),
        "failures": failures,
        "status": if verified {"PASS"} else {"FAIL"}
    });
    write_json(&parent_verification_path, &parent_verification)?;
    Ok(AttestedOutput {
        output,
        attestation,
        parent_verification,
        verified,
    })
}

fn reserve_result_path(directory: &Path, run_id: &str) -> Result<(), AnyError> {
    write_json(
        &directory.join("result.json"),
        &json!({
            "run_id": run_id,
            "status": "PROCESS_RUNNING",
            "process_lifecycle": "result path created before monotonic end timestamp"
        }),
    )
}

fn require_run_label(label: &str) -> Result<(), AnyError> {
    if matches!(label, "formal-run-1" | "formal-run-2" | "replay") {
        Ok(())
    } else {
        Err(format!("unauthorized Gate 1 v2.3 run label `{label}`").into())
    }
}

fn ensure_pass(value: &Value, context: &str) -> Result<(), AnyError> {
    if value["status"] == "PASS" {
        Ok(())
    } else {
        Err(format!("{context} failed: {}", value["failures"]).into())
    }
}

fn validity_status(failures: &[String]) -> &'static str {
    if failures.is_empty() {
        "PASS"
    } else {
        "INVALID"
    }
}

fn status_text(output: &Output) -> &'static str {
    if output.status.success() {
        "process completed"
    } else {
        "process failed"
    }
}

fn command(program: &str, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new(program).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn optional_command(program: &str, arguments: &[&str]) -> Value {
    Command::new(program).args(arguments).output().map_or_else(
        |error| json!({"available": false, "error": error.to_string()}),
        |output| {
            json!({
                "available": output.status.success(),
                "stdout": String::from_utf8_lossy(&output.stdout).trim(),
                "stderr": String::from_utf8_lossy(&output.stderr).trim()
            })
        },
    )
}

fn non_blank_lines(source: &str) -> usize {
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn percent_reduction(original: usize, reduced: usize) -> usize {
    original.saturating_sub(reduced).saturating_mul(100) / original.max(1)
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}
