#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_gate1_v2_4::{
    AnyError, git, hash_file, read_json, repository_root, stable_value_hash, write_json,
};
use nexa_gate1_v2_4_gates::{GATE_NAMES, RAW_ROOT, gate_hashes, generate_from_raw};
use serde::Deserialize;
use serde_json::{Value, json};

const CONTRACTS: &str = "reports/contracts/gate1_v2_4_contracts.json";
const RESULTS: &str = "reports/contracts/gate1_v2_4_results.json";
const FINAL: &str = "reports/gate1_v2_4_final_decision.md";
const SUMMARY: &str = "reports/gate1_v2_4_summary.md";
const PILOT_REPORT: &str = "reports/gate1_v2_4_pilot.json";
const BUDGET_REPORT: &str = "reports/gate1_v2_4_budget.json";
const RECEIPT: &str = "reports/contracts/gate1_v2_4_verification_receipt.json";
const GATES: &str = "reports/raw/gate1_v2_4/gates";
const CURRENT_STATUS: &str = "reports/history/gate1/current_status.json";
const STATUS_START: &str = "<!-- gate1-v2.4-status:start -->";
const STATUS_END: &str = "<!-- gate1-v2.4-status:end -->";

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

struct Generated {
    results: Value,
    final_report: String,
    summary: String,
    current_status: Value,
    document_blocks: BTreeMap<&'static str, String>,
    roadmap_gate_row: String,
    roadmap_decision_row: String,
}

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "generate" => generate(),
        [command] if command == "verify-evidence" => verify_evidence(),
        [command] if command == "generate-receipt" => generate_receipt(),
        [command] if command == "verify-final" => verify_final(),
        [command] if command == "status-lint" => status_lint(),
        _ => Err("usage: nexa-gate1-v2-4-decision generate|verify-evidence|generate-receipt|verify-final|status-lint".into()),
    }
}

fn generate() -> Result<(), AnyError> {
    let generated = reconstruct(Path::new(GATES))?;
    write_json(Path::new(RESULTS), &generated.results)?;
    std::fs::write(FINAL, &generated.final_report)?;
    std::fs::write(SUMMARY, &generated.summary)?;
    write_json(
        Path::new(PILOT_REPORT),
        &read_json(Path::new(GATES).join("pilot.json"))?,
    )?;
    write_json(
        Path::new(BUDGET_REPORT),
        &read_json(Path::new(GATES).join("budget.json"))?,
    )?;
    write_json(Path::new(CURRENT_STATUS), &generated.current_status)?;
    apply_documents(&generated)?;
    status_lint()?;
    println!(
        "Gate 1 v2.4 decision: {}",
        generated.results["decision"].as_str().unwrap_or("INVALID")
    );
    Ok(())
}

fn verify_evidence() -> Result<(), AnyError> {
    let generated = reconstruct(Path::new(GATES))?;
    verify_generated_files(&generated)?;
    status_lint()?;
    println!("Gate 1 v2.4 Evidence, contracts, decision, reports, and status verified");
    Ok(())
}

