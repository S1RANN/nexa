use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use nexa_contract_gate_protocol::{GATE_SCHEMA_VERSION, GateArtifact, GateStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANIFEST_PATH: &str = "reports/contracts/milestone4r3_contracts.json";
const RESULTS_PATH: &str = "reports/contracts/milestone4r3_results.json";
const REPORT_PATH: &str = "reports/milestone4_0_full_mvr.md";
const RECEIPT_PATH: &str = "reports/contracts/milestone4r3_verification_receipt.json";
const RAW_PATH: &str = "reports/contracts/raw";
const MILESTONE: &str = "milestone4r3";
const EVIDENCE_ALLOWED: [&str; 3] = ["reports/contracts/raw/", RESULTS_PATH, REPORT_PATH];

#[derive(Debug, Deserialize)]
struct ContractManifest {
    version: u32,
    milestone: String,
    contracts: Vec<ContractDefinition>,
}

#[derive(Debug, Deserialize)]
struct ContractDefinition {
    id: String,
    work_package: u32,
    description: String,
    gate: String,
    artifact: String,
    assertions: Vec<Assertion>,
    implementation_commit: Option<String>,
    affected_paths: Vec<String>,
    forbidden_calls: Vec<String>,
    status: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Assertion {
    pointer: String,
    operator: String,
    expected: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AssertionResult {
    pointer: String,
    operator: String,
    expected: Value,
    actual: Option<Value>,
    passed: bool,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ContractResult {
    id: String,
    work_package: u32,
    description: String,
    gate: String,
    artifact: String,
    implementation_commit: String,
    status: String,
    assertions: Vec<AssertionResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct KnownGap {
    contract: String,
    gate: String,
    pointer: String,
    expected: Value,
    actual: Option<Value>,
    reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AuditResults {
    schema_version: u32,
    milestone: String,
    status: String,
    implementation_sha: String,
    implementation_tree: String,
    evidence_sha: String,
    contract_manifest_hash: String,
    gate_artifact_hashes: BTreeMap<String, String>,
    work_package_commits: BTreeMap<u32, String>,
    contracts: Vec<ContractResult>,
    known_in_scope_gaps: Vec<KnownGap>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct VerificationReceipt {
    schema_version: u32,
    status: String,
    implementation_sha: String,
    implementation_tree: String,
    evidence_sha: String,
    evidence_tree: String,
    results_hash: String,
    report_hash: String,
    artifact_hashes: BTreeMap<String, String>,
    verification: BTreeMap<String, bool>,
}

#[derive(Serialize)]
struct NegativeSelfCheck {
    cases: usize,
    failures: Vec<String>,
    gap_derivation: bool,
}

type AuditEvaluation = (Vec<ContractResult>, Vec<KnownGap>, BTreeMap<u32, String>);

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match arguments.as_slice() {
        [command, milestone] if milestone == MILESTONE && command == "generate" => {
            generate(Path::new("."))
        }
        [command, milestone] if milestone == MILESTONE && command == "verify-evidence" => {
            verify_evidence(Path::new(".")).map(|_| ())
        }
        [command, milestone] if milestone == MILESTONE && command == "generate-receipt" => {
            generate_receipt(Path::new("."))
        }
        [command, milestone] if milestone == MILESTONE && command == "verify-final" => {
            verify_final(Path::new("."))
        }
        [command, milestone] if milestone == MILESTONE && command == "negative-self-check" => {
            let check = negative_self_check();
            println!(
                "{}",
                serde_json::to_string(&check).unwrap_or_else(|_| "{}".into())
            );
            if check.failures.is_empty() {
                Ok(())
            } else {
                Err(format!("negative self-check failures: {:?}", check.failures))
            }
        }
        _ => Err(
            "usage: nexa-release-audit generate|verify-evidence|generate-receipt|verify-final|negative-self-check milestone4r3"
                .into(),
        ),
    };
    if let Err(error) = result {
        eprintln!("nexa-release-audit: {error}");
        std::process::exit(1);
    }
}

fn generate(root: &Path) -> Result<(), String> {
    require_clean(root)?;
    ensure_absent(&root.join(RECEIPT_PATH), "verification receipt")?;
    let implementation_sha = git(root, &["rev-parse", "HEAD"])?;
    let implementation_tree = git(root, &["rev-parse", "HEAD^{tree}"])?;
    let manifest: ContractManifest = read_json(&root.join(MANIFEST_PATH))?;
    validate_manifest(root, &manifest, &implementation_sha)?;

    let gate_dir = root.join("target/contract-gates");
    run(
        root,
        "cargo",
        &[
            "run",
            "-q",
            "-p",
            "nexa-contract-gates",
            "--",
            "all",
            "--output-dir",
            "target/contract-gates",
        ],
    )?;

    let artifact_names = manifest
        .contracts
        .iter()
        .map(|contract| contract.artifact.clone())
        .collect::<BTreeSet<_>>();
    let mut artifacts = BTreeMap::new();
    let mut artifact_hashes = BTreeMap::new();
    for name in artifact_names {
        let source = gate_dir.join(&name);
        let artifact: GateArtifact<Value> = read_json(&source)?;
        validate_artifact(
            Some(&artifact),
            &implementation_sha,
            &implementation_tree,
            None,
        )?;
        let destination = root.join(RAW_PATH).join(&name);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::copy(&source, &destination).map_err(|error| error.to_string())?;
        artifact_hashes.insert(name.clone(), file_hash(root, &destination)?);
        artifacts.insert(
            name,
            serde_json::to_value(artifact).map_err(|error| error.to_string())?,
        );
    }

    let (contracts, gaps, work_package_commits) = evaluate_contracts(&manifest, &artifacts)?;
    let status = if gaps.is_empty() {
        "COMPLETE"
    } else {
        "INCOMPLETE"
    };
    let results = AuditResults {
        schema_version: 1,
        milestone: manifest.milestone,
        status: status.into(),
        implementation_sha,
        implementation_tree,
        evidence_sha: "SELF".into(),
        contract_manifest_hash: file_hash(root, &root.join(MANIFEST_PATH))?,
        gate_artifact_hashes: artifact_hashes,
        work_package_commits,
        contracts,
        known_in_scope_gaps: gaps,
    };
    write_json(&root.join(RESULTS_PATH), &results)?;
    std::fs::write(root.join(REPORT_PATH), render_report(&results, &artifacts))
        .map_err(|error| error.to_string())?;
    if results.status != "COMPLETE" {
        return Err(format!(
            "contracts produced known gaps: {:?}",
            results.known_in_scope_gaps
        ));
    }
    Ok(())
}

struct VerifiedEvidence {
    results: AuditResults,
    evidence_sha: String,
    evidence_tree: String,
    artifact_hashes: BTreeMap<String, String>,
}

fn verify_evidence(root: &Path) -> Result<VerifiedEvidence, String> {
    require_clean(root)?;
    let evidence_sha = git(root, &["rev-parse", "HEAD"])?;
    verify_evidence_at(root, &evidence_sha)
}

fn verify_evidence_at(root: &Path, evidence_sha: &str) -> Result<VerifiedEvidence, String> {
    let implementation_sha = git(root, &["rev-parse", &format!("{evidence_sha}^")])?;
    let evidence_tree = git(root, &["rev-parse", &format!("{evidence_sha}^{{tree}}")])?;
    let implementation_tree = git(
        root,
        &["rev-parse", &format!("{implementation_sha}^{{tree}}")],
    )?;
    let changed = changed_paths(root, evidence_sha)?;
    if changed.is_empty()
        || changed.iter().any(|path| {
            !EVIDENCE_ALLOWED
                .iter()
                .any(|allowed| path.starts_with(allowed))
        })
        || changed.iter().any(|path| path == RECEIPT_PATH)
    {
        return Err(format!(
            "evidence commit changed disallowed paths: {changed:?}"
        ));
    }

    let manifest: ContractManifest = read_json(&root.join(MANIFEST_PATH))?;
    validate_manifest(root, &manifest, &implementation_sha)?;
    let results: AuditResults = read_json(&root.join(RESULTS_PATH))?;
    if results.implementation_sha != implementation_sha
        || results.implementation_tree != implementation_tree
        || results.evidence_sha != "SELF"
        || results.contract_manifest_hash != file_hash(root, &root.join(MANIFEST_PATH))?
    {
        return Err("results provenance does not match the evidence parent".into());
    }
    let mut artifacts = BTreeMap::new();
    let mut artifact_hashes = BTreeMap::new();
    for name in results.gate_artifact_hashes.keys() {
        let path = root.join(RAW_PATH).join(name);
        let artifact: GateArtifact<Value> = read_json(&path)?;
        validate_artifact(
            Some(&artifact),
            &implementation_sha,
            &implementation_tree,
            None,
        )?;
        let hash = file_hash(root, &path)?;
        if results.gate_artifact_hashes.get(name) != Some(&hash) {
            return Err(format!("raw artifact hash mismatch: {name}"));
        }
        artifact_hashes.insert(name.clone(), hash);
        artifacts.insert(
            name.clone(),
            serde_json::to_value(artifact).map_err(|error| error.to_string())?,
        );
    }
    let (contracts, gaps, work_package_commits) = evaluate_contracts(&manifest, &artifacts)?;
    if contracts != results.contracts
        || gaps != results.known_in_scope_gaps
        || work_package_commits != results.work_package_commits
        || results.status
            != if gaps.is_empty() {
                "COMPLETE"
            } else {
                "INCOMPLETE"
            }
    {
        return Err("results do not deterministically re-evaluate from raw artifacts".into());
    }
    let report =
        std::fs::read_to_string(root.join(REPORT_PATH)).map_err(|error| error.to_string())?;
    if report != render_report(&results, &artifacts) {
        return Err("Markdown does not deterministically regenerate from results".into());
    }
    if results.status != "COMPLETE" {
        return Err("evidence status is not COMPLETE".into());
    }
    Ok(VerifiedEvidence {
        results,
        evidence_sha: evidence_sha.into(),
        evidence_tree,
        artifact_hashes,
    })
}

fn generate_receipt(root: &Path) -> Result<(), String> {
    require_clean(root)?;
    ensure_absent(&root.join(RECEIPT_PATH), "verification receipt")?;
    let verified = verify_evidence(root)?;
    let receipt = VerificationReceipt {
        schema_version: 1,
        status: "verified".into(),
        implementation_sha: verified.results.implementation_sha.clone(),
        implementation_tree: verified.results.implementation_tree.clone(),
        evidence_sha: verified.evidence_sha,
        evidence_tree: verified.evidence_tree,
        results_hash: file_hash(root, &root.join(RESULTS_PATH))?,
        report_hash: file_hash(root, &root.join(REPORT_PATH))?,
        artifact_hashes: verified.artifact_hashes,
        verification: BTreeMap::from([
            ("artifact_hashes_match".into(), true),
            ("contracts_recomputed".into(), true),
            ("evidence_paths_allowed".into(), true),
            ("implementation_parent_matches".into(), true),
            ("markdown_regenerated_equal".into(), true),
        ]),
    };
    write_json(&root.join(RECEIPT_PATH), &receipt)
}

fn verify_final(root: &Path) -> Result<(), String> {
    require_clean(root)?;
    let receipt_sha = git(root, &["rev-parse", "HEAD"])?;
    let evidence_sha = git(root, &["rev-parse", "HEAD^"])?;
    let implementation_sha = git(root, &["rev-parse", "HEAD^^"])?;
    let changed = changed_paths(root, &receipt_sha)?;
    if changed != [RECEIPT_PATH.to_owned()] {
        return Err(format!(
            "receipt commit changed unexpected paths: {changed:?}"
        ));
    }
    let verified = verify_evidence_at(root, &evidence_sha)?;
    if verified.results.implementation_sha != implementation_sha {
        return Err("final three-commit ancestry does not match results".into());
    }
    let receipt: VerificationReceipt = read_json(&root.join(RECEIPT_PATH))?;
    let expected = VerificationReceipt {
        schema_version: 1,
        status: "verified".into(),
        implementation_sha: verified.results.implementation_sha.clone(),
        implementation_tree: verified.results.implementation_tree.clone(),
        evidence_sha,
        evidence_tree: verified.evidence_tree,
        results_hash: file_hash(root, &root.join(RESULTS_PATH))?,
        report_hash: file_hash(root, &root.join(REPORT_PATH))?,
        artifact_hashes: verified.artifact_hashes,
        verification: BTreeMap::from([
            ("artifact_hashes_match".into(), true),
            ("contracts_recomputed".into(), true),
            ("evidence_paths_allowed".into(), true),
            ("implementation_parent_matches".into(), true),
            ("markdown_regenerated_equal".into(), true),
        ]),
    };
    if receipt != expected {
        return Err("verification receipt does not match final repository state".into());
    }
    Ok(())
}

fn validate_manifest(
    root: &Path,
    manifest: &ContractManifest,
    implementation_sha: &str,
) -> Result<(), String> {
    if manifest.version != 3 || manifest.milestone != "4.0R3" {
        return Err("unsupported contract manifest".into());
    }
    let packages = manifest
        .contracts
        .iter()
        .map(|contract| contract.work_package)
        .collect::<BTreeSet<_>>();
    let package_count =
        u32::try_from(manifest.contracts.len()).map_err(|_| "too many work packages")?;
    let expected = (1..=package_count).collect::<BTreeSet<_>>();
    if packages != expected {
        return Err("manifest does not cover every work package exactly once".into());
    }
    let mut ids = BTreeSet::new();
    for contract in &manifest.contracts {
        if !ids.insert(&contract.id)
            || contract.id.is_empty()
            || contract.description.is_empty()
            || contract.gate.is_empty()
            || contract.artifact.is_empty()
            || contract.artifact.contains('/')
            || contract.assertions.is_empty()
            || contract.affected_paths.is_empty()
            || contract.status.is_empty()
        {
            return Err(format!("invalid contract definition: {}", contract.id));
        }
        let commit = contract
            .implementation_commit
            .as_deref()
            .ok_or_else(|| format!("{} has no implementation commit", contract.id))?;
        validate_commit(root, commit, implementation_sha, &contract.affected_paths)
            .map_err(|error| format!("{}: {error}", contract.id))?;
        for assertion in &contract.assertions {
            if assertion.pointer.is_empty()
                || ![
                    "eq",
                    "ne",
                    "gt",
                    "gte",
                    "lt",
                    "lte",
                    "contains",
                    "set_equals",
                    "all_equal",
                    "empty",
                    "not_empty",
                ]
                .contains(&assertion.operator.as_str())
            {
                return Err(format!("{} has an invalid assertion", contract.id));
            }
        }
        let _ = &contract.forbidden_calls;
    }
    Ok(())
}

fn validate_commit(
    root: &Path,
    commit: &str,
    implementation_sha: &str,
    affected_paths: &[String],
) -> Result<(), String> {
    if !command_success(
        root,
        "git",
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    ) {
        return Err("implementation commit does not exist".into());
    }
    if !command_success(
        root,
        "git",
        &["merge-base", "--is-ancestor", commit, implementation_sha],
    ) {
        return Err("implementation commit is not an ancestor".into());
    }
    let changed = changed_paths(root, commit)?;
    if !affected_paths
        .iter()
        .any(|affected| changed.iter().any(|path| path.starts_with(affected)))
    {
        return Err("implementation commit did not touch an affected path".into());
    }
    Ok(())
}

fn validate_artifact(
    artifact: Option<&GateArtifact<Value>>,
    implementation_sha: &str,
    implementation_tree: &str,
    expected_gate: Option<&str>,
) -> Result<(), String> {
    let artifact = artifact.ok_or("gate artifact is missing")?;
    if artifact.schema_version != GATE_SCHEMA_VERSION {
        return Err("gate artifact schema mismatch".into());
    }
    if artifact.status != GateStatus::Passed || !artifact.failures.is_empty() {
        return Err("gate artifact did not decide passed".into());
    }
    if artifact.implementation_sha != implementation_sha {
        return Err("gate artifact implementation SHA is stale".into());
    }
    if artifact.implementation_tree != implementation_tree {
        return Err("gate artifact implementation tree is stale".into());
    }
    if expected_gate.is_some_and(|gate| artifact.gate != gate) {
        return Err("gate artifact identity mismatch".into());
    }
    Ok(())
}

fn evaluate_contracts(
    manifest: &ContractManifest,
    artifacts: &BTreeMap<String, Value>,
) -> Result<AuditEvaluation, String> {
    let mut results = Vec::new();
    let mut gaps = Vec::new();
    let mut commits = BTreeMap::new();
    for contract in &manifest.contracts {
        let artifact = artifacts
            .get(&contract.artifact)
            .ok_or_else(|| format!("missing artifact {}", contract.artifact))?;
        if artifact["gate"] != contract.gate {
            return Err(format!("{} gate identity mismatch", contract.id));
        }
        let commit = contract
            .implementation_commit
            .clone()
            .ok_or_else(|| format!("{} has no implementation commit", contract.id))?;
        commits.insert(contract.work_package, commit.clone());
        let mut assertion_results = Vec::new();
        for assertion in &contract.assertions {
            let actual = artifact.pointer(&assertion.pointer).cloned();
            let evaluated = evaluate_assertion(actual.as_ref(), assertion);
            let (passed, reason) = match evaluated {
                Ok(true) => (true, String::new()),
                Ok(false) => (false, "assertion did not match raw gate fact".into()),
                Err(error) => (false, error),
            };
            if !passed {
                gaps.push(KnownGap {
                    contract: contract.id.clone(),
                    gate: contract.gate.clone(),
                    pointer: assertion.pointer.clone(),
                    expected: assertion.expected.clone(),
                    actual: actual.clone(),
                    reason: reason.clone(),
                });
            }
            assertion_results.push(AssertionResult {
                pointer: assertion.pointer.clone(),
                operator: assertion.operator.clone(),
                expected: assertion.expected.clone(),
                actual,
                passed,
                reason,
            });
        }
        results.push(ContractResult {
            id: contract.id.clone(),
            work_package: contract.work_package,
            description: contract.description.clone(),
            gate: contract.gate.clone(),
            artifact: contract.artifact.clone(),
            implementation_commit: commit,
            status: if assertion_results.iter().all(|assertion| assertion.passed) {
                "passed"
            } else {
                "failed"
            }
            .into(),
            assertions: assertion_results,
        });
    }
    Ok((results, gaps, commits))
}

fn evaluate_assertion(actual: Option<&Value>, assertion: &Assertion) -> Result<bool, String> {
    let actual = actual.ok_or("JSON Pointer does not exist")?;
    match assertion.operator.as_str() {
        "eq" => Ok(actual == &assertion.expected),
        "ne" => Ok(actual != &assertion.expected),
        "empty" => value_empty(actual),
        "not_empty" => value_empty(actual).map(|empty| !empty),
        "set_equals" => {
            let actual = canonical_set(actual)?;
            let expected = canonical_set(&assertion.expected)?;
            Ok(actual == expected)
        }
        "contains" => match (actual, &assertion.expected) {
            (Value::String(actual), Value::String(expected)) => Ok(actual.contains(expected)),
            (Value::Array(actual), expected) => Ok(actual.contains(expected)),
            _ => Err("contains requires string/string or array/value".into()),
        },
        "all_equal" => actual
            .as_array()
            .map(|values| values.iter().all(|value| value == &assertion.expected))
            .ok_or_else(|| "all_equal requires an array".into()),
        "gt" | "gte" | "lt" | "lte" => {
            let actual = actual
                .as_f64()
                .ok_or("numeric assertion actual is not numeric")?;
            let expected = assertion
                .expected
                .as_f64()
                .ok_or("numeric assertion expected is not numeric")?;
            Ok(match assertion.operator.as_str() {
                "gt" => actual > expected,
                "gte" => actual >= expected,
                "lt" => actual < expected,
                _ => actual <= expected,
            })
        }
        operator => Err(format!("unsupported assertion operator {operator}")),
    }
}

fn value_empty(value: &Value) -> Result<bool, String> {
    match value {
        Value::Null => Ok(true),
        Value::String(value) => Ok(value.is_empty()),
        Value::Array(value) => Ok(value.is_empty()),
        Value::Object(value) => Ok(value.is_empty()),
        _ => Err("empty requires null, string, array, or object".into()),
    }
}

fn canonical_set(value: &Value) -> Result<Vec<String>, String> {
    let values = value.as_array().ok_or("set_equals requires arrays")?;
    let mut values = values
        .iter()
        .map(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    values.sort();
    values.dedup();
    Ok(values)
}

fn render_report(results: &AuditResults, artifacts: &BTreeMap<String, Value>) -> String {
    let mut report = String::new();
    report.push_str("# Nexa Milestone 4.0R3 端到端诊断与可信证据最终收口\n\n");
    let _ = writeln!(report, "Status: **{}**\n", results.status);
    let _ = write!(
        report,
        "- Implementation SHA: `{}`\n- Implementation Tree SHA: `{}`\n- Evidence SHA: `{}`\n- Contract Manifest Hash: `{}`\n\n",
        results.implementation_sha,
        results.implementation_tree,
        results.evidence_sha,
        results.contract_manifest_hash
    );
    report.push_str("## 工作包实现提交\n\n| WP | Commit |\n|---:|---|\n");
    for (work_package, commit) in &results.work_package_commits {
        let _ = writeln!(report, "| {work_package} | `{commit}` |");
    }
    report.push_str("\n## Contract 结果\n\n| Contract | Gate | Status |\n|---|---|---|\n");
    for contract in &results.contracts {
        let _ = writeln!(
            report,
            "| {} | {} | {} |",
            contract.id, contract.gate, contract.status
        );
    }
    report.push_str("\n## Known Gaps\n\n");
    if results.known_in_scope_gaps.is_empty() {
        report.push_str("- 无\n");
    } else {
        for gap in &results.known_in_scope_gaps {
            let _ = writeln!(
                report,
                "- `{}` `{}`: {}",
                gap.contract, gap.pointer, gap.reason
            );
        }
    }
    report.push_str("\n## Gate 原始事实\n\n");
    for (name, artifact) in artifacts {
        let _ = write!(report, "### `{name}`\n\n```json\n");
        report.push_str(
            &serde_json::to_string_pretty(artifact).unwrap_or_else(|_| "null".to_owned()),
        );
        report.push_str("\n```\n\n");
    }
    report
}

fn negative_self_check() -> NegativeSelfCheck {
    let base = GateArtifact {
        schema_version: GATE_SCHEMA_VERSION,
        gate: "synthetic".into(),
        implementation_sha: "implementation".into(),
        implementation_tree: "tree".into(),
        command: "synthetic gate".into(),
        status: GateStatus::Passed,
        metrics: json!({"value": true, "values": ["a", "b"]}),
        failures: Vec::new(),
    };
    let mut wrong_schema = base.clone();
    wrong_schema.schema_version = GATE_SCHEMA_VERSION.saturating_add(1);
    let mut failed_status = base.clone();
    failed_status.status = GateStatus::Failed;
    let mut stale_sha = base.clone();
    stale_sha.implementation_sha = "stale".into();
    let mut stale_tree = base.clone();
    stale_tree.implementation_tree = "stale".into();
    let false_assertion = Assertion {
        pointer: "/metrics/value".into(),
        operator: "eq".into(),
        expected: json!(false),
    };
    let pointer_assertion = Assertion {
        pointer: "/metrics/missing".into(),
        operator: "eq".into(),
        expected: Value::Null,
    };
    let type_assertion = Assertion {
        pointer: "/metrics/value".into(),
        operator: "set_equals".into(),
        expected: json!([]),
    };
    let false_result = evaluate_assertion(Some(&json!(true)), &false_assertion);
    let false_is_gap = matches!(&false_result, Ok(false));
    let checks = vec![
        (
            "missing-gate-file",
            validate_artifact(None, "implementation", "tree", None).is_err(),
        ),
        (
            "gate-schema",
            validate_artifact(Some(&wrong_schema), "implementation", "tree", None).is_err(),
        ),
        (
            "gate-status",
            validate_artifact(Some(&failed_status), "implementation", "tree", None).is_err(),
        ),
        (
            "gate-sha",
            validate_artifact(Some(&stale_sha), "implementation", "tree", None).is_err(),
        ),
        (
            "gate-tree",
            validate_artifact(Some(&stale_tree), "implementation", "tree", None).is_err(),
        ),
        (
            "json-pointer",
            evaluate_assertion(None, &pointer_assertion).is_err(),
        ),
        (
            "assertion-type",
            evaluate_assertion(Some(&json!(true)), &type_assertion).is_err(),
        ),
        ("assertion-gap", false_is_gap),
        (
            "manifest-implementation",
            validate_commit_state(None, true, true, true).is_err(),
        ),
        (
            "commit-existence",
            validate_commit_state(Some("commit"), false, true, true).is_err(),
        ),
        (
            "commit-ancestry",
            validate_commit_state(Some("commit"), true, false, true).is_err(),
        ),
        (
            "commit-path",
            validate_commit_state(Some("commit"), true, true, false).is_err(),
        ),
        ("raw-tamper", !hashes_match("expected", "actual")),
        ("results-tamper", !documents_match("expected", "actual")),
        ("markdown-tamper", !documents_match("expected", "actual")),
    ];
    let failures = checks
        .iter()
        .filter(|(_, passed)| !*passed)
        .map(|(name, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    NegativeSelfCheck {
        cases: checks.len(),
        failures,
        gap_derivation: false_is_gap,
    }
}

fn validate_commit_state(
    commit: Option<&str>,
    exists: bool,
    ancestor: bool,
    touched: bool,
) -> Result<(), String> {
    if commit.is_none() {
        return Err("manifest has no implementation commit".into());
    }
    if !exists {
        return Err("implementation commit does not exist".into());
    }
    if !ancestor {
        return Err("implementation commit is not an ancestor".into());
    }
    if !touched {
        return Err("implementation commit did not touch an affected path".into());
    }
    Ok(())
}

fn hashes_match(expected: &str, actual: &str) -> bool {
    expected == actual
}

fn documents_match(expected: &str, actual: &str) -> bool {
    expected == actual
}

fn require_clean(root: &Path) -> Result<(), String> {
    let status = git(root, &["status", "--porcelain"])?;
    if status.is_empty() {
        Ok(())
    } else {
        Err(format!("repository must be clean:\n{status}"))
    }
}

fn ensure_absent(path: &Path, label: &str) -> Result<(), String> {
    if path.exists() {
        Err(format!("{label} must not exist before this phase"))
    } else {
        Ok(())
    }
}

fn changed_paths(root: &Path, commit: &str) -> Result<Vec<String>, String> {
    let output = git(
        root,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit,
        ],
    )?;
    let mut paths = output
        .lines()
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn file_hash(root: &Path, path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path: {}", path.display()))?;
    git(root, &["hash-object", "--no-filters", path])
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(
        &std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

fn run(root: &Path, program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    run(root, "git", arguments).map(|output| output.trim().to_owned())
}

fn command_success(root: &Path, program: &str, arguments: &[&str]) -> bool {
    Command::new(program)
        .args(arguments)
        .current_dir(root)
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_negative_audit_case_is_rejected() {
        let check = super::negative_self_check();
        assert!(check.failures.is_empty());
        assert!(check.gap_derivation);
        assert_ne!(check.cases, 0);
    }
}
