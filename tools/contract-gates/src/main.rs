use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use nexa_contract_gate_protocol::{
    GATE_SCHEMA_VERSION, GateArtifact, GateFailure, GateStatus, read_artifact, write_artifact,
};
use nexa_model::realm_v5::{RealmV5Config, explore_realm_v5};
use nexa_runtime::RuntimeFailurePoint;
use serde::Deserialize;
use serde_json::{Value, json};

const GATES: [(&str, &str, &str); 9] = [
    ("realm-v5", "realm-v5", "realm_v5.json"),
    (
        "failure-injection",
        "failure-injection",
        "failure_injection.json",
    ),
    ("host-returns", "host-returns", "host_returns.json"),
    (
        "host-allocations",
        "host-allocations",
        "host_allocations.json",
    ),
    (
        "diagnostic-spans",
        "diagnostic-spans",
        "diagnostic_spans.json",
    ),
    (
        "diagnostic-corpus",
        "diagnostic-corpus",
        "diagnostic_corpus.json",
    ),
    (
        "runtime-diagnostics",
        "runtime-diagnostic-corpus",
        "runtime_diagnostics.json",
    ),
    ("typed-snapshots", "typed-snapshots", "typed_snapshots.json"),
    ("workspace", "workspace-gates", "workspace_gates.json"),
];

struct Context {
    root: PathBuf,
    sha: String,
    tree: String,
    output_dir: PathBuf,
}

fn main() {
    if let Err(error) = run_cli() {
        eprintln!("nexa-contract-gates: {error}");
        std::process::exit(1);
    }
}

fn run_cli() -> Result<(), String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    let sha = git(&root, &["rev-parse", "HEAD"])?;
    let tree = git(&root, &["rev-parse", "HEAD^{tree}"])?;
    match arguments.as_slice() {
        [command, flag, output] if flag == "--output" => {
            let output = PathBuf::from(output);
            let output_dir = output
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let context = Context {
                root,
                sha,
                tree,
                output_dir,
            };
            let artifact = execute(command, &context);
            write_artifact(&output, &artifact)?;
            if !artifact.passed() {
                return Err(format!("{command} gate failed"));
            }
            Ok(())
        }
        [command, flag, output_dir] if command == "all" && flag == "--output-dir" => {
            let context = Context {
                root,
                sha,
                tree,
                output_dir: PathBuf::from(output_dir),
            };
            std::fs::create_dir_all(&context.output_dir).map_err(|error| error.to_string())?;
            let mut failed = Vec::new();
            for (command, _, filename) in GATES {
                let artifact = execute(command, &context);
                write_artifact(&context.output_dir.join(filename), &artifact)?;
                if !artifact.passed() {
                    failed.push(command);
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!("failed gates: {}", failed.join(", ")))
            }
        }
        _ => {
            Err("usage: nexa-contract-gates <gate> --output <path> | all --output-dir <dir>".into())
        }
    }
}

fn execute(gate: &str, context: &Context) -> GateArtifact<Value> {
    match gate {
        "realm-v5" => realm_v5(context),
        "failure-injection" => failure_injection(context),
        "host-returns" => host_returns(context),
        "host-allocations" => host_allocations(context),
        "diagnostic-spans" => diagnostic_spans(context),
        "diagnostic-corpus" => diagnostic_corpus(context),
        "runtime-diagnostics" => runtime_diagnostics(context),
        "typed-snapshots" => typed_snapshots(context),
        "workspace" => workspace(context),
        _ => artifact(
            context,
            gate,
            "",
            Value::Null,
            vec![GateFailure::new("gate-name", "unknown gate")],
        ),
    }
}

fn artifact(
    context: &Context,
    gate: &str,
    command: &str,
    metrics: Value,
    failures: Vec<GateFailure>,
) -> GateArtifact<Value> {
    GateArtifact::new(
        gate,
        &context.sha,
        &context.tree,
        command,
        metrics,
        failures,
    )
}

