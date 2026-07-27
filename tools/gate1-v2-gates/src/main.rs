#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use nexa_gate1_v2_3::{
    AnyError, MeasurementKind, ObservedMetric, git, git_clean_failures, hash_file, read_json,
    repository_root, write_json,
};
use serde_json::{Value, json};

const RUNS: [&str; 3] = ["formal-run-1", "formal-run-2", "replay"];
const RAW_ROOT: &str = "reports/raw/gate1_v2_3";

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "all" => package_all(),
        _ => Err("usage: nexa-gate1-v2-3-gates all".into()),
    }
}

fn package_all() -> Result<(), AnyError> {
    let raw = Path::new(RAW_ROOT);
    if raw.exists() {
        return Err(
            "reports/raw/gate1_v2_3 already exists; evidence packaging is immutable".into(),
        );
    }
    for run in RUNS {
        copy_evidence_tree(
            &Path::new("target/gate1-v2.3").join(run),
            &raw.join("runs").join(run),
        )?;
    }
    copy_evidence_tree(
        Path::new("target/gate1-v2.3/comparisons"),
        &raw.join("comparisons"),
    )?;
    let implementation_sha = git(&["rev-parse", "HEAD"])?;
    let implementation_tree = git(&["rev-parse", "HEAD^{tree}"])?;
    let runs = RUNS
        .iter()
        .map(|run| {
            Ok((
                *run,
                read_json(raw.join("runs").join(run).join("worker/result.json"))?,
            ))
        })
        .collect::<Result<Vec<_>, AnyError>>()?;
    let supervisors = RUNS
        .iter()
        .map(|run| {
            Ok((
                *run,
                read_json(raw.join("runs").join(run).join("result.json"))?,
            ))
        })
        .collect::<Result<Vec<_>, AnyError>>()?;

    let history = history_gate(&implementation_sha, &implementation_tree)?;
    let governance = governance_gate(&history, &implementation_sha, &implementation_tree)?;
    let validity = validity_gate(&supervisors, &implementation_sha, &implementation_tree)?;
    let process = process_gate(
        &runs,
        &supervisors,
        &implementation_sha,
        &implementation_tree,
    )?;
    let environment = environment_qualification_gate(&implementation_sha, &implementation_tree)?;
    let h1 = hypothesis_gate("h1", &runs, &implementation_sha, &implementation_tree);
    let h2_semantic = h2_semantic_gate(&runs, &implementation_sha, &implementation_tree);
    let h2_allocations = h2_allocation_gate(&runs, &implementation_sha, &implementation_tree);
    let h2_performance = h2_performance_gate(&runs, &implementation_sha, &implementation_tree);
    let h3_migration = h3_gate(
        "migration",
        11,
        &runs,
        &implementation_sha,
        &implementation_tree,
    );
    let h3_completion = h3_gate(
        "completion",
        10,
        &runs,
        &implementation_sha,
        &implementation_tree,
    );
    let h3_transaction = h3_gate(
        "transaction",
        9,
        &runs,
        &implementation_sha,
        &implementation_tree,
    );
    let comparison = comparison_gate(&implementation_sha, &implementation_tree)?;
    let replay = replay_gate(&implementation_sha, &implementation_tree)?;
    let pilot = external_gate(
        "experiments/gate1-v2.3/pilot.json",
        "commitment",
        &implementation_sha,
        &implementation_tree,
    )?;
    let budget = external_gate(
        "experiments/gate1-v2.3/budget.json",
        "approved",
        &implementation_sha,
        &implementation_tree,
    )?;
    let workspace = workspace_gate(&implementation_sha, &implementation_tree)?;
    let static_evidence = static_evidence_gate(&implementation_sha, &implementation_tree)?;
    let _decision = decision_gate(
        &validity,
        &h1,
        [&h2_semantic, &h2_allocations, &h2_performance],
        [&h3_migration, &h3_completion, &h3_transaction],
        &comparison,
        &replay,
        &pilot,
        &budget,
        &implementation_sha,
        &implementation_tree,
    );
    let gates = [
        ("governance.json", governance),
        ("history.json", history),
        ("environment.json", environment),
        ("validity.json", validity),
        ("process_provenance.json", process),
        ("h1.json", h1),
        ("h2_semantic.json", h2_semantic),
        ("h2_allocations.json", h2_allocations),
        ("h2_performance.json", h2_performance),
        ("h3_migration.json", h3_migration),
        ("h3_completion.json", h3_completion),
        ("h3_transaction.json", h3_transaction),
        ("comparison.json", comparison),
        ("replay.json", replay),
        ("pilot.json", pilot),
        ("budget.json", budget),
        ("workspace.json", workspace),
    ];
    let mut apparatus_failures = Vec::new();
    for (name, gate) in &gates {
        write_json(&raw.join("gates").join(name), &gate)?;
        if matches!(
            *name,
            "governance.json"
                | "history.json"
                | "environment.json"
                | "process_provenance.json"
                | "workspace.json"
        ) && gate["status"] != "PASS"
        {
            apparatus_failures.push(format!("{name}: {}", gate["failures"]));
        }
    }
    if static_evidence["status"] != "PASS" {
        apparatus_failures.push(format!("static evidence: {}", static_evidence["failures"]));
    }
    std::fs::copy(
        "experiments/gate1-v2.3/pilot.json",
        "reports/gate1_v2_3_pilot.json",
    )?;
    std::fs::copy(
        "experiments/gate1-v2.3/budget.json",
        "reports/gate1_v2_3_budget.json",
    )?;
    if !apparatus_failures.is_empty() {
        return Err(format!("Gate 1 v2.3 apparatus gates failed: {apparatus_failures:?}").into());
    }
    println!("packaged 17 Gate 1 v2.3 gate artifacts");
    Ok(())
}

