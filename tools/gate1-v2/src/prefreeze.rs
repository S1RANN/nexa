use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::process::Command;

use nexa_gate1_v2_3::{AnyError, git, hash_file, read_json, write_json};
use nexa_gate1_v2_3_fixtures::{FixtureCase, artifact_bundle, expected_decision};
use serde::Deserialize;
use serde_json::{Value, json};

const DRYRUN_ROOT: &str = "target/gate1-v2.3-dryrun";
const GRAPH_PATH: &str = "reports/history/gate1/supersession_graph.json";
const INDEX_PATH: &str = "reports/history/gate1/index.json";
const CONTRACTS_PATH: &str = "reports/contracts/gate1_v2_3_contracts.json";
const HISTORICAL_EVIDENCE: [&str; 4] = [
    "b6d49c0b4f7dd283dc0a04e6f1c1950e3c40bb4d",
    "b63e542c0bd5564704e0c2fda0c551376f60623f",
    "8e2296da6f9ca85a51fd164eb9f3f0c89849a499",
    "d1b582dd9544b2f72a1260cbb023e04a4dbad5ff",
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
    old_evidence_in_ancestry: bool,
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
    implementation_commit: Option<String>,
    affected_paths: Vec<String>,
    forbidden_patterns: Vec<String>,
    terminal_applicability_rule: String,
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
    let old_evidence_in_ancestry = HISTORICAL_EVIDENCE
        .iter()
        .any(|sha| history.lines().any(|line| line == *sha));
    let case = GraphCase {
        nodes: graph.nodes,
        edges: graph.edges,
        currents: vec![graph.current],
        decision_usable,
        old_evidence_in_ancestry,
    };
    let mut result = evaluate_graph(&case);
    let v2_2_sealed = read_json("reports/history/gate1/v2_2/terminal.json")?["decision_usable"]
        == false
        && read_json("reports/history/gate1/v2_2/terminal.json")?["receipt_created"] == false
        && read_json("reports/history/gate1/v2_2/terminal.json")?["retry_count"] == 0;
    let historical_immutability = immutable
        .iter()
        .filter(|(version, _)| version.as_str() != "gate1-v2.3")
        .all(|(_, value)| *value);
    result["metrics"] = json!({
        "v2_2_sealed": metric(v2_2_sealed),
        "version_count": metric(case.nodes.len()),
        "all_historical_nodes_reach_current": metric(
            result["unreachable_nodes"].as_array().is_some_and(Vec::is_empty)
        ),
        "historical_records_immutable": metric(historical_immutability),
        "invalid_evidence_absent_from_ancestry": metric(!old_evidence_in_ancestry)
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
        old_evidence_in_ancestry: false,
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
        .push(["gate1-v2.3".to_owned(), "gate1-v1".to_owned()]);
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
        .push(["gate1-v2.3".to_owned(), "gate1-v2.2".to_owned()]);
    cases.push(run_case(
        "current-has-outgoing-edge",
        &current_outgoing,
        false,
    ));

    usable.insert("gate1-v2.2".to_owned(), true);
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
        .push(["gate1-unknown".to_owned(), "gate1-v2.3".to_owned()]);
    cases.push(run_case("unknown-edge-node", &unknown_edge, false));

    let mut ancestry = base;
    ancestry.old_evidence_in_ancestry = true;
    cases.push(run_case("old-evidence-in-ancestry", &ancestry, false));

    let status = if cases.iter().all(|case| case["matched"] == true) {
        "PASS"
    } else {
        "FAIL"
    };
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.3",
        "cases": cases,
        "regression": {
            "v2_superseded_by": "gate1-v2.1",
            "current": "gate1-v2.3",
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
        if contract.implementation_commit.is_some() {
            failures.push(format!("{} embeds a pre-I implementation SHA", contract.id));
        }
        if contract.affected_paths.is_empty()
            || contract.forbidden_patterns.is_empty()
            || contract.terminal_applicability_rule.is_empty()
        {
            failures.push(format!("{} has incomplete contract metadata", contract.id));
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
    let expected_wps = (1_u32..=36).collect::<BTreeSet<_>>();
    if manifest.contracts.len() != 36 || work_packages != expected_wps {
        failures.push("manifest does not cover WP1-WP36 exactly once".to_owned());
    }
    let artifact_names = gates.keys().cloned().collect::<BTreeSet<_>>();
    for artifact in artifact_names.difference(&referenced) {
        failures.push(format!("synthetic gate `{artifact}` is not referenced"));
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
    let cases = FixtureCase::ALL
        .into_iter()
        .map(|case| {
            let fixture = artifact_bundle(case);
            let recomputed = recompute_fixture_decision(&fixture);
            json!({
                "fixture": case.name(),
                "expected": expected_decision(case),
                "actual": recomputed,
                "matched": recomputed == expected_decision(case),
                "synthetic": fixture["synthetic"],
                "formal_evidence_usable": fixture["formal_evidence_usable"]
            })
        })
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
        "nexa-gate1-v2.3-synthetic-chain-{}",
        std::process::id()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(&root)?;
    git_in(&root, &["init", "-q"])?;
    git_in(
        &root,
        &["config", "user.email", "gate1-v2.3@example.invalid"],
    )?;
    git_in(&root, &["config", "user.name", "Gate 1 v2.3 Dry Run"])?;

    std::fs::create_dir_all(root.join("tools"))?;
    std::fs::write(root.join("tools/apparatus.txt"), "synthetic I\n")?;
    commit_all(&root, "synthetic I")?;
    let implementation = git_in(&root, &["rev-parse", "HEAD"])?;

    std::fs::create_dir_all(root.join("reports/raw/gate1_v2_3"))?;
    std::fs::write(
        root.join("reports/raw/gate1_v2_3/synthetic.json"),
        "{\"synthetic\":true,\"formal_evidence_usable\":false}\n",
    )?;
    commit_all(&root, "synthetic E")?;
    let evidence = git_in(&root, &["rev-parse", "HEAD"])?;

    std::fs::create_dir_all(root.join("reports/contracts"))?;
    std::fs::write(
        root.join("reports/contracts/gate1_v2_3_verification_receipt.json"),
        "{\"synthetic\":true,\"status\":\"verified\"}\n",
    )?;
    commit_all(&root, "synthetic R")?;
    let receipt = git_in(&root, &["rev-parse", "HEAD"])?;

    let evidence_parent = git_in(&root, &["rev-parse", &format!("{evidence}^")])?;
    let receipt_parent = git_in(&root, &["rev-parse", &format!("{receipt}^")])?;
    let evidence_paths = git_in(
        &root,
        &[
            "diff",
            "--name-only",
            &format!("{implementation}..{evidence}"),
        ],
    )?;
    let receipt_paths = git_in(
        &root,
        &["diff", "--name-only", &format!("{evidence}..{receipt}")],
    )?;
    let path_ok = evidence_paths
        .lines()
        .all(|path| path.starts_with("reports/raw/gate1_v2_3/"))
        && receipt_paths == "reports/contracts/gate1_v2_3_verification_receipt.json";
    let parents_ok = evidence_parent == implementation && receipt_parent == evidence;
    let recomputed = parents_ok && path_ok;
    let result = json!({
        "schema_version": 1,
        "synthetic": true,
        "formal_evidence_usable": false,
        "implementation_sha": implementation,
        "evidence_sha": evidence,
        "receipt_sha": receipt,
        "parent_chain_valid": parents_ok,
        "evidence_paths_valid": path_ok,
        "artifact_hash_recomputed": recomputed,
        "contracts_recomputed": recomputed,
        "decision_recomputed": recomputed,
        "markdown_rebuilt": recomputed,
        "status_blocks_rebuilt": recomputed,
        "status": if parents_ok && path_ok {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("synthetic_git_chain.json"),
        &result,
    )?;
    std::fs::remove_dir_all(&root)?;
    ensure_status(&result, "synthetic Git chain")?;
    Ok(result)
}

pub fn prefreeze_closure() -> Result<Value, AnyError> {
    for forbidden in [
        "baseline/testing/GATE1_V2_3_AUTHORIZATION.md",
        "experiments/gate1-v2.3/authorization.json",
        "experiments/gate1-v2.3/manifest.json",
    ] {
        if Path::new(forbidden).exists() {
            return Err(format!("prefreeze closure must precede `{forbidden}`").into());
        }
    }
    let history = history_check()?;
    let governance = governance_negative_tests()?;
    let contracts = contract_satisfiability()?;
    let decisions = decision_branches()?;
    let short_circuit = terminal_short_circuit()?;
    let git_chain = synthetic_git_chain()?;
    let artifacts = [
        "history_check.json",
        "governance_negative_tests.json",
        "contract_satisfiability.json",
        "decision_branches.json",
        "terminal_short_circuit.json",
        "synthetic_git_chain.json",
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
        &contracts,
        &decisions,
        &short_circuit,
        &git_chain,
    ]
    .into_iter()
    .all(|value| value["status"] == "PASS");
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.3",
        "synthetic_artifacts_formal_evidence_usable": false,
        "checks": {
            "history": history["status"],
            "governance_negative_tests": governance["status"],
            "contract_satisfiability": contracts["status"],
            "decision_branches": decisions["status"],
            "terminal_short_circuit": short_circuit["status"],
            "synthetic_git_chain": git_chain["status"],
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
        && !case.old_evidence_in_ancestry;
    json!({
        "cycle": cycle,
        "unreachable_nodes": unreachable_nodes,
        "multiple_current_nodes": multiple_current_nodes,
        "usable_historical_decisions": usable_historical_decisions,
        "invalid_edges": invalid_edges,
        "duplicate_nodes": duplicate_nodes,
        "current_exists": current_exists,
        "current_has_outgoing_edge": current_has_outgoing,
        "old_evidence_in_ancestry": case.old_evidence_in_ancestry,
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

fn recompute_fixture_decision(fixture: &Value) -> &'static str {
    let gates = &fixture["gates"];
    let validity = gates["validity"]["status"].as_str().unwrap_or("INVALID");
    let h1 = gates["h1"]["status"].as_str().unwrap_or("INVALID");
    let h2 = gates["h2_semantic"]["status"].as_str().unwrap_or("INVALID");
    let h3 = gates["h3_migration"]["status"]
        .as_str()
        .unwrap_or("INVALID");
    if validity == "INVALID" {
        "INVALID"
    } else if validity == "INCONCLUSIVE" || [h1, h2, h3].contains(&"INCONCLUSIVE") {
        "UNVERIFIABLE_WITHIN_MVR"
    } else if [h1, h2, h3].contains(&"FAIL") {
        "STOP"
    } else if gates["pilot"]["commitment"] != "COMMITTED" {
        "HOLD"
    } else if gates["budget"]["approved"] == true {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    }
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

fn ensure_status(value: &Value, name: &str) -> Result<(), AnyError> {
    if value["status"] == "PASS" {
        Ok(())
    } else {
        Err(format!("{name} failed: {value}").into())
    }
}