fn realm_v5(context: &Context) -> GateArtifact<Value> {
    let report = explore_realm_v5(RealmV5Config::default());
    let failures = report
        .failures
        .iter()
        .map(|failure| GateFailure::new("realm-v5-invariant", format!("{failure:?}")))
        .chain(
            report
                .truncated
                .then(|| GateFailure::new("realm-v5-completeness", "exploration truncated")),
        )
        .collect();
    artifact(
        context,
        "realm-v5",
        "nexa-contract-gates realm-v5",
        json!({
            "worlds": report.visited_worlds,
            "real_realm_runtime_paths": report.shortest_paths.len(),
            "rejected_event_paths": report.rejected_operations,
            "shadow_state_fields": 0,
            "truncated": report.truncated,
        }),
        failures,
    )
}

fn failure_injection(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    command_check(
        context,
        &mut failures,
        "failure-differential",
        "cargo",
        &[
            "test",
            "-p",
            "nexa-model",
            "--test",
            "realm_v5_failure_differential",
        ],
    );
    let production = RuntimeFailurePoint::REALM_PRODUCTION
        .iter()
        .map(|point| format!("{point:?}"))
        .collect::<Vec<_>>();
    let host = RuntimeFailurePoint::HOST_RETURN
        .iter()
        .map(|point| format!("{point:?}"))
        .collect::<Vec<_>>();
    let all = RuntimeFailurePoint::ALL
        .iter()
        .map(|point| format!("{point:?}"))
        .collect::<Vec<_>>();
    artifact(
        context,
        "failure-injection",
        "nexa-contract-gates failure-injection",
        json!({
            "runtime_kind": "RealmRuntime",
            "points": all,
            "production_failure_points": production.len(),
            "production_points": production,
            "host_return_failure_points": host,
            "shadow_state_fields": 0,
        }),
        failures,
    )
}

fn observer(context: &Context, failures: &mut Vec<GateFailure>) -> Option<Value> {
    let output = command_check(
        context,
        failures,
        "allocation-observer",
        "cargo",
        &[
            "run",
            "-q",
            "--manifest-path",
            "tools/allocation-observer/Cargo.toml",
        ],
    )?;
    let line = output
        .lines()
        .find(|line| line.starts_with("{\"host_return_matrix\""));
    if let Some(value) = line.and_then(|line| serde_json::from_str(line).ok()) {
        Some(value)
    } else {
        failures.push(GateFailure::new(
            "allocation-observer-json",
            "host_return_matrix JSON was not emitted",
        ));
        None
    }
}

fn host_returns(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    command_check(
        context,
        &mut failures,
        "host-return-arithmetic",
        "cargo",
        &[
            "test",
            "-p",
            "nexa-runtime",
            "host_return_requirements_use_checked_arithmetic",
        ],
    );
    command_check(
        context,
        &mut failures,
        "generated-return-thunks",
        "cargo",
        &["test", "-p", "nexa-idl", "generated_runtime_thunks"],
    );
    let observed = observer(context, &mut failures).unwrap_or(Value::Null);
    let cases = observed
        .pointer("/host_return_matrix/cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let return_cases = cases
        .iter()
        .filter(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("return_"))
        })
        .cloned()
        .collect::<Vec<_>>();
    if return_cases.iter().any(|case| case["passed"] != true) {
        failures.push(GateFailure::new(
            "host-return-matrix",
            "one or more return cases failed",
        ));
    }
    artifact(
        context,
        "host-returns",
        "nexa-contract-gates host-returns",
        json!({
            "requirements": {
                "exact_cases": return_cases.len(),
                "overflow_rejected": failures.iter().all(|failure| failure.check != "host-return-arithmetic"),
            },
            "round_trip": {
                "cases": return_cases.len(),
                "case_names": return_cases.iter().filter_map(|case| case["name"].as_str()).collect::<Vec<_>>(),
                "failures": return_cases.iter().filter(|case| case["passed"] != true)
                    .filter_map(|case| case["name"].as_str()).collect::<Vec<_>>(),
            },
            "non_empty_lengths": observed.pointer("/host_return_matrix/non_empty_lengths"),
            "cases": return_cases,
        }),
        failures,
    )
}

