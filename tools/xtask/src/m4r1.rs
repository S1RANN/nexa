use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DynError, workspace_root};

const GATE_DIRECTORY: &str = "target/nexa-artifacts/m4r1-gates";
const FINAL_REPORT_PATH: &str = "target/nexa-artifacts/m4r1-finalize/final-report.json";
const LEGACY_AUDIT_PATH: &str = "target/nexa-artifacts/m4r1-finalize/legacy-audit.json";
const REGRESSION_RECEIPT_PATH: &str = "target/nexa-artifacts/m4r1-gates/m1-m4-regression.json";
const NIDL_STRESS_REPORT_PATH: &str =
    "target/nexa-artifacts/m4r1-scale-stress/nidl-mutation-report.json";
const NIDL_RELOAD_STRESS_REPORT_PATH: &str =
    "target/nexa-artifacts/m4r1-scale-stress/nidl-reload-stress-report.json";
const EDITOR_REPORT_PATH: &str = "target/nexa-editor-support/editor-package-report.json";
const ANALYSIS_SCALE_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/analysis-scale-report.json";
const FACADE_SCALE_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/facade-scale-report.json";
const RELOAD_STRESS_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/reload-stress-report.json";

const COMPLETION_BRANCH: &str = "codex/language-scale-m4-r1";
const COMPLETION_TAG: &str = "language-scale-m4-complete-r1";
const CANONICAL_ORIGIN_URLS: [&str; 3] = [
    "https://github.com/S1RANN/nexa",
    "ssh://git@github.com/S1RANN/nexa",
    "git@github.com:S1RANN/nexa",
];

const PREVIOUS_MILESTONE_TAGS: [(&str, &str, &str); 9] = [
    (
        "gate1-v2.9-stop",
        "1217d4dfdb323a6442717b18385dfcb6fb74d499",
        "8552064ec01b3191467633717de7b77c97cb24f1",
    ),
    (
        "internal-pivot-m1-complete",
        "772fe1a6faa66b9377e4339fd6f3f02452671fe5",
        "a44ec778f2733e1e1cc9e122823190ff131c9c70",
    ),
    (
        "internal-pivot-m1-complete-r1",
        "ff67117903df6996a1a94f19fed61fde57b17386",
        "049b7b52891d4731af1793ab0a755f79130a03dd",
    ),
    (
        "embed-snake-m2-complete",
        "d3ae62fab3c3d40a5741853c8a7332ecffcfa676",
        "aef12a0f92a1efe8c0f0497c3cb6147cb86f0c7e",
    ),
    (
        "developer-loop-m3-complete",
        "058e3c5638997735cc1faa55f6d9cf8496141dea",
        "621612f49c4180989711df3ca80021fd21ad9277",
    ),
    (
        "developer-loop-m3-complete-r1",
        "a7e809ad42036d9286cf8d78f7ba126e3964e05c",
        "b53ce21f98db7387b37cca0572fbbf920ab53d61",
    ),
    (
        "developer-loop-m3-complete-r2",
        "8077ff2b5c99f2c3a145d95d04d42b0b81a76e8a",
        "71c3a3ead70533f013928b6d1c434e1870f49b24",
    ),
    (
        "developer-loop-m3-complete-r3",
        "cc05c461c0db2b7ba2310536071d136807563cfd",
        "9d31064536b5c201ffdb064fb6af8837e87edbb5",
    ),
    (
        "language-scale-m4-complete",
        "ccb2284ad4a949d0d32978c4258599263dfd74b9",
        "dffdede878e88845e21d6f1733f75d57839e81da",
    ),
];

const EMBEDDED_NEGATIVE_TESTS: [(&str, &str, usize); 11] = [
    (
        "crates/nexa-syntax/tests/language_v2.rs",
        "legacy_surface_forms_are_rejected_at_the_removed_word",
        11,
    ),
    (
        "crates/nexa-syntax/tests/language_v2.rs",
        "prefix_await_is_rejected_but_full_postfix_chain_is_preserved",
        2,
    ),
    (
        "crates/nexa-contract/tests/nidl_v2.rs",
        "rejects_every_removed_nidl_spelling",
        15,
    ),
    (
        "crates/nexa-compiler/tests/async_v2.rs",
        "prefix_await_is_not_part_of_language_v2",
        1,
    ),
    (
        "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
        "virtual_snippet_rejects_removed_module_syntax_without_losing_origin",
        1,
    ),
    (
        "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
        "removed_nexa_v1_surface_forms_are_rejected",
        11,
    ),
    (
        "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
        "nidl_v2_accepts_contract_blocks_and_rejects_removed_surface_forms",
        7,
    ),
    (
        "crates/nexa-cli/tests/m4_tooling.rs",
        "single_file_rejects_legacy_module_headers",
        1,
    ),
    (
        "crates/nexa-cli/src/lsp.rs",
        "lsp_v2_rejects_legacy_surface_at_the_legacy_token",
        1,
    ),
    (
        "crates/nexa-cli/src/project.rs",
        "project_config_rejects_the_removed_required_exports_key",
        1,
    ),
    (
        "crates/nexa-analysis/tests/language_v2.rs",
        "legacy_surface_matrix_is_rejected",
        11,
    ),
];

#[derive(Clone, Copy)]
struct TestGateSpec {
    name: &'static str,
    package: &'static str,
    target: &'static str,
    source: &'static str,
    editor: bool,
}

const TEST_GATES: [TestGateSpec; 8] = [
    TestGateSpec {
        name: "test-language-v2",
        package: "nexa-syntax",
        target: "language_v2",
        source: "crates/nexa-syntax/tests/language_v2.rs",
        editor: true,
    },
    TestGateSpec {
        name: "test-object-model-v2",
        package: "nexa-runtime",
        target: "object_model_v2",
        source: "crates/nexa-runtime/tests/object_model_v2.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-async-v2",
        package: "nexa-compiler",
        target: "async_v2",
        source: "crates/nexa-compiler/tests/async_v2.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-nidl-v2",
        package: "nexa-contract",
        target: "nidl_v2",
        source: "crates/nexa-contract/tests/nidl_v2.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-structured-codegen",
        package: "nexa-contract",
        target: "structured_codegen",
        source: "crates/nexa-contract/tests/structured_codegen.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-standalone",
        package: "nexa-cli",
        target: "standalone",
        source: "crates/nexa-cli/tests/standalone.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-repl",
        package: "nexa-cli",
        target: "repl",
        source: "crates/nexa-cli/tests/repl.rs",
        editor: false,
    },
    TestGateSpec {
        name: "test-entrypoints",
        package: "nexa-embed",
        target: "entrypoints",
        source: "crates/nexa-embed/tests/entrypoints.rs",
        editor: false,
    },
];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Status {
    Pass,
    Fail,
}

