#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_9::{
    AnyError, DecisionInputs, git, hash_file, product_decision, read_json, repository_root,
    stable_bytes_hash, stable_value_hash, write_json,
};
use nexa_gate1_v2_9_gates::{GATE_NAMES, RAW_ROOT, gate_hashes, generate_from_raw};
use serde::Deserialize;
use serde_json::{Value, json};

const CONTRACTS: &str = "reports/contracts/gate1_v2_9_contracts.json";
const EVALUATION: &str = "reports/contracts/gate1_v2_9_contract_evaluation.json";
const PENDING_RESULTS: &str = "reports/contracts/gate1_v2_9_results_pending_receipt.json";
const PENDING_DECISION: &str = "reports/gate1_v2_9_decision_pending_receipt.md";
const PENDING_SUMMARY: &str = "reports/gate1_v2_9_summary_pending_receipt.md";
const PILOT_REPORT: &str = "reports/gate1_v2_9_pilot.json";
const BUDGET_REPORT: &str = "reports/gate1_v2_9_budget.json";
const FINAL_RESULTS: &str = "reports/contracts/gate1_v2_9_results.json";
const FINALIZATION: &str = "reports/contracts/gate1_v2_9_finalization.json";
const RECEIPT: &str = "reports/contracts/gate1_v2_9_verification_receipt.json";
const FINAL_DECISION: &str = "reports/gate1_v2_9_final_decision.md";
const FINAL_SUMMARY: &str = "reports/gate1_v2_9_summary.md";
const GATES: &str = "reports/raw/gate1_v2_9/gates";
const CURRENT_STATUS: &str = "reports/history/gate1/current_status.json";
const STATUS_START: &str = "<!-- gate1-v2.9-status:start -->";
const STATUS_END: &str = "<!-- gate1-v2.9-status:end -->";
const PREVIOUS_STATUS_START: &str = "<!-- gate1-v2.8-status:start -->";
const PREVIOUS_STATUS_END: &str = "<!-- gate1-v2.8-status:end -->";

