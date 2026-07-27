#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_6::{
    AnyError, DecisionInputs, git, hash_file, product_decision, read_json, repository_root,
    stable_bytes_hash, stable_value_hash, write_json,
};
use nexa_gate1_v2_6_gates::{GATE_NAMES, RAW_ROOT, gate_hashes, generate_from_raw};
use serde::Deserialize;
use serde_json::{Value, json};

const CONTRACTS: &str = "reports/contracts/gate1_v2_6_contracts.json";
const EVALUATION: &str = "reports/contracts/gate1_v2_6_contract_evaluation.json";
const PENDING_RESULTS: &str = "reports/contracts/gate1_v2_6_results_pending_receipt.json";
const PENDING_DECISION: &str = "reports/gate1_v2_6_decision_pending_receipt.md";
const PENDING_SUMMARY: &str = "reports/gate1_v2_6_summary_pending_receipt.md";
const PILOT_REPORT: &str = "reports/gate1_v2_6_pilot.json";
const BUDGET_REPORT: &str = "reports/gate1_v2_6_budget.json";
const FINAL_RESULTS: &str = "reports/contracts/gate1_v2_6_results.json";
const FINALIZATION: &str = "reports/contracts/gate1_v2_6_finalization.json";
const RECEIPT: &str = "reports/contracts/gate1_v2_6_verification_receipt.json";
const FINAL_DECISION: &str = "reports/gate1_v2_6_final_decision.md";
const FINAL_SUMMARY: &str = "reports/gate1_v2_6_summary.md";
const GATES: &str = "reports/raw/gate1_v2_6/gates";
const CURRENT_STATUS: &str = "reports/history/gate1/current_status.json";
const STATUS_START: &str = "<!-- gate1-v2.6-status:start -->";
const STATUS_END: &str = "<!-- gate1-v2.6-status:end -->";

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
    FINALIZATION,
    FINAL_DECISION,
    FINAL_SUMMARY,
    CURRENT_STATUS,
    "README.md",
    "ROADMAP.md",
    "baseline/BASELINE_INDEX.md",
];

const R_PATHS: [&str; 1] = [RECEIPT];

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
    comparison_outcomes: Value,
    stable_core_failures: Vec<String>,
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
        [command] if command == "generate-receipt" => generate_receipt(),
        [command] if command == "verify-receipt" => verify_receipt(),
        [command] if command == "verify-final" => verify_final(),
        [command] if command == "status-lint" => status_lint(),
        _ => Err("usage: nexa-gate1-v2-6-decision decision-state-check|generate-pending|verify-pending|generate-receipt|verify-receipt|finalize|verify-final|status-lint".into()),
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
        let stable = ["H1", "H2", "H3"]
            .into_iter()
            .any(|name| product_outcomes[name] == "FAIL");
        let actual = decide_product(
            &product_outcomes,
            pilot,
            budget,
            ["PASS", "PASS"],
            stable,
            false,
        )?;
        records.push(json!({
            "case": name,
            "expected": expected,
            "actual": actual,
            "matched": actual == expected
        }));
    }
    let structural = final_state(false, Some("STOP"), 49, false);
    let receipt_missing = final_state(true, Some("STOP"), 49, false);
    let final_verified = final_state(true, Some("STOP"), 50, true);
    records.extend([
        json!({
            "case": "contract-49-of-50",
            "matched": structural["gate1_status"] == "NOT_TRUSTWORTHY"
                && structural["milestone_status"] == "INCOMPLETE"
        }),
        json!({
            "case": "receipt-missing",
            "matched": receipt_missing["gate1_status"] == "DECISION_COMPUTED_PENDING_RECEIPT"
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
        "experiment_version": "gate1-v2.6",
        "status": status,
        "cases": records
    });
    write_json(
        Path::new("target/gate1-v2.6-dryrun/decision_state_check.json"),
        &result,
    )?;
    if status != "PASS" {
        return Err("decision state regression failed".into());
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn generate_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.6", validate_e_paths)?;
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
    println!("Gate 1 v2.6 pending decision: {decision}");
    Ok(())
}

fn verify_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.6", validate_e_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    verify_pending_files(&reconstruction)?;
    println!("Gate 1 v2.6 D2.6 candidate files verified");
    Ok(())
}

fn finalize() -> Result<(), AnyError> {
    ensure_head_phase("R2.6", validate_r_paths)?;
    let pending = read_json(PENDING_RESULTS)?;
    if pending["known_structural_gaps"]
        .as_array()
        .is_none_or(|gaps| !gaps.is_empty())
        || pending["contracts_satisfied"] != 49
        || pending["wp_050_status"] != "PENDING_FINALIZATION"
    {
        return Err("D2.6 pending result is not finalizable after Receipt".into());
    }
    let reconstruction = reconstruct(Path::new(GATES), true)?;
    if !reconstruction.gaps.is_empty() {
        return Err("final reconstruction contains structural gaps".into());
    }
    let decision = reconstruction
        .product_decision
        .clone()
        .ok_or("final product decision is missing")?;
    let files = finalization_files(&reconstruction, &decision)?;
    for (path, bytes) in &files {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)?;
    }
    verify_expected_finalization_hashes(&files, &read_json(RECEIPT)?)?;
    verify_candidate_files(&reconstruction)?;
    println!("Gate 1 v2.6 deterministic F2.6 candidate tree generated from R2.6 Receipt");
    Ok(())
}