impl Status {
    const fn from_passed(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Fail }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CommandEvidence {
    program: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    exit_code: Option<i32>,
    duration_ms: u64,
    success: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TestTargetEvidence {
    package: String,
    target: String,
    source: String,
    registered_tests: usize,
    registered_test_names: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GateReceipt {
    schema: u32,
    gate: String,
    head: String,
    branch: String,
    started_clean: bool,
    completed_head: String,
    completed_branch: String,
    completed_clean: bool,
    test_target: Option<TestTargetEvidence>,
    commands: Vec<CommandEvidence>,
    machine_reports: Vec<String>,
    failures: Vec<String>,
    status: Status,
}

impl GateReceipt {
    fn reusable_for(&self, gate: &str, head: &str) -> bool {
        self.schema == 1
            && self.gate == gate
            && self.head == head
            && self.started_clean
            && self.completed_head == self.head
            && self.completed_branch == self.branch
            && self.completed_clean
            && self.status == Status::Pass
            && self.failures.is_empty()
            && !self.commands.is_empty()
            && self
                .commands
                .iter()
                .all(|command| command.success && command.exit_code == Some(0))
            && gate_command_matrix_matches(self)
            && machine_report_matrix_matches(self)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegressionReceipt {
    schema: u32,
    head: String,
    branch: String,
    worktree_clean: bool,
    status: Status,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    pattern: String,
    path: String,
    line: usize,
    excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyAudit {
    schema: u32,
    scanned_roots: Vec<String>,
    scanned_files: usize,
    pattern_counts: BTreeMap<String, usize>,
    evidence: Vec<Finding>,
    allowlisted_negative_corpus: Vec<Finding>,
    allowlist_validation: BTreeMap<String, bool>,
    failures: Vec<String>,
    status: Status,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutEvidence {
    branch: String,
    head: String,
    main_head: String,
    milestone_branch_head: String,
    worktree_clean: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TagEvidence {
    object_type: String,
    object: String,
    target: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemotePublication {
    remote: String,
    remote_url: String,
    canonical_remote: bool,
    main_head: String,
    milestone_branch_head: String,
    completion_tag_object: String,
    completion_tag_target: String,
    previous_tags: BTreeMap<String, TagEvidence>,
    query_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FinalReport {
    schema: u32,
    milestone: &'static str,
    tested_checkout: CheckoutEvidence,
    completed_checkout: CheckoutEvidence,
    regression: RegressionReceipt,
    gates: Vec<GateReceipt>,
    legacy_audit: LegacyAudit,
    previous_tags: BTreeMap<String, TagEvidence>,
    completion_tag: TagEvidence,
    remote_publication: RemotePublication,
    failures: Vec<String>,
    status: Status,
}

#[derive(Default)]
struct CommandRunner {
    evidence: Vec<CommandEvidence>,
}

impl CommandRunner {
    fn run(
        &mut self,
        program: &Path,
        args: &[&str],
        environment: &[(&str, &Path)],
    ) -> Result<(), DynError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(workspace_root())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let recorded_environment = apply_environment(&mut command, environment)?;
        let started = Instant::now();
        let status = command.status();
        let duration_ms = elapsed_millis(started);
        match status {
            Ok(status) => {
                self.evidence.push(CommandEvidence {
                    program: program.display().to_string(),
                    args: owned_args(args),
                    environment: recorded_environment,
                    exit_code: status.code(),
                    duration_ms,
                    success: status.success(),
                });
                if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "`{} {}` failed with {}",
                        program.display(),
                        args.join(" "),
                        status
                            .code()
                            .map_or_else(|| "signal".to_owned(), |code| code.to_string())
                    )
                    .into())
                }
            }
            Err(error) => {
                self.evidence.push(CommandEvidence {
                    program: program.display().to_string(),
                    args: owned_args(args),
                    environment: recorded_environment,
                    exit_code: None,
                    duration_ms,
                    success: false,
                });
                Err(format!(
                    "could not start `{} {}`: {error}",
                    program.display(),
                    args.join(" ")
                )
                .into())
            }
        }
    }

    fn run_captured(
        &mut self,
        program: &Path,
        args: &[&str],
        environment: &[(&str, &Path)],
    ) -> Result<Vec<u8>, DynError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(workspace_root())
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let recorded_environment = apply_environment(&mut command, environment)?;
        let started = Instant::now();
        let child = command.spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) => {
                self.evidence.push(CommandEvidence {
                    program: program.display().to_string(),
                    args: owned_args(args),
                    environment: recorded_environment,
                    exit_code: None,
                    duration_ms: elapsed_millis(started),
                    success: false,
                });
                return Err(format!(
                    "could not start `{} {}`: {error}",
                    program.display(),
                    args.join(" ")
                )
                .into());
            }
        };
        let stdout = child.stdout.take().ok_or("child stdout was not piped")?;
        let stderr = child.stderr.take().ok_or("child stderr was not piped")?;
        let stdout_thread = thread::spawn(move || copy_and_capture(stdout, true));
        let stderr_thread = thread::spawn(move || copy_and_capture(stderr, false));
        let status = child.wait()?;
        let stdout = stdout_thread
            .join()
            .map_err(|_| "stdout forwarding thread panicked")??;
        let stderr = stderr_thread
            .join()
            .map_err(|_| "stderr forwarding thread panicked")??;
        let duration_ms = elapsed_millis(started);
        self.evidence.push(CommandEvidence {
            program: program.display().to_string(),
            args: owned_args(args),
            environment: recorded_environment,
            exit_code: status.code(),
            duration_ms,
            success: status.success(),
        });
        if !status.success() {
            return Err(format!(
                "`{} {}` failed with {}\nstdout:\n{}\nstderr:\n{}",
                program.display(),
                args.join(" "),
                status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            )
            .into());
        }
        let mut combined = stdout;
        combined.extend_from_slice(&stderr);
        Ok(combined)
    }
}

pub(super) fn test_language_v2() -> Result<(), DynError> {
    run_test_gate_command("test-language-v2")
}

pub(super) fn test_object_model_v2() -> Result<(), DynError> {
    run_test_gate_command("test-object-model-v2")
}

pub(super) fn test_async_v2() -> Result<(), DynError> {
    run_test_gate_command("test-async-v2")
}

pub(super) fn test_nidl_v2() -> Result<(), DynError> {
    run_test_gate_command("test-nidl-v2")
}

pub(super) fn test_structured_codegen() -> Result<(), DynError> {
    run_test_gate_command("test-structured-codegen")
}

pub(super) fn test_standalone() -> Result<(), DynError> {
    run_test_gate_command("test-standalone")
}

pub(super) fn test_repl() -> Result<(), DynError> {
    run_test_gate_command("test-repl")
}

pub(super) fn test_entrypoints() -> Result<(), DynError> {
    run_test_gate_command("test-entrypoints")
}

pub(super) fn m4r1_scale_stress() -> Result<(), DynError> {
    let receipt = execute_scale_stress_gate();
    write_gate_receipt(&receipt)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    receipt_result(&receipt)
}

pub(super) fn record_regression_pass() -> Result<(), DynError> {
    let head = git_optional(&["rev-parse", "HEAD"]);
    let worktree_clean = worktree_clean();
    let receipt = RegressionReceipt {
        schema: 1,
        head: head.clone(),
        branch: current_branch(),
        worktree_clean,
        status: Status::from_passed(head != "missing" && worktree_clean),
    };
    write_json_atomic(
        &workspace_root().join(REGRESSION_RECEIPT_PATH),
        &receipt,
        "M4R1 regression receipt",
    )?;
    if receipt.status == Status::Pass {
        Ok(())
    } else {
        Err("M1-M4 regression completed outside a clean, attached checkout".into())
    }
}

pub(super) fn finalize_after_workspace() -> Result<(), DynError> {
    let head = git_optional(&["rev-parse", "HEAD"]);
    let branch = current_branch();
    let started_clean = worktree_clean();
    if head == "missing" || branch == "missing" || !started_clean {
        return Err("M4R1 post-workspace evidence requires a clean attached checkout".into());
    }
    for spec in TEST_GATES {
        require_test_target(spec.source)?;
    }

    let started = Instant::now();
    let mut runner = CommandRunner::default();
    run_editor_gate(&mut runner)?;
    let nidl_report = run_nidl_mutation_gate(&mut runner)?;
    run_structured_codegen_cargo_check(&mut runner)?;
    let scale = execute_scale_stress_gate();
    write_gate_receipt(&scale)?;
    receipt_result(&scale)?;

    let completed_head = git_optional(&["rev-parse", "HEAD"]);
    let completed_branch = current_branch();
    let completed_clean = worktree_clean();
    if completed_head != head || completed_branch != branch || !completed_clean {
        return Err("M4R1 post-workspace evidence changed or dirtied the checkout".into());
    }
    let receipt = serde_json::json!({
        "schema": 1,
        "gate": "finalize-m4r1-post-workspace",
        "head": head,
        "branch": branch,
        "durationMs": elapsed_millis(started),
        "workspaceCoverage": TEST_GATES.map(|spec| spec.name),
        "commands": runner.evidence,
        "machineReports": [
            workspace_root().join(EDITOR_REPORT_PATH).display().to_string(),
            nidl_report.display().to_string(),
            workspace_root().join(ANALYSIS_SCALE_REPORT_PATH).display().to_string(),
            workspace_root().join(FACADE_SCALE_REPORT_PATH).display().to_string(),
            workspace_root().join(RELOAD_STRESS_REPORT_PATH).display().to_string(),
        ],
        "scaleReceipt": scale,
        "status": "PASS",
    });
    let output =
        workspace_root().join("target/nexa-artifacts/m4r1-finalize/post-workspace-receipt.json");
    write_json_atomic(&output, &receipt, "M4R1 post-workspace receipt")?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub(super) fn finalize_m4r1() -> Result<(), DynError> {
    let root = workspace_root();
    remove_stale_file(&root.join(FINAL_REPORT_PATH))?;
    let tested_checkout = checkout_evidence();
    let head = tested_checkout.head.clone();
    let preflight_tag = load_tag(COMPLETION_TAG);
    let preflight_remote = load_remote_publication();
    let previous_tags = PREVIOUS_MILESTONE_TAGS
        .into_iter()
        .map(|(name, _, _)| (name.to_owned(), load_tag(name)))
        .collect::<BTreeMap<_, _>>();
    let mut historical_tag_failures = Vec::new();
    validate_previous_tags(
        &previous_tags,
        &preflight_remote,
        &mut historical_tag_failures,
    );
    if tested_checkout.branch != "main"
        || tested_checkout.head == "missing"
        || tested_checkout.head != tested_checkout.main_head
        || tested_checkout.head != tested_checkout.milestone_branch_head
        || !tested_checkout.worktree_clean
        || preflight_tag.object_type != "tag"
        || preflight_tag.target != tested_checkout.head
        || preflight_remote.query_error.is_some()
        || !preflight_remote.canonical_remote
        || preflight_remote.main_head != tested_checkout.head
        || preflight_remote.milestone_branch_head != tested_checkout.head
        || preflight_remote.completion_tag_object != preflight_tag.object
        || preflight_remote.completion_tag_target != tested_checkout.head
        || !historical_tag_failures.is_empty()
    {
        return Err(format!(
            "finalize-m4-r1 preflight requires clean local and canonical-origin \
             main=milestone=annotated-tag HEAD; checkout={tested_checkout:?}, \
             tag={preflight_tag:?}, remote={preflight_remote:?}, \
             historical_tag_failures={historical_tag_failures:?}"
        )
        .into());
    }

    let regression = load_reusable_regression(&head).unwrap_or_else(|| {
        let result = super::check();
        if let Err(error) = &result {
            eprintln!("M1-M4 regression failed while finalizing M4R1: {error}");
        }
        load_regression_receipt().unwrap_or(RegressionReceipt {
            schema: 1,
            head: head.clone(),
            branch: current_branch(),
            worktree_clean: worktree_clean(),
            status: Status::Fail,
        })
    });

    let mut gates = Vec::with_capacity(TEST_GATES.len() + 1);
    for spec in TEST_GATES {
        gates.push(load_reusable_gate(spec.name, &head).unwrap_or_else(|| {
            let receipt = execute_test_gate(spec);
            if let Err(error) = write_gate_receipt(&receipt) {
                eprintln!("could not write {} receipt: {error}", spec.name);
            }
            receipt
        }));
    }
    gates.push(
        load_reusable_gate("m4r1-scale-stress", &head).unwrap_or_else(|| {
            let receipt = execute_scale_stress_gate();
            if let Err(error) = write_gate_receipt(&receipt) {
                eprintln!("could not write m4r1-scale-stress receipt: {error}");
            }
            receipt
        }),
    );

    let legacy_audit = run_legacy_audit();
    write_json_atomic(
        &root.join(LEGACY_AUDIT_PATH),
        &legacy_audit,
        "M4R1 legacy audit",
    )?;

    let previous_tags = PREVIOUS_MILESTONE_TAGS
        .into_iter()
        .map(|(name, _, _)| (name.to_owned(), load_tag(name)))
        .collect::<BTreeMap<_, _>>();
    let completion_tag = load_tag(COMPLETION_TAG);
    let remote_publication = load_remote_publication();
    let completed_checkout = checkout_evidence();

    let mut failures = Vec::new();
    validate_tested_checkout(&tested_checkout, &mut failures);
    validate_regression(&regression, &head, &mut failures);
    validate_gate_set(&gates, &head, &mut failures);
    if legacy_audit.status != Status::Pass {
        failures.extend(
            legacy_audit
                .failures
                .iter()
                .map(|failure| format!("legacy audit: {failure}")),
        );
    }
    validate_previous_tags(&previous_tags, &remote_publication, &mut failures);
    validate_completion(
        &completed_checkout,
        &completion_tag,
        &remote_publication,
        &mut failures,
    );
    if tested_checkout.head != completed_checkout.head {
        failures.push(format!(
            "checkout changed during finalization: {} -> {}",
            tested_checkout.head, completed_checkout.head
        ));
    }

    let status = Status::from_passed(failures.is_empty());
    let report = FinalReport {
        schema: 1,
        milestone: "Nexa M4R1 Language Surface & Toolchain Reset",
        tested_checkout,
        completed_checkout,
        regression,
        gates,
        legacy_audit,
        previous_tags,
        completion_tag,
        remote_publication,
        failures,
        status,
    };
    let output = root.join(FINAL_REPORT_PATH);
    write_json_atomic(&output, &report, "M4R1 final report")?;
    let decoded: Value = serde_json::from_slice(&fs::read(&output)?)?;
    println!("{}", serde_json::to_string_pretty(&decoded)?);
    if report.status == Status::Pass {
        Ok(())
    } else {
        Err(format!("M4R1 finalization failed; inspect {}", output.display()).into())
    }
}

fn run_test_gate_command(name: &str) -> Result<(), DynError> {
    let spec = TEST_GATES
        .iter()
        .copied()
        .find(|spec| spec.name == name)
        .ok_or_else(|| format!("unknown M4R1 test gate `{name}`"))?;
    let receipt = execute_test_gate(spec);
    write_gate_receipt(&receipt)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    receipt_result(&receipt)
}

fn execute_test_gate(spec: TestGateSpec) -> GateReceipt {
    let head = git_optional(&["rev-parse", "HEAD"]);
    let branch = current_branch();
    let started_clean = worktree_clean();
    if !started_clean || head == "missing" || branch == "missing" {
        return gate_precondition_failure(spec.name, head, branch, started_clean);
    }
    let mut runner = CommandRunner::default();
    let mut machine_reports = Vec::new();
    let result: Result<TestTargetEvidence, DynError> = (|| {
        require_test_target(spec.source)?;
        let registered_test_names = list_test_target(&mut runner, spec.package, spec.target)?;
        if registered_test_names.is_empty() {
            return Err(format!(
                "{} registered zero tests; marker-only targets do not satisfy M4R1",
                spec.source
            )
            .into());
        }
        require_registered_tests(spec.name, spec.source, &registered_test_names)?;
        let output = runner.run_captured(
            Path::new("cargo"),
            &["test", "-p", spec.package, "--test", spec.target],
            &[],
        )?;
        require_passed_tests(
            &output,
            minimum_primary_passes(spec.name),
            &format!("{} primary integration target", spec.name),
        )?;
        require_named_tests_passed(
            &output,
            required_executed_primary_tests(spec.name),
            &format!("{} primary integration target", spec.name),
        )?;
        if spec.name == "test-language-v2" {
            run_analysis_language_gate(&mut runner)?;
        }
        if spec.name == "test-object-model-v2" {
            run_analysis_object_model_gate(&mut runner)?;
            run_canonical_object_model_gate(&mut runner)?;
        }
        if spec.name == "test-async-v2" {
            run_runtime_async_gate(&mut runner)?;
        }
        if spec.name == "test-nidl-v2" {
            let report = run_nidl_mutation_gate(&mut runner)?;
            machine_reports.push(report.display().to_string());
        }
        if spec.name == "test-structured-codegen" {
            run_structured_codegen_cargo_check(&mut runner)?;
        }
        if spec.editor {
            run_editor_gate(&mut runner)?;
            machine_reports.push(
                workspace_root()
                    .join(EDITOR_REPORT_PATH)
                    .display()
                    .to_string(),
            );
        }
        Ok(TestTargetEvidence {
            package: spec.package.to_owned(),
            target: spec.target.to_owned(),
            source: spec.source.to_owned(),
            registered_tests: registered_test_names.len(),
            registered_test_names,
        })
    })();
    let (test_target, mut failures) = match result {
        Ok(target) => (Some(target), Vec::new()),
        Err(error) => (None, vec![error.to_string()]),
    };
    let completed_head = git_optional(&["rev-parse", "HEAD"]);
    let completed_branch = current_branch();
    let completed_clean = worktree_clean();
    append_gate_checkout_failures(
        started_clean,
        &head,
        &branch,
        completed_clean,
        &completed_head,
        &completed_branch,
        &mut failures,
    );
    GateReceipt {
        schema: 1,
        gate: spec.name.to_owned(),
        head,
        branch,
        started_clean,
        completed_head,
        completed_branch,
        completed_clean,
        test_target,
        commands: runner.evidence,
        machine_reports,
        status: Status::from_passed(failures.is_empty()),
        failures,
    }
}

#[allow(clippy::too_many_lines)]
fn execute_scale_stress_gate() -> GateReceipt {
    let head = git_optional(&["rev-parse", "HEAD"]);
    let branch = current_branch();
    let started_clean = worktree_clean();
    if !started_clean || head == "missing" || branch == "missing" {
        return gate_precondition_failure("m4r1-scale-stress", head, branch, started_clean);
    }
    let mut runner = CommandRunner::default();
    let mut reports = Vec::new();
    let result: Result<(), DynError> = (|| {
        let root = workspace_root();
        run_incremental_stale_module_gate(&mut runner)?;
        let nidl_reload_report = root.join(NIDL_RELOAD_STRESS_REPORT_PATH);
        remove_stale_file(&nidl_reload_report)?;
        let reload_list = runner.run_captured(
            Path::new("cargo"),
            &[
                "test",
                "-p",
                "nexa-embed",
                "--test",
                "m4_reload_stress",
                "m4r1_nidl_reload_stress",
                "--",
                "--ignored",
                "--exact",
                "--list",
            ],
            &[],
        )?;
        if registered_test_count(&reload_list) != 1 {
            return Err(
                "m4_reload_stress::m4r1_nidl_reload_stress is not one registered exact test".into(),
            );
        }
        runner.run(
            Path::new("cargo"),
            &[
                "test",
                "-p",
                "nexa-embed",
                "--test",
                "m4_reload_stress",
                "m4r1_nidl_reload_stress",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
            &[("NEXA_M4R1_NIDL_RELOAD_STRESS_REPORT", &nidl_reload_report)],
        )?;
        validate_nidl_reload_stress_report(&nidl_reload_report)?;
        reports.push(nidl_reload_report.display().to_string());

        let executable = std::env::current_exe()?;
        runner.run(&executable, &["m4-scale-stress"], &[])?;
        validate_scale_reports(&root)?;
        reports.extend(
            [
                ANALYSIS_SCALE_REPORT_PATH,
                FACADE_SCALE_REPORT_PATH,
                RELOAD_STRESS_REPORT_PATH,
            ]
            .map(|path| root.join(path).display().to_string()),
        );

        let audit = run_legacy_audit();
        write_json_atomic(&root.join(LEGACY_AUDIT_PATH), &audit, "M4R1 legacy audit")?;
        reports.push(root.join(LEGACY_AUDIT_PATH).display().to_string());
        if audit.status != Status::Pass {
            return Err(format!(
                "M4R1 legacy audit failed:\n- {}",
                audit.failures.join("\n- ")
            )
            .into());
        }
        Ok(())
    })();
    let mut failures = result
        .err()
        .map_or_else(Vec::new, |error| vec![error.to_string()]);
    let completed_head = git_optional(&["rev-parse", "HEAD"]);
    let completed_branch = current_branch();
    let completed_clean = worktree_clean();
    append_gate_checkout_failures(
        started_clean,
        &head,
        &branch,
        completed_clean,
        &completed_head,
        &completed_branch,
        &mut failures,
    );
    GateReceipt {
        schema: 1,
        gate: "m4r1-scale-stress".into(),
        head,
        branch,
        started_clean,
        completed_head,
        completed_branch,
        completed_clean,
        test_target: None,
        commands: runner.evidence,
        machine_reports: reports,
        status: Status::from_passed(failures.is_empty()),
        failures,
    }
}

fn gate_precondition_failure(
    gate: &str,
    head: String,
    branch: String,
    started_clean: bool,
) -> GateReceipt {
    GateReceipt {
        schema: 1,
        gate: gate.into(),
        completed_head: head.clone(),
        completed_branch: branch.clone(),
        completed_clean: worktree_clean(),
        head,
        branch,
        started_clean,
        test_target: None,
        commands: Vec::new(),
        machine_reports: Vec::new(),
        failures: vec![
            "M4R1 final gate requires a clean, attached checkout; run the underlying targeted \
             tests directly while developing"
                .into(),
        ],
        status: Status::Fail,
    }
}

fn run_incremental_stale_module_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    const SOURCE: &str = "crates/nexa-analysis/tests/m4_incremental.rs";
    const TARGET: &str = "m4_incremental";
    const REQUIRED: [&str; 2] = [
        "m4_incremental_evidence",
        "deleted_dependency_module_cannot_resurrect_stale_consumer_typed_ir",
    ];
    require_test_target(SOURCE)?;
    let list = runner.run_captured(
        Path::new("cargo"),
        &[
            "test",
            "-p",
            "nexa-analysis",
            "--test",
            TARGET,
            "--",
            "--list",
        ],
        &[],
    )?;
    let names = registered_test_names(&list);
    for test in REQUIRED {
        if !names.iter().any(|name| name == test) {
            return Err(format!("{SOURCE} does not register required stress test `{test}`").into());
        }
        let output = runner.run_captured(
            Path::new("cargo"),
            &[
                "test",
                "-p",
                "nexa-analysis",
                "--test",
                TARGET,
                test,
                "--",
                "--exact",
            ],
            &[],
        )?;
        require_passed_tests(&output, 1, &format!("nexa-analysis::{TARGET}::{test}"))?;
    }
    Ok(())
}

fn require_test_target(source: &str) -> Result<(), DynError> {
    let path = workspace_root().join(source);
    let metadata = fs::metadata(&path).map_err(|error| {
        format!(
            "required integration target {} is missing: {error}",
            path.display()
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "required integration target {} is not a non-empty file",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn append_gate_checkout_failures(
    started_clean: bool,
    started_head: &str,
    started_branch: &str,
    completed_clean: bool,
    completed_head: &str,
    completed_branch: &str,
    failures: &mut Vec<String>,
) {
    if !started_clean {
        failures.push("gate must start from a clean worktree".into());
    }
    if started_head == "missing" || completed_head != started_head {
        failures.push(format!(
            "gate changed checkout HEAD: started={started_head}, completed={completed_head}"
        ));
    }
    if started_branch == "missing" || completed_branch != started_branch {
        failures.push(format!(
            "gate changed checkout branch: started={started_branch}, completed={completed_branch}"
        ));
    }
    if !completed_clean {
        failures.push("gate left the worktree dirty".into());
    }
}

fn list_test_target(
    runner: &mut CommandRunner,
    package: &str,
    target: &str,
) -> Result<Vec<String>, DynError> {
    let output = runner.run_captured(
        Path::new("cargo"),
        &["test", "-p", package, "--test", target, "--", "--list"],
        &[],
    )?;
    Ok(registered_test_names(&output))
}

fn require_registered_tests(
    gate: &str,
    source: &str,
    registered: &[String],
) -> Result<(), DynError> {
    let required = required_primary_tests(gate)
        .ok_or_else(|| format!("no frozen primary test matrix for M4R1 gate `{gate}`"))?;
    let missing = required
        .iter()
        .filter(|name| !registered.iter().any(|observed| observed == **name))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{source} is missing required M4R1 tests {missing:?}; registered={registered:?}"
        )
        .into())
    }
}

fn required_primary_tests(gate: &str) -> Option<&'static [&'static str]> {
    match gate {
        "test-language-v2" => Some(&[
            "language_v2_positive_surface_matrix",
            "removed_nexa_words_are_plain_identifiers_in_the_lexer",
            "legacy_surface_forms_are_rejected_at_the_removed_word",
            "mut_is_only_valid_for_let_and_class_fields",
            "prefix_await_is_rejected_but_full_postfix_chain_is_preserved",
            "named_attribute_arguments_preserve_names_and_classification",
            "naming_rules_have_stable_diagnostics",
        ]),
        "test-object-model-v2" => Some(&[
            "struct_updates_copy_values_and_enum_payloads_keep_their_tag",
            "class_copy_aliases_fields_and_equality_uses_object_identity",
            "class_write_barrier_is_atomic_and_retains_the_published_child",
            "rooted_class_cycle_survives_and_unrooted_cycle_is_reclaimed",
            "enum_and_struct_composites_trace_nested_class_references_exactly",
        ]),
        "test-async-v2" => Some(&[
            "postfix_await_and_yield_lower_to_the_runtime_task_model",
            "postfix_try_can_consume_an_awaited_result_in_one_expression",
            "await_outside_async_keeps_the_canonical_diagnostic_and_caller_span",
            "async_calls_must_be_consumed_immediately",
            "prefix_await_is_not_part_of_language_v2",
        ]),
        "test-nidl-v2" => Some(&[
            "parses_and_validates_the_complete_nidl_v2_surface",
            "rejects_every_removed_nidl_spelling",
            "validates_names_attributes_layouts_and_source_spans",
            "descriptor_obeys_frozen_order_and_comment_rules",
            "async_entrypoint_effect_changes_the_descriptor",
            "m4r1_nidl_mutation_stress",
        ]),
        "test-structured-codegen" => Some(&[
            "binding_model_contains_prevalidated_names_and_strategies",
            "generated_tokens_are_parsed_formatted_and_parsed_again",
            "illegal_or_colliding_names_never_reach_executable_rust",
            "codegen_descriptor_and_fingerprint_are_byte_deterministic",
            "snapshot_schema_tracks_the_transitive_type_layout",
            "generated_fixture_cargo_checks",
        ]),
        "test-standalone" => Some(&[
            "single_file_sync_main_receives_arguments_routes_console_and_returns_exit_code",
            "async_main_and_top_level_await_use_the_runtime_task_path",
            "top_level_scripts_receive_implicit_args",
            "standalone_rejects_main_conflicts_missing_main_and_wrong_signatures",
            "package_and_project_entrypoints_receive_arguments_without_function_indices",
            "bytecode_execution_is_only_available_through_exec",
            "standalone_traps_use_the_fixed_tool_exit_code",
        ]),
        "test-repl" => Some(&[
            "repl_persists_bindings_mutation_shadowing_functions_and_async_cells",
            "repl_inspection_and_maintenance_commands_use_the_compiled_session",
            "failed_cells_do_not_commit_and_reset_discards_the_old_environment",
            "repl_load_compiles_file_cells_into_the_current_session",
            "repl_resource_failures_recover_and_limit_flags_are_validated",
            "ctrl_c_cancels_only_the_current_cell_and_keeps_the_session_alive",
        ]),
        "test-entrypoints" => Some(&[
            "snake_packages_use_required_broadcast_and_typed_optional_routing",
            "required_missing_and_optional_signature_mismatch_are_rejected",
            "required_marker_must_be_declared_by_the_contract",
        ]),
        _ => None,
    }
}

fn minimum_primary_passes(gate: &str) -> usize {
    let ignored = usize::from(matches!(gate, "test-nidl-v2" | "test-structured-codegen"));
    required_primary_tests(gate)
        .map_or(usize::MAX, <[_]>::len)
        .saturating_sub(ignored)
}

fn required_executed_primary_tests(gate: &str) -> Vec<&'static str> {
    required_primary_tests(gate)
        .unwrap_or_default()
        .iter()
        .copied()
        .filter(|name| {
            !matches!(
                *name,
                "m4r1_nidl_mutation_stress" | "generated_fixture_cargo_checks"
            )
        })
        .collect()
}

fn require_named_tests_passed(
    output: &[u8],
    required: Vec<&str>,
    label: &str,
) -> Result<(), DynError> {
    let output = String::from_utf8_lossy(output);
    let missing = required
        .into_iter()
        .filter(|name| {
            !output.lines().any(|line| {
                let line = line.trim();
                line.starts_with(&format!("test {name} "))
                    && line
                        .strip_prefix(&format!("test {name} "))
                        .is_some_and(|result| result.trim_end().ends_with("ok"))
            })
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{label} did not execute required passing tests {missing:?}").into())
    }
}

fn require_passed_tests(output: &[u8], minimum: usize, label: &str) -> Result<(), DynError> {
    let observed = String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let (_, summary) = line.split_once("test result: ok. ")?;
            let (passed, _) = summary.split_once(" passed;")?;
            passed.trim().parse::<usize>().ok()
        })
        .max()
        .unwrap_or(0);
    if observed >= minimum {
        Ok(())
    } else {
        Err(format!(
            "{label} executed only {observed} passing tests; at least {minimum} are required"
        )
        .into())
    }
}

fn registered_test_count(output: &[u8]) -> usize {
    registered_test_names(output).len()
}

fn registered_test_names(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.trim().strip_suffix(": test").map(str::to_owned))
        .collect()
}

fn run_analysis_object_model_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    const SOURCE: &str = "crates/nexa-analysis/tests/object_model_v2.rs";
    require_test_target(SOURCE)?;
    let output = runner.run_captured(
        Path::new("cargo"),
        &[
            "test",
            "-p",
            "nexa-analysis",
            "--test",
            "object_model_v2",
            "--",
            "--list",
        ],
        &[],
    )?;
    let names = registered_test_names(&output);
    for required in [
        "struct_place_mutability_is_enforced",
        "recursive_value_types_are_rejected",
        "box_and_pointer_surface_types_are_rejected",
    ] {
        if !names.iter().any(|name| name == required) {
            return Err(format!(
                "{SOURCE} does not register required object-model test `{required}`: {names:?}"
            )
            .into());
        }
    }
    let output = runner.run_captured(
        Path::new("cargo"),
        &["test", "-p", "nexa-analysis", "--test", "object_model_v2"],
        &[],
    )?;
    require_passed_tests(&output, 3, "nexa-analysis object_model_v2")?;
    require_named_tests_passed(
        &output,
        vec![
            "struct_place_mutability_is_enforced",
            "recursive_value_types_are_rejected",
            "box_and_pointer_surface_types_are_rejected",
        ],
        "nexa-analysis object_model_v2",
    )
}