const H1_GATES: [&str; 2] = ["h1_equivalence", "h1_metrics"];
const H2_GATES: [&str; 5] = [
    "h2_configuration",
    "h2_cleanup",
    "h2_invariants",
    "h2_allocations",
    "h2_performance",
];
const H3_GATES: [&str; 3] = ["h3_migration", "h3_completion", "h3_transaction"];

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
    per_run_product_outcomes: Value,
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
        _ => Err("usage: nexa-gate1-v2-9-decision decision-state-check|generate-pending|verify-pending|generate-receipt|verify-receipt|finalize|verify-final|status-lint".into()),
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
        stable_failure_extraction_regression()?,
    ]);
    let status = if records.iter().all(|record| record["matched"] == true) {
        "PASS"
    } else {
        "FAIL"
    };
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.9",
        "status": status,
        "cases": records
    });
    write_json(
        Path::new("target/gate1-v2.9-dryrun/decision_state_check.json"),
        &result,
    )?;
    if status != "PASS" {
        return Err("decision state regression failed".into());
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn stable_failure_extraction_regression() -> Result<Value, AnyError> {
    let root = Path::new("target/gate1-v2.9-dryrun/stable-failure-regression");
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root)?;
    let pass = ["PASS", "PASS", "PASS"];
    let fail = ["FAIL", "FAIL", "FAIL"];
    for (name, outcomes) in [
        ("h1_equivalence", pass),
        ("h1_metrics", fail),
        ("h2_configuration", pass),
        ("h2_cleanup", fail),
        ("h2_invariants", pass),
        ("h2_allocations", pass),
        ("h2_performance", pass),
        ("h3_migration", pass),
        ("h3_completion", fail),
        ("h3_transaction", fail),
    ] {
        let signature = match name {
            "h1_equivalence" => Some("h1-stable"),
            "h2_configuration" => Some("h2-stable"),
            "h3_migration" => Some("h3-stable"),
            _ => None,
        };
        write_json(
            &root.join(format!("{name}.json")),
            &json!({
                "outcome": outcomes[0],
                "contract_status": "PASS",
                "metrics": {
                    "derived_outcomes": outcomes,
                    "recorded_outcomes": outcomes,
                    "direct_contract_assertions": [true, true, true],
                    "measurements": {
                        "semantic_signatures": signature
                            .map_or_else(|| json!([]), |value| json!([value, value, value]))
                    }
                }
            }),
        )?;
    }
    write_json(
        &root.join("process.json"),
        &json!({
            "outcome": "PASS",
            "contract_status": "PASS",
            "metrics": {"apparatus_statuses": ["PASS", "PASS", "PASS"]}
        }),
    )?;
    write_json(
        &root.join("validity.json"),
        &json!({
            "outcome": "PASS",
            "contract_status": "PASS",
            "metrics": {
                "preflight_statuses": ["PASS", "PASS", "PASS"],
                "postflight_statuses": ["PASS", "PASS", "PASS"],
                "validity_statuses": ["PASS", "PASS", "PASS"]
            }
        }),
    )?;
    let paired = |hypothesis: &str, signature: &str| {
        json!({
            "status": "PASS",
            "left_outcome": "FAIL",
            "right_outcome": "FAIL",
            "left_signature": signature,
            "right_signature": signature,
            "semantic_equal": true,
            "outcome_equal": true,
            "hypothesis": hypothesis
        })
    };
    let comparison = |right: &str| {
        json!({
            "outcome": "INCONCLUSIVE",
            "components": {
                "semantic": "PASS",
                "allocation": "PASS",
                "performance": "INCONCLUSIVE"
            },
            "metrics": {
                "comparison": {
                    "right": right,
                    "h1": paired("H1", "h1-stable"),
                    "h2": {
                        "semantic": {
                            "status": "PASS",
                            "observations": {
                                "left_signature": "h2-stable",
                                "right_signature": "h2-stable"
                            }
                        }
                    },
                    "h3": paired("H3", "h3-stable")
                }
            }
        })
    };
    let formal = comparison("formal-run-2");
    let replay = comparison("replay");
    let representative_subgate_outcomes = json!({
        "H1": read_json(root.join("h1_equivalence.json"))?["metrics"]["recorded_outcomes"],
        "H2": read_json(root.join("h2_configuration.json"))?["metrics"]["recorded_outcomes"],
        "H3": read_json(root.join("h3_migration.json"))?["metrics"]["recorded_outcomes"]
    });
    let per_run = per_run_product_outcomes(root)?;
    let aggregate = aggregate_product_outcomes(&per_run)?;
    let stable = stable_core_failures(root, &per_run, &formal, &replay)?;
    let decision = decide_product(
        &aggregate,
        false,
        false,
        ["INCONCLUSIVE", "INCONCLUSIVE"],
        !stable.is_empty(),
        false,
    )?;
    let matched = per_run
        == json!({
            "H1": ["FAIL", "FAIL", "FAIL"],
            "H2": ["FAIL", "FAIL", "FAIL"],
            "H3": ["FAIL", "FAIL", "FAIL"]
        })
        && representative_subgate_outcomes
            == json!({
                "H1": ["PASS", "PASS", "PASS"],
                "H2": ["PASS", "PASS", "PASS"],
                "H3": ["PASS", "PASS", "PASS"]
            })
        && stable == ["H1", "H2", "H3"]
        && decision == "STOP";
    std::fs::remove_dir_all(root)?;
    Ok(json!({
        "case": "representative-subgates-pass-aggregate-hypotheses-fail",
        "exercises_stable_failure_extraction": true,
        "representative_subgate_outcomes": representative_subgate_outcomes,
        "per_run_product_outcomes": per_run,
        "stable_core_failures": stable,
        "actual": decision,
        "expected": "STOP",
        "matched": matched
    }))
}

fn generate_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.9", validate_e_paths)?;
    validate_i_paths(&git(&["rev-parse", "HEAD^"])?)?;
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
    println!("Gate 1 v2.9 pending decision: {decision}");
    Ok(())
}

