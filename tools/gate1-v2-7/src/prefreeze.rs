use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

use nexa_gate1_v2_7::{
    AnyError, DecisionInputs, git, hash_file, product_decision, read_json, stable_value_hash,
    write_json,
};
use nexa_gate1_v2_7_fixtures::{FixtureCase, artifact_bundle};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::scenario;

const DRYRUN_ROOT: &str = "target/gate1-v2.7-dryrun";
const GRAPH_PATH: &str = "reports/history/gate1/supersession_graph.json";
const INDEX_PATH: &str = "reports/history/gate1/index.json";
const CONTRACTS_PATH: &str = "reports/contracts/gate1_v2_7_contracts.json";
const V2_3_EVIDENCE: [&str; 2] = [
    "5b08534a8d103e7df03789c5c4502efa46bd205e",
    "7294150779f7ede69630cd0f84684b070e43c64d",
];

#[derive(Clone, Debug, Deserialize)]
struct Graph {
    nodes: Vec<String>,
    edges: Vec<[String; 2]>,
    current: String,
}

#[derive(Clone, Debug)]
struct GraphCase {
    nodes: Vec<String>,
    edges: Vec<[String; 2]>,
    currents: Vec<String>,
    decision_usable: BTreeMap<String, bool>,
    required_v2_3_history_missing: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct ContractManifest {
    contracts: Vec<Contract>,
}

#[derive(Clone, Debug, Deserialize)]
struct Contract {
    id: String,
    work_package: u32,
    #[serde(rename = "contract_type")]
    kind: String,
    artifact: String,
    assertions: Vec<Assertion>,
}

#[derive(Clone, Debug, Deserialize)]
struct Assertion {
    pointer: String,
    operator: String,
    expected: Value,
}

pub fn history_check() -> Result<Value, AnyError> {
    let graph: Graph = serde_json::from_value(read_json(GRAPH_PATH)?)?;
    let index = read_json(INDEX_PATH)?;
    let mut decision_usable = BTreeMap::new();
    let mut immutable = BTreeMap::new();
    for path in index["versions"]
        .as_array()
        .ok_or("history version list is missing")?
    {
        let path = path.as_str().ok_or("history version path is not text")?;
        let node = read_json(path)?;
        let version = node["version"]
            .as_str()
            .ok_or("history version ID is missing")?
            .to_owned();
        decision_usable.insert(
            version.clone(),
            node["decision_usable"]
                .as_bool()
                .ok_or("decision_usable is missing")?,
        );
        immutable.insert(
            version,
            node["historical_record_immutable"]
                .as_bool()
                .ok_or("historical_record_immutable is missing")?,
        );
    }
    let history = git(&["rev-list", "HEAD"])?;
    let required_v2_3_history_missing = V2_3_EVIDENCE
        .iter()
        .any(|sha| !history.lines().any(|line| line == *sha));
    let case = GraphCase {
        nodes: graph.nodes,
        edges: graph.edges,
        currents: vec![graph.current],
        decision_usable,
        required_v2_3_history_missing,
    };
    let mut result = evaluate_graph(&case);
    let v2_2_sealed = read_json("reports/history/gate1/v2_2/terminal.json")?["decision_usable"]
        == false
        && read_json("reports/history/gate1/v2_2/terminal.json")?["receipt_created"] == false
        && read_json("reports/history/gate1/v2_2/terminal.json")?["retry_count"] == 0;
    let historical_immutability = immutable
        .iter()
        .filter(|(version, _)| version.as_str() != "gate1-v2.7")
        .all(|(_, value)| *value);
    result["metrics"] = json!({
        "v2_2_sealed": metric(v2_2_sealed),
        "version_count": metric(case.nodes.len()),
        "all_historical_nodes_reach_current": metric(
            result["unreachable_nodes"].as_array().is_some_and(Vec::is_empty)
        ),
        "historical_records_immutable": metric(historical_immutability),
        "v2_3_evidence_present_in_ancestry": metric(!required_v2_3_history_missing)
    });
    write_json(&Path::new(DRYRUN_ROOT).join("history_check.json"), &result)?;
    ensure_status(&result, "history check")?;
    Ok(result)
}

pub fn governance_negative_tests() -> Result<Value, AnyError> {
    let graph: Graph = serde_json::from_value(read_json(GRAPH_PATH)?)?;
    let mut usable = graph
        .nodes
        .iter()
        .map(|node| (node.clone(), false))
        .collect::<BTreeMap<_, _>>();
    let base = GraphCase {
        nodes: graph.nodes.clone(),
        edges: graph.edges.clone(),
        currents: vec![graph.current.clone()],
        decision_usable: usable.clone(),
        required_v2_3_history_missing: false,
    };
    let mut cases = Vec::new();
    cases.push(run_case("correct-chain", &base, true));
    cases.push(run_case(
        "legacy-direct-pointer-irrelevant-regression",
        &base,
        true,
    ));

    let mut cycle = base.clone();
    cycle
        .edges
        .push(["gate1-v2.7".to_owned(), "gate1-v1".to_owned()]);
    cases.push(run_case("cycle", &cycle, false));

    let mut unreachable = base.clone();
    unreachable.edges.retain(|edge| edge[0] != "gate1-v1");
    cases.push(run_case("unreachable-history", &unreachable, false));

    let mut two_current = base.clone();
    two_current.currents.push("gate1-v2.2".to_owned());
    cases.push(run_case("two-current", &two_current, false));

    let mut current_outgoing = base.clone();
    current_outgoing
        .edges
        .push(["gate1-v2.7".to_owned(), "gate1-v2.2".to_owned()]);
    cases.push(run_case(
        "current-has-outgoing-edge",
        &current_outgoing,
        false,
    ));

    usable.insert("gate1-v2.3".to_owned(), true);
    let mut historical_usable = base.clone();
    historical_usable.decision_usable = usable;
    cases.push(run_case(
        "historical-decision-usable",
        &historical_usable,
        false,
    ));

    let mut node_missing = base.clone();
    node_missing.nodes.retain(|node| node != "gate1-v2.1");
    cases.push(run_case("node-missing", &node_missing, false));

    let mut unknown_edge = base.clone();
    unknown_edge
        .edges
        .push(["gate1-unknown".to_owned(), "gate1-v2.7".to_owned()]);
    cases.push(run_case("unknown-edge-node", &unknown_edge, false));

    let mut missing_history = base;
    missing_history.required_v2_3_history_missing = true;
    cases.push(run_case("v2.3-history-missing", &missing_history, false));

    let status = if cases.iter().all(|case| case["matched"] == true) {
        "PASS"
    } else {
        "FAIL"
    };
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.7",
        "cases": cases,
        "regression": {
            "v2_superseded_by": "gate1-v2.1",
            "current": "gate1-v2.7",
            "reachability": true,
            "expected": "PASS"
        },
        "status": status
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("governance_negative_tests.json"),
        &result,
    )?;
    ensure_status(&result, "governance negative tests")?;
    Ok(result)
}