fn run_analysis_language_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    const SOURCE: &str = "crates/nexa-analysis/tests/language_v2.rs";
    require_test_target(SOURCE)?;
    let output = runner.run_captured(
        Path::new("cargo"),
        &[
            "test",
            "-p",
            "nexa-analysis",
            "--test",
            "language_v2",
            "--",
            "--list",
        ],
        &[],
    )?;
    let names = registered_test_names(&output);
    for required in [
        "language_v2_positive_matrix_analyzes",
        "legacy_surface_matrix_is_rejected",
    ] {
        if !names.iter().any(|name| name == required) {
            return Err(format!(
                "{SOURCE} does not register required language-v2 test `{required}`: {names:?}"
            )
            .into());
        }
    }
    let output = runner.run_captured(
        Path::new("cargo"),
        &["test", "-p", "nexa-analysis", "--test", "language_v2"],
        &[],
    )?;
    require_passed_tests(&output, 2, "nexa-analysis language_v2")?;
    require_named_tests_passed(
        &output,
        vec![
            "language_v2_positive_matrix_analyzes",
            "legacy_surface_matrix_is_rejected",
        ],
        "nexa-analysis language_v2",
    )
}

fn run_canonical_object_model_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    const SOURCE: &str = "crates/nexa/tests/m4r1_object_model_e2e.rs";
    const TARGET: &str = "m4r1_object_model_e2e";
    const TEST: &str = "canonical_object_model_source_executes_through_verified_bytecode";
    require_test_target(SOURCE)?;
    let list = runner.run_captured(
        Path::new("cargo"),
        &["test", "-p", "nexa", "--test", TARGET, "--", "--list"],
        &[],
    )?;
    if !registered_test_names(&list).iter().any(|name| name == TEST) {
        return Err(format!("{SOURCE} does not register required canonical test `{TEST}`").into());
    }
    let output = runner.run_captured(
        Path::new("cargo"),
        &[
            "test", "-p", "nexa", "--test", TARGET, TEST, "--", "--exact",
        ],
        &[],
    )?;
    require_passed_tests(&output, 1, &format!("nexa::{TARGET}::{TEST}"))
}

fn run_runtime_async_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    const FACADE_SOURCE: &str = "crates/nexa/tests/m4_host_nominal_async.rs";
    const FACADE_TARGET: &str = "m4_host_nominal_async";
    const FACADE_TEST: &str = "canonical_build_preserves_host_nominals_and_async_result_arms";

    for (target, source, required) in [
        (
            "task_lifecycle",
            "crates/nexa-runtime/tests/task_lifecycle.rs",
            &[
                "host_success_preserves_the_declared_nominal_payload",
                "task_cancel_returns_terminal_poll",
                "request_abandon_traps_without_invalid_task_state",
            ][..],
        ),
        (
            "restart_reload",
            "crates/nexa-runtime/tests/restart_reload.rs",
            &["late_completion_from_old_epoch_is_discarded"][..],
        ),
    ] {
        require_test_target(source)?;
        let output = runner.run_captured(
            Path::new("cargo"),
            &[
                "test",
                "-p",
                "nexa-runtime",
                "--test",
                target,
                "--",
                "--list",
            ],
            &[],
        )?;
        let names = registered_test_names(&output);
        for name in required {
            if !names.iter().any(|registered| registered == name) {
                return Err(format!(
                    "{source} does not register required async runtime test `{name}`: {names:?}"
                )
                .into());
            }
            let output = runner.run_captured(
                Path::new("cargo"),
                &[
                    "test",
                    "-p",
                    "nexa-runtime",
                    "--test",
                    target,
                    name,
                    "--",
                    "--exact",
                ],
                &[],
            )?;
            require_passed_tests(&output, 1, &format!("nexa-runtime {target}::{name}"))?;
        }
    }
    require_test_target(FACADE_SOURCE)?;
    let output = runner.run_captured(
        Path::new("cargo"),
        &[
            "test",
            "-p",
            "nexa",
            "--test",
            FACADE_TARGET,
            "--",
            "--list",
        ],
        &[],
    )?;
    let names = registered_test_names(&output);
    if !names.iter().any(|name| name == FACADE_TEST) {
        return Err(format!(
            "{FACADE_SOURCE} does not register required canonical async test `{FACADE_TEST}`"
        )
        .into());
    }
    let output = runner.run_captured(
        Path::new("cargo"),
        &[
            "test",
            "-p",
            "nexa",
            "--test",
            FACADE_TARGET,
            FACADE_TEST,
            "--",
            "--exact",
        ],
        &[],
    )?;
    require_passed_tests(&output, 1, &format!("nexa::{FACADE_TARGET}::{FACADE_TEST}"))?;
    Ok(())
}

fn run_nidl_mutation_gate(runner: &mut CommandRunner) -> Result<PathBuf, DynError> {
    let report = workspace_root().join(NIDL_STRESS_REPORT_PATH);
    remove_stale_file(&report)?;
    let args = [
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "nidl_v2",
        "m4r1_nidl_mutation_stress",
        "--",
        "--ignored",
        "--exact",
    ];
    let mut list_args = args.to_vec();
    list_args.push("--list");
    let list = runner.run_captured(Path::new("cargo"), &list_args, &[])?;
    if registered_test_count(&list) != 1 {
        return Err("nidl_v2::m4r1_nidl_mutation_stress is not one registered exact test".into());
    }
    let mut run_args = args.to_vec();
    run_args.push("--nocapture");
    let output = runner.run_captured(
        Path::new("cargo"),
        &run_args,
        &[("NEXA_M4R1_NIDL_STRESS_REPORT", &report)],
    )?;
    require_passed_tests(&output, 1, "nidl_v2::m4r1_nidl_mutation_stress")?;
    validate_nidl_stress_report(&report)?;
    Ok(report)
}

fn run_structured_codegen_cargo_check(runner: &mut CommandRunner) -> Result<(), DynError> {
    let args = [
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "structured_codegen",
        "generated_fixture_cargo_checks",
        "--",
        "--ignored",
        "--exact",
    ];
    let mut list_args = args.to_vec();
    list_args.push("--list");
    let output = runner.run_captured(Path::new("cargo"), &list_args, &[])?;
    if registered_test_count(&output) != 1 {
        return Err(
            "structured_codegen::generated_fixture_cargo_checks is not one registered exact test"
                .into(),
        );
    }
    let mut run_args = args.to_vec();
    run_args.push("--nocapture");
    let output = runner.run_captured(Path::new("cargo"), &run_args, &[])?;
    require_passed_tests(
        &output,
        1,
        "structured_codegen::generated_fixture_cargo_checks",
    )
}

fn run_editor_gate(runner: &mut CommandRunner) -> Result<(), DynError> {
    let root = workspace_root();
    let report = root.join(EDITOR_REPORT_PATH);
    remove_stale_file(&report)?;
    for command in ["generate", "check", "package"] {
        runner.run(Path::new("pnpm"), &["--dir", "editors", command], &[])?;
    }
    validate_editor_report(&report)
}

fn validate_editor_report(report: &Path) -> Result<(), DynError> {
    super::m4::validate_editor_report_for_m4r1(report)
}

fn validate_nidl_stress_report(path: &Path) -> Result<(), DynError> {
    let document = read_json(path, "M4R1 NIDL mutation report")?;
    let categories_pass = [
        "contract",
        "host_nexa",
        "attributes",
        "types",
        "naming",
        "duplicates",
        "recursive_layout",
        "illegal_async",
        "comments",
        "source_spans",
    ]
    .into_iter()
    .all(|category| {
        document
            .pointer(&format!("/categories/{category}"))
            .and_then(Value::as_u64)
            .is_some_and(|count| count > 0)
    });
    if document.get("schema").and_then(Value::as_u64) != Some(1)
        || document.get("status").and_then(Value::as_str) != Some("PASS")
        || document
            .get("mutations")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            < 100
        || !categories_pass
        || document
            .get("failures")
            .and_then(Value::as_array)
            .is_none_or(|failures| !failures.is_empty())
    {
        return Err(format!(
            "NIDL mutation report does not prove at least 100 successful mutations: {document}"
        )
        .into());
    }
    Ok(())
}