fn generate_receipt() -> Result<(), AnyError> {
    ensure_head_phase("D2.6", validate_d_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    if !reconstruction.gaps.is_empty() {
        return Err("Receipt reconstruction contains structural gaps".into());
    }
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("Receipt product decision is missing")?;
    let files = finalization_files(&reconstruction, decision)?;
    let receipt = build_receipt(&reconstruction, &files)?;
    write_json(Path::new(RECEIPT), &receipt)?;
    verify_receipt_candidate(&reconstruction, &receipt, &files)?;
    println!("Gate 1 v2.6 R2.6 Receipt candidate generated");
    Ok(())
}

fn verify_receipt() -> Result<(), AnyError> {
    ensure_head_phase("D2.6", validate_d_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("Receipt product decision is missing")?;
    let files = finalization_files(&reconstruction, decision)?;
    let receipt = read_json(RECEIPT)?;
    verify_receipt_candidate(&reconstruction, &receipt, &files)?;
    println!("Gate 1 v2.6 R2.6 Receipt candidate verified");
    Ok(())
}

fn verify_final() -> Result<(), AnyError> {
    let head = git(&["rev-parse", "HEAD"])?;
    let r = git(&["rev-parse", "HEAD^"])?;
    let d = git(&["rev-parse", "HEAD^^"])?;
    let e = git(&["rev-parse", "HEAD^^^"])?;
    let i = git(&["rev-parse", "HEAD^^^^"])?;
    let receipt = read_json(RECEIPT)?;
    if receipt["status"] != "RECEIPT_VERIFIED_PENDING_F"
        || receipt["decision_commit"] != d
        || receipt["evidence_commit"] != e
        || receipt["implementation_commit"] != i
    {
        return Err("F2.6 Receipt does not bind the actual I/E/D chain".into());
    }
    let paths = commit_paths(&head)?;
    if paths
        != F_PATHS
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    {
        return Err(format!("F2.6 path set is invalid: {paths:?}").into());
    }
    validate_r_paths(&r)?;
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
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("final product decision is missing")?;
    let files = finalization_files(&reconstruction, decision)?;
    verify_expected_finalization_hashes(&files, &receipt)?;
    status_lint()?;
    if !git(&["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty() {
        return Err("verify-final requires a clean worktree".into());
    }
    println!("Gate 1 v2.6 F2.6 and I/E/D/R/F topology: VERIFIED");
    Ok(())
}

fn reconstruct(gates: &Path, final_phase: bool) -> Result<Reconstruction, AnyError> {
    let manifest: Manifest = serde_json::from_value(read_json(CONTRACTS)?)?;
    let work_packages = manifest
        .contracts
        .iter()
        .map(|contract| contract.work_package)
        .collect::<BTreeSet<_>>();
    if manifest.contracts.len() != 50 || work_packages != (1_u32..=50).collect() {
        return Err("contract manifest must cover WP1-WP50 exactly once".into());
    }
    let mut evaluations = Vec::new();
    let mut gaps = Vec::new();
    for contract in &manifest.contracts {
        if contract.work_package == 50 {
            evaluations.push(json!({
                "id": contract.id,
                "work_package": 50,
                "description": contract.description,
                "contract_type": contract.kind,
                "gate": contract.gate,
                "artifact": contract.artifact,
                "checks": [{
                    "pointer": "/candidate_finalization",
                    "operator": "eq",
                    "expected": true,
                    "actual": final_phase,
                    "satisfied": final_phase,
                    "reason": if final_phase {"F candidate is deterministically derivable"} else {"WP50 requires committed I/E/D/R/F topology"}
                }],
                "observed_outcome": "NOT_APPLICABLE",
                "contract_status": if final_phase {"PASS"} else {"PENDING_FINALIZATION"},
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
            "observed_outcome": artifact["outcome"],
            "contract_status": if satisfied {"PASS"} else {"FAIL"},
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
    let formal_comparison = read_json(gates.join("comparison.json"))?;
    let replay_comparison = read_json(gates.join("replay.json"))?;
    let comparison_outcome_values = [
        formal_comparison["outcome"]
            .as_str()
            .ok_or("formal comparison outcome is missing")?,
        replay_comparison["outcome"]
            .as_str()
            .ok_or("replay comparison outcome is missing")?,
    ];
    let stable_core_failures = stable_core_failures(gates, &formal_comparison, &replay_comparison)?;
    let pivot_approved = structured_pivot_approval(&stable_core_failures)?;
    let pilot_committed = read_json(gates.join("pilot.json"))?["metrics"]["committed"] == true;
    let budget = read_json(gates.join("budget.json"))?["metrics"]["approved"] == true;
    let product_decision = if gaps.is_empty() {
        Some(decide_product(
            &product_outcomes,
            pilot_committed,
            budget,
            comparison_outcome_values,
            !stable_core_failures.is_empty(),
            pivot_approved,
        )?)
    } else {
        None
    };
    Ok(Reconstruction {
        evaluations,
        gaps,
        product_outcomes,
        comparison_outcomes: json!({
            "formal": comparison_outcome_values[0],
            "replay": comparison_outcome_values[1]
        }),
        stable_core_failures,
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
        "satisfied": satisfied,
        "reason": if satisfied {"assertion matched"} else {"assertion did not match"}
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
    } else if outcomes.iter().any(|value| value == "FAIL") {
        "FAIL"
    } else if outcomes.iter().any(|value| value == "INCONCLUSIVE") {
        "INCONCLUSIVE"
    } else if outcomes.iter().all(|value| value == "PASS") {
        "PASS"
    } else {
        "NOT_RUN"
    })
}

#[allow(clippy::fn_params_excessive_bools)]
fn decide_product(
    product_outcomes: &Value,
    pilot: bool,
    budget: bool,
    comparison_outcomes: [&str; 2],
    stable_core_failure: bool,
    structured_pivot_approved: bool,
) -> Result<String, AnyError> {
    let values = ["H1", "H2", "H3"]
        .map(|name| {
            product_outcomes[name]
                .as_str()
                .ok_or_else(|| format!("{name} product outcome is missing"))
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    Ok(product_decision(&DecisionInputs {
        apparatus_status: "PASS",
        hypothesis_outcomes: [values[0], values[1], values[2]],
        comparison_outcomes,
        stable_core_failure,
        structured_pivot_approved,
        pilot_committed: pilot,
        gate2_budget_approved: budget,
    })
    .to_owned())
}

fn stable_core_failures(
    gates: &Path,
    formal: &Value,
    replay: &Value,
) -> Result<Vec<String>, AnyError> {
    let mut stable = Vec::new();
    for (hypothesis, gate_name) in [
        ("H1", "h1_equivalence"),
        ("H2", "h2_configuration"),
        ("H3", "h3_migration"),
    ] {
        let gate = read_json(gates.join(format!("{gate_name}.json")))?;
        let outcomes = gate
            .pointer("/metrics/recorded_outcomes")
            .or_else(|| gate.pointer("/metrics/derived_outcomes"))
            .and_then(Value::as_array)
            .ok_or_else(|| format!("{gate_name} does not expose three run outcomes"))?;
        let all_fail = outcomes.len() == 3 && outcomes.iter().all(|outcome| outcome == "FAIL");
        let signatures = gate
            .pointer("/metrics/measurements/semantic_signatures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let signatures_stable = signatures.len() == 3
            && signatures
                .first()
                .is_some_and(|first| signatures.iter().all(|value| value == first));
        let semantic_comparison_pass = match hypothesis {
            "H1" => {
                formal.pointer("/metrics/comparison/h1/status") == Some(&json!("PASS"))
                    && replay.pointer("/metrics/comparison/h1/status") == Some(&json!("PASS"))
            }
            "H2" => {
                formal.pointer("/components/semantic") == Some(&json!("PASS"))
                    && replay.pointer("/components/semantic") == Some(&json!("PASS"))
                    && formal.pointer("/components/allocation") == Some(&json!("PASS"))
                    && replay.pointer("/components/allocation") == Some(&json!("PASS"))
            }
            "H3" => {
                formal.pointer("/metrics/comparison/h3/status") == Some(&json!("PASS"))
                    && replay.pointer("/metrics/comparison/h3/status") == Some(&json!("PASS"))
            }
            _ => false,
        };
        if all_fail && signatures_stable && semantic_comparison_pass {
            stable.push(hypothesis.to_owned());
        }
    }
    Ok(stable)
}

fn structured_pivot_approval(failed_hypotheses: &[String]) -> Result<bool, AnyError> {
    let path = Path::new("reports/gate1_v2_6_pivot_approval.json");
    if !path.exists() {
        return Ok(false);
    }
    let approval = read_json(path)?;
    Ok(approval["approved"] == true
        && approval["approved_by"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && approval["approved_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && approval["pivot_scope"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && approval["reason"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
        && approval["failed_hypotheses"]
            .as_array()
            .is_some_and(|values| {
                failed_hypotheses
                    .iter()
                    .all(|hypothesis| values.contains(&json!(hypothesis)))
            }))
}

fn evaluation_document(reconstruction: &Reconstruction, final_phase: bool) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "phase": if final_phase {"F2.6_CANDIDATE"} else {"D2.6"},
        "contract_count": 50,
        "contracts_satisfied": reconstruction.evaluations.iter()
            .filter(|contract| contract["contract_status"] == "PASS").count(),
        "contracts": reconstruction.evaluations,
        "known_structural_gaps": reconstruction.gaps
    })
}

fn pending_results(reconstruction: &Reconstruction, decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 50,
        "contracts_satisfied": 49,
        "wp_050_status": "PENDING_FINALIZATION",
        "known_structural_gaps": reconstruction.gaps,
        "apparatus_status": "PASS",
        "evidence_status": "DECISION_COMPUTED",
        "product_outcomes": reconstruction.product_outcomes,
        "comparison_outcomes": reconstruction.comparison_outcomes,
        "stable_core_failures": reconstruction.stable_core_failures,
        "product_decision": decision,
        "finalization_status": "DECISION_COMPUTED",
        "gate1_status": "DECISION_COMPUTED_PENDING_RECEIPT",
        "milestone_status": "INCOMPLETE",
        "receipt_verified": false,
        "finalization_verified": false
    })
}

fn final_results(reconstruction: &Reconstruction, decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 50,
        "contracts_satisfied": 50,
        "wp_050_status": "PASS",
        "known_structural_gaps": [],
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "product_outcomes": reconstruction.product_outcomes,
        "comparison_outcomes": reconstruction.comparison_outcomes,
        "stable_core_failures": reconstruction.stable_core_failures,
        "product_decision": decision,
        "finalization_status": "FINALIZED",
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "milestone_status": "COMPLETE",
        "receipt_verified": true,
        "finalization_verified": true,
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
    if contracts_satisfied < 50 || !receipt_verified {
        return json!({
            "gate1_status": "DECISION_COMPUTED_PENDING_RECEIPT",
            "milestone_status": "INCOMPLETE",
            "finalization_status": "DECISION_COMPUTED"
        });
    }
    json!({
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "milestone_status": "COMPLETE",
        "finalization_status": "FINALIZED",
        "decision": decision
    })
}

fn outcomes(h1: &str, h2: &str, h3: &str) -> Value {
    json!({"H1": h1, "H2": h2, "H3": h3})
}

fn render_pending_decision(results: &Value) -> String {
    format!(
        "# Gate 1 v2.6 Decision Pending Receipt\n\nProduct decision: **{}**\n\nGate 1 status: **DECISION_COMPUTED_PENDING_RECEIPT**\n\nMilestone 5.0R6: **INCOMPLETE**\n\nWP-050 remains PENDING_FINALIZATION until the Receipt is committed and F2.6 matches its eight expected hashes.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_pending_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.6 Pending Receipt Summary\n\nThe product decision is **{}**. 49/50 contracts are satisfied; Receipt verification and deterministic finalization remain pending.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_decision(results: &Value) -> String {
    format!(
        "# Gate 1 v2.6 Final Decision\n\nProduct decision: **{}**\n\nMilestone 5.0R6: **COMPLETE**\n\n- Gates regenerated from Raw Run: 21\n- Contracts satisfied: 50/50\n- Known structural gaps: 0\n- Receipt verified: true\n- Push status: AUTHORIZED\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.6 Summary\n\nThe verified terminal product decision is **{}**. All 50 contracts are satisfied, the 21 Gates and comparisons are reproducible from Raw Run evidence, and no structural gap is known.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn final_current_status(decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "current_experiment": "gate1-v2.6",
        "experiment_status": "VERIFIED_TERMINAL_DECISION",
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "product_decision": decision,
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "finalization_status": "FINALIZED",
        "milestone": "5.0R6",
        "milestone_status": "COMPLETE",
        "receipt_verified": true,
        "finalization_verified": true,
        "push_status": "AUTHORIZED"
    })
}

fn final_document_blocks(decision: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "README.md",
            format!(
                "{STATUS_START}\nGate 1 v2.4: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION\nGate 1 v2.5: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION\nGate 1 v2.6: VERIFIED_TERMINAL_DECISION\nCurrent decision: {decision}\nMilestone 5.0R6: COMPLETE\nPush: AUTHORIZED\n{STATUS_END}"
            ),
        ),
        (
            "ROADMAP.md",
            format!(
                "{STATUS_START}\nCurrent project gate: **Gate 1 v2.6 verified terminal decision**.\n\nGate 1 v2.5 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable.\n\nCurrent Gate 1 v2.6 decision: **{decision}**.\n\nCurrent milestone: **5.0R6 — COMPLETE**.\n{STATUS_END}"
            ),
        ),
        (
            "baseline/BASELINE_INDEX.md",
            format!(
                "{STATUS_START}\nGate 1 v2.5 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable. Gate 1 v2.6 is **VERIFIED_TERMINAL_DECISION**, its decision is **{decision}**, and Milestone 5.0R6 is **COMPLETE**.\n{STATUS_END}"
            ),
        ),
    ])
}

fn final_roadmap_rows(decision: &str) -> [String; 2] {
    [
        "| Gate 1 v2.6 | Stable semantic projection and explicit finalization | I/E/D/R/F chain and Raw-derived Receipt | Verified terminal decision |".to_owned(),
        format!(
            "| Gate 1 v2.6 Decision | Valid v2.6 evidence and 50/50 contracts | Verified I/E/D/R/F and empty structural gaps | {decision} |"
        ),
    ]
}

fn build_receipt(
    expected_reconstruction: &Reconstruction,
    files: &BTreeMap<String, Vec<u8>>,
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
    let reconstruction = reconstruct(&regenerated, false)?;
    if !reconstruction.gaps.is_empty()
        || reconstruction.product_decision != expected_reconstruction.product_decision
    {
        return Err("Receipt decision reconstruction differs from final candidate".into());
    }
    let expected_finalization_files = files
        .iter()
        .map(|(path, bytes)| (path.clone(), stable_bytes_hash(bytes)))
        .collect::<BTreeMap<_, _>>();
    let receipt = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "status": "RECEIPT_VERIFIED_PENDING_F",
        "topology": "I2.6 -> E2.6 -> D2.6 -> R2.6 -> F2.6",
        "implementation_commit": i,
        "implementation_tree": git(&["rev-parse", &format!("{i}^{{tree}}")])?,
        "evidence_commit": e,
        "evidence_tree": git(&["rev-parse", &format!("{e}^{{tree}}")])?,
        "decision_commit": d,
        "decision_tree": git(&["rev-parse", &format!("{d}^{{tree}}")])?,
        "expected_f_parent": "R2.6_RECEIPT_COMMIT",
        "f_candidate_paths": F_PATHS,
        "expected_finalization_files": expected_finalization_files,
        "gates_regenerated_from_raw": true,
        "gate_byte_comparison": comparison,
        "gate_hashes": gate_hashes(Path::new(GATES))?,
        "comparisons_recomputed": {
            "formal": reconstruction.comparison_outcomes["formal"],
            "replay": reconstruction.comparison_outcomes["replay"]
        },
        "contracts_recomputed": reconstruction.evaluations.iter()
            .filter(|value| value["work_package"] != 50)
            .all(|value| value["contract_status"] == "PASS"),
        "contract_count": 50,
        "contracts_satisfied": 49,
        "wp_050_status": "PENDING_FINALIZATION",
        "decision_recomputed": true,
        "product_decision": reconstruction.product_decision,
        "stable_core_failures": reconstruction.stable_core_failures,
        "reports_recomputed": true,
        "artifact_hygiene_verified": read_json(Path::new(GATES).join("artifact_hygiene.json"))?["contract_status"] == "PASS",
        "known_structural_gaps": reconstruction.gaps,
        "frozen_manifest_hash": hash_file("experiments/gate1-v2.6/manifest.json")?,
        "raw_hash_manifest_hash": hash_file(Path::new(RAW_ROOT).join("raw_hash_manifest.json"))?,
        "gate_manifest_hash": hash_file(Path::new(GATES).join("manifest.json"))?,
        "contract_manifest_hash": hash_file(CONTRACTS)?,
        "contract_evaluation_hash": hash_file(EVALUATION)?,
        "decision_hash": stable_value_hash(&json!(reconstruction.product_decision))
    });
    std::fs::remove_dir_all(temporary)?;
    Ok(receipt)
}

fn finalization_files(
    reconstruction: &Reconstruction,
    decision: &str,
) -> Result<BTreeMap<String, Vec<u8>>, AnyError> {
    let results = final_results(reconstruction, decision);
    let finalization = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "topology": "I2.6 -> E2.6 -> D2.6 -> R2.6 -> F2.6",
        "contract_count": 50,
        "contracts_satisfied": 50,
        "contracts_failed": 0,
        "contracts_pending": 0,
        "wp_050_status": "PASS",
        "known_structural_gaps": [],
        "receipt_verified": true,
        "finalization_verified": true,
        "status": "FINALIZED"
    });
    let final_report = render_final_decision(&results);
    let summary = render_final_summary(&results);
    let current = final_current_status(decision);
    let blocks = final_document_blocks(decision);
    let rows = final_roadmap_rows(decision);
    let readme = replace_block_text(&std::fs::read_to_string("README.md")?, &blocks["README.md"])?;
    let roadmap = replace_block_text(
        &std::fs::read_to_string("ROADMAP.md")?,
        &blocks["ROADMAP.md"],
    )?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.6 |", &rows[0])?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.6 Decision |", &rows[1])?;
    let baseline = replace_block_text(
        &std::fs::read_to_string("baseline/BASELINE_INDEX.md")?,
        &blocks["baseline/BASELINE_INDEX.md"],
    )?;
    Ok(BTreeMap::from([
        (FINAL_RESULTS.to_owned(), json_bytes(&results)?),
        (FINALIZATION.to_owned(), json_bytes(&finalization)?),
        (FINAL_DECISION.to_owned(), final_report.into_bytes()),
        (FINAL_SUMMARY.to_owned(), summary.into_bytes()),
        (CURRENT_STATUS.to_owned(), json_bytes(&current)?),
        ("README.md".to_owned(), readme.into_bytes()),
        ("ROADMAP.md".to_owned(), roadmap.into_bytes()),
        (
            "baseline/BASELINE_INDEX.md".to_owned(),
            baseline.into_bytes(),
        ),
    ]))
}