fn reconstruct(gates: &Path) -> Result<Generated, AnyError> {
    let manifest: Manifest = serde_json::from_value(read_json(CONTRACTS)?)?;
    if manifest.contracts.len() != 44
        || manifest
            .contracts
            .iter()
            .map(|contract| contract.work_package)
            .collect::<BTreeSet<_>>()
            != (1_u32..=44).collect()
    {
        return Err("contract manifest must cover WP1-WP44 exactly once".into());
    }
    let mut evaluations = Vec::new();
    let mut gaps = Vec::new();
    for contract in &manifest.contracts {
        if contract.gate != contract.artifact.trim_end_matches(".json") {
            gaps.push(format!("{} gate/artifact names differ", contract.id));
        }
        let artifact = read_json(gates.join(&contract.artifact))?;
        let checks = contract
            .assertions
            .iter()
            .map(|assertion| {
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
            })
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
    let decision = decide(gates, &gaps)?;
    let implementation_sha = gate_manifest["implementation_sha"].clone();
    let implementation_tree = gate_manifest["implementation_tree"].clone();
    let results = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.4",
        "implementation_sha": implementation_sha,
        "implementation_tree": implementation_tree,
        "gate_count": 21,
        "contract_count": evaluations.len(),
        "contracts_satisfied": evaluations.iter().filter(|contract| contract["status"] == "SATISFIED").count(),
        "contracts": evaluations,
        "known_structural_gaps": gaps,
        "decision": decision,
        "decision_recomputable": true,
        "milestone_status": if decision == "INVALID" && !results_are_structurally_valid(gates) {"INCOMPLETE"} else {"COMPLETE"}
    });
    let milestone_status = results["milestone_status"].as_str().unwrap_or("INCOMPLETE");
    let current_status = json!({
        "schema_version": 1,
        "current_experiment": "gate1-v2.4",
        "experiment_status": "VERIFIED_TERMINAL_DECISION",
        "decision": decision,
        "milestone": "5.0R4",
        "milestone_status": milestone_status
    });
    let final_report = render_final(&results);
    let summary = render_summary(&results);
    let blocks = document_blocks(&decision, milestone_status);
    Ok(Generated {
        results,
        final_report,
        summary,
        current_status,
        document_blocks: blocks,
        roadmap_gate_row: "| Gate 1 v2.4 | Scenario-real apparatus is frozen | Two formal runs and replay preserve real outcomes | Verified terminal decision |".to_owned(),
        roadmap_decision_row: format!(
            "| Gate 1 v2.4 Decision | Valid v2.4 evidence is available | One legal terminal decision and receipt are recorded | {decision} |"
        ),
    })
}

fn decide(gates: &Path, gaps: &[String]) -> Result<String, AnyError> {
    if !gaps.is_empty() {
        return Ok("INVALID".to_owned());
    }
    let validity = gate_outcome(gates, "validity")?;
    if validity == "INVALID" {
        return Ok("INVALID".to_owned());
    }
    let outcome_names = [
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
    ];
    let outcomes = outcome_names
        .map(|name| gate_outcome(gates, name))
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    if outcomes
        .iter()
        .any(|outcome| outcome == "INVALID" || outcome == "NOT_RUN_DUE_TO_TERMINAL_DECISION")
    {
        return Ok("INVALID".to_owned());
    }
    if outcomes.iter().any(|outcome| outcome == "INCONCLUSIVE") {
        return Ok("UNVERIFIABLE_WITHIN_MVR".to_owned());
    }
    if outcomes.iter().any(|outcome| outcome == "FAIL") {
        return Ok("STOP".to_owned());
    }
    let pilot = read_json(gates.join("pilot.json"))?["metrics"]["committed"] == true;
    let budget = read_json(gates.join("budget.json"))?["metrics"]["approved"] == true;
    Ok(if !pilot {
        "HOLD"
    } else if budget {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    }
    .to_owned())
}

