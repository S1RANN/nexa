#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_9::{AnyError, hash_file, read_json, stable_value_hash, write_json};
use serde_json::{Value, json};

pub const RAW_ROOT: &str = "reports/raw/gate1_v2_9";
pub const RUNS: [&str; 3] = ["formal-run-1", "formal-run-2", "replay"];
pub const GATE_NAMES: [&str; 21] = [
    "history",
    "governance",
    "environment",
    "process",
    "validity",
    "h1_equivalence",
    "h1_metrics",
    "h2_configuration",
    "h2_cleanup",
    "h2_invariants",
    "h2_allocations",
    "h2_performance",
    "h3_migration",
    "h3_completion",
    "h3_transaction",
    "comparison",
    "replay",
    "pilot",
    "budget",
    "workspace",
    "artifact_hygiene",
];

const STATIC_INPUTS: [&str; 58] = [
    "experiments/gate1-v2.9/qualification/environment_qualification.json",
    "experiments/gate1-v2.9/qualification/environment_qualification_hashes.json",
    "experiments/gate1-v2.9/qualification/root-cause.json",
    "experiments/gate1-v2.9/qualification/formal-handshake/process_attestation.json",
    "experiments/gate1-v2.9/qualification/formal-handshake/parent_verification.json",
    "experiments/gate1-v2.9/qualification/formal-handshake/probe.json",
    "experiments/gate1-v2.9/prefreeze/prefreeze_closure.json",
    "experiments/gate1-v2.9/prefreeze/history_check.json",
    "experiments/gate1-v2.9/prefreeze/governance_negative_tests.json",
    "experiments/gate1-v2.9/prefreeze/scenario_independence.json",
    "experiments/gate1-v2.9/prefreeze/outcome_transport.json",
    "experiments/gate1-v2.9/prefreeze/h1_transformer_equivalence.json",
    "experiments/gate1-v2.9/prefreeze/h2_dimension_effectiveness.json",
    "experiments/gate1-v2.9/prefreeze/h2_cleanup_independence.json",
    "experiments/gate1-v2.9/prefreeze/h2_projection.json",
    "experiments/gate1-v2.9/prefreeze/h2_noise_invariance.json",
    "experiments/gate1-v2.9/prefreeze/h2_semantic_sensitivity.json",
    "experiments/gate1-v2.9/prefreeze/comparison_policy.json",
    "experiments/gate1-v2.9/prefreeze/comparison_truth_table.json",
    "experiments/gate1-v2.9/prefreeze/inconclusive_contract.json",
    "experiments/gate1-v2.9/prefreeze/h3_execution_independence.json",
    "experiments/gate1-v2.9/prefreeze/raw_regeneration_exercise.json",
    "experiments/gate1-v2.9/prefreeze/contract_satisfiability.json",
    "experiments/gate1-v2.9/prefreeze/decision_branches.json",
    "experiments/gate1-v2.9/prefreeze/decision_state_check.json",
    "experiments/gate1-v2.9/prefreeze/status_lint.json",
    "experiments/gate1-v2.9/prefreeze/structural_failure_regression.json",
    "experiments/gate1-v2.9/prefreeze/synthetic_git_chain.json",
    "experiments/gate1-v2.9/prefreeze/terminal_short_circuit.json",
    "experiments/gate1-v2.9/authorization.json",
    "experiments/gate1-v2.9/threshold_equivalence.json",
    "experiments/gate1-v2.9/h2_semantic_projection.json",
    "experiments/gate1-v2.9/h2_performance_policy.json",
    "experiments/gate1-v2.9/decision_state_machine.json",
    "experiments/gate1-v2.9/pilot.json",
    "experiments/gate1-v2.9/budget.json",
    "reports/history/gate1/current_status.json",
    "reports/history/gate1/supersession_graph.json",
    "reports/history/gate1/index.json",
    "reports/history/gate1/versions/gate1-v2.5.json",
    "reports/history/gate1/versions/gate1-v2.6.json",
    "reports/history/gate1/versions/gate1-v2.7.json",
    "reports/history/gate1/versions/gate1-v2.8.json",
    "reports/history/gate1/versions/gate1-v2.9.json",
    "reports/history/gate1/v2_5/terminal.json",
    "reports/history/gate1/v2_5/commits.json",
    "reports/history/gate1/v2_5/raw_manifest.json",
    "reports/history/gate1/v2_6/terminal.json",
    "reports/history/gate1/v2_6/commits.json",
    "reports/history/gate1/v2_6/raw_manifest.json",
    "reports/history/gate1/v2_7/terminal.json",
    "reports/history/gate1/v2_7/attempt_manifest.json",
    "reports/history/gate1/v2_8/terminal.json",
    "reports/history/gate1/v2_8/terminal.md",
    "reports/history/gate1/v2_8/commits.json",
    "reports/history/gate1/v2_8/raw_manifest.json",
    "reports/contracts/gate1_v2_8_semantic_invalidation.json",
    "reports/gate1_v2_8_semantic_invalidation.md",
];

pub fn package_all() -> Result<(), AnyError> {
    let raw = Path::new(RAW_ROOT);
    if raw.exists() {
        return Err(
            "reports/raw/gate1_v2_9 already exists; Evidence packaging is immutable".into(),
        );
    }
    for run in RUNS {
        copy_evidence_tree(
            &Path::new("target/gate1-v2.9").join(run),
            &raw.join("runs").join(run),
        )?;
    }
    copy_evidence_tree(
        Path::new("target/gate1-v2.9/comparisons"),
        &raw.join("comparisons"),
    )?;
    for source in STATIC_INPUTS {
        let destination = raw.join("static").join(source);
        copy_evidence_file(Path::new(source), &destination)?;
    }
    for source in [
        "experiments/gate1-v2.9/h1_mutations.json",
        "experiments/gate1-v2.9/h2_matrix.json",
        "experiments/gate1-v2.9/h2_cleanup_matrix.json",
        "experiments/gate1-v2.9/h3_scenarios.json",
        "experiments/gate1-v2.9/scenario_schema.json",
    ] {
        copy_evidence_file(Path::new(source), &raw.join("static").join(source))?;
    }
    generate_from_raw(raw, &raw.join("gates"))?;
    write_raw_hash_manifest(raw)
}

pub fn generate_from_raw(raw: &Path, output: &Path) -> Result<(), AnyError> {
    let first_supervisor = raw_json(raw, "runs/formal-run-1/result.json")?;
    let implementation_sha = text_at(&first_supervisor, "/implementation_sha")?;
    let implementation_tree = text_at(&first_supervisor, "/implementation_tree")?;
    let gates = build_gates(raw, implementation_sha, implementation_tree)?;
    if gates.len() != GATE_NAMES.len() {
        return Err("Gate generator did not produce exactly 21 Gates".into());
    }
    std::fs::create_dir_all(output)?;
    for name in GATE_NAMES {
        let gate = gates
            .get(name)
            .ok_or_else(|| format!("Gate generator omitted {name}"))?;
        write_json(&output.join(format!("{name}.json")), gate)?;
    }
    let manifest = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.9",
        "gate_count": GATE_NAMES.len(),
        "gates": GATE_NAMES,
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree
    });
    write_json(&output.join("manifest.json"), &manifest)
}