fn validate_nidl_reload_stress_report(path: &Path) -> Result<(), DynError> {
    let document = read_json(path, "M4R1 NIDL Reload stress report")?;
    let iterations = document
        .get("iterations")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let nidl_changes = document
        .pointer("/classes/nidl_change")
        .or_else(|| document.get("nidl_changes"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let created = document
        .pointer("/terminal/created")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let terminal = document
        .pointer("/terminal/terminal")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duplicate = document
        .pointer("/terminal/duplicate")
        .and_then(Value::as_u64);
    let missing = document
        .pointer("/terminal/missing")
        .and_then(Value::as_u64);
    let rejected = document
        .pointer("/outcomes/nidl_rejected")
        .and_then(Value::as_u64);
    let restored = document
        .pointer("/outcomes/restored_committed")
        .and_then(Value::as_u64);
    let contract_mismatch = document
        .pointer("/outcomes/host_contract_mismatch")
        .and_then(Value::as_u64);
    if document.get("schema").and_then(Value::as_u64) != Some(1)
        || document.get("status").and_then(Value::as_str) != Some("PASS")
        || iterations != 100
        || nidl_changes != 100
        || created != 200
        || terminal != 200
        || duplicate != Some(0)
        || missing != Some(0)
        || rejected != Some(100)
        || restored != Some(100)
        || contract_mismatch != Some(100)
        || document
            .get("failures")
            .and_then(Value::as_array)
            .is_none_or(|failures| !failures.is_empty())
    {
        return Err(format!(
            "NIDL Reload stress report does not prove 100 complete, safe NIDL changes: {document}"
        )
        .into());
    }
    validate_zero_safety_counters(&document, "NIDL Reload stress")
}

fn validate_scale_reports(root: &Path) -> Result<(), DynError> {
    super::m4::validate_scale_reports_for_m4r1(
        &root.join(ANALYSIS_SCALE_REPORT_PATH),
        &root.join(FACADE_SCALE_REPORT_PATH),
        &root.join(RELOAD_STRESS_REPORT_PATH),
    )?;
    let analysis = read_json(
        &root.join(ANALYSIS_SCALE_REPORT_PATH),
        "M4 analysis scale report",
    )?;
    let facade = read_json(
        &root.join(FACADE_SCALE_REPORT_PATH),
        "M4 facade scale report",
    )?;
    let stress = read_json(
        &root.join(RELOAD_STRESS_REPORT_PATH),
        "M4 reload stress report",
    )?;
    for (name, schema, report) in [
        ("analysis", 3, &analysis),
        ("facade", 3, &facade),
        ("reload", 2, &stress),
    ] {
        if report.get("schema").and_then(Value::as_u64) != Some(schema)
            || report.get("status").and_then(Value::as_str) != Some("PASS")
        {
            return Err(format!("{name} scale/stress report is not schema {schema} PASS").into());
        }
    }
    let analysis_identity = analysis
        .get("closure_identity")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let facade_identity = facade
        .get("closure_identity")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if analysis_identity.len() != 64 || facade_identity != analysis_identity {
        return Err(format!(
            "analysis/facade closure identity is not one shared 32-byte fingerprint: \
             analysis={analysis_identity:?}, facade={facade_identity:?}"
        )
        .into());
    }
    for (name, minimum) in [
        ("modules", 100),
        ("symbols", 1_000),
        ("import_edges", 500),
        ("packages", 20),
    ] {
        let observed = analysis
            .pointer(&format!("/scale/{name}"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let facade_observed = facade
            .pointer(&format!("/scale/{name}"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if observed < minimum || facade_observed < minimum || observed != facade_observed {
            return Err(format!(
                "scale counter `{name}` is not closed across analysis/facade: \
                 analysis={observed}, facade={facade_observed}, minimum={minimum}"
            )
            .into());
        }
    }
    for class in [
        "successful_reload",
        "syntax_failure",
        "type_failure",
        "verifier_failure",
        "migration_failure",
        "dependency_change",
        "aba_change",
        "add_module",
        "delete_module",
        "rename_module",
    ] {
        let count = stress
            .pointer(&format!("/classes/{class}"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if count < 100 {
            return Err(format!("reload stress class `{class}` has only {count} runs").into());
        }
    }
    validate_zero_safety_counters(&stress, "M4 Reload stress")
}

fn validate_zero_safety_counters(document: &Value, label: &str) -> Result<(), DynError> {
    for counter in [
        "stale_candidate_committed",
        "active_lkg_violation",
        "duplicate_terminal",
        "missing_terminal",
        "task_request_resource_growth",
        "release_queue_not_empty",
        "worker_residual",
    ] {
        let value = document
            .pointer(&format!("/safety/{counter}"))
            .and_then(Value::as_u64);
        if value != Some(0) {
            return Err(format!(
                "{label} safety counter `{counter}` is not exactly zero: {value:?}"
            )
            .into());
        }
    }
    Ok(())
}

fn validate_frozen_versions(
    root: &Path,
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
) {
    for (path, literal) in [
        (
            "crates/nexa-analysis/src/options.rs",
            "pub const NEXA_LANGUAGE_VERSION: u16 = 2;",
        ),
        (
            "crates/nexa-analysis/src/manifest.rs",
            "pub const PACKAGE_MANIFEST_SCHEMA: u32 = 2;",
        ),
        (
            "crates/nexa-contract/src/descriptor.rs",
            "pub const CONTRACT_SYNTAX_VERSION: u16 = 3;",
        ),
        (
            "crates/nexa-runtime/src/host.rs",
            "pub const HOST_CONTRACT_SCHEMA_VERSION: u32 = 2;",
        ),
        (
            "crates/nexa-contract/src/descriptor.rs",
            "pub const ABI_DESCRIPTOR_VERSION: u16 = 2;",
        ),
        (
            "crates/nexa-core/src/lib.rs",
            "pub const BYTECODE_VERSION: u16 = 7;",
        ),
    ] {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        if !source.contains(literal) {
            record_synthetic_finding(
                counts,
                evidence,
                "frozen_version_mismatch",
                path,
                &format!("missing exact frozen version declaration `{literal}`"),
            );
        }
    }
    let manifest =
        fs::read_to_string(root.join("crates/nexa-analysis/src/manifest.rs")).unwrap_or_default();
    let implementation = manifest
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(manifest.as_str());
    for needle in ["schema == 1", "schema <= 1", "schema < 2", "SchemaV1"] {
        record_literal_matches(
            counts,
            evidence,
            "schema1_package_compatibility",
            needle,
            "crates/nexa-analysis/src/manifest.rs",
            implementation,
        );
    }
    for required in [
        "if schema == PACKAGE_MANIFEST_SCHEMA",
        "Err(ManifestError::UnsupportedSchema(schema))",
    ] {
        if !implementation.contains(required) {
            record_synthetic_finding(
                counts,
                evidence,
                "schema1_package_compatibility",
                "crates/nexa-analysis/src/manifest.rs",
                &format!("schema-2-only validator is missing `{required}`"),
            );
        }
    }
}

fn validate_structured_codegen_backends(
    root: &Path,
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
) {
    for (path, required) in [
        (
            "crates/nexa-contract/src/codegen.rs",
            &[
                "pub fn generate_rust_tokens(",
                "Result<TokenStream, CodegenError>",
                "fn generate_model_tokens(",
                "syn::parse2::<syn::File>",
                "prettyplease::unparse",
                "syn::parse_file",
                "quote!",
                "let tokens = generate_rust_tokens(contract)?;",
            ][..],
        ),
        (
            "crates/nexa-machine/src/lib.rs",
            &[
                "pub fn generate_rust_tokens(&self) -> TokenStream",
                "syn::parse2::<syn::File>",
                "prettyplease::unparse",
                "syn::parse_file",
                "quote!",
                "render_rust(self.generate_rust_tokens())",
                "tokens.extend(spec.generate_rust_tokens());",
                "render_rust(tokens)",
            ][..],
        ),
    ] {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        for fragment in required {
            if !source.contains(fragment) {
                record_synthetic_finding(
                    counts,
                    evidence,
                    "structured_codegen_backend_missing",
                    path,
                    &format!("missing required structured backend fragment `{fragment}`"),
                );
            }
        }
        for forbidden in [
            "TokenStream::from_str(",
            "Literal::from_str(",
            ".parse::<TokenStream>()",
            ".parse::<proc_macro2::TokenStream>()",
        ] {
            record_literal_matches(
                counts,
                evidence,
                "string_rust_codegen",
                forbidden,
                path,
                &source,
            );
        }
    }
}

fn validate_completion_status_docs(
    root: &Path,
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
) {
    for path in ["README.md", "ROADMAP.md", "baseline/BASELINE_INDEX.md"] {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        let status = source.lines().take(80).collect::<Vec<_>>().join("\n");
        let status_lines = status.lines().map(str::trim).collect::<Vec<_>>();
        let complete = [
            "Nexa M4 Language Scale Foundation = COMPLETE",
            "Nexa M4R1 Language Surface Reset = COMPLETE",
            "Nexa Language v2 = COMPLETE",
            "NIDL v2 = COMPLETE",
            "Structured Codegen v2 = COMPLETE",
            "Standalone Profile v1 = COMPLETE",
            "REPL v1 = COMPLETE",
            "Multiple Entrypoint Model = COMPLETE",
        ]
        .into_iter()
        .all(|expected| status_lines.contains(&expected));
        if !complete || status.contains("FINALIZING") {
            record_synthetic_finding(
                counts,
                evidence,
                "status_not_complete",
                path,
                "M4R1 status block is not an exact COMPLETE block",
            );
        }
    }
    for path in [
        "docs/M4_LANGUAGE.md",
        "docs/MODULES.md",
        "docs/EMBEDDING.md",
        "docs/DEVELOPMENT_LOOP.md",
        "docs/STANDARD_LIBRARY.md",
        "docs/PACKAGE_TESTS.md",
        "docs/STANDALONE.md",
        "docs/REPL.md",
        "docs/NIDL.md",
    ] {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        let header = source.lines().take(20).collect::<Vec<_>>();
        let complete = header.iter().any(|line| {
            let line = line.trim();
            line.starts_with("Status:")
                && line.contains("M4R1")
                && line.split_whitespace().last() == Some("COMPLETE")
                && !line.contains("FINALIZING")
        });
        if !complete {
            record_synthetic_finding(
                counts,
                evidence,
                "status_not_complete",
                path,
                "M4R1 document header must contain an exact M4R1 COMPLETE status",
            );
        }
    }
    for path in [
        "baseline/internal/INTERNAL_LANGUAGE_SCOPE.md",
        "baseline/language/LANGUAGE_V2.md",
        "baseline/language/OBJECT_MODEL_V2.md",
        "baseline/language/ASYNC_V2.md",
        "baseline/language/STANDALONE_V2.md",
        "baseline/language/REPL_V1.md",
        "baseline/abi/NIDL_V2.md",
        "baseline/abi/BINDING_CODEGEN_V2.md",
    ] {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        let complete = source
            .lines()
            .take(20)
            .any(|line| line.trim() == "Status: **COMPLETE**");
        if !complete {
            record_synthetic_finding(
                counts,
                evidence,
                "status_not_complete",
                path,
                "M4R1 baseline header must contain exact `Status: **COMPLETE**`",
            );
        }
    }
}

fn validate_negative_corpus_allowlist(
    root: &Path,
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    validation: &mut BTreeMap<String, bool>,
) {
    let source =
        fs::read_to_string(root.join("fixtures/diagnostics/analysis/NX2701/src/main.nexa"))
            .unwrap_or_default();
    let case =
        fs::read_to_string(root.join("fixtures/diagnostics/cases/NX2701.json")).unwrap_or_default();
    let test = fs::read_to_string(root.join("crates/nexa-analysis/tests/m4_diagnostic_codes.rs"))
        .unwrap_or_default();
    let legacy_lines = source
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("module ")
                || line.starts_with("import ")
                || line.starts_with("var ")
                || contains_keyword_pair(line, "task", "fn")
        })
        .collect::<Vec<_>>();
    let consumed = source == "module wrong.name;\n\npub fn entry() -> i32 {\n    return 1;\n}\n"
        && legacy_lines == ["module wrong.name;"]
        && case.contains("\"code\":\"NX2701\"")
        && case.contains("\"input\":\"../analysis/NX2701\"")
        && test.contains("code: \"NX2701\"")
        && test.contains("directory: \"NX2701\"");
    validation.insert("NX2701_explicit_module_rejection".into(), consumed);
    if !consumed {
        record_synthetic_finding(
            counts,
            evidence,
            "negative_corpus_allowlist_invalid",
            "fixtures/diagnostics/analysis/NX2701/src/main.nexa",
            "allowlisted legacy source is not exact or is not consumed by its frozen diagnostic test",
        );
    }
    for (path, test, _) in EMBEDDED_NEGATIVE_TESTS {
        let source = fs::read_to_string(root.join(path)).unwrap_or_default();
        let valid = negative_test_evidence(path, test, &source);
        validation.insert(format!("embedded:{path}::{test}"), valid);
        if !valid {
            record_synthetic_finding(
                counts,
                evidence,
                "negative_corpus_allowlist_invalid",
                path,
                &format!(
                    "embedded legacy syntax allowlist entry `{test}` does not contain its frozen \
                     parser/compiler invocation and rejection assertion"
                ),
            );
        }
    }
}

fn registered_source_test_body<'a>(source: &'a str, expected: &str) -> Option<&'a str> {
    let lines = source.lines().collect::<Vec<_>>();
    let declaration_line = lines.iter().enumerate().find_map(|(index, line)| {
        let trimmed = line.trim_start();
        let remainder = trimmed.strip_prefix("fn ")?;
        let name = remainder
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .next()
            .unwrap_or_default();
        (name == expected
            && lines[index.saturating_sub(4)..index]
                .iter()
                .any(|attribute| attribute.trim() == "#[test]"))
        .then_some(index + 1)
    })?;
    let declaration_offset = source
        .lines()
        .take(declaration_line.saturating_sub(1))
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let skeleton = rust_code_skeleton(source);
    let opening = skeleton[declaration_offset..].find('{')? + declaration_offset;
    let mut depth = 0_usize;
    for (relative, byte) in skeleton.as_bytes()[opening..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return source.get(opening + 1..opening + relative);
                }
            }
            _ => {}
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
fn negative_test_evidence(path: &str, test: &str, source: &str) -> bool {
    let Some(body) = registered_source_test_body(source, test) else {
        return false;
    };
    let required: &[&str] = match (path, test) {
        (
            "crates/nexa-syntax/tests/language_v2.rs",
            "legacy_surface_forms_are_rejected_at_the_removed_word",
        ) => &[
            "let cases = [",
            "for (source, expected) in cases",
            "parse_nexa(source)",
            "parse_nexa_ast(&tree)",
            ".find(|error| error.message.contains(expected))",
            "error.range.start.get()",
            "module app::main;",
            "import host::snake;",
            "task fn run()",
            "immediate fn run()",
            "migration fn run()",
            "activation fn run()",
            "cleanup fn run()",
            "stateful class State",
            "var value = 1;",
            "await load();",
            "value with { score: 1 }",
        ],
        (
            "crates/nexa-syntax/tests/language_v2.rs",
            "prefix_await_is_rejected_but_full_postfix_chain_is_preserved",
        ) => &[
            "async fn run() { await load(); }",
            "parse_nexa(",
            "parse_nexa_ast(",
            ".any(|error| error.message.contains(\"prefix `await`\"))",
            "${await load()}",
        ],
        ("crates/nexa-contract/tests/nidl_v2.rs", "rejects_every_removed_nidl_spelling") => &[
            "let cases = [",
            "for source in cases",
            "parse(source).is_err()",
            "interface Old",
            "opaque Entity",
            "sync fn log",
            "request fn load",
            "export Run",
            "array<i32>",
            "buffer<i32>",
            "option<i32>",
            "result<i32, i32>",
            "token<Entity>",
            "snapshot<Entity>",
            "request<i32>",
            "Request<i32>",
            "host_request<i32>",
            "void",
        ],
        ("crates/nexa-compiler/tests/async_v2.rs", "prefix_await_is_not_part_of_language_v2") => &[
            "nexa_compiler::compile(",
            "return await produce();",
            ".expect_err(\"language v2 accepts only postfix `.await`\")",
        ],
        (
            "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
            "virtual_snippet_rejects_removed_module_syntax_without_losing_origin",
        ) => &[
            "compile_file(\"module Game.Combat;\\n\", file).unwrap_err()",
            "CompileError::AnalysisDiagnostic",
            "diagnostic.message.contains(\"module\")",
            "AnalysisDiagnosticSource::Caller",
            "diagnostic.primary.span.start < diagnostic.primary.span.end",
        ],
        (
            "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
            "removed_nexa_v1_surface_forms_are_rejected",
        ) => &[
            "let removed = [",
            "for (name, source) in removed",
            "nexa_compiler::compile(source).is_err()",
            "module old.surface",
            "import std.core",
            "task fn old_task",
            "return await child()",
            "stateful class OldState",
            "migration fn migrate",
            "activation fn activate",
            "cleanup fn cleanup",
            "immediate fn calculate",
            "cell with {",
        ],
        (
            "crates/nexa-compiler/tests/m4_virtual_snippet.rs",
            "nidl_v2_accepts_contract_blocks_and_rejects_removed_surface_forms",
        ) => &[
            "let removed = [",
            "for (name, source) in removed",
            "nexa_contract::parse(source).is_err()",
            "interface Old",
            "opaque Ticket",
            "sync fn ping",
            "request(return_error, trap) fn load",
            "export OnEvent",
            "array<i32>",
            "void",
        ],
        ("crates/nexa-cli/tests/m4_tooling.rs", "single_file_rejects_legacy_module_headers") => &[
            "fs::write(&source, \"module main;",
            "fixture.run(&[\"check\", path(&source), \"--diagnostic-format\", \"json\"])",
            "assert_exit(&rejected, 1)",
            "serde_json::from_slice(&rejected.stderr)",
            "message.contains(\"module\")",
        ],
        ("crates/nexa-cli/src/lsp.rs", "lsp_v2_rejects_legacy_surface_at_the_legacy_token") => &[
            "let source = \"module legacy;",
            "nexa_diagnostic_with_code(path, source, nexa::ErrorCode::NX1002)",
            "lsp_range_contains_source_range",
        ],
        (
            "crates/nexa-cli/src/project.rs",
            "project_config_rejects_the_removed_required_exports_key",
        ) => &[
            "schema = 2",
            "required_exports = []",
            "toml::from_str::<ProjectConfig>(legacy).is_err()",
        ],
        ("crates/nexa-analysis/tests/language_v2.rs", "legacy_surface_matrix_is_rejected") => &[
            "for (name, source) in CASES",
            "let outcome = analyze_sources",
            "!outcome.diagnostics.diagnostics().is_empty()",
            "assert_analysis_rejected(&outcome)",
            "module wrong.name;",
            "import package.support;",
            "var value: i32 = 0;",
            "task fn run()",
            "return await load();",
            "stateful class State",
            "pub migration fn migrate()",
            "pub activation fn activate()",
            "pub cleanup fn cleanup()",
            "pub immediate fn calculate()",
            "cell with { x: 2 }",
        ],
        _ => return false,
    };
    required.iter().all(|needle| body.contains(needle))
        && match (path, test) {
            ("crates/nexa-cli/src/lsp.rs", "lsp_v2_rejects_legacy_surface_at_the_legacy_token") => {
                source.contains("super::diagnostics_for_path(path, Some(source))")
                    && source.contains(".find(|diagnostic| diagnostic.diagnostic.code == code)")
            }
            ("crates/nexa-analysis/tests/language_v2.rs", "legacy_surface_matrix_is_rejected") => {
                source.contains("analyze_package(")
                    && source.contains("outcome.ir.is_none()")
                    && source.contains("!outcome.diagnostics.diagnostics().is_empty()")
            }
            _ => true,
        }
}

#[allow(clippy::too_many_lines)]
fn run_legacy_audit() -> LegacyAudit {
    let root = workspace_root();
    let scanned_roots = vec![
        "crates/*/src".to_owned(),
        "**/*.nexa".to_owned(),
        "**/*.nidl".to_owned(),
        "**/*.idl".to_owned(),
        "**/*.toml".to_owned(),
        "**/build.rs".to_owned(),
        "editors/tree-sitter-*/grammar.js".to_owned(),
        "editors/language-syntax.json".to_owned(),
    ];
    let mut files = Vec::new();
    let mut scan_errors = Vec::new();
    collect_audit_files(&root, &mut files, &mut scan_errors);
    files.sort();
    let mut counts = BTreeMap::new();
    let mut evidence = Vec::new();
    let mut allowlisted_negative_corpus = Vec::new();
    let mut allowlist_validation = BTreeMap::new();
    for (path, error) in scan_errors {
        record_synthetic_finding(
            &mut counts,
            &mut evidence,
            "audit_scan_error",
            &path,
            &error,
        );
    }

    validate_frozen_versions(&root, &mut counts, &mut evidence);
    validate_structured_codegen_backends(&root, &mut counts, &mut evidence);
    validate_completion_status_docs(&root, &mut counts, &mut evidence);
    validate_negative_corpus_allowlist(
        &root,
        &mut counts,
        &mut evidence,
        &mut allowlist_validation,
    );

    for file in &files {
        let relative = relative_path(&root, file);
        let source = match fs::read_to_string(file) {
            Ok(source) => source,
            Err(error) => {
                record_synthetic_finding(
                    &mut counts,
                    &mut evidence,
                    "audit_scan_error",
                    &relative,
                    &format!("could not read selected audit file: {error}"),
                );
                continue;
            }
        };
        let is_rust = file.extension().and_then(|extension| extension.to_str()) == Some("rs");
        let in_product_rust =
            is_rust && relative.starts_with("crates/") && relative.contains("/src/");
        if is_rust {
            audit_embedded_surface_source(
                &mut counts,
                &mut evidence,
                &mut allowlisted_negative_corpus,
                &relative,
                &source,
            );
        }
        if in_product_rust {
            record_identifier_matches(
                &mut counts,
                &mut evidence,
                "legacy_function_index",
                "LEGACY_FUNCTION_INDEX",
                &relative,
                &source,
            );
            for name in [
                "decode_v5",
                "decode_bytecode_v5",
                "decode_version_5",
                "BytecodeV5",
                "SchemaV1",
                "BYTECODE_V5",
            ] {
                record_identifier_matches(
                    &mut counts,
                    &mut evidence,
                    "bytecode_v5_or_schema1_compat",
                    name,
                    &relative,
                    &source,
                );
            }
            if relative == "crates/nexa-runtime/src/realm.rs" {
                let normalized = source.split_whitespace().collect::<Vec<_>>().join(" ");
                for signature in [
                    "pub fn spawn_task( &mut self, module: ModuleHandle, function: u32,",
                    "pub fn resolve_export<E: crate::ScriptExport>( &self, module: ModuleHandle, \
                     ) -> Result<u32,",
                ] {
                    record_literal_matches(
                        &mut counts,
                        &mut evidence,
                        "public_function_index_api",
                        signature,
                        &relative,
                        &normalized,
                    );
                }
            }
            if relative.starts_with("crates/nexa-bytecode/src/") {
                for needle in ["version == 5", "version == 1", "schema == 1", "::V5"] {
                    record_literal_matches(
                        &mut counts,
                        &mut evidence,
                        "bytecode_v5_or_schema1_compat",
                        needle,
                        &relative,
                        &source,
                    );
                }
            }
        }
        if relative.starts_with("crates/nexa-contract/src/") {
            for needle in ["fn tokenize(", "struct Parser ", "struct Parser<"] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "private_nidl_parser",
                    needle,
                    &relative,
                    &source,
                );
            }
            for needle in [
                "output: &mut String",
                "writeln!(output",
                "write!(output",
                "output.push_str(",
                "fn emit_host_dispatch(",
                "LEGACY_FUNCTION_INDEX",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "string_rust_codegen",
                    needle,
                    &relative,
                    &source,
                );
            }
            for identifier in [
                "canonical_required_exports",
                "canonical_all_required_exports",
                "exact_hash",
                "abi_type_descriptor",
            ] {
                record_identifier_matches(
                    &mut counts,
                    &mut evidence,
                    "legacy_canonical_string_hash",
                    identifier,
                    &relative,
                    &source,
                );
            }
        }
        if relative.starts_with("crates/nexa-machine/src/") {
            for needle in [
                "writeln!(generated",
                "generated.push_str(",
                "output: &mut String",
                "let mut output = String",
                "writeln!(output",
                "write!(output",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "string_rust_codegen",
                    needle,
                    &relative,
                    &source,
                );
            }
        }
        if relative.ends_with("build.rs") {
            for needle in [
                "writeln!(generated",
                "generated.push_str(",
                "LEGACY_FUNCTION_INDEX",
                "fs::write(&output, generate_rust(",
                "fs::write(output, generate_rust(",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "build_rs_codegen_bypass",
                    needle,
                    &relative,
                    &source,
                );
            }
            let compact = source.split_whitespace().collect::<String>();
            for needle in [
                "fs::write(&output,generate_rust(",
                "fs::write(&output,super::generate_rust(",
                "fs::write(output,generate_rust(",
                "fs::write(output,super::generate_rust(",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "build_rs_codegen_bypass",
                    needle,
                    &relative,
                    &compact,
                );
            }
        }
        if relative.starts_with("crates/nexa-syntax/src/") {
            for identifier in [
                "Keyword::Var",
                "Keyword::Module",
                "Keyword::Import",
                "Keyword::Task",
                "Keyword::Immediate",
                "Keyword::Migration",
                "Keyword::Activation",
                "Keyword::Cleanup",
                "Keyword::Stateful",
                "Keyword::With",
                "Keyword::Interface",
                "Keyword::Opaque",
                "ModuleDeclaration",
                "ImportDeclaration",
            ] {
                record_identifier_matches(
                    &mut counts,
                    &mut evidence,
                    "legacy_surface_semantic_implementation",
                    identifier,
                    &relative,
                    &source,
                );
            }
        }
        if matches!(
            relative.as_str(),
            "editors/tree-sitter-nexa/grammar.js" | "editors/language-syntax.json"
        ) {
            for needle in [
                "\"var\"",
                "\"module\"",
                "\"import\"",
                "\"task\"",
                "\"stateful\"",
                "\"with\"",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "legacy_editor_grammar",
                    needle,
                    &relative,
                    &source,
                );
            }
        }
        if matches!(
            relative.as_str(),
            "editors/tree-sitter-nexa-idl/grammar.js" | "editors/language-syntax.json"
        ) {
            for needle in [
                "\"interface\"",
                "\"opaque\"",
                "\"sync\"",
                "\"request\"",
                "\"export\"",
                "\"void\"",
                "\"array\"",
            ] {
                record_literal_matches(
                    &mut counts,
                    &mut evidence,
                    "legacy_editor_grammar",
                    needle,
                    &relative,
                    &source,
                );
            }
        }
        if matches!(
            file.extension().and_then(|extension| extension.to_str()),
            Some("nidl" | "idl")
        ) {
            audit_nidl_source(&mut counts, &mut evidence, &relative, &source);
        }
        if file.extension().and_then(|extension| extension.to_str()) == Some("nexa") {
            audit_nexa_source(
                &mut counts,
                &mut evidence,
                &mut allowlisted_negative_corpus,
                &relative,
                &source,
            );
        }
        if file.extension().and_then(|extension| extension.to_str()) == Some("toml") {
            audit_manifest_source(&mut counts, &mut evidence, &relative, &source);
        }
    }

    let exact_allowlist = allowlisted_negative_corpus.iter().all(|finding| {
        (finding.path == "fixtures/diagnostics/analysis/NX2701/src/main.nexa"
            && finding.excerpt == "module wrong.name;"
            && finding.context.as_deref() == Some("NX2701_explicit_module_rejection"))
            || (finding.pattern == "legacy_embedded_surface_source"
                && embedded_negative_allowed(&finding.path, finding.context.as_deref()))
    }) && allowlisted_negative_corpus
        .iter()
        .filter(|finding| finding.path == "fixtures/diagnostics/analysis/NX2701/src/main.nexa")
        .count()
        == 1
        && EMBEDDED_NEGATIVE_TESTS
            .iter()
            .all(|(path, test, expected_count)| {
                allowlisted_negative_corpus
                    .iter()
                    .filter(|finding| {
                        finding.path == *path && finding.context.as_deref() == Some(*test)
                    })
                    .count()
                    == *expected_count
            })
        && allowlist_validation.values().all(|valid| *valid);
    allowlist_validation.insert("exact_allowlist_closed".into(), exact_allowlist);
    if !exact_allowlist {
        record_synthetic_finding(
            &mut counts,
            &mut evidence,
            "negative_corpus_allowlist_invalid",
            "fixtures/diagnostics/analysis/NX2701/src/main.nexa",
            "legacy source allowlist must equal the frozen NX2701 plus 62 embedded negative inputs",
        );
    }

    for name in [
        "legacy_function_index",
        "public_function_index_api",
        "bytecode_v5_or_schema1_compat",
        "private_nidl_parser",
        "string_rust_codegen",
        "build_rs_codegen_bypass",
        "structured_codegen_backend_missing",
        "frozen_version_mismatch",
        "schema1_package_compatibility",
        "legacy_canonical_string_hash",
        "legacy_surface_semantic_implementation",
        "legacy_editor_grammar",
        "legacy_nidl_source",
        "legacy_nexa_source",
        "legacy_box_pointer_surface",
        "legacy_manifest_required_exports",
        "manifest_schema_mismatch",
        "legacy_embedded_surface_source",
        "negative_corpus_allowlist_invalid",
        "audit_scan_error",
        "status_not_complete",
    ] {
        counts.entry(name.into()).or_insert(0);
    }
    let failures = counts
        .iter()
        .filter(|(_, count)| **count != 0)
        .map(|(name, count)| format!("`{name}` has {count} active occurrence(s)"))
        .collect::<Vec<_>>();
    LegacyAudit {
        schema: 1,
        scanned_roots,
        scanned_files: files.len(),
        pattern_counts: counts,
        evidence,
        allowlisted_negative_corpus,
        allowlist_validation,
        status: Status::from_passed(failures.is_empty()),
        failures,
    }
}

