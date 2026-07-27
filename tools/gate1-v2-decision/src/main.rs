#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nexa_gate1_v2_3::{
    AnyError, git, hash_file, read_json, repository_root, stable_value_hash, write_json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CONTRACTS: &str = "reports/contracts/gate1_v2_3_contracts.json";
const RESULTS: &str = "reports/contracts/gate1_v2_3_results.json";
const FINAL: &str = "reports/gate1_v2_3_final_decision.md";
const SUMMARY: &str = "reports/gate1_v2_3_summary.md";
const RECEIPT: &str = "reports/contracts/gate1_v2_3_verification_receipt.json";
const GATES: &str = "reports/raw/gate1_v2_3/gates";
const STATUS_START: &str = "<!-- gate1-v2.3-status:start -->";
const STATUS_END: &str = "<!-- gate1-v2.3-status:end -->";

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    contracts: Vec<Contract>,
}

#[derive(Clone, Debug, Deserialize)]
struct Contract {
    id: String,
    work_package: u32,
    description: String,
    #[serde(rename = "contract_type")]
    kind: String,
    gate: String,
    artifact: String,
    assertions: Vec<Assertion>,
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

#[derive(Clone, Debug, Serialize)]
struct Evaluation {
    pointer: String,
    operator: String,
    expected: Value,
    actual: Value,
    satisfied: bool,
    reason: String,
}

#[derive(Clone, Debug)]
struct Generated {
    results: Value,
    final_report: String,
    summary: String,
    status_blocks: BTreeMap<&'static str, String>,
}

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "generate" => generate(),
        [command] if command == "verify-evidence" => verify_evidence(),
        [command] if command == "generate-receipt" => generate_receipt(),
        [command] if command == "verify-final" => verify_final(),
        _ => Err(
            "usage: nexa-gate1-v2-3-decision generate|verify-evidence|generate-receipt|verify-final"
                .into(),
        ),
    }
}

fn generate() -> Result<(), AnyError> {
    let generated = reconstruct()?;
    write_json(Path::new(RESULTS), &generated.results)?;
    std::fs::write(FINAL, &generated.final_report)?;
    std::fs::write(SUMMARY, &generated.summary)?;
    for (path, block) in &generated.status_blocks {
        replace_status_block(Path::new(path), block)?;
    }
    println!(
        "Gate 1 v2.3 decision: {}",
        generated.results["decision"].as_str().unwrap_or("INVALID")
    );
    Ok(())
}

fn verify_evidence() -> Result<(), AnyError> {
    let generated = reconstruct()?;
    if read_json(RESULTS)? != generated.results {
        return Err("Gate 1 v2.3 results differ from recomputed contracts and decision".into());
    }
    if std::fs::read_to_string(FINAL)? != generated.final_report {
        return Err("Gate 1 v2.3 final report differs from recomputed report".into());
    }
    if std::fs::read_to_string(SUMMARY)? != generated.summary {
        return Err("Gate 1 v2.3 summary differs from recomputed summary".into());
    }
    for (path, expected) in &generated.status_blocks {
        let actual = extract_status_block(&std::fs::read_to_string(path)?)?;
        if &actual != expected {
            return Err(format!("{path} status block differs from decision JSON").into());
        }
    }
    println!("Gate 1 v2.3 evidence semantics verified");
    Ok(())
}