fn verify_pending() -> Result<(), AnyError> {
    ensure_head_phase("E2.9", validate_e_paths)?;
    validate_i_paths(&git(&["rev-parse", "HEAD^"])?)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    verify_pending_files(&reconstruction)?;
    println!("Gate 1 v2.9 D2.9 candidate files verified");
    Ok(())
}

fn finalize() -> Result<(), AnyError> {
    ensure_head_phase("R2.9", validate_r_paths)?;
    let pending = read_json(PENDING_RESULTS)?;
    if pending["known_structural_gaps"]
        .as_array()
        .is_none_or(|gaps| !gaps.is_empty())
        || pending["contracts_satisfied"] != 49
        || pending["wp_050_status"] != "PENDING_FINALIZATION"
    {
        return Err("D2.9 pending result is not finalizable after Receipt".into());
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
    println!("Gate 1 v2.9 deterministic F2.9 candidate tree generated from R2.9 Receipt");
    Ok(())
}

fn generate_receipt() -> Result<(), AnyError> {
    ensure_head_phase("D2.9", validate_d_paths)?;
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
    println!("Gate 1 v2.9 R2.9 Receipt candidate generated");
    Ok(())
}

fn verify_receipt() -> Result<(), AnyError> {
    ensure_head_phase("D2.9", validate_d_paths)?;
    let reconstruction = reconstruct(Path::new(GATES), false)?;
    let decision = reconstruction
        .product_decision
        .as_deref()
        .ok_or("Receipt product decision is missing")?;
    let files = finalization_files(&reconstruction, decision)?;
    let receipt = read_json(RECEIPT)?;
    verify_receipt_candidate(&reconstruction, &receipt, &files)?;
    println!("Gate 1 v2.9 R2.9 Receipt candidate verified");
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
        return Err("F2.9 Receipt does not bind the actual I/E/D chain".into());
    }
    let paths = commit_paths(&head)?;
    if paths
        != F_PATHS
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    {
        return Err(format!("F2.9 path set is invalid: {paths:?}").into());
    }
    validate_r_paths(&r)?;
    validate_e_paths(&e)?;
    validate_d_paths(&d)?;
    validate_i_paths(&i)?;
    verify_phase_paths_disjoint([&i, &e, &d, &r, &head])?;
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
    println!("Gate 1 v2.9 F2.9 and I/E/D/R/F topology: VERIFIED");
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
    let per_run_product_outcomes = per_run_product_outcomes(gates)?;
    let product_outcomes = aggregate_product_outcomes(&per_run_product_outcomes)?;
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
    let stable_core_failures = stable_core_failures(
        gates,
        &per_run_product_outcomes,
        &formal_comparison,
        &replay_comparison,
    )?;
    let pivot_approved = structured_pivot_approval(&stable_core_failures)?;
    let pilot_committed = read_json(gates.join("pilot.json"))?["metrics"]["committed"] == true;
    let budget = read_json(gates.join("budget.json"))?["metrics"]["approved"] == true;
    let computed_decision = decide_product(
        &product_outcomes,
        pilot_committed,
        budget,
        comparison_outcome_values,
        !stable_core_failures.is_empty(),
        pivot_approved,
    )?;
    let formal_decision_evidence =
        read_json(gates.join("governance.json"))?["metrics"]["formal_decision_evidence"].clone();
    if formal_decision_evidence["per_run_product_outcomes"] != per_run_product_outcomes {
        gaps.push(
            "governance Gate per-run product outcomes differ from independent reconstruction"
                .to_owned(),
        );
    }
    if formal_decision_evidence["aggregate_product_outcomes"] != product_outcomes {
        gaps.push(
            "governance Gate aggregate product outcomes differ from independent reconstruction"
                .to_owned(),
        );
    }
    if formal_decision_evidence["stable_core_failures"] != json!(stable_core_failures) {
        gaps.push(
            "governance Gate Stable Core Failures differ from independent reconstruction"
                .to_owned(),
        );
    }
    if formal_decision_evidence["derived_product_decision"] != computed_decision {
        gaps.push(
            "governance Gate product decision differs from independent reconstruction".to_owned(),
        );
    }
    let product_decision = gaps.is_empty().then_some(computed_decision);
    Ok(Reconstruction {
        evaluations,
        gaps,
        per_run_product_outcomes,
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

fn per_run_product_outcomes(gates: &Path) -> Result<Value, AnyError> {
    Ok(json!({
        "H1": aggregate_component_runs(gates, &H1_GATES)?,
        "H2": aggregate_component_runs(gates, &H2_GATES)?,
        "H3": aggregate_component_runs(gates, &H3_GATES)?
    }))
}

fn aggregate_component_runs(gates: &Path, names: &[&str]) -> Result<Vec<String>, AnyError> {
    let components = names
        .iter()
        .map(|name| {
            let gate = read_json(gates.join(format!("{name}.json")))?;
            let recorded = gate
                .pointer("/metrics/recorded_outcomes")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name} recorded outcomes are missing"))?;
            let derived = gate
                .pointer("/metrics/derived_outcomes")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name} derived outcomes are missing"))?;
            let direct = gate
                .pointer("/metrics/direct_contract_assertions")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name} direct assertions are missing"))?;
            if recorded.len() != 3
                || recorded != derived
                || direct.len() != 3
                || direct.iter().any(|value| value != true)
            {
                return Err(format!(
                    "{name} does not expose three equal, directly verified run outcomes"
                )
                .into());
            }
            recorded
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{name} run outcome is not text").into())
                })
                .collect::<Result<Vec<_>, AnyError>>()
        })
        .collect::<Result<Vec<_>, AnyError>>()?;
    (0..3)
        .map(|run| {
            let values = components
                .iter()
                .map(|component| component[run].as_str())
                .collect::<Vec<_>>();
            Ok(aggregate_values(&values).to_owned())
        })
        .collect()
}