fn audit_nidl_source(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    path: &str,
    source: &str,
) {
    let code = strip_surface_trivia(source);
    let mut found_line = false;
    for (index, (original, code_line)) in source.lines().zip(code.lines()).enumerate() {
        let line = code_line.trim();
        if nidl_surface_marker(line) {
            found_line = true;
            record_line_finding(
                counts,
                evidence,
                "legacy_nidl_source",
                path,
                index + 1,
                original,
            );
        }
    }
    let normalized = code.split_whitespace().collect::<Vec<_>>().join(" ");
    if !found_line && nidl_surface_marker(&normalized) {
        record_synthetic_finding(
            counts,
            evidence,
            "legacy_nidl_source",
            path,
            "legacy NIDL surface spans trivia or line boundaries",
        );
    }
}

fn audit_nexa_source(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    allowlisted: &mut Vec<Finding>,
    path: &str,
    source: &str,
) {
    let code = strip_surface_trivia(source);
    let mut found_line = false;
    for (index, (original, code_line)) in source.lines().zip(code.lines()).enumerate() {
        let line = code_line.trim();
        if nexa_surface_marker(line) {
            found_line = true;
            let pattern = if contains_forbidden_pointer_surface(line) {
                "legacy_box_pointer_surface"
            } else {
                "legacy_nexa_source"
            };
            if path == "fixtures/diagnostics/analysis/NX2701/src/main.nexa"
                && line == "module wrong.name;"
            {
                allowlisted.push(Finding {
                    pattern: "legacy_nexa_source".into(),
                    path: path.into(),
                    line: index + 1,
                    excerpt: original.trim().chars().take(180).collect(),
                    context: Some("NX2701_explicit_module_rejection".into()),
                });
            } else {
                record_line_finding(counts, evidence, pattern, path, index + 1, original);
            }
        }
    }
    let normalized = code.split_whitespace().collect::<Vec<_>>().join(" ");
    if !found_line && nexa_surface_marker(&normalized) {
        record_synthetic_finding(
            counts,
            evidence,
            if contains_forbidden_pointer_surface(&normalized) {
                "legacy_box_pointer_surface"
            } else {
                "legacy_nexa_source"
            },
            path,
            "legacy Nexa surface spans trivia or line boundaries",
        );
    }
}

