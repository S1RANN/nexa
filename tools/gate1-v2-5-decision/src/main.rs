#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_5::{
    AnyError, git, hash_file, read_json, repository_root, stable_value_hash, write_json,
};
use nexa_gate1_v2_5_gates::{GATE_NAMES, RAW_ROOT, gate_hashes, generate_from_raw};
use serde::Deserialize;
use serde_json::{Value, json};

const CONTRACTS: &str = "reports/contracts/gate1_v2_5_contracts.json";
const EVALUATION: &str = "reports/contracts/gate1_v2_5_contract_evaluation.json";
const PENDING_RESULTS: &str = "reports/contracts/gate1_v2_5_results_pending_finalization.json";
const PENDING_DECISION: &str = "reports/gate1_v2_5_decision_pending_finalization.md";
const PENDING_SUMMARY: &str = "reports/gate1_v2_5_summary_pending_finalization.md";
const PILOT_REPORT: &str = "reports/gate1_v2_5_pilot.json";
const BUDGET_REPORT: &str = "reports/gate1_v2_5_budget.json";
const FINAL_RESULTS: &str = "reports/contracts/gate1_v2_5_results.json";
const RECEIPT: &str = "reports/contracts/gate1_v2_5_verification_receipt.json";
const FINAL_DECISION: &str = "reports/gate1_v2_5_final_decision.md";
const FINAL_SUMMARY: &str = "reports/gate1_v2_5_summary.md";
const GATES: &str = "reports/raw/gate1_v2_5/gates";
const CURRENT_STATUS: &str = "reports/history/gate1/current_status.json";
const STATUS_START: &str = "<!-- gate1-v2.5-status:start -->";
const STATUS_END: &str = "<!-- gate1-v2.5-status:end -->";

const D_PATHS: [&str; 6] = [
    EVALUATION,
    PENDING_RESULTS,
    PENDING_DECISION,
    PENDING_SUMMARY,
    PILOT_REPORT,
    BUDGET_REPORT,
];

const F_PATHS: [&str; 8] = [
    FINAL_RESULTS,
    RECEIPT,
    FINAL_DECISION,
    FINAL_SUMMARY,
    CURRENT_STATUS,
    "README.md",
    "ROADMAP.md",
    "baseline/BASELINE_INDEX.md",
];

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
}

#[derive(Clone, Debug, Deserialize)]
struct Assertion {
    pointer: String,
    operator: String,
    expected: Value,
}

#[derive(Clone)]
struct Reconstruction {
    evaluations: Vec<Value>,
    gaps: Vec<String>,
    product_outcomes: Value,
    product_decision: Option<String>,
    implementation_sha: String,
    implementation_tree: String,
}

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "decision-state-check" => decision_state_check(),
        [command] if command == "generate-pending" => generate_pending(),
        [command] if command == "verify-pending" => verify_pending(),
        [command] if command == "finalize" => finalize(),
        [command] if command == "generate-receipt" => verify_candidate_receipt(),
        [command] if command == "verify-final" => verify_final(),
        [command] if command == "status-lint" => status_lint(),
        _ => Err("usage: nexa-gate1-v2-5-decision decision-state-check|generate-pending|verify-pending|finalize|generate-receipt|verify-final|status-lint".into()),
    }
}