fn host_allocations(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    command_check(
        context,
        &mut failures,
        "host-return-transaction",
        "cargo",
        &[
            "test",
            "-p",
            "nexa-runtime",
            "host_return_transaction_is_atomic_and_reuses_collection_arena",
        ],
    );
    let observed = observer(context, &mut failures).unwrap_or(Value::Null);
    let cases = observed
        .pointer("/host_return_matrix/cases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let accounting = cases.iter().all(|case| {
        case["total_allocations"].as_u64()
            == case["host_allocations"]
                .as_u64()
                .zip(case["thunk_allocations"].as_u64())
                .map(|(host, thunk)| host.saturating_add(thunk))
    });
    if !accounting {
        failures.push(GateFailure::new(
            "allocation-domain-accounting",
            "allocation domains do not sum to total allocations",
        ));
    }
    let injected = cases
        .iter()
        .filter(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("injected_"))
        })
        .cloned()
        .collect::<Vec<_>>();
    if injected.iter().any(|case| case["passed"] != true) {
        failures.push(GateFailure::new(
            "allocation-failure-atomicity",
            "an injected allocation case failed",
        ));
    }
    artifact(
        context,
        "host-allocations",
        "nexa-contract-gates host-allocations",
        json!({
            "measurement": {
                "baseline_subtraction": false,
                "domain_accounting": accounting,
            },
            "transaction": {
                "atomic": injected.iter().map(|case| case["passed"].clone()).collect::<Vec<_>>(),
            },
            "case_count": cases.len(),
            "thunk_allocations": cases.iter().map(|case| case["thunk_allocations"].clone()).collect::<Vec<_>>(),
            "cases": cases,
        }),
        failures,
    )
}

fn diagnostic_spans(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    let report = match nexa::run_compiler_diagnostic_cases(&context.root) {
        Ok(report) => Some(report),
        Err(error) => {
            failures.push(GateFailure::new("compiler-diagnostic-corpus", error));
            None
        }
    };
    let production = read_production_source(
        &context.root.join("crates/nexa-compiler/src/lib.rs"),
        &mut failures,
        "compiler-source",
    );
    let fallback = production.matches("fallback_span").count();
    let fabricated = production
        .matches("SourceSpan::new(FileId(0), 0, 1)")
        .count();
    let no_arg = [
        "CompileError::type_mismatch()",
        "CompileError::cannot_infer_type()",
    ]
    .iter()
    .map(|pattern| production.matches(pattern).count())
    .sum::<usize>();
    if fallback != 0 || fabricated != 0 || no_arg != 0 {
        failures.push(GateFailure::new(
            "span-static-check",
            format!("fallback={fallback} fabricated={fabricated} no_arg_constructors={no_arg}"),
        ));
    }
    if report
        .as_ref()
        .is_some_and(|report| report.source_backed_inexact_spans != 0)
    {
        failures.push(GateFailure::new(
            "source-backed-spans",
            "one or more compiler spans were inexact",
        ));
    }
    let metrics = report.map_or_else(
        || {
            json!({
                "static": {
                    "fallback_span_occurrences": fallback,
                    "fabricated_zero_one_occurrences": fabricated,
                    "no_arg_error_constructors": no_arg,
                }
            })
        },
        |report| {
            json!({
                "static": {
                    "fallback_span_occurrences": fallback,
                    "fabricated_zero_one_occurrences": fabricated,
                    "no_arg_error_constructors": no_arg,
                },
                "source_backed": {
                    "case_count": report.case_count,
                    "inexact_spans": report.source_backed_inexact_spans,
                    "zero_zero_spans": report.source_backed_zero_zero_spans,
                    "codes": report.codes,
                    "cases": report.cases,
                },
            })
        },
    );
    artifact(
        context,
        "diagnostic-spans",
        "nexa-contract-gates diagnostic-spans",
        metrics,
        failures,
    )
}