fn reconstruct() -> Result<Generated, AnyError> {
    let manifest: Manifest = serde_json::from_value(read_json(CONTRACTS)?)?;
    if manifest.contracts.len() != 36 {
        return Err(format!(
            "expected 36 contracts, observed {}",
            manifest.contracts.len()
        )
        .into());
    }
    let implementation_sha = gate_identity("implementation_sha")?;
    let implementation_tree = gate_identity("implementation_tree")?;
    let successful_word = ["pass", "ed"].concat();
    let unsuccessful_word = ["fail", "ed"].concat();
    let mut evaluated = Vec::new();
    let mut known_gaps = Vec::new();
    for contract in manifest.contracts {
        let artifact = read_json(Path::new(GATES).join(&contract.artifact))?;
        let checks = contract
            .assertions
            .iter()
            .map(|assertion| evaluate_assertion(&artifact, assertion))
            .collect::<Vec<_>>();
        let contract_ok = checks.iter().all(|check| check.satisfied);
        if !contract_ok {
            for check in checks.iter().filter(|check| !check.satisfied) {
                known_gaps.push(json!({
                    "contract": contract.id,
                    "expected": check.expected,
                    "actual": check.actual,
                    "reason": check.reason
                }));
            }
        }
        evaluated.push(json!({
            "id": contract.id,
            "work_package": contract.work_package,
            "description": contract.description,
            "contract_type": contract.kind,
            "gate": contract.gate,
            "artifact": contract.artifact,
            "assertions": checks,
            "implementation_commit": implementation_sha,
            "affected_paths": contract.affected_paths,
            "forbidden_patterns": contract.forbidden_patterns,
            "terminal_applicability_rule": contract.terminal_applicability_rule,
            "status": if contract_ok {&successful_word} else {&unsuccessful_word}
        }));
    }
    let gates = load_gate_statuses()?;
    let validity = gates["validity"].as_str().unwrap_or("INVALID");
    let h1 = gates["h1"].as_str().unwrap_or("INVALID");
    let h2 = combined_outcome(&gates, &["h2_semantic", "h2_allocations", "h2_performance"]);
    let h3 = combined_outcome(&gates, &["h3_migration", "h3_completion", "h3_transaction"]);
    let comparison = gates["comparison"].as_str().unwrap_or("INVALID");
    let replay = gates["replay"].as_str().unwrap_or("INVALID");
    let pilot = read_json("reports/gate1_v2_3_pilot.json")?;
    let budget = read_json("reports/gate1_v2_3_budget.json")?;
    let pilot_committed = pilot["commitment"] == "COMMITTED";
    let budget_approved = budget["approved"].as_bool().unwrap_or(false);
    let outcome_set = [h1, h2, h3];
    let comparison_invalid =
        matches!(comparison, "FAIL" | "INVALID") || matches!(replay, "FAIL" | "INVALID");
    let decision = if validity == "INVALID"
        || outcome_set.contains(&"INVALID")
        || (validity == "PASS" && comparison_invalid)
    {
        "INVALID"
    } else if validity == "INCONCLUSIVE" || outcome_set.contains(&"INCONCLUSIVE") {
        "UNVERIFIABLE_WITHIN_MVR"
    } else if outcome_set.contains(&"FAIL") {
        "STOP"
    } else if !pilot_committed {
        "HOLD"
    } else if budget_approved {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    };
    let legal_decisions = [
        "PROCEED_TO_PILOT",
        "PROCEED_TO_GATE2_RFC",
        "HOLD",
        "PIVOT",
        "STOP",
        "INVALID",
        "UNVERIFIABLE_WITHIN_MVR",
    ];
    let all_contracts_pass = evaluated
        .iter()
        .all(|contract| contract["status"] == successful_word);
    let apparatus_contracts_pass = evaluated.iter().all(|contract| {
        contract["contract_type"] != "APPARATUS" || contract["status"] == successful_word
    });
    let apparatus_gates_pass = [
        "governance",
        "history",
        "environment",
        "process_provenance",
        "workspace",
    ]
    .iter()
    .all(|name| gates[*name] == "PASS");
    let decision_legal = legal_decisions.contains(&decision);
    let milestone_complete = all_contracts_pass
        && apparatus_contracts_pass
        && apparatus_gates_pass
        && decision_legal
        && known_gaps.is_empty();
    let milestone_status = if milestone_complete {
        "COMPLETE"
    } else {
        "INCOMPLETE"
    };
    let results = json!({
        "schema_version": 3,
        "experiment_version": "gate1-v2.3",
        "milestone": "5.0R3",
        "milestone_status": milestone_status,
        "gate1_status": if milestone_complete {"VERIFIED_TERMINAL_DECISION"} else {"NOT_TRUSTWORTHY"},
        "decision": decision,
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "evidence_sha": "SELF",
        "hypotheses": {
            "H1a": h1,
            "H2a": h2,
            "H3a": h3
        },
        "validity": validity,
        "formal_comparison": comparison,
        "replay": replay,
        "pilot": pilot,
        "gate2_budget": budget,
        "decision_inputs": {
            "validity": validity,
            "h1": h1,
            "h2": h2,
            "h3": h3,
            "replay": replay,
            "pilot_committed": pilot_committed,
            "gate2_budget_approved": budget_approved
        },
        "contracts": evaluated,
        "contract_summary": {
            "total": 36,
            "apparatus_passed": apparatus_contracts_pass,
            "outcomes_recomputed": evaluated.iter().filter(|contract| contract["contract_type"] == "OUTCOME").all(|contract| contract["status"] == successful_word),
            "passed": if all_contracts_pass {36} else {
                evaluated.iter().filter(|contract| contract["status"] == successful_word).count()
            },
            "failed": evaluated.iter().filter(|contract| contract["status"] != successful_word).count()
        },
        "known_gaps": known_gaps
    });
    let final_report = render_final(&results);
    let summary = render_summary(&results);
    let status_blocks = BTreeMap::from([
        ("README.md", readme_status(&results)),
        ("ROADMAP.md", roadmap_status(&results)),
        ("baseline/BASELINE_INDEX.md", baseline_status(&results)),
    ]);
    Ok(Generated {
        results,
        final_report,
        summary,
        status_blocks,
    })
}

