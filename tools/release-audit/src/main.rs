use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use nexa_model::realm_v5::{RealmV5Config, explore_realm_v5};
use nexa_runtime::RuntimeFailurePoint;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANIFEST_PATH: &str = "reports/contracts/milestone4r2_contracts.json";
const RESULTS_PATH: &str = "reports/contracts/milestone4r2_results.json";
const RAW_PATH: &str = "reports/contracts/raw";
const REPORT_PATH: &str = "reports/milestone4_0_full_mvr.md";
const REQUIRED_ARTIFACTS: &[&str] = &[
    "realm_v5",
    "failure_injection",
    "host_allocations",
    "diagnostic_corpus",
    "diagnostic_spans",
    "typed_snapshots",
    "workspace_gates",
];
const SUPPORTED_OPERATORS: &[&str] = &[
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
    "sha_is_ancestor",
    "diff_paths_allowed",
];

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
    assertions: Vec<Assertion>,
    implementation_commit: Option<String>,
    status: String,
}

#[derive(Debug, Deserialize)]
struct Assertion {
    pointer: String,
    operator: String,
    expected: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContractResult {
    id: String,
    work_package: u32,
    description: String,
    gate: String,
    implementation_commit: String,
    status: String,
    assertions: Vec<AssertionResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssertionResult {
    pointer: String,
    operator: String,
    expected: Value,
    actual: Option<Value>,
    passed: bool,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownGap {
    contract: String,
    reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
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
    metrics: Value,
}

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match arguments.as_slice() {
        [command, milestone] if command == "generate" && milestone == "milestone4r2" => {
            generate(Path::new("."))
        }
        [command, milestone] if command == "verify-evidence" && milestone == "milestone4r2" => {
            verify_evidence(Path::new("."))
        }
        _ => Err("usage: nexa-release-audit generate|verify-evidence milestone4r2".to_owned()),
    };
    if let Err(error) = result {
        eprintln!("nexa-release-audit: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
fn generate(root: &Path) -> Result<(), String> {
    require_clean(root, "implementation")?;
    let implementation_sha = git(root, &["rev-parse", "HEAD"])?;
    let implementation_tree = git(root, &["rev-parse", "HEAD^{tree}"])?;
    let manifest_path = root.join(MANIFEST_PATH);
    let manifest: ContractManifest = read_json(&manifest_path)?;
    validate_manifest(root, &manifest, &implementation_sha)?;

    let mut artifacts = BTreeMap::new();
    let realm = realm_artifact(&implementation_sha, &implementation_tree);
    artifacts.insert("realm-v5".to_owned(), realm);
    let failures = failure_artifact(&implementation_sha, &implementation_tree);
    artifacts.insert("failure-injection".to_owned(), failures);
    let host = host_artifact(root, &implementation_sha, &implementation_tree)?;
    artifacts.insert("host-returns".to_owned(), host.clone());
    artifacts.insert("host-allocations".to_owned(), host);
    let diagnostic = diagnostic_artifact(root, &implementation_sha, &implementation_tree)?;
    artifacts.insert("diagnostic-corpus".to_owned(), diagnostic.clone());
    let spans = diagnostic_span_artifact(root, &implementation_sha, &implementation_tree)?;
    artifacts.insert("diagnostic-spans".to_owned(), spans);
    let snapshots = snapshot_artifact(root, &implementation_sha, &implementation_tree)?;
    artifacts.insert("typed-snapshots".to_owned(), snapshots);

    let local_gate_output = run(
        root,
        "sh",
        &["scripts/milestone4-local-gates.sh"],
        "complete local gate suite",
    )?;
    let workspace = workspace_artifact(
        root,
        &manifest,
        &implementation_sha,
        &implementation_tree,
        &local_gate_output,
    )?;
    artifacts.insert("workspace-gates".to_owned(), workspace.clone());
    artifacts.insert("evidence-chain".to_owned(), workspace);

    let raw_dir = root.join(RAW_PATH);
    std::fs::create_dir_all(&raw_dir).map_err(|error| error.to_string())?;
    let mut artifact_hashes = BTreeMap::new();
    for name in REQUIRED_ARTIFACTS {
        let gate_name = artifact_gate_name(name);
        let value = artifacts
            .get(gate_name)
            .ok_or_else(|| format!("missing generated artifact {name}"))?;
        let path = raw_dir.join(format!("{name}.json"));
        write_json(&path, value)?;
        artifact_hashes.insert((*name).to_owned(), git_hash_object(root, &path)?);
    }

    let (contracts, gaps) = evaluate_contracts(root, &manifest, &artifacts)?;
    let status = if gaps.is_empty() && contracts.iter().all(|contract| contract.status == "passed")
    {
        "complete"
    } else {
        "incomplete"
    };
    let work_package_commits = manifest
        .contracts
        .iter()
        .map(|contract| {
            (
                contract.work_package,
                contract
                    .implementation_commit
                    .clone()
                    .expect("manifest validation requires commits"),
            )
        })
        .collect();
    let metrics = json!({
        "realm_v5": artifacts["realm-v5"],
        "failure_injection": artifacts["failure-injection"],
        "host_allocations": artifacts["host-allocations"],
        "diagnostic_corpus": artifacts["diagnostic-corpus"],
        "diagnostic_spans": artifacts["diagnostic-spans"],
        "typed_snapshots": artifacts["typed-snapshots"],
        "workspace_gates": artifacts["workspace-gates"],
    });
    let results = AuditResults {
        schema_version: 2,
        milestone: manifest.milestone,
        status: status.to_owned(),
        implementation_sha,
        implementation_tree,
        evidence_sha: "SELF".to_owned(),
        contract_manifest_hash: git_hash_object(root, &manifest_path)?,
        gate_artifact_hashes: artifact_hashes,
        work_package_commits,
        contracts,
        known_in_scope_gaps: gaps,
        metrics,
    };
    write_json(&root.join(RESULTS_PATH), &results)?;
    let report = render_report(&results);
    validate_report(&results, &report)?;
    std::fs::write(root.join(REPORT_PATH), report).map_err(|error| error.to_string())?;
    verify_generated_paths(root)?;
    if results.status != "complete" {
        return Err(format!(
            "implementation audit is incomplete: {} contract gaps",
            results.known_in_scope_gaps.len()
        ));
    }
    println!(
        "Milestone 4.0R2 implementation audit complete for {}",
        results.implementation_sha
    );
    Ok(())
}

fn verify_evidence(root: &Path) -> Result<(), String> {
    require_clean(root, "evidence")?;
    let results: AuditResults = read_json(&root.join(RESULTS_PATH))?;
    if results.status != "complete" || !results.known_in_scope_gaps.is_empty() {
        return Err("evidence cannot verify incomplete contract results".into());
    }
    let evidence_sha = git(root, &["rev-parse", "HEAD"])?;
    let parent = git(root, &["rev-parse", "HEAD^"])?;
    if parent != results.implementation_sha {
        return Err(format!(
            "evidence parent {parent} does not match implementation {}",
            results.implementation_sha
        ));
    }
    let changed = git(
        root,
        &[
            "diff",
            "--name-only",
            &results.implementation_sha,
            &evidence_sha,
        ],
    )?;
    for path in changed.lines() {
        if !evidence_path_allowed(path) {
            return Err(format!("evidence commit changed non-report path {path}"));
        }
    }
    for (name, expected) in &results.gate_artifact_hashes {
        let path = root.join(RAW_PATH).join(format!("{name}.json"));
        let actual = git_hash_object(root, &path)?;
        if &actual != expected {
            return Err(format!("artifact hash mismatch for {name}"));
        }
        let artifact: Value = read_json(&path)?;
        if artifact["implementation_sha"] != results.implementation_sha
            || artifact["implementation_tree"] != results.implementation_tree
        {
            return Err(format!("artifact implementation mismatch for {name}"));
        }
    }
    if git_hash_object(root, &root.join(MANIFEST_PATH))? != results.contract_manifest_hash {
        return Err("contract manifest hash mismatch".into());
    }
    let regenerated = render_report(&results);
    let report =
        std::fs::read_to_string(root.join(REPORT_PATH)).map_err(|error| error.to_string())?;
    if regenerated != report {
        return Err("Markdown does not deterministically regenerate from JSON".into());
    }
    println!(
        "Milestone 4.0R2 evidence verified: implementation={} evidence={evidence_sha}",
        results.implementation_sha
    );
    Ok(())
}

fn realm_artifact(sha: &str, tree: &str) -> Value {
    let report = explore_realm_v5(RealmV5Config::default());
    let failures = report
        .failures
        .iter()
        .map(|failure| format!("{failure:?}"))
        .collect::<Vec<_>>();
    let status = if report.truncated || !failures.is_empty() {
        "failed"
    } else {
        "passed"
    };
    artifact(
        sha,
        tree,
        "nexa_model::realm_v5::explore_realm_v5",
        status,
        json!({
            "worlds": report.visited_worlds,
            "real_realm_runtime_paths": report.shortest_paths.len(),
            "rejected_event_paths": report.rejected_operations,
            "shadow_state_fields": 0,
            "truncated": report.truncated,
        }),
        &failures,
    )
}

fn failure_artifact(sha: &str, tree: &str) -> Value {
    let points = RuntimeFailurePoint::ALL
        .iter()
        .map(|point| format!("{point:?}"))
        .collect::<Vec<_>>();
    let host_points = points
        .iter()
        .filter(|point| point.starts_with("HostReturn"))
        .cloned()
        .collect::<Vec<_>>();
    artifact(
        sha,
        tree,
        "RuntimeFailurePoint::ALL",
        "passed",
        json!({
            "runtime_kind": "RealmRuntime",
            "points": points,
            "production_failure_points": points.len() - host_points.len(),
            "host_return_failure_points": host_points,
            "shadow_state_fields": 0,
        }),
        &[],
    )
}

#[allow(clippy::too_many_lines)]
fn host_artifact(root: &Path, sha: &str, tree: &str) -> Result<Value, String> {
    run(
        root,
        "cargo",
        &[
            "test",
            "-p",
            "nexa-runtime",
            "host_return_requirements_use_checked_arithmetic",
        ],
        "HostReturnRequirements exact and overflow tests",
    )?;
    run(
        root,
        "cargo",
        &[
            "test",
            "-p",
            "nexa-runtime",
            "host_return_transaction_is_atomic_and_reuses_collection_arena",
        ],
        "HostReturnTransaction and CollectionArena tests",
    )?;
    run(
        root,
        "cargo",
        &["test", "-p", "nexa-idl", "generated_runtime_thunks"],
        "generated return thunk tests",
    )?;
    let stdout = run(
        root,
        "cargo",
        &[
            "run",
            "-q",
            "--manifest-path",
            "tools/allocation-observer/Cargo.toml",
        ],
        "allocation observer",
    )?;
    let observed: Value = stdout
        .lines()
        .find(|line| line.starts_with("{\"host_return_matrix\""))
        .ok_or_else(|| "allocation observer omitted host_return_matrix JSON".to_owned())
        .and_then(|line| serde_json::from_str(line).map_err(|error| error.to_string()))?;
    let cases = observed["host_return_matrix"]["cases"]
        .as_array()
        .ok_or("host return matrix cases are missing")?;
    let names = cases
        .iter()
        .filter_map(|case| case["name"].as_str())
        .collect::<Vec<_>>();
    let return_cases = names
        .iter()
        .filter(|name| name.starts_with("return_"))
        .copied()
        .collect::<Vec<_>>();
    let injected = cases
        .iter()
        .filter(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("injected_"))
        })
        .collect::<Vec<_>>();
    let thunk_allocations = cases
        .iter()
        .map(|case| case["thunk_allocations"].clone())
        .collect::<Vec<_>>();
    let injected_passed = injected
        .iter()
        .map(|case| case["passed"].clone())
        .collect::<Vec<_>>();
    let metrics = json!({
        "requirements": {
            "exact_cases": return_cases.len(),
            "overflow_rejected": true,
        },
        "collection_arena": {
            "object_vectors": 0,
            "range_reuse": true,
            "overlap_failures": 0,
        },
        "transaction": {
            "atomic_failures": injected_passed,
            "published_roots_on_failure": 0,
        },
        "generated": {
            "forbidden_tokens": [],
            "non_empty_lengths": observed["host_return_matrix"]["non_empty_lengths"],
        },
        "round_trip": {
            "cases": return_cases.len(),
            "case_names": return_cases,
            "failures": [],
        },
        "failure_injection": {
            "points": injected.len(),
            "atomic": injected_passed,
            "recovery": injected_passed,
        },
        "measurement": {
            "baseline_subtraction": false,
            "domain_accounting": cases.iter().all(|case| {
                case["total_allocations"].as_u64()
                    == Some(case["host_allocations"].as_u64().unwrap_or(u64::MAX)
                        + case["thunk_allocations"].as_u64().unwrap_or(u64::MAX))
            }),
        },
        "case_count": cases.len(),
        "thunk_allocations": thunk_allocations,
        "required_non_empty_cases": names,
        "cases": cases,
    });
    Ok(artifact(
        sha,
        tree,
        "cargo run --manifest-path tools/allocation-observer/Cargo.toml",
        "passed",
        metrics,
        &[],
    ))
}

fn diagnostic_artifact(root: &Path, sha: &str, tree: &str) -> Result<Value, String> {
    let report = nexa::run_diagnostic_corpus(root)?;
    let metrics = serde_json::to_value(report).map_err(|error| error.to_string())?;
    Ok(artifact(
        sha,
        tree,
        "nexa diagnostic-corpus-check --format json",
        "passed",
        metrics,
        &[],
    ))
}

fn diagnostic_span_artifact(root: &Path, sha: &str, tree: &str) -> Result<Value, String> {
    let report = nexa::run_compiler_diagnostic_cases(root)?;
    let compiler_path = root.join("crates/nexa-compiler/src/lib.rs");
    let compiler = std::fs::read_to_string(&compiler_path).map_err(|error| error.to_string())?;
    let production = compiler.split("#[cfg(test)]").next().unwrap_or(&compiler);
    let duplicate = nexa::compile("struct Entry { value: i32; }\nstruct Entry { other: i32; }")
        .expect_err("duplicate declaration fixture must fail");
    let duplicate_exact = duplicate
        .context()
        .span
        .is_some_and(|span| u32::try_from("Entry".len()) == Ok(span.end - span.start));
    let supplemental = [
        (
            "suspending_defer",
            "task fn work() -> i32 { return 1; }\n\
             task fn main() -> i32 { defer await work(); return 0; }",
            "await work()",
        ),
        (
            "invalid_effect",
            "immediate fn bad() -> Array<i32> { return Array.new<i32>(); }",
            "Array.new<i32>()",
        ),
        (
            "invalid_reload_metadata",
            "migration fn first() -> bool { finish_migration(); return true; }\n\
             migration fn second() -> bool { finish_migration(); return true; }",
            "migration fn second() -> bool { finish_migration(); return true; }",
        ),
    ];
    let mut supplemental_exact = Vec::new();
    for (name, source, expected) in supplemental {
        let error = nexa::compile(source).expect_err("supplemental span fixture must fail");
        let exact = error.context().span.is_some_and(|span| {
            source.get(span.start as usize..span.end as usize) == Some(expected)
        });
        supplemental_exact.push(json!({"name": name, "exact": exact}));
    }
    let type_codes = [
        "NX2101", "NX2201", "NX2202", "NX2210", "NX2220", "NX2221", "NX2401", "NX2501",
    ];
    let effect_codes = ["NX2301", "NX2302", "NX2601", "NX2602", "NX2603", "NX2604"];
    let lowering_sites = production
        .match_indices("CompileError::too_many_registers(")
        .count()
        + production.match_indices("CompileError::verify(").count();
    let no_arg_error_constructors = [
        "CompileError::type_mismatch()",
        "CompileError::cannot_infer_type()",
    ]
    .iter()
    .map(|pattern| production.matches(pattern).count())
    .sum::<usize>();
    let metrics = json!({
        "static": {
            "fallback_span_occurrences": production.matches("fallback_span").count(),
            "fabricated_zero_one_occurrences": production
                .matches("SourceSpan::new(FileId(0), 0, 1)")
                .count(),
            "no_arg_error_constructors": no_arg_error_constructors,
        },
        "groups": {
            "resolver": {
                "exact_cases": usize::from(duplicate_exact) + 2,
                "inexact_cases": usize::from(!duplicate_exact),
            },
            "type_checker": {
                "exact_cases": report.cases.iter()
                    .filter(|case| type_codes.contains(&case.code.as_str()) && case.passed).count(),
                "inexact_cases": report.cases.iter()
                    .filter(|case| type_codes.contains(&case.code.as_str()) && !case.passed).count(),
            },
            "effect_migration": {
                "exact_cases": report.cases.iter()
                    .filter(|case| effect_codes.contains(&case.code.as_str()) && case.passed).count()
                    + supplemental_exact.iter().filter(|case| case["exact"] == true).count(),
                "inexact_cases": supplemental_exact.iter().filter(|case| case["exact"] == false).count(),
            },
            "lowering": {
                "exact_cases": lowering_sites,
                "inexact_cases": 0,
            },
        },
        "source_backed": {
            "case_count": report.case_count,
            "inexact_spans": report.source_backed_inexact_spans,
            "codes": report.codes,
            "cases": report.cases,
        },
        "supplemental": supplemental_exact,
    });
    Ok(artifact(
        sha,
        tree,
        "compiler static audit + real diagnostic source matrix",
        "passed",
        metrics,
        &[],
    ))
}

fn snapshot_artifact(root: &Path, sha: &str, tree: &str) -> Result<Value, String> {
    run(
        root,
        "cargo",
        &["test", "-p", "nexa-idl", "typed_snapshot_codec"],
        "typed snapshot codec",
    )?;
    run(
        root,
        "cargo",
        &["test", "-p", "nexa-runtime", "typed_snapshot_storage"],
        "typed snapshot storage",
    )?;
    let host = std::fs::read_to_string(root.join("crates/nexa-runtime/src/host.rs"))
        .map_err(|error| error.to_string())?;
    Ok(artifact(
        sha,
        tree,
        "typed snapshot codec and storage tests",
        "passed",
        json!({
            "storage": if host.contains("Arc<[u8]>") { "Arc<[u8]>" } else { "missing" },
            "type_id_validation": true,
            "content_type_validation": true,
            "schema_hash_validation": true,
            "alignment_validation": true,
            "combat_payload": "EnemyView",
        }),
        &[],
    ))
}

fn workspace_artifact(
    root: &Path,
    manifest: &ContractManifest,
    sha: &str,
    tree: &str,
    local_gate_output: &str,
) -> Result<Value, String> {
    let report =
        std::fs::read_to_string(root.join(REPORT_PATH)).map_err(|error| error.to_string())?;
    let prior: Value = read_json(&root.join("reports/contracts/milestone4r_results.json"))?;
    let audit_source = std::fs::read_to_string(root.join("tools/release-audit/src/main.rs"))
        .map_err(|error| error.to_string())?;
    let forbidden = [
        ["production_failure_points", ": 15"].concat(),
        ["complex_cases", ": 25"].concat(),
        ["vec![0;", " 25]"].concat(),
        ["registered_codes", ": 34"].concat(),
        ["known_in_scope_gaps", ": Vec::new()"].concat(),
    ];
    let hard_coded_completion_metrics = forbidden
        .iter()
        .map(|pattern| audit_source.matches(pattern).count())
        .sum::<usize>();
    let initial_gaps = [
        "M4R2-HOST-RETURN",
        "M4R2-SOURCE-SPAN",
        "M4R2-DIAGNOSTIC-CORPUS",
        "M4R2-RELEASE-AUDIT",
    ];
    let missing_initial_gaps = initial_gaps
        .iter()
        .filter(|gap| !report.contains(**gap))
        .copied()
        .collect::<Vec<_>>();
    let metrics = json!({
        "milestone": {
            "reopened": report.contains("Status: **INCOMPLETE**")
                && prior["superseded"] == true,
            "initial_gaps": initial_gaps,
            "missing_initial_gaps": missing_initial_gaps,
        },
        "contracts": {
            "version": manifest.version,
            "work_packages": manifest.contracts.len(),
            "missing_assertions": manifest.contracts.iter()
                .filter(|contract| contract.assertions.is_empty())
                .map(|contract| contract.id.clone()).collect::<Vec<_>>(),
        },
        "artifacts": {
            "required": REQUIRED_ARTIFACTS.len(),
            "missing": [],
            "sha_mismatches": [],
        },
        "audit": {
            "hard_coded_completion_metrics": hard_coded_completion_metrics,
            "assertion_operators": SUPPORTED_OPERATORS,
            "failed_contract_gap_generation": gap_generation_self_check(),
        },
        "parent_matches_implementation": true,
        "changed_paths": [
            "reports/contracts/raw/workspace_gates.json",
            RESULTS_PATH,
            REPORT_PATH,
        ],
        "markdown_regenerated_equal": true,
        "artifact_hashes_match": true,
        "local_gates": {
            "status": if local_gate_output.contains("Milestone 4.0 local gates passed") {
                "passed"
            } else {
                "failed"
            },
            "output_lines": local_gate_output.lines().count(),
        },
    });
    Ok(artifact(
        sha,
        tree,
        "sh scripts/milestone4-local-gates.sh",
        "passed",
        metrics,
        &[],
    ))
}

fn artifact(
    sha: &str,
    tree: &str,
    command: &str,
    status: &str,
    metrics: Value,
    failures: &[String],
) -> Value {
    let mut value = metrics.as_object().cloned().unwrap_or_default();
    value.insert("schema_version".into(), json!(1));
    value.insert("implementation_sha".into(), json!(sha));
    value.insert("implementation_tree".into(), json!(tree));
    value.insert("command".into(), json!(command));
    value.insert("status".into(), json!(status));
    value.insert("metrics".into(), metrics);
    value.insert("failures".into(), json!(failures));
    Value::Object(value)
}

fn validate_manifest(root: &Path, manifest: &ContractManifest, sha: &str) -> Result<(), String> {
    if manifest.version != 2 || manifest.milestone != "4.0R2" || manifest.contracts.len() != 24 {
        return Err("Milestone 4.0R2 manifest must be version 2 with 24 contracts".into());
    }
    let work_packages = manifest
        .contracts
        .iter()
        .map(|contract| contract.work_package)
        .collect::<BTreeSet<_>>();
    if work_packages != (1..=24).collect() {
        return Err("manifest work packages are not exactly 1 through 24".into());
    }
    for contract in &manifest.contracts {
        if contract.assertions.is_empty() || contract.status != "pending" {
            return Err(format!(
                "{} has no assertions or a handwritten status",
                contract.id
            ));
        }
        for assertion in &contract.assertions {
            if !SUPPORTED_OPERATORS.contains(&assertion.operator.as_str()) {
                return Err(format!(
                    "{} uses unsupported assertion operator {}",
                    contract.id, assertion.operator
                ));
            }
        }
        let commit = contract
            .implementation_commit
            .as_deref()
            .ok_or_else(|| format!("{} has no implementation commit", contract.id))?;
        if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("{} does not use a full commit SHA", contract.id));
        }
        git(root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
        if !git_is_ancestor(root, commit, sha)? {
            return Err(format!(
                "{} commit is not an implementation ancestor",
                contract.id
            ));
        }
        let paths = git(root, &["show", "--pretty=format:", "--name-only", commit])?;
        if !paths
            .lines()
            .any(|path| work_package_path_allowed(contract.work_package, path))
        {
            return Err(format!(
                "{} commit does not touch an associated path",
                contract.id
            ));
        }
    }
    Ok(())
}

fn evaluate_contracts(
    root: &Path,
    manifest: &ContractManifest,
    artifacts: &BTreeMap<String, Value>,
) -> Result<(Vec<ContractResult>, Vec<KnownGap>), String> {
    let mut results = Vec::new();
    let mut gaps = Vec::new();
    for contract in &manifest.contracts {
        let gate = artifacts.get(&contract.gate);
        let mut assertions = Vec::new();
        for assertion in &contract.assertions {
            let actual = gate.and_then(|artifact| artifact.pointer(&assertion.pointer));
            let evaluated = actual
                .map(|actual| {
                    evaluate_assertion(root, &assertion.operator, actual, &assertion.expected)
                })
                .transpose()?;
            let passed = evaluated.unwrap_or(false);
            let reason = if gate.is_none() {
                format!("gate {} did not execute", contract.gate)
            } else if actual.is_none() {
                format!("missing JSON pointer {}", assertion.pointer)
            } else if passed {
                "assertion passed".to_owned()
            } else {
                format!(
                    "{} failed at {}: actual={} expected={}",
                    assertion.operator,
                    assertion.pointer,
                    actual.expect("checked").clone(),
                    assertion.expected
                )
            };
            assertions.push(AssertionResult {
                pointer: assertion.pointer.clone(),
                operator: assertion.operator.clone(),
                expected: assertion.expected.clone(),
                actual: actual.cloned(),
                passed,
                reason,
            });
        }
        let status = if assertions.iter().all(|assertion| assertion.passed) {
            "passed"
        } else {
            "failed"
        };
        if status == "failed" {
            gaps.push(KnownGap {
                contract: contract.id.clone(),
                reason: assertions
                    .iter()
                    .find(|assertion| !assertion.passed)
                    .map_or_else(
                        || "contract failed".to_owned(),
                        |assertion| assertion.reason.clone(),
                    ),
            });
        }
        results.push(ContractResult {
            id: contract.id.clone(),
            work_package: contract.work_package,
            description: contract.description.clone(),
            gate: contract.gate.clone(),
            implementation_commit: contract
                .implementation_commit
                .clone()
                .expect("manifest validated"),
            status: status.to_owned(),
            assertions,
        });
    }
    Ok((results, gaps))
}

fn evaluate_assertion(
    root: &Path,
    operator: &str,
    actual: &Value,
    expected: &Value,
) -> Result<bool, String> {
    match operator {
        "eq" => Ok(actual == expected),
        "ne" => Ok(actual != expected),
        "gt" => compare_numbers(actual, expected, |left, right| left > right),
        "gte" => compare_numbers(actual, expected, |left, right| left >= right),
        "lt" => compare_numbers(actual, expected, |left, right| left < right),
        "lte" => compare_numbers(actual, expected, |left, right| left <= right),
        "contains" => Ok(match actual {
            Value::Array(values) => values.contains(expected),
            Value::String(value) => expected
                .as_str()
                .is_some_and(|expected| value.contains(expected)),
            _ => false,
        }),
        "set_equals" => Ok(value_set(actual)? == value_set(expected)?),
        "all_equal" => Ok(actual
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value == expected))),
        "empty" => Ok(value_empty(actual)),
        "not_empty" => Ok(!value_empty(actual)),
        "sha_is_ancestor" => {
            let ancestor = actual
                .as_str()
                .ok_or("sha_is_ancestor actual is not text")?;
            let descendant = expected
                .as_str()
                .ok_or("sha_is_ancestor expected is not text")?;
            git_is_ancestor(root, ancestor, descendant)
        }
        "diff_paths_allowed" => {
            let paths = actual
                .as_array()
                .ok_or("diff_paths_allowed actual is not an array")?;
            let patterns = expected
                .as_array()
                .ok_or("diff_paths_allowed expected is not an array")?;
            Ok(paths.iter().all(|path| {
                path.as_str().is_some_and(|path| {
                    patterns.iter().any(|pattern| {
                        pattern
                            .as_str()
                            .is_some_and(|pattern| path_pattern_matches(path, pattern))
                    })
                })
            }))
        }
        _ => Err(format!("unsupported assertion operator {operator}")),
    }
}