fn decision_state_check() -> Result<(), AnyError> {
    let cases = [
        (
            "all-pass-no-pilot",
            outcomes("PASS", "PASS", "PASS"),
            false,
            false,
            "HOLD",
        ),
        (
            "h1-fail",
            outcomes("FAIL", "PASS", "PASS"),
            false,
            false,
            "STOP",
        ),
        (
            "h2-fail",
            outcomes("PASS", "FAIL", "PASS"),
            false,
            false,
            "STOP",
        ),
        (
            "h3-fail",
            outcomes("PASS", "PASS", "FAIL"),
            false,
            false,
            "STOP",
        ),
        (
            "all-fail",
            outcomes("FAIL", "FAIL", "FAIL"),
            false,
            false,
            "STOP",
        ),
        (
            "invalid",
            outcomes("INVALID", "PASS", "PASS"),
            false,
            false,
            "INVALID",
        ),
        (
            "pilot",
            outcomes("PASS", "PASS", "PASS"),
            true,
            false,
            "PROCEED_TO_PILOT",
        ),
        (
            "gate2",
            outcomes("PASS", "PASS", "PASS"),
            true,
            true,
            "PROCEED_TO_GATE2_RFC",
        ),
    ];
    let mut records = Vec::new();
    for (name, product_outcomes, pilot, budget, expected) in cases {
        let actual = decide_product(&product_outcomes, pilot, budget, 0)?;
        records.push(json!({
            "case": name,
            "expected": expected,
            "actual": actual,
            "matched": actual == expected
        }));
    }
    let structural = final_state(false, Some("STOP"), 47, false);
    let receipt_missing = final_state(true, Some("STOP"), 48, false);
    let final_verified = final_state(true, Some("STOP"), 48, true);
    records.extend([
        json!({
            "case": "contract-47-of-48",
            "matched": structural["gate1_status"] == "NOT_TRUSTWORTHY"
                && structural["milestone_status"] == "INCOMPLETE"
        }),
        json!({
            "case": "receipt-missing",
            "matched": receipt_missing["gate1_status"] == "DECISION_COMPUTED_PENDING_FINALIZATION"
                && receipt_missing["milestone_status"] == "INCOMPLETE"
        }),
        json!({
            "case": "verified",
            "matched": final_verified["gate1_status"] == "VERIFIED_TERMINAL_DECISION"
                && final_verified["milestone_status"] == "COMPLETE"
        }),
    ]);
    let status = if records.iter().all(|record| record["matched"] == true) {
        "PASS"
    } else {
        "FAIL"
    };
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.5",
        "status": status,
        "cases": records
    });
    write_json(
        Path::new("target/gate1-v2.5-dryrun/decision_state_check.json"),
        &result,
    )?;
    if status != "PASS" {
        return Err("decision state regression failed".into());
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn generate_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.5", validate_e_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    if !reconstruction.gaps.is_empty() {
        return Err(format!(
            "cannot compute a product decision with structural gaps: {:?}",
            reconstruction.gaps
        )
        .into());
    }
    let decision = reconstruction
        .product_decision
        .clone()
        .ok_or("product decision was not computed")?;
    let evaluation = evaluation_document(&reconstruction, false);
    let results = pending_results(&reconstruction, &decision);
    write_json(Path::new(EVALUATION), &evaluation)?;
    write_json(Path::new(PENDING_RESULTS), &results)?;
    std::fs::write(PENDING_DECISION, render_pending_decision(&results))?;
    std::fs::write(PENDING_SUMMARY, render_pending_summary(&results))?;
    write_json(
        Path::new(PILOT_REPORT),
        &read_json(Path::new(GATES).join("pilot.json"))?,
    )?;
    write_json(
        Path::new(BUDGET_REPORT),
        &read_json(Path::new(GATES).join("budget.json"))?,
    )?;
    verify_pending_files(&reconstruction)?;
    println!("Gate 1 v2.5 pending decision: {decision}");
    Ok(())
}

fn verify_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.5", validate_e_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    verify_pending_files(&reconstruction)?;
    println!("Gate 1 v2.5 D2.5 candidate files verified");
    Ok(())
}

fn finalize() -> Result<(), AnyError> {
    ensure_head_phase("D2.5", validate_d_paths)?;
    let pending = read_json(PENDING_RESULTS)?;
    if pending["known_structural_gaps"]
        .as_array()
        .is_none_or(|gaps| !gaps.is_empty())
        || pending["contracts_satisfied"] != 47
        || pending["wp_048_status"] != "PENDING_FINALIZATION"
    {
        return Err("D2.5 pending result is not finalizable".into());
    }
    let reconstruction = reconstruct(Path::new(GATES), true)?;
    if !reconstruction.gaps.is_empty() {
        return Err("final reconstruction contains structural gaps".into());
    }
    let decision = reconstruction
        .product_decision
        .clone()
        .ok_or("final product decision is missing")?;
    let results = final_results(&reconstruction, &decision);
    let final_report = render_final_decision(&results);
    let summary = render_final_summary(&results);
    let current = final_current_status(&decision);
    let document_blocks = final_document_blocks(&decision);
    let roadmap_rows = final_roadmap_rows(&decision);
    write_json(Path::new(FINAL_RESULTS), &results)?;
    std::fs::write(FINAL_DECISION, &final_report)?;
    std::fs::write(FINAL_SUMMARY, &summary)?;
    write_json(Path::new(CURRENT_STATUS), &current)?;
    apply_documents(&document_blocks, &roadmap_rows)?;
    let receipt = build_receipt(
        &results,
        &final_report,
        &summary,
        &current,
        &document_blocks,
    )?;
    write_json(Path::new(RECEIPT), &receipt)?;
    verify_candidate_files(&reconstruction)?;
    println!("Gate 1 v2.5 complete F2.5 candidate tree generated");
    Ok(())
}