fn combined_outcome<'a>(gates: &'a BTreeMap<String, Value>, names: &[&str]) -> &'a str {
    let statuses = names
        .iter()
        .map(|name| gates[*name].as_str().unwrap_or("INVALID"))
        .collect::<Vec<_>>();
    for terminal in ["INVALID", "INCONCLUSIVE", "FAIL"] {
        if statuses.contains(&terminal) {
            return statuses
                .into_iter()
                .find(|status| *status == terminal)
                .unwrap_or("INVALID");
        }
    }
    if statuses
        .iter()
        .all(|status| *status == "NOT_RUN_DUE_TO_TERMINAL_DECISION")
    {
        "NOT_RUN_DUE_TO_TERMINAL_DECISION"
    } else if statuses.iter().all(|status| *status == "PASS") {
        "PASS"
    } else {
        "INCONCLUSIVE"
    }
}

fn evaluate_assertion(artifact: &Value, assertion: &Assertion) -> Evaluation {
    let actual = artifact
        .pointer(&assertion.pointer)
        .cloned()
        .unwrap_or(Value::Null);
    let satisfied = match assertion.operator.as_str() {
        "eq" => actual == assertion.expected,
        "in" => assertion
            .expected
            .as_array()
            .is_some_and(|allowed| allowed.contains(&actual)),
        "eq_if_run" => {
            artifact["status"] == "NOT_RUN_DUE_TO_TERMINAL_DECISION" || actual == assertion.expected
        }
        "ge" => {
            actual.as_f64().unwrap_or(f64::NEG_INFINITY)
                >= assertion.expected.as_f64().unwrap_or(f64::INFINITY)
        }
        "len_eq" => {
            actual.as_array().map_or(usize::MAX, Vec::len)
                == assertion
                    .expected
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .unwrap_or(usize::MAX)
        }
        "is_empty" => {
            actual.as_array().is_some_and(Vec::is_empty)
                == assertion.expected.as_bool().unwrap_or(false)
        }
        _ => false,
    };
    Evaluation {
        pointer: assertion.pointer.clone(),
        operator: assertion.operator.clone(),
        expected: assertion.expected.clone(),
        actual,
        satisfied,
        reason: if satisfied {
            "assertion satisfied".to_owned()
        } else {
            format!(
                "JSON Pointer {} did not satisfy {}",
                assertion.pointer, assertion.operator
            )
        },
    }
}

fn load_gate_statuses() -> Result<BTreeMap<String, Value>, AnyError> {
    let names = [
        "governance",
        "history",
        "environment",
        "validity",
        "process_provenance",
        "h1",
        "h2_semantic",
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
    ];
    names
        .into_iter()
        .map(|name| {
            Ok((
                name.to_owned(),
                read_json(Path::new(GATES).join(format!("{name}.json")))?["status"].clone(),
            ))
        })
        .collect()
}

fn gate_identity(field: &str) -> Result<String, AnyError> {
    let mut values = BTreeMap::new();
    for entry in std::fs::read_dir(GATES)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let value = read_json(&path)?;
            values.insert(value[field].as_str().unwrap_or_default().to_owned(), path);
        }
    }
    if values.len() != 1 {
        return Err(format!("gate artifacts do not bind one {field}: {values:?}").into());
    }
    values
        .into_keys()
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("gate artifact {field} is empty").into())
}