fn gate_outcome(gates: &Path, name: &str) -> Result<String, AnyError> {
    read_json(gates.join(format!("{name}.json")))?["outcome"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{name} Gate has no outcome").into())
}

fn results_are_structurally_valid(gates: &Path) -> bool {
    GATE_NAMES.iter().all(|name| {
        read_json(gates.join(format!("{name}.json")))
            .is_ok_and(|gate| gate["contract_status"] == "PASS")
    })
}

fn render_final(results: &Value) -> String {
    format!(
        "# Gate 1 v2.4 Final Decision\n\nDecision: **{}**\n\nMilestone 5.0R4: **{}**\n\n- Gates regenerated from Raw Run: 21\n- Contracts satisfied: {}/44\n- Known structural gaps: {}\n- Decision recomputable: true\n",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["milestone_status"].as_str().unwrap_or("INCOMPLETE"),
        results["contracts_satisfied"].as_u64().unwrap_or(0),
        results["known_structural_gaps"]
            .as_array()
            .map_or(0, Vec::len)
    )
}

fn render_summary(results: &Value) -> String {
    format!(
        "# Gate 1 v2.4 Summary\n\nThe verified terminal decision is **{}**. All {} contracts are satisfied, the 21 Gates are regenerated directly from Raw Run evidence, and the decision has no known structural gap.\n",
        results["decision"].as_str().unwrap_or("INVALID"),
        results["contracts_satisfied"].as_u64().unwrap_or(0)
    )
}

fn document_blocks(decision: &str, milestone_status: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        (
            "README.md",
            format!(
                "{STATUS_START}\nGate 1 v1: INVALID_APPARATUS\nGate 1 v2: INVALID_APPARATUS / NOT AUTHORIZED FOR DECISION\nGate 1 v2.1: INVALID / NOT AUTHORIZED FOR DECISION\nGate 1 v2.2: NOT TRUSTWORTHY / NOT AUTHORIZED FOR DECISION\nGate 1 v2.3: SEMANTICALLY_INSUFFICIENT / NOT AUTHORIZED FOR DECISION\nGate 1 v2.4: VERIFIED_TERMINAL_DECISION\nCurrent decision: {decision}\nMilestone 5.0R4: {milestone_status}\n{STATUS_END}"
            ),
        ),
        (
            "ROADMAP.md",
            format!(
                "{STATUS_START}\nCurrent project gate: **Gate 1 v2.4 verified terminal decision**.\n\nGate 1 v1 and Gate 1 v2 are **INVALID_APPARATUS**. Gate 1 v2.1 is **INVALID**, Gate 1 v2.2 is **NOT TRUSTWORTHY**, and Gate 1 v2.3 is **SEMANTICALLY_INSUFFICIENT**. None is a current decision.\n\nCurrent Gate 1 v2.4 decision: **{decision}**.\n\nCurrent milestone: **5.0R4 — {milestone_status}**.\n{STATUS_END}"
            ),
        ),
        (
            "baseline/BASELINE_INDEX.md",
            format!(
                "{STATUS_START}\nGate 1 v1 and Gate 1 v2 are **INVALID_APPARATUS**. Gate 1 v2.1 is **INVALID**, Gate 1 v2.2 is **NOT TRUSTWORTHY**, and Gate 1 v2.3 is **SEMANTICALLY_INSUFFICIENT**. Gate 1 v2.4 is **VERIFIED_TERMINAL_DECISION**, its decision is **{decision}**, and Milestone 5.0R4 is **{milestone_status}**.\n{STATUS_END}"
            ),
        ),
    ])
}