fn verify_candidate_receipt() -> Result<(), AnyError> {
    ensure_head_phase("D2.5", validate_d_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), true)?;
    verify_candidate_files(&reconstruction)?;
    println!("Gate 1 v2.5 candidate Receipt and F tree verified");
    Ok(())
}

fn verify_final() -> Result<(), AnyError> {
    let head = git(&["rev-parse", "HEAD"])?;
    let parent = git(&["rev-parse", "HEAD^"])?;
    let receipt = read_json(RECEIPT)?;
    if receipt["status"] != "CANDIDATE_VERIFIED"
        || receipt["expected_f_parent"] != parent
        || receipt["decision_commit"] != parent
    {
        return Err("F2.5 Receipt does not bind its actual parent".into());
    }
    let paths = commit_paths(&head)?;
    if paths
        != F_PATHS
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    {
        return Err(format!("F2.5 path set is invalid: {paths:?}").into());
    }
    let d = parent;
    let e = git(&["rev-parse", &format!("{d}^")])?;
    let i = git(&["rev-parse", &format!("{e}^")])?;
    if receipt["implementation_commit"] != i
        || receipt["evidence_commit"] != e
        || receipt["decision_commit"] != d
    {
        return Err("F2.5 does not close the bound I/E/D chain".into());
    }
    validate_e_paths(&e)?;
    validate_d_paths(&d)?;
    let temporary = receipt_temp_directory();
    reset_temp(&temporary)?;
    let regenerated = temporary.join("gates");
    generate_from_raw(Path::new(RAW_ROOT), &regenerated)?;
    compare_gate_bytes(Path::new(GATES), &regenerated)?;
    std::fs::remove_dir_all(&temporary)?;
    let reconstruction = reconstruct(Path::new(GATES), true)?;
    verify_candidate_files(&reconstruction)?;
    status_lint()?;
    println!("Gate 1 v2.5 F2.5 and I/E/D/F topology: VERIFIED");
    Ok(())
}