fn governance_gate(history_gate: &Value, sha: &str, tree: &str) -> Result<Value, AnyError> {
    let v1_invalidation = read_json("reports/contracts/gate1_v1_invalidation.json")?;
    let v2_invalidation = read_json("reports/contracts/gate1_v2_invalidation.json")?;
    let v2_1_invalidation = read_json("reports/contracts/gate1_v2_1_invalidation.json")?;
    let authorization = read_json("experiments/gate1-v2.3/authorization.json")?;
    let thresholds = read_json("experiments/gate1-v2.3/threshold_equivalence.json")?;
    let contracts = read_json("reports/contracts/gate1_v2_3_contracts.json")?;
    let manifest = read_json("experiments/gate1-v2.3/manifest.json")?;
    let closure = read_json("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?;
    let history = git(&["rev-list", "HEAD"])?;
    let origin_main = git(&["rev-parse", "origin/main"])?;
    let origin_main_base = git(&["merge-base", "origin/main", "HEAD"])? == origin_main;
    let invalid_history_absent = !history.lines().any(|commit| {
        matches!(
            commit,
            "b6d49c0b4f7dd283dc0a04e6f1c1950e3c40bb4d"
                | "b63e542c0bd5564704e0c2fda0c551376f60623f"
                | "8e2296da6f9ca85a51fd164eb9f3f0c89849a499"
                | "d1b582dd9544b2f72a1260cbb023e04a4dbad5ff"
        )
    });
    let mut failures = Vec::new();
    if v1_invalidation["status"] != "INVALID_APPARATUS"
        || v1_invalidation["decision_usable"] != false
    {
        failures.push("Gate 1 v1 was not invalidated for decisions".to_owned());
    }
    if v2_invalidation["status"] != "INVALID_APPARATUS"
        || v2_invalidation["decision_usable"] != false
    {
        failures.push("Gate 1 v2 was not invalidated for decisions".to_owned());
    }
    if v2_1_invalidation["status"] != "INVALID"
        || v2_1_invalidation["decision_usable"] != false
        || v2_1_invalidation["receipt_created"] != false
    {
        failures.push("Gate 1 v2.1 invalid history is incomplete".to_owned());
    }
    if authorization["status"] != "AUTHORIZED" {
        failures.push("Gate 1 v2.3 authorization is missing".to_owned());
    }
    if thresholds["outcome_thresholds_weakened"] != false
        || thresholds["removed_hard_conditions"]
            .as_array()
            .is_none_or(|items| !items.is_empty())
        || thresholds["changed_outcome_rules"]
            .as_array()
            .is_none_or(|items| !items.is_empty())
    {
        failures.push("Gate 1 v2.3 thresholds were weakened".to_owned());
    }
    if contracts["contracts"].as_array().map_or(0, Vec::len) != 36 {
        failures.push("contract manifest does not contain 36 contracts".to_owned());
    }
    if !origin_main_base || !invalid_history_absent {
        failures.push("v2.2 branch ancestry is not isolated from invalid evidence".to_owned());
    }
    if history_gate["status"] != "PASS" {
        failures.push("supersession history graph failed".to_owned());
    }
    if closure["status"] != "PASS" {
        failures.push("prefreeze closure is not PASS".to_owned());
    }
    if manifest["state"] != "FROZEN"
        || manifest["supersession_graph_hash"]
            != hash_file("reports/history/gate1/supersession_graph.json")?
        || manifest["prefreeze_closure_hash"]
            != hash_file("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?
    {
        failures.push("frozen graph or prefreeze binding is invalid".to_owned());
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "v1_decision_usable": observed(false, MeasurementKind::ExternalDecision, "reports/contracts/gate1_v1_invalidation.json", "/decision_usable", 1, "packaging"),
            "v2_decision_usable": observed(false, MeasurementKind::ExternalDecision, "reports/contracts/gate1_v2_invalidation.json", "/decision_usable", 1, "packaging"),
            "v2_1_decision_usable": observed(false, MeasurementKind::ExternalDecision, "reports/contracts/gate1_v2_1_invalidation.json", "/decision_usable", 1, "packaging"),
            "unique_authorization": observed(authorization["status"] == "AUTHORIZED", MeasurementKind::ExternalDecision, "experiments/gate1-v2.3/authorization.json", "/status", 1, "packaging"),
            "thresholds_equivalent": observed(thresholds["outcome_thresholds_weakened"] == false && thresholds["changed_outcome_rules"].as_array().is_some_and(Vec::is_empty), MeasurementKind::FileHash, "experiments/gate1-v2.3/threshold_equivalence.json", "/", 1, "packaging"),
            "contract_count": observed(contracts["contracts"].as_array().map_or(0, Vec::len), MeasurementKind::DerivedCalculation, "reports/contracts/gate1_v2_3_contracts.json", "/contracts", 36, "packaging")
            ,"origin_main_base": observed(origin_main_base, MeasurementKind::ProcessResult, "git", "/merge-base", 1, "packaging")
            ,"invalid_evidence_absent_from_ancestry": observed(invalid_history_absent, MeasurementKind::ProcessResult, "git", "/rev-list", 1, "packaging")
            ,"negative_matrix_passed": observed(closure["checks"]["governance_negative_tests"] == "PASS", MeasurementKind::DerivedCalculation, "experiments/gate1-v2.3/prefreeze/prefreeze_closure.json", "/checks/governance_negative_tests", 10, "packaging")
            ,"frozen_inputs_valid": observed(manifest["state"] == "FROZEN", MeasurementKind::FileHash, "experiments/gate1-v2.3/manifest.json", "/state", 1, "packaging")
        }),
    ))
}