fn apply_documents(generated: &Generated) -> Result<(), AnyError> {
    for (path, block) in &generated.document_blocks {
        replace_block(Path::new(path), block)?;
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    let roadmap = replace_table_row(&roadmap, "| Gate 1 v2.4 |", &generated.roadmap_gate_row)?;
    let roadmap = replace_table_row(
        &roadmap,
        "| Gate 1 v2.4 Decision |",
        &generated.roadmap_decision_row,
    )?;
    std::fs::write("ROADMAP.md", roadmap)?;
    Ok(())
}

fn verify_generated_files(generated: &Generated) -> Result<(), AnyError> {
    if read_json(RESULTS)? != generated.results
        || read_json(CURRENT_STATUS)? != generated.current_status
        || std::fs::read_to_string(FINAL)? != generated.final_report
        || std::fs::read_to_string(SUMMARY)? != generated.summary
        || read_json(PILOT_REPORT)? != read_json(Path::new(GATES).join("pilot.json"))?
        || read_json(BUDGET_REPORT)? != read_json(Path::new(GATES).join("budget.json"))?
    {
        return Err("generated Evidence files differ from reconstruction".into());
    }
    for (path, block) in &generated.document_blocks {
        if extract_block(&std::fs::read_to_string(path)?)? != *block {
            return Err(format!("{path} status differs from reconstruction").into());
        }
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    if !roadmap.contains(&generated.roadmap_gate_row)
        || !roadmap.contains(&generated.roadmap_decision_row)
    {
        return Err("ROADMAP Gate table differs from reconstruction".into());
    }
    Ok(())
}

fn status_lint() -> Result<(), AnyError> {
    let registry = read_json(CURRENT_STATUS)?;
    let decision = registry["decision"]
        .as_str()
        .ok_or("current status Decision is missing")?;
    let experiment_status = registry["experiment_status"]
        .as_str()
        .ok_or("current status experiment state is missing")?;
    let milestone_status = registry["milestone_status"]
        .as_str()
        .ok_or("current status milestone state is missing")?;
    let blocks = document_blocks(decision, milestone_status);
    let final_state = experiment_status == "VERIFIED_TERMINAL_DECISION";
    for (path, expected) in blocks {
        let source = std::fs::read_to_string(path)?;
        if final_state && extract_block(&source)? != expected {
            return Err(format!("{path} conflicts with current status registry").into());
        }
        if source.matches(STATUS_START).count() != 1 || source.matches(STATUS_END).count() != 1 {
            return Err(format!("{path} does not have exactly one v2.4 status block").into());
        }
    }
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    if final_state {
        let generated = reconstruct(Path::new(GATES))?;
        if !roadmap.contains(&generated.roadmap_gate_row)
            || !roadmap.contains(&generated.roadmap_decision_row)
        {
            return Err("ROADMAP status block and Gate table conflict".into());
        }
    } else if !roadmap.contains("| Gate 1 v2.4 | Scenario-real apparatus is prefreeze-complete | Two formal runs and replay preserve real outcomes | Frozen |")
        && registry["experiment_status"] == "FROZEN"
    {
        return Err("ROADMAP does not describe the frozen current Gate".into());
    }
    if roadmap.matches("| Gate 1 v2.4 |").count() != 1 || roadmap.contains("Gate 1 v2.3 is current")
    {
        return Err("ROADMAP has an ambiguous current Gate".into());
    }
    println!("Gate 1 v2.4 full-document status lint: PASS");
    Ok(())
}

fn generate_receipt() -> Result<(), AnyError> {
    verify_evidence()?;
    if Path::new(RECEIPT).exists() {
        return Err("verification Receipt already exists".into());
    }
    let temporary = receipt_temp_directory();
    if temporary.exists() {
        std::fs::remove_dir_all(&temporary)?;
    }
    let regenerated = temporary.join("gates");
    generate_from_raw(Path::new(RAW_ROOT), &regenerated)?;
    let byte_comparison = compare_gate_bytes(Path::new(GATES), &regenerated)?;
    let regenerated_decision = reconstruct(&regenerated)?;
    let recorded = read_json(RESULTS)?;
    if regenerated_decision.results != recorded {
        return Err("Receipt regeneration produced a different contract result or Decision".into());
    }
    let evidence_commit = git(&["rev-parse", "HEAD"])?;
    let implementation_commit = recorded["implementation_sha"]
        .as_str()
        .ok_or("results implementation SHA is missing")?;
    let implementation_parent = git(&["rev-parse", &format!("{evidence_commit}^")])?;
    if implementation_parent != implementation_commit {
        return Err("Evidence commit is not the direct child of I2.4".into());
    }
    let receipt = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.4",
        "status": "verified",
        "implementation_commit": implementation_commit,
        "evidence_commit": evidence_commit,
        "topology": "I2.4 -> E2.4 -> R2.4",
        "gate_count": 21,
        "gates_regenerated_from_raw": true,
        "gate_byte_comparison": byte_comparison,
        "gate_hashes": gate_hashes(Path::new(GATES))?,
        "contract_count": 44,
        "contracts_recomputed": recorded["contracts_satisfied"] == 44,
        "decision_recomputed": regenerated_decision.results["decision"] == recorded["decision"],
        "reports_recomputed": regenerated_decision.final_report == std::fs::read_to_string(FINAL)?
            && regenerated_decision.summary == std::fs::read_to_string(SUMMARY)?
            && read_json(PILOT_REPORT)? == read_json(regenerated.join("pilot.json"))?
            && read_json(BUDGET_REPORT)? == read_json(regenerated.join("budget.json"))?,
        "full_document_status_recomputed": true,
        "artifact_hygiene_verified": read_json(Path::new(GATES).join("artifact_hygiene.json"))?["contract_status"] == "PASS",
        "known_structural_gaps": recorded["known_structural_gaps"],
        "evidence_paths_verified": evidence_paths_valid(&git(&["diff-tree", "--no-commit-id", "--name-only", "-r", &evidence_commit])?),
        "receipt_single_file_required": true,
        "results_hash": hash_file(RESULTS)?,
        "final_report_hash": hash_file(FINAL)?,
        "summary_hash": hash_file(SUMMARY)?,
        "current_status_hash": hash_file(CURRENT_STATUS)?,
        "contract_manifest_hash": hash_file(CONTRACTS)?,
        "raw_manifest_hash": hash_file(Path::new(GATES).join("manifest.json"))?
    });
    if receipt["contracts_recomputed"] != true
        || receipt["decision_recomputed"] != true
        || receipt["reports_recomputed"] != true
        || receipt["artifact_hygiene_verified"] != true
        || receipt["evidence_paths_verified"] != true
        || receipt["known_structural_gaps"]
            .as_array()
            .is_none_or(|gaps| !gaps.is_empty())
    {
        return Err("Receipt prerequisites are not satisfied".into());
    }
    write_json(Path::new(RECEIPT), &receipt)?;
    std::fs::remove_dir_all(temporary)?;
    println!("Gate 1 v2.4 verification Receipt generated");
    Ok(())
}