fn reconstruct(gates: &Path, final_phase: bool) -> Result<Reconstruction, AnyError> {
    let manifest: Manifest = serde_json::from_value(read_json(CONTRACTS)?)?;
    let work_packages = manifest
        .contracts
        .iter()
        .map(|contract| contract.work_package)
        .collect::<BTreeSet<_>>();
    if manifest.contracts.len() != 48 || work_packages != (1_u32..=48).collect() {
        return Err("contract manifest must cover WP1-WP48 exactly once".into());
    }
    let mut evaluations = Vec::new();
    let mut gaps = Vec::new();
    for contract in &manifest.contracts {
        if contract.work_package == 48 {
            evaluations.push(json!({
                "id": contract.id,
                "work_package": 48,
                "description": contract.description,
                "contract_type": contract.kind,
                "gate": contract.gate,
                "artifact": contract.artifact,
                "checks": [{
                    "pointer": "/candidate_finalization",
                    "expected": true,
                    "actual": final_phase,
                    "satisfied": final_phase
                }],
                "status": if final_phase {"SATISFIED"} else {"PENDING_FINALIZATION"}
            }));
            continue;
        }
        let artifact = read_json(gates.join(&contract.artifact))?;
        let checks = contract
            .assertions
            .iter()
            .map(|assertion| evaluate_assertion(&artifact, assertion))
            .collect::<Vec<_>>();
        let satisfied = checks.iter().all(|check| check["satisfied"] == true);
        if !satisfied {
            gaps.push(format!("{} assertions were not satisfied", contract.id));
        }
        evaluations.push(json!({
            "id": contract.id,
            "work_package": contract.work_package,
            "description": contract.description,
            "contract_type": contract.kind,
            "gate": contract.gate,
            "artifact": contract.artifact,
            "checks": checks,
            "status": if satisfied {"SATISFIED"} else {"UNSATISFIED"}
        }));
    }
    let gate_manifest = read_json(gates.join("manifest.json"))?;
    if gate_manifest["gate_count"] != 21 {
        gaps.push("Gate manifest does not contain 21 Gates".to_owned());
    }
    for name in GATE_NAMES {
        let gate = read_json(gates.join(format!("{name}.json")))?;
        if gate["contract_status"] != "PASS" {
            gaps.push(format!("{name} Gate contract status is not PASS"));
        }
    }
    let product_outcomes = aggregate_product_outcomes(gates)?;
    let comparison_inconclusive_count = ["comparison", "replay"]
        .into_iter()
        .filter(|name| {
            read_json(gates.join(format!("{name}.json")))
                .is_ok_and(|gate| gate["outcome"] == "INCONCLUSIVE")
        })
        .count();
    let pilot = read_json(gates.join("pilot.json"))?["metrics"]["committed"] == true;
    let budget = read_json(gates.join("budget.json"))?["metrics"]["approved"] == true;
    let product_decision = if gaps.is_empty() {
        Some(decide_product(
            &product_outcomes,
            pilot,
            budget,
            comparison_inconclusive_count,
        )?)
    } else {
        None
    };
    Ok(Reconstruction {
        evaluations,
        gaps,
        product_outcomes,
        product_decision,
        implementation_sha: gate_manifest["implementation_sha"]
            .as_str()
            .ok_or("Gate manifest implementation SHA is missing")?
            .to_owned(),
        implementation_tree: gate_manifest["implementation_tree"]
            .as_str()
            .ok_or("Gate manifest implementation tree is missing")?
            .to_owned(),
    })
}

fn evaluate_assertion(artifact: &Value, assertion: &Assertion) -> Value {
    let actual = artifact
        .pointer(&assertion.pointer)
        .cloned()
        .unwrap_or(Value::Null);
    let satisfied = match assertion.operator.as_str() {
        "eq" | "eq_if_run" => actual == assertion.expected,
        "in" => assertion
            .expected
            .as_array()
            .is_some_and(|values| values.contains(&actual)),
        _ => false,
    };
    json!({
        "pointer": assertion.pointer,
        "operator": assertion.operator,
        "expected": assertion.expected,
        "actual": actual,
        "satisfied": satisfied
    })
}

fn aggregate_product_outcomes(gates: &Path) -> Result<Value, AnyError> {
    Ok(json!({
        "H1": aggregate_outcome(gates, &["h1_equivalence", "h1_metrics"])?,
        "H2": aggregate_outcome(gates, &[
            "h2_configuration", "h2_cleanup", "h2_invariants",
            "h2_allocations", "h2_performance"
        ])?,
        "H3": aggregate_outcome(gates, &[
            "h3_migration", "h3_completion", "h3_transaction"
        ])?
    }))
}

fn aggregate_outcome(gates: &Path, names: &[&str]) -> Result<&'static str, AnyError> {
    let outcomes = names
        .iter()
        .map(|name| {
            read_json(gates.join(format!("{name}.json")))?
                .get("outcome")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{name} Gate outcome is missing").into())
        })
        .collect::<Result<Vec<String>, AnyError>>()?;
    Ok(if outcomes.iter().any(|value| value == "INVALID") {
        "INVALID"
    } else if outcomes.iter().any(|value| value == "INCONCLUSIVE") {
        "INCONCLUSIVE"
    } else if outcomes.iter().any(|value| value == "FAIL") {
        "FAIL"
    } else if outcomes.iter().all(|value| value == "PASS") {
        "PASS"
    } else {
        "NOT_RUN"
    })
}