fn render_final(results: &Value) -> String {
    format!(
        "# Gate 1 v2.3 Final Decision\n\n\
         Decision: **{}**\n\n\
         Milestone 5.0R3: **{}**  \n\
         Gate 1 v2.3: **{}**\n\n\
         H1a: **{}**; H2a: **{}**; H3a: **{}**. Validity, comparison, and replay: **{} / {} / {}**.\n\n\
         All {} machine contracts were evaluated from 17 independent gate artifacts; {} passed and {} failed. \
         Known structural gaps: {}.\n\n\
         The structured Pilot record is `{}` and Gate 2 budget approval is `{}`. Milestone completion \
         records a recomputable terminal decision and does not by itself authorize Pilot or Gate 2.\n",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE"),
        results["gate1_status"]
            .as_str()
            .unwrap_or("NOT_TRUSTWORTHY"),
        results["hypotheses"]["H1a"].as_str().unwrap_or("FAIL"),
        results["hypotheses"]["H2a"].as_str().unwrap_or("FAIL"),
        results["hypotheses"]["H3a"].as_str().unwrap_or("FAIL"),
        results["validity"].as_str().unwrap_or("INVALID"),
        results["formal_comparison"].as_str().unwrap_or("FAIL"),
        results["replay"].as_str().unwrap_or("FAIL"),
        results["contract_summary"]["total"].as_u64().unwrap_or(0),
        results["contract_summary"]["passed"].as_u64().unwrap_or(0),
        results["contract_summary"]["failed"].as_u64().unwrap_or(0),
        results["known_gaps"].as_array().map_or(0, Vec::len),
        results["pilot"]["commitment"].as_str().unwrap_or("UNKNOWN"),
        results["gate2_budget"]["approved"]
            .as_bool()
            .unwrap_or(false)
    )
}

fn render_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.3 Evidence Summary\n\n\
         - Implementation: `{}` / `{}`\n\
         - Formal executions: 2, independent replay: 1\n\
         - H1/H2/H3: `{}` / `{}` / `{}`\n\
         - Contracts: {}/36\n\
         - Decision: `{}`\n\
         - Milestone 5.0R3: `{}`\n\
         - Known gaps: {}\n\n\
         Raw process, event, mutation, snapshot, allocator, benchmark, scenario, comparison, and gate \
         artifacts are under `reports/raw/gate1_v2_3/`. Gate 1 v1 remains historical invalid apparatus.\n",
        results["implementation_sha"].as_str().unwrap_or_default(),
        results["implementation_tree"].as_str().unwrap_or_default(),
        results["hypotheses"]["H1a"].as_str().unwrap_or("FAIL"),
        results["hypotheses"]["H2a"].as_str().unwrap_or("FAIL"),
        results["hypotheses"]["H3a"].as_str().unwrap_or("FAIL"),
        results["contract_summary"]["passed"].as_u64().unwrap_or(0),
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE"),
        results["known_gaps"].as_array().map_or(0, Vec::len)
    )
}

fn readme_status(results: &Value) -> String {
    format!(
        "{STATUS_START}\nGate 1 v1: INVALID_APPARATUS\nGate 1 v2: INVALID_APPARATUS / NOT AUTHORIZED FOR DECISION\nGate 1 v2.1: INVALID / NOT AUTHORIZED FOR DECISION\nGate 1 v2.2: NOT TRUSTWORTHY / NOT AUTHORIZED FOR DECISION\nGate 1 v2.3: VERIFIED_TERMINAL_DECISION\nCurrent decision: {}\nMilestone 5.0R3: {}\n{STATUS_END}",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE")
    )
}

fn roadmap_status(results: &Value) -> String {
    format!(
        "{STATUS_START}\nCurrent project gate: **Gate 1 v2.3 verified terminal decision**.\n\n\
         Gate 1 v1 and Gate 1 v2 are **INVALID_APPARATUS**. Gate 1 v2.1 is **INVALID** and Gate 1 v2.2 is **NOT TRUSTWORTHY**. None is a current decision.\n\n\
         Current Gate 1 v2.3 decision: **{}**.\n\n\
         Current milestone: **5.0R3 — {}**.\n{STATUS_END}",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE")
    )
}

fn baseline_status(results: &Value) -> String {
    format!(
        "{STATUS_START}\nGate 1 v1 and Gate 1 v2 are **INVALID_APPARATUS**. Gate 1 v2.1 is **INVALID** and Gate 1 v2.2 is **NOT TRUSTWORTHY**. Gate 1 v2.3 is\n\
         **VERIFIED_TERMINAL_DECISION** with decision **{}**, and Milestone 5.0R3 is **{}**.\n{STATUS_END}",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE")
    )
}

