#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_5::{AnyError, hash_file, read_json, stable_value_hash, write_json};
use serde_json::{Value, json};

pub const RAW_ROOT: &str = "reports/raw/gate1_v2_5";
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

const STATIC_INPUTS: [&str; 38] = [
    "experiments/gate1-v2.5/qualification/environment_qualification.json",
    "experiments/gate1-v2.5/qualification/environment_qualification_hashes.json",
    "experiments/gate1-v2.5/qualification/root-cause.json",
    "experiments/gate1-v2.5/qualification/formal-handshake/process_attestation.json",
    "experiments/gate1-v2.5/qualification/formal-handshake/parent_verification.json",
    "experiments/gate1-v2.5/qualification/formal-handshake/probe.json",
    "experiments/gate1-v2.5/prefreeze/prefreeze_closure.json",
    "experiments/gate1-v2.5/prefreeze/history_check.json",
    "experiments/gate1-v2.5/prefreeze/governance_negative_tests.json",
    "experiments/gate1-v2.5/prefreeze/scenario_independence.json",
    "experiments/gate1-v2.5/prefreeze/outcome_transport.json",
    "experiments/gate1-v2.5/prefreeze/h1_transformer_equivalence.json",
    "experiments/gate1-v2.5/prefreeze/h2_dimension_effectiveness.json",
    "experiments/gate1-v2.5/prefreeze/h2_cleanup_independence.json",
    "experiments/gate1-v2.5/prefreeze/h2_projection.json",
    "experiments/gate1-v2.5/prefreeze/h2_noise_invariance.json",
    "experiments/gate1-v2.5/prefreeze/h2_semantic_sensitivity.json",
    "experiments/gate1-v2.5/prefreeze/comparison_policy.json",
    "experiments/gate1-v2.5/prefreeze/h3_execution_independence.json",
    "experiments/gate1-v2.5/prefreeze/raw_regeneration_exercise.json",
    "experiments/gate1-v2.5/prefreeze/contract_satisfiability.json",
    "experiments/gate1-v2.5/prefreeze/decision_branches.json",
    "experiments/gate1-v2.5/prefreeze/status_lint.json",
    "experiments/gate1-v2.5/prefreeze/structural_failure_regression.json",
    "experiments/gate1-v2.5/prefreeze/synthetic_git_chain.json",
    "experiments/gate1-v2.5/prefreeze/terminal_short_circuit.json",
    "experiments/gate1-v2.5/authorization.json",
    "experiments/gate1-v2.5/threshold_equivalence.json",
    "experiments/gate1-v2.5/h2_semantic_projection.json",
    "experiments/gate1-v2.5/h2_performance_policy.json",
    "experiments/gate1-v2.5/decision_state_machine.json",
    "experiments/gate1-v2.5/pilot.json",
    "experiments/gate1-v2.5/budget.json",
    "reports/history/gate1/current_status.json",
    "reports/history/gate1/versions/gate1-v2.3.json",
    "reports/history/gate1/versions/gate1-v2.4.json",
    "reports/history/gate1/v2_4/terminal.json",
    "reports/history/gate1/v2_4/raw_hash_manifest.json",
];