fn json_bytes(value: &Value) -> Result<Vec<u8>, AnyError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn verify_expected_finalization_hashes(
    files: &BTreeMap<String, Vec<u8>>,
    receipt: &Value,
) -> Result<(), AnyError> {
    let expected = receipt["expected_finalization_files"]
        .as_object()
        .ok_or("Receipt expected_finalization_files is missing")?;
    if expected.len() != F_PATHS.len() || files.len() != F_PATHS.len() {
        return Err("Receipt does not bind exactly eight finalization files".into());
    }
    for (path, bytes) in files {
        if expected[path] != stable_bytes_hash(bytes) {
            return Err(format!("{path} differs from the Receipt expected hash").into());
        }
    }
    Ok(())
}

fn verify_receipt_candidate(
    reconstruction: &Reconstruction,
    receipt: &Value,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), AnyError> {
    if receipt["status"] != "RECEIPT_VERIFIED_PENDING_F"
        || receipt["topology"] != "I2.6 -> E2.6 -> D2.6 -> R2.6 -> F2.6"
        || receipt["contracts_recomputed"] != true
        || receipt["contracts_satisfied"] != 49
        || receipt["wp_050_status"] != "PENDING_FINALIZATION"
        || receipt["decision_recomputed"] != true
        || receipt["product_decision"] != json!(reconstruction.product_decision)
        || receipt["known_structural_gaps"]
            .as_array()
            .is_none_or(|gaps| !gaps.is_empty())
    {
        return Err("R2.6 Receipt candidate is incomplete".into());
    }
    verify_expected_finalization_hashes(files, receipt)
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
        return Err("D2.6 candidate files differ from reconstruction".into());
    }
    Ok(())
}

