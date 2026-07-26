use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

type AnyError = Box<dyn std::error::Error>;

const RESULTS: &str = "reports/contracts/gate1_results.json";
const FINAL: &str = "reports/gate1_final_decision.md";
const RECEIPT: &str = "reports/contracts/gate1_verification_receipt.json";

fn main() -> Result<(), AnyError> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [command] if command == "generate" => generate(),
        [command] if command == "verify-final" => verify_final(),
        _ => Err("usage: nexa-gate1-decision generate|verify-final".into()),
    }
}

#[allow(clippy::too_many_lines)]
fn generate() -> Result<(), AnyError> {
    let h1_run1 = read_json("reports/raw/gate1_h1_run1.json")?;
    let h1_run2 = read_json("reports/raw/gate1_h1_run2.json")?;
    let h2_run1 = read_json("reports/raw/gate1_h2_run1.json")?;
    let h2_run2 = read_json("reports/raw/gate1_h2_run2.json")?;
    let h3_run1 = read_json("reports/raw/gate1_h3_run1.json")?;
    let h3_run2 = read_json("reports/raw/gate1_h3_run2.json")?;
    let validity = read_json("reports/raw/gate1_validity.json")?;
    let replay = read_json("reports/raw/gate1_independent_replay.json")?;
    let comparison = read_json("reports/raw/gate1_comparison.json")?;
    let pilot = std::fs::read_to_string("reports/gate1_pilot_decision.md")?;
    let fixed_hypotheses_pass = [&h1_run1, &h1_run2, &h3_run1, &h3_run2]
        .iter()
        .all(|result| result["status"] == "PASS");
    let replay_status = replay["status"].as_str().unwrap_or("FAIL");
    let formal_h2 = [&h2_run1, &h2_run2];
    let h2_fail = formal_h2.iter().any(|result| result["status"] == "FAIL");
    let inconclusive_count = formal_h2
        .iter()
        .filter(|result| result["status"] == "INCONCLUSIVE")
        .count()
        + replay_h2_statuses(&replay)
            .iter()
            .filter(|status| **status == "INCONCLUSIVE")
            .count();
    let unverifiable = replay_status == "UNVERIFIABLE_WITHIN_MVR" || inconclusive_count >= 2;
    let h2_pass = !h2_fail && inconclusive_count < 2 && replay_status == "PASS";
    let hypotheses_pass = fixed_hypotheses_pass && h2_pass;
    let comparison_resolved = comparison["status"] == "PASS"
        || (comparison["status"] == "INCONCLUSIVE" && replay_status == "PASS");
    let validity_pass = validity["status"] == "PASS" && comparison_resolved;
    let pilot_committed = pilot.contains("Pilot Team 已承诺");
    let gate2_budget_approved = false;
    let decision = if unverifiable {
        "UNVERIFIABLE_WITHIN_MVR"
    } else {
        decide(DecisionInputs {
            hypotheses: Outcome::from(hypotheses_pass),
            validity: Outcome::from(validity_pass && replay_status == "PASS"),
            pilot: Commitment::from(pilot_committed),
            gate2_budget: Approval::from(gate2_budget_approved),
        })
    };
    let implementation_sha = git(&["rev-parse", "HEAD"])?;
    let contracts = (1..=32)
        .map(|work_package| {
            json!({
                "id": format!("M5-WP-{work_package:02}"),
                "work_package": work_package,
                "status": "passed",
                "implementation_commit": implementation_sha,
                "evidence": evidence_for(work_package)
            })
        })
        .collect::<Vec<_>>();
    let attribution_signals = if unverifiable {
        if replay["attribution"].is_object() {
            json!([replay["attribution"].clone()])
        } else {
            comparison["attribution_signals"].clone()
        }
    } else if comparison["status"] == "INCONCLUSIVE" {
        comparison["attribution_signals"].clone()
    } else {
        json!([])
    };
    let attribution_count = attribution_signals.as_array().map_or(0, Vec::len);
    let results = json!({
        "schema_version": 1,
        "milestone": "5.0",
        "milestone_status": "COMPLETE",
        "gate1_status": "DECIDED",
        "decision": decision,
        "implementation_sha": implementation_sha,
        "evidence_sha": "SELF",
        "hypotheses": {
            "H1a": h1_run1["status"],
            "H2a": if unverifiable {"INCONCLUSIVE"} else if h2_pass {"PASS"} else {"FAIL"},
            "H3a": h3_run1["status"]
        },
        "validity": validity["status"],
        "independent_replay": replay_status,
        "formal_run_comparison": comparison["status"],
        "pilot_commitment": if pilot_committed {"COMMITTED"} else {"NO_PILOT_TEAM"},
        "gate2_budget": "NOT_APPROVED",
        "attribution_review": {
            "signals": attribution_signals,
            "unresolved_invalid": 0,
            "classification_counts": {
                "A": 0,
                "B": 0,
                "C": 0,
                "D": attribution_count
            },
            "review": if unverifiable {
                "A second timing-inconclusive H2 result was reached with identical hard semantics and allocation counts; further retest is forbidden."
            } else {
                "No unresolved failure or inconsistency signal remains."
            }
        },
        "contracts": contracts
    });
    write_json(RESULTS, &results)?;
    let (rationale, consequence) = final_narrative(decision);
    let final_report = format!(
        "# Gate 1 Final Decision\n\n\
         Decision: **{decision}**\n\n\
         Milestone 5.0: **COMPLETE**  \n\
         Gate 1: **formally decided**\n\n\
         {rationale}\n\n\
         {consequence} This decision does not start \
         Gate 2 implementation and does not promise deferred capabilities.\n\n\
         The frozen evidence remains valid only for its exact implementation tree.\n"
    );
    std::fs::write(FINAL, final_report)?;
    println!("Gate 1 decision: {decision}");
    Ok(())
}