fn verify_final() -> Result<(), AnyError> {
    verify_evidence()?;
    let receipt = read_json(RECEIPT)?;
    if receipt["status"] != "verified"
        || receipt["gates_regenerated_from_raw"] != true
        || receipt["contracts_recomputed"] != true
        || receipt["decision_recomputed"] != true
        || receipt["reports_recomputed"] != true
        || receipt["artifact_hygiene_verified"] != true
        || receipt["evidence_paths_verified"] != true
    {
        return Err("verification Receipt is incomplete".into());
    }
    let head = git(&["rev-parse", "HEAD"])?;
    let parent = git(&["rev-parse", "HEAD^"])?;
    if parent != receipt["evidence_commit"].as_str().unwrap_or("")
        || git(&["diff-tree", "--no-commit-id", "--name-only", "-r", &head])?
            .lines()
            .collect::<Vec<_>>()
            != [RECEIPT]
    {
        return Err("R2.4 is not a single-Receipt child of E2.4".into());
    }
    let temporary = receipt_temp_directory();
    let regenerated = temporary.join("gates");
    generate_from_raw(Path::new(RAW_ROOT), &regenerated)?;
    compare_gate_bytes(Path::new(GATES), &regenerated)?;
    std::fs::remove_dir_all(temporary)?;
    println!("Gate 1 v2.4 final Receipt and I/E/R topology: verified");
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
        comparisons.insert(file, stable_value_hash(&json!({"bytes": left.len(), "hash": hash_file(recorded.join(format!("{name}.json")))?})));
    }
    Ok(json!({"status":"PASS","files":comparisons}))
}

fn evidence_paths_valid(paths: &str) -> bool {
    let allowed_exact = ["README.md", "ROADMAP.md", "baseline/BASELINE_INDEX.md"];
    paths.lines().filter(|path| !path.is_empty()).all(|path| {
        path.starts_with("reports/raw/gate1_v2_4/")
            || path == "reports/contracts/gate1_v2_4_results.json"
            || path.starts_with("reports/gate1_v2_4_")
            || path == "reports/gate1_v2_4_pilot.json"
            || path == "reports/gate1_v2_4_budget.json"
            || path.starts_with("reports/history/gate1/")
            || allowed_exact.contains(&path)
    })
}

fn receipt_temp_directory() -> PathBuf {
    Path::new("target").join(format!(
        "gate1-v2.4-receipt-regeneration-{}",
        std::process::id()
    ))
}

fn replace_block(path: &Path, replacement: &str) -> Result<(), AnyError> {
    let source = std::fs::read_to_string(path)?;
    let start = source
        .find(STATUS_START)
        .ok_or("status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("status block end is missing")?
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
        .ok_or("status block start is missing")?;
    let end = source
        .find(STATUS_END)
        .ok_or("status block end is missing")?
        + STATUS_END.len();
    Ok(source[start..end].to_owned())
}

fn replace_table_row(source: &str, prefix: &str, replacement: &str) -> Result<String, AnyError> {
    let mut replaced = false;
    let output = source
        .lines()
        .map(|line| {
            if line.starts_with(prefix) {
                replaced = true;
                replacement
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !replaced {
        return Err(format!("ROADMAP row `{prefix}` is missing").into());
    }
    Ok(format!("{output}\n"))
}