pub fn package_all() -> Result<(), AnyError> {
    let raw = Path::new(RAW_ROOT);
    if raw.exists() {
        return Err(
            "reports/raw/gate1_v2_5 already exists; Evidence packaging is immutable".into(),
        );
    }
    for run in RUNS {
        copy_evidence_tree(
            &Path::new("target/gate1-v2.5").join(run),
            &raw.join("runs").join(run),
        )?;
    }
    copy_evidence_tree(
        Path::new("target/gate1-v2.5/comparisons"),
        &raw.join("comparisons"),
    )?;
    for source in STATIC_INPUTS {
        let destination = raw.join("static").join(source);
        copy_evidence_file(Path::new(source), &destination)?;
    }
    for source in [
        "experiments/gate1-v2.5/h1_mutations.json",
        "experiments/gate1-v2.5/h2_matrix.json",
        "experiments/gate1-v2.5/h2_cleanup_matrix.json",
        "experiments/gate1-v2.5/h3_scenarios.json",
        "experiments/gate1-v2.5/scenario_schema.json",
    ] {
        copy_evidence_file(Path::new(source), &raw.join("static").join(source))?;
    }
    generate_from_raw(raw, &raw.join("gates"))
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
        "experiment_version": "gate1-v2.5",
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
    let regenerated = Path::new("target/gate1-v2.5-gate-rebuild");
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
            "experiment_version": "gate1-v2.5",
            "status": "PASS",
            "compared": compared
        }))?
    );
    Ok(())
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

    let history_check = static_json(raw, "experiments/gate1-v2.5/prefreeze/history_check.json")?;
    let v2_3 = static_json(raw, "reports/history/gate1/versions/gate1-v2.3.json")?;
    let v2_4 = static_json(raw, "reports/history/gate1/versions/gate1-v2.4.json")?;
    let v2_4_terminal = static_json(raw, "reports/history/gate1/v2_4/terminal.json")?;
    let v2_4_manifest = static_json(raw, "reports/history/gate1/v2_4/raw_hash_manifest.json")?;
    let current = static_json(raw, "reports/history/gate1/current_status.json")?;
    let history_failures = failures([
        (
            history_check["status"] == "PASS",
            "prefreeze history check did not pass",
        ),
        (
            v2_3["status"] == "SEMANTICALLY_INSUFFICIENT" && v2_3["decision_usable"] == false,
            "v2.3 was not sealed as semantically insufficient",
        ),
        (
            v2_4["status"] == "STRUCTURAL_CLOSURE_FAILED" && v2_4["decision_usable"] == false,
            "v2.4 was not sealed as structurally incomplete",
        ),
        (
            v2_4_terminal["status"] == "STRUCTURAL_CLOSURE_FAILED"
                && v2_4_terminal["decision_usable"] == false
                && v2_4_terminal["receipt_created"] == false,
            "v2.4 terminal state is not an unusable structural failure",
        ),
        (
            v2_4_manifest["status"] == "STRUCTURAL_CLOSURE_FAILED"
                && v2_4_manifest["formal_evidence_usable"] == false
                && v2_4_manifest["artifact_count"] == 311,
            "v2.4 Raw archive manifest is incomplete or decision-usable",
        ),
        (
            current["current_experiment"] == "gate1-v2.5",
            "v2.5 is not the unique current experiment",
        ),
    ]);
    let history = gate(
        "history",
        "APPARATUS",
        &history_failures,
        "PASS",
        json!({
            "v2_3_semantically_insufficient": v2_3["status"] == "SEMANTICALLY_INSUFFICIENT",
            "v2_3_decision_usable": v2_3["decision_usable"],
            "v2_4_status": v2_4["status"],
            "v2_4_decision_usable": v2_4["decision_usable"],
            "v2_4_terminal_status": v2_4_terminal["status"],
            "v2_4_archive_artifact_count": v2_4_manifest["artifact_count"],
            "current_experiment": current["current_experiment"],
            "history_prefreeze_status": history_check["status"]
        }),
        implementation_sha,
        implementation_tree,
    );

    let closure = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/prefreeze_closure.json",
    )?;
    let negative = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/governance_negative_tests.json",
    )?;
    let authorization = static_json(raw, "experiments/gate1-v2.5/authorization.json")?;
    let thresholds = static_json(raw, "experiments/gate1-v2.5/threshold_equivalence.json")?;
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
            "v2.5 authorization is absent",
        ),
        (
            thresholds["outcome_thresholds_weakened"] == false,
            "outcome thresholds were weakened",
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
            "thresholds_weakened": thresholds["outcome_thresholds_weakened"]
        }),
        implementation_sha,
        implementation_tree,
    );

    let qualification = static_json(
        raw,
        "experiments/gate1-v2.5/qualification/environment_qualification.json",
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

    let h2_configuration = outcome_gate(
        "h2_configuration",
        &h2,
        |value| {
            value["scenarios"].as_array().is_some_and(|scenarios| {
                scenarios.len() == 32
                    && scenarios.iter().all(|scenario| {
                        scenario["expected_calls"] == scenario["observed_calls"]
                            && scenario["expected_promotions"] == scenario["observed_promotions"]
                    })
            })
        },
        json!({
            "configuration_counts": h2.iter().map(|value| value["scenarios"].as_array().map_or(0, Vec::len)).collect::<Vec<_>>(),
            "execution_fingerprint_counts": h2.iter().map(|value| value["scenarios"].as_array().map_or(0, |items| items.iter().filter_map(|item| item["execution_fingerprint"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>(),
            "semantic_signatures": signatures(&h2)
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_cleanup = outcome_gate(
        "h2_cleanup",
        &h2,
        |value| {
            value["cleanup"].as_array().is_some_and(|cleanup| {
                cleanup.len() == 12
                    && cleanup
                        .iter()
                        .filter_map(|item| item["trigger_fingerprint"].as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        == 12
            })
        },
        json!({
            "cleanup_counts": h2.iter().map(|value| value["cleanup"].as_array().map_or(0, Vec::len)).collect::<Vec<_>>(),
            "trigger_fingerprint_counts": h2.iter().map(|value| value["cleanup"].as_array().map_or(0, |items| items.iter().filter_map(|item| item["trigger_fingerprint"].as_str()).collect::<BTreeSet<_>>().len())).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_invariants = outcome_gate(
        "h2_invariants",
        &h2,
        |value| {
            value["scenarios"].as_array().is_some_and(|scenarios| {
                scenarios
                    .iter()
                    .all(|scenario| scenario["violations"].as_array().is_some_and(Vec::is_empty))
            })
        },
        json!({
            "violation_counts": h2.iter().map(|value| value["scenarios"].as_array().map_or(usize::MAX, |items| items.iter().map(|item| item["violations"].as_array().map_or(usize::MAX, Vec::len)).sum())).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_allocations = outcome_gate(
        "h2_allocations",
        &h2,
        |value| {
            value["allocator_failures"]
                .as_array()
                .is_some_and(Vec::is_empty)
        },
        json!({
            "allocator_failures": h2.iter().map(|value| value["allocator_failures"].clone()).collect::<Vec<_>>(),
            "allocation_counts": h2.iter().map(|value| value["allocation_counts"].clone()).collect::<Vec<_>>()
        }),
        implementation_sha,
        implementation_tree,
    );
    let h2_performance = outcome_gate(
        "h2_performance",
        &h2,
        |value| {
            value["timing_failures"]
                .as_array()
                .is_some_and(Vec::is_empty)
        },
        json!({
            "timing_failures": h2.iter().map(|value| value["timing_failures"].clone()).collect::<Vec<_>>(),
            "benchmark_process_counts": h2.iter().map(|value| value["benchmark_processes"].as_array().map_or(0, Vec::len)).collect::<Vec<_>>()
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

    let formal_comparison = raw_json(raw, "comparisons/formal-run-1__formal-run-2.json")?;
    let replay_comparison = raw_json(raw, "comparisons/formal-run-1__replay.json")?;
    let comparison_failures = failures([(
        formal_comparison["status"] == "PASS",
        "formal runs have different semantic signatures or outcomes",
    )]);
    let comparison = gate(
        "comparison",
        "OUTCOME",
        &comparison_failures,
        formal_comparison["status"].as_str().unwrap_or("INVALID"),
        json!({"comparison": formal_comparison}),
        implementation_sha,
        implementation_tree,
    );
    let replay_failures = failures([(
        replay_comparison["status"] == "PASS",
        "replay differs from formal run 1",
    )]);
    let replay = gate(
        "replay",
        "OUTCOME",
        &replay_failures,
        replay_comparison["status"].as_str().unwrap_or("INVALID"),
        json!({"comparison": replay_comparison}),
        implementation_sha,
        implementation_tree,
    );

    let pilot_value = static_json(raw, "experiments/gate1-v2.5/pilot.json")?;
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
    let budget_value = static_json(raw, "experiments/gate1-v2.5/budget.json")?;
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
        "experiments/gate1-v2.5/prefreeze/scenario_independence.json",
    )?;
    let transport = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/outcome_transport.json",
    )?;
    let contracts = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/contract_satisfiability.json",
    )?;
    let regeneration = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/raw_regeneration_exercise.json",
    )?;
    let h2_projection = static_json(raw, "experiments/gate1-v2.5/prefreeze/h2_projection.json")?;
    let h2_noise = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/h2_noise_invariance.json",
    )?;
    let h2_sensitivity = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/h2_semantic_sensitivity.json",
    )?;
    let comparison_policy = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/comparison_policy.json",
    )?;
    let decision_branches = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/decision_branches.json",
    )?;
    let structural_failure = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/structural_failure_regression.json",
    )?;
    let terminal_short_circuit = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/terminal_short_circuit.json",
    )?;
    let synthetic_git_chain = static_json(
        raw,
        "experiments/gate1-v2.5/prefreeze/synthetic_git_chain.json",
    )?;
    let status_lint = static_json(raw, "experiments/gate1-v2.5/prefreeze/status_lint.json")?;
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
            "48-contract satisfiability check failed",
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
            structural_failure["status"] == "PASS",
            "structural-failure propagation check failed",
        ),
        (
            terminal_short_circuit["status"] == "PASS",
            "terminal short-circuit check failed",
        ),
        (
            synthetic_git_chain["status"] == "PASS",
            "synthetic I/E/D/F chain check failed",
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
            "structural_failure_propagation": structural_failure["status"],
            "terminal_short_circuit": terminal_short_circuit["status"],
            "synthetic_git_chain": synthetic_git_chain["status"],
            "status_lint": status_lint["status"],
            "contract_count": 48
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
    let derived = runs
        .iter()
        .map(|value| if predicate(value) { "PASS" } else { "FAIL" })
        .collect::<Vec<_>>();
    let recorded = runs
        .iter()
        .map(|value| value["outcome"].as_str().unwrap_or("INVALID"))
        .collect::<Vec<_>>();
    let failures = if derived.iter().all(|outcome| outcome == &derived[0])
        && recorded.iter().all(|outcome| outcome == &recorded[0])
    {
        Vec::new()
    } else {
        vec![format!(
            "{name} derived outcomes {derived:?} differ from recorded outcomes {recorded:?}"
        )]
    };
    gate(
        name,
        "OUTCOME",
        &failures,
        derived.first().copied().unwrap_or("INVALID"),
        json!({
            "derived_outcomes": derived,
            "recorded_outcomes": recorded,
            "measurements": metrics
        }),
        implementation_sha,
        implementation_tree,
    )
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
        "experiment_version": "gate1-v2.5",
        "gate": name,
        "layer": layer,
        "contract_status": if failures.is_empty() {"PASS"} else {"FAIL"},
        "outcome": outcome,
        "failures": failures,
        "metrics": metrics,
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "generator": "nexa-gate1-v2-5-gates/raw-v1"
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