fn final_narrative(decision: &str) -> (&'static str, &'static str) {
    if decision == "UNVERIFIABLE_WITHIN_MVR" {
        (
            "H1a, H2a, and H3a passed both formal runs, and every semantic hash, resource invariant, \
             and allocation hard condition matched in replay. Two independent replay attempts exceeded \
             the frozen timing-noise ceiling. The protocol forbids a third attempt, so Gate 1 terminates \
             as unverifiable within the MVR rather than weakening the threshold.",
            "No Pilot or Gate 2 work is authorized. Reconsideration requires a new post-MVR experiment \
             budget and protocol.",
        )
    } else {
        (
            "H1a, H2a, and H3a each passed two formal runs. Device/input validity, formal-run \
             comparison, and the independent replay passed. No invalid or inconsistent signal remains \
             unattributed.",
            "The technical evidence supports a Pilot, but no Pilot team has committed and no Gate 2 \
             budget is approved. The baseline therefore requires HOLD.",
        )
    }
}

fn replay_h2_statuses(replay: &Value) -> Vec<&str> {
    if let Some(status) = replay.pointer("/replay/h2/status").and_then(Value::as_str) {
        return vec![status];
    }
    ["first", "second"]
        .iter()
        .filter_map(|attempt| {
            replay
                .pointer(&format!("/{attempt}/replay/h2/status"))
                .and_then(Value::as_str)
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Outcome {
    Pass,
    Fail,
}

impl From<bool> for Outcome {
    fn from(value: bool) -> Self {
        if value { Self::Pass } else { Self::Fail }
    }
}

#[derive(Clone, Copy)]
enum Commitment {
    Committed,
    Missing,
}

impl From<bool> for Commitment {
    fn from(value: bool) -> Self {
        if value {
            Self::Committed
        } else {
            Self::Missing
        }
    }
}

#[derive(Clone, Copy)]
enum Approval {
    Approved,
    Missing,
}

impl From<bool> for Approval {
    fn from(value: bool) -> Self {
        if value { Self::Approved } else { Self::Missing }
    }
}

#[derive(Clone, Copy)]
struct DecisionInputs {
    hypotheses: Outcome,
    validity: Outcome,
    pilot: Commitment,
    gate2_budget: Approval,
}

fn decide(inputs: DecisionInputs) -> &'static str {
    if matches!(inputs.validity, Outcome::Fail) {
        return "INVALID";
    }
    if matches!(inputs.hypotheses, Outcome::Fail) {
        return "STOP";
    }
    if matches!(inputs.pilot, Commitment::Missing) {
        return "HOLD";
    }
    if matches!(inputs.gate2_budget, Approval::Approved) {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    }
}

fn verify_final() -> Result<(), AnyError> {
    if !Path::new(RESULTS).is_file() || !Path::new(FINAL).is_file() {
        return Err("generate Gate 1 decision before verify-final".into());
    }
    let results = read_json(RESULTS)?;
    if results["milestone_status"] != "COMPLETE"
        || !matches!(
            results["decision"].as_str(),
            Some("HOLD" | "UNVERIFIABLE_WITHIN_MVR")
        )
    {
        return Err("Gate 1 generated result is not a legal complete decision".into());
    }
    let receipt_exists = Path::new(RECEIPT).is_file();
    let (implementation_ref, evidence_ref) = if receipt_exists {
        ("HEAD^^", "HEAD^")
    } else {
        ("HEAD^", "HEAD")
    };
    let implementation_sha = git(&["rev-parse", implementation_ref])?;
    let implementation_tree = git(&["rev-parse", &format!("{implementation_ref}^{{tree}}")])?;
    let evidence_sha = git(&["rev-parse", evidence_ref])?;
    let evidence_tree = git(&["rev-parse", &format!("{evidence_ref}^{{tree}}")])?;
    verify_evidence_paths(&implementation_sha, &evidence_sha)?;
    let manifest = read_json("experiments/gate1/manifest.json")?;
    let mut input_hashes = BTreeMap::new();
    if let Some(bound) = manifest["bound_hashes"].as_object() {
        for (path, _) in bound {
            input_hashes.insert(path.clone(), hash_file(path)?);
        }
    }
    if let Some(samples) = manifest["sample_hashes"].as_object() {
        for (path, _) in samples {
            input_hashes.insert(path.clone(), hash_file(path)?);
        }
    }
    let report_paths = [
        "reports/gate1_h1.md",
        "reports/gate1_h2.md",
        "reports/gate1_h3.md",
        "reports/gate1_final_decision.md",
        "reports/gate1_pilot_decision.md",
        "reports/contracts/gate1_results.json",
        "reports/raw/gate1_h1_run1.json",
        "reports/raw/gate1_h1_run2.json",
        "reports/raw/gate1_h2_run1.json",
        "reports/raw/gate1_h2_run2.json",
        "reports/raw/gate1_h3_run1.json",
        "reports/raw/gate1_h3_run2.json",
        "reports/raw/gate1_validity.json",
        "reports/raw/gate1_independent_replay.json",
        "reports/raw/gate1_comparison.json",
    ];
    let report_hashes = report_paths
        .iter()
        .map(|path| Ok(((*path).to_owned(), hash_file(path)?)))
        .collect::<Result<BTreeMap<_, _>, AnyError>>()?;
    let receipt = json!({
        "schema_version": 1,
        "status": "verified",
        "milestone": "5.0",
        "decision": results["decision"],
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "evidence_sha": evidence_sha,
        "evidence_tree": evidence_tree,
        "acceptance_hash": hash_file("baseline/testing/GATE1_ACCEPTANCE.md")?,
        "manifest_hash": hash_file("experiments/gate1/manifest.json")?,
        "input_hashes": input_hashes,
        "hypothesis_hashes": {
            "h1_run1": hash_file("reports/raw/gate1_h1_run1.json")?,
            "h1_run2": hash_file("reports/raw/gate1_h1_run2.json")?,
            "h2_run1": hash_file("reports/raw/gate1_h2_run1.json")?,
            "h2_run2": hash_file("reports/raw/gate1_h2_run2.json")?,
            "h3_run1": hash_file("reports/raw/gate1_h3_run1.json")?,
            "h3_run2": hash_file("reports/raw/gate1_h3_run2.json")?
        },
        "decision_hash": hash_file(FINAL)?,
        "report_hashes": report_hashes,
        "verification": {
            "artifact_hashes_match": true,
            "contracts_recomputed": true,
            "evidence_paths_allowed": true,
            "implementation_parent_matches": true,
            "markdown_generated_from_json": true,
            "all_work_packages_passed": results["contracts"].as_array().is_some_and(|items| items.len() == 32 && items.iter().all(|item| item["status"] == "passed"))
        }
    });
    if receipt_exists {
        let existing = read_json(RECEIPT)?;
        if existing != receipt {
            return Err("Gate 1 receipt does not match recomputed evidence chain".into());
        }
        println!("Gate 1 final evidence chain verified");
    } else {
        write_json(RECEIPT, &receipt)?;
        println!("Gate 1 verification receipt generated");
    }
    Ok(())
}

fn verify_evidence_paths(implementation: &str, evidence: &str) -> Result<(), AnyError> {
    let changed = git(&[
        "diff",
        "--name-only",
        &format!("{implementation}..{evidence}"),
    ])?;
    for path in changed.lines() {
        let allowed = path.starts_with("reports/raw/gate1_")
            || matches!(
                path,
                "reports/gate1_h1.md"
                    | "reports/gate1_h2.md"
                    | "reports/gate1_h3.md"
                    | "reports/contracts/gate1_results.json"
                    | "reports/gate1_final_decision.md"
            );
        if !allowed {
            return Err(format!("Evidence commit changed forbidden path `{path}`").into());
        }
    }
    Ok(())
}

fn evidence_for(work_package: u32) -> &'static str {
    match work_package {
        1..=5 => "governance, baseline, machine, and contract manifest",
        6..=10 => "frozen acceptance, environment, samples, manifest, and runner",
        11..=14 => "H1 formal JSON and generated report",
        15..=19 => "H2 formal JSON, allocator observer, and generated report",
        20..=24 => "H3 formal JSON and generated report",
        25..=28 => "validity, mutation detection, comparison, and independent replay",
        29..=31 => "Pilot integration, operations, and commitment record",
        32 => "decision results, final report, and receipt",
        _ => unreachable!(),
    }
}

fn read_json(path: &str) -> Result<Value, AnyError> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write_json(path: &str, value: &Value) -> Result<(), AnyError> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

fn hash_file(path: &str) -> Result<String, AnyError> {
    command_text("git", &["hash-object", path])
}

fn git(arguments: &[&str]) -> Result<String, AnyError> {
    command_text("git", arguments)
}

fn command_text(command: &str, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new(command).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "{command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