fn decide_product(
    product_outcomes: &Value,
    pilot: bool,
    budget: bool,
    comparison_inconclusive_count: usize,
) -> Result<String, AnyError> {
    let values = ["H1", "H2", "H3"]
        .map(|name| {
            product_outcomes[name]
                .as_str()
                .ok_or_else(|| format!("{name} product outcome is missing"))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    if values.contains(&"INVALID") || values.contains(&"NOT_RUN") {
        return Ok("INVALID".to_owned());
    }
    if values.contains(&"INCONCLUSIVE") || comparison_inconclusive_count >= 2 {
        return Ok("UNVERIFIABLE_WITHIN_MVR".to_owned());
    }
    if values.contains(&"FAIL") {
        return Ok("STOP".to_owned());
    }
    Ok(if !pilot {
        "HOLD"
    } else if budget {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    }
    .to_owned())
}

fn evaluation_document(reconstruction: &Reconstruction, final_phase: bool) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.5",
        "phase": if final_phase {"F2.5"} else {"D2.5"},
        "contract_count": 48,
        "contracts_satisfied": reconstruction.evaluations.iter()
            .filter(|contract| contract["status"] == "SATISFIED").count(),
        "contracts": reconstruction.evaluations,
        "known_structural_gaps": reconstruction.gaps
    })
}

fn pending_results(reconstruction: &Reconstruction, decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.5",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 48,
        "contracts_satisfied": 47,
        "wp_048_status": "PENDING_FINALIZATION",
        "known_structural_gaps": reconstruction.gaps,
        "apparatus_status": "PASS",
        "evidence_status": "DECISION_COMPUTED",
        "product_outcomes": reconstruction.product_outcomes,
        "product_decision": decision,
        "finalization_status": "PENDING",
        "gate1_status": "DECISION_COMPUTED_PENDING_FINALIZATION",
        "milestone_status": "INCOMPLETE",
        "receipt_verified": false
    })
}

fn final_results(reconstruction: &Reconstruction, decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.5",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 48,
        "contracts_satisfied": 48,
        "wp_048_status": "PASS",
        "known_structural_gaps": [],
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "product_outcomes": reconstruction.product_outcomes,
        "product_decision": decision,
        "finalization_status": "VERIFIED",
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "milestone_status": "COMPLETE",
        "receipt_verified": true,
        "push_status": "AUTHORIZED"
    })
}

fn final_state(
    structurally_valid: bool,
    decision: Option<&str>,
    contracts_satisfied: usize,
    receipt_verified: bool,
) -> Value {
    if !structurally_valid {
        return json!({
            "gate1_status": "NOT_TRUSTWORTHY",
            "milestone_status": "INCOMPLETE",
            "finalization_status": "FAILED"
        });
    }
    if contracts_satisfied < 48 || !receipt_verified {
        return json!({
            "gate1_status": "DECISION_COMPUTED_PENDING_FINALIZATION",
            "milestone_status": "INCOMPLETE",
            "finalization_status": "PENDING"
        });
    }
    json!({
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "milestone_status": "COMPLETE",
        "finalization_status": "VERIFIED",
        "decision": decision
    })
}

fn outcomes(h1: &str, h2: &str, h3: &str) -> Value {
    json!({"H1": h1, "H2": h2, "H3": h3})
}

fn render_pending_decision(results: &Value) -> String {
    format!(
        "# Gate 1 v2.5 Decision Pending Finalization\n\nProduct decision: **{}**\n\nGate 1 status: **DECISION_COMPUTED_PENDING_FINALIZATION**\n\nMilestone 5.0R5: **INCOMPLETE**\n\nWP-048 remains pending until the F2.5 candidate is complete and committed.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_pending_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.5 Pending Summary\n\nThe product decision is **{}**. 47/48 contracts are satisfied; finalization and Receipt verification remain pending.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_decision(results: &Value) -> String {
    format!(
        "# Gate 1 v2.5 Final Decision\n\nProduct decision: **{}**\n\nMilestone 5.0R5: **COMPLETE**\n\n- Gates regenerated from Raw Run: 21\n- Contracts satisfied: 48/48\n- Known structural gaps: 0\n- Receipt verified: true\n- Push status: AUTHORIZED\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.5 Summary\n\nThe verified terminal product decision is **{}**. All 48 contracts are satisfied, the 21 Gates and comparisons are reproducible from Raw Run evidence, and no structural gap is known.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn final_current_status(decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "current_experiment": "gate1-v2.5",
        "experiment_status": "VERIFIED_TERMINAL_DECISION",
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "decision_state": "FINAL",
        "decision": decision,
        "milestone": "5.0R5",
        "milestone_status": "COMPLETE",
        "receipt_verified": true,
        "push_status": "AUTHORIZED"
    })
}