fn audit_manifest_source(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    path: &str,
    source: &str,
) {
    if matches!(
        Path::new(path).file_name().and_then(|name| name.to_str()),
        Some("package.toml" | "nexa.dev.toml")
    ) {
        match root_manifest_schema(source) {
            Ok(2) => {}
            Ok(schema) => record_synthetic_finding(
                counts,
                evidence,
                "manifest_schema_mismatch",
                path,
                &format!("root package/source-set schema is {schema}; expected exactly 2"),
            ),
            Err(error) => {
                record_synthetic_finding(
                    counts,
                    evidence,
                    "manifest_schema_mismatch",
                    path,
                    &error,
                );
            }
        }
    }
    for (index, original) in source.lines().enumerate() {
        let line = original.split('#').next().unwrap_or_default().trim();
        let key = line.split_once('=').map(|(key, _)| {
            key.trim()
                .trim_matches(|character| matches!(character, '"' | '\''))
        });
        if key == Some("required_exports") {
            record_line_finding(
                counts,
                evidence,
                "legacy_manifest_required_exports",
                path,
                index + 1,
                original,
            );
        }
    }
}

fn root_manifest_schema(source: &str) -> Result<u32, String> {
    let mut schema = None;
    for original in source.lines() {
        let line = strip_toml_comment(original).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        let Some((raw_key, raw_value)) = split_toml_assignment(line) else {
            continue;
        };
        let key = raw_key
            .trim()
            .trim_matches(|character| matches!(character, '"' | '\''));
        if key != "schema" {
            continue;
        }
        if schema.is_some() {
            return Err("root package/source-set manifest declares `schema` more than once".into());
        }
        let value = raw_value.trim().replace('_', "");
        schema =
            Some(value.parse::<u32>().map_err(|_| {
                format!("root package/source-set schema is not an integer: {value}")
            })?);
    }
    schema.ok_or_else(|| "root package/source-set manifest is missing `schema = 2`".into())
}

fn split_toml_assignment(line: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(active) if character == active => quote = None,
            None if matches!(character, '"' | '\'') => quote = Some(character),
            None if character == '=' => return Some((&line[..index], &line[index + 1..])),
            Some(_) | None => {}
        }
    }
    None
}

fn strip_toml_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(active) if character == active => quote = None,
            None if matches!(character, '"' | '\'') => quote = Some(character),
            None if character == '#' => return &line[..index],
            Some(_) | None => {}
        }
    }
    line
}

fn strip_surface_trivia(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code(Option<u32>),
        LineComment(Option<u32>),
        BlockComment {
            depth: u32,
            interpolation: Option<u32>,
        },
        String {
            escaped: bool,
            return_interpolation: Option<u32>,
        },
    }

    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut state = State::Code(None);
    let mut interpolation_returns = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            State::Code(interpolation) if bytes[index..].starts_with(b"//") => {
                output.extend_from_slice(b"  ");
                state = State::LineComment(interpolation);
                index += 2;
                continue;
            }
            State::Code(interpolation) if bytes[index..].starts_with(b"/*") => {
                output.extend_from_slice(b"  ");
                state = State::BlockComment {
                    depth: 1,
                    interpolation,
                };
                index += 2;
                continue;
            }
            State::Code(interpolation) if byte == b'"' => {
                output.push(b' ');
                state = State::String {
                    escaped: false,
                    return_interpolation: interpolation,
                };
            }
            State::Code(Some(depth)) if byte == b'{' => {
                output.push(byte);
                state = State::Code(Some(depth.saturating_add(1)));
            }
            State::Code(Some(1)) if byte == b'}' => {
                output.push(b' ');
                state = State::String {
                    escaped: false,
                    return_interpolation: interpolation_returns.pop().unwrap_or(None),
                };
            }
            State::Code(Some(depth)) if byte == b'}' => {
                output.push(byte);
                state = State::Code(Some(depth - 1));
            }
            State::Code(_) => output.push(byte),
            State::LineComment(interpolation) => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                if byte == b'\n' {
                    state = State::Code(interpolation);
                }
            }
            State::BlockComment {
                depth,
                interpolation,
            } if bytes[index..].starts_with(b"/*") => {
                output.extend_from_slice(b"  ");
                state = State::BlockComment {
                    depth: depth.saturating_add(1),
                    interpolation,
                };
                index += 2;
                continue;
            }
            State::BlockComment {
                depth,
                interpolation,
            } if bytes[index..].starts_with(b"*/") => {
                output.extend_from_slice(b"  ");
                state = if depth == 1 {
                    State::Code(interpolation)
                } else {
                    State::BlockComment {
                        depth: depth - 1,
                        interpolation,
                    }
                };
                index += 2;
                continue;
            }
            State::BlockComment { .. } => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
            }
            State::String {
                escaped,
                return_interpolation,
            } if !escaped && bytes[index..].starts_with(b"${") => {
                output.extend_from_slice(b"  ");
                interpolation_returns.push(return_interpolation);
                state = State::Code(Some(1));
                index += 2;
                continue;
            }
            State::String {
                escaped,
                return_interpolation,
            } => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                state = if escaped {
                    State::String {
                        escaped: false,
                        return_interpolation,
                    }
                } else if byte == b'\\' {
                    State::String {
                        escaped: true,
                        return_interpolation,
                    }
                } else if byte == b'"' {
                    State::Code(return_interpolation)
                } else {
                    State::String {
                        escaped: false,
                        return_interpolation,
                    }
                };
            }
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn nidl_surface_marker(line: &str) -> bool {
    ["interface", "opaque", "export"]
        .into_iter()
        .any(|keyword| contains_old_named_declaration(line, keyword))
        || contains_keyword_pair(line, "sync", "fn")
        || contains_keyword_pair(line, "request", "fn")
        || contains_legacy_request_policy(line)
        || contains_generic_name(line, "request")
        || contains_generic_name(line, "Request")
        || contains_generic_name(line, "host_request")
        || ["array", "buffer", "option", "result", "token", "snapshot"]
            .into_iter()
            .any(|name| contains_generic_name(line, name))
        || contains_type_name(line, "void")
}

fn nexa_surface_marker(line: &str) -> bool {
    contains_removed_path_declaration_anywhere(line, "module")
        || contains_removed_path_declaration_anywhere(line, "import")
        || contains_removed_var_declaration_anywhere(line)
        || contains_keyword_pair(line, "stateful", "class")
        || line.contains("@stateful")
        || contains_keyword_pair(line, "task", "fn")
        || contains_keyword_pair(line, "migration", "fn")
        || contains_keyword_pair(line, "activation", "fn")
        || contains_keyword_pair(line, "cleanup", "fn")
        || contains_keyword_pair(line, "immediate", "fn")
        || contains_legacy_with_update(line)
        || contains_forbidden_pointer_surface(line)
        || contains_prefix_await(line)
}

fn contains_legacy_with_update(line: &str) -> bool {
    line.match_indices("with").any(|(offset, _)| {
        identifier_boundaries(line.as_bytes(), offset, "with".len())
            && line[offset + "with".len()..].trim_start().starts_with('{')
    })
}

fn contains_legacy_request_policy(line: &str) -> bool {
    line.match_indices("request").any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, "request".len()) {
            return false;
        }
        let remainder = line[offset + "request".len()..].trim_start();
        let Some(policy) = remainder.strip_prefix('(') else {
            return false;
        };
        policy
            .split_once(')')
            .is_some_and(|(_, after)| contains_identifier(after, "fn"))
    })
}

fn contains_generic_name(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(offset, _)| {
        identifier_boundaries(line.as_bytes(), offset, name.len())
            && line[offset + name.len()..].trim_start().starts_with('<')
    })
}

fn contains_type_name(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(offset, _)| {
        identifier_boundaries(line.as_bytes(), offset, name.len())
            && matches!(
                line[..offset]
                    .chars()
                    .rev()
                    .find(|character| !character.is_whitespace()),
                Some(':' | '>' | '<' | ',' | '(')
            )
    })
}

fn contains_identifier(line: &str, identifier: &str) -> bool {
    line.match_indices(identifier)
        .any(|(offset, _)| identifier_boundaries(line.as_bytes(), offset, identifier.len()))
}

fn contains_keyword_pair(line: &str, first: &str, second: &str) -> bool {
    line.match_indices(first).any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, first.len()) {
            return false;
        }
        let remainder = line[offset + first.len()..].trim_start();
        remainder.starts_with(second)
            && identifier_boundaries(remainder.as_bytes(), 0, second.len())
    })
}

fn contains_forbidden_pointer_surface(line: &str) -> bool {
    ["Box<", "Box <", "Gc<", "Gc <", "Ref<", "Ref <", "&mut"]
        .into_iter()
        .any(|needle| line.contains(needle))
        || [": *", "-> *", "<*", ": &", "-> &", "<&"]
            .into_iter()
            .any(|needle| line.contains(needle))
}

fn contains_prefix_await(line: &str) -> bool {
    line.match_indices("await").any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, "await".len()) {
            return false;
        }
        let previous = line[..offset]
            .chars()
            .rev()
            .find(|character| !character.is_whitespace());
        previous != Some('.')
    })
}

fn audit_embedded_surface_source(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    allowlisted: &mut Vec<Finding>,
    path: &str,
    source: &str,
) {
    let mut literals = BTreeMap::<usize, Vec<RustStringLine>>::new();
    for line in rust_string_lines(source) {
        literals.entry(line.literal).or_default().push(line);
    }
    for lines in literals.into_values() {
        let Some(first) = lines.first() else {
            continue;
        };
        let literal_line = first.line;
        let literal = lines
            .iter()
            .map(|line| line.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let decoded = literal
            .replace("\\\n", " ")
            .replace("\\n", " ")
            .replace("\\r", " ")
            .replace("\\t", " ");
        let code = strip_surface_trivia(&decoded);
        let auditable = code.split_whitespace().collect::<Vec<_>>().join(" ");
        if embedded_legacy_marker(&auditable).is_none() {
            continue;
        }
        let excerpt = literal.trim().chars().take(180).collect::<String>();
        let test = enclosing_test_name(source, literal_line);
        if embedded_negative_allowed(path, test.as_deref()) {
            allowlisted.push(Finding {
                pattern: "legacy_embedded_surface_source".into(),
                path: path.into(),
                line: literal_line,
                excerpt,
                context: test,
            });
        } else {
            record_line_finding(
                counts,
                evidence,
                "legacy_embedded_surface_source",
                path,
                literal_line,
                &literal,
            );
        }
    }
}

fn embedded_legacy_marker(line: &str) -> Option<&'static str> {
    contains_removed_path_declaration_anywhere(line, "module")
        .then_some("module declaration")
        .or_else(|| {
            contains_removed_path_declaration_anywhere(line, "import")
                .then_some("import declaration")
        })
        .or_else(|| contains_removed_var_declaration_anywhere(line).then_some("var declaration"))
        .or_else(|| contains_keyword_pair(line, "task", "fn").then_some("task fn"))
        .or_else(|| contains_keyword_pair(line, "stateful", "class").then_some("stateful class"))
        .or_else(|| line.contains("@stateful").then_some("@stateful"))
        .or_else(|| contains_keyword_pair(line, "migration", "fn").then_some("migration fn"))
        .or_else(|| contains_keyword_pair(line, "activation", "fn").then_some("activation fn"))
        .or_else(|| contains_keyword_pair(line, "cleanup", "fn").then_some("cleanup fn"))
        .or_else(|| contains_keyword_pair(line, "immediate", "fn").then_some("immediate fn"))
        .or_else(|| line.contains("return await ").then_some("prefix await"))
        .or_else(|| line.contains("{ await ").then_some("prefix await"))
        .or_else(|| {
            (contains_prefix_await(line) && (line.contains(';') || line.contains('{')))
                .then_some("prefix await")
        })
        .or_else(|| contains_legacy_with_update(line).then_some("with update"))
        .or_else(|| contains_keyword_pair(line, "sync", "fn").then_some("sync fn"))
        .or_else(|| contains_keyword_pair(line, "request", "fn").then_some("request fn"))
        .or_else(|| contains_legacy_request_policy(line).then_some("request policy"))
        .or_else(|| {
            [
                "request", "Request", "array", "buffer", "option", "result", "token", "snapshot",
            ]
            .into_iter()
            .find(|name| contains_generic_name(line, name))
        })
        .or_else(|| contains_generic_name(line, "host_request").then_some("host_request"))
        .or_else(|| contains_type_name(line, "void").then_some("void"))
        .or_else(|| {
            line.contains("required_exports =")
                .then_some("required_exports")
        })
        .or_else(|| {
            ["interface", "opaque", "export"]
                .into_iter()
                .find(|keyword| contains_old_named_declaration(line, keyword))
        })
}

fn contains_removed_path_declaration_anywhere(line: &str, keyword: &str) -> bool {
    line.match_indices(keyword).any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, keyword.len()) {
            return false;
        }
        let remainder = line[offset + keyword.len()..].trim_start();
        let path_length = remainder
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':'))
            .count();
        if path_length == 0 {
            return false;
        }
        let tail = remainder[path_length..].trim_start();
        tail.starts_with(';')
            || (keyword == "import"
                && tail.strip_prefix("as").is_some_and(|alias| {
                    let alias = alias.trim_start();
                    let length = alias
                        .bytes()
                        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                        .count();
                    length > 0 && alias[length..].trim_start().starts_with(';')
                }))
    })
}

fn contains_removed_var_declaration_anywhere(line: &str) -> bool {
    line.match_indices("var").any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, "var".len()) {
            return false;
        }
        let remainder = line[offset + "var".len()..].trim_start();
        let name_length = remainder
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        name_length > 0
            && matches!(
                remainder[name_length..]
                    .chars()
                    .find(|character| !character.is_whitespace()),
                Some(':' | '=')
            )
    })
}

fn contains_old_named_declaration(line: &str, keyword: &str) -> bool {
    line.match_indices(keyword).any(|(offset, _)| {
        if !identifier_boundaries(line.as_bytes(), offset, keyword.len()) {
            return false;
        }
        let remainder = line[offset + keyword.len()..].trim_start();
        remainder
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
            && (remainder.contains('{') || remainder.contains(';'))
    })
}

#[derive(Debug)]
struct RustStringLine {
    literal: usize,
    line: usize,
    content: String,
}

#[derive(Clone, Copy)]
enum RustLexState {
    Code,
    LineComment,
    BlockComment(u32),
    String { escaped: bool },
    RawString { hashes: usize },
    Character { escaped: bool },
}

fn rust_string_lines(source: &str) -> Vec<RustStringLine> {
    let bytes = source.as_bytes();
    let mut result = Vec::new();
    let mut content = Vec::new();
    let mut line = 1_usize;
    let mut content_line = 1_usize;
    let mut literal = 0_usize;
    let mut state = RustLexState::Code;
    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            RustLexState::Code => {
                if bytes[index..].starts_with(b"//") {
                    state = RustLexState::LineComment;
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"/*") {
                    state = RustLexState::BlockComment(1);
                    index += 2;
                    continue;
                }
                if let Some((content_start, hashes)) = raw_string_start(bytes, index) {
                    literal += 1;
                    state = RustLexState::RawString { hashes };
                    content.clear();
                    content_line = line;
                    index = content_start;
                    continue;
                }
                if byte == b'"' {
                    literal += 1;
                    state = RustLexState::String { escaped: false };
                    content.clear();
                    content_line = line;
                } else if byte == b'\'' && character_literal_start(bytes, index) {
                    state = RustLexState::Character { escaped: false };
                }
            }
            RustLexState::LineComment => {
                if byte == b'\n' {
                    state = RustLexState::Code;
                }
            }
            RustLexState::BlockComment(depth) => {
                if bytes[index..].starts_with(b"/*") {
                    state = RustLexState::BlockComment(depth.saturating_add(1));
                    index += 2;
                    continue;
                }
                if bytes[index..].starts_with(b"*/") {
                    state = if depth == 1 {
                        RustLexState::Code
                    } else {
                        RustLexState::BlockComment(depth - 1)
                    };
                    index += 2;
                    continue;
                }
            }
            RustLexState::String { escaped } => {
                if escaped {
                    content.push(byte);
                    state = RustLexState::String { escaped: false };
                } else if byte == b'\\' {
                    content.push(byte);
                    state = RustLexState::String { escaped: true };
                } else if byte == b'"' {
                    publish_string_line(&mut result, literal, content_line, &content);
                    content.clear();
                    state = RustLexState::Code;
                } else {
                    content.push(byte);
                    if byte == b'\n' {
                        publish_string_line(&mut result, literal, content_line, &content);
                        content.clear();
                        content_line = line + 1;
                    }
                }
            }
            RustLexState::RawString { hashes } => {
                if raw_string_end(bytes, index, hashes) {
                    publish_string_line(&mut result, literal, content_line, &content);
                    content.clear();
                    state = RustLexState::Code;
                    index += hashes;
                } else {
                    content.push(byte);
                    if byte == b'\n' {
                        publish_string_line(&mut result, literal, content_line, &content);
                        content.clear();
                        content_line = line + 1;
                    }
                }
            }
            RustLexState::Character { escaped } => {
                if escaped {
                    state = RustLexState::Character { escaped: false };
                } else if byte == b'\\' {
                    state = RustLexState::Character { escaped: true };
                } else if byte == b'\'' {
                    state = RustLexState::Code;
                }
            }
        }
        if byte == b'\n' {
            line += 1;
        }
        index += 1;
    }
    result
}