fn history_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let graph = read_json("reports/history/gate1/supersession_graph.json")?;
    let index = read_json("reports/history/gate1/index.json")?;
    let nodes = graph["nodes"]
        .as_array()
        .ok_or("history graph nodes are missing")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let current = graph["current"]
        .as_str()
        .ok_or("history graph current is missing")?;
    let edges = graph["edges"]
        .as_array()
        .ok_or("history graph edges are missing")?
        .iter()
        .filter_map(|edge| {
            Some([
                edge.get(0)?.as_str()?.to_owned(),
                edge.get(1)?.as_str()?.to_owned(),
            ])
        })
        .collect::<Vec<_>>();
    let node_set = nodes.iter().cloned().collect::<BTreeSet<_>>();
    let edge_set = edges.iter().cloned().collect::<BTreeSet<_>>();
    let invalid_edges = edges
        .iter()
        .filter(|edge| {
            edge[0] == edge[1] || !node_set.contains(&edge[0]) || !node_set.contains(&edge[1])
        })
        .cloned()
        .collect::<Vec<_>>();
    let cycle = graph_cycle(&node_set, &edges);
    let unreachable = nodes
        .iter()
        .filter(|node| node.as_str() != current && !graph_reaches(node, current, &edges))
        .cloned()
        .collect::<Vec<_>>();
    let current_has_outgoing = edges.iter().any(|edge| edge[0] == current);
    let mut usable_historical = Vec::new();
    let mut historical_immutable = true;
    for path in index["versions"].as_array().into_iter().flatten() {
        let node = read_json(path.as_str().ok_or("history version path is invalid")?)?;
        if node["version"] != current && node["decision_usable"] == true {
            usable_historical.push(node["version"].clone());
        }
        if node["version"] != current && node["historical_record_immutable"] != true {
            historical_immutable = false;
        }
    }
    let v2_2 = read_json("reports/history/gate1/v2_2/terminal.json")?;
    let v2_2_sealed = v2_2["status"] == "NOT_TRUSTWORTHY"
        && v2_2["decision_usable"] == false
        && v2_2["receipt_created"] == false
        && v2_2["retry_count"] == 0;
    let all_reach = unreachable.is_empty();
    let mut failures = Vec::new();
    if node_set.len() != 5
        || node_set.len() != nodes.len()
        || edge_set.len() != edges.len()
        || current != "gate1-v2.3"
        || current_has_outgoing
        || !invalid_edges.is_empty()
        || !cycle.is_empty()
        || !all_reach
        || !usable_historical.is_empty()
        || !historical_immutable
        || !v2_2_sealed
    {
        failures.push("Gate 1 supersession graph or sealed history is invalid".to_owned());
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "v2_2_sealed": observed(v2_2_sealed, MeasurementKind::FileHash, "reports/history/gate1/v2_2/terminal.json", "/", 1, "packaging"),
            "version_count": observed(node_set.len(), MeasurementKind::DerivedCalculation, "reports/history/gate1/index.json", "/versions", 5, "packaging"),
            "all_historical_nodes_reach_current": observed(all_reach, MeasurementKind::DerivedCalculation, "reports/history/gate1/supersession_graph.json", "/edges", edges.len() as u64, "packaging"),
            "cycle": observed(cycle, MeasurementKind::DerivedCalculation, "reports/history/gate1/supersession_graph.json", "/edges", edges.len() as u64, "packaging"),
            "unreachable_nodes": observed(unreachable, MeasurementKind::DerivedCalculation, "reports/history/gate1/supersession_graph.json", "/edges", edges.len() as u64, "packaging"),
            "historical_records_immutable": observed(historical_immutable, MeasurementKind::FileHash, "reports/history/gate1/versions", "/", 4, "packaging")
        }),
    ))
}