pub fn rebuild_and_compare() -> Result<(), AnyError> {
    let raw = Path::new(RAW_ROOT);
    let recorded = raw.join("gates");
    let regenerated = Path::new("target/gate1-v2.9-gate-rebuild");
    if regenerated.exists() {
        std::fs::remove_dir_all(regenerated)?;
    }
    generate_from_raw(raw, regenerated)?;
    let mut compared = BTreeMap::new();
    for name in GATE_NAMES.into_iter().chain(std::iter::once("manifest")) {
        let file = format!("{name}.json");
        let recorded_bytes = std::fs::read(recorded.join(&file))?;
        let regenerated_bytes = std::fs::read(regenerated.join(&file))?;
        if recorded_bytes != regenerated_bytes {
            return Err(format!("regenerated Gate {file} differs byte-for-byte").into());
        }
        compared.insert(
            file,
            json!({
                "bytes": recorded_bytes.len(),
                "hash": hash_file(recorded.join(format!("{name}.json")))?
            }),
        );
    }
    std::fs::remove_dir_all(regenerated)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "experiment_version": "gate1-v2.9",
            "status": "PASS",
            "compared": compared
        }))?
    );
    Ok(())
}

pub fn artifact_hygiene() -> Result<(), AnyError> {
    let violations = hygiene_violations(Path::new(RAW_ROOT))?;
    if !violations.is_empty() {
        return Err(format!("Evidence artifact hygiene failed: {violations:?}").into());
    }
    println!("Gate 1 v2.9 Evidence artifact hygiene: PASS");
    Ok(())
}

fn write_raw_hash_manifest(raw: &Path) -> Result<(), AnyError> {
    let mut files = Vec::new();
    walk(raw, &mut files)?;
    files.sort();
    let entries = files
        .into_iter()
        .filter(|path| path != &raw.join("raw_hash_manifest.json"))
        .map(|path| {
            let relative = path.strip_prefix(raw)?.to_string_lossy().to_string();
            Ok(json!({
                "path": relative,
                "bytes": std::fs::metadata(&path)?.len(),
                "hash": hash_file(&path)?
            }))
        })
        .collect::<Result<Vec<_>, AnyError>>()?;
    write_json(
        &raw.join("raw_hash_manifest.json"),
        &json!({
            "schema_version": 1,
            "experiment_version": "gate1-v2.9",
            "artifact_count": entries.len(),
            "artifacts": entries
        }),
    )
}