fn aggregate_product_outcomes(per_run: &Value) -> Result<Value, AnyError> {
    let mut outcomes = serde_json::Map::new();
    for hypothesis in ["H1", "H2", "H3"] {
        let values = per_run[hypothesis]
            .as_array()
            .ok_or_else(|| format!("{hypothesis} per-run outcomes are missing"))?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| format!("{hypothesis} per-run outcome is not text"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        outcomes.insert(hypothesis.to_owned(), json!(aggregate_values(&values)));
    }
    Ok(Value::Object(outcomes))
}

fn aggregate_values(outcomes: &[&str]) -> &'static str {
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
    per_run_product_outcomes: &Value,
    formal: &Value,
    replay: &Value,
) -> Result<Vec<String>, AnyError> {
    if !apparatus_runs_valid(gates)? {
        return Ok(Vec::new());
    }
    let mut stable = Vec::new();
    for (hypothesis, signature_gate) in [
        ("H1", "h1_equivalence"),
        ("H2", "h2_configuration"),
        ("H3", "h3_migration"),
    ] {
        let outcomes = per_run_product_outcomes[hypothesis]
            .as_array()
            .ok_or_else(|| format!("{hypothesis} does not expose three aggregate run outcomes"))?;
        let all_fail = outcomes.len() == 3 && outcomes.iter().all(|outcome| outcome == "FAIL");
        let gate = read_json(gates.join(format!("{signature_gate}.json")))?;
        let signatures = gate
            .pointer("/metrics/measurements/semantic_signatures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let signatures_stable = signatures.len() == 3
            && signatures
                .first()
                .is_some_and(|first| signatures.iter().all(|value| value == first));
        let comparison_stable = match hypothesis {
            "H1" => paired_hypothesis_comparison(formal, replay, "h1", "FAIL"),
            "H2" => h2_comparison_stable(formal, replay),
            "H3" => paired_hypothesis_comparison(formal, replay, "h3", "FAIL"),
            _ => unreachable!(),
        };
        if all_fail && signatures_stable && comparison_stable {
            stable.push(hypothesis.to_owned());
        }
    }
    Ok(stable)
}

fn apparatus_runs_valid(gates: &Path) -> Result<bool, AnyError> {
    let process = read_json(gates.join("process.json"))?;
    let validity = read_json(gates.join("validity.json"))?;
    let all_three_pass = |pointer: &str, value: &Value| {
        value
            .pointer(pointer)
            .and_then(Value::as_array)
            .is_some_and(|statuses| {
                statuses.len() == 3 && statuses.iter().all(|status| status == "PASS")
            })
    };
    Ok(process["contract_status"] == "PASS"
        && process["outcome"] == "PASS"
        && all_three_pass("/metrics/apparatus_statuses", &process)
        && validity["contract_status"] == "PASS"
        && validity["outcome"] == "PASS"
        && all_three_pass("/metrics/preflight_statuses", &validity)
        && all_three_pass("/metrics/postflight_statuses", &validity)
        && all_three_pass("/metrics/validity_statuses", &validity))
}

fn paired_hypothesis_comparison(
    formal: &Value,
    replay: &Value,
    hypothesis: &str,
    expected_outcome: &str,
) -> bool {
    let pointer = |field: &str| format!("/metrics/comparison/{hypothesis}/{field}");
    let signatures = [
        formal.pointer(&pointer("left_signature")),
        formal.pointer(&pointer("right_signature")),
        replay.pointer(&pointer("left_signature")),
        replay.pointer(&pointer("right_signature")),
    ];
    formal.pointer(&pointer("status")) == Some(&json!("PASS"))
        && replay.pointer(&pointer("status")) == Some(&json!("PASS"))
        && formal.pointer(&pointer("left_outcome")) == Some(&json!(expected_outcome))
        && formal.pointer(&pointer("right_outcome")) == Some(&json!(expected_outcome))
        && replay.pointer(&pointer("left_outcome")) == Some(&json!(expected_outcome))
        && replay.pointer(&pointer("right_outcome")) == Some(&json!(expected_outcome))
        && signatures[0].is_some()
        && signatures
            .iter()
            .all(|signature| *signature == signatures[0])
}

fn h2_comparison_stable(formal: &Value, replay: &Value) -> bool {
    let allowed_performance = |value: Option<&Value>| {
        value == Some(&json!("PASS")) || value == Some(&json!("INCONCLUSIVE"))
    };
    let signatures = [
        formal.pointer("/metrics/comparison/h2/semantic/observations/left_signature"),
        formal.pointer("/metrics/comparison/h2/semantic/observations/right_signature"),
        replay.pointer("/metrics/comparison/h2/semantic/observations/left_signature"),
        replay.pointer("/metrics/comparison/h2/semantic/observations/right_signature"),
    ];
    formal.pointer("/components/semantic") == Some(&json!("PASS"))
        && replay.pointer("/components/semantic") == Some(&json!("PASS"))
        && formal.pointer("/components/allocation") == Some(&json!("PASS"))
        && replay.pointer("/components/allocation") == Some(&json!("PASS"))
        && allowed_performance(formal.pointer("/components/performance"))
        && allowed_performance(replay.pointer("/components/performance"))
        && signatures[0].is_some()
        && signatures
            .iter()
            .all(|signature| *signature == signatures[0])
}

fn structured_pivot_approval(failed_hypotheses: &[String]) -> Result<bool, AnyError> {
    let path = Path::new("reports/gate1_v2_9_pivot_approval.json");
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
        "experiment_version": "gate1-v2.9",
        "phase": if final_phase {"F2.9_CANDIDATE"} else {"D2.9"},
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
        "experiment_version": "gate1-v2.9",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 50,
        "contracts_satisfied": 49,
        "wp_050_status": "PENDING_FINALIZATION",
        "known_structural_gaps": reconstruction.gaps,
        "apparatus_status": "PASS",
        "evidence_status": "DECISION_COMPUTED",
        "per_run_product_outcomes": reconstruction.per_run_product_outcomes,
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
        "experiment_version": "gate1-v2.9",
        "implementation_sha": reconstruction.implementation_sha,
        "implementation_tree": reconstruction.implementation_tree,
        "gate_count": 21,
        "contract_count": 50,
        "contracts_satisfied": 50,
        "wp_050_status": "PASS",
        "known_structural_gaps": [],
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "per_run_product_outcomes": reconstruction.per_run_product_outcomes,
        "product_outcomes": reconstruction.product_outcomes,
        "comparison_outcomes": reconstruction.comparison_outcomes,
        "stable_core_failures": reconstruction.stable_core_failures,
        "per_run_product_outcomes": reconstruction.per_run_product_outcomes,
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
        "# Gate 1 v2.9 Decision Pending Receipt\n\nProduct decision: **{}**\n\nGate 1 status: **DECISION_COMPUTED_PENDING_RECEIPT**\n\nMilestone 5.0R9: **INCOMPLETE**\n\nWP-050 remains PENDING_FINALIZATION until the Receipt is committed and F2.9 matches its eight expected hashes.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_pending_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.9 Pending Receipt Summary\n\nThe product decision is **{}**. 49/50 contracts are satisfied; Receipt verification and deterministic finalization remain pending.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_decision(results: &Value) -> String {
    format!(
        "# Gate 1 v2.9 Final Decision\n\nProduct decision: **{}**\n\nMilestone 5.0R9: **COMPLETE**\n\n- Gates regenerated from Raw Run: 21\n- Contracts satisfied: 50/50\n- Known structural gaps: 0\n- Receipt verified: true\n- Push status: AUTHORIZED\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn render_final_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.9 Summary\n\nThe verified terminal product decision is **{}**. All 50 contracts are satisfied, the 21 Gates and comparisons are reproducible from Raw Run evidence, and no structural gap is known.\n",
        results["product_decision"].as_str().unwrap_or("INVALID")
    )
}

fn final_current_status(decision: &str) -> Value {
    json!({
        "schema_version": 1,
        "current_experiment": "gate1-v2.9",
        "experiment_status": "VERIFIED_TERMINAL_DECISION",
        "apparatus_status": "PASS",
        "evidence_status": "VERIFIED",
        "product_decision": decision,
        "gate1_status": "VERIFIED_TERMINAL_DECISION",
        "finalization_status": "FINALIZED",
        "milestone": "5.0R9",
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
                "{STATUS_START}\nGate 1 v2.4: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION\nGate 1 v2.5: STRUCTURAL_CLOSURE_FAILED / NOT AUTHORIZED FOR DECISION\nGate 1 v2.6: STRUCTURAL_CLOSURE_FAILED / INCOMPLETE / RECORDED STOP NOT AUTHORIZED\nGate 1 v2.7: INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE / NOT AUTHORIZED FOR DECISION\nGate 1 v2.8: SEMANTICALLY_INSUFFICIENT / INCOMPLETE / RECORDED DECISION NOT AUTHORIZED\nGate 1 v2.9: VERIFIED_TERMINAL_DECISION\nCurrent decision: {decision}\nMilestone 5.0R9: COMPLETE\nPush: AUTHORIZED\n{STATUS_END}"
            ),
        ),
        (
            "ROADMAP.md",
            format!(
                "{STATUS_START}\nCurrent project gate: **Gate 1 v2.9 verified terminal decision**.\n\nGate 1 v2.5 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable. Gate 1 v2.6 is **STRUCTURAL_CLOSURE_FAILED / INCOMPLETE**; its recorded STOP is not authorized. Gate 1 v2.7 is **INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE** and not decision-usable. Gate 1 v2.8 is **SEMANTICALLY_INSUFFICIENT / INCOMPLETE** and its recorded decision is not authorized.\n\nCurrent Gate 1 v2.9 decision: **{decision}**.\n\nCurrent milestone: **5.0R9 — COMPLETE**.\n{STATUS_END}"
            ),
        ),
        (
            "baseline/BASELINE_INDEX.md",
            format!(
                "{STATUS_START}\nGate 1 v2.5 is **STRUCTURAL_CLOSURE_FAILED** and not decision-usable. Gate 1 v2.6 is **STRUCTURAL_CLOSURE_FAILED / INCOMPLETE** and its recorded STOP is unauthorized. Gate 1 v2.7 is **INVALID_ENVIRONMENT_EXECUTION / INCOMPLETE** and not decision-usable. Gate 1 v2.8 is **SEMANTICALLY_INSUFFICIENT / INCOMPLETE** and its recorded decision is unauthorized. Gate 1 v2.9 is **VERIFIED_TERMINAL_DECISION**, its decision is **{decision}**, and Milestone 5.0R9 is **COMPLETE**.\n{STATUS_END}"
            ),
        ),
    ])
}