fn environment_qualification_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let qualification =
        read_json("experiments/gate1-v2.3/qualification/environment_qualification.json")?;
    let authorization = read_json("experiments/gate1-v2.3/authorization.json")?;
    let failures = if qualification["status"] == "QUALIFIED"
        && qualification["failures"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && authorization["environment_qualification"]["hash"]
            == nexa_gate1_v2_3::hash_file(
                "experiments/gate1-v2.3/qualification/environment_qualification.json",
            )? {
        Vec::new()
    } else {
        vec!["environment qualification is absent, failed, or unbound".to_owned()]
    };
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "qualification_status": observed(qualification["status"].clone(), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/status", 1, "packaging"),
            "stress_failure_count": observed(qualification["stress_test"]["failures"].as_array().map_or(0, Vec::len), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/stress_test/failures", 1, "packaging"),
            "provenance_protocol": observed(qualification["provenance_protocol"].clone(), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/provenance_protocol", 1, "packaging"),
            "v2_1_failure_reproduced": observed(qualification["root_cause"]["v2_1_failure_reproduced"].clone(), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/root_cause/v2_1_failure_reproduced", 1, "packaging"),
            "failing_atomic_check": observed(qualification["root_cause"]["failing_atomic_check"].clone(), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/root_cause/failing_atomic_check", 1, "packaging"),
            "spawn_itself_supported": observed(qualification["root_cause"]["spawn_itself_supported"].clone(), MeasurementKind::ProcessResult, "experiments/gate1-v2.3/qualification/environment_qualification.json", "/root_cause/spawn_itself_supported", 1, "packaging")
        }),
    ))
}

#[allow(clippy::too_many_arguments)]
fn decision_gate(
    validity: &Value,
    h1: &Value,
    h2: [&Value; 3],
    h3: [&Value; 3],
    comparison: &Value,
    replay: &Value,
    pilot: &Value,
    budget: &Value,
    sha: &str,
    tree: &str,
) -> Value {
    let valid = validity["status"] == "PASS";
    let hypotheses_pass = h1["status"] == "PASS"
        && h2.iter().all(|gate| gate["status"] == "PASS")
        && h3.iter().all(|gate| gate["status"] == "PASS");
    let comparison_pass = comparison["status"] == "PASS";
    let replay_pass = replay["status"] == "PASS";
    let pilot_committed = pilot["metrics"]["commitment"]["value"] == "COMMITTED";
    let budget_approved = budget["metrics"]["approved"]["value"] == true;
    let decision = if !valid || !comparison_pass || !replay_pass {
        "INVALID"
    } else if !hypotheses_pass {
        "STOP"
    } else if !pilot_committed {
        "HOLD"
    } else if budget_approved {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    };
    gate(
        Vec::new(),
        sha,
        tree,
        json!({
            "computed_decision": observed(decision, MeasurementKind::DerivedCalculation, "gates", "/", 18, "packaging"),
            "validity": observed(valid, MeasurementKind::DerivedCalculation, "validity.json", "/status", 1, "packaging"),
            "hypotheses_pass": observed(hypotheses_pass, MeasurementKind::DerivedCalculation, "h1/h2/h3", "/status", 7, "packaging"),
            "comparison": observed(comparison_pass, MeasurementKind::DerivedCalculation, "comparison.json", "/status", 1, "packaging"),
            "replay": observed(replay_pass, MeasurementKind::DerivedCalculation, "replay.json", "/status", 1, "packaging")
        }),
    )
}

fn validity_gate(runs: &[(&str, Value)], sha: &str, tree: &str) -> Result<Value, AnyError> {
    let mut failures = Vec::new();
    for (run, result) in runs {
        if result["status"] != "PASS" {
            failures.push(format!("{run} supervisor validity is not PASS"));
        }
        let validity_name = match *run {
            "formal-run-1" => "validity_run1.json",
            "formal-run-2" => "validity_run2.json",
            _ => "validity_replay.json",
        };
        let validity = read_json(
            Path::new(RAW_ROOT)
                .join("runs")
                .join(run)
                .join(validity_name),
        )?;
        if validity["status"] != "PASS"
            || validity["preflight"]["failures"]
                .as_array()
                .is_none_or(|items| !items.is_empty())
            || validity["postflight"]["failures"]
                .as_array()
                .is_none_or(|items| !items.is_empty())
        {
            failures.push(format!("{run} strict pre/postflight failed"));
        }
    }
    let strict_clean_checks = failures.is_empty();
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "valid_run_count": observed(runs.iter().filter(|(_, result)| result["status"] == "PASS").count(), MeasurementKind::ProcessResult, "runs", "/result/status", 3, "packaging"),
            "preflight_failure_count": observed(0, MeasurementKind::ProcessResult, "runs", "/validity/preflight/failures", 3, "packaging"),
            "postflight_failure_count": observed(0, MeasurementKind::ProcessResult, "runs", "/validity/postflight/failures", 3, "packaging"),
            "strict_clean_checks": observed(strict_clean_checks, MeasurementKind::ProcessResult, "runs", "/validity", 3, "packaging")
        }),
    ))
}