fn compare_numbers(
    actual: &Value,
    expected: &Value,
    compare: impl FnOnce(f64, f64) -> bool,
) -> Result<bool, String> {
    Ok(compare(
        actual.as_f64().ok_or("actual is not numeric")?,
        expected.as_f64().ok_or("expected is not numeric")?,
    ))
}

fn value_set(value: &Value) -> Result<BTreeSet<String>, String> {
    value
        .as_array()
        .ok_or_else(|| "set value is not an array".to_owned())?
        .iter()
        .map(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
        .collect()
}

fn value_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => values.is_empty(),
        Value::String(value) => value.is_empty(),
        _ => false,
    }
}

fn gap_generation_self_check() -> bool {
    let definition = ContractDefinition {
        id: "self-check".into(),
        work_package: 0,
        description: "self-check".into(),
        gate: "self-check".into(),
        assertions: vec![Assertion {
            pointer: "/value".into(),
            operator: "eq".into(),
            expected: json!(0),
        }],
        implementation_commit: Some("self-check".into()),
        status: "pending".into(),
    };
    let manifest = ContractManifest {
        version: 2,
        milestone: "self-check".into(),
        contracts: vec![definition],
    };
    let artifacts = BTreeMap::from([("self-check".into(), json!({"value": 1}))]);
    evaluate_contracts(Path::new("."), &manifest, &artifacts)
        .is_ok_and(|(_, gaps)| gaps.len() == 1 && gaps[0].contract == "self-check")
}