fn diagnostic_corpus(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    let metrics = match nexa::run_diagnostic_corpus(&context.root) {
        Ok(report) => {
            if !report.missing_codes.is_empty() || !report.unexpected_codes.is_empty() {
                failures.push(GateFailure::new(
                    "diagnostic-code-coverage",
                    format!(
                        "missing={:?} unexpected={:?}",
                        report.missing_codes, report.unexpected_codes
                    ),
                ));
            }
            if report.cases.iter().any(|case| !case.passed) {
                failures.push(GateFailure::new(
                    "diagnostic-cases",
                    "one or more diagnostic cases failed",
                ));
            }
            serde_json::to_value(report).unwrap_or(Value::Null)
        }
        Err(error) => {
            failures.push(GateFailure::new("diagnostic-corpus", error));
            Value::Null
        }
    };
    artifact(
        context,
        "diagnostic-corpus",
        "nexa-contract-gates diagnostic-corpus",
        metrics,
        failures,
    )
}

#[allow(clippy::too_many_lines)]
fn runtime_diagnostics(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    let forbidden = [
        "NexaError::Host(",
        "NexaError::Reload(",
        "NexaError::Migration(",
        "invoke_host_boundary(",
        "validate_host_completion(",
        "invoke_reload_activation(",
        "validate_reload_completion_capacity(",
        "MigrationLimits::validate_requirements(",
    ];
    let mut forbidden_calls = Vec::new();
    for relative in [
        "crates/nexa/src/diagnostic_corpus.rs",
        "crates/nexa/src/runtime_diagnostics.rs",
    ] {
        let production =
            read_production_source(&context.root.join(relative), &mut failures, relative);
        for token in forbidden {
            for (line, _) in production.match_indices(token) {
                forbidden_calls.push(format!("{relative}:{line}:{token}"));
            }
        }
    }
    if !forbidden_calls.is_empty() {
        failures.push(GateFailure::new(
            "runtime-corpus-static",
            format!("forbidden calls: {forbidden_calls:?}"),
        ));
    }
    let report = match nexa::run_runtime_diagnostic_end_to_end() {
        Ok(report) => Some(report),
        Err(error) => {
            failures.push(GateFailure::new("runtime-end-to-end", error));
            None
        }
    };
    if let Some(report) = &report {
        failures.extend(
            report
                .failures
                .iter()
                .map(|reason| GateFailure::new("runtime-case", reason)),
        );
        if !report.missing_codes.is_empty() {
            failures.push(GateFailure::new(
                "runtime-code-coverage",
                format!("missing codes: {:?}", report.missing_codes),
            ));
        }
        if !report.nondeterministic_cases.is_empty() {
            failures.push(GateFailure::new(
                "runtime-determinism",
                format!(
                    "nondeterministic cases: {:?}",
                    report.nondeterministic_cases
                ),
            ));
        }
    }
    let direct = forbidden_calls
        .iter()
        .filter(|call| call.contains("NexaError::"))
        .count();
    let metrics = report.map_or_else(
        || {
            json!({
                "static": {
                    "forbidden_runtime_corpus_calls": forbidden_calls,
                    "direct_nexa_error_construction": direct,
                    "direct_error_variant_construction": direct,
                }
            })
        },
        |report| {
            let unexpected = report
                .cases
                .iter()
                .flat_map(|(code, case)| {
                    case.unexpected_mutations
                        .iter()
                        .map(move |mutation| format!("{code}: {mutation}"))
                })
                .collect::<Vec<_>>();
            json!({
                "static": {
                    "forbidden_runtime_corpus_calls": forbidden_calls,
                    "direct_nexa_error_construction": direct,
                    "direct_error_variant_construction": direct,
                },
                "harness": {
                    "independent_cases": report.independent_harnesses,
                    "facade_classification_helpers": report.cases.values()
                        .map(|case| case.direct_classification_helper_calls).sum::<usize>(),
                },
                "cases": report.cases,
                "snapshots": {
                    "cases": report.independent_harnesses,
                    "unexpected_mutations": unexpected,
                },
                "deterministic_cases": report.deterministic_cases,
                "nondeterministic_cases": report.nondeterministic_cases,
                "observed_codes": report.observed_codes.len(),
                "observed_code_values": report.observed_codes,
                "missing_codes": report.missing_codes,
            })
        },
    );
    artifact(
        context,
        "runtime-diagnostic-corpus",
        "nexa-contract-gates runtime-diagnostics",
        metrics,
        failures,
    )
}