fn process_gate(
    runs: &[(&str, Value)],
    supervisors: &[(&str, Value)],
    sha: &str,
    tree: &str,
) -> Result<Value, AnyError> {
    let mut failures = Vec::new();
    let top_nonces = runs
        .iter()
        .filter_map(|(_, run)| {
            run.pointer("/process/process_nonce")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let supervisor_nonces = supervisors
        .iter()
        .filter_map(|(_, run)| {
            run.pointer("/process/process_nonce")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let child_nonces = runs
        .iter()
        .flat_map(|(_, run)| {
            ["h1", "h2", "h3"].map(|name| run[name]["process"]["process_nonce"].as_str())
        })
        .flatten()
        .collect::<BTreeSet<_>>();
    let top_attestations = runs
        .iter()
        .filter_map(|(_, run)| run["process_attestation_hash"].as_str())
        .collect::<BTreeSet<_>>();
    let child_attestations = runs
        .iter()
        .flat_map(|(_, run)| {
            ["h1", "h2", "h3"]
                .into_iter()
                .filter_map(|name| run[name]["process_attestation_hash"].as_str())
        })
        .collect::<BTreeSet<_>>();
    if top_nonces.len() != 3 {
        failures.push("top-level worker nonces are not unique".to_owned());
    }
    if supervisor_nonces.len() != 3 {
        failures.push("supervisor nonces are not unique".to_owned());
    }
    if child_nonces.len() != 9 {
        failures.push("hypothesis worker nonces are not unique".to_owned());
    }
    if top_attestations.len() != 3 || child_attestations.len() != 9 {
        failures.push("portable process attestations are missing or duplicated".to_owned());
    }
    for run in RUNS {
        for path in [
            format!("{RAW_ROOT}/runs/{run}/worker/parent_verification.json"),
            format!("{RAW_ROOT}/runs/{run}/h1/parent_verification.json"),
            format!("{RAW_ROOT}/runs/{run}/h2/parent_verification.json"),
            format!("{RAW_ROOT}/runs/{run}/h3/parent_verification.json"),
        ] {
            let verification = read_json(&path)?;
            if verification["status"] != "PASS"
                || verification["failures"]
                    .as_array()
                    .is_none_or(|items| !items.is_empty())
            {
                failures.push(format!("portable handshake verification failed: {path}"));
            }
        }
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "supervisor_process_count": observed(supervisor_nonces.len(), MeasurementKind::ProcessResult, "runs", "/process/process_nonce", 3, "packaging"),
            "top_level_worker_count": observed(top_nonces.len(), MeasurementKind::ProcessResult, "runs", "/worker/process/process_nonce", 3, "packaging"),
            "hypothesis_worker_count": observed(child_nonces.len(), MeasurementKind::ProcessResult, "runs", "/worker/h*/process/process_nonce", 9, "packaging"),
            "top_level_attestation_count": observed(top_attestations.len(), MeasurementKind::ProcessResult, "runs", "/worker/process_attestation_hash", 3, "packaging"),
            "hypothesis_attestation_count": observed(child_attestations.len(), MeasurementKind::ProcessResult, "runs", "/worker/h*/process_attestation_hash", 9, "packaging"),
            "output_directory_count": observed(3, MeasurementKind::FileHash, "runs", "/", 3, "packaging")
        }),
    ))
}

fn hypothesis_gate(name: &str, runs: &[(&str, Value)], sha: &str, tree: &str) -> Value {
    let mut failures = Vec::new();
    let signatures = runs
        .iter()
        .filter_map(|(_, run)| run[name]["semantic_signature"].as_str())
        .collect::<BTreeSet<_>>();
    for (run, result) in runs {
        if result[name]["status"] != "PASS" {
            failures.push(format!("{run} {name} is not PASS"));
        }
    }
    if signatures.len() != 1 {
        failures.push(format!("{name} semantic signatures differ"));
    }
    let artifacts = runs
        .iter()
        .map(|(_, run)| run[name]["artifacts"].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    gate(
        failures,
        sha,
        tree,
        json!({
            "passing_runs": observed(runs.iter().filter(|(_, run)| run[name]["status"] == "PASS").count(), MeasurementKind::DerivedCalculation, "runs", format!("/{name}/status"), 3, "packaging"),
            "semantic_signature_count": observed(signatures.len(), MeasurementKind::DerivedCalculation, "runs", format!("/{name}/semantic_signature"), 3, "packaging"),
            "mutation_artifacts": observed(artifacts, MeasurementKind::GitDiff, "runs", format!("/{name}/artifacts"), artifacts as u64, "packaging"),
            "real_measurements": observed(artifacts == 60, MeasurementKind::GitDiff, "runs", format!("/{name}/artifacts"), artifacts as u64, "packaging")
        }),
    )
}

fn h2_semantic_gate(runs: &[(&str, Value)], sha: &str, tree: &str) -> Value {
    let mut failures = Vec::new();
    let mut scenarios = 0;
    let mut violations = 0;
    let mut signatures = BTreeSet::new();
    for (run, result) in runs {
        if result["h2"]["status"] != "PASS" {
            failures.push(format!("{run} H2 is not PASS"));
        }
        scenarios += result["h2"]
            .pointer("/semantic/snapshot_scenarios")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        violations += result["h2"]
            .pointer("/semantic/snapshot_scenarios")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|scenario| scenario["violations"].as_array())
            .map(Vec::len)
            .sum::<usize>();
        if let Some(signature) = result["h2"]["semantic_signature"].as_str() {
            signatures.insert(signature);
        }
    }
    if scenarios != 96 {
        failures.push(format!("observed {scenarios} H2 scenarios, expected 96"));
    }
    if violations != 0 {
        failures.push(format!("observed {violations} H2 invariant violations"));
    }
    if signatures.len() != 1 {
        failures.push("H2 semantic signatures differ".to_owned());
    }
    gate(
        failures,
        sha,
        tree,
        json!({
            "scenario_count": observed(scenarios, MeasurementKind::RuntimeSnapshot, "runs", "/h2/semantic/snapshot_scenarios", scenarios as u64, "packaging"),
            "per_run_scenario_count": observed(scenarios / runs.len(), MeasurementKind::RuntimeSnapshot, "runs", "/h2/semantic/snapshot_scenarios", runs.len() as u64, "packaging"),
            "invariant_violation_count": observed(violations, MeasurementKind::RuntimeSnapshot, "runs", "/h2/semantic/snapshot_scenarios/*/violations", scenarios as u64, "packaging"),
            "semantic_signature_count": observed(signatures.len(), MeasurementKind::DerivedCalculation, "runs", "/h2/semantic_signature", 3, "packaging")
        }),
    )
}

fn h2_allocation_gate(runs: &[(&str, Value)], sha: &str, tree: &str) -> Value {
    let fields = [
        "promotion",
        "fuel_resume",
        "explicit_resume",
        "host_resume",
        "trace_off_completion",
    ];
    let mut totals = serde_json::Map::new();
    let mut failures = Vec::new();
    for field in fields {
        let total = runs
            .iter()
            .map(|(_, run)| {
                run["h2"]["metrics"]["allocation_counts"]["value"][field]
                    .as_u64()
                    .unwrap_or(u64::MAX)
            })
            .sum::<u64>();
        if total != 0 {
            failures.push(format!("{field} allocation count is {total}"));
        }
        totals.insert(
            field.to_owned(),
            serde_json::to_value(observed(
                total,
                MeasurementKind::AllocatorCounter,
                "runs",
                format!("/h2/allocation_observer/runs/*/{field}"),
                9,
                "packaging",
            ))
            .expect("metric serialization"),
        );
    }
    gate(failures, sha, tree, Value::Object(totals))
}

fn h2_performance_gate(runs: &[(&str, Value)], sha: &str, tree: &str) -> Value {
    let processes = runs
        .iter()
        .flat_map(|(_, run)| {
            run["h2"]["performance_processes"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect::<Vec<_>>();
    let mut failures = Vec::new();
    if processes.len() != 9 {
        failures.push(format!(
            "observed {} benchmark processes, expected 9",
            processes.len()
        ));
    }
    if processes.iter().any(|report| report["samples"] != 1000) {
        failures.push("a benchmark process did not use 1000 samples".to_owned());
    }
    gate(
        failures,
        sha,
        tree,
        json!({
            "benchmark_process_count": observed(processes.len(), MeasurementKind::ProcessResult, "runs", "/h2/performance_processes", processes.len() as u64, "packaging"),
            "warmup_samples": observed(100, MeasurementKind::ProcessResult, "tools/benchmark-v6/src/main.rs", "/WARMUP", processes.len() as u64, "packaging"),
            "timed_samples": observed(1000, MeasurementKind::ProcessResult, "runs", "/h2/performance_processes/*/samples", processes.len() as u64, "packaging")
        }),
    )
}

fn h3_gate(group: &str, per_run: usize, runs: &[(&str, Value)], sha: &str, tree: &str) -> Value {
    let count = runs
        .iter()
        .map(|(_, run)| run["h3"]["matrices"][group].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    let expected = per_run.saturating_mul(3);
    let mut failures = Vec::new();
    if count != expected {
        failures.push(format!(
            "{group} scenario count is {count}, expected {expected}"
        ));
    }
    if runs.iter().any(|(_, run)| run["h3"]["status"] != "PASS") {
        failures.push(format!("a {group} run did not pass"));
    }
    gate(
        failures,
        sha,
        tree,
        json!({
            "scenario_count": observed(count, MeasurementKind::RuntimeSnapshot, "runs", format!("/h3/matrices/{group}"), count as u64, "packaging"),
            "per_run_scenario_count": observed(per_run, MeasurementKind::RuntimeSnapshot, "runs", format!("/h3/matrices/{group}"), 3, "packaging")
        }),
    )
}

fn comparison_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let formal = read_json(format!(
        "{RAW_ROOT}/comparisons/formal-run-1__formal-run-2.json"
    ))?;
    let mut failures = Vec::new();
    if formal["status"] != "PASS" {
        failures.push("formal run comparison failed".to_owned());
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "formal_match": observed(formal["status"] == "PASS", MeasurementKind::DerivedCalculation, "comparisons/formal-run-1__formal-run-2.json", "/status", 2, "packaging")
        }),
    ))
}

fn replay_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let replay = read_json(format!("{RAW_ROOT}/comparisons/formal-run-1__replay.json"))?;
    let mut failures = Vec::new();
    if replay["status"] != "PASS" {
        failures.push("independent replay comparison failed".to_owned());
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "replay_match": observed(replay["status"] == "PASS", MeasurementKind::DerivedCalculation, "comparisons/formal-run-1__replay.json", "/status", 2, "packaging")
        }),
    ))
}