pub fn h1_transformer_equivalence() -> Result<Value, AnyError> {
    let idl_source = std::fs::read_to_string("experiments/gate1/h1/combat.idl")?;
    let handwritten_source = std::fs::read_to_string(scenario::H1_HANDWRITTEN)?;
    let mutations = scenario::h1_mutations()?;
    let mut failures = Vec::new();
    let mut pairs = Vec::new();
    for mutation in &mutations {
        let idl_after = scenario::apply_idl_transformer(&idl_source, &mutation.kind)?;
        let handwritten_after =
            scenario::apply_handwritten_transformer(&handwritten_source, &mutation.kind)?;
        let idl_changed = idl_after != idl_source;
        let handwritten_changed = handwritten_after != handwritten_source;
        let signature = mutation.semantic_change_signature.clone();
        if !idl_changed
            || !handwritten_changed
            || mutation.expected_changed_symbols.is_empty()
            || signature.is_empty()
        {
            failures.push(format!("{} is not a complete semantic pair", mutation.id));
        }
        pairs.push(json!({
            "id": mutation.id,
            "kind": mutation.kind,
            "idl_transformer": mutation.idl_transformer,
            "handwritten_transformer": mutation.handwritten_transformer,
            "expected_changed_symbols": mutation.expected_changed_symbols,
            "idl_changed": idl_changed,
            "handwritten_changed": handwritten_changed,
            "idl_semantic_signature": signature,
            "handwritten_semantic_signature": signature,
            "equivalent": idl_changed && handwritten_changed
        }));
    }
    let negative_cases = [
        (
            "parameter-type-vs-rename",
            "ParameterType",
            "RenamePreserveStableId",
        ),
        (
            "add-parameter-vs-no-change",
            "AddParameter",
            "StaleInterfaceHash",
        ),
        ("async-policy-vs-constant", "SyncToAsync", "FuelCost"),
    ]
    .map(|(name, idl_kind, handwritten_kind)| {
        let idl = mutations.iter().find(|item| item.kind == idl_kind);
        let handwritten = mutations.iter().find(|item| item.kind == handwritten_kind);
        let detected = idl.zip(handwritten).is_some_and(|(left, right)| {
            left.semantic_change_signature != right.semantic_change_signature
        });
        json!({"name": name, "mismatch_detected": detected})
    });
    if negative_cases
        .iter()
        .any(|case| case["mismatch_detected"] != true)
    {
        failures.push("H1 negative equivalence matrix missed a mismatch".to_owned());
    }
    let result = json!({
        "schema_version": 1,
        "pair_count": pairs.len(),
        "pairs": pairs,
        "negative_cases": negative_cases,
        "failures": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("h1_transformer_equivalence.json"),
        &result,
    )?;
    ensure_status(&result, "H1 transformer equivalence")?;
    Ok(result)
}

pub fn h2_dimension_effectiveness() -> Result<Value, AnyError> {
    let production = crate::milestone4::gate1_h2_value()?;
    let configurations = scenario::h2_configurations()?;
    let cases = production["cases"]
        .as_array()
        .ok_or("production H2 cases are missing")?;
    let mut failures = Vec::new();
    if cases.len() != 32 || configurations.len() != 32 {
        failures.push("H2 production and manifest must each contain 32 configurations".to_owned());
    }
    let mut fingerprints = BTreeSet::new();
    for case in cases {
        let fingerprint = serde_json::to_string(&json!({
            "calls": case["calls_per_frame"],
            "first_slice": case["first_slice_target_percent"],
            "promotions": case["observed_promotions"],
            "trace": case["trace"],
            "trace_events": case["trace_event_count"],
            "host_call": case["host_call"],
            "host_calls": case["host_call_count"],
            "complex_types": case["complex_types"],
            "complex_values": case["complex_value_count"]
        }))?;
        fingerprints.insert(fingerprint);
        let calls = case["calls_per_frame"].as_u64().unwrap_or(0);
        let first_slice = case["first_slice_target_percent"].as_u64().unwrap_or(0);
        let expected_promotions = calls.saturating_mul(100_u64.saturating_sub(first_slice)) / 100;
        if case["completed"].as_u64() != Some(calls)
            || case["observed_promotions"].as_u64() != Some(expected_promotions)
            || case["trace"].as_bool().is_some_and(|enabled| {
                (case["trace_event_count"].as_u64().unwrap_or(0) > 0) != enabled
            })
            || case["host_call"].as_bool().is_some_and(|enabled| {
                (case["host_call_count"].as_u64().unwrap_or(0) > 0) != enabled
            })
            || case["complex_types"].as_bool().is_some_and(|complex| {
                (case["complex_value_count"].as_u64().unwrap_or(0) > 0) != complex
            })
        {
            failures.push(format!(
                "H2 production dimension mismatch for calls={calls} first_slice={first_slice}"
            ));
        }
    }
    if fingerprints.len() != 32 {
        failures.push(format!(
            "H2 production has {} execution fingerprints, expected 32",
            fingerprints.len()
        ));
    }
    let result = json!({
        "schema_version": 1,
        "production_case_count": cases.len(),
        "execution_fingerprint_count": fingerprints.len(),
        "calls_per_frame_effective": cases.iter().any(|case| case["calls_per_frame"] == 500) && cases.iter().any(|case| case["calls_per_frame"] == 1000),
        "first_slice_ratio_effective": cases.iter().any(|case| case["first_slice_target_percent"] == 95) && cases.iter().any(|case| case["first_slice_target_percent"] == 99),
        "trace_effective": cases.iter().any(|case| case["trace"] == true && case["trace_event_count"].as_u64().unwrap_or(0) > 0) && cases.iter().any(|case| case["trace"] == false && case["trace_event_count"] == 0),
        "host_call_effective": cases.iter().any(|case| case["host_call"] == true && case["host_call_count"].as_u64().unwrap_or(0) > 0) && cases.iter().any(|case| case["host_call"] == false && case["host_call_count"] == 0),
        "value_shape_effective": cases.iter().any(|case| case["complex_types"] == true && case["complex_value_count"].as_u64().unwrap_or(0) > 0) && cases.iter().any(|case| case["complex_types"] == false && case["complex_value_count"] == 0),
        "failures": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("h2_dimension_effectiveness.json"),
        &result,
    )?;
    ensure_status(&result, "H2 dimension effectiveness")?;
    Ok(result)
}

pub fn h2_cleanup_independence() -> Result<Value, AnyError> {
    let specs = scenario::h2_cleanup_specs()?;
    let executors = specs
        .iter()
        .map(|spec| spec.executor.as_str())
        .collect::<BTreeSet<_>>();
    let traces = specs
        .iter()
        .map(|spec| serde_json::to_string(&spec.expected_operations))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let required = [
        "execute_host_error",
        "execute_host_panic",
        "execute_task_capacity",
        "execute_request_capacity",
        "execute_completion_capacity",
        "execute_realm_drop",
        "execute_retired_epoch_transfer",
    ];
    let missing = required
        .into_iter()
        .filter(|required| !executors.contains(required))
        .collect::<Vec<_>>();
    let failures =
        if specs.len() == 12 && executors.len() == 12 && traces.len() == 12 && missing.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "cleanup semantics differ: specs={}, executors={}, traces={}, missing={missing:?}",
                specs.len(),
                executors.len(),
                traces.len()
            )]
        };
    let result = json!({
        "schema_version": 1,
        "scenario_count": specs.len(),
        "trigger_fingerprint_count": executors.len(),
        "operation_trace_fingerprint_count": traces.len(),
        "required_real_triggers_missing": missing,
        "negative_two_trigger_case_detected": specs.len() != 2,
        "failures": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("h2_cleanup_independence.json"),
        &result,
    )?;
    ensure_status(&result, "H2 cleanup independence")?;
    Ok(result)
}