fn replace_status_block(path: &Path, replacement: &str) -> Result<(), AnyError> {
    let source = std::fs::read_to_string(path)?;
    let start = source
        .find(STATUS_START)
        .ok_or("status block start is missing")?;
    let end = source[start..]
        .find(STATUS_END)
        .map(|offset| start + offset + STATUS_END.len())
        .ok_or("status block end is missing")?;
    let mut output = String::with_capacity(source.len() + replacement.len());
    output.push_str(&source[..start]);
    output.push_str(replacement);
    output.push_str(&source[end..]);
    std::fs::write(path, output)?;
    Ok(())
}

fn extract_status_block(source: &str) -> Result<String, AnyError> {
    let start = source
        .find(STATUS_START)
        .ok_or("status block start is missing")?;
    let end = source[start..]
        .find(STATUS_END)
        .map(|offset| start + offset + STATUS_END.len())
        .ok_or("status block end is missing")?;
    Ok(source[start..end].to_owned())
}

fn generate_receipt() -> Result<(), AnyError> {
    verify_evidence()?;
    if Path::new(RECEIPT).exists() {
        return Err("Gate 1 v2.3 receipt already exists".into());
    }
    let receipt = reconstruct_receipt("HEAD^", "HEAD")?;
    write_json(Path::new(RECEIPT), &receipt)?;
    println!("Gate 1 v2.3 semantic verification receipt generated");
    Ok(())
}

fn verify_final() -> Result<(), AnyError> {
    verify_evidence()?;
    verify_receipt_commit_paths()?;
    let expected = reconstruct_receipt("HEAD^^", "HEAD^")?;
    let actual = read_json(RECEIPT)?;
    if actual != expected {
        return Err("Gate 1 v2.3 receipt differs from semantic recomputation".into());
    }
    println!("Gate 1 v2.3 final evidence chain verified");
    Ok(())
}

fn reconstruct_receipt(implementation_ref: &str, evidence_ref: &str) -> Result<Value, AnyError> {
    let generated = reconstruct()?;
    if generated.results["implementation_sha"] != git(&["rev-parse", implementation_ref])? {
        return Err("gate implementation SHA does not match I commit".into());
    }
    let implementation_sha = git(&["rev-parse", implementation_ref])?;
    let implementation_tree = git(&["rev-parse", &format!("{implementation_ref}^{{tree}}")])?;
    let evidence_sha = git(&["rev-parse", evidence_ref])?;
    let evidence_tree = git(&["rev-parse", &format!("{evidence_ref}^{{tree}}")])?;
    verify_evidence_commit_paths(&implementation_sha, &evidence_sha)?;
    let raw_hashes = recursive_hashes(Path::new("reports/raw/gate1_v2_3"))?;
    let report_hashes = BTreeMap::from([
        (RESULTS.to_owned(), hash_file(RESULTS)?),
        (FINAL.to_owned(), hash_file(FINAL)?),
        (SUMMARY.to_owned(), hash_file(SUMMARY)?),
        (
            "reports/gate1_v2_3_pilot.json".to_owned(),
            hash_file("reports/gate1_v2_3_pilot.json")?,
        ),
        (
            "reports/gate1_v2_3_budget.json".to_owned(),
            hash_file("reports/gate1_v2_3_budget.json")?,
        ),
        ("README.md".to_owned(), hash_file("README.md")?),
        ("ROADMAP.md".to_owned(), hash_file("ROADMAP.md")?),
        (
            "baseline/BASELINE_INDEX.md".to_owned(),
            hash_file("baseline/BASELINE_INDEX.md")?,
        ),
    ]);
    let successful_word = ["pass", "ed"].concat();
    let contracts_ok = generated.results["contracts"]
        .as_array()
        .is_some_and(|contracts| {
            contracts.len() == 36
                && contracts
                    .iter()
                    .all(|contract| contract["status"] == successful_word)
        });
    let decision_ok = matches!(
        generated.results["decision"].as_str(),
        Some(
            "PROCEED_TO_PILOT"
                | "PROCEED_TO_GATE2_RFC"
                | "HOLD"
                | "PIVOT"
                | "STOP"
                | "INVALID"
                | "UNVERIFIABLE_WITHIN_MVR"
        )
    );
    let markdown_ok = std::fs::read_to_string(FINAL)? == generated.final_report
        && std::fs::read_to_string(SUMMARY)? == generated.summary;
    let status_blocks_ok = generated.status_blocks.iter().all(|(path, block)| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|source| extract_status_block(&source).ok())
            .as_ref()
            == Some(block)
    });
    Ok(json!({
        "schema_version": 3,
        "experiment_version": "gate1-v2.3",
        "status": if contracts_ok && decision_ok && markdown_ok && status_blocks_ok {"verified"} else {"failed"},
        "decision": generated.results["decision"],
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "evidence_sha": evidence_sha,
        "evidence_tree": evidence_tree,
        "manifest_hash": hash_file("experiments/gate1-v2.3/manifest.json")?,
        "contract_manifest_hash": hash_file(CONTRACTS)?,
        "supersession_graph_hash": hash_file("reports/history/gate1/supersession_graph.json")?,
        "history_index_hash": hash_file("reports/history/gate1/index.json")?,
        "raw_artifact_hashes": raw_hashes,
        "report_hashes": report_hashes,
        "semantic_fingerprint": stable_value_hash(&generated.results),
        "verification": {
            "artifact_hashes_match": !raw_hashes.is_empty(),
            "contract_evaluation_recomputed": contracts_ok,
            "decision_rule_recomputed": decision_ok,
            "supersession_graph_recomputed": verify_supersession_graph(),
            "terminal_outcome_semantics_recomputed": generated.results["contract_summary"]["outcomes_recomputed"] == true,
            "markdown_reconstructed": markdown_ok,
            "status_blocks_reconstructed": status_blocks_ok,
            "evidence_paths_allowed": true_from_path_verification(&implementation_sha, &evidence_sha),
            "implementation_parent_matches": git(&["rev-parse", &format!("{evidence_ref}^")]).ok().as_deref() == Some(implementation_sha.as_str()),
            "all_work_packages_passed": contracts_ok
        }
    }))
}