fn final_document_blocks(decision: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "README.md",
            format!(
                "{STATUS_START}\nGate 1 v2.4: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION\nGate 1 v2.5: VERIFIED_TERMINAL_DECISION\nCurrent decision: {decision}\nMilestone 5.0R5: COMPLETE\nPush: AUTHORIZED\n{STATUS_END}"
            ),
        ),
        (
            "ROADMAP.md",
            format!(
                "{STATUS_START}\nCurrent project gate: **Gate 1 v2.5 verified terminal decision**.\n\nGate 1 v2.4 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable.\n\nCurrent Gate 1 v2.5 decision: **{decision}**.\n\nCurrent milestone: **5.0R5 — COMPLETE**.\n{STATUS_END}"
            ),
        ),
        (
            "baseline/BASELINE_INDEX.md",
            format!(
                "{STATUS_START}\nGate 1 v2.4 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable. Gate 1 v2.5 is **VERIFIED_TERMINAL_DECISION**, its decision is **{decision}**, and Milestone 5.0R5 is **COMPLETE**.\n{STATUS_END}"
            ),
        ),
    ])
}

fn final_roadmap_rows(decision: &str) -> [String; 2] {
    [
        "| Gate 1 v2.5 | Stable semantic projection and explicit finalization | I/E/D/F chain and Raw-derived Receipt | Verified terminal decision |".to_owned(),
        format!(
            "| Gate 1 v2.5 Decision | Valid v2.5 evidence and 48/48 contracts | Verified F2.5 and empty structural gaps | {decision} |"
        ),
    ]
}

fn build_receipt(
    results: &Value,
    final_report: &str,
    summary: &str,
    current: &Value,
    blocks: &BTreeMap<&'static str, String>,
) -> Result<Value, AnyError> {
    let d = git(&["rev-parse", "HEAD"])?;
    let e = git(&["rev-parse", "HEAD^"])?;
    let i = git(&["rev-parse", "HEAD^^"])?;
    validate_d_paths(&d)?;
    validate_e_paths(&e)?;
    let temporary = receipt_temp_directory();
    reset_temp(&temporary)?;
    let regenerated = temporary.join("gates");
    generate_from_raw(Path::new(RAW_ROOT), &regenerated)?;
    let comparison = compare_gate_bytes(Path::new(GATES), &regenerated)?;
    let reconstruction = reconstruct(&regenerated, true)?;
    if !reconstruction.gaps.is_empty()
        || reconstruction.product_decision.as_deref() != results["product_decision"].as_str()
    {
        return Err("Receipt decision reconstruction differs from final candidate".into());
    }
    let receipt = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.5",
        "status": "CANDIDATE_VERIFIED",
        "topology": "I2.5 -> E2.5 -> D2.5 -> F2.5",
        "implementation_commit": i,
        "implementation_tree": git(&["rev-parse", &format!("{i}^{{tree}}")])?,
        "evidence_commit": e,
        "evidence_tree": git(&["rev-parse", &format!("{e}^{{tree}}")])?,
        "decision_commit": d,
        "decision_tree": git(&["rev-parse", &format!("{d}^{{tree}}")])?,
        "expected_f_parent": d,
        "f_candidate_paths": F_PATHS,
        "gates_regenerated_from_raw": true,
        "gate_byte_comparison": comparison,
        "gate_hashes": gate_hashes(Path::new(GATES))?,
        "contracts_recomputed": reconstruction.evaluations.iter().all(|value| value["status"] == "SATISFIED"),
        "contract_count": 48,
        "decision_recomputed": true,
        "reports_recomputed": true,
        "artifact_hygiene_verified": read_json(Path::new(GATES).join("artifact_hygiene.json"))?["contract_status"] == "PASS",
        "known_structural_gaps": reconstruction.gaps,
        "frozen_manifest_hash": hash_file("experiments/gate1-v2.5/manifest.json")?,
        "raw_manifest_hash": hash_file(Path::new(GATES).join("manifest.json"))?,
        "contract_manifest_hash": hash_file(CONTRACTS)?,
        "contract_evaluation_hash": hash_file(EVALUATION)?,
        "decision_hash": stable_value_hash(&results["product_decision"]),
        "final_results_hash": stable_value_hash(results),
        "final_report_hash": stable_value_hash(&json!(final_report)),
        "summary_hash": stable_value_hash(&json!(summary)),
        "current_status_hash": stable_value_hash(current),
        "status_document_hashes": blocks.iter().map(|(path, block)| ((*path).to_owned(), stable_value_hash(&json!(block)))).collect::<BTreeMap<_,_>>()
    });
    std::fs::remove_dir_all(temporary)?;
    Ok(receipt)
}