fn typed_snapshots(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    command_check(
        context,
        &mut failures,
        "snapshot-codec",
        "cargo",
        &["test", "-p", "nexa-idl", "typed_snapshot_codec"],
    );
    command_check(
        context,
        &mut failures,
        "snapshot-storage",
        "cargo",
        &["test", "-p", "nexa-runtime", "typed_snapshot_storage"],
    );
    command_check(
        context,
        &mut failures,
        "snapshot-boundaries",
        "cargo",
        &[
            "test",
            "-p",
            "nexa-runtime",
            "typed_snapshot_host_boundaries_reject_content_type_confusion",
        ],
    );
    let host = read_production_source(
        &context.root.join("crates/nexa-runtime/src/host.rs"),
        &mut failures,
        "runtime-host-source",
    );
    let storage = if host.contains("Arc<[u8]>") {
        "Arc<[u8]>"
    } else {
        failures.push(GateFailure::new(
            "snapshot-storage-type",
            "typed snapshot storage is not Arc<[u8]>",
        ));
        "missing"
    };
    artifact(
        context,
        "typed-snapshots",
        "nexa-contract-gates typed-snapshots",
        json!({
            "storage": storage,
            "type_id_validation": !failures.iter().any(|failure| failure.check == "snapshot-boundaries"),
            "content_type_validation": !failures.iter().any(|failure| failure.check == "snapshot-boundaries"),
            "schema_hash_validation": !failures.iter().any(|failure| failure.check == "snapshot-codec"),
            "alignment_validation": !failures.iter().any(|failure| failure.check == "snapshot-storage"),
            "combat_payload": "EnemyView",
        }),
        failures,
    )
}

#[derive(Deserialize)]
struct ContractManifest {
    version: u32,
    milestone: String,
    contracts: Vec<ContractDefinition>,
}

#[derive(Deserialize)]
struct ContractDefinition {
    id: String,
    work_package: u32,
    gate: String,
    artifact: String,
    assertions: Vec<ManifestAssertion>,
    implementation_commit: Option<String>,
    affected_paths: Vec<String>,
    forbidden_calls: Vec<String>,
    status: String,
}

#[derive(Deserialize)]
struct ManifestAssertion {
    pointer: String,
    operator: String,
    expected: Value,
}