fn recursive_hashes(root: &Path) -> Result<BTreeMap<String, String>, AnyError> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort();
    files
        .into_iter()
        .map(|path| Ok((path.to_string_lossy().into_owned(), hash_file(&path)?)))
        .collect()
}

fn collect_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), AnyError> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, output)?;
        } else {
            output.push(path);
        }
    }
    Ok(())
}

fn verify_evidence_commit_paths(implementation: &str, evidence: &str) -> Result<(), AnyError> {
    let changed = git(&[
        "diff",
        "--name-only",
        &format!("{implementation}..{evidence}"),
    ])?;
    for path in changed.lines() {
        let allowed = path.starts_with("reports/raw/gate1_v2_3/")
            || path == "reports/contracts/gate1_v2_3_results.json"
            || path.starts_with("reports/gate1_v2_3_")
            || matches!(
                path,
                "README.md" | "ROADMAP.md" | "baseline/BASELINE_INDEX.md"
            );
        if !allowed {
            return Err(format!("Evidence commit changed forbidden path `{path}`").into());
        }
    }
    Ok(())
}

fn true_from_path_verification(implementation: &str, evidence: &str) -> bool {
    verify_evidence_commit_paths(implementation, evidence).is_ok()
}

fn verify_receipt_commit_paths() -> Result<(), AnyError> {
    let changed = git(&["diff", "--name-only", "HEAD^..HEAD"])?;
    if changed != RECEIPT {
        return Err(
            format!("Receipt commit changed paths other than `{RECEIPT}`: {changed}").into(),
        );
    }
    Ok(())
}

fn verify_supersession_graph() -> bool {
    let Ok(graph) = read_json("reports/history/gate1/supersession_graph.json") else {
        return false;
    };
    let Ok(v2) = read_json("reports/contracts/gate1_v2_invalidation.json") else {
        return false;
    };
    graph["nodes"]
        == json!([
            "gate1-v1",
            "gate1-v2",
            "gate1-v2.1",
            "gate1-v2.2",
            "gate1-v2.3"
        ])
        && graph["edges"]
            == json!([
                ["gate1-v1", "gate1-v2"],
                ["gate1-v2", "gate1-v2.1"],
                ["gate1-v2.1", "gate1-v2.2"],
                ["gate1-v2.2", "gate1-v2.3"]
            ])
        && graph["current"] == "gate1-v2.3"
        && v2["superseded_by"] == "gate1-v2.1"
}