fn publish_string_line(
    output: &mut Vec<RustStringLine>,
    literal: usize,
    line: usize,
    content: &[u8],
) {
    if !content.is_empty() {
        output.push(RustStringLine {
            literal,
            line,
            content: String::from_utf8_lossy(content).into_owned(),
        });
    }
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let mut cursor = index;
    if bytes.get(cursor) == Some(&b'b') || bytes.get(cursor) == Some(&b'c') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hashes_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor + 1, cursor - hashes_start))
}

fn raw_string_end(bytes: &[u8], index: usize, hashes: usize) -> bool {
    bytes.get(index) == Some(&b'"')
        && (0..hashes).all(|offset| bytes.get(index + 1 + offset) == Some(&b'#'))
}

fn character_literal_start(bytes: &[u8], index: usize) -> bool {
    let Some(next) = bytes.get(index + 1).copied() else {
        return false;
    };
    next == b'\\' || bytes.get(index + 2) == Some(&b'\'')
}

fn enclosing_test_name(source: &str, target_line: usize) -> Option<String> {
    let code = rust_code_skeleton(source);
    let mut pending_test = false;
    let mut pending_name = None;
    let mut current_test: Option<(String, usize)> = None;
    let mut depth = 0_usize;
    for (index, line) in code.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim_start();
        if trimmed == "#[test]" {
            pending_test = true;
        } else if pending_test && trimmed.starts_with("#[") {
            // Preserve the pending test across attributes such as `#[ignore]`.
        } else if pending_test {
            if let Some(remainder) = trimmed.strip_prefix("fn ") {
                let name = remainder
                    .split(|character: char| {
                        !(character.is_ascii_alphanumeric() || character == '_')
                    })
                    .next()
                    .unwrap_or_default();
                pending_name = (!name.is_empty()).then(|| name.to_owned());
            }
            pending_test = false;
        }
        for character in line.chars() {
            if character == '{' {
                depth += 1;
                if let Some(name) = pending_name.take() {
                    current_test = Some((name, depth));
                }
            } else if character == '}' {
                if current_test
                    .as_ref()
                    .is_some_and(|(_, body_depth)| *body_depth == depth)
                {
                    current_test = None;
                }
                depth = depth.saturating_sub(1);
            }
        }
        if line_number == target_line {
            return current_test.map(|(name, _)| name);
        }
    }
    None
}

fn rust_code_skeleton(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut state = RustLexState::Code;
    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            RustLexState::Code if bytes[index..].starts_with(b"//") => {
                output.extend_from_slice(b"  ");
                state = RustLexState::LineComment;
                index += 2;
                continue;
            }
            RustLexState::Code if bytes[index..].starts_with(b"/*") => {
                output.extend_from_slice(b"  ");
                state = RustLexState::BlockComment(1);
                index += 2;
                continue;
            }
            RustLexState::Code => {
                if let Some((content_start, hashes)) = raw_string_start(bytes, index) {
                    output.resize(output.len() + content_start - index, b' ');
                    state = RustLexState::RawString { hashes };
                    index = content_start;
                    continue;
                }
                if byte == b'"' {
                    output.push(b' ');
                    state = RustLexState::String { escaped: false };
                } else if byte == b'\'' && character_literal_start(bytes, index) {
                    output.push(b' ');
                    state = RustLexState::Character { escaped: false };
                } else {
                    output.push(byte);
                }
            }
            RustLexState::LineComment => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                if byte == b'\n' {
                    state = RustLexState::Code;
                }
            }
            RustLexState::BlockComment(depth) if bytes[index..].starts_with(b"/*") => {
                output.extend_from_slice(b"  ");
                state = RustLexState::BlockComment(depth.saturating_add(1));
                index += 2;
                continue;
            }
            RustLexState::BlockComment(depth) if bytes[index..].starts_with(b"*/") => {
                output.extend_from_slice(b"  ");
                state = if depth == 1 {
                    RustLexState::Code
                } else {
                    RustLexState::BlockComment(depth - 1)
                };
                index += 2;
                continue;
            }
            RustLexState::BlockComment(_) => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
            }
            RustLexState::String { escaped } => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                state = if escaped {
                    RustLexState::String { escaped: false }
                } else if byte == b'\\' {
                    RustLexState::String { escaped: true }
                } else if byte == b'"' {
                    RustLexState::Code
                } else {
                    RustLexState::String { escaped: false }
                };
            }
            RustLexState::RawString { hashes } => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                if raw_string_end(bytes, index, hashes) {
                    output.extend(std::iter::repeat_n(b' ', hashes));
                    state = RustLexState::Code;
                    index += hashes;
                }
            }
            RustLexState::Character { escaped } => {
                output.push(if byte == b'\n' { b'\n' } else { b' ' });
                state = if escaped {
                    RustLexState::Character { escaped: false }
                } else if byte == b'\\' {
                    RustLexState::Character { escaped: true }
                } else if byte == b'\'' {
                    RustLexState::Code
                } else {
                    RustLexState::Character { escaped: false }
                };
            }
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn embedded_negative_allowed(path: &str, test: Option<&str>) -> bool {
    test.is_some_and(|test| {
        EMBEDDED_NEGATIVE_TESTS
            .iter()
            .any(|(allowed_path, allowed_test, _)| path == *allowed_path && test == *allowed_test)
    })
}

fn collect_audit_files(root: &Path, output: &mut Vec<PathBuf>, errors: &mut Vec<(String, String)>) {
    let tracked = match Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
    {
        Ok(result) if result.status.success() => result.stdout,
        Ok(result) => {
            errors.push((
                ".".into(),
                format!(
                    "`git ls-files -z` failed with {}: {}",
                    result
                        .status
                        .code()
                        .map_or_else(|| "signal".into(), |code| code.to_string()),
                    String::from_utf8_lossy(&result.stderr).trim()
                ),
            ));
            return;
        }
        Err(error) => {
            errors.push((
                ".".into(),
                format!("could not start `git ls-files`: {error}"),
            ));
            return;
        }
    };
    for encoded in tracked
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = match std::str::from_utf8(encoded) {
            Ok(relative) => relative,
            Err(error) => {
                errors.push((
                    ".".into(),
                    format!("tracked audit path is not UTF-8: {error}"),
                ));
                continue;
            }
        };
        let path = root.join(relative);
        if !audit_file_selected(root, &path) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push((
                    relative_path(root, &path),
                    format!("could not inspect audit path: {error}"),
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            errors.push((
                relative.into(),
                "tracked selected audit file is a symlink and is not followed".into(),
            ));
            continue;
        }
        if metadata.is_file() {
            output.push(path);
        } else {
            errors.push((
                relative.into(),
                "tracked selected audit path is not a regular file".into(),
            ));
        }
    }
}

fn audit_file_selected(root: &Path, path: &Path) -> bool {
    let relative = relative_path(root, path);
    if relative == "tools/xtask/src/m4r1.rs" {
        return false;
    }
    let extension = path.extension().and_then(|extension| extension.to_str());
    matches!(extension, Some("nexa" | "nidl" | "idl" | "toml"))
        || relative.ends_with("build.rs")
        || (extension == Some("rs")
            && (relative.starts_with("crates/") || relative.starts_with("examples/")))
        || relative == "editors/tree-sitter-nexa/grammar.js"
        || relative == "editors/tree-sitter-nexa-idl/grammar.js"
        || relative == "editors/language-syntax.json"
}

fn record_literal_matches(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    pattern: &str,
    needle: &str,
    path: &str,
    source: &str,
) {
    for (line_index, line) in source.lines().enumerate() {
        let occurrences = line.match_indices(needle).count();
        for _ in 0..occurrences {
            record_line_finding(counts, evidence, pattern, path, line_index + 1, line);
        }
    }
}

fn record_identifier_matches(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    pattern: &str,
    identifier: &str,
    path: &str,
    source: &str,
) {
    for (line_index, line) in source.lines().enumerate() {
        let occurrences = line
            .match_indices(identifier)
            .filter(|(offset, _)| identifier_boundaries(line.as_bytes(), *offset, identifier.len()))
            .count();
        for _ in 0..occurrences {
            record_line_finding(counts, evidence, pattern, path, line_index + 1, line);
        }
    }
}

fn identifier_boundaries(bytes: &[u8], offset: usize, length: usize) -> bool {
    let before = offset
        .checked_sub(1)
        .and_then(|index| bytes.get(index))
        .copied();
    let after = bytes.get(offset + length).copied();
    !before.is_some_and(identifier_byte) && !after.is_some_and(identifier_byte)
}

const fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn record_synthetic_finding(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    pattern: &str,
    path: &str,
    excerpt: &str,
) {
    record_line_finding(counts, evidence, pattern, path, 1, excerpt);
}

fn record_line_finding(
    counts: &mut BTreeMap<String, usize>,
    evidence: &mut Vec<Finding>,
    pattern: &str,
    path: &str,
    line: usize,
    excerpt: &str,
) {
    *counts.entry(pattern.to_owned()).or_insert(0) += 1;
    if evidence.len() < 200 {
        evidence.push(Finding {
            pattern: pattern.to_owned(),
            path: path.to_owned(),
            line,
            excerpt: excerpt.trim().chars().take(180).collect(),
            context: None,
        });
    }
}

fn validate_regression(receipt: &RegressionReceipt, head: &str, failures: &mut Vec<String>) {
    if receipt.schema != 1
        || receipt.status != Status::Pass
        || receipt.head != head
        || !receipt.worktree_clean
    {
        failures.push(format!(
            "M1-M4 regression receipt is not a clean PASS for HEAD {head}: \
             schema={}, head={}, clean={}, status={:?}",
            receipt.schema, receipt.head, receipt.worktree_clean, receipt.status
        ));
    }
}

fn validate_gate_set(gates: &[GateReceipt], head: &str, failures: &mut Vec<String>) {
    let expected = TEST_GATES
        .iter()
        .map(|spec| spec.name)
        .chain(std::iter::once("m4r1-scale-stress"))
        .collect::<Vec<_>>();
    let observed = gates
        .iter()
        .map(|receipt| receipt.gate.as_str())
        .collect::<Vec<_>>();
    if observed != expected {
        failures.push(format!(
            "M4R1 gate set/order is not exact: observed={observed:?}, expected={expected:?}"
        ));
    }
    for receipt in gates {
        if !receipt.reusable_for(&receipt.gate, head) {
            failures.push(format!(
                "{} is not a clean PASS receipt for finalized HEAD {head}: {:?}",
                receipt.gate, receipt.failures
            ));
        }
        if receipt.gate != "m4r1-scale-stress"
            && receipt
                .test_target
                .as_ref()
                .is_none_or(|target| target.registered_tests == 0)
        {
            failures.push(format!(
                "{} has no non-zero registered test evidence",
                receipt.gate
            ));
        }
    }
}

fn validate_previous_tags(
    tags: &BTreeMap<String, TagEvidence>,
    remote: &RemotePublication,
    failures: &mut Vec<String>,
) {
    for (name, expected_object, expected_target) in PREVIOUS_MILESTONE_TAGS {
        let Some(tag) = tags.get(name) else {
            failures.push(format!("historical tag `{name}` is missing"));
            continue;
        };
        if tag.object_type != "tag"
            || tag.object != expected_object
            || tag.target != expected_target
        {
            failures.push(format!(
                "historical annotated tag `{name}` changed: type={}, object={}, target={}; \
                 expected_object={expected_object}, expected_target={expected_target}",
                tag.object_type, tag.object, tag.target
            ));
        }
        let remote_tag = remote.previous_tags.get(name);
        if remote_tag.is_none_or(|tag| {
            tag.object_type != "tag"
                || tag.object != expected_object
                || tag.target != expected_target
        }) {
            failures.push(format!(
                "origin historical annotated tag `{name}` is missing or changed: {remote_tag:?}"
            ));
        }
    }
}

fn validate_tested_checkout(checkout: &CheckoutEvidence, failures: &mut Vec<String>) {
    if checkout.branch != "main" {
        failures.push(format!(
            "M4R1 gates must start on `main`, observed `{}`",
            checkout.branch
        ));
    }
    if checkout.head == "missing"
        || checkout.head != checkout.main_head
        || checkout.head != checkout.milestone_branch_head
    {
        failures.push(format!(
            "tested checkout is not closed on one local HEAD: head={}, main={}, milestone={}",
            checkout.head, checkout.main_head, checkout.milestone_branch_head
        ));
    }
    if !checkout.worktree_clean {
        failures.push("M4R1 gates must start from a clean worktree".into());
    }
}

fn validate_completion(
    checkout: &CheckoutEvidence,
    tag: &TagEvidence,
    remote: &RemotePublication,
    failures: &mut Vec<String>,
) {
    if checkout.branch != "main" {
        failures.push(format!(
            "M4R1 finalization must run on `main`, observed `{}`",
            checkout.branch
        ));
    }
    if checkout.head == "missing" || checkout.head != checkout.main_head {
        failures.push(format!(
            "current HEAD `{}` does not equal local main `{}`",
            checkout.head, checkout.main_head
        ));
    }
    if checkout.milestone_branch_head != checkout.head {
        failures.push(format!(
            "local `{COMPLETION_BRANCH}` `{}` does not equal finalized HEAD `{}`",
            checkout.milestone_branch_head, checkout.head
        ));
    }
    if !checkout.worktree_clean {
        failures.push("M4R1 finalization requires a clean worktree".into());
    }
    if tag.object_type != "tag" || tag.target != checkout.head {
        failures.push(format!(
            "`{COMPLETION_TAG}` is not an annotated tag targeting HEAD: \
             type={}, target={}, head={}",
            tag.object_type, tag.target, checkout.head
        ));
    }
    if let Some(error) = &remote.query_error {
        failures.push(format!("remote publication could not be verified: {error}"));
    }
    if remote.remote != "origin" || !remote.canonical_remote {
        failures.push(format!(
            "remote publication was not queried from the canonical `origin`: \
             remote={}, url={}",
            remote.remote, remote.remote_url
        ));
    }
    if remote.main_head != checkout.head
        || remote.milestone_branch_head != checkout.head
        || remote.completion_tag_target != checkout.head
        || remote.completion_tag_object != tag.object
    {
        failures.push(format!(
            "origin publication is not closed on HEAD {}: main={}, milestone={}, \
             tag_object={}, local_tag_object={}, tag_target={}",
            checkout.head,
            remote.main_head,
            remote.milestone_branch_head,
            remote.completion_tag_object,
            tag.object,
            remote.completion_tag_target
        ));
    }
}

fn load_reusable_gate(name: &str, head: &str) -> Option<GateReceipt> {
    let receipt = read_gate_receipt(name)?;
    receipt.reusable_for(name, head).then_some(receipt)
}

fn load_reusable_regression(head: &str) -> Option<RegressionReceipt> {
    let receipt = load_regression_receipt()?;
    (receipt.schema == 1
        && receipt.status == Status::Pass
        && receipt.head == head
        && receipt.worktree_clean)
        .then_some(receipt)
}