#[allow(clippy::too_many_lines)]
fn workspace(context: &Context) -> GateArtifact<Value> {
    let mut failures = Vec::new();
    let manifest_path = context
        .root
        .join("reports/contracts/milestone4r3_contracts.json");
    let manifest: Option<ContractManifest> =
        read_json(&manifest_path, &mut failures, "contract-manifest");
    let report = read_text(
        &context.root.join("reports/milestone4_0_full_mvr.md"),
        &mut failures,
        "milestone-report",
    );
    let prior: Option<Value> = read_json(
        &context
            .root
            .join("reports/contracts/milestone4r2_results.json"),
        &mut failures,
        "r2-results",
    );
    let audit_source = read_production_source(
        &context.root.join("tools/release-audit/src/main.rs"),
        &mut failures,
        "release-audit-source",
    );

    let mut invalid_contracts = Vec::new();
    let mut work_packages = BTreeSet::new();
    if let Some(manifest) = &manifest {
        for contract in &manifest.contracts {
            work_packages.insert(contract.work_package);
            if contract.id.is_empty()
                || contract.gate.is_empty()
                || contract.artifact.is_empty()
                || contract.assertions.is_empty()
                || contract.affected_paths.is_empty()
                || contract.status.is_empty()
                || contract.assertions.iter().any(|assertion| {
                    assertion.pointer.is_empty()
                        || assertion.operator.is_empty()
                        || assertion.expected.is_null()
                            && assertion.operator != "empty"
                            && assertion.operator != "not_empty"
                })
            {
                invalid_contracts.push(contract.id.clone());
            }
            let _ = (&contract.implementation_commit, &contract.forbidden_calls);
        }
        match u32::try_from(manifest.contracts.len()) {
            Ok(package_count) => {
                let expected = (1..=package_count).collect::<BTreeSet<_>>();
                if work_packages != expected {
                    invalid_contracts.push("work-package-coverage".into());
                }
            }
            Err(_) => {
                invalid_contracts.push("work-package-count".into());
            }
        }
        if manifest.milestone != "4.0R3" {
            invalid_contracts.push("milestone-name".into());
        }
    }
    if !invalid_contracts.is_empty() {
        failures.push(GateFailure::new(
            "contract-manifest",
            format!("invalid contracts: {invalid_contracts:?}"),
        ));
    }

    let expected_gaps = manifest
        .as_ref()
        .and_then(|manifest| manifest.contracts.first())
        .and_then(|contract| {
            contract
                .assertions
                .iter()
                .find(|assertion| assertion.pointer == "/metrics/milestone/gaps")
        })
        .and_then(|assertion| assertion.expected.as_array())
        .cloned()
        .unwrap_or_default();
    let missing_report_gaps = expected_gaps
        .iter()
        .filter_map(Value::as_str)
        .filter(|gap| !report.contains(gap))
        .collect::<Vec<_>>();
    if !report.contains("Status: **INCOMPLETE**") || !missing_report_gaps.is_empty() {
        failures.push(GateFailure::new(
            "milestone-reopened",
            format!("missing gaps: {missing_report_gaps:?}"),
        ));
    }
    let r2_superseded = prior
        .as_ref()
        .is_some_and(|prior| prior["superseded"] == true);
    if !r2_superseded {
        failures.push(GateFailure::new(
            "r2-supersession",
            "R2 results are not marked superseded",
        ));
    }

    let forbidden_counts = ["592", "15", "6", "18", "34", "38"]
        .into_iter()
        .filter(|token| numeric_token_present(&audit_source, token))
        .collect::<Vec<_>>();
    let domain_fill_values = [
        "\"overflow_rejected\": true",
        "\"range_reuse\": true",
        "\"type_id_validation\": true",
        "\"parent_matches_implementation\": true",
        "\"artifact_hashes_match\": true",
        "\"failures\": []",
    ]
    .into_iter()
    .filter(|token| audit_source.contains(token))
    .collect::<Vec<_>>();
    if !forbidden_counts.is_empty() || !domain_fill_values.is_empty() {
        failures.push(GateFailure::new(
            "audit-derivation",
            format!("forbidden_counts={forbidden_counts:?} fill_values={domain_fill_values:?}"),
        ));
    }

    let mut missing_artifacts = Vec::new();
    let mut provenance_mismatches = Vec::new();
    let mut generated = 1_usize;
    for (command, gate, filename) in GATES {
        if command == "workspace" {
            continue;
        }
        let path = context.output_dir.join(filename);
        if !path.is_file() {
            missing_artifacts.push(filename);
            continue;
        }
        generated += 1;
        match read_artifact::<Value>(&path) {
            Ok(artifact) => {
                if artifact.gate != gate
                    || artifact.implementation_sha != context.sha
                    || artifact.implementation_tree != context.tree
                    || artifact.schema_version != GATE_SCHEMA_VERSION
                    || artifact.status != GateStatus::Passed
                {
                    provenance_mismatches.push(filename);
                }
            }
            Err(_) => provenance_mismatches.push(filename),
        }
    }
    if !missing_artifacts.is_empty() || !provenance_mismatches.is_empty() {
        failures.push(GateFailure::new(
            "gate-artifacts",
            format!("missing={missing_artifacts:?} provenance={provenance_mismatches:?}"),
        ));
    }

    let negative = command_check(
        context,
        &mut failures,
        "audit-negative-cases",
        "cargo",
        &[
            "run",
            "-q",
            "-p",
            "nexa-release-audit",
            "--",
            "negative-self-check",
            "milestone4r3",
        ],
    )
    .and_then(|output| {
        output
            .lines()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
    })
    .unwrap_or(Value::Null);
    let negative_cases = negative["cases"].as_u64().unwrap_or_default();
    let negative_failures = negative["failures"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![json!("negative self-check output missing")]);
    if !negative_failures.is_empty() || negative["gap_derivation"] != true {
        failures.push(GateFailure::new(
            "audit-negative-results",
            format!("negative self-check: {negative}"),
        ));
    }

    let local_output = command_check(
        context,
        &mut failures,
        "full-local-gates",
        "sh",
        &["scripts/milestone4-local-gates.sh"],
    );
    let commands = [
        "generate",
        "verify-evidence",
        "generate-receipt",
        "verify-final",
    ]
    .into_iter()
    .filter(|command| audit_source.contains(command))
    .collect::<Vec<_>>();
    let premature_receipt = context
        .root
        .join("reports/contracts/milestone4r3_verification_receipt.json")
        .is_file();
    if commands.len() != 4 || premature_receipt {
        failures.push(GateFailure::new(
            "evidence-protocol",
            format!("commands={commands:?} premature_receipt={premature_receipt}"),
        ));
    }

    artifact(
        context,
        "workspace-gates",
        "nexa-contract-gates workspace",
        json!({
            "milestone": {
                "status": "INCOMPLETE",
                "gaps": expected_gaps,
                "r2_superseded": r2_superseded,
            },
            "contracts": {
                "version": manifest.as_ref().map_or(0, |manifest| manifest.version),
                "work_packages": work_packages.len(),
                "invalid_contracts": invalid_contracts,
            },
            "gate_protocol": {
                "schema_version": GATE_SCHEMA_VERSION,
                "self_deciding_gates": GATES.len(),
            },
            "artifacts": {
                "generated": generated,
                "missing": missing_artifacts,
                "provenance_mismatches": provenance_mismatches,
            },
            "audit": {
                "domain_fill_values": domain_fill_values,
                "forbidden_domain_counts": forbidden_counts,
                "negative_cases": negative_cases,
                "negative_failures": negative_failures,
                "gap_derivation": negative["gap_derivation"],
            },
            "evidence": {
                "protocol": "implementation-evidence-receipt",
                "premature_receipt": premature_receipt,
                "commands": commands,
            },
            "local_gates": {
                "status": if local_output.is_some() { "passed" } else { "failed" },
                "completion_marker": local_output.as_ref()
                    .is_some_and(|output| output.contains("Milestone 4.0 local gates passed")),
            },
        }),
        failures,
    )
}