fn verify_candidate_files(reconstruction: &Reconstruction) -> Result<(), AnyError> {
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("final product decision is missing")?;
    let files = finalization_files(reconstruction, decision)?;
    for (path, expected) in &files {
        if std::fs::read(path)? != *expected {
            return Err(format!("F2.6 file {path} differs from reconstruction").into());
        }
    }
    let receipt = read_json(RECEIPT)?;
    verify_receipt_candidate(reconstruction, &receipt, &files)
}

#[allow(dead_code)]
fn apply_documents(
    blocks: &BTreeMap<&'static str, String>,
    rows: &[String; 2],
) -> Result<(), AnyError> {
    for (path, block) in blocks {
        replace_block(Path::new(path), block)?;
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.6 |", &rows[0])?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.6 Decision |", &rows[1])?;
    std::fs::write("ROADMAP.md", roadmap)?;
    Ok(())
}

fn status_lint() -> Result<(), AnyError> {
    let status = read_json(CURRENT_STATUS)?;
    if status["current_experiment"] != "gate1-v2.6" {
        return Err("current status does not point at gate1-v2.6".into());
    }
    if status["experiment_status"] == "VERIFIED_TERMINAL_DECISION" {
        if status["milestone_status"] != "COMPLETE"
            || status["receipt_verified"] != true
            || status["finalization_verified"] != true
            || status["product_decision"].is_null()
        {
            return Err("VERIFIED state is missing completion prerequisites".into());
        }
        let decision = status["product_decision"]
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
    println!("Gate 1 v2.6 status lint: PASS");
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
            .any(|path| !path.starts_with("reports/raw/gate1_v2_6/"))
    {
        return Err(format!("E2.6 path set is invalid: {paths:?}").into());
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
        return Err(format!("D2.6 path set is invalid: {paths:?}").into());
    }
    Ok(())
}

fn validate_r_paths(commit: &str) -> Result<(), AnyError> {
    let paths = commit_paths(commit)?;
    let expected = R_PATHS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if paths != expected {
        return Err(format!("R2.6 path set is invalid: {paths:?}").into());
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
        "gate1-v2.6-receipt-regeneration-{}",
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
    std::fs::write(path, replace_block_text(&source, replacement)?)?;
    Ok(())
}

fn replace_block_text(source: &str, replacement: &str) -> Result<String, AnyError> {
    let start = source
        .find(STATUS_START)
        .ok_or("v2.6 status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("v2.6 status block end is missing")?
        + STATUS_END.len();
    let mut output = String::with_capacity(source.len() + replacement.len());
    output.push_str(&source[..start]);
    output.push_str(replacement);
    output.push_str(&source[end..]);
    Ok(output)
}

fn extract_block(source: &str) -> Result<String, AnyError> {
    let start = source
        .find(STATUS_START)
        .ok_or("v2.6 status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("v2.6 status block end is missing")?
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