fn read_gate_receipt(name: &str) -> Option<GateReceipt> {
    serde_json::from_slice(&fs::read(gate_receipt_path(name)).ok()?).ok()
}

fn load_regression_receipt() -> Option<RegressionReceipt> {
    serde_json::from_slice(&fs::read(workspace_root().join(REGRESSION_RECEIPT_PATH)).ok()?).ok()
}

fn write_gate_receipt(receipt: &GateReceipt) -> Result<(), DynError> {
    write_json_atomic(
        &gate_receipt_path(&receipt.gate),
        receipt,
        &format!("{} gate receipt", receipt.gate),
    )
}

fn gate_receipt_path(name: &str) -> PathBuf {
    workspace_root()
        .join(GATE_DIRECTORY)
        .join(format!("{name}.json"))
}

fn receipt_result(receipt: &GateReceipt) -> Result<(), DynError> {
    if receipt.status == Status::Pass {
        Ok(())
    } else {
        Err(format!(
            "{} failed:\n- {}",
            receipt.gate,
            receipt.failures.join("\n- ")
        )
        .into())
    }
}

#[allow(clippy::too_many_lines)]
fn gate_command_matrix_matches(receipt: &GateReceipt) -> bool {
    let primary = TEST_GATES
        .iter()
        .find(|spec| spec.name == receipt.gate)
        .is_some_and(|spec| {
            primary_test_evidence_matches(receipt, *spec)
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        spec.package,
                        "--test",
                        spec.target,
                        "--",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &["test", "-p", spec.package, "--test", spec.target],
                )
        });
    match receipt.gate.as_str() {
        "test-language-v2" => {
            primary
                && has_test_target_pair(receipt, "nexa-analysis", "language_v2")
                && ["generate", "check", "package"]
                    .into_iter()
                    .all(|command| has_command(receipt, "pnpm", &["--dir", "editors", command]))
        }
        "test-object-model-v2" => {
            primary
                && has_test_target_pair(receipt, "nexa-analysis", "object_model_v2")
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa",
                        "--test",
                        "m4r1_object_model_e2e",
                        "--",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa",
                        "--test",
                        "m4r1_object_model_e2e",
                        "canonical_object_model_source_executes_through_verified_bytecode",
                        "--",
                        "--exact",
                    ],
                )
        }
        "test-async-v2" => {
            primary
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-runtime",
                        "--test",
                        "task_lifecycle",
                        "--",
                        "--list",
                    ],
                )
                && [
                    "host_success_preserves_the_declared_nominal_payload",
                    "task_cancel_returns_terminal_poll",
                    "request_abandon_traps_without_invalid_task_state",
                ]
                .into_iter()
                .all(|name| {
                    has_command(
                        receipt,
                        "cargo",
                        &[
                            "test",
                            "-p",
                            "nexa-runtime",
                            "--test",
                            "task_lifecycle",
                            name,
                            "--",
                            "--exact",
                        ],
                    )
                })
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-runtime",
                        "--test",
                        "restart_reload",
                        "--",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-runtime",
                        "--test",
                        "restart_reload",
                        "late_completion_from_old_epoch_is_discarded",
                        "--",
                        "--exact",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa",
                        "--test",
                        "m4_host_nominal_async",
                        "--",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa",
                        "--test",
                        "m4_host_nominal_async",
                        "canonical_build_preserves_host_nominals_and_async_result_arms",
                        "--",
                        "--exact",
                    ],
                )
        }
        "test-structured-codegen" => {
            primary
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-contract",
                        "--test",
                        "structured_codegen",
                        "generated_fixture_cargo_checks",
                        "--",
                        "--ignored",
                        "--exact",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-contract",
                        "--test",
                        "structured_codegen",
                        "generated_fixture_cargo_checks",
                        "--",
                        "--ignored",
                        "--exact",
                        "--nocapture",
                    ],
                )
        }
        "test-nidl-v2" => {
            primary
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-contract",
                        "--test",
                        "nidl_v2",
                        "m4r1_nidl_mutation_stress",
                        "--",
                        "--ignored",
                        "--exact",
                        "--list",
                    ],
                )
                && has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-contract",
                        "--test",
                        "nidl_v2",
                        "m4r1_nidl_mutation_stress",
                        "--",
                        "--ignored",
                        "--exact",
                        "--nocapture",
                    ],
                )
        }
        "test-standalone" | "test-repl" | "test-entrypoints" => primary,
        "m4r1-scale-stress" => {
            has_command(
                receipt,
                "cargo",
                &[
                    "test",
                    "-p",
                    "nexa-analysis",
                    "--test",
                    "m4_incremental",
                    "--",
                    "--list",
                ],
            ) && [
                "m4_incremental_evidence",
                "deleted_dependency_module_cannot_resurrect_stale_consumer_typed_ir",
            ]
            .into_iter()
            .all(|test| {
                has_command(
                    receipt,
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-analysis",
                        "--test",
                        "m4_incremental",
                        test,
                        "--",
                        "--exact",
                    ],
                )
            }) && has_command(
                receipt,
                "cargo",
                &[
                    "test",
                    "-p",
                    "nexa-embed",
                    "--test",
                    "m4_reload_stress",
                    "m4r1_nidl_reload_stress",
                    "--",
                    "--ignored",
                    "--exact",
                    "--list",
                ],
            ) && has_command(
                receipt,
                "cargo",
                &[
                    "test",
                    "-p",
                    "nexa-embed",
                    "--test",
                    "m4_reload_stress",
                    "m4r1_nidl_reload_stress",
                    "--",
                    "--ignored",
                    "--exact",
                    "--nocapture",
                ],
            ) && receipt.commands.iter().any(|command| {
                command.args == ["m4-scale-stress"]
                    && Path::new(&command.program)
                        .file_stem()
                        .and_then(|name| name.to_str())
                        == Some("xtask")
            })
        }
        _ => false,
    }
}

fn primary_test_evidence_matches(receipt: &GateReceipt, spec: TestGateSpec) -> bool {
    let Some(target) = &receipt.test_target else {
        return false;
    };
    target.package == spec.package
        && target.target == spec.target
        && target.source == spec.source
        && target.registered_tests == target.registered_test_names.len()
        && target.registered_tests >= minimum_primary_passes(spec.name)
        && required_primary_tests(spec.name).is_some_and(|required| {
            required.iter().all(|name| {
                target
                    .registered_test_names
                    .iter()
                    .any(|registered| registered == name)
            })
        })
}

fn machine_report_matrix_matches(receipt: &GateReceipt) -> bool {
    let root = workspace_root();
    let expected = match receipt.gate.as_str() {
        "test-language-v2" => vec![root.join(EDITOR_REPORT_PATH)],
        "test-nidl-v2" => vec![root.join(NIDL_STRESS_REPORT_PATH)],
        "m4r1-scale-stress" => vec![
            root.join(NIDL_RELOAD_STRESS_REPORT_PATH),
            root.join(ANALYSIS_SCALE_REPORT_PATH),
            root.join(FACADE_SCALE_REPORT_PATH),
            root.join(RELOAD_STRESS_REPORT_PATH),
            root.join(LEGACY_AUDIT_PATH),
        ],
        _ => Vec::new(),
    };
    if receipt.machine_reports
        != expected
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
    {
        return false;
    }
    match receipt.gate.as_str() {
        "test-language-v2" => validate_editor_report(&root.join(EDITOR_REPORT_PATH)).is_ok(),
        "test-nidl-v2" => validate_nidl_stress_report(&root.join(NIDL_STRESS_REPORT_PATH)).is_ok(),
        "m4r1-scale-stress" => {
            validate_nidl_reload_stress_report(&root.join(NIDL_RELOAD_STRESS_REPORT_PATH)).is_ok()
                && validate_scale_reports(&root).is_ok()
                && read_legacy_audit(&root.join(LEGACY_AUDIT_PATH))
                    .is_ok_and(|audit| audit.status == Status::Pass && audit.failures.is_empty())
        }
        _ => true,
    }
}

fn read_legacy_audit(path: &Path) -> Result<LegacyAudit, DynError> {
    let bytes = fs::read(path)
        .map_err(|error| format!("could not read legacy audit {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("legacy audit {} is not JSON: {error}", path.display()).into())
}

fn has_test_target_pair(receipt: &GateReceipt, package: &str, target: &str) -> bool {
    has_command(
        receipt,
        "cargo",
        &["test", "-p", package, "--test", target, "--", "--list"],
    ) && has_command(receipt, "cargo", &["test", "-p", package, "--test", target])
}

fn has_command(receipt: &GateReceipt, program: &str, args: &[&str]) -> bool {
    receipt.commands.iter().any(|command| {
        Path::new(&command.program)
            .file_stem()
            .and_then(|name| name.to_str())
            == Some(program)
            && command.args == owned_args(args)
    })
}

fn checkout_evidence() -> CheckoutEvidence {
    CheckoutEvidence {
        branch: current_branch(),
        head: git_optional(&["rev-parse", "HEAD"]),
        main_head: git_optional(&["rev-parse", "refs/heads/main"]),
        milestone_branch_head: git_optional(&[
            "rev-parse",
            &format!("refs/heads/{COMPLETION_BRANCH}"),
        ]),
        worktree_clean: worktree_clean(),
    }
}

fn load_tag(name: &str) -> TagEvidence {
    let reference = format!("refs/tags/{name}");
    TagEvidence {
        object_type: git_optional(&["cat-file", "-t", &reference]),
        object: git_optional(&["rev-parse", &reference]),
        target: git_optional(&["rev-parse", &format!("{reference}^{{}}")]),
    }
}

#[allow(clippy::too_many_lines)]
fn load_remote_publication() -> RemotePublication {
    let remote_url = match git_origin_url() {
        Ok(remote_url) => remote_url,
        Err(error) => return failed_remote_publication("missing", error),
    };
    if !canonical_origin_url(&remote_url) {
        return failed_remote_publication(
            &remote_url,
            format!("`origin` points to `{remote_url}`; expected the canonical Nexa repository"),
        );
    }
    let main = "refs/heads/main";
    let milestone = "refs/heads/codex/language-scale-m4-r1";
    let tag = "refs/tags/language-scale-m4-complete-r1";
    let peeled = "refs/tags/language-scale-m4-complete-r1^{}";
    let mut queries = vec![
        main.to_owned(),
        milestone.to_owned(),
        tag.to_owned(),
        peeled.to_owned(),
    ];
    for (name, _, _) in PREVIOUS_MILESTONE_TAGS {
        queries.push(format!("refs/tags/{name}"));
        queries.push(format!("refs/tags/{name}^{{}}"));
    }
    let output = Command::new("git")
        .args(["ls-remote", "origin"])
        .args(&queries)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(workspace_root())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let references = String::from_utf8(output.stdout)
                .ok()
                .map(|stdout| {
                    stdout
                        .lines()
                        .filter_map(|line| {
                            let (object, reference) = line.split_once('\t')?;
                            Some((reference.to_owned(), object.to_owned()))
                        })
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let previous_tags = PREVIOUS_MILESTONE_TAGS
                .into_iter()
                .map(|(name, _, _)| {
                    let reference = format!("refs/tags/{name}");
                    let peeled_reference = format!("{reference}^{{}}");
                    let object = references
                        .get(&reference)
                        .cloned()
                        .unwrap_or_else(|| "missing".into());
                    let target = references
                        .get(&peeled_reference)
                        .cloned()
                        .unwrap_or_else(|| "missing".into());
                    let object_type = if object != "missing" && target != "missing" {
                        "tag".into()
                    } else {
                        "missing".into()
                    };
                    (
                        name.to_owned(),
                        TagEvidence {
                            object_type,
                            object,
                            target,
                        },
                    )
                })
                .collect();
            RemotePublication {
                remote: "origin".into(),
                remote_url,
                canonical_remote: true,
                main_head: references
                    .get(main)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                milestone_branch_head: references
                    .get(milestone)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                completion_tag_object: references
                    .get(tag)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                completion_tag_target: references
                    .get(peeled)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                previous_tags,
                query_error: None,
            }
        }
        Ok(output) => failed_remote_publication(
            &remote_url,
            format!(
                "git ls-remote exited with {}: {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ),
        Err(error) => failed_remote_publication(
            &remote_url,
            format!("could not start git ls-remote: {error}"),
        ),
    }
}

fn git_origin_url() -> Result<String, String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace_root())
        .output()
        .map_err(|error| format!("could not start `git remote get-url origin`: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git remote get-url origin` exited with {}: {}",
            output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let url = String::from_utf8(output.stdout)
        .map_err(|error| format!("`origin` URL is not UTF-8: {error}"))?;
    let url = url.trim();
    if url.is_empty() {
        return Err("`origin` URL is empty".into());
    }
    Ok(url.to_owned())
}

fn canonical_origin_url(url: &str) -> bool {
    let normalized = url.trim().trim_end_matches('/');
    let normalized = normalized.strip_suffix(".git").unwrap_or(normalized);
    CANONICAL_ORIGIN_URLS.contains(&normalized)
}

fn failed_remote_publication(remote_url: &str, error: String) -> RemotePublication {
    RemotePublication {
        remote: "origin".into(),
        remote_url: remote_url.into(),
        canonical_remote: canonical_origin_url(remote_url),
        main_head: "missing".into(),
        milestone_branch_head: "missing".into(),
        completion_tag_object: "missing".into(),
        completion_tag_target: "missing".into(),
        previous_tags: BTreeMap::new(),
        query_error: Some(error),
    }
}

fn current_branch() -> String {
    git_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])
}

fn worktree_clean() -> bool {
    let output = Command::new("git")
        .args([
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])
        .current_dir(workspace_root())
        .stderr(Stdio::null())
        .output();
    output.is_ok_and(|output| output.status.success() && output.stdout.is_empty())
}

fn git_optional(arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace_root())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .map_or_else(|_| "missing".into(), |stdout| stdout.trim().to_owned()),
        _ => "missing".into(),
    }
}

fn apply_environment(
    command: &mut Command,
    environment: &[(&str, &Path)],
) -> Result<BTreeMap<String, String>, DynError> {
    let mut recorded = BTreeMap::new();
    for (name, path) in environment {
        let value = path
            .to_str()
            .ok_or_else(|| format!("environment path {} is not UTF-8", path.display()))?;
        command.env(name, path);
        recorded.insert((*name).to_owned(), value.to_owned());
    }
    Ok(recorded)
}

fn copy_and_capture(reader: impl Read, stdout: bool) -> std::io::Result<Vec<u8>> {
    let mut reader = std::io::BufReader::new(reader);
    let mut captured = Vec::new();
    let mut buffer = [0; 8_192];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        captured.extend_from_slice(&buffer[..count]);
        if stdout {
            std::io::stdout().write_all(&buffer[..count])?;
            std::io::stdout().flush()?;
        } else {
            std::io::stderr().write_all(&buffer[..count])?;
            std::io::stderr().flush()?;
        }
    }
    Ok(captured)
}

fn owned_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|argument| (*argument).to_owned()).collect()
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn read_json(path: &Path, label: &str) -> Result<Value, DynError> {
    let bytes = fs::read(path)
        .map_err(|error| format!("could not read {label} {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("{label} {} is not JSON: {error}", path.display()).into())
}

fn remove_stale_file(path: &Path) -> Result<(), DynError> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            format!("could not remove stale report {}: {error}", path.display())
        })?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_json_atomic(path: &Path, value: &impl Serialize, label: &str) -> Result<(), DynError> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let mut encoded = serde_json::to_vec_pretty(value)?;
    encoded.push(b'\n');
    fs::write(&temporary, encoded).map_err(|error| {
        format!(
            "could not write temporary {label} {}: {error}",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, path).map_err(|error| {
        format!(
            "could not publish {label} {} from {}: {error}",
            path.display(),
            temporary.display()
        )
        .into()
    })
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