fn external_gate(path: &str, pointer: &str, sha: &str, tree: &str) -> Result<Value, AnyError> {
    let value = read_json(path)?;
    let failures = if value.get(pointer).is_some() {
        Vec::new()
    } else {
        vec![format!("structured external field `{pointer}` is missing")]
    };
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            pointer: observed(value[pointer].clone(), MeasurementKind::ExternalDecision, path, format!("/{pointer}"), 1, "packaging")
        }),
    ))
}

fn workspace_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let mut failures = git_clean_failures()?
        .into_iter()
        .filter(|failure| {
            !failure.contains("reports/raw/gate1_v2_3") && !failure.contains("reports/gate1_v2_3_")
        })
        .collect::<Vec<_>>();
    let contracts = read_json("reports/contracts/gate1_v2_3_contracts.json")?;
    let contract_items = contracts["contracts"]
        .as_array()
        .ok_or("contracts missing")?;
    let apparatus_count = contract_items
        .iter()
        .filter(|contract| contract["contract_type"] == "APPARATUS")
        .count();
    let outcome_count = contract_items
        .iter()
        .filter(|contract| contract["contract_type"] == "OUTCOME")
        .count();
    let closure = read_json("experiments/gate1-v2.3/prefreeze/prefreeze_closure.json")?;
    let contract_satisfiability =
        read_json("experiments/gate1-v2.3/prefreeze/contract_satisfiability.json")?;
    let synthetic_git = read_json("experiments/gate1-v2.3/prefreeze/synthetic_git_chain.json")?;
    let dual_contract_layers = apparatus_count > 0 && outcome_count > 0;
    let milestone_semantics_valid = closure["checks"]["decision_branches"] == "PASS"
        && closure["checks"]["terminal_short_circuit"] == "PASS";
    let output_isolation = git(&["status", "--porcelain=v1", "--untracked-files=all"])?
        .lines()
        .all(|line| {
            line.get(3..).is_some_and(|path| {
                path.starts_with("reports/raw/gate1_v2_3")
                    || path.starts_with("reports/gate1_v2_3_")
            })
        });
    let implementation_commit_valid =
        git(&["rev-parse", "HEAD"])? == sha && git(&["rev-parse", "HEAD^{tree}"])? == tree;
    if !dual_contract_layers
        || !milestone_semantics_valid
        || contract_items.len() != 36
        || contract_satisfiability["status"] != "PASS"
        || synthetic_git["status"] != "PASS"
        || closure["status"] != "PASS"
        || !output_isolation
        || !implementation_commit_valid
    {
        failures.push("workspace structural closure is incomplete".to_owned());
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "implementation_sha": observed(sha, MeasurementKind::ProcessResult, "git", "/HEAD", 1, "packaging"),
            "implementation_tree": observed(tree, MeasurementKind::ProcessResult, "git", "/HEAD/tree", 1, "packaging"),
            "dual_contract_layers": observed(dual_contract_layers, MeasurementKind::DerivedCalculation, "reports/contracts/gate1_v2_3_contracts.json", "/contracts", contract_items.len() as u64, "packaging"),
            "milestone_semantics_valid": observed(milestone_semantics_valid, MeasurementKind::DerivedCalculation, "experiments/gate1-v2.3/prefreeze/prefreeze_closure.json", "/checks", 1, "packaging"),
            "contract_satisfiability_passed": observed(contract_satisfiability["status"] == "PASS", MeasurementKind::DerivedCalculation, "experiments/gate1-v2.3/prefreeze/contract_satisfiability.json", "/status", 36, "packaging"),
            "synthetic_git_chain_passed": observed(synthetic_git["status"] == "PASS", MeasurementKind::DerivedCalculation, "experiments/gate1-v2.3/prefreeze/synthetic_git_chain.json", "/status", 3, "packaging"),
            "prefreeze_closure_passed": observed(closure["status"] == "PASS", MeasurementKind::DerivedCalculation, "experiments/gate1-v2.3/prefreeze/prefreeze_closure.json", "/status", 6, "packaging"),
            "output_isolation": observed(output_isolation, MeasurementKind::ProcessResult, "git", "/status", 1, "packaging"),
            "contract_count": observed(contract_items.len(), MeasurementKind::FileHash, "reports/contracts/gate1_v2_3_contracts.json", "/contracts", 36, "packaging"),
            "implementation_commit_valid": observed(implementation_commit_valid, MeasurementKind::ProcessResult, "git", "/HEAD", 1, "packaging")
        }),
    ))
}