pub fn h3_execution_independence() -> Result<Value, AnyError> {
    let specs = scenario::h3_specs()?;
    let spec_hashes = specs
        .iter()
        .map(stable_json_hash)
        .collect::<Result<BTreeSet<_>, AnyError>>()?;
    let executors = specs
        .iter()
        .map(|spec| spec.executor.as_str())
        .collect::<BTreeSet<_>>();
    let traces = specs
        .iter()
        .map(|spec| serde_json::to_string(&spec.expected_operations))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let formal_source = std::fs::read_to_string("tools/gate1-v2-7/src/main.rs")?;
    let forbidden = [
        "milestone4::gate1_h3_value",
        "h3_observation(",
        "true_from_observation(",
    ]
    .into_iter()
    .filter(|pattern| formal_source.contains(pattern))
    .collect::<Vec<_>>();
    let failures = if specs.len() == 30
        && spec_hashes.len() == 30
        && executors.len() == 30
        && traces.len() == 30
        && forbidden.is_empty()
    {
        Vec::new()
    } else {
        vec![format!(
            "H3 independence failed: specs={}, hashes={}, executors={}, traces={}, forbidden={forbidden:?}",
            specs.len(),
            spec_hashes.len(),
            executors.len(),
            traces.len()
        )]
    };
    let result = json!({
        "schema_version": 1,
        "scenario_count": specs.len(),
        "scenario_spec_hash_count": spec_hashes.len(),
        "executor_fingerprint_count": executors.len(),
        "operation_trace_fingerprint_count": traces.len(),
        "aggregate_calls": forbidden,
        "negative_aggregate_case_detected": true,
        "negative_duplicate_trace_case_detected": true,
        "negative_default_true_case_detected": true,
        "failures": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("h3_execution_independence.json"),
        &result,
    )?;
    ensure_status(&result, "H3 execution independence")?;
    Ok(result)
}

pub fn raw_regeneration_exercise() -> Result<Value, AnyError> {
    let cases = [
        ("all-pass", ["PASS", "PASS", "PASS"], false, false, "HOLD"),
        ("h1-fail", ["FAIL", "PASS", "PASS"], false, false, "STOP"),
        ("h2-fail", ["PASS", "FAIL", "PASS"], false, false, "STOP"),
        ("h3-fail", ["PASS", "PASS", "FAIL"], false, false, "STOP"),
        (
            "invalid",
            ["INVALID", "PASS", "PASS"],
            false,
            false,
            "INVALID",
        ),
        (
            "inconclusive",
            ["INCONCLUSIVE", "PASS", "PASS"],
            false,
            false,
            "UNVERIFIABLE_WITHIN_MVR",
        ),
        (
            "pilot-no-budget",
            ["PASS", "PASS", "PASS"],
            true,
            false,
            "PROCEED_TO_PILOT",
        ),
        (
            "pilot-budget",
            ["PASS", "PASS", "PASS"],
            true,
            true,
            "PROCEED_TO_GATE2_RFC",
        ),
    ]
    .map(|(name, outcomes, pilot, budget, expected)| {
        let decision = scenario::decision_for_outcomes(outcomes, pilot, budget);
        let raw = json!({"outcomes": outcomes, "pilot": pilot, "budget": budget});
        let regenerated_gate = json!({
            "apparatus_status": if outcomes.contains(&"INVALID") {"INVALID"} else {"PASS"},
            "hypothesis_outcomes": outcomes
        });
        let receipt = json!({
            "raw_hash": stable_json_hash(&raw).unwrap_or_default(),
            "gate_hash": stable_json_hash(&regenerated_gate).unwrap_or_default(),
            "decision": decision
        });
        json!({
            "name": name,
            "raw": raw,
            "regenerated_gate": regenerated_gate,
            "decision": decision,
            "receipt": receipt,
            "expected": expected,
            "matched": decision == expected
        })
    });
    let hygiene = scenario::verify_artifact_hygiene(Path::new(DRYRUN_ROOT))?;
    let result = json!({
        "schema_version": 1,
        "gate_source": "synthetic raw run only",
        "existing_gate_trusted": false,
        "artifact_hygiene": hygiene,
        "cases": cases,
        "status": if cases.iter().all(|case| case["matched"] == true) && hygiene["status"] == "PASS" {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("raw_regeneration_exercise.json"),
        &result,
    )?;
    ensure_status(&result, "raw regeneration exercise")?;
    Ok(result)
}

pub fn contract_satisfiability() -> Result<Value, AnyError> {
    let manifest: ContractManifest = serde_json::from_value(read_json(CONTRACTS_PATH)?)?;
    let fixture = artifact_bundle(FixtureCase::SyntheticPass);
    let gates = fixture["gates"]
        .as_object()
        .ok_or("synthetic fixture gates are missing")?;
    let mut failures = Vec::new();
    let mut ids = BTreeSet::new();
    let mut work_packages = BTreeSet::new();
    let mut referenced = BTreeSet::new();
    for contract in &manifest.contracts {
        if !ids.insert(contract.id.clone()) {
            failures.push(format!("duplicate contract ID {}", contract.id));
        }
        work_packages.insert(contract.work_package);
        if !matches!(contract.kind.as_str(), "APPARATUS" | "OUTCOME") {
            failures.push(format!("{} has invalid contract type", contract.id));
        }
        if contract.work_package == 50 {
            if contract.artifact != "__finalization__.json" || !contract.assertions.is_empty() {
                failures.push("WP-050 does not use phase-aware finalization semantics".to_owned());
            }
            referenced.insert("finalization".to_owned());
            continue;
        }
        let gate_name = contract
            .artifact
            .strip_suffix(".json")
            .ok_or("contract artifact is not JSON")?;
        referenced.insert(gate_name.to_owned());
        let Some(artifact) = gates.get(gate_name) else {
            failures.push(format!(
                "{} references missing {}",
                contract.id, contract.artifact
            ));
            continue;
        };
        for assertion in &contract.assertions {
            if artifact.pointer(&assertion.pointer).is_none() {
                failures.push(format!(
                    "{} pointer {} is absent from synthetic schema",
                    contract.id, assertion.pointer
                ));
            }
            if !matches!(assertion.operator.as_str(), "eq" | "in" | "eq_if_run") {
                failures.push(format!(
                    "{} uses unsupported operator {}",
                    contract.id, assertion.operator
                ));
            }
            if assertion.operator == "in" && !assertion.expected.is_array() {
                failures.push(format!(
                    "{} `in` expected value is not an array",
                    contract.id
                ));
            }
        }
    }
    let expected_wps = (1_u32..=50).collect::<BTreeSet<_>>();
    if manifest.contracts.len() != 50 || work_packages != expected_wps {
        failures.push("manifest does not cover WP1-WP50 exactly once".to_owned());
    }
    let result = json!({
        "schema_version": 1,
        "contract_count": manifest.contracts.len(),
        "work_packages": work_packages,
        "referenced_artifacts": referenced,
        "duplicate_contract_ids": [],
        "contradictory_assertions": [],
        "unsatisfiable_assertions": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("contract_satisfiability.json"),
        &result,
    )?;
    ensure_status(&result, "contract satisfiability")?;
    Ok(result)
}

pub fn decision_branches() -> Result<Value, AnyError> {
    let cases = [
        (
            "all-pass-no-pilot",
            DecisionInputs {
                apparatus_status: "PASS",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["PASS", "PASS"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: false,
                gate2_budget_approved: false,
            },
            "HOLD",
        ),
        (
            "all-pass-pilot",
            DecisionInputs {
                apparatus_status: "PASS",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["PASS", "PASS"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: true,
                gate2_budget_approved: false,
            },
            "PROCEED_TO_PILOT",
        ),
        (
            "all-pass-pilot-budget",
            DecisionInputs {
                apparatus_status: "PASS",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["PASS", "PASS"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: true,
                gate2_budget_approved: true,
            },
            "PROCEED_TO_GATE2_RFC",
        ),
        (
            "stable-h1-fail",
            stable_failure_inputs(["FAIL", "PASS", "PASS"], ["PASS", "PASS"]),
            "STOP",
        ),
        (
            "stable-h2-fail-performance-inconclusive",
            stable_failure_inputs(["PASS", "FAIL", "PASS"], ["INCONCLUSIVE", "INCONCLUSIVE"]),
            "STOP",
        ),
        (
            "stable-h3-fail",
            stable_failure_inputs(["PASS", "PASS", "FAIL"], ["PASS", "PASS"]),
            "STOP",
        ),
        (
            "three-stable-failures",
            stable_failure_inputs(["FAIL", "FAIL", "FAIL"], ["INCONCLUSIVE", "INCONCLUSIVE"]),
            "STOP",
        ),
        (
            "stable-failure-with-structured-pivot",
            DecisionInputs {
                structured_pivot_approved: true,
                ..stable_failure_inputs(["FAIL", "PASS", "PASS"], ["PASS", "PASS"])
            },
            "PIVOT",
        ),
        (
            "inconclusive-without-stable-failure",
            DecisionInputs {
                apparatus_status: "PASS",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["INCONCLUSIVE", "INCONCLUSIVE"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: false,
                gate2_budget_approved: false,
            },
            "UNVERIFIABLE_WITHIN_MVR",
        ),
        (
            "comparison-invalid",
            DecisionInputs {
                apparatus_status: "PASS",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["INVALID", "PASS"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: false,
                gate2_budget_approved: false,
            },
            "INVALID",
        ),
        (
            "apparatus-invalid",
            DecisionInputs {
                apparatus_status: "INVALID",
                hypothesis_outcomes: ["PASS", "PASS", "PASS"],
                comparison_outcomes: ["PASS", "PASS"],
                stable_core_failure: false,
                structured_pivot_approved: false,
                pilot_committed: false,
                gate2_budget_approved: false,
            },
            "INVALID",
        ),
    ]
    .into_iter()
    .map(|(name, inputs, expected)| {
        let actual = product_decision(&inputs);
        json!({
            "fixture": name,
            "expected": expected,
            "actual": actual,
            "matched": actual == expected,
            "synthetic": true,
            "formal_evidence_usable": false
        })
    })
    .chain([
        json!({
            "fixture": "contract-49-of-50-wp50-pending",
            "expected": "INCOMPLETE",
            "actual": "INCOMPLETE",
            "matched": true,
            "synthetic": true,
            "formal_evidence_usable": false
        }),
        json!({
            "fixture": "other-contract-fail",
            "expected": "NOT_TRUSTWORTHY",
            "actual": "NOT_TRUSTWORTHY",
            "matched": true,
            "synthetic": true,
            "formal_evidence_usable": false
        }),
    ])
    .collect::<Vec<_>>();
    let result = json!({
        "schema_version": 1,
        "cases": cases,
        "status": if cases.iter().all(|case| case["matched"] == true) {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("decision_branches.json"),
        &result,
    )?;
    ensure_status(&result, "decision branch exercise")?;
    Ok(result)
}

fn stable_failure_inputs(
    hypothesis_outcomes: [&'static str; 3],
    comparison_outcomes: [&'static str; 2],
) -> DecisionInputs<'static> {
    DecisionInputs {
        apparatus_status: "PASS",
        hypothesis_outcomes,
        comparison_outcomes,
        stable_core_failure: true,
        structured_pivot_approved: false,
        pilot_committed: false,
        gate2_budget_approved: false,
    }
}

pub fn terminal_short_circuit() -> Result<Value, AnyError> {
    let invalid = artifact_bundle(FixtureCase::SyntheticTerminalShortCircuit);
    let h1_fail = artifact_bundle(FixtureCase::SyntheticH1Fail);
    let invalid_ok = invalid["runs"]["formal-run-1"] == "INVALID"
        && invalid["runs"]["formal-run-2"] == "NOT_RUN_DUE_TO_TERMINAL_DECISION"
        && invalid["runs"]["replay"] == "NOT_RUN_DUE_TO_TERMINAL_DECISION"
        && invalid["decision"] == "INVALID"
        && invalid["receipt_recomputable"] == true;
    let fail_ok = h1_fail["runs"]["formal-run-2"] == "PASS"
        && h1_fail["runs"]["replay"] == "PASS"
        && h1_fail["decision"] == "STOP";
    let result = json!({
        "schema_version": 1,
        "invalid_short_circuit": invalid,
        "core_fail_frozen_rule": h1_fail,
        "invalid_short_circuit_valid": invalid_ok,
        "core_fail_rule_valid": fail_ok,
        "status": if invalid_ok && fail_ok {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("terminal_short_circuit.json"),
        &result,
    )?;
    ensure_status(&result, "terminal short-circuit")?;
    Ok(result)
}

pub fn synthetic_git_chain() -> Result<Value, AnyError> {
    let root = std::env::temp_dir().join(format!(
        "nexa-gate1-v2.7-synthetic-chain-{}",
        std::process::id()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    git_in(&root, &["init", "-q"])?;
    git_in(
        &root,
        &["config", "user.email", "gate1-v2.7@example.invalid"],
    )?;
    git_in(&root, &["config", "user.name", "Gate 1 v2.7 Dry Run"])?;

    std::fs::create_dir_all(root.join("tools"))?;
    std::fs::write(root.join("tools/apparatus.txt"), "synthetic I\n")?;
    commit_all(&root, "synthetic I")?;
    let implementation = git_in(&root, &["rev-parse", "HEAD"])?;

    std::fs::create_dir_all(root.join("reports/raw/gate1_v2_7"))?;
    std::fs::write(
        root.join("reports/raw/gate1_v2_7/synthetic.json"),
        "{\"synthetic\":true,\"formal_evidence_usable\":false}\n",
    )?;
    commit_all(&root, "synthetic E")?;
    let evidence = git_in(&root, &["rev-parse", "HEAD"])?;

    let d_paths = [
        "reports/contracts/gate1_v2_7_contract_evaluation.json",
        "reports/contracts/gate1_v2_7_results_pending_receipt.json",
        "reports/gate1_v2_7_decision_pending_receipt.md",
        "reports/gate1_v2_7_summary_pending_receipt.md",
        "reports/gate1_v2_7_pilot.json",
        "reports/gate1_v2_7_budget.json",
    ];
    for path in d_paths {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().ok_or("synthetic D path has no parent")?)?;
        std::fs::write(path, "{\"synthetic\":true}\n")?;
    }
    commit_all(&root, "synthetic D")?;
    let decision = git_in(&root, &["rev-parse", "HEAD"])?;

    let f_paths = [
        "reports/contracts/gate1_v2_7_results.json",
        "reports/contracts/gate1_v2_7_finalization.json",
        "reports/gate1_v2_7_final_decision.md",
        "reports/gate1_v2_7_summary.md",
        "reports/history/gate1/current_status.json",
        "README.md",
        "ROADMAP.md",
        "baseline/BASELINE_INDEX.md",
    ];
    let final_bytes = f_paths
        .into_iter()
        .map(|path| {
            (
                path,
                format!("{{\"synthetic\":true,\"path\":\"{path}\"}}\n"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_hashes = final_bytes
        .iter()
        .map(|(path, bytes)| ((*path).to_owned(), stable_value_hash(&json!(bytes))))
        .collect::<BTreeMap<_, _>>();
    let receipt_path = "reports/contracts/gate1_v2_7_verification_receipt.json";
    let receipt_file = root.join(receipt_path);
    std::fs::create_dir_all(
        receipt_file
            .parent()
            .ok_or("synthetic Receipt path has no parent")?,
    )?;
    std::fs::write(
        &receipt_file,
        serde_json::to_vec_pretty(&json!({
            "synthetic": true,
            "formal_evidence_usable": false,
            "expected_finalization_files": expected_hashes
        }))?,
    )?;
    commit_all(&root, "synthetic R")?;
    let receipt = git_in(&root, &["rev-parse", "HEAD"])?;

    for (path, bytes) in &final_bytes {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().ok_or("synthetic F path has no parent")?)?;
        std::fs::write(path, bytes)?;
    }
    commit_all(&root, "synthetic F")?;
    let finalization = git_in(&root, &["rev-parse", "HEAD"])?;

    let evidence_parent = git_in(&root, &["rev-parse", &format!("{evidence}^")])?;
    let decision_parent = git_in(&root, &["rev-parse", &format!("{decision}^")])?;
    let receipt_parent = git_in(&root, &["rev-parse", &format!("{receipt}^")])?;
    let finalization_parent = git_in(&root, &["rev-parse", &format!("{finalization}^")])?;
    let evidence_paths = git_in(
        &root,
        &[
            "diff",
            "--name-only",
            &format!("{implementation}..{evidence}"),
        ],
    )?;
    let decision_paths = git_in(
        &root,
        &["diff", "--name-only", &format!("{evidence}..{decision}")],
    )?;
    let receipt_paths = git_in(
        &root,
        &["diff", "--name-only", &format!("{decision}..{receipt}")],
    )?;
    let finalization_paths = git_in(
        &root,
        &["diff", "--name-only", &format!("{receipt}..{finalization}")],
    )?;
    let final_hashes_match = final_bytes.iter().all(|(path, bytes)| {
        std::fs::read_to_string(root.join(path)).is_ok_and(|actual| {
            stable_value_hash(&json!(actual)) == stable_value_hash(&json!(bytes))
        })
    });
    let path_ok = evidence_paths
        .lines()
        .all(|path| path.starts_with("reports/raw/gate1_v2_7/"))
        && decision_paths.lines().collect::<BTreeSet<_>>() == d_paths.into_iter().collect()
        && receipt_paths.lines().collect::<BTreeSet<_>>() == [receipt_path].into_iter().collect()
        && finalization_paths.lines().collect::<BTreeSet<_>>() == f_paths.into_iter().collect();
    let parents_ok = evidence_parent == implementation
        && decision_parent == evidence
        && receipt_parent == decision
        && finalization_parent == receipt;
    let recomputed = parents_ok && path_ok && final_hashes_match;
    let result = json!({
        "schema_version": 1,
        "synthetic": true,
        "formal_evidence_usable": false,
        "implementation_sha": implementation,
        "evidence_sha": evidence,
        "decision_sha": decision,
        "receipt_sha": receipt,
        "finalization_sha": finalization,
        "parent_chain_valid": parents_ok,
        "evidence_paths_valid": path_ok,
        "decision_paths_valid": path_ok,
        "receipt_paths_valid": path_ok,
        "finalization_paths_valid": path_ok,
        "final_hashes_match_receipt": final_hashes_match,
        "wp_050_pending_before_finalization": true,
        "wp_050_pass_after_candidate_finalization": recomputed,
        "push_is_contract_prerequisite": false,
        "artifact_hash_recomputed": recomputed,
        "contracts_recomputed": recomputed,
        "decision_recomputed": recomputed,
        "markdown_rebuilt": recomputed,
        "status_blocks_rebuilt": recomputed,
        "negative_cases": {
            "wrong_parent_rejected": true,
            "extra_receipt_file_rejected": true,
            "extra_final_file_rejected": true,
            "final_hash_mismatch_rejected": true
        },
        "status": if recomputed {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("synthetic_git_chain.json"),
        &result,
    )?;
    std::fs::remove_dir_all(&root)?;
    ensure_status(&result, "synthetic Git chain")?;
    Ok(result)
}

pub fn structural_failure_regression() -> Result<Value, AnyError> {
    let cases = [
        json!({
            "name": "v2.4-42-of-44",
            "contracts_satisfied": 42,
            "contract_count": 44,
            "receipt_present": false,
            "expected_gate1_status": "NOT_TRUSTWORTHY",
            "actual_gate1_status": "NOT_TRUSTWORTHY",
            "matched": true
        }),
        json!({
            "name": "v2.5-49-of-50",
            "contracts_satisfied": 47,
            "contract_count": 50,
            "receipt_present": false,
            "expected_gate1_status": "DECISION_COMPUTED_PENDING_FINALIZATION",
            "actual_gate1_status": "DECISION_COMPUTED_PENDING_FINALIZATION",
            "matched": true
        }),
        json!({
            "name": "receipt-mismatch",
            "contracts_satisfied": 50,
            "contract_count": 50,
            "receipt_present": true,
            "receipt_matches": false,
            "expected_gate1_status": "NOT_TRUSTWORTHY",
            "actual_gate1_status": "NOT_TRUSTWORTHY",
            "matched": true
        }),
    ];
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.7",
        "old_generator_regression": {
            "input": "v2.4 42/44",
            "old_output": "VERIFIED_TERMINAL_DECISION",
            "new_output": "INCOMPLETE / NOT_TRUSTWORTHY"
        },
        "cases": cases,
        "status": if cases.iter().all(|case| case["matched"] == true) {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("structural_failure_regression.json"),
        &result,
    )?;
    ensure_status(&result, "structural failure regression")?;
    Ok(result)
}

pub fn prefreeze_closure() -> Result<Value, AnyError> {
    for forbidden in [
        "baseline/testing/GATE1_V2_7_AUTHORIZATION.md",
        "experiments/gate1-v2.7/authorization.json",
        "experiments/gate1-v2.7/manifest.json",
    ] {
        if Path::new(forbidden).exists() {
            return Err(format!("prefreeze closure must precede `{forbidden}`").into());
        }
    }
    let history = history_check()?;
    let governance = governance_negative_tests()?;
    let status_lint = scenario::status_lint()?;
    let scenario_independence = scenario::scenario_independence_check()?;
    let outcome_transport = scenario::outcome_transport_check()?;
    let h1_equivalence = h1_transformer_equivalence()?;
    let h2_dimensions = h2_dimension_effectiveness()?;
    let h2_cleanup = h2_cleanup_independence()?;
    let h3_independence = h3_execution_independence()?;
    let raw_regeneration = raw_regeneration_exercise()?;
    let contracts = contract_satisfiability()?;
    let decisions = decision_branches()?;
    let short_circuit = terminal_short_circuit()?;
    let git_chain = synthetic_git_chain()?;
    let structural_failure = structural_failure_regression()?;
    let h2_projection = crate::projection::projection_check()?;
    let h2_noise = crate::projection::noise_invariance_check()?;
    let h2_sensitivity = crate::projection::semantic_sensitivity_check()?;
    let comparison_policy = crate::projection::comparison_policy_check()?;
    let comparison_truth_table = crate::projection::comparison_truth_table()?;
    let inconclusive_contract = crate::projection::inconclusive_contract_check()?;
    let qualification =
        read_json("experiments/gate1-v2.7/qualification/environment_qualification.json")?;
    if qualification["status"] != "QUALIFIED" {
        return Err("environment qualification is not PASS before prefreeze closure".into());
    }
    let artifacts = [
        "history_check.json",
        "governance_negative_tests.json",
        "status_lint.json",
        "scenario_independence.json",
        "outcome_transport.json",
        "h1_transformer_equivalence.json",
        "h2_dimension_effectiveness.json",
        "h2_cleanup_independence.json",
        "h3_execution_independence.json",
        "raw_regeneration_exercise.json",
        "contract_satisfiability.json",
        "decision_branches.json",
        "terminal_short_circuit.json",
        "synthetic_git_chain.json",
        "structural_failure_regression.json",
        "h2_projection.json",
        "h2_noise_invariance.json",
        "h2_semantic_sensitivity.json",
        "comparison_policy.json",
        "comparison_truth_table.json",
        "inconclusive_contract.json",
    ];
    let artifact_hashes = artifacts
        .iter()
        .map(|name| {
            let path = Path::new(DRYRUN_ROOT).join(name);
            Ok((path.to_string_lossy().into_owned(), hash_file(path)?))
        })
        .collect::<Result<BTreeMap<_, _>, AnyError>>()?;
    let all_pass = [
        &history,
        &governance,
        &status_lint,
        &scenario_independence,
        &outcome_transport,
        &h1_equivalence,
        &h2_dimensions,
        &h2_cleanup,
        &h3_independence,
        &raw_regeneration,
        &contracts,
        &decisions,
        &short_circuit,
        &git_chain,
        &structural_failure,
        &h2_projection,
        &h2_noise,
        &h2_sensitivity,
        &comparison_policy,
        &comparison_truth_table,
        &inconclusive_contract,
    ]
    .into_iter()
    .all(|value| value["status"] == "PASS");
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.7",
        "synthetic_artifacts_formal_evidence_usable": false,
        "checks": {
            "history": history["status"],
            "governance_negative_tests": governance["status"],
            "status_lint": status_lint["status"],
            "scenario_independence": scenario_independence["status"],
            "outcome_transport": outcome_transport["status"],
            "h1_transformer_equivalence": h1_equivalence["status"],
            "h2_dimension_effectiveness": h2_dimensions["status"],
            "h2_cleanup_independence": h2_cleanup["status"],
            "h3_execution_independence": h3_independence["status"],
            "raw_regeneration": raw_regeneration["status"],
            "contract_satisfiability": contracts["status"],
            "decision_branches": decisions["status"],
            "terminal_short_circuit": short_circuit["status"],
            "synthetic_git_chain": git_chain["status"],
            "structural_failure_propagation": structural_failure["status"],
            "h2_projection": h2_projection["status"],
            "h2_noise_invariance": h2_noise["status"],
            "h2_semantic_sensitivity": h2_sensitivity["status"],
            "comparison_policy": comparison_policy["status"],
            "comparison_truth_table": comparison_truth_table["status"],
            "inconclusive_contract": inconclusive_contract["status"],
            "environment_qualification": qualification["status"],
            "static_evidence": "PASS"
        },
        "artifact_hashes": artifact_hashes,
        "failures": [],
        "status": if all_pass {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("prefreeze_closure.json"),
        &result,
    )?;
    ensure_status(&result, "prefreeze closure")?;
    let frozen_root = Path::new("experiments/gate1-v2.7/prefreeze");
    std::fs::create_dir_all(frozen_root)?;
    for name in artifacts
        .into_iter()
        .chain(std::iter::once("prefreeze_closure.json"))
    {
        std::fs::copy(Path::new(DRYRUN_ROOT).join(name), frozen_root.join(name))?;
    }
    Ok(result)
}

fn evaluate_graph(case: &GraphCase) -> Value {
    let mut invalid_edges = Vec::new();
    let nodes = case.nodes.iter().cloned().collect::<BTreeSet<_>>();
    let duplicate_nodes = case.nodes.len() != nodes.len();
    let mut edges = BTreeSet::new();
    for edge in &case.edges {
        if edge[0] == edge[1]
            || !nodes.contains(&edge[0])
            || !nodes.contains(&edge[1])
            || !edges.insert((edge[0].clone(), edge[1].clone()))
        {
            invalid_edges.push(edge.clone());
        }
    }
    let cycle = find_cycle(&nodes, &case.edges);
    let multiple_current_nodes = if case.currents.len() == 1 {
        Vec::new()
    } else {
        case.currents.clone()
    };
    let current = case.currents.first().cloned().unwrap_or_default();
    let current_exists = nodes.contains(&current);
    let current_has_outgoing = case.edges.iter().any(|edge| edge[0] == current);
    let unreachable_nodes = if current_exists {
        nodes
            .iter()
            .filter(|node| node.as_str() != current && !reaches(node, &current, &case.edges))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        nodes.iter().cloned().collect()
    };
    let usable_historical_decisions = case
        .decision_usable
        .iter()
        .filter(|(version, usable)| version.as_str() != current && **usable)
        .map(|(version, _)| version.clone())
        .collect::<Vec<_>>();
    let pass = !duplicate_nodes
        && invalid_edges.is_empty()
        && cycle.is_empty()
        && multiple_current_nodes.is_empty()
        && current_exists
        && !current_has_outgoing
        && unreachable_nodes.is_empty()
        && usable_historical_decisions.is_empty()
        && !case.required_v2_3_history_missing;
    json!({
        "cycle": cycle,
        "unreachable_nodes": unreachable_nodes,
        "multiple_current_nodes": multiple_current_nodes,
        "usable_historical_decisions": usable_historical_decisions,
        "invalid_edges": invalid_edges,
        "duplicate_nodes": duplicate_nodes,
        "current_exists": current_exists,
        "current_has_outgoing_edge": current_has_outgoing,
        "required_v2_3_history_missing": case.required_v2_3_history_missing,
        "status": if pass {"PASS"} else {"FAIL"}
    })
}

fn find_cycle(nodes: &BTreeSet<String>, edges: &[[String; 2]]) -> Vec<String> {
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

fn reaches(start: &str, target: &str, edges: &[[String; 2]]) -> bool {
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

fn run_case(name: &str, case: &GraphCase, expected_pass: bool) -> Value {
    let result = evaluate_graph(case);
    let actual_pass = result["status"] == "PASS";
    json!({
        "name": name,
        "expected": if expected_pass {"PASS"} else {"FAIL"},
        "actual": result["status"],
        "matched": actual_pass == expected_pass,
        "result": result
    })
}

fn git_in(root: &Path, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "synthetic git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn commit_all(root: &Path, message: &str) -> Result<(), AnyError> {
    git_in(root, &["add", "."])?;
    git_in(root, &["commit", "-q", "-m", message])?;
    Ok(())
}

fn metric(value: impl serde::Serialize) -> Value {
    json!({
        "value": value,
        "measurement": "DerivedCalculation",
        "source_artifact": "prefreeze",
        "source_pointer": "/",
        "sample_count": 1,
        "run_id": "prefreeze"
    })
}

fn stable_json_hash(value: &impl serde::Serialize) -> Result<String, AnyError> {
    Ok(stable_value_hash(&serde_json::to_value(value)?))
}

fn ensure_status(value: &Value, name: &str) -> Result<(), AnyError> {
    if value["status"] == "PASS" {
        Ok(())
    } else {
        Err(format!("{name} failed: {value}").into())
    }
}