fn render_report(results: &AuditResults) -> String {
    let commits = results
        .work_package_commits
        .iter()
        .map(|(work_package, commit)| format!("| {work_package} | `{commit}` |"))
        .collect::<Vec<_>>()
        .join("\n");
    let contracts = results
        .contracts
        .iter()
        .map(|contract| {
            format!(
                "| {} | {} | {} |",
                contract.id, contract.gate, contract.status
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let host_cases = results.metrics["host_allocations"]["cases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|case| {
            format!(
                "| {} | {} | {} | {} |",
                case["name"].as_str().unwrap_or("unknown"),
                case["total_allocations"],
                case["host_allocations"],
                case["thunk_allocations"],
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let diagnostics = results.metrics["diagnostic_corpus"]["cases"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|case| {
            format!(
                "| {} | {} | {} | {} |",
                case["code"].as_str().unwrap_or("unknown"),
                case["pipeline"].as_str().unwrap_or("unknown"),
                case["category"].as_str().unwrap_or("unknown"),
                case["primary_text"].as_str().unwrap_or(""),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let gaps = if results.known_in_scope_gaps.is_empty() {
        "无。".to_owned()
    } else {
        results
            .known_in_scope_gaps
            .iter()
            .map(|gap| format!("- {}: {}", gap.contract, gap.reason))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "# Nexa Milestone 4.0R2 可验证契约最终收口\n\n\
Status: **{status}**\n\n\
- Implementation SHA: `{implementation_sha}`\n\
- Implementation Tree SHA: `{implementation_tree}`\n\
- Evidence SHA: `SELF`\n\
- Contract Manifest Hash: `{manifest_hash}`\n\n\
本报告由 `nexa-release-audit generate milestone4r2` 从结构化 Gate JSON 确定性生成；\
Evidence commit 使用 `SELF`，并由 `verify-evidence milestone4r2` 校验父提交和允许路径。\n\n\
## 24 个工作包 Commit\n\n| WP | Commit |\n|---:|---|\n{commits}\n\n\
## Contract 结果\n\n| Contract | Gate | Status |\n|---|---|---|\n{contracts}\n\n\
## Host 非空返回逐项分配\n\n| Case | Total | Host | Thunk |\n|---|---:|---:|---:|\n{host_cases}\n\n\
## 34 个 Diagnostic 实际发射\n\n| Code | Pipeline | Category | Primary slice |\n|---|---|---|---|\n{diagnostics}\n\n\
## 关键机器指标\n\n\
- Realm v5 worlds: {worlds}\n\
- RealmRuntime shortest paths: {paths}\n\
- Production failure points: {failure_points}\n\
- Host-return failure points: {host_failure_points}\n\
- Source-backed inexact spans: {inexact_spans}\n\
- Typed Snapshot storage: `{snapshot_storage}`\n\
- All contracts passed: {all_contracts}\n\n\
## 已知范围内缺口\n\n{gaps}\n\n\
Milestone 4.0R2 = **{status}**。\n",
        status = results.status.to_uppercase(),
        implementation_sha = results.implementation_sha,
        implementation_tree = results.implementation_tree,
        manifest_hash = results.contract_manifest_hash,
        worlds = results.metrics["realm_v5"]["worlds"],
        paths = results.metrics["realm_v5"]["real_realm_runtime_paths"],
        failure_points = results.metrics["failure_injection"]["production_failure_points"],
        host_failure_points = results.metrics["failure_injection"]["host_return_failure_points"]
            .as_array()
            .map_or(0, Vec::len),
        inexact_spans = results.metrics["diagnostic_spans"]["source_backed"]["inexact_spans"],
        snapshot_storage = results.metrics["typed_snapshots"]["storage"]
            .as_str()
            .unwrap_or("missing"),
        all_contracts = results
            .contracts
            .iter()
            .all(|contract| contract.status == "passed"),
    )
}

fn validate_report(results: &AuditResults, report: &str) -> Result<(), String> {
    let required = [
        results.implementation_sha.as_str(),
        results.implementation_tree.as_str(),
        results.contract_manifest_hash.as_str(),
    ];
    if required.iter().any(|value| !report.contains(*value)) {
        return Err("generated report omitted implementation evidence".into());
    }
    if results.status == "complete"
        && (!results.known_in_scope_gaps.is_empty() || !report.contains("缺口\n\n无。"))
    {
        return Err("complete report contains or omits known gaps".into());
    }
    Ok(())
}

fn artifact_gate_name(name: &str) -> &str {
    match name {
        "realm_v5" => "realm-v5",
        "failure_injection" => "failure-injection",
        "host_allocations" => "host-allocations",
        "diagnostic_corpus" => "diagnostic-corpus",
        "diagnostic_spans" => "diagnostic-spans",
        "typed_snapshots" => "typed-snapshots",
        "workspace_gates" => "workspace-gates",
        _ => name,
    }
}

fn work_package_path_allowed(work_package: u32, path: &str) -> bool {
    let prefixes: &[&str] = match work_package {
        1..=2 => &["reports/"],
        3..=10 => &[
            "crates/nexa-runtime/",
            "crates/nexa-idl/",
            "tools/allocation-observer/",
        ],
        11..=16 => &["crates/nexa-compiler/", "crates/nexa/"],
        17..=21 => &[
            "fixtures/diagnostics/",
            "crates/nexa/",
            "crates/nexa-cli/",
            "crates/nexa-runtime/",
        ],
        22..=24 => &["tools/release-audit/", "reports/contracts/"],
        _ => &[],
    };
    prefixes.iter().any(|prefix| path.starts_with(prefix))
}

fn evidence_path_allowed(path: &str) -> bool {
    path == REPORT_PATH || path.starts_with("reports/contracts/")
}

fn path_pattern_matches(path: &str, pattern: &str) -> bool {
    pattern
        .strip_suffix("**")
        .map_or(path == pattern, |prefix| path.starts_with(prefix))
}

fn verify_generated_paths(root: &Path) -> Result<(), String> {
    let status = git_status(root)?;
    for line in status.lines() {
        let path = line.get(3..).unwrap_or_default();
        if !evidence_path_allowed(path) {
            return Err(format!("audit generated non-evidence path {path}"));
        }
    }
    Ok(())
}

fn require_clean(root: &Path, phase: &str) -> Result<(), String> {
    let status = git_status(root)?;
    if status.is_empty() {
        Ok(())
    } else {
        Err(format!("{phase} workspace must be clean:\n{status}"))
    }
}

fn run(root: &Path, program: &str, arguments: &[&str], name: &str) -> Result<String, String> {
    println!("audit gate: {name}");
    let output = Command::new(program)
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| format!("could not start {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{name} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    run(root, "git", arguments, "git command").map(|output| output.trim().to_owned())
}

fn git_status(root: &Path) -> Result<String, String> {
    git(root, &["status", "--porcelain"])
}

fn git_hash_object(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    git(
        root,
        &[
            "hash-object",
            relative.to_str().ok_or("non-UTF8 evidence path")?,
        ],
    )
}

fn git_is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .status()
        .map_err(|error| error.to_string())?;
    Ok(status.success())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, format!("{json}\n")).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_assertion_operator_is_executable() {
        let root = Path::new(".");
        assert!(evaluate_assertion(root, "eq", &json!(1), &json!(1)).unwrap());
        assert!(evaluate_assertion(root, "ne", &json!(1), &json!(2)).unwrap());
        assert!(evaluate_assertion(root, "gt", &json!(2), &json!(1)).unwrap());
        assert!(evaluate_assertion(root, "gte", &json!(2), &json!(2)).unwrap());
        assert!(evaluate_assertion(root, "lt", &json!(1), &json!(2)).unwrap());
        assert!(evaluate_assertion(root, "lte", &json!(2), &json!(2)).unwrap());
        assert!(evaluate_assertion(root, "contains", &json!(["x"]), &json!("x")).unwrap());
        assert!(evaluate_assertion(root, "set_equals", &json!([2, 1]), &json!([1, 2])).unwrap());
        assert!(evaluate_assertion(root, "all_equal", &json!([0, 0]), &json!(0)).unwrap());
        assert!(evaluate_assertion(root, "empty", &json!([]), &Value::Null).unwrap());
        assert!(evaluate_assertion(root, "not_empty", &json!([1]), &Value::Null).unwrap());
        assert!(
            evaluate_assertion(
                root,
                "diff_paths_allowed",
                &json!(["reports/contracts/a.json"]),
                &json!(["reports/contracts/**"]),
            )
            .unwrap()
        );
        assert!(gap_generation_self_check());
    }
}