fn verify_pending_files(reconstruction: &Reconstruction) -> Result<(), AnyError> {
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("pending decision is missing")?;
    let evaluation = evaluation_document(reconstruction, false);
    let results = pending_results(reconstruction, decision);
    if read_json(EVALUATION)? != evaluation
        || read_json(PENDING_RESULTS)? != results
        || std::fs::read_to_string(PENDING_DECISION)? != render_pending_decision(&results)
        || std::fs::read_to_string(PENDING_SUMMARY)? != render_pending_summary(&results)
        || read_json(PILOT_REPORT)? != read_json(Path::new(GATES).join("pilot.json"))?
        || read_json(BUDGET_REPORT)? != read_json(Path::new(GATES).join("budget.json"))?
    {
        return Err("D2.5 candidate files differ from reconstruction".into());
    }
    Ok(())
}

fn verify_candidate_files(reconstruction: &Reconstruction) -> Result<(), AnyError> {
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("final product decision is missing")?;
    let results = final_results(reconstruction, decision);
    let blocks = final_document_blocks(decision);
    let rows = final_roadmap_rows(decision);
    if read_json(FINAL_RESULTS)? != results
        || std::fs::read_to_string(FINAL_DECISION)? != render_final_decision(&results)
        || std::fs::read_to_string(FINAL_SUMMARY)? != render_final_summary(&results)
        || read_json(CURRENT_STATUS)? != final_current_status(decision)
    {
        return Err("F2.5 candidate files differ from reconstruction".into());
    }
    for (path, block) in &blocks {
        if extract_block(&std::fs::read_to_string(path)?)? != *block {
            return Err(format!("{path} final status block differs").into());
        }
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    if !roadmap.contains(&rows[0]) || !roadmap.contains(&rows[1]) {
        return Err("ROADMAP final rows differ".into());
    }
    let receipt = read_json(RECEIPT)?;
    if receipt["status"] != "CANDIDATE_VERIFIED"
        || receipt["contracts_recomputed"] != true
        || receipt["decision_recomputed"] != true
        || receipt["reports_recomputed"] != true
        || receipt["known_structural_gaps"]
            .as_array()
            .is_none_or(|gaps| !gaps.is_empty())
    {
        return Err("F2.5 candidate Receipt is incomplete".into());
    }
    Ok(())
}

fn apply_documents(
    blocks: &BTreeMap<&'static str, String>,
    rows: &[String; 2],
) -> Result<(), AnyError> {
    for (path, block) in blocks {
        replace_block(Path::new(path), block)?;
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.5 |", &rows[0])?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.5 Decision |", &rows[1])?;
    std::fs::write("ROADMAP.md", roadmap)?;
    Ok(())
}

fn status_lint() -> Result<(), AnyError> {
    let status = read_json(CURRENT_STATUS)?;
    if status["current_experiment"] != "gate1-v2.5" {
        return Err("current status does not point at gate1-v2.5".into());
    }
    if status["experiment_status"] == "VERIFIED_TERMINAL_DECISION" {
        if status["milestone_status"] != "COMPLETE"
            || status["receipt_verified"] != true
            || status["decision"].is_null()
        {
            return Err("VERIFIED state is missing completion prerequisites".into());
        }
        let decision = status["decision"]
            .as_str()
            .ok_or("verified decision is missing")?;
        for (path, block) in final_document_blocks(decision) {
            if extract_block(&std::fs::read_to_string(path)?)? != block {
                return Err(format!("{path} conflicts with verified current status").into());
            }
        }
    } else if status["milestone_status"] == "COMPLETE" || status["receipt_verified"] == true {
        return Err("non-final status claims completion or verified Receipt".into());
    }
    println!("Gate 1 v2.5 status lint: PASS");
    Ok(())
}

fn ensure_head_phase(
    name: &str,
    validator: fn(&str) -> Result<(), AnyError>,
) -> Result<(), AnyError> {
    let head = git(&["rev-parse", "HEAD"])?;
    validator(&head).map_err(|error| format!("{name} validation failed: {error}").into())
}

fn validate_e_paths(commit: &str) -> Result<(), AnyError> {
    let paths = commit_paths(commit)?;
    if paths.is_empty()
        || paths
            .iter()
            .any(|path| !path.starts_with("reports/raw/gate1_v2_5/"))
    {
        return Err(format!("E2.5 path set is invalid: {paths:?}").into());
    }
    Ok(())
}

fn validate_d_paths(commit: &str) -> Result<(), AnyError> {
    let paths = commit_paths(commit)?;
    let expected = D_PATHS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if paths != expected {
        return Err(format!("D2.5 path set is invalid: {paths:?}").into());
    }
    Ok(())
}

fn commit_paths(commit: &str) -> Result<BTreeSet<String>, AnyError> {
    Ok(git(&[
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "-r",
        commit,
    ])?
    .lines()
    .filter(|path| !path.is_empty())
    .map(str::to_owned)
    .collect())
}

fn compare_gate_bytes(recorded: &Path, regenerated: &Path) -> Result<Value, AnyError> {
    let mut comparisons = BTreeMap::new();
    for name in GATE_NAMES.into_iter().chain(std::iter::once("manifest")) {
        let file = format!("{name}.json");
        let left = std::fs::read(recorded.join(&file))?;
        let right = std::fs::read(regenerated.join(&file))?;
        if left != right {
            return Err(format!("regenerated Gate {file} differs byte-for-byte").into());
        }
        comparisons.insert(
            file,
            json!({
                "bytes": left.len(),
                "hash": hash_file(recorded.join(format!("{name}.json")))?
            }),
        );
    }
    Ok(json!({"status": "PASS", "files": comparisons}))
}

fn receipt_temp_directory() -> PathBuf {
    Path::new("target").join(format!(
        "gate1-v2.5-receipt-regeneration-{}",
        std::process::id()
    ))
}

fn reset_temp(path: &Path) -> Result<(), AnyError> {
    if path.exists() {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn replace_block(path: &Path, replacement: &str) -> Result<(), AnyError> {
    let source = std::fs::read_to_string(path)?;
    let start = source
        .find(STATUS_START)
        .ok_or("v2.5 status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("v2.5 status block end is missing")?
        + STATUS_END.len();
    let mut output = String::with_capacity(source.len() + replacement.len());
    output.push_str(&source[..start]);
    output.push_str(replacement);
    output.push_str(&source[end..]);
    std::fs::write(path, output)?;
    Ok(())
}

fn extract_block(source: &str) -> Result<String, AnyError> {
    let start = source
        .find(STATUS_START)
        .ok_or("v2.5 status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("v2.5 status block end is missing")?
        + STATUS_END.len();
    Ok(source[start..end].to_owned())
}

fn replace_table_row(source: &str, prefix: &str, replacement: &str) -> Result<String, AnyError> {
    let mut matched = 0;
    let output = source
        .lines()
        .map(|line| {
            if line.starts_with(prefix) {
                matched += 1;
                replacement
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if matched != 1 {
        return Err(format!("ROADMAP row {prefix} matched {matched} times").into());
    }
    Ok(format!("{output}\n"))
}