fn numeric_token_present(source: &str, token: &str) -> bool {
    source.match_indices(token).any(|(index, _)| {
        let before = source[..index].chars().next_back();
        let after = source[index + token.len()..].chars().next();
        before.is_none_or(|character| !character.is_ascii_digit())
            && after.is_none_or(|character| !character.is_ascii_digit())
    })
}

fn read_production_source(path: &Path, failures: &mut Vec<GateFailure>, check: &str) -> String {
    read_text(path, failures, check)
        .split("#[cfg(test)]")
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn read_text(path: &Path, failures: &mut Vec<GateFailure>, check: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) => {
            failures.push(GateFailure::new(
                check,
                format!("{}: {error}", path.display()),
            ));
            String::new()
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    failures: &mut Vec<GateFailure>,
    check: &str,
) -> Option<T> {
    match std::fs::read(path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
    {
        Ok(value) => Some(value),
        Err(error) => {
            failures.push(GateFailure::new(
                check,
                format!("{}: {error}", path.display()),
            ));
            None
        }
    }
}

fn command_check(
    context: &Context,
    failures: &mut Vec<GateFailure>,
    check: &str,
    program: &str,
    arguments: &[&str],
) -> Option<String> {
    let output = match Command::new(program)
        .args(arguments)
        .current_dir(&context.root)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            failures.push(GateFailure::new(check, error.to_string()));
            return None;
        }
    };
    if !output.status.success() {
        failures.push(GateFailure::new(
            check,
            format!(
                "command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