fn build_gates(
    raw: &Path,
    implementation_sha: &str,
    implementation_tree: &str,
) -> Result<BTreeMap<&'static str, Value>, AnyError> {
    let supervisors = RUNS
        .map(|run| raw_json(raw, &format!("runs/{run}/result.json")))
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let h1 = run_children(raw, "h1")?;
    let h2 = run_children(raw, "h2")?;
    let h3 = run_children(raw, "h3")?;
    let formal_comparison = raw_json(raw, "comparisons/formal-run-1__formal-run-2.json")?;
    let replay_comparison = raw_json(raw, "comparisons/formal-run-1__replay.json")?;

    let history_check = static_json(raw, "experiments/gate1-v2.9/prefreeze/history_check.json")?;
    let v2_5 = static_json(raw, "reports/history/gate1/versions/gate1-v2.5.json")?;
    let v2_5_terminal = static_json(raw, "reports/history/gate1/v2_5/terminal.json")?;
    let v2_5_commits = static_json(raw, "reports/history/gate1/v2_5/commits.json")?;
    let v2_5_manifest = static_json(raw, "reports/history/gate1/v2_5/raw_manifest.json")?;
    let v2_6 = static_json(raw, "reports/history/gate1/versions/gate1-v2.6.json")?;
    let v2_6_terminal = static_json(raw, "reports/history/gate1/v2_6/terminal.json")?;
    let v2_6_commits = static_json(raw, "reports/history/gate1/v2_6/commits.json")?;
    let v2_6_manifest = static_json(raw, "reports/history/gate1/v2_6/raw_manifest.json")?;
    let v2_7 = static_json(raw, "reports/history/gate1/versions/gate1-v2.7.json")?;
    let v2_7_terminal = static_json(raw, "reports/history/gate1/v2_7/terminal.json")?;
    let v2_7_attempt = static_json(raw, "reports/history/gate1/v2_7/attempt_manifest.json")?;
    let v2_8 = static_json(raw, "reports/history/gate1/versions/gate1-v2.8.json")?;
    let v2_8_terminal = static_json(raw, "reports/history/gate1/v2_8/terminal.json")?;
    let v2_8_commits = static_json(raw, "reports/history/gate1/v2_8/commits.json")?;
    let v2_8_manifest = static_json(raw, "reports/history/gate1/v2_8/raw_manifest.json")?;
    let current = static_json(raw, "reports/history/gate1/current_status.json")?;
    let history_failures = failures([
        (
            history_check["status"] == "PASS",
            "prefreeze history check did not pass",
        ),
        (
            v2_5["status"] == "STRUCTURAL_CLOSURE_FAILED" && v2_5["decision_usable"] == false,
            "v2.5 was not sealed as structurally incomplete",
        ),
        (
            v2_5_terminal["status"] == "STRUCTURAL_CLOSURE_FAILED"
                && v2_5_terminal["decision_usable"] == false,
            "v2.5 terminal state is not an unusable structural failure",
        ),
        (
            v2_5_commits["implementation"]["sha"] == "c00665630871dc813541e2bb4506141f57cd51da"
                && v2_5_commits["evidence"]["sha"] == "d3876db85f2c031a51b6a4f87abdd51515c5fbdf",
            "v2.5 historical commits are not bound",
        ),
        (
            v2_5_manifest["formal_evidence_usable"] == false
                && v2_5_manifest["artifact_count"] == 331,
            "v2.5 Raw archive manifest is incomplete or decision-usable",
        ),
        (
            v2_6["status"] == "STRUCTURAL_CLOSURE_FAILED"
                && v2_6["decision_usable"] == false
                && v2_6["recorded_product_decision_authorized"] == false,
            "v2.6 was not sealed as structurally incomplete and decision-ineligible",
        ),
        (
            v2_6_terminal["status"] == "STRUCTURAL_CLOSURE_FAILED"
                && v2_6_terminal["gate1_status"] == "INCOMPLETE"
                && v2_6_terminal["decision_usable"] == false
                && v2_6_terminal["recorded_product_decision_authorized"] == false,
            "v2.6 terminal state does not invalidate its recorded STOP decision",
        ),
        (
            v2_6_commits["implementation"]["sha"] == "e83579984056226737202fad455ef9e312ae2a22"
                && v2_6_commits["evidence"]["sha"] == "a9c3ddd2d10c793bff21bd51dccb79ccf7f65f5c"
                && v2_6_commits["finalization"]["sha"]
                    == "a59e028ad1067bab6284df3fe83311c938a6c903"
                && v2_6_commits["decision"]["authorized"] == false,
            "v2.6 historical commit chain is not bound or is still marked authorized",
        ),
        (
            v2_6_manifest["formal_evidence_usable"] == false
                && v2_6_manifest["decision_usable"] == false
                && v2_6_manifest["artifact_count"] == 337,
            "v2.6 Raw archive manifest is incomplete or decision-usable",
        ),
        (
            v2_7["status"] == "INVALID_ENVIRONMENT_EXECUTION"
                && v2_7["gate1_status"] == "INCOMPLETE"
                && v2_7["decision_usable"] == false,
            "v2.7 was not sealed as an invalid environment execution",
        ),
        (
            v2_7_terminal["status"] == "INVALID_ENVIRONMENT_EXECUTION"
                && v2_7_terminal["formal_run_1_status"] == "INVALID"
                && v2_7_terminal["formal_run_2_started"] == false
                && v2_7_terminal["replay_started"] == false
                && v2_7_terminal["decision_usable"] == false,
            "v2.7 terminal record does not preserve its zero-retry invalid state",
        ),
        (
            v2_7_attempt["implementation"]["sha"] == "6e65ab62e592536609ebe7de30a9f753a5df3b39"
                && v2_7_attempt["status"] == "INVALID"
                && v2_7_attempt["formal_evidence_usable"] == false
                && v2_7_attempt["artifacts"].as_array().map_or(0, Vec::len) == 5,
            "v2.7 invalid attempt manifest is not bound",
        ),
        (
            v2_8["status"] == "SEMANTICALLY_INSUFFICIENT"
                && v2_8["decision_usable"] == false
                && v2_8["recorded_product_decision_authorized"] == false,
            "v2.8 was not sealed as semantically insufficient",
        ),
        (
            v2_8_terminal["status"] == "SEMANTICALLY_INSUFFICIENT"
                && v2_8_terminal["formal_evidence_usable"] == true
                && v2_8_terminal["recorded_product_decision_authorized"] == false
                && v2_8_terminal["correct_stable_core_failures"] == json!(["H1", "H2", "H3"]),
            "v2.8 terminal record does not isolate its decision-semantic failure",
        ),
        (
            v2_8_commits["implementation"]["sha"] == "61b091636b14484b7776f76d1147c0c599b87922"
                && v2_8_commits["evidence"]["sha"] == "22f4b15d6ec37c7b3550abae2617d646b1e7d091"
                && v2_8_commits["finalization"]["sha"]
                    == "3089d7cf7ca225a2fdf24b49903924601ef88633"
                && v2_8_commits["semantic_decision_valid"] == false,
            "v2.8 historical commit chain is not bound to its semantic invalidation",
        ),
        (
            v2_8_manifest["formal_evidence_usable"] == true
                && v2_8_manifest["decision_usable"] == false
                && v2_8_manifest["artifact_count"] == 343,
            "v2.8 Raw history was not preserved as valid evidence with invalid decision",
        ),
        (
            current["current_experiment"] == "gate1-v2.8"
                && current["finalization_status"] == "FINALIZED",
            "pre-F2.9 published status is not the immutable v2.8 terminal snapshot",
        ),
    ]);
    let history = gate(
        "history",
        "APPARATUS",
        &history_failures,
        "PASS",
        json!({
            "v2_5_status": v2_5["status"],
            "v2_5_decision_usable": v2_5["decision_usable"],
            "v2_5_terminal_status": v2_5_terminal["status"],
            "v2_5_archive_artifact_count": v2_5_manifest["artifact_count"],
            "v2_6_status": v2_6["status"],
            "v2_6_gate1_status": v2_6_terminal["gate1_status"],
            "v2_6_recorded_decision": v2_6_terminal["recorded_product_decision"],
            "v2_6_recorded_decision_authorized": v2_6_terminal["recorded_product_decision_authorized"],
            "v2_6_archive_artifact_count": v2_6_manifest["artifact_count"],
            "v2_7_status": v2_7["status"],
            "v2_7_gate1_status": v2_7_terminal["gate1_status"],
            "v2_7_formal_run_1_status": v2_7_terminal["formal_run_1_status"],
            "v2_7_decision_usable": v2_7_terminal["decision_usable"],
            "v2_7_bound_invalid_artifact_count": v2_7_attempt["artifacts"].as_array().map_or(0, Vec::len),
            "v2_8_status": v2_8["status"],
            "v2_8_gate1_status": v2_8_terminal["gate1_status"],
            "v2_8_recorded_decision": v2_8_terminal["recorded_product_decision"],
            "v2_8_recorded_decision_authorized": v2_8_terminal["recorded_product_decision_authorized"],
            "v2_8_correct_stable_core_failures": v2_8_terminal["correct_stable_core_failures"],
            "v2_8_archive_artifact_count": v2_8_manifest["artifact_count"],
            "published_status_experiment": current["current_experiment"],
            "v2_9_status_publication_pending_f": true,
            "history_prefreeze_status": history_check["status"]
        }),
        implementation_sha,
        implementation_tree,
    );

    let closure = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/prefreeze_closure.json",
    )?;
    let negative = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/governance_negative_tests.json",
    )?;
    let authorization = static_json(raw, "experiments/gate1-v2.9/authorization.json")?;
    let thresholds = static_json(raw, "experiments/gate1-v2.9/threshold_equivalence.json")?;
    let formal_decision_evidence = formal_decision_evidence(
        &supervisors,
        &h1,
        &h2,
        &h3,
        &formal_comparison,
        &replay_comparison,
    )?;
    let governance_failures = failures([
        (
            closure["status"] == "PASS",
            "prefreeze closure did not pass",
        ),
        (
            negative["status"] == "PASS",
            "governance negative tests did not pass",
        ),
        (
            authorization["status"] == "AUTHORIZED",
            "v2.9 authorization is absent",
        ),
        (
            thresholds["outcome_thresholds_weakened"] == false,
            "outcome thresholds were weakened",
        ),
        (
            formal_decision_evidence["apparatus_statuses"] == json!(["PASS", "PASS", "PASS"]),
            "formal apparatus statuses are not three PASS values",
        ),
        (
            formal_decision_evidence["stable_core_failures"] == json!(["H1", "H2", "H3"]),
            "formal Stable Core Failure extraction did not find H1/H2/H3",
        ),
        (
            formal_decision_evidence["derived_product_decision"] == "STOP",
            "formal Stable Core Failure did not dominate comparison INCONCLUSIVE",
        ),
    ]);
    let governance = gate(
        "governance",
        "APPARATUS",
        &governance_failures,
        "PASS",
        json!({
            "prefreeze_closure": closure["status"],
            "negative_tests": negative["status"],
            "authorization": authorization["status"],
            "thresholds_weakened": thresholds["outcome_thresholds_weakened"],
            "formal_decision_evidence": formal_decision_evidence
        }),
        implementation_sha,
        implementation_tree,
    );

    let qualification = static_json(
        raw,
        "experiments/gate1-v2.9/qualification/environment_qualification.json",
    )?;
    let environment_failures = failures([(
        qualification["status"] == "QUALIFIED",
        "formal environment is not qualified",
    )]);
    let environment = gate(
        "environment",
        "APPARATUS",
        &environment_failures,
        "PASS",
        json!({
            "qualification_status": qualification["status"],
            "hard_failures": qualification["hard_failures"],
            "qualification_hash": stable_value_hash(&qualification)
        }),
        implementation_sha,
        implementation_tree,
    );

    let validity_files = RUNS
        .iter()
        .enumerate()
        .map(|(index, run)| {
            let name = match index {
                0 => "validity_run1.json",
                1 => "validity_run2.json",
                _ => "validity_replay.json",
            };
            raw_json(raw, &format!("runs/{run}/{name}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let child_verifications = RUNS
        .iter()
        .map(|run| {
            ["worker", "h1", "h2", "h3"]
                .map(|child| raw_json(raw, &format!("runs/{run}/{child}/parent_verification.json")))
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let supervisor_pids = supervisors
        .iter()
        .filter_map(|value| value.pointer("/process/pid").and_then(Value::as_u64))
        .collect::<BTreeSet<_>>();
    let supervisor_nonces = supervisors
        .iter()
        .filter_map(|value| {
            value
                .pointer("/process/process_nonce")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let worker_pids = supervisors
        .iter()
        .filter_map(|value| {
            value
                .pointer("/worker_result/worker_pid")
                .and_then(Value::as_u64)
        })
        .collect::<BTreeSet<_>>();
    let worker_nonces = supervisors
        .iter()
        .filter_map(|value| {
            value
                .pointer("/worker_result/worker_nonce")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let mut process_failures = Vec::new();
    for (index, supervisor) in supervisors.iter().enumerate() {
        if supervisor["apparatus_status"] != "PASS" {
            process_failures.push(format!("{} supervisor apparatus is not PASS", RUNS[index]));
        }
        if supervisor["run_id"] != RUNS[index] {
            process_failures.push(format!("{} supervisor run_id mismatch", RUNS[index]));
        }
        if supervisor["implementation_sha"] != implementation_sha
            || supervisor["implementation_tree"] != implementation_tree
        {
            process_failures.push(format!(
                "{} supervisor implementation identity mismatch",
                RUNS[index]
            ));
        }
        for (child_index, verification) in child_verifications[index].iter().enumerate() {
            if verification["status"] != "PASS" {
                process_failures.push(format!(
                    "{} {} parent verification is not PASS",
                    RUNS[index],
                    ["worker", "h1", "h2", "h3"][child_index]
                ));
            }
        }
    }
    if supervisor_pids.len() != RUNS.len()
        || supervisor_nonces.len() != RUNS.len()
        || worker_pids.len() != RUNS.len()
        || worker_nonces.len() != RUNS.len()
    {
        process_failures.push(
            "formal-run-1, formal-run-2, and replay do not have independent process identities"
                .to_owned(),
        );
    }
    let process = gate(
        "process",
        "APPARATUS",
        &process_failures,
        "PASS",
        json!({
            "supervisor_count": supervisors.len(),
            "apparatus_statuses": supervisors.iter().map(|value| value["apparatus_status"].clone()).collect::<Vec<_>>(),
            "independent_supervisor_pids": supervisor_pids.len(),
            "independent_supervisor_nonces": supervisor_nonces.len(),
            "independent_worker_pids": worker_pids.len(),
            "independent_worker_nonces": worker_nonces.len(),
            "verified_child_handshakes": child_verifications.iter().flatten().filter(|value| value["status"] == "PASS").count()
        }),
        implementation_sha,
        implementation_tree,
    );

    let mut validity_failures = Vec::new();
    for (index, validity_record) in validity_files.iter().enumerate() {
        if validity_record["status"] != "PASS"
            || validity_record["preflight"]["status"] != "PASS"
            || validity_record["postflight"]["status"] != "PASS"
        {
            validity_failures.push(format!(
                "{} preflight, execution, or postflight validity is not PASS",
                RUNS[index]
            ));
        }
        if validity_record["preflight"]["implementation_sha"] != implementation_sha
            || validity_record["postflight"]["implementation_sha"] != implementation_sha
        {
            validity_failures.push(format!(
                "{} validity implementation identity mismatch",
                RUNS[index]
            ));
        }
        if validity_record["failures"]
            .as_array()
            .is_none_or(|values| !values.is_empty())
        {
            validity_failures.push(format!("{} validity recorded failures", RUNS[index]));
        }
    }
    let validity_outcome = if process_failures.is_empty() && validity_failures.is_empty() {
        "PASS"
    } else {
        "INVALID"
    };
    let validity = gate(
        "validity",
        "APPARATUS",
        &validity_failures,
        validity_outcome,
        json!({
            "apparatus_valid": process_failures.is_empty() && validity_failures.is_empty(),
            "formal_run_count": supervisors.len(),
            "preflight_postflight_present": validity_files.len() == RUNS.len(),
            "validity_statuses": validity_files.iter().map(|value| value["status"].clone()).collect::<Vec<_>>(),
            "preflight_statuses": validity_files.iter().map(|value| value["preflight"]["status"].clone()).collect::<Vec<_>>(),
            "postflight_statuses": validity_files.iter().map(|value| value["postflight"]["status"].clone()).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );

    let h1_equivalence = outcome_gate(
        "h1_equivalence",
        &h1,
        |value| {
            value["artifacts"].as_array().is_some_and(|artifacts| {
                artifacts.len() == 20
                    && artifacts
                        .iter()
                        .all(|artifact| artifact["semantic_equivalence"] == true)
            })
        },
        json!({
            "mutation_counts": h1.iter().map(|value| value["artifacts"].as_array().map_or(0, Vec::len)).collect::<Vec<_>>(),
            "equivalent_patch_counts": h1.iter().map(|value| value["artifacts"].as_array().map_or(0, |items| items.iter().filter(|item| item["semantic_equivalence"] == true).count())).collect::<Vec<_>>(),
            "semantic_signatures": signatures(&h1)
        }),
        implementation_sha,
        implementation_tree,
    );
    let h1_metrics = outcome_gate(
        "h1_metrics",
        &h1,
        |value| {
            metric_value(value, "api_count") == Some(20)
                && metric_value(value, "abi_change_count") == Some(20)
                && metric_value(value, "early_rejections") == Some(20)
                && metric_value(value, "runtime_errors") == Some(0)
                && metric_value(value, "maintained_line_reduction_percent")
                    .is_some_and(|value| value >= 50)
                && metric_value(value, "edit_point_reduction_percent")
                    .is_some_and(|value| value >= 50)
                && value
                    .pointer("/metrics/generation_deterministic/value")
                    .and_then(Value::as_bool)
                    == Some(true)
        },
        json!({
            "api_counts": h1.iter().map(|value| value.pointer("/metrics/api_count/value").cloned()).collect::<Vec<_>>(),
            "early_rejections": h1.iter().map(|value| value.pointer("/metrics/early_rejections/value").cloned()).collect::<Vec<_>>(),
            "line_reduction": h1.iter().map(|value| value.pointer("/metrics/maintained_line_reduction_percent/value").cloned()).collect::<Vec<_>>(),
            "edit_reduction": h1.iter().map(|value| value.pointer("/metrics/edit_point_reduction_percent/value").cloned()).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );

    let h2_configuration = outcome_gate_with_contract(
        "h2_configuration",
        &h2,
        |value| {
            value
                .pointer("/semantic/snapshot_scenarios")
                .and_then(Value::as_array)
                .is_some_and(|scenarios| {
                    scenarios.len() == 32
                        && scenarios.iter().all(|scenario| {
                            scenario["expected_calls"] == scenario["observed_calls"]
                                && scenario["expected_promotions"]
                                    == scenario["observed_promotions"]
                        })
                })
        },
        |value| {
            value
                .pointer("/semantic/snapshot_scenarios")
                .and_then(Value::as_array)
                .is_some_and(|scenarios| {
                    scenarios.len() == 32
                        && scenarios
                            .iter()
                            .filter_map(|item| item["execution_fingerprint"].as_str())
                            .collect::<BTreeSet<_>>()
                            .len()
                            == 32
                })
        },
        json!({
            "configuration_counts": h2.iter().map(|value| value.pointer("/semantic/snapshot_scenarios").and_then(Value::as_array).map_or(0, Vec::len)).collect::<Vec<_>>(),
            "execution_fingerprint_counts": h2.iter().map(|value| value.pointer("/semantic/snapshot_scenarios").and_then(Value::as_array).map_or(0, |items| items.iter().filter_map(|item| item["execution_fingerprint"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>(),
            "semantic_signatures": signatures(&h2)
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_cleanup = outcome_gate_with_contract(
        "h2_cleanup",
        &h2,
        |value| {
            value
                .pointer("/semantic/cleanup_matrix")
                .and_then(Value::as_array)
                .is_some_and(|cleanup| {
                    cleanup.len() == 12
                        && cleanup.iter().all(|item| item["status"] == "observed")
                        && cleanup
                            .iter()
                            .filter_map(|item| item["trigger_fingerprint"].as_str())
                            .collect::<BTreeSet<_>>()
                            .len()
                            == 12
                })
        },
        |value| {
            value
                .pointer("/semantic/cleanup_matrix")
                .and_then(Value::as_array)
                .is_some_and(|cleanup| cleanup.len() == 12)
        },
        json!({
            "cleanup_counts": h2.iter().map(|value| value.pointer("/semantic/cleanup_matrix").and_then(Value::as_array).map_or(0, Vec::len)).collect::<Vec<_>>(),
            "trigger_fingerprint_counts": h2.iter().map(|value| value.pointer("/semantic/cleanup_matrix").and_then(Value::as_array).map_or(0, |items| items.iter().filter_map(|item| item["trigger_fingerprint"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_invariants = outcome_gate_with_contract(
        "h2_invariants",
        &h2,
        |value| {
            value
                .pointer("/semantic/snapshot_scenarios")
                .and_then(Value::as_array)
                .is_some_and(|scenarios| {
                    scenarios.len() == 32
                        && scenarios.iter().all(|scenario| {
                            scenario["violations"].as_array().is_some_and(Vec::is_empty)
                        })
                })
        },
        |value| {
            value
                .pointer("/semantic/snapshot_scenarios")
                .and_then(Value::as_array)
                .is_some_and(|scenarios| {
                    scenarios.len() == 32
                        && scenarios
                            .iter()
                            .map(|scenario| {
                                scenario["violations"]
                                    .as_array()
                                    .map_or(usize::MAX, Vec::len)
                            })
                            .sum::<usize>()
                            == 0
                })
        },
        json!({
            "violation_counts": h2.iter().map(|value| value.pointer("/semantic/snapshot_scenarios").and_then(Value::as_array).map_or(usize::MAX, |items| items.iter().map(|item| item["violations"].as_array().map_or(usize::MAX, Vec::len)).sum())).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_allocations = outcome_gate_with_contract(
        "h2_allocations",
        &h2,
        |value| {
            value["allocator_failures"]
                .as_array()
                .is_some_and(Vec::is_empty)
                && absolute_allocation_contract(value)
        },
        absolute_allocation_contract,
        json!({
            "allocator_failures": h2.iter().map(|value| value["allocator_failures"].clone()).collect::<Vec<_>>(),
            "allocation_counts": h2.iter().map(|value| value.pointer("/metrics/allocation_counts/value").cloned()).collect::<Vec<_>>(),
            "observer_run_counts": h2.iter().map(|value| value.pointer("/allocation_observer/runs").and_then(Value::as_array).map_or(0, Vec::len)).collect::<Vec<_>>(),
            "absolute_zero_verified": h2.iter().map(absolute_allocation_contract).collect::<Vec<_>>(),
            "baseline_subtracted": h2.iter().map(|_| false).collect::<Vec<_>>(),
            "count_semantics": "ABSOLUTE_GLOBAL_ALLOCATOR_DELTAS"
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_performance = outcome_gate_with_contract(
        "h2_performance",
        &h2,
        |value| {
            value["timing_failures"]
                .as_array()
                .is_some_and(Vec::is_empty)
                && performance_contract(value)
        },
        performance_contract,
        json!({
            "timing_failures": h2.iter().map(|value| value["timing_failures"].clone()).collect::<Vec<_>>(),
            "benchmark_process_counts": h2.iter().map(|value| value["performance_processes"].as_array().map_or(0, Vec::len)).collect::<Vec<_>>(),
            "warmup_samples": h2.iter().map(|value| value.pointer("/metrics/warmup_samples/value").cloned()).collect::<Vec<_>>(),
            "timed_samples": h2.iter().map(|value| value.pointer("/metrics/timed_samples/value").cloned()).collect::<Vec<_>>(),
            "process_sample_counts": h2.iter().map(|value| value["performance_processes"].as_array().map_or(Vec::new(), |items| items.iter().map(|item| item["samples"].clone()).collect())).collect::<Vec<Vec<Value>>>()
        }),
        implementation_sha,
        implementation_tree,
    );

    let h3_migration = h3_group_gate(
        "h3_migration",
        "migration",
        11,
        &h3,
        implementation_sha,
        implementation_tree,
    );
    let h3_completion = h3_group_gate(
        "h3_completion",
        "completion",
        10,
        &h3,
        implementation_sha,
        implementation_tree,
    );
    let h3_transaction = h3_group_gate(
        "h3_transaction",
        "transaction",
        9,
        &h3,
        implementation_sha,
        implementation_tree,
    );

    let comparison = comparison_gate(
        "comparison",
        &formal_comparison,
        implementation_sha,
        implementation_tree,
    );
    let replay = comparison_gate(
        "replay",
        &replay_comparison,
        implementation_sha,
        implementation_tree,
    );

    let pilot_value = static_json(raw, "experiments/gate1-v2.9/pilot.json")?;
    let pilot = gate(
        "pilot",
        "EXTERNAL",
        &[],
        "PASS",
        json!({
            "commitment": pilot_value["commitment"],
            "committed": pilot_value["commitment"] != "NO_PILOT_TEAM",
            "source": pilot_value
        }),
        implementation_sha,
        implementation_tree,
    );
    let budget_value = static_json(raw, "experiments/gate1-v2.9/budget.json")?;
    let budget = gate(
        "budget",
        "EXTERNAL",
        &[],
        "PASS",
        json!({"approved": budget_value["approved"], "source": budget_value}),
        implementation_sha,
        implementation_tree,
    );

    let scenario = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/scenario_independence.json",
    )?;
    let transport = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/outcome_transport.json",
    )?;
    let contracts = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/contract_satisfiability.json",
    )?;
    let regeneration = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/raw_regeneration_exercise.json",
    )?;
    let h2_projection = static_json(raw, "experiments/gate1-v2.9/prefreeze/h2_projection.json")?;
    let h2_noise = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/h2_noise_invariance.json",
    )?;
    let h2_sensitivity = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/h2_semantic_sensitivity.json",
    )?;
    let comparison_policy = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/comparison_policy.json",
    )?;
    let decision_branches = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/decision_branches.json",
    )?;
    let production_decision_state = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/decision_state_check.json",
    )?;
    let extraction_case = production_decision_state["cases"]
        .as_array()
        .and_then(|cases| {
            cases.iter().find(|case| {
                case["case"] == "representative-subgates-pass-aggregate-hypotheses-fail"
            })
        })
        .cloned()
        .unwrap_or(Value::Null);
    let structural_failure = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/structural_failure_regression.json",
    )?;
    let terminal_short_circuit = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/terminal_short_circuit.json",
    )?;
    let synthetic_git_chain = static_json(
        raw,
        "experiments/gate1-v2.9/prefreeze/synthetic_git_chain.json",
    )?;
    let status_lint = static_json(raw, "experiments/gate1-v2.9/prefreeze/status_lint.json")?;
    let workspace_failures = failures([
        (
            scenario["status"] == "PASS",
            "scenario independence prefreeze check failed",
        ),
        (
            transport["status"] == "PASS",
            "outcome transport prefreeze check failed",
        ),
        (
            contracts["status"] == "PASS",
            "50-contract satisfiability check failed",
        ),
        (
            regeneration["status"] == "PASS",
            "Raw-to-Gate regeneration exercise failed",
        ),
        (
            h2_projection["status"] == "PASS",
            "stable H2 projection check failed",
        ),
        (
            h2_noise["status"] == "PASS",
            "H2 noise-invariance check failed",
        ),
        (
            h2_sensitivity["status"] == "PASS",
            "H2 semantic-sensitivity check failed",
        ),
        (
            comparison_policy["status"] == "PASS",
            "H2 comparison-policy check failed",
        ),
        (
            decision_branches["status"] == "PASS",
            "decision state-machine branch check failed",
        ),
        (
            production_decision_state["status"] == "PASS",
            "production decision extraction regression failed",
        ),
        (
            extraction_case["matched"] == true
                && extraction_case["exercises_stable_failure_extraction"] == true
                && extraction_case["stable_core_failures"] == json!(["H1", "H2", "H3"])
                && extraction_case["actual"] == "STOP",
            "representative-subGate Stable Core Failure regression failed",
        ),
        (
            structural_failure["status"] == "PASS",
            "structural-failure propagation check failed",
        ),
        (
            terminal_short_circuit["status"] == "PASS",
            "terminal short-circuit check failed",
        ),
        (
            synthetic_git_chain["status"] == "PASS",
            "synthetic I/E/D/R/F chain check failed",
        ),
        (
            status_lint["status"] == "PASS",
            "status consistency lint failed",
        ),
    ]);
    let workspace = gate(
        "workspace",
        "APPARATUS",
        &workspace_failures,
        "PASS",
        json!({
            "scenario_independence": scenario["status"],
            "outcome_transport": transport["status"],
            "contract_satisfiability": contracts["status"],
            "raw_regeneration": regeneration["status"],
            "h2_projection": h2_projection["status"],
            "h2_noise_invariance": h2_noise["status"],
            "h2_semantic_sensitivity": h2_sensitivity["status"],
            "comparison_policy": comparison_policy["status"],
            "decision_branches": decision_branches["status"],
            "production_decision_state_check": production_decision_state["status"],
            "stable_failure_extraction_regression": extraction_case,
            "structural_failure_propagation": structural_failure["status"],
            "terminal_short_circuit": terminal_short_circuit["status"],
            "synthetic_git_chain": synthetic_git_chain["status"],
            "status_lint": status_lint["status"],
            "contract_count": 50
        }),
        implementation_sha,
        implementation_tree,
    );

    let hygiene_violations = hygiene_violations(raw)?;
    let artifact_hygiene = gate(
        "artifact_hygiene",
        "APPARATUS",
        &hygiene_violations,
        "PASS",
        json!({
            "violations": hygiene_violations,
            "forbidden_extensions": ["rlib", "rmeta", "o", "d"],
            "forbidden_names": [".fingerprint", "CACHEDIR.TAG", "Cargo.lock"],
            "build_output_location": "target/"
        }),
        implementation_sha,
        implementation_tree,
    );

    Ok(BTreeMap::from([
        ("history", history),
        ("governance", governance),
        ("environment", environment),
        ("process", process),
        ("validity", validity),
        ("h1_equivalence", h1_equivalence),
        ("h1_metrics", h1_metrics),
        ("h2_configuration", h2_configuration),
        ("h2_cleanup", h2_cleanup),
        ("h2_invariants", h2_invariants),
        ("h2_allocations", h2_allocations),
        ("h2_performance", h2_performance),
        ("h3_migration", h3_migration),
        ("h3_completion", h3_completion),
        ("h3_transaction", h3_transaction),
        ("comparison", comparison),
        ("replay", replay),
        ("pilot", pilot),
        ("budget", budget),
        ("workspace", workspace),
        ("artifact_hygiene", artifact_hygiene),
    ]))
}

fn comparison_gate(
    name: &str,
    comparison: &Value,
    implementation_sha: &str,
    implementation_tree: &str,
) -> Value {
    let component_status = |pointer: &str| {
        comparison
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("INVALID")
    };
    let semantic = if ["FAIL", "INVALID"].into_iter().any(|status| {
        component_status("/h1/status") == status
            || component_status("/h2/semantic/status") == status
            || component_status("/h3/status") == status
    }) {
        if [
            component_status("/h1/status"),
            component_status("/h2/semantic/status"),
            component_status("/h3/status"),
        ]
        .contains(&"INVALID")
        {
            "INVALID"
        } else {
            "FAIL"
        }
    } else {
        "PASS"
    };
    let allocation = component_status("/h2/allocation/status");
    let performance = component_status("/h2/performance/status");
    let expected = if semantic == "INVALID" || allocation == "INVALID" || performance == "INVALID" {
        "INVALID"
    } else if semantic == "FAIL" || allocation == "FAIL" {
        "FAIL"
    } else if performance == "INCONCLUSIVE" {
        "INCONCLUSIVE"
    } else if semantic == "PASS" && allocation == "PASS" && performance == "PASS" {
        "PASS"
    } else {
        "INVALID"
    };
    let observed = comparison["status"].as_str().unwrap_or("INVALID");
    let mut contract_failures = Vec::new();
    if observed != expected {
        contract_failures.push(format!(
            "recorded comparison outcome {observed} differs from derived {expected}"
        ));
    }
    if comparison["left"].as_str().is_none()
        || comparison["right"].as_str().is_none()
        || comparison["implementation_sha"].as_str().is_none()
        || comparison["implementation_tree"].as_str().is_none()
    {
        contract_failures.push("comparison provenance is incomplete".to_owned());
    }
    json!({
        "schema_version": 2,
        "experiment_version": "gate1-v2.9",
        "gate": name,
        "layer": "COMPARISON",
        "apparatus_status": if contract_failures.is_empty() {"PASS"} else {"STRUCTURAL_FAILURE"},
        "outcome": observed,
        "contract_status": if contract_failures.is_empty() {"PASS"} else {"FAIL"},
        "components": {
            "semantic": semantic,
            "allocation": allocation,
            "performance": performance
        },
        "failures": comparison["failures"],
        "contract_failures": contract_failures,
        "metrics": {"comparison": comparison},
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "generator": "nexa-gate1-v2-9-gates/raw-v2"
    })
}

fn h3_group_gate(
    name: &'static str,
    group: &str,
    expected_count: usize,
    runs: &[Value],
    implementation_sha: &str,
    implementation_tree: &str,
) -> Value {
    outcome_gate(
        name,
        runs,
        |value| {
            value["matrices"][group]
                .as_array()
                .is_some_and(|scenarios| {
                    scenarios.len() == expected_count
                        && scenarios.iter().all(|scenario| {
                            scenario["actual_outcome"] == "PASS"
                                && scenario["fresh_runtime_host"] == true
                                && scenario["fresh_realm_runtime"] == true
                        })
                })
        },
        json!({
            "group": group,
            "expected_count": expected_count,
            "scenario_counts": runs.iter().map(|value| value["matrices"][group].as_array().map_or(0, Vec::len)).collect::<Vec<_>>(),
            "spec_hash_counts": runs.iter().map(|value| value["matrices"][group].as_array().map_or(0, |items| items.iter().filter_map(|item| item["scenario_spec_hash"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>(),
            "executor_fingerprint_counts": runs.iter().map(|value| value["matrices"][group].as_array().map_or(0, |items| items.iter().filter_map(|item| item["executor_fingerprint"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>(),
            "semantic_signatures": signatures(runs)
        }),
        implementation_sha,
        implementation_tree,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn outcome_gate(
    name: &'static str,
    runs: &[Value],
    predicate: impl Fn(&Value) -> bool,
    metrics: Value,
    implementation_sha: &str,
    implementation_tree: &str,
) -> Value {
    outcome_gate_with_contract(
        name,
        runs,
        predicate,
        |_| true,
        metrics,
        implementation_sha,
        implementation_tree,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn outcome_gate_with_contract(
    name: &'static str,
    runs: &[Value],
    predicate: impl Fn(&Value) -> bool,
    direct_contract: impl Fn(&Value) -> bool,
    metrics: Value,
    implementation_sha: &str,
    implementation_tree: &str,
) -> Value {
    let derived = runs
        .iter()
        .map(|value| if predicate(value) { "PASS" } else { "FAIL" })
        .collect::<Vec<_>>();
    let recorded = runs
        .iter()
        .map(|value| {
            value
                .pointer(&format!("/component_outcomes/{name}"))
                .and_then(Value::as_str)
                .unwrap_or("INVALID")
        })
        .collect::<Vec<_>>();
    let direct = runs.iter().map(direct_contract).collect::<Vec<_>>();
    let mut failures = Vec::new();
    if runs.len() != RUNS.len() {
        failures.push(format!(
            "{name} expected {} independent runs, observed {}",
            RUNS.len(),
            runs.len()
        ));
    }
    if !derived
        .iter()
        .zip(recorded.iter())
        .all(|(derived, recorded)| derived == recorded)
    {
        failures.push(format!(
            "{name} per-run derived outcomes {derived:?} differ from recorded component outcomes {recorded:?}"
        ));
    }
    if direct.iter().any(|passed| !passed) {
        failures.push(format!(
            "{name} direct Raw contract assertions failed by run: {direct:?}"
        ));
    }
    gate(
        name,
        "OUTCOME",
        &failures,
        derived.first().copied().unwrap_or("INVALID"),
        json!({
            "derived_outcomes": derived,
            "recorded_outcomes": recorded,
            "direct_contract_assertions": direct,
            "measurements": metrics
        }),
        implementation_sha,
        implementation_tree,
    )
}

fn formal_decision_evidence(
    supervisors: &[Value],
    h1: &[Value],
    h2: &[Value],
    h3: &[Value],
    formal: &Value,
    replay: &Value,
) -> Result<Value, AnyError> {
    let per_run = json!({
        "H1": aggregate_raw_run_outcomes(h1, &["h1_equivalence", "h1_metrics"])?,
        "H2": aggregate_raw_run_outcomes(h2, &[
            "h2_configuration",
            "h2_cleanup",
            "h2_invariants",
            "h2_allocations",
            "h2_performance",
        ])?,
        "H3": aggregate_raw_run_outcomes(h3, &[
            "h3_migration",
            "h3_completion",
            "h3_transaction",
        ])?
    });
    let aggregate = json!({
        "H1": aggregate_json_outcomes(&per_run["H1"])?,
        "H2": aggregate_json_outcomes(&per_run["H2"])?,
        "H3": aggregate_json_outcomes(&per_run["H3"])?,
    });
    let signature_stable = json!({
        "H1": raw_signatures_stable(h1),
        "H2": raw_signatures_stable(h2),
        "H3": raw_signatures_stable(h3),
    });
    let formal_semantic = json!({
        "H1": formal.pointer("/h1/status").cloned().unwrap_or(Value::Null),
        "H2": formal.pointer("/h2/semantic/status").cloned().unwrap_or(Value::Null),
        "H3": formal.pointer("/h3/status").cloned().unwrap_or(Value::Null),
    });
    let replay_semantic = json!({
        "H1": replay.pointer("/h1/status").cloned().unwrap_or(Value::Null),
        "H2": replay.pointer("/h2/semantic/status").cloned().unwrap_or(Value::Null),
        "H3": replay.pointer("/h3/status").cloned().unwrap_or(Value::Null),
    });
    let h2_allocation = json!([
        formal
            .pointer("/h2/allocation/status")
            .cloned()
            .unwrap_or(Value::Null),
        replay
            .pointer("/h2/allocation/status")
            .cloned()
            .unwrap_or(Value::Null),
    ]);
    let h2_performance = json!([
        formal
            .pointer("/h2/performance/status")
            .cloned()
            .unwrap_or(Value::Null),
        replay
            .pointer("/h2/performance/status")
            .cloned()
            .unwrap_or(Value::Null),
    ]);
    let performance_allowed = h2_performance.as_array().is_some_and(|values| {
        values.len() == 2
            && values
                .iter()
                .all(|value| value == "PASS" || value == "INCONCLUSIVE")
    });
    let mut stable = Vec::new();
    for hypothesis in ["H1", "H2", "H3"] {
        let all_fail = per_run[hypothesis]
            .as_array()
            .is_some_and(|values| values.len() == 3 && values.iter().all(|value| value == "FAIL"));
        let comparisons_pass = formal_semantic[hypothesis] == "PASS"
            && replay_semantic[hypothesis] == "PASS"
            && (hypothesis != "H2"
                || (h2_allocation == json!(["PASS", "PASS"]) && performance_allowed));
        if all_fail && signature_stable[hypothesis] == true && comparisons_pass {
            stable.push(hypothesis);
        }
    }
    let apparatus_statuses = supervisors
        .iter()
        .map(|value| value["apparatus_status"].clone())
        .collect::<Vec<_>>();
    let apparatus_valid =
        apparatus_statuses.len() == 3 && apparatus_statuses.iter().all(|value| value == "PASS");
    let derived_product_decision = if !apparatus_valid {
        "INVALID"
    } else if stable.is_empty() {
        "UNVERIFIABLE_WITHIN_MVR"
    } else {
        "STOP"
    };
    Ok(json!({
        "source": "FORMAL_RAW_COMPONENT_OUTCOMES_AND_COMPARISONS",
        "decision_recomputed_from_raw": true,
        "apparatus_statuses": apparatus_statuses,
        "per_run_product_outcomes": per_run,
        "aggregate_product_outcomes": aggregate,
        "representative_subgate_outcomes": {
            "H1": raw_component_outcomes(h1, "h1_equivalence")?,
            "H2": raw_component_outcomes(h2, "h2_configuration")?,
            "H3": raw_component_outcomes(h3, "h3_migration")?,
        },
        "signature_stable": signature_stable,
        "formal_semantic_comparisons": formal_semantic,
        "replay_semantic_comparisons": replay_semantic,
        "h2_allocation_comparisons": h2_allocation,
        "h2_performance_comparisons": h2_performance,
        "stable_core_failures": stable,
        "decision_priority_branch": if derived_product_decision == "STOP" {
            "stable_core_failure"
        } else {
            "comparison_fail_or_inconclusive_without_stable_failure"
        },
        "stable_failure_dominates_inconclusive": derived_product_decision == "STOP"
            && (formal["status"] == "INCONCLUSIVE" || replay["status"] == "INCONCLUSIVE"),
        "stable_failure_priority_applied": derived_product_decision == "STOP"
            && !stable.is_empty(),
        "structured_pivot_approved": false,
        "derived_product_decision": derived_product_decision,
    }))
}

fn aggregate_raw_run_outcomes(runs: &[Value], names: &[&str]) -> Result<Vec<String>, AnyError> {
    runs.iter()
        .map(|run| {
            let outcomes = names
                .iter()
                .map(|name| {
                    run.pointer(&format!("/component_outcomes/{name}"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| format!("Raw component outcome {name} is missing"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(aggregate_outcome_strings(&outcomes).to_owned())
        })
        .collect()
}

fn raw_component_outcomes(runs: &[Value], name: &str) -> Result<Vec<String>, AnyError> {
    runs.iter()
        .map(|run| {
            run.pointer(&format!("/component_outcomes/{name}"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("Raw component outcome {name} is missing").into())
        })
        .collect()
}

fn aggregate_json_outcomes(outcomes: &Value) -> Result<&'static str, AnyError> {
    let values = outcomes
        .as_array()
        .ok_or("per-run outcome array is missing")?
        .iter()
        .map(|value| value.as_str().ok_or("per-run outcome is not text"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(aggregate_outcome_strings(&values))
}

fn aggregate_outcome_strings(outcomes: &[&str]) -> &'static str {
    if outcomes.contains(&"INVALID") {
        "INVALID"
    } else if outcomes.contains(&"FAIL") {
        "FAIL"
    } else if outcomes.contains(&"INCONCLUSIVE") {
        "INCONCLUSIVE"
    } else if outcomes.iter().all(|value| *value == "PASS") {
        "PASS"
    } else {
        "NOT_RUN"
    }
}

fn raw_signatures_stable(runs: &[Value]) -> bool {
    let signatures = runs
        .iter()
        .filter_map(|run| run["semantic_signature"].as_str())
        .collect::<Vec<_>>();
    signatures.len() == 3
        && signatures
            .first()
            .is_some_and(|first| signatures.iter().all(|value| value == first))
}

fn absolute_allocation_contract(value: &Value) -> bool {
    const ZERO_FIELDS: [&str; 5] = [
        "promotion",
        "fuel_resume",
        "explicit_resume",
        "host_resume",
        "trace_off",
    ];
    let aggregate_zero = value
        .pointer("/metrics/allocation_counts/value")
        .and_then(Value::as_object)
        .is_some_and(|counts| {
            ["promotion", "fuel_resume", "explicit_resume", "host_resume"]
                .iter()
                .all(|field| counts.get(*field).and_then(Value::as_u64) == Some(0))
                && counts.get("trace_off_completion").and_then(Value::as_u64) == Some(0)
        });
    let observer_zero = value
        .pointer("/allocation_observer/runs")
        .and_then(Value::as_array)
        .is_some_and(|runs| {
            runs.len() == 3
                && runs.iter().all(|run| {
                    ZERO_FIELDS
                        .iter()
                        .all(|field| run.get(*field).and_then(Value::as_u64) == Some(0))
                })
        });
    aggregate_zero
        && observer_zero
        && value
            .pointer("/allocation_observer/observer")
            .and_then(Value::as_str)
            == Some("global_allocator")
}

fn performance_contract(value: &Value) -> bool {
    value["performance_processes"]
        .as_array()
        .is_some_and(|processes| {
            processes.len() == 3
                && processes
                    .iter()
                    .all(|process| process["samples"].as_u64() == Some(1_000))
        })
        && value
            .pointer("/metrics/warmup_samples/value")
            .and_then(Value::as_u64)
            == Some(100)
        && value
            .pointer("/metrics/timed_samples/value")
            .and_then(Value::as_u64)
            == Some(1_000)
}

#[allow(clippy::needless_pass_by_value)]
fn gate(
    name: &str,
    layer: &str,
    failures: &[String],
    outcome: &str,
    metrics: Value,
    implementation_sha: &str,
    implementation_tree: &str,
) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.9",
        "gate": name,
        "layer": layer,
        "contract_status": if failures.is_empty() {"PASS"} else {"FAIL"},
        "outcome": outcome,
        "failures": failures,
        "metrics": metrics,
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "generator": "nexa-gate1-v2-9-gates/raw-v1"
    })
}

fn failures<const N: usize>(conditions: [(bool, &'static str); N]) -> Vec<String> {
    conditions
        .into_iter()
        .filter_map(|(passed, reason)| (!passed).then_some(reason.to_owned()))
        .collect()
}

fn signatures(values: &[Value]) -> Vec<Value> {
    values
        .iter()
        .map(|value| value["semantic_signature"].clone())
        .collect()
}

fn metric_value(value: &Value, name: &str) -> Option<u64> {
    value
        .pointer(&format!("/metrics/{name}/value"))
        .and_then(Value::as_u64)
}

fn run_children(raw: &Path, child: &str) -> Result<Vec<Value>, AnyError> {
    RUNS.map(|run| raw_json(raw, &format!("runs/{run}/{child}/result.json")))
        .into_iter()
        .collect()
}

fn raw_json(raw: &Path, relative: &str) -> Result<Value, AnyError> {
    read_json(raw.join(relative))
}

fn static_json(raw: &Path, source_path: &str) -> Result<Value, AnyError> {
    raw_json(raw, &format!("static/{source_path}"))
}

fn text_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, AnyError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing text at {pointer}").into())
}

fn copy_evidence_tree(source: &Path, destination: &Path) -> Result<(), AnyError> {
    if !source.is_dir() {
        return Err(format!("formal Evidence source {} is missing", source.display()).into());
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        if path.is_dir() {
            copy_evidence_tree(&path, &target)?;
        } else {
            copy_evidence_file(&path, &target)?;
        }
    }
    Ok(())
}

fn copy_evidence_file(source: &Path, destination: &Path) -> Result<(), AnyError> {
    if !source.is_file() {
        return Err(format!("Evidence source {} is missing", source.display()).into());
    }
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let allowed = matches!(extension, "json" | "ndjson" | "md" | "txt" | "log");
    if !allowed {
        return Err(format!(
            "Evidence source {} has forbidden file type",
            source.display()
        )
        .into());
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

fn hygiene_violations(root: &Path) -> Result<Vec<String>, AnyError> {
    let mut files = Vec::new();
    walk(root, &mut files)?;
    let forbidden_names = [".fingerprint", "CACHEDIR.TAG", "Cargo.lock"];
    let forbidden_directories = ["build", "debug", "release", "incremental"];
    let forbidden_extensions = ["rlib", "rmeta", "o", "d"];
    let mut violations = Vec::new();
    for path in files {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let components = path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .collect::<Vec<_>>();
        if forbidden_names.contains(&name)
            || forbidden_extensions.contains(&extension)
            || components
                .iter()
                .any(|component| forbidden_directories.contains(component))
        {
            violations.push(path.display().to_string());
        }
    }
    Ok(violations)
}

fn walk(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), AnyError> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

pub fn gate_hashes(directory: &Path) -> Result<BTreeMap<String, String>, AnyError> {
    GATE_NAMES
        .iter()
        .map(|name| {
            Ok((
                (*name).to_owned(),
                hash_file(directory.join(format!("{name}.json")))?,
            ))
        })
        .collect()
}