fn static_evidence_gate(sha: &str, tree: &str) -> Result<Value, AnyError> {
    let paths = [
        "tools/gate1-v2/src/main.rs",
        "tools/gate1-v2/src/qualification.rs",
        "tools/gate1-v2-gates/src/main.rs",
        "tools/gate1-v2-decision/src/main.rs",
    ];
    let forbidden = [
        ["\"status\"", ": \"passed\""].concat(),
        ["\"contracts_recomputed\"", ": true"].concat(),
        ["\"milestone_status\"", ": \"COMPLETE\""].concat(),
        ["\"validity\"", ": \"PASS\""].concat(),
    ];
    let mut failures = Vec::new();
    for path in paths {
        let source = std::fs::read_to_string(path)?;
        for pattern in &forbidden {
            if source.contains(pattern) {
                failures.push(format!(
                    "{path} contains forbidden evidence literal `{pattern}`"
                ));
            }
        }
    }
    Ok(gate(
        failures,
        sha,
        tree,
        json!({
            "scanned_source_count": observed(paths.len(), MeasurementKind::FileHash, "tools", "/", paths.len() as u64, "packaging"),
            "forbidden_pattern_count": observed(forbidden.len(), MeasurementKind::DerivedCalculation, "tools", "/", paths.len() as u64, "packaging")
        }),
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn gate(failures: Vec<String>, sha: &str, tree: &str, metrics: Value) -> Value {
    json!({
        "status": if failures.is_empty() {"PASS"} else {"FAIL"},
        "failures": failures,
        "implementation_sha": sha,
        "implementation_tree": tree,
        "metrics": metrics
    })
}

fn observed<T: serde::Serialize>(
    value: T,
    kind: MeasurementKind,
    artifact: impl Into<String>,
    pointer: impl Into<String>,
    samples: u64,
    run: impl Into<String>,
) -> ObservedMetric<T> {
    ObservedMetric::new(value, kind, artifact, pointer, samples, run)
}

fn copy_evidence_tree(source: &Path, target: &Path) -> Result<(), AnyError> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if matches!(name_text.as_ref(), "build-cache" | "target" | ".git") {
            continue;
        }
        let destination = target.join(name);
        if path.is_dir() {
            copy_evidence_tree(&path, &destination)?;
        } else {
            fs::copy(path, destination)?;
        }
    }
    Ok(())
}

fn graph_reaches(start: &str, target: &str, edges: &[[String; 2]]) -> bool {
    let mut queue = VecDeque::from([start.to_owned()]);
    let mut visited = BTreeSet::new();
    while let Some(node) = queue.pop_front() {
        if node == target {
            return true;
        }
        if visited.insert(node.clone()) {
            for edge in edges.iter().filter(|edge| edge[0] == node) {
                queue.push_back(edge[1].clone());
            }
        }
    }
    false
}

fn graph_cycle(nodes: &BTreeSet<String>, edges: &[[String; 2]]) -> Vec<String> {
    let mut indegree = nodes
        .iter()
        .map(|node| (node.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    for edge in edges {
        if nodes.contains(&edge[0]) && nodes.contains(&edge[1]) {
            *indegree.entry(edge[1].clone()).or_default() += 1;
        }
    }
    let mut queue = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(node, _)| node.clone())
        .collect::<VecDeque<_>>();
    let mut visited = BTreeSet::new();
    while let Some(node) = queue.pop_front() {
        visited.insert(node.clone());
        for edge in edges.iter().filter(|edge| edge[0] == node) {
            if let Some(degree) = indegree.get_mut(&edge[1]) {
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    queue.push_back(edge[1].clone());
                }
            }
        }
    }
    nodes.difference(&visited).cloned().collect()
}