fn final_roadmap_rows(decision: &str) -> [String; 2] {
    [
        "| Gate 1 v2.9 | Stable semantic projection and explicit finalization | I/E/D/R/F chain and Raw-derived Receipt | Verified terminal decision |".to_owned(),
        format!(
            "| Gate 1 v2.9 Decision | Valid v2.9 evidence and 50/50 contracts | Verified I/E/D/R/F and empty structural gaps | {decision} |"
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
    validate_i_paths(&i)?;
    verify_phase_paths_disjoint([&i, &e, &d])?;
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
        "experiment_version": "gate1-v2.9",
        "status": "RECEIPT_VERIFIED_PENDING_F",
        "topology": "I2.9 -> E2.9 -> D2.9 -> R2.9 -> F2.9",
        "implementation_commit": i,
        "implementation_tree": git(&["rev-parse", &format!("{i}^{{tree}}")])?,
        "evidence_commit": e,
        "evidence_tree": git(&["rev-parse", &format!("{e}^{{tree}}")])?,
        "decision_commit": d,
        "decision_tree": git(&["rev-parse", &format!("{d}^{{tree}}")])?,
        "expected_f_parent": "R2.9_RECEIPT_COMMIT",
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
        "frozen_manifest_hash": hash_file("experiments/gate1-v2.9/manifest.json")?,
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
        "experiment_version": "gate1-v2.9",
        "topology": "I2.9 -> E2.9 -> D2.9 -> R2.9 -> F2.9",
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
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.8 |", &rows[0])?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.8 Decision |", &rows[1])?;
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
        || receipt["topology"] != "I2.9 -> E2.9 -> D2.9 -> R2.9 -> F2.9"
        || receipt["contracts_recomputed"] != true
        || receipt["contracts_satisfied"] != 49
        || receipt["wp_050_status"] != "PENDING_FINALIZATION"
        || receipt["decision_recomputed"] != true
        || receipt["product_decision"] != json!(reconstruction.product_decision)
        || receipt["known_structural_gaps"]
            .as_array()
            .is_none_or(|gaps| !gaps.is_empty())
    {
        return Err("R2.9 Receipt candidate is incomplete".into());
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
        return Err("D2.9 candidate files differ from reconstruction".into());
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
            return Err(format!("F2.9 file {path} differs from reconstruction").into());
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
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.9 |", &rows[0])?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.9 Decision |", &rows[1])?;
    std::fs::write("ROADMAP.md", roadmap)?;
    Ok(())
}

fn status_lint() -> Result<(), AnyError> {
    let status = read_json(CURRENT_STATUS)?;
    if status["current_experiment"] != "gate1-v2.9" {
        return Err("current status does not point at gate1-v2.9".into());
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
    println!("Gate 1 v2.9 status lint: PASS");
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
    let manifest = read_json(Path::new(RAW_ROOT).join("raw_hash_manifest.json"))?;
    let mut expected = manifest["artifacts"]
        .as_array()
        .ok_or("v2.9 Raw hash manifest artifacts are missing")?
        .iter()
        .map(|artifact| {
            artifact["path"]
                .as_str()
                .map(|path| format!("{RAW_ROOT}/{path}"))
                .ok_or_else(|| "v2.9 Raw artifact path is missing".into())
        })
        .collect::<Result<BTreeSet<String>, AnyError>>()?;
    expected.insert(format!("{RAW_ROOT}/raw_hash_manifest.json"));
    if paths != expected {
        return Err(format!("E2.9 path set is invalid: {paths:?}").into());
    }
    Ok(())
}

fn validate_i_paths(commit: &str) -> Result<(), AnyError> {
    let paths = commit_paths(commit)?;
    let required = [
        "baseline/testing/GATE1_ACCEPTANCE_V2_9.md",
        "baseline/testing/GATE1_V2_9_AUTHORIZATION.md",
        "experiments/gate1-v2.9/manifest.json",
        "reports/contracts/gate1_v2_9_contracts.json",
        "reports/history/gate1/versions/gate1-v2.9.json",
        "tools/gate1-v2-9-decision/src/main.rs",
        "tools/gate1-v2-9-gates/src/lib.rs",
    ];
    if !required.iter().all(|path| paths.contains(*path)) {
        return Err(format!("I2.9 is missing required implementation paths: {paths:?}").into());
    }
    let allowed = |path: &str| {
        matches!(path, "Cargo.toml" | "Cargo.lock")
            || path == "baseline/testing/GATE1_ACCEPTANCE_V2_9.md"
            || path == "baseline/testing/GATE1_V2_9_AUTHORIZATION.md"
            || path.starts_with("experiments/gate1-v2.9/")
            || path == "reports/contracts/gate1_v2_9_contracts.json"
            || path == "reports/contracts/gate1_v2_8_semantic_invalidation.json"
            || path == "reports/gate1_v2_8_semantic_invalidation.md"
            || path == "reports/history/gate1/index.json"
            || path == "reports/history/gate1/supersession_graph.json"
            || path == "reports/history/gate1/versions/gate1-v2.8.json"
            || path == "reports/history/gate1/versions/gate1-v2.9.json"
            || path.starts_with("reports/history/gate1/v2_8/")
            || path.starts_with("tools/gate1-v2-9/")
            || path.starts_with("tools/gate1-v2-9-fixtures/")
            || path.starts_with("tools/gate1-v2-9-gates/")
            || path.starts_with("tools/gate1-v2-9-decision/")
    };
    if paths.is_empty() || paths.iter().any(|path| !allowed(path)) {
        return Err(format!("I2.9 contains an unauthorized path: {paths:?}").into());
    }
    let forbidden = F_PATHS
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if !paths.is_disjoint(&forbidden) {
        return Err("I2.9 overlaps an F2.9 output path".into());
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
        return Err(format!("D2.9 path set is invalid: {paths:?}").into());
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
        return Err(format!("R2.9 path set is invalid: {paths:?}").into());
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

fn verify_phase_paths_disjoint<const N: usize>(commits: [&str; N]) -> Result<(), AnyError> {
    let sets = commits
        .into_iter()
        .map(commit_paths)
        .collect::<Result<Vec<_>, _>>()?;
    for left in 0..sets.len() {
        for right in (left + 1)..sets.len() {
            if !sets[left].is_disjoint(&sets[right]) {
                return Err(format!(
                    "phase path sets {left} and {right} overlap: {:?}",
                    sets[left].intersection(&sets[right]).collect::<Vec<_>>()
                )
                .into());
            }
        }
    }
    Ok(())
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
        "gate1-v2.9-receipt-regeneration-{}",
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
    let (start_marker, end_marker) = if source.contains(STATUS_START) {
        (STATUS_START, STATUS_END)
    } else {
        (PREVIOUS_STATUS_START, PREVIOUS_STATUS_END)
    };
    let start = source
        .find(start_marker)
        .ok_or("Gate 1 status block start is missing")?;
    let end = source
        .find(end_marker)
        .ok_or("Gate 1 status block end is missing")?
        + end_marker.len();
    let mut output = String::with_capacity(source.len() + replacement.len());
    output.push_str(&source[..start]);
    output.push_str(replacement);
    output.push_str(&source[end..]);
    Ok(output)
}

fn extract_block(source: &str) -> Result<String, AnyError> {
    let start = source
        .find(STATUS_START)
        .ok_or("v2.9 status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("v2.9 status block end is missing")?
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
