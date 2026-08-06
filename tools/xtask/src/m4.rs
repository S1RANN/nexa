use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::{DynError, check_through_m3, workspace_root};

const FINAL_REPORT_PATH: &str = "target/nexa-artifacts/m4-finalize/final-report.json";
const INCREMENTAL_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-incremental/incremental-report.json";
const ANALYSIS_SCALE_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/analysis-scale-report.json";
const FACADE_SCALE_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/facade-scale-report.json";
const RELOAD_STRESS_REPORT_PATH: &str =
    "target/nexa-artifacts/m4-scale-stress/reload-stress-report.json";
const LANGUAGE_SCALE_PROJECT_PATH: &str = "examples/language-scale/nexa.dev.toml";
const LANGUAGE_SCALE_LOCK_PATH: &str = "examples/language-scale/packages/app/nexa.lock";
const LANGUAGE_SCALE_ARTIFACT_PATH: &str =
    "target/nexa-artifacts/m4-tooling/example.language-scale.nxb";
const EDITOR_PACKAGE_REPORT_PATH: &str = "target/nexa-editor-support/editor-package-report.json";

const M4_GATE_NAMES: [&str; 5] = [
    "test-m4-source",
    "test-m4-semantics",
    "test-m4-incremental",
    "test-m4-tooling",
    "m4-scale-stress",
];

const REQUIRED_INCREMENTAL_SCENARIOS: [&str; 11] = [
    "private-body-change",
    "package-api-change",
    "dependency-api-change",
    "dependency-implementation-change",
    "source-add",
    "source-delete",
    "source-rename",
    "source-aba",
    "manifest-change",
    "lock-drift",
    "contract-change",
];

const REQUIRED_INCREMENTAL_ORACLE_DIGESTS: [(&str, &str); 11] = [
    (
        "private-body-change",
        "9bce3193f8b14a26652257fb37058623c05859b68b064fd6f3bd38bb1965084a",
    ),
    (
        "package-api-change",
        "453129be0abcc6000705266c6dfa6af79b8fd1f65cbe95f0383ccb4a3cee8524",
    ),
    (
        "dependency-api-change",
        "4a1009a39150f55b40341fde21a1e0fa696db82a646466ebc9a4018a0967438f",
    ),
    (
        "dependency-implementation-change",
        "313f9cef0dd4f7e601edb001e4f2ce91bd21c83eb27b52b628961582f0d941ba",
    ),
    (
        "source-add",
        "1f663ce2bfd9c1a811616ec460316097583d3269c09c1558b99796425504bfd4",
    ),
    (
        "source-delete",
        "68732cafdf7ec7380d3bcd472355de6526a17239319b83c88656a5a416986244",
    ),
    (
        "source-rename",
        "d49f27fbfc88925b78e8989aa6dd7b88278e3230da78a1eb0241cc0109494c5f",
    ),
    (
        "source-aba",
        "3463a2cb27f11f9d5a15c0ee9fca3b3a16ed63c3b14f6a6ded8af5f4d734d909",
    ),
    (
        "manifest-change",
        "9f4196d5b666bd58619e3d9481749103b9d5c9fd3c8ce5c89bdaa41cf35602f4",
    ),
    (
        "lock-drift",
        "488b64eb961533b4635ec288810fb3e5db99347b6d94da8484a2b0df39b49998",
    ),
    (
        "contract-change",
        "774a15155dc82ae77b0d9703cc96dc5cd52a68558025c7f78dc1944d1f8224eb",
    ),
];

const REQUIRED_FACADE_SCENARIOS: [&str; 10] = [
    "forward",
    "reverse",
    "random_seed_1",
    "random_seed_2",
    "cold_cache",
    "hot_cache",
    "temp_root_a",
    "temp_root_b",
    "worker_order_a",
    "worker_order_b",
];

const REQUIRED_RELOAD_CLASSES: [&str; 10] = [
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
];

const REQUIRED_RELOAD_SAFETY_COUNTERS: [&str; 7] = [
    "stale_candidate_committed",
    "active_lkg_violation",
    "duplicate_terminal",
    "missing_terminal",
    "task_request_resource_growth",
    "release_queue_not_empty",
    "worker_residual",
];

const REQUIRED_RELOAD_PIPELINE_COUNTERS: [&str; 7] = [
    "change_detected",
    "compile_queued",
    "compile_started",
    "late_results_observed",
    "host_requests_exercised",
    "candidate_superseded",
    "reload_committed",
];

const REQUIRED_RELOAD_TERMINAL_OUTCOMES: [&str; 7] = [
    "committed",
    "compile_failed",
    "verify_failed",
    "rolled_back_before_commit",
    "activation_faulted",
    "superseded",
    "host_rebuild_required",
];

const REQUIRED_RELOAD_FAILURE_EVIDENCE: [&str; 5] = [
    "syntax_parse_diagnostic",
    "typecheck_diagnostic",
    "verifier_diagnostic",
    "migration_rollback",
    "activation_fault",
];

const PREVIOUS_MILESTONE_TAGS: [(&str, &str); 8] = [
    (
        "gate1-v2.9-stop",
        "8552064ec01b3191467633717de7b77c97cb24f1",
    ),
    (
        "internal-pivot-m1-complete",
        "a44ec778f2733e1e1cc9e122823190ff131c9c70",
    ),
    (
        "internal-pivot-m1-complete-r1",
        "049b7b52891d4731af1793ab0a755f79130a03dd",
    ),
    (
        "embed-snake-m2-complete",
        "aef12a0f92a1efe8c0f0497c3cb6147cb86f0c7e",
    ),
    (
        "developer-loop-m3-complete",
        "621612f49c4180989711df3ca80021fd21ad9277",
    ),
    (
        "developer-loop-m3-complete-r1",
        "b53ce21f98db7387b37cca0572fbbf920ab53d61",
    ),
    (
        "developer-loop-m3-complete-r2",
        "71c3a3ead70533f013928b6d1c434e1870f49b24",
    ),
    (
        "developer-loop-m3-complete-r3",
        "9d31064536b5c201ffdb064fb6af8837e87edbb5",
    ),
];

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum GateStatus {
    Pass,
    Fail,
}

impl GateStatus {
    const fn from_passed(passed: bool) -> Self {
        if passed { Self::Pass } else { Self::Fail }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct M4EditorArtifactEvidence {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct M4EditorGrammarRevisions {
    nexa: String,
    nexa_contract: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct M4ZedArtifactEvidence {
    path: String,
    bytes: u64,
    sha256: String,
    target: String,
    manifest: String,
    grammar_revisions: M4EditorGrammarRevisions,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct M4EditorPackageReport {
    schema: u64,
    status: GateStatus,
    vscode: M4EditorArtifactEvidence,
    zed: M4ZedArtifactEvidence,
}

impl M4EditorPackageReport {
    fn validate(&self) -> Result<(), DynError> {
        if self.schema != 1 || self.status != GateStatus::Pass {
            return Err(format!(
                "editor package report is not schema 1 PASS: schema={}, status={:?}",
                self.schema, self.status
            )
            .into());
        }
        validate_editor_artifact(
            &self.vscode.path,
            self.vscode.bytes,
            &self.vscode.sha256,
            "target/nexa-editor-support/vscode/nexa-language-support-0.2.0.vsix",
            &[0x50, 0x4b, 0x03, 0x04],
            "VSIX",
        )?;
        if self.zed.target != "wasm32-wasip2" {
            return Err(format!(
                "Zed extension was not built for wasm32-wasip2: observed `{}`",
                self.zed.target
            )
            .into());
        }
        validate_editor_artifact(
            &self.zed.path,
            self.zed.bytes,
            &self.zed.sha256,
            "target/nexa-editor-support/zed/extension.wasm",
            &[0x00, 0x61, 0x73, 0x6d],
            "Zed WebAssembly",
        )?;
        if self.zed.manifest != "target/nexa-editor-support/zed/extension.toml" {
            return Err(format!(
                "unexpected packaged Zed manifest path `{}`",
                self.zed.manifest
            )
            .into());
        }
        let manifest_path = workspace_root().join(&self.zed.manifest);
        let manifest = fs::read_to_string(&manifest_path).map_err(|error| {
            format!(
                "could not read packaged Zed manifest {}: {error}",
                manifest_path.display()
            )
        })?;
        for (name, directory_name, revision) in [
            ("nexa", "tree-sitter-nexa", &self.zed.grammar_revisions.nexa),
            (
                "nexa_contract",
                "tree-sitter-nexa-contract",
                &self.zed.grammar_revisions.nexa_contract,
            ),
        ] {
            validate_git_revision(revision, &format!("Zed {name} grammar revision"))?;
            if !manifest.contains(&format!("rev = \"{revision}\"")) {
                return Err(format!(
                    "packaged Zed manifest does not pin {name} grammar revision `{revision}`"
                )
                .into());
            }
            let directory = workspace_root()
                .join("target/nexa-editor-support/zed")
                .join(directory_name);
            let actual_revision = git_head_in(&directory)?;
            if actual_revision != *revision {
                return Err(format!(
                    "packaged Zed {name} grammar revision mismatch: report={revision}, \
                     repository={actual_revision}"
                )
                .into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct M4CommandEvidence {
    program: String,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    exit_code: Option<i32>,
    success: bool,
    duration_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct M4GateEvidence {
    commands: Vec<M4CommandEvidence>,
    machine_reports: Vec<String>,
    failure: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4GateRecord {
    name: String,
    status: GateStatus,
    duration_ms: u64,
    tested_head: String,
    tested_branch: String,
    completed_head: String,
    completed_branch: String,
    evidence: M4GateEvidence,
}

#[derive(Default)]
struct GateContext {
    evidence: M4GateEvidence,
}

impl GateContext {
    fn run(
        &mut self,
        program: &str,
        args: &[&str],
        environment: &[(&str, &Path)],
    ) -> Result<(), DynError> {
        let rendered = render_command(program, args);
        let mut command = Command::new(program);
        command.args(args).current_dir(workspace_root());
        let mut recorded_environment = BTreeMap::new();
        for (name, path) in environment {
            let value = path
                .to_str()
                .ok_or_else(|| format!("{rendered} environment path is not UTF-8"))?;
            command.env(name, path);
            recorded_environment.insert((*name).to_owned(), value.to_owned());
        }
        let started = Instant::now();
        let status = command.status();
        let duration_ms = elapsed_millis(started);
        match status {
            Ok(status) => {
                self.evidence.commands.push(M4CommandEvidence {
                    program: program.to_owned(),
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    environment: recorded_environment,
                    exit_code: status.code(),
                    success: status.success(),
                    duration_ms,
                });
                if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "M4 gate command `{rendered}` failed with {}",
                        status
                            .code()
                            .map_or_else(|| "signal".to_owned(), |code| code.to_string())
                    )
                    .into())
                }
            }
            Err(error) => {
                self.evidence.commands.push(M4CommandEvidence {
                    program: program.to_owned(),
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    environment: recorded_environment,
                    exit_code: None,
                    success: false,
                    duration_ms,
                });
                Err(format!("M4 gate command `{rendered}` could not start: {error}").into())
            }
        }
    }

    fn run_stdout(
        &mut self,
        program: &str,
        args: &[&str],
        environment: &[(&str, &Path)],
    ) -> Result<Vec<u8>, DynError> {
        let rendered = render_command(program, args);
        let mut command = Command::new(program);
        command.args(args).current_dir(workspace_root());
        let mut recorded_environment = BTreeMap::new();
        for (name, path) in environment {
            let value = path
                .to_str()
                .ok_or_else(|| format!("{rendered} environment path is not UTF-8"))?;
            command.env(name, path);
            recorded_environment.insert((*name).to_owned(), value.to_owned());
        }
        let started = Instant::now();
        let output = command.output();
        let duration_ms = elapsed_millis(started);
        match output {
            Ok(output) => {
                self.evidence.commands.push(M4CommandEvidence {
                    program: program.to_owned(),
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    environment: recorded_environment,
                    exit_code: output.status.code(),
                    success: output.status.success(),
                    duration_ms,
                });
                if output.status.success() {
                    Ok(output.stdout)
                } else {
                    Err(format!(
                        "M4 gate command `{rendered}` failed with {}\nstdout:\n{}\nstderr:\n{}",
                        output
                            .status
                            .code()
                            .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                    .into())
                }
            }
            Err(error) => {
                self.evidence.commands.push(M4CommandEvidence {
                    program: program.to_owned(),
                    args: args.iter().map(|argument| (*argument).to_owned()).collect(),
                    environment: recorded_environment,
                    exit_code: None,
                    success: false,
                    duration_ms,
                });
                Err(format!("M4 gate command `{rendered}` could not start: {error}").into())
            }
        }
    }

    fn record_machine_report(&mut self, path: &Path) {
        self.evidence
            .machine_reports
            .push(path.display().to_string());
    }
}

struct ExecutedGate<T> {
    record: M4GateRecord,
    value: Option<T>,
}

impl<T> ExecutedGate<T> {
    fn into_result(self) -> Result<T, DynError> {
        match self.value {
            Some(value) => Ok(value),
            None => Err(self
                .record
                .evidence
                .failure
                .unwrap_or_else(|| format!("M4 gate {} failed without an error", self.record.name))
                .into()),
        }
    }
}

fn execute_gate<T>(
    name: &'static str,
    gate: impl FnOnce(&mut GateContext) -> Result<T, DynError>,
) -> ExecutedGate<T> {
    let started = Instant::now();
    let tested_head = git_optional_output(&["rev-parse", "HEAD"]);
    let tested_branch = git_optional_output(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let mut context = GateContext::default();
    let result = gate(&mut context);
    let duration_ms = elapsed_millis(started);
    let completed_head = git_optional_output(&["rev-parse", "HEAD"]);
    let completed_branch = git_optional_output(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    match result {
        Ok(value) => ExecutedGate {
            record: M4GateRecord {
                name: name.to_owned(),
                status: GateStatus::Pass,
                duration_ms,
                tested_head,
                tested_branch,
                completed_head,
                completed_branch,
                evidence: context.evidence,
            },
            value: Some(value),
        },
        Err(error) => {
            context.evidence.failure = Some(error.to_string());
            ExecutedGate {
                record: M4GateRecord {
                    name: name.to_owned(),
                    status: GateStatus::Fail,
                    duration_ms,
                    tested_head,
                    tested_branch,
                    completed_head,
                    completed_branch,
                    evidence: context.evidence,
                },
                value: None,
            }
        }
    }
}

fn run_one_gate<T>(
    name: &'static str,
    gate: impl FnOnce(&mut GateContext) -> Result<T, DynError>,
) -> Result<T, DynError> {
    let executed = execute_gate(name, gate);
    println!("{}", serde_json::to_string_pretty(&executed.record)?);
    let failures = single_gate_record_failures(&executed.record);
    if !failures.is_empty() {
        return Err(format!(
            "{name} command evidence failed its frozen gate definition:\n- {}",
            failures.join("\n- ")
        )
        .into());
    }
    executed.into_result()
}

pub(super) fn test_m4_source() -> Result<(), DynError> {
    run_one_gate("test-m4-source", source_gate)
}

pub(super) fn test_m4_semantics() -> Result<(), DynError> {
    run_one_gate("test-m4-semantics", semantics_gate)
}

pub(super) fn test_m4_incremental() -> Result<(), DynError> {
    run_one_gate("test-m4-incremental", incremental_gate).map(|_| ())
}

pub(super) fn test_m4_tooling() -> Result<(), DynError> {
    run_one_gate("test-m4-tooling", tooling_gate)
}

pub(super) fn m4_scale_stress() -> Result<(), DynError> {
    run_one_gate("m4-scale-stress", scale_stress_gate).map(|_| ())
}

pub(super) fn finalize_after_workspace() -> Result<(), DynError> {
    let tested_head = git_optional_output(&["rev-parse", "HEAD"]);
    let tested_branch = git_optional_output(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let started_clean = git_optional_output(&["status", "--porcelain"]).is_empty();
    if tested_head == "missing" || tested_branch == "missing" || !started_clean {
        return Err("M4 post-workspace evidence requires a clean attached checkout".into());
    }

    let started = Instant::now();
    let mut context = GateContext::default();
    let incremental = incremental_gate(&mut context)?;
    incremental.validate()?;

    let root = workspace_root();
    let editor_report_path = root.join(EDITOR_PACKAGE_REPORT_PATH);
    prepare_machine_report(&editor_report_path, "M4 editor package")?;
    run_language_scale_tooling(&mut context, &root)?;
    run_editor_packaging(&mut context, &editor_report_path)?;
    let (_, facade, stress) = scale_stress_gate(&mut context)?;
    facade.validate()?;
    stress.validate()?;

    let completed_head = git_optional_output(&["rev-parse", "HEAD"]);
    let completed_branch = git_optional_output(&["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let completed_clean = git_optional_output(&["status", "--porcelain"]).is_empty();
    if completed_head != tested_head || completed_branch != tested_branch || !completed_clean {
        return Err("M4 post-workspace evidence changed or dirtied the checkout".into());
    }
    let receipt = serde_json::json!({
        "schema": 1,
        "gate": "finalize-m4-post-workspace",
        "testedHead": tested_head,
        "testedBranch": tested_branch,
        "completedHead": completed_head,
        "completedBranch": completed_branch,
        "durationMs": elapsed_millis(started),
        "workspaceCoverage": [
            "test-m4-source",
            "test-m4-semantics",
            "test-m4-tooling Rust tests",
        ],
        "commands": context.evidence.commands,
        "machineReports": context.evidence.machine_reports,
        "status": "PASS",
    });
    let output = root.join("target/nexa-artifacts/m4-finalize/post-workspace-receipt.json");
    prepare_final_report(&output)?;
    let encoded = format!("{}\n", serde_json::to_string_pretty(&receipt)?);
    atomic_write_final_report(&output, encoded.as_bytes())?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}

fn source_gate(context: &mut GateContext) -> Result<(), DynError> {
    require_integration_test(
        "test-m4-source",
        "crates/nexa-analysis",
        "m4_source",
        "cargo test -p nexa-analysis --test m4_source",
    )?;
    context.run(
        "cargo",
        &["test", "-p", "nexa-analysis", "--test", "m4_source"],
        &[],
    )
}

fn semantics_gate(context: &mut GateContext) -> Result<(), DynError> {
    require_integration_test(
        "test-m4-semantics",
        "crates/nexa-analysis",
        "m4_semantics",
        "cargo test -p nexa-analysis --test m4_semantics",
    )?;
    context.run(
        "cargo",
        &["test", "-p", "nexa-analysis", "--test", "m4_semantics"],
        &[],
    )
}

fn incremental_gate(context: &mut GateContext) -> Result<M4IncrementalReport, DynError> {
    require_integration_test(
        "test-m4-incremental",
        "crates/nexa-analysis",
        "m4_incremental",
        "cargo test -p nexa-analysis --test m4_incremental",
    )?;
    let report_path = workspace_root().join(INCREMENTAL_REPORT_PATH);
    prepare_machine_report(&report_path, "M4 incremental")?;
    context.run(
        "cargo",
        &[
            "test",
            "-p",
            "nexa-analysis",
            "--test",
            "m4_incremental",
            "--",
            "--nocapture",
        ],
        &[("NEXA_M4_INCREMENTAL_REPORT", &report_path)],
    )?;
    let report = read_machine_report::<M4IncrementalReport>(
        &report_path,
        "M4 incremental gate",
        "NEXA_M4_INCREMENTAL_REPORT",
    )?;
    report.validate()?;
    context.record_machine_report(&report_path);
    Ok(report)
}

fn tooling_gate(context: &mut GateContext) -> Result<(), DynError> {
    require_integration_test(
        "test-m4-tooling",
        "crates/nexa-cli",
        "m4_tooling",
        "cargo test -p nexa-cli --test m4_tooling",
    )?;
    require_file(
        "test-m4-tooling",
        "crates/nexa-test-runner/Cargo.toml",
        "cargo test -p nexa-test-runner",
    )?;
    require_file(
        "test-m4-tooling",
        "editors/package.json",
        "pnpm --dir editors package",
    )?;
    require_file(
        "test-m4-tooling",
        "editors/scripts/build-zed.mjs",
        "pnpm --dir editors package",
    )?;
    require_file(
        "test-m4-tooling",
        "editors/scripts/verify-package.mjs",
        "pnpm --dir editors package",
    )?;
    let root = workspace_root();
    let editor_report_path = root.join(EDITOR_PACKAGE_REPORT_PATH);
    prepare_machine_report(&editor_report_path, "M4 editor package")?;
    context.run(
        "cargo",
        &["test", "-p", "nexa-cli", "--test", "m4_tooling"],
        &[],
    )?;
    run_language_scale_tooling(context, &root)?;
    context.run("cargo", &["test", "-p", "nexa-test-runner"], &[])?;
    context.run("cargo", &["test", "-p", "nexa-cli", "lsp::tests"], &[])?;
    run_editor_packaging(context, &editor_report_path)
}

fn run_language_scale_tooling(context: &mut GateContext, root: &Path) -> Result<(), DynError> {
    let language_scale_lock = root.join(LANGUAGE_SCALE_LOCK_PATH);
    let checked_lock = fs::read(&language_scale_lock).map_err(|error| {
        format!(
            "could not read checked language-scale lock {}: {error}",
            language_scale_lock.display()
        )
    })?;
    context.run(
        "cargo",
        &[
            "run",
            "-p",
            "nexa-cli",
            "--",
            "check",
            "--project",
            LANGUAGE_SCALE_PROJECT_PATH,
            "--diagnostic-format",
            "json",
        ],
        &[],
    )?;
    let artifact_path = root.join(LANGUAGE_SCALE_ARTIFACT_PATH);
    prepare_machine_report(&artifact_path, "M4 language-scale bytecode")?;
    context.run(
        "cargo",
        &[
            "run",
            "-p",
            "nexa-cli",
            "--",
            "build",
            "--project",
            LANGUAGE_SCALE_PROJECT_PATH,
            "--output",
            LANGUAGE_SCALE_ARTIFACT_PATH,
            "--diagnostic-format",
            "json",
        ],
        &[],
    )?;
    let artifact_length = fs::metadata(&artifact_path)
        .map_err(|error| {
            format!(
                "canonical language-scale build did not produce {}: {error}",
                artifact_path.display()
            )
        })?
        .len();
    if artifact_length == 0 {
        return Err(format!(
            "canonical language-scale build produced an empty artifact at {}",
            artifact_path.display()
        )
        .into());
    }
    let test_output = context.run_stdout(
        "cargo",
        &[
            "run",
            "-p",
            "nexa-cli",
            "--",
            "test",
            "--project",
            LANGUAGE_SCALE_PROJECT_PATH,
            "--diagnostic-format",
            "json",
        ],
        &[],
    )?;
    validate_language_scale_test_output(&test_output)?;
    let observed_lock = fs::read(&language_scale_lock).map_err(|error| {
        format!(
            "could not reread checked language-scale lock {}: {error}",
            language_scale_lock.display()
        )
    })?;
    if observed_lock != checked_lock {
        return Err(
            "canonical language-scale check/build/test mutated the checked nexa.lock".into(),
        );
    }
    Ok(())
}

fn run_editor_packaging(context: &mut GateContext, report_path: &Path) -> Result<(), DynError> {
    context.run("pnpm", &["--dir", "editors", "generate"], &[])?;
    context.run("pnpm", &["--dir", "editors", "check"], &[])?;
    context.run("pnpm", &["--dir", "editors", "package"], &[])?;
    let editor_report = read_machine_report::<M4EditorPackageReport>(
        report_path,
        "M4 editor package gate",
        "editor-package-report.json",
    )?;
    editor_report.validate()?;
    context.record_machine_report(report_path);
    Ok(())
}

pub(super) fn validate_editor_report_for_m4r1(report_path: &Path) -> Result<(), DynError> {
    let report = read_machine_report::<M4EditorPackageReport>(
        report_path,
        "M4R1 cached editor package gate",
        "editor-package-report.json",
    )?;
    report.validate()
}

fn validate_language_scale_test_output(bytes: &[u8]) -> Result<(), DynError> {
    let document: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("language-scale test output is not JSON: {error}"))?;
    if document.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
        || document.get("command").and_then(serde_json::Value::as_str) != Some("test")
        || document.get("status").and_then(serde_json::Value::as_str) != Some("ok")
    {
        return Err(
            format!("language-scale test output has an unexpected envelope: {document}").into(),
        );
    }
    let summary = document
        .get("summary")
        .ok_or("language-scale test output has no summary")?;
    if summary.get("total").and_then(serde_json::Value::as_u64) != Some(2)
        || summary.get("passed").and_then(serde_json::Value::as_u64) != Some(2)
        || summary.get("failed").and_then(serde_json::Value::as_u64) != Some(0)
        || summary.get("errors").and_then(serde_json::Value::as_u64) != Some(0)
    {
        return Err(format!(
            "language-scale test summary does not prove both canonical tests passed: {summary}"
        )
        .into());
    }
    let results = document
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or("language-scale test output has no result array")?;
    let observed = results
        .iter()
        .filter_map(|result| {
            Some((
                result.get("qualifiedName")?.as_str()?.to_owned(),
                result.get("status")?.as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let expected = BTreeMap::from([
        (
            "example.language-scale::test.basic.scoring::library_is_linked".to_owned(),
            "PASS".to_owned(),
        ),
        (
            "example.language-scale::test.basic.scoring::package_helper_uses_the_constant"
                .to_owned(),
            "PASS".to_owned(),
        ),
    ]);
    if results.len() != expected.len() || observed.len() != results.len() || observed != expected {
        return Err(format!(
            "language-scale canonical test result set is not exact: observed={observed:?}, \
             observed_rows={}, expected={expected:?}",
            results.len()
        )
        .into());
    }
    Ok(())
}

fn scale_stress_gate(
    context: &mut GateContext,
) -> Result<
    (
        M4AnalysisScaleReport,
        M4FacadeScaleReport,
        M4ReloadStressReport,
    ),
    DynError,
> {
    require_integration_test(
        "m4-scale-stress",
        "crates/nexa-analysis",
        "m4_scale_stress",
        "cargo test -p nexa-analysis --test m4_scale_stress m4_scale_stress -- --ignored --exact",
    )?;
    require_integration_test(
        "m4-scale-stress",
        "crates/nexa",
        "m4_package_build",
        "cargo test -p nexa --test m4_package_build facade_scale_determinism_report -- \
         --ignored --exact --nocapture",
    )?;
    require_integration_test(
        "m4-scale-stress",
        "crates/nexa-embed",
        "m4_reload_stress",
        "cargo test -p nexa-embed --test m4_reload_stress m4_reload_stress -- --ignored --exact",
    )?;
    let analysis_report_path = workspace_root().join(ANALYSIS_SCALE_REPORT_PATH);
    let facade_report_path = workspace_root().join(FACADE_SCALE_REPORT_PATH);
    let stress_report_path = workspace_root().join(RELOAD_STRESS_REPORT_PATH);
    prepare_machine_report(&analysis_report_path, "M4 analysis scale")?;
    prepare_machine_report(&facade_report_path, "M4 facade scale")?;
    prepare_machine_report(&stress_report_path, "M4 Reload stress")?;
    context.run(
        "cargo",
        &[
            "test",
            "-p",
            "nexa-analysis",
            "--test",
            "m4_scale_stress",
            "m4_scale_stress",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ],
        &[("NEXA_M4_SCALE_REPORT", &analysis_report_path)],
    )?;
    let analysis_report = read_machine_report::<M4AnalysisScaleReport>(
        &analysis_report_path,
        "M4 analysis scale gate",
        "NEXA_M4_SCALE_REPORT",
    )?;
    analysis_report.validate()?;
    context.record_machine_report(&analysis_report_path);
    context.run(
        "cargo",
        &[
            "test",
            "-p",
            "nexa",
            "--test",
            "m4_package_build",
            "facade_scale_determinism_report",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ],
        &[("NEXA_M4_FACADE_SCALE_REPORT", &facade_report_path)],
    )?;
    let facade_report = read_machine_report::<M4FacadeScaleReport>(
        &facade_report_path,
        "M4 facade scale gate",
        "NEXA_M4_FACADE_SCALE_REPORT",
    )?;
    facade_report.validate()?;
    validate_facade_report_matches_analysis(&analysis_report, &facade_report)?;
    context.record_machine_report(&facade_report_path);
    context.run(
        "cargo",
        &[
            "test",
            "-p",
            "nexa-embed",
            "--test",
            "m4_reload_stress",
            "m4_reload_stress",
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ],
        &[("NEXA_M4_RELOAD_STRESS_REPORT", &stress_report_path)],
    )?;
    let stress_report = read_machine_report::<M4ReloadStressReport>(
        &stress_report_path,
        "M4 Reload stress gate",
        "NEXA_M4_RELOAD_STRESS_REPORT",
    )?;
    stress_report.validate()?;
    context.record_machine_report(&stress_report_path);
    Ok((analysis_report, facade_report, stress_report))
}

pub(super) fn validate_scale_reports_for_m4r1(
    analysis_report_path: &Path,
    facade_report_path: &Path,
    stress_report_path: &Path,
) -> Result<(), DynError> {
    let analysis = read_machine_report::<M4AnalysisScaleReport>(
        analysis_report_path,
        "M4R1 cached analysis scale gate",
        "NEXA_M4_SCALE_REPORT",
    )?;
    analysis.validate()?;
    let facade = read_machine_report::<M4FacadeScaleReport>(
        facade_report_path,
        "M4R1 cached facade scale gate",
        "NEXA_M4_FACADE_SCALE_REPORT",
    )?;
    facade.validate()?;
    validate_facade_report_matches_analysis(&analysis, &facade)?;
    let stress = read_machine_report::<M4ReloadStressReport>(
        stress_report_path,
        "M4R1 cached Reload stress gate",
        "NEXA_M4_RELOAD_STRESS_REPORT",
    )?;
    stress.validate()
}

fn require_integration_test(
    gate: &str,
    package_directory: &str,
    target: &str,
    expected_command: &str,
) -> Result<(), DynError> {
    let root = workspace_root();
    let flat = root
        .join(package_directory)
        .join("tests")
        .join(format!("{target}.rs"));
    let directory = root
        .join(package_directory)
        .join("tests")
        .join(target)
        .join("main.rs");
    if flat.is_file() || directory.is_file() {
        Ok(())
    } else {
        Err(format!(
            "M4 gate `{gate}` is unavailable: missing named integration test target `{target}` \
             (expected {} or {}; command `{expected_command}`)",
            flat.display(),
            directory.display()
        )
        .into())
    }
}

fn require_file(gate: &str, relative: &str, expected_command: &str) -> Result<(), DynError> {
    let path = workspace_root().join(relative);
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "M4 gate `{gate}` is unavailable: missing `{relative}` required by \
             `{expected_command}`"
        )
        .into())
    }
}

fn prepare_machine_report(path: &Path, label: &str) -> Result<(), DynError> {
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            format!(
                "could not remove stale {label} report {}: {error}",
                path.display()
            )
        })?;
    }
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| format!("{label} report path has no parent: {}", path.display()))?,
    )
    .map_err(|error| format!("could not create {label} report directory: {error}"))?;
    Ok(())
}

fn read_machine_report<T: DeserializeOwned>(
    path: &Path,
    gate: &str,
    environment_variable: &str,
) -> Result<T, DynError> {
    if !path.is_file() {
        return Err(format!(
            "{gate} passed its command but did not produce `{environment_variable}` at {}",
            path.display()
        )
        .into());
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("could not read {gate} report {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "{gate} report {} is not valid typed JSON: {error}",
            path.display()
        )
        .into()
    })
}

fn validate_editor_artifact(
    reported_path: &str,
    reported_bytes: u64,
    reported_sha256: &str,
    expected_path: &str,
    magic: &[u8],
    label: &str,
) -> Result<(), DynError> {
    if reported_path != expected_path {
        return Err(format!(
            "{label} report path is `{reported_path}`; expected `{expected_path}`"
        )
        .into());
    }
    let path = workspace_root().join(expected_path);
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "could not read packaged {label} {}: {error}",
            path.display()
        )
    })?;
    if bytes.len() < 1024 || !bytes.starts_with(magic) {
        return Err(format!(
            "packaged {label} {} is empty, truncated, or has the wrong binary signature",
            path.display()
        )
        .into());
    }
    if reported_bytes != bytes.len() as u64 {
        return Err(format!(
            "packaged {label} size does not match its report: report={reported_bytes}, \
             observed={}",
            bytes.len()
        )
        .into());
    }
    validate_sha256(reported_sha256, &format!("{label} SHA-256"))?;
    let actual_sha256 = sha256_with_node(&path)?;
    if actual_sha256 != reported_sha256 {
        return Err(format!(
            "packaged {label} SHA-256 does not match its report: \
             report={reported_sha256}, observed={actual_sha256}"
        )
        .into());
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<(), DynError> {
    validate_digest_32(value, label)
}

fn validate_digest_32(value: &str, label: &str) -> Result<(), DynError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("{label} is not a lowercase 64-digit hexadecimal digest").into());
    }
    Ok(())
}

fn sha256_with_node(path: &Path) -> Result<String, DynError> {
    let script = "const fs=require('node:fs'),c=require('node:crypto');\
                  process.stdout.write(c.createHash('sha256')\
                  .update(fs.readFileSync(process.argv[1])).digest('hex'));";
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(path)
        .current_dir(workspace_root())
        .output()
        .map_err(|error| {
            format!(
                "could not start Node.js to hash {}: {error}",
                path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "Node.js could not hash {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| format!("Node.js returned a non-UTF-8 SHA-256 digest: {error}").into())
}

fn validate_git_revision(value: &str, label: &str) -> Result<(), DynError> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("{label} is not a lowercase 40-digit SHA-1 revision").into());
    }
    Ok(())
}

fn git_head_in(directory: &Path) -> Result<String, DynError> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(directory)
        .output()
        .map_err(|error| {
            format!(
                "could not inspect packaged grammar repository {}: {error}",
                directory.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "could not resolve packaged grammar HEAD in {}: {}",
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|error| {
            format!(
                "packaged grammar HEAD in {} is not UTF-8: {error}",
                directory.display()
            )
            .into()
        })
}

fn render_command(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4ScaleCounters {
    modules: u64,
    symbols: u64,
    import_edges: u64,
    packages: u64,
}

impl M4ScaleCounters {
    fn validate(&self) -> Result<(), DynError> {
        if self.modules < 100 {
            return Err(format!(
                "M4 scale evidence has {} Modules; expected at least 100",
                self.modules
            )
            .into());
        }
        if self.symbols < 1_000 {
            return Err(format!(
                "M4 scale evidence has {} Symbols; expected at least 1000",
                self.symbols
            )
            .into());
        }
        if self.import_edges < 500 {
            return Err(format!(
                "M4 scale evidence has {} Import Edges; expected at least 500",
                self.import_edges
            )
            .into());
        }
        if self.packages < 20 {
            return Err(format!(
                "M4 scale evidence has {} Packages; expected at least 20",
                self.packages
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4FacadeScaleCounters {
    modules: u64,
    symbols: u64,
    package_modules: u64,
    package_symbols: u64,
    import_edges: u64,
    packages: u64,
}

impl M4FacadeScaleCounters {
    fn validate(&self) -> Result<(), DynError> {
        M4ScaleCounters {
            modules: self.modules,
            symbols: self.symbols,
            import_edges: self.import_edges,
            packages: self.packages,
        }
        .validate()?;
        if self.package_modules < 100 || self.package_modules > self.modules {
            return Err(format!(
                "M4 facade scale has {} package Modules out of {} total Modules; expected \
                 100..=total",
                self.package_modules, self.modules
            )
            .into());
        }
        if self.package_symbols < 1_000 || self.package_symbols > self.symbols {
            return Err(format!(
                "M4 facade scale has {} package Symbols out of {} total Symbols; expected \
                 1000..=total",
                self.package_symbols, self.symbols
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4EqualityEvidence {
    first: String,
    second: String,
}

impl M4EqualityEvidence {
    fn validate(&self, label: &str) -> Result<(), DynError> {
        validate_digest_32(&self.first, &format!("M4 {label} first digest"))?;
        validate_digest_32(&self.second, &format!("M4 {label} second digest"))?;
        if self.first != self.second {
            return Err(format!(
                "M4 {label} determinism mismatch: first=`{}`, second=`{}`",
                self.first, self.second
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4AnalysisDeterminismEvidence {
    fingerprint: M4EqualityEvidence,
    lockfile: M4EqualityEvidence,
    analysis_graph: M4EqualityEvidence,
    analysis_diagnostics: M4EqualityEvidence,
    query_cold_hot: M4EqualityEvidence,
    hot_cache_hits: u64,
    temporary_root: M4TemporaryRootEvidence,
    worker_order: M4WorkerOrderEvidence,
}

impl M4AnalysisDeterminismEvidence {
    fn validate(&self) -> Result<(), DynError> {
        self.fingerprint.validate("fingerprint")?;
        self.lockfile.validate("lockfile")?;
        self.analysis_graph.validate("analysis graph")?;
        self.analysis_diagnostics.validate("analysis diagnostics")?;
        self.query_cold_hot.validate("cold/hot analysis query")?;
        if self.hot_cache_hits == 0 {
            return Err("M4 hot analysis query reported zero persistent cache hits".into());
        }
        self.temporary_root.validate()?;
        self.worker_order.validate()
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4TemporaryRootEvidence {
    first: String,
    second: String,
    mechanism: String,
    first_root_digest: String,
    second_root_digest: String,
    first_packages: usize,
    second_packages: usize,
    first_modules: usize,
    second_modules: usize,
}

impl M4TemporaryRootEvidence {
    fn validate(&self) -> Result<(), DynError> {
        M4EqualityEvidence {
            first: self.first.clone(),
            second: self.second.clone(),
        }
        .validate("temporary-root analysis")?;
        validate_digest_32(&self.first_root_digest, "M4 first temporary-root identity")?;
        validate_digest_32(
            &self.second_root_digest,
            "M4 second temporary-root identity",
        )?;
        if self.mechanism != "filesystem-directory-loader-full-closure-analysis"
            || self.first_root_digest == self.second_root_digest
            || self.first_packages < 20
            || self.second_packages < 20
            || self.first_modules < 100
            || self.second_modules < 100
        {
            return Err(format!(
                "M4 temporary-root analysis lacks two distinct full-closure directory-loader \
                 executions: {self:?}"
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4WorkerOrderEvidence {
    first: String,
    second: String,
    mechanism: String,
    first_completion_order: Vec<u64>,
    second_completion_order: Vec<u64>,
    builds_per_order: usize,
    first_max_in_flight: u64,
    second_max_in_flight: u64,
}

impl M4WorkerOrderEvidence {
    fn validate(&self) -> Result<(), DynError> {
        M4EqualityEvidence {
            first: self.first.clone(),
            second: self.second.clone(),
        }
        .validate("analysis Worker order")?;
        if self.mechanism != "concurrent-thread-analysis-controlled-completion"
            || self.builds_per_order < 2
            || self.first_completion_order.len() != self.builds_per_order
            || self.second_completion_order.len() != self.builds_per_order
            || self.first_completion_order == self.second_completion_order
            || self.first_max_in_flight < 2
            || self.second_max_in_flight < 2
        {
            return Err(format!(
                "M4 analysis Worker evidence lacks two real completion orders: {self:?}"
            )
            .into());
        }
        for (label, order) in [
            ("first", &self.first_completion_order),
            ("second", &self.second_completion_order),
        ] {
            let unique = order.iter().copied().collect::<BTreeSet<_>>();
            if unique.len() != order.len() {
                return Err(format!(
                    "M4 analysis Worker {label} completion order repeats worker IDs"
                )
                .into());
            }
        }
        let first = self
            .first_completion_order
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let second = self
            .second_completion_order
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if first != second {
            return Err(
                "M4 analysis Worker schedules did not use the same exact worker set".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4AnalysisScaleReport {
    schema: u32,
    scale: M4ScaleCounters,
    closure_identity: String,
    determinism: M4AnalysisDeterminismEvidence,
    status: String,
}

impl M4AnalysisScaleReport {
    fn validate(&self) -> Result<(), DynError> {
        if self.schema != 3 {
            return Err(format!(
                "M4 analysis scale report schema is {}; expected exactly 3",
                self.schema
            )
            .into());
        }
        validate_digest_32(&self.closure_identity, "M4 analysis closure identity")?;
        self.scale.validate()?;
        self.determinism.validate()?;
        if self.determinism.hot_cache_hits < self.scale.modules {
            return Err(format!(
                "M4 hot analysis reused {} queries; expected at least one per {}-module closure",
                self.determinism.hot_cache_hits, self.scale.modules
            )
            .into());
        }
        if self.status != "PASS" {
            return Err(format!(
                "M4 analysis scale report status is `{}`; expected `PASS`",
                self.status
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4CompiledScenarioEvidence {
    artifact_bytes_digest: String,
    diagnostic_ndjson_digest: String,
    diagnostic_records: u64,
    source_fingerprint: String,
    public_api_fingerprint: String,
    state_schema_fingerprint: String,
    build_fingerprint: String,
    linked_state_fingerprint: String,
    closure_identity: String,
    lock_digest: String,
    compiled_package_ids: Vec<String>,
    compiled_module_ids: Vec<String>,
    mechanism: String,
    filesystem_root_digest: Option<String>,
    loaded_package_directories: u64,
    loaded_package_ids: Vec<String>,
    worker_completion_order: Vec<u64>,
    max_in_flight: u64,
}

impl M4CompiledScenarioEvidence {
    fn validate(&self, scenario: &str) -> Result<(), DynError> {
        for (field, value) in [
            ("artifact_bytes_digest", self.artifact_bytes_digest.as_str()),
            (
                "diagnostic_ndjson_digest",
                self.diagnostic_ndjson_digest.as_str(),
            ),
            ("source_fingerprint", self.source_fingerprint.as_str()),
            (
                "public_api_fingerprint",
                self.public_api_fingerprint.as_str(),
            ),
            (
                "state_schema_fingerprint",
                self.state_schema_fingerprint.as_str(),
            ),
            ("build_fingerprint", self.build_fingerprint.as_str()),
            (
                "linked_state_fingerprint",
                self.linked_state_fingerprint.as_str(),
            ),
            ("closure_identity", self.closure_identity.as_str()),
            ("lock_digest", self.lock_digest.as_str()),
        ] {
            validate_digest_32(
                value,
                &format!("M4 compiled scenario `{scenario}` `{field}`"),
            )?;
        }
        if let Some(root) = &self.filesystem_root_digest {
            validate_digest_32(
                root,
                &format!("M4 compiled scenario `{scenario}` filesystem root"),
            )?;
        }
        if self.diagnostic_records == 0 {
            return Err(format!(
                "M4 compiled scenario `{scenario}` has no independently rendered diagnostic \
                 records"
            )
            .into());
        }
        validate_identity_list(
            &format!("M4 compiled scenario `{scenario}` package"),
            &self.compiled_package_ids,
        )?;
        validate_identity_list(
            &format!("M4 compiled scenario `{scenario}` module"),
            &self.compiled_module_ids,
        )?;
        if self.compiled_package_ids.len() < 20 || self.compiled_module_ids.len() < 100 {
            return Err(format!(
                "M4 compiled scenario `{scenario}` did not enumerate the full compiled closure: \
                 packages={}, modules={}",
                self.compiled_package_ids.len(),
                self.compiled_module_ids.len()
            )
            .into());
        }
        Ok(())
    }

    fn deterministic_payload_eq(&self, other: &Self) -> bool {
        self.artifact_bytes_digest == other.artifact_bytes_digest
            && self.diagnostic_ndjson_digest == other.diagnostic_ndjson_digest
            && self.diagnostic_records == other.diagnostic_records
            && self.source_fingerprint == other.source_fingerprint
            && self.public_api_fingerprint == other.public_api_fingerprint
            && self.state_schema_fingerprint == other.state_schema_fingerprint
            && self.build_fingerprint == other.build_fingerprint
            && self.linked_state_fingerprint == other.linked_state_fingerprint
            && self.closure_identity == other.closure_identity
            && self.lock_digest == other.lock_digest
            && self.compiled_package_ids == other.compiled_package_ids
            && self.compiled_module_ids == other.compiled_module_ids
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4FacadePipelineEvidence {
    analyzer_runs: u64,
    invalid_analyzer_runs: u64,
    successful_check_analyzer_runs: u64,
    compile_analyzer_runs: u64,
    typed_compiler_runs: u64,
    verifier_runs: u64,
    module_encode_runs: u64,
    module_bytes_length: u64,
}

impl M4FacadePipelineEvidence {
    fn validate(&self) -> Result<(), DynError> {
        let expected = Self {
            analyzer_runs: 42,
            invalid_analyzer_runs: 10,
            successful_check_analyzer_runs: 16,
            compile_analyzer_runs: 16,
            typed_compiler_runs: 16,
            verifier_runs: 16,
            module_encode_runs: 16,
            module_bytes_length: self.module_bytes_length,
        };
        if self.analyzer_runs != expected.analyzer_runs
            || self.invalid_analyzer_runs != expected.invalid_analyzer_runs
            || self.successful_check_analyzer_runs != expected.successful_check_analyzer_runs
            || self.compile_analyzer_runs != expected.compile_analyzer_runs
            || self.typed_compiler_runs != expected.typed_compiler_runs
            || self.verifier_runs != expected.verifier_runs
            || self.module_encode_runs != expected.module_encode_runs
        {
            return Err(format!(
                "M4 facade pipeline does not match the exact 10-scenario execution matrix: \
                 observed={self:?}, expected={expected:?}"
            )
            .into());
        }
        if self.module_bytes_length == 0 {
            return Err("M4 facade pipeline encoded zero Module bytes".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4CanonicalDiagnosticEvidence {
    format: String,
    scenario_runs: u64,
    records: u64,
}

impl M4CanonicalDiagnosticEvidence {
    fn validate(&self) -> Result<(), DynError> {
        if self.format != "canonical-ndjson" {
            return Err(format!(
                "M4 facade diagnostics format is `{}`; expected `canonical-ndjson`",
                self.format
            )
            .into());
        }
        if self.records == 0 {
            return Err("M4 facade diagnostics contain no canonical NDJSON records".into());
        }
        if self.scenario_runs != 10 {
            return Err(format!(
                "M4 facade diagnostics cover {} scenario runs; expected exactly 10",
                self.scenario_runs
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4QueryRunEvidence {
    revision: u64,
    parsed_sources: u64,
    analyzed_modules: u64,
    reused_queries: u64,
    invalidated_queries: u64,
    cumulative_hits: u64,
    cumulative_misses: u64,
    cumulative_writes: u64,
    cumulative_invalidations: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4QueryCacheEvidence {
    cold: M4QueryRunEvidence,
    hot: M4QueryRunEvidence,
}

impl M4QueryCacheEvidence {
    fn validate(&self, scale: &M4FacadeScaleCounters) -> Result<(), DynError> {
        if self.cold.parsed_sources != scale.modules || self.cold.analyzed_modules != scale.modules
        {
            return Err(format!(
                "M4 facade cold query run did not parse/analyze the exact {}-module closure: {:?}",
                scale.modules, self.cold
            )
            .into());
        }
        if self.hot.parsed_sources != 0 || self.hot.analyzed_modules != 0 {
            return Err(format!(
                "M4 facade hot query run repeated parse/analysis work: {:?}",
                self.hot
            )
            .into());
        }
        if self.cold.reused_queries != 0
            || self.cold.invalidated_queries != 0
            || self.hot.invalidated_queries != 0
            || self.hot.reused_queries < scale.modules
            || self.hot.cumulative_hits <= self.cold.cumulative_hits
            || self.hot.revision != self.cold.revision
            || self.hot.cumulative_misses != self.cold.cumulative_misses
            || self.hot.cumulative_writes < self.cold.cumulative_writes
            || self.hot.cumulative_invalidations != self.cold.cumulative_invalidations
        {
            return Err(format!(
                "M4 facade cold/hot QueryDatabase evidence is not an unchanged persistent-cache \
                 pair: cold={:?}, hot={:?}",
                self.cold, self.hot
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4FacadeScaleReport {
    schema: u32,
    status: String,
    closure_identity: String,
    scale: M4FacadeScaleCounters,
    pipeline: M4FacadePipelineEvidence,
    diagnostics: M4CanonicalDiagnosticEvidence,
    query_cache: M4QueryCacheEvidence,
    scenarios: BTreeMap<String, M4CompiledScenarioEvidence>,
}

impl M4FacadeScaleReport {
    fn validate(&self) -> Result<(), DynError> {
        if self.schema != 3 {
            return Err(format!(
                "M4 facade scale report schema is {}; expected exactly 3",
                self.schema
            )
            .into());
        }
        validate_digest_32(&self.closure_identity, "M4 facade closure identity")?;
        if self.status != "PASS" {
            return Err(format!(
                "M4 facade scale report status is `{}`; expected `PASS`",
                self.status
            )
            .into());
        }
        self.scale.validate()?;
        self.pipeline.validate()?;
        self.diagnostics.validate()?;
        self.query_cache.validate(&self.scale)?;
        validate_exact_map_keys(
            "M4 facade compiled scenario",
            &self.scenarios,
            &REQUIRED_FACADE_SCENARIOS,
        )?;
        let forward = self
            .scenarios
            .get("forward")
            .ok_or("M4 facade report omitted forward scenario")?;
        forward.validate("forward")?;
        if u64::try_from(forward.compiled_package_ids.len())? != self.scale.packages
            || u64::try_from(forward.compiled_module_ids.len())? != self.scale.package_modules
        {
            return Err(format!(
                "M4 facade compiled identity sets do not match scale counters: packages={}, \
                 modules={}, scale={:?}",
                forward.compiled_package_ids.len(),
                forward.compiled_module_ids.len(),
                self.scale
            )
            .into());
        }
        for (name, scenario) in &self.scenarios {
            scenario.validate(name)?;
            if scenario.closure_identity != self.closure_identity {
                return Err(format!(
                    "M4 facade scenario `{name}` closure identity `{}` differs from report `{}`",
                    scenario.closure_identity, self.closure_identity
                )
                .into());
            }
            if !scenario.deterministic_payload_eq(forward) {
                return Err(format!(
                    "M4 facade compiled scenario `{name}` is not byte/fingerprint/diagnostic \
                     identical to `forward`: forward={forward:?}, observed={scenario:?}"
                )
                .into());
            }
        }
        let diagnostic_records = self
            .scenarios
            .values()
            .try_fold(0_u64, |total, scenario| {
                total.checked_add(scenario.diagnostic_records)
            })
            .ok_or("M4 facade diagnostic record evidence overflowed u64")?;
        if self.diagnostics.scenario_runs != u64::try_from(self.scenarios.len())?
            || self.diagnostics.records != diagnostic_records
        {
            return Err(format!(
                "M4 facade diagnostic totals do not match independent scenarios: totals={:?}, \
                 scenario_count={}, scenario_records={diagnostic_records}",
                self.diagnostics,
                self.scenarios.len()
            )
            .into());
        }
        self.validate_execution_mechanisms()?;
        Ok(())
    }

    fn validate_execution_mechanisms(&self) -> Result<(), DynError> {
        for name in ["forward", "reverse", "random_seed_1", "random_seed_2"] {
            let scenario = &self.scenarios[name];
            if scenario.mechanism != "direct-session"
                || scenario.filesystem_root_digest.is_some()
                || scenario.loaded_package_directories != 0
                || !scenario.loaded_package_ids.is_empty()
                || !scenario.worker_completion_order.is_empty()
                || scenario.max_in_flight != 1
            {
                return Err(format!(
                    "M4 facade scenario `{name}` is not exact direct-session evidence: \
                     {scenario:?}"
                )
                .into());
            }
        }
        for name in ["cold_cache", "hot_cache"] {
            let scenario = &self.scenarios[name];
            if scenario.mechanism != "persistent-query-database"
                || scenario.filesystem_root_digest.is_some()
                || scenario.loaded_package_directories != 0
                || !scenario.loaded_package_ids.is_empty()
                || !scenario.worker_completion_order.is_empty()
                || scenario.max_in_flight != 1
            {
                return Err(format!(
                    "M4 facade scenario `{name}` is not exact persistent-query evidence: \
                     {scenario:?}"
                )
                .into());
            }
        }
        let temp_a = &self.scenarios["temp_root_a"];
        let temp_b = &self.scenarios["temp_root_b"];
        for (name, scenario) in [("temp_root_a", temp_a), ("temp_root_b", temp_b)] {
            if scenario.mechanism != "filesystem-directory-loader"
                || scenario.loaded_package_directories
                    != u64::try_from(scenario.compiled_package_ids.len())?
                || scenario.loaded_package_ids != scenario.compiled_package_ids
                || scenario.filesystem_root_digest.is_none()
                || !scenario.worker_completion_order.is_empty()
                || scenario.max_in_flight != 1
            {
                return Err(format!(
                    "M4 facade scenario `{name}` lacks real filesystem directory-loader \
                     evidence: {scenario:?}"
                )
                .into());
            }
        }
        if temp_a.filesystem_root_digest == temp_b.filesystem_root_digest {
            return Err("M4 facade temporary-root scenarios used the same filesystem root".into());
        }

        let worker_a = &self.scenarios["worker_order_a"];
        let worker_b = &self.scenarios["worker_order_b"];
        for (name, scenario) in [("worker_order_a", worker_a), ("worker_order_b", worker_b)] {
            if scenario.mechanism != "thread-dispatch-and-completion"
                || scenario.worker_completion_order.len() != 4
                || scenario.max_in_flight < 2
                || scenario.filesystem_root_digest.is_some()
                || scenario.loaded_package_directories != 0
                || !scenario.loaded_package_ids.is_empty()
            {
                return Err(format!(
                    "M4 facade scenario `{name}` lacks actual worker completion evidence: \
                     {scenario:?}"
                )
                .into());
            }
            let expected_workers = [0_u64, 1, 2, 3].into_iter().collect::<BTreeSet<_>>();
            let unique = scenario
                .worker_completion_order
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if unique != expected_workers {
                return Err(format!(
                    "M4 facade scenario `{name}` completion order does not contain the exact \
                     worker set 0..4: {:?}",
                    scenario.worker_completion_order
                )
                .into());
            }
        }
        if worker_a.worker_completion_order == worker_b.worker_completion_order {
            return Err("M4 facade Worker scenarios have the same completion order".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4InvalidationSets {
    parsed: Vec<String>,
    analyzed: Vec<String>,
    reused: Vec<String>,
    invalidated: Vec<String>,
}

impl M4InvalidationSets {
    fn validate_canonical(&self, scenario: &str, kind: &str) -> Result<(), DynError> {
        validate_sorted_unique(scenario, kind, "parsed", &self.parsed)?;
        validate_sorted_unique(scenario, kind, "analyzed", &self.analyzed)?;
        validate_sorted_unique(scenario, kind, "reused", &self.reused)?;
        validate_sorted_unique(scenario, kind, "invalidated", &self.invalidated)
    }

    fn is_empty(&self) -> bool {
        self.parsed.is_empty()
            && self.analyzed.is_empty()
            && self.reused.is_empty()
            && self.invalidated.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4InvalidationScenario {
    name: String,
    expected: M4InvalidationSets,
    observed: M4InvalidationSets,
    hot_cache_hits: u64,
}

impl M4InvalidationScenario {
    fn validate(&self) -> Result<(), DynError> {
        if self.name.is_empty() {
            return Err("M4 incremental report contains an empty scenario name".into());
        }
        self.expected.validate_canonical(&self.name, "expected")?;
        self.observed.validate_canonical(&self.name, "observed")?;
        let required_digest = REQUIRED_INCREMENTAL_ORACLE_DIGESTS
            .iter()
            .find_map(|(name, digest)| (*name == self.name.as_str()).then_some(*digest))
            .ok_or_else(|| {
                format!(
                    "M4 incremental scenario `{}` has no frozen xtask oracle",
                    self.name
                )
            })?;
        for (kind, sets) in [("expected", &self.expected), ("observed", &self.observed)] {
            let actual_digest = incremental_oracle_digest(&self.name, sets);
            if actual_digest != required_digest {
                return Err(format!(
                    "M4 incremental scenario `{}` {kind} sets do not match the frozen xtask \
                     oracle: expected={required_digest}, observed={actual_digest}",
                    self.name
                )
                .into());
            }
        }
        if self.observed.is_empty() {
            return Err(format!(
                "M4 incremental scenario `{}` has no real invalidation evidence",
                self.name
            )
            .into());
        }
        if self.hot_cache_hits == 0 || self.observed.reused.is_empty() {
            return Err(format!(
                "M4 incremental scenario `{}` has no real hot-query reuse",
                self.name
            )
            .into());
        }
        if self.observed != self.expected {
            return Err(format!(
                "M4 incremental scenario `{}` did not produce its exact invalidation sets: \
                 expected={:?}, observed={:?}",
                self.name, self.expected, self.observed
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4IncrementalEvidence {
    scenarios: Vec<M4InvalidationScenario>,
}

impl M4IncrementalEvidence {
    fn validate(&self) -> Result<(), DynError> {
        let required = REQUIRED_INCREMENTAL_SCENARIOS
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut observed = BTreeSet::new();
        for scenario in &self.scenarios {
            if !observed.insert(scenario.name.as_str()) {
                return Err(format!(
                    "M4 incremental report contains duplicate scenario `{}`",
                    scenario.name
                )
                .into());
            }
            scenario.validate()?;
        }
        if observed != required {
            let missing = required.difference(&observed).copied().collect::<Vec<_>>();
            let unexpected = observed.difference(&required).copied().collect::<Vec<_>>();
            return Err(format!(
                "M4 incremental report does not contain the exact 11-scenario matrix: \
                 missing={missing:?}, unexpected={unexpected:?}"
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4IncrementalReport {
    schema: u32,
    scenarios: Vec<M4InvalidationScenario>,
    status: String,
}

impl M4IncrementalReport {
    fn evidence(&self) -> M4IncrementalEvidence {
        M4IncrementalEvidence {
            scenarios: self.scenarios.clone(),
        }
    }

    fn validate(&self) -> Result<(), DynError> {
        if self.schema != 2 {
            return Err(format!(
                "M4 incremental report schema is {}; expected exactly 2",
                self.schema
            )
            .into());
        }
        self.evidence().validate()?;
        if self.status != "PASS" {
            return Err(format!(
                "M4 incremental report status is `{}`; expected `PASS`",
                self.status
            )
            .into());
        }
        Ok(())
    }
}

fn incremental_oracle_digest(name: &str, sets: &M4InvalidationSets) -> String {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hashes = [
        0xcbf2_9ce4_8422_2325,
        0x8422_2325_cbf2_9ce4,
        0x9e37_79b9_7f4a_7c15,
        0x6a09_e667_f3bc_c909,
    ];
    let mut update = |value: &str| {
        for byte in value.bytes() {
            for hash in &mut hashes {
                *hash = (*hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
            }
        }
    };
    update(name);
    update("\0");
    for (field, values) in [
        ("parsed", &sets.parsed),
        ("analyzed", &sets.analyzed),
        ("reused", &sets.reused),
        ("invalidated", &sets.invalidated),
    ] {
        update(field);
        update("\0");
        for value in values {
            update(value);
            update("\0");
        }
        update("\u{ff}");
    }
    let mut digest = String::with_capacity(64);
    for hash in hashes {
        write!(digest, "{hash:016x}").expect("writing a digest to String cannot fail");
    }
    digest
}

fn validate_sorted_unique(
    scenario: &str,
    kind: &str,
    field: &str,
    values: &[String],
) -> Result<(), DynError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        return Err(format!(
            "M4 incremental scenario `{scenario}` has an empty or NUL-containing \
             {kind}.{field} identity"
        )
        .into());
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!(
            "M4 incremental scenario `{scenario}` {kind}.{field} set is not strictly sorted and \
             unique: {values:?}"
        )
        .into());
    }
    Ok(())
}

fn validate_identity_list(label: &str, values: &[String]) -> Result<(), DynError> {
    if values
        .iter()
        .any(|value| value.is_empty() || value.contains('\0'))
    {
        return Err(format!("{label} identity list contains an empty or NUL identity").into());
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(format!("{label} identity list is not strictly sorted and unique").into());
    }
    Ok(())
}

fn validate_exact_map_keys<T>(
    label: &str,
    observed: &BTreeMap<String, T>,
    required: &[&str],
) -> Result<(), DynError> {
    let expected = required.iter().copied().collect::<BTreeSet<_>>();
    let actual = observed.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if actual == expected {
        Ok(())
    } else {
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
        Err(
            format!("{label} keys are not exact: missing={missing:?}, unexpected={unexpected:?}")
                .into(),
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4ReloadStressReport {
    schema: u32,
    status: String,
    classes: BTreeMap<String, u64>,
    activation_fault_recovery: u64,
    development_pipeline: BTreeMap<String, u64>,
    terminal_outcomes: BTreeMap<String, u64>,
    failure_evidence: BTreeMap<String, u64>,
    safety: BTreeMap<String, u64>,
}

impl M4ReloadStressReport {
    fn validate(&self) -> Result<(), DynError> {
        if self.schema != 2 {
            return Err(format!(
                "M4 Reload stress report schema is {}; expected exactly 2",
                self.schema
            )
            .into());
        }
        if self.status != "PASS" {
            return Err(format!(
                "M4 Reload stress report status is `{}`; expected `PASS`",
                self.status
            )
            .into());
        }
        validate_exact_map_keys(
            "M4 Reload stress class",
            &self.classes,
            &REQUIRED_RELOAD_CLASSES,
        )?;
        for (class, count) in &self.classes {
            if *count < 100 {
                return Err(format!(
                    "M4 Reload stress class `{class}` ran {count} cases; expected at least 100"
                )
                .into());
            }
        }
        if self.activation_fault_recovery < 10 {
            return Err(format!(
                "M4 Reload activation fault recovery ran {} cases; expected at least 10",
                self.activation_fault_recovery
            )
            .into());
        }
        validate_exact_map_keys(
            "M4 Reload stress Development pipeline counter",
            &self.development_pipeline,
            &REQUIRED_RELOAD_PIPELINE_COUNTERS,
        )?;
        validate_exact_map_keys(
            "M4 Reload stress terminal outcome",
            &self.terminal_outcomes,
            &REQUIRED_RELOAD_TERMINAL_OUTCOMES,
        )?;
        validate_exact_map_keys(
            "M4 Reload stress failure evidence",
            &self.failure_evidence,
            &REQUIRED_RELOAD_FAILURE_EVIDENCE,
        )?;
        self.validate_pipeline_evidence()?;
        validate_exact_map_keys(
            "M4 Reload stress safety counter",
            &self.safety,
            &REQUIRED_RELOAD_SAFETY_COUNTERS,
        )?;
        let violations = self
            .safety
            .iter()
            .filter(|(_, value)| **value != 0)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>();
        if !violations.is_empty() {
            return Err(format!(
                "M4 Reload stress safety counters must all be zero: {}",
                violations.join(", ")
            )
            .into());
        }
        Ok(())
    }

    fn validate_pipeline_evidence(&self) -> Result<(), DynError> {
        let expected_generations = self.expected_generation_count()?;
        self.validate_generation_pipeline(expected_generations)?;
        self.validate_terminal_evidence(expected_generations)?;
        self.validate_failure_evidence()
    }

    fn expected_generation_count(&self) -> Result<u64, DynError> {
        let class_total = checked_counter_sum(
            "M4 Reload stress class total",
            self.classes.values().copied(),
        )?;
        let activation_generations = self
            .activation_fault_recovery
            .checked_mul(2)
            .ok_or("M4 Reload activation generation evidence overflowed u64")?;
        checked_counter_sum(
            "M4 Reload stress generation total",
            [
                class_total,
                self.classes["add_module"],
                self.classes["delete_module"],
                self.classes["rename_module"],
                activation_generations,
            ],
        )
    }

    fn validate_generation_pipeline(&self, expected_generations: u64) -> Result<(), DynError> {
        for counter in [
            "change_detected",
            "compile_queued",
            "compile_started",
            "late_results_observed",
        ] {
            if self.development_pipeline[counter] != expected_generations {
                return Err(format!(
                    "M4 Reload stress `{counter}`={} does not account for all \
                     {expected_generations} real generations",
                    self.development_pipeline[counter]
                )
                .into());
            }
        }
        if self.development_pipeline["host_requests_exercised"] == 0 {
            return Err(
                "M4 Reload stress resource safety has no real Host request lifecycle".into(),
            );
        }
        Ok(())
    }

    fn validate_terminal_evidence(&self, expected_generations: u64) -> Result<(), DynError> {
        let expected_superseded = checked_counter_sum(
            "M4 Reload stress superseded total",
            [
                self.classes["add_module"],
                self.classes["delete_module"],
                self.classes["rename_module"],
                self.classes["aba_change"],
            ],
        )?;
        if self.development_pipeline["candidate_superseded"] != expected_superseded
            || self.terminal_outcomes["superseded"] != expected_superseded
        {
            return Err(format!(
                "M4 Reload stress stale-candidate evidence is inconsistent: \
                 expected={expected_superseded}, pipeline={}, terminal={}",
                self.development_pipeline["candidate_superseded"],
                self.terminal_outcomes["superseded"]
            )
            .into());
        }

        let expected_committed = checked_counter_sum(
            "M4 Reload stress committed total",
            [
                self.classes["successful_reload"],
                self.classes["dependency_change"],
                self.classes["add_module"],
                self.classes["delete_module"],
                self.classes["rename_module"],
                self.activation_fault_recovery,
            ],
        )?;
        if self.development_pipeline["reload_committed"] != expected_committed
            || self.terminal_outcomes["committed"] != expected_committed
        {
            return Err(format!(
                "M4 Reload stress committed evidence is inconsistent: expected={expected_committed}, \
                 pipeline={}, terminal={}",
                self.development_pipeline["reload_committed"],
                self.terminal_outcomes["committed"]
            )
            .into());
        }

        let expected_compile_failed = self.classes["syntax_failure"]
            .checked_add(self.classes["type_failure"])
            .ok_or("M4 Reload compile-failure evidence overflowed u64")?;
        let expected_terminal = [
            ("compile_failed", expected_compile_failed),
            ("verify_failed", self.classes["verifier_failure"]),
            (
                "rolled_back_before_commit",
                self.classes["migration_failure"],
            ),
            ("activation_faulted", self.activation_fault_recovery),
            ("host_rebuild_required", 0),
        ];
        for (name, expected) in expected_terminal {
            if self.terminal_outcomes[name] != expected {
                return Err(format!(
                    "M4 Reload stress terminal `{name}`={} does not match real class evidence \
                     {expected}",
                    self.terminal_outcomes[name]
                )
                .into());
            }
        }
        let terminal_total = checked_counter_sum(
            "M4 Reload stress terminal total",
            self.terminal_outcomes.values().copied(),
        )?;
        if terminal_total != expected_generations {
            return Err(format!(
                "M4 Reload stress terminal outcomes account for {terminal_total} generations; \
                 expected {expected_generations}"
            )
            .into());
        }
        Ok(())
    }

    fn validate_failure_evidence(&self) -> Result<(), DynError> {
        for (name, expected) in [
            ("syntax_parse_diagnostic", self.classes["syntax_failure"]),
            ("typecheck_diagnostic", self.classes["type_failure"]),
            ("verifier_diagnostic", self.classes["verifier_failure"]),
            ("migration_rollback", self.classes["migration_failure"]),
            ("activation_fault", self.activation_fault_recovery),
        ] {
            if self.failure_evidence[name] != expected {
                return Err(format!(
                    "M4 Reload stress failure evidence `{name}`={} does not match class count \
                     {expected}",
                    self.failure_evidence[name]
                )
                .into());
            }
        }
        Ok(())
    }
}

fn checked_counter_sum(
    label: &str,
    values: impl IntoIterator<Item = u64>,
) -> Result<u64, DynError> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| format!("{label} overflowed u64").into())
    })
}

pub(super) struct M4GateRun {
    gates: Vec<M4GateRecord>,
    incremental: Option<M4IncrementalReport>,
    analysis_scale: Option<M4AnalysisScaleReport>,
    facade_scale: Option<M4FacadeScaleReport>,
    stress: Option<M4ReloadStressReport>,
}

impl M4GateRun {
    pub(super) fn ensure_passed(&self) -> Result<(), DynError> {
        println!("{}", serde_json::to_string_pretty(&self.gates)?);
        let failures = self.evidence_failures();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!("one or more M4 gates failed:\n- {}", failures.join("\n- ")).into())
        }
    }

    fn evidence_failures(&self) -> Vec<String> {
        let mut failures = gate_record_failures(&self.gates);
        match &self.incremental {
            Some(report) => push_validation(
                &mut failures,
                "incremental machine evidence",
                report.validate(),
            ),
            None => failures.push("incremental machine evidence is missing".into()),
        }
        match &self.analysis_scale {
            Some(report) => {
                push_validation(
                    &mut failures,
                    "analysis scale machine evidence",
                    report.validate(),
                );
            }
            None => failures.push("analysis scale machine evidence is missing".into()),
        }
        match &self.facade_scale {
            Some(report) => {
                push_validation(
                    &mut failures,
                    "facade compiled scale machine evidence",
                    report.validate(),
                );
            }
            None => failures.push("facade compiled scale machine evidence is missing".into()),
        }
        match &self.stress {
            Some(report) => {
                push_validation(
                    &mut failures,
                    "Reload stress machine evidence",
                    report.validate(),
                );
            }
            None => failures.push("Reload stress machine evidence is missing".into()),
        }
        if let (Some(analysis), Some(facade)) = (&self.analysis_scale, &self.facade_scale) {
            push_validation(
                &mut failures,
                "compiled/analysis scale closure",
                validate_facade_report_matches_analysis(analysis, facade),
            );
            if facade.scenarios.get("forward").is_some_and(|scenario| {
                scenario.artifact_bytes_digest == analysis.determinism.analysis_graph.first
            }) {
                failures.push(
                    "facade artifact_bytes_digest reuses the analysis_graph digest instead of \
                     Module::encode bytes"
                        .into(),
                );
            }
        }
        failures
    }
}

pub(super) fn run_m4_gates() -> M4GateRun {
    let source = execute_gate("test-m4-source", source_gate);
    let semantics = execute_gate("test-m4-semantics", semantics_gate);
    let incremental = execute_gate("test-m4-incremental", incremental_gate);
    let tooling = execute_gate("test-m4-tooling", tooling_gate);
    let scale_stress = execute_gate("m4-scale-stress", scale_stress_gate);

    let analysis_scale = scale_stress
        .value
        .as_ref()
        .map(|(analysis, _, _)| analysis.clone());
    let facade_scale = scale_stress
        .value
        .as_ref()
        .map(|(_, facade, _)| facade.clone());
    let stress = scale_stress
        .value
        .as_ref()
        .map(|(_, _, stress)| stress.clone());
    M4GateRun {
        gates: vec![
            source.record,
            semantics.record,
            incremental.record,
            tooling.record,
            scale_stress.record,
        ],
        incremental: incremental.value,
        analysis_scale,
        facade_scale,
        stress,
    }
}

fn gate_record_failures(gates: &[M4GateRecord]) -> Vec<String> {
    let mut failures = Vec::new();
    let names = gates
        .iter()
        .map(|gate| gate.name.as_str())
        .collect::<Vec<_>>();
    if names != M4_GATE_NAMES {
        failures.push(format!(
            "M4 gate record matrix is not exact: observed={names:?}, expected={M4_GATE_NAMES:?}"
        ));
    }
    for gate in gates {
        failures.extend(single_gate_record_failures(gate));
    }
    failures
}

fn single_gate_record_failures(gate: &M4GateRecord) -> Vec<String> {
    let mut failures = Vec::new();
    if gate.tested_head == "missing"
        || gate.completed_head == "missing"
        || gate.tested_branch == "missing"
        || gate.completed_branch == "missing"
    {
        failures.push(format!(
            "{} did not record a concrete Git HEAD and branch around execution",
            gate.name
        ));
    } else if gate.tested_head != gate.completed_head || gate.tested_branch != gate.completed_branch
    {
        failures.push(format!(
            "{} changed checkout while running: {}@{} -> {}@{}",
            gate.name,
            gate.tested_branch,
            gate.tested_head,
            gate.completed_branch,
            gate.completed_head
        ));
    }
    let commands_passed = !gate.evidence.commands.is_empty()
        && gate.evidence.commands.iter().all(|command| {
            command.success && command.exit_code == Some(0) && command.duration_ms > 0
        });
    let expected_status =
        GateStatus::from_passed(commands_passed && gate.evidence.failure.is_none());
    if gate.status != expected_status {
        failures.push(format!(
            "{} status is inconsistent with its real command evidence",
            gate.name
        ));
    }
    if !gate_has_frozen_command_matrix(gate) {
        failures.push(format!(
            "{} command/report evidence does not match its frozen M4 gate definition",
            gate.name
        ));
    }
    if gate.status != GateStatus::Pass {
        failures.push(format!(
            "{}: {}",
            gate.name,
            gate.evidence.failure.as_deref().unwrap_or(
                "failed without a precise error; inspect the recorded command exit codes"
            )
        ));
    }
    failures
}

fn gate_has_frozen_command_matrix(gate: &M4GateRecord) -> bool {
    let commands = &gate.evidence.commands;
    match gate.name.as_str() {
        "test-m4-source" => {
            commands.len() == 1
                && command_is(
                    &commands[0],
                    "cargo",
                    &["test", "-p", "nexa-analysis", "--test", "m4_source"],
                )
                && commands[0].environment.is_empty()
                && gate.evidence.machine_reports.is_empty()
        }
        "test-m4-semantics" => {
            commands.len() == 1
                && command_is(
                    &commands[0],
                    "cargo",
                    &["test", "-p", "nexa-analysis", "--test", "m4_semantics"],
                )
                && commands[0].environment.is_empty()
                && gate.evidence.machine_reports.is_empty()
        }
        "test-m4-incremental" => {
            commands.len() == 1
                && command_is(
                    &commands[0],
                    "cargo",
                    &[
                        "test",
                        "-p",
                        "nexa-analysis",
                        "--test",
                        "m4_incremental",
                        "--",
                        "--nocapture",
                    ],
                )
                && report_environment_matches(gate, 0, "NEXA_M4_INCREMENTAL_REPORT", 0)
                && fixed_machine_report_matches(gate, 0, INCREMENTAL_REPORT_PATH)
                && gate.evidence.machine_reports.len() == 1
        }
        "test-m4-tooling" => tooling_command_matrix_matches(gate),
        "m4-scale-stress" => scale_stress_command_matrix_matches(gate),
        _ => false,
    }
}

fn tooling_command_matrix_matches(gate: &M4GateRecord) -> bool {
    let commands = &gate.evidence.commands;
    commands.len() == 9
        && command_is(
            &commands[0],
            "cargo",
            &["test", "-p", "nexa-cli", "--test", "m4_tooling"],
        )
        && command_is(
            &commands[1],
            "cargo",
            &[
                "run",
                "-p",
                "nexa-cli",
                "--",
                "check",
                "--project",
                LANGUAGE_SCALE_PROJECT_PATH,
                "--diagnostic-format",
                "json",
            ],
        )
        && command_is(
            &commands[2],
            "cargo",
            &[
                "run",
                "-p",
                "nexa-cli",
                "--",
                "build",
                "--project",
                LANGUAGE_SCALE_PROJECT_PATH,
                "--output",
                LANGUAGE_SCALE_ARTIFACT_PATH,
                "--diagnostic-format",
                "json",
            ],
        )
        && command_is(
            &commands[3],
            "cargo",
            &[
                "run",
                "-p",
                "nexa-cli",
                "--",
                "test",
                "--project",
                LANGUAGE_SCALE_PROJECT_PATH,
                "--diagnostic-format",
                "json",
            ],
        )
        && command_is(&commands[4], "cargo", &["test", "-p", "nexa-test-runner"])
        && command_is(
            &commands[5],
            "cargo",
            &["test", "-p", "nexa-cli", "lsp::tests"],
        )
        && command_is(&commands[6], "pnpm", &["--dir", "editors", "generate"])
        && command_is(&commands[7], "pnpm", &["--dir", "editors", "check"])
        && command_is(&commands[8], "pnpm", &["--dir", "editors", "package"])
        && commands
            .iter()
            .all(|command| command.environment.is_empty())
        && fixed_machine_report_matches(gate, 0, EDITOR_PACKAGE_REPORT_PATH)
        && gate.evidence.machine_reports.len() == 1
}

fn scale_stress_command_matrix_matches(gate: &M4GateRecord) -> bool {
    let commands = &gate.evidence.commands;
    commands.len() == 3
        && command_is(
            &commands[0],
            "cargo",
            &[
                "test",
                "-p",
                "nexa-analysis",
                "--test",
                "m4_scale_stress",
                "m4_scale_stress",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        )
        && command_is(
            &commands[1],
            "cargo",
            &[
                "test",
                "-p",
                "nexa",
                "--test",
                "m4_package_build",
                "facade_scale_determinism_report",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        )
        && command_is(
            &commands[2],
            "cargo",
            &[
                "test",
                "-p",
                "nexa-embed",
                "--test",
                "m4_reload_stress",
                "m4_reload_stress",
                "--",
                "--ignored",
                "--exact",
                "--nocapture",
            ],
        )
        && report_environment_matches(gate, 0, "NEXA_M4_SCALE_REPORT", 0)
        && report_environment_matches(gate, 1, "NEXA_M4_FACADE_SCALE_REPORT", 1)
        && report_environment_matches(gate, 2, "NEXA_M4_RELOAD_STRESS_REPORT", 2)
        && fixed_machine_report_matches(gate, 0, ANALYSIS_SCALE_REPORT_PATH)
        && fixed_machine_report_matches(gate, 1, FACADE_SCALE_REPORT_PATH)
        && fixed_machine_report_matches(gate, 2, RELOAD_STRESS_REPORT_PATH)
        && gate.evidence.machine_reports.len() == 3
}

fn command_is(command: &M4CommandEvidence, program: &str, args: &[&str]) -> bool {
    command.program == program
        && command
            .args
            .iter()
            .map(String::as_str)
            .eq(args.iter().copied())
}

fn report_environment_matches(
    gate: &M4GateRecord,
    command_index: usize,
    variable: &str,
    report_index: usize,
) -> bool {
    let Some(command) = gate.evidence.commands.get(command_index) else {
        return false;
    };
    let Some(report) = gate.evidence.machine_reports.get(report_index) else {
        return false;
    };
    command.environment.len() == 1
        && command
            .environment
            .get(variable)
            .is_some_and(|value| value == report)
}

fn fixed_machine_report_matches(
    gate: &M4GateRecord,
    report_index: usize,
    relative_path: &str,
) -> bool {
    let expected = workspace_root().join(relative_path);
    gate.evidence
        .machine_reports
        .get(report_index)
        .is_some_and(|report| Path::new(report) == expected)
}

fn push_validation(failures: &mut Vec<String>, label: &str, result: Result<(), DynError>) {
    if let Err(error) = result {
        failures.push(format!("{label}: {error}"));
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct PreviousGateEvidence {
    status: GateStatus,
    failure: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct TagEvidence {
    object_type: String,
    target: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct RemotePublicationEvidence {
    remote: String,
    query_error: Option<String>,
    main_head: String,
    milestone_branch_head: String,
    local_completion_tag_object: String,
    completion_tag_object: String,
    completion_tag_target: String,
}

impl RemotePublicationEvidence {
    fn validate(&self, expected_head: &str) -> Result<(), DynError> {
        if self.remote != "origin" {
            return Err(format!(
                "M4 publication was checked against `{}` instead of `origin`",
                self.remote
            )
            .into());
        }
        if let Some(error) = &self.query_error {
            return Err(format!("could not query authoritative origin refs: {error}").into());
        }
        for (reference, observed) in [
            ("refs/heads/main", self.main_head.as_str()),
            (
                "refs/heads/codex/language-scale-m4",
                self.milestone_branch_head.as_str(),
            ),
            (
                "refs/tags/language-scale-m4-complete^{}",
                self.completion_tag_target.as_str(),
            ),
        ] {
            if observed != expected_head {
                return Err(format!(
                    "origin {reference} is `{observed}`, expected finalized HEAD `{expected_head}`"
                )
                .into());
            }
        }
        if self.local_completion_tag_object == "missing" {
            return Err("could not resolve the local M4 completion tag object".into());
        }
        if self.completion_tag_object != self.local_completion_tag_object {
            return Err(format!(
                "origin annotated tag object `{}` does not match local tag object `{}`",
                self.completion_tag_object, self.local_completion_tag_object
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct M4FinalDeterminismEvidence {
    analysis: M4AnalysisDeterminismEvidence,
    facade: M4FacadeScaleReport,
}

impl M4FinalDeterminismEvidence {
    fn validate(&self) -> Result<(), DynError> {
        self.analysis.validate()?;
        self.facade.validate()?;
        if self
            .facade
            .scenarios
            .get("forward")
            .is_some_and(|scenario| {
                scenario.artifact_bytes_digest == self.analysis.analysis_graph.first
            })
        {
            return Err(
                "facade artifact_bytes_digest reuses the analysis_graph digest instead of \
                 Module::encode bytes"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4CheckoutEvidence {
    branch: String,
    head: String,
    main_head: String,
    worktree_clean: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct M4FinalReport {
    schema: u32,
    milestone: String,
    tested_checkout: M4CheckoutEvidence,
    completed_checkout: M4CheckoutEvidence,
    previous_gates: PreviousGateEvidence,
    gates: Vec<M4GateRecord>,
    scale: M4ScaleCounters,
    closure_identity: String,
    determinism: M4FinalDeterminismEvidence,
    incremental: M4IncrementalEvidence,
    stress: M4ReloadStressReport,
    previous_tags: BTreeMap<String, TagEvidence>,
    completion_tag: TagEvidence,
    remote_publication: RemotePublicationEvidence,
    failures: Vec<String>,
    status: GateStatus,
}

impl M4FinalReport {
    fn evidence_failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        if self.schema != 2 {
            failures.push(format!(
                "M4 final report schema is {}; expected exactly 2",
                self.schema
            ));
        }
        if self.milestone != "Nexa M4 Language Scale Foundation" {
            failures.push(format!(
                "M4 final report milestone is unexpected: `{}`",
                self.milestone
            ));
        }
        if self.previous_gates.status != GateStatus::Pass || self.previous_gates.failure.is_some() {
            failures.push(format!(
                "preserved M1-M3 gates failed: {}",
                self.previous_gates
                    .failure
                    .as_deref()
                    .unwrap_or("no precise failure was recorded")
            ));
        }
        failures.extend(gate_record_failures(&self.gates));
        push_validation(&mut failures, "scale counters", self.scale.validate());
        push_validation(
            &mut failures,
            "final closure identity",
            validate_digest_32(&self.closure_identity, "M4 final closure identity"),
        );
        push_validation(
            &mut failures,
            "determinism equality",
            self.determinism.validate(),
        );
        push_validation(
            &mut failures,
            "compiled/analysis scale closure",
            validate_facade_scale_matches_analysis(&self.scale, &self.determinism.facade.scale),
        );
        if self.closure_identity != self.determinism.facade.closure_identity {
            failures.push(format!(
                "M4 final analysis closure identity `{}` differs from facade `{}`",
                self.closure_identity, self.determinism.facade.closure_identity
            ));
        }
        push_validation(
            &mut failures,
            "incremental invalidation sets",
            self.incremental.validate(),
        );
        push_validation(&mut failures, "stress safety", self.stress.validate());
        push_validation(
            &mut failures,
            "historical tag audit",
            validate_previous_tags(&self.previous_tags),
        );
        push_validation(
            &mut failures,
            "remote publication",
            self.remote_publication
                .validate(&self.completed_checkout.head),
        );
        if self.tested_checkout != self.completed_checkout {
            failures.push(format!(
                "M4 finalization checkout changed while historical and M4 gates ran: \
                 tested={:?}, completed={:?}",
                self.tested_checkout, self.completed_checkout
            ));
        }
        failures.extend(gate_checkout_identity_failures(
            &self.gates,
            &self.tested_checkout,
        ));
        failures.extend(completion_checkout_failures(
            &self.completed_checkout.branch,
            &self.completed_checkout.head,
            &self.completed_checkout.main_head,
            &self.completion_tag,
            self.completed_checkout.worktree_clean,
        ));
        failures
    }

    fn validate_consistency(&self) -> Result<(), DynError> {
        let evidence_failures = self.evidence_failures();
        let expected_status = GateStatus::from_passed(evidence_failures.is_empty());
        if self.status != expected_status {
            return Err(format!(
                "M4 final report status {:?} is inconsistent with typed evidence {:?}",
                self.status, evidence_failures
            )
            .into());
        }
        if self.failures != evidence_failures {
            return Err(format!(
                "M4 final report failure list is inconsistent: recorded={:?}, expected={:?}",
                self.failures, evidence_failures
            )
            .into());
        }
        Ok(())
    }
}

pub(super) fn finalize_m4() -> Result<(), DynError> {
    let output = workspace_root().join(FINAL_REPORT_PATH);
    prepare_final_report(&output)?;
    let tested_checkout = load_checkout_evidence();

    let previous_result = check_through_m3();
    let previous_gates = match previous_result {
        Ok(()) => PreviousGateEvidence {
            status: GateStatus::Pass,
            failure: None,
        },
        Err(error) => PreviousGateEvidence {
            status: GateStatus::Fail,
            failure: Some(error.to_string()),
        },
    };
    let gate_run = run_m4_gates();
    let scale = gate_run
        .analysis_scale
        .as_ref()
        .map_or_else(M4ScaleCounters::default, |report| report.scale.clone());
    let closure_identity = gate_run
        .analysis_scale
        .as_ref()
        .map_or_else(String::new, |report| report.closure_identity.clone());
    let determinism = M4FinalDeterminismEvidence {
        analysis: gate_run
            .analysis_scale
            .as_ref()
            .map_or_else(M4AnalysisDeterminismEvidence::default, |report| {
                report.determinism.clone()
            }),
        facade: gate_run.facade_scale.clone().unwrap_or_default(),
    };
    let incremental = gate_run.incremental.as_ref().map_or_else(
        M4IncrementalEvidence::default,
        M4IncrementalReport::evidence,
    );
    let stress = gate_run.stress.clone().unwrap_or_default();
    let previous_tags = PREVIOUS_MILESTONE_TAGS
        .into_iter()
        .map(|(name, _)| (name.to_owned(), load_tag(name)))
        .collect();
    let completion_tag = load_tag("language-scale-m4-complete");
    let remote_publication = load_remote_publication("origin");
    let completed_checkout = load_checkout_evidence();
    let mut report = M4FinalReport {
        schema: 2,
        milestone: "Nexa M4 Language Scale Foundation".into(),
        tested_checkout,
        completed_checkout,
        previous_gates,
        gates: gate_run.gates,
        scale,
        closure_identity,
        determinism,
        incremental,
        stress,
        previous_tags,
        completion_tag,
        remote_publication,
        failures: Vec::new(),
        status: GateStatus::Fail,
    };
    report.failures = report.evidence_failures();
    report.status = GateStatus::from_passed(report.failures.is_empty());
    report.validate_consistency()?;

    let mut encoded = serde_json::to_vec_pretty(&report)?;
    encoded.push(b'\n');
    atomic_write_final_report(&output, &encoded)?;
    let decoded: M4FinalReport = serde_json::from_slice(&fs::read(&output)?)?;
    decoded.validate_consistency()?;
    println!("{}", serde_json::to_string_pretty(&decoded)?);
    if decoded.status == GateStatus::Pass {
        Ok(())
    } else {
        Err(format!(
            "M4 finalization failed; typed evidence is available at {}",
            output.display()
        )
        .into())
    }
}

fn prepare_final_report(output: &Path) -> Result<(), DynError> {
    prepare_machine_report(output, "M4 final")?;
    let temporary = final_report_temporary_path(output)?;
    if temporary.exists() {
        fs::remove_file(&temporary).map_err(|error| {
            format!(
                "could not remove stale M4 final report temporary file {}: {error}",
                temporary.display()
            )
        })?;
    }
    Ok(())
}

fn atomic_write_final_report(output: &Path, encoded: &[u8]) -> Result<(), DynError> {
    let temporary = final_report_temporary_path(output)?;
    fs::write(&temporary, encoded).map_err(|error| {
        format!(
            "could not write M4 final report temporary file {}: {error}",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, output).map_err(|error| {
        format!(
            "could not atomically publish M4 final report {} from {}: {error}",
            output.display(),
            temporary.display()
        )
        .into()
    })
}

fn final_report_temporary_path(output: &Path) -> Result<std::path::PathBuf, DynError> {
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("M4 final report filename is not UTF-8")?;
    Ok(output.with_file_name(format!("{file_name}.tmp")))
}

fn git_optional_output(arguments: &[&str]) -> String {
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

fn load_checkout_evidence() -> M4CheckoutEvidence {
    let output = Command::new("git")
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])
        .current_dir(workspace_root())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return M4CheckoutEvidence {
            branch: "missing".into(),
            head: "missing".into(),
            main_head: git_optional_output(&["rev-parse", "refs/heads/main"]),
            worktree_clean: false,
        };
    };
    let Ok(status) = String::from_utf8(output.stdout) else {
        return M4CheckoutEvidence {
            branch: "missing".into(),
            head: "missing".into(),
            main_head: git_optional_output(&["rev-parse", "refs/heads/main"]),
            worktree_clean: false,
        };
    };
    if !output.status.success() {
        return M4CheckoutEvidence {
            branch: "missing".into(),
            head: "missing".into(),
            main_head: git_optional_output(&["rev-parse", "refs/heads/main"]),
            worktree_clean: false,
        };
    }
    let mut branch = None;
    let mut head = None;
    let mut worktree_clean = true;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("# branch.head ") {
            if branch.replace(value.to_owned()).is_some() {
                branch = None;
                break;
            }
        } else if let Some(value) = line.strip_prefix("# branch.oid ") {
            if head.replace(value.to_owned()).is_some() {
                head = None;
                break;
            }
        } else if !line.starts_with("# ") {
            worktree_clean = false;
        }
    }
    let branch = branch.unwrap_or_else(|| "missing".into());
    let head = head.unwrap_or_else(|| "missing".into());
    let main_head = if branch == "main" {
        head.clone()
    } else {
        git_optional_output(&["rev-parse", "refs/heads/main"])
    };
    M4CheckoutEvidence {
        branch,
        head,
        main_head,
        worktree_clean,
    }
}

fn load_remote_publication(remote: &str) -> RemotePublicationEvidence {
    const MAIN_REFERENCE: &str = "refs/heads/main";
    const MILESTONE_BRANCH_REFERENCE: &str = "refs/heads/codex/language-scale-m4";
    const COMPLETION_TAG_REFERENCE: &str = "refs/tags/language-scale-m4-complete";
    const COMPLETION_TAG_TARGET_REFERENCE: &str = "refs/tags/language-scale-m4-complete^{}";

    let output = Command::new("git")
        .args([
            "ls-remote",
            "--exit-code",
            remote,
            MAIN_REFERENCE,
            MILESTONE_BRANCH_REFERENCE,
            COMPLETION_TAG_REFERENCE,
            COMPLETION_TAG_TARGET_REFERENCE,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(workspace_root())
        .output();
    let local_completion_tag_object = git_optional_output(&["rev-parse", COMPLETION_TAG_REFERENCE]);
    match output {
        Ok(output) if output.status.success() => {
            let refs = String::from_utf8(output.stdout)
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
            RemotePublicationEvidence {
                remote: remote.to_owned(),
                query_error: None,
                main_head: refs
                    .get(MAIN_REFERENCE)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                milestone_branch_head: refs
                    .get(MILESTONE_BRANCH_REFERENCE)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                local_completion_tag_object,
                completion_tag_object: refs
                    .get(COMPLETION_TAG_REFERENCE)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
                completion_tag_target: refs
                    .get(COMPLETION_TAG_TARGET_REFERENCE)
                    .cloned()
                    .unwrap_or_else(|| "missing".into()),
            }
        }
        Ok(output) => RemotePublicationEvidence {
            remote: remote.to_owned(),
            query_error: Some(format!(
                "git ls-remote exited with {}: {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            local_completion_tag_object,
            ..RemotePublicationEvidence::default()
        },
        Err(error) => RemotePublicationEvidence {
            remote: remote.to_owned(),
            query_error: Some(format!("could not start git ls-remote: {error}")),
            local_completion_tag_object,
            ..RemotePublicationEvidence::default()
        },
    }
}

fn load_tag(name: &str) -> TagEvidence {
    let reference = format!("refs/tags/{name}");
    TagEvidence {
        object_type: git_optional_output(&["cat-file", "-t", &reference]),
        target: git_optional_output(&["rev-parse", &format!("{reference}^{{}}")]),
    }
}

fn completion_checkout_failures(
    current_branch: &str,
    git_head: &str,
    main_head: &str,
    completion_tag: &TagEvidence,
    worktree_clean: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    if current_branch != "main" {
        failures.push(format!(
            "M4 finalization must run on branch `main`, observed `{current_branch}`"
        ));
    }
    if git_head == "missing" {
        failures.push("could not resolve current HEAD".into());
    }
    if main_head == "missing" {
        failures.push("could not resolve local main".into());
    }
    if git_head != main_head {
        failures.push(format!(
            "M4 finalization HEAD `{git_head}` is not local main `{main_head}`"
        ));
    }
    if completion_tag.object_type != "tag" {
        failures.push(format!(
            "language-scale-m4-complete is not an annotated tag: type={}",
            completion_tag.object_type
        ));
    }
    if completion_tag.target != git_head || completion_tag.target != main_head {
        failures.push(format!(
            "language-scale-m4-complete targets `{}`, expected current/main commit `{git_head}`",
            completion_tag.target
        ));
    }
    if !worktree_clean {
        failures.push("M4 finalization requires a clean worktree".into());
    }
    failures
}

fn gate_checkout_identity_failures(
    gates: &[M4GateRecord],
    checkout: &M4CheckoutEvidence,
) -> Vec<String> {
    let mut failures = Vec::new();
    for gate in gates {
        if gate.tested_head != checkout.head
            || gate.completed_head != checkout.head
            || gate.tested_branch != checkout.branch
            || gate.completed_branch != checkout.branch
        {
            failures.push(format!(
                "{} did not run on the finalized checkout {}@{}: tested={}@{}, completed={}@{}",
                gate.name,
                checkout.branch,
                checkout.head,
                gate.tested_branch,
                gate.tested_head,
                gate.completed_branch,
                gate.completed_head
            ));
        }
    }
    failures
}

fn validate_previous_tags(tags: &BTreeMap<String, TagEvidence>) -> Result<(), DynError> {
    let expected_names = PREVIOUS_MILESTONE_TAGS
        .into_iter()
        .map(|(name, _)| name)
        .collect::<BTreeSet<_>>();
    let observed_names = tags.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if observed_names != expected_names {
        return Err(format!(
            "historical milestone tag set changed: expected={expected_names:?}, \
             observed={observed_names:?}"
        )
        .into());
    }
    for (name, expected_target) in PREVIOUS_MILESTONE_TAGS {
        let evidence = tags
            .get(name)
            .ok_or_else(|| format!("historical milestone tag `{name}` is missing"))?;
        if evidence.object_type != "tag" || evidence.target != expected_target {
            return Err(format!(
                "historical annotated tag `{name}` changed: type={}, target={}, expected={expected_target}",
                evidence.object_type, evidence.target
            )
            .into());
        }
    }
    Ok(())
}

fn validate_facade_scale_matches_analysis(
    analysis: &M4ScaleCounters,
    facade: &M4FacadeScaleCounters,
) -> Result<(), DynError> {
    if facade.modules == analysis.modules
        && facade.symbols == analysis.symbols
        && facade.import_edges == analysis.import_edges
        && facade.packages == analysis.packages
    {
        Ok(())
    } else {
        Err(format!(
            "facade compiled scale does not match analysis scale: analysis={analysis:?}, \
             facade={facade:?}"
        )
        .into())
    }
}

fn validate_facade_report_matches_analysis(
    analysis: &M4AnalysisScaleReport,
    facade: &M4FacadeScaleReport,
) -> Result<(), DynError> {
    validate_facade_scale_matches_analysis(&analysis.scale, &facade.scale)?;
    if analysis.closure_identity != facade.closure_identity {
        return Err(format!(
            "analysis/facade closure identity mismatch: analysis={}, facade={}",
            analysis.closure_identity, facade.closure_identity
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn equality(byte: u8) -> M4EqualityEvidence {
        M4EqualityEvidence {
            first: digest(byte),
            second: digest(byte),
        }
    }

    fn valid_analysis_determinism() -> M4AnalysisDeterminismEvidence {
        M4AnalysisDeterminismEvidence {
            fingerprint: equality(1),
            lockfile: equality(2),
            analysis_graph: equality(3),
            analysis_diagnostics: equality(4),
            query_cold_hot: equality(3),
            hot_cache_hits: 100,
            temporary_root: M4TemporaryRootEvidence {
                first: digest(3),
                second: digest(3),
                mechanism: "filesystem-directory-loader-full-closure-analysis".into(),
                first_root_digest: digest(5),
                second_root_digest: digest(6),
                first_packages: 20,
                second_packages: 20,
                first_modules: 100,
                second_modules: 100,
            },
            worker_order: M4WorkerOrderEvidence {
                first: digest(3),
                second: digest(3),
                mechanism: "concurrent-thread-analysis-controlled-completion".into(),
                first_completion_order: vec![0, 1, 2, 3],
                second_completion_order: vec![3, 2, 1, 0],
                builds_per_order: 4,
                first_max_in_flight: 4,
                second_max_in_flight: 4,
            },
        }
    }

    fn compiled_scenario() -> M4CompiledScenarioEvidence {
        M4CompiledScenarioEvidence {
            artifact_bytes_digest: digest(10),
            diagnostic_ndjson_digest: digest(11),
            diagnostic_records: 2,
            source_fingerprint: digest(12),
            public_api_fingerprint: digest(13),
            state_schema_fingerprint: digest(14),
            build_fingerprint: digest(15),
            linked_state_fingerprint: digest(16),
            closure_identity: digest(17),
            lock_digest: digest(18),
            compiled_package_ids: (0..20).map(|index| format!("package-{index:02}")).collect(),
            compiled_module_ids: (0..100)
                .map(|index| format!("package-00::module-{index:03}"))
                .collect(),
            mechanism: "direct-session".into(),
            filesystem_root_digest: None,
            loaded_package_directories: 0,
            loaded_package_ids: Vec::new(),
            worker_completion_order: Vec::new(),
            max_in_flight: 1,
        }
    }

    fn valid_facade_report() -> M4FacadeScaleReport {
        let mut scenarios = REQUIRED_FACADE_SCENARIOS
            .into_iter()
            .map(|name| (name.into(), compiled_scenario()))
            .collect::<BTreeMap<String, M4CompiledScenarioEvidence>>();
        for (name, root) in [("temp_root_a", digest(20)), ("temp_root_b", digest(21))] {
            let scenario = scenarios.get_mut(name).unwrap();
            scenario.mechanism = "filesystem-directory-loader".into();
            scenario.filesystem_root_digest = Some(root);
            scenario.loaded_package_directories = 20;
            scenario.loaded_package_ids = scenario.compiled_package_ids.clone();
        }
        for name in ["cold_cache", "hot_cache"] {
            scenarios.get_mut(name).unwrap().mechanism = "persistent-query-database".into();
        }
        scenarios.get_mut("worker_order_a").unwrap().mechanism =
            "thread-dispatch-and-completion".into();
        scenarios
            .get_mut("worker_order_a")
            .unwrap()
            .worker_completion_order = vec![0, 1, 2, 3];
        scenarios.get_mut("worker_order_a").unwrap().max_in_flight = 4;
        scenarios.get_mut("worker_order_b").unwrap().mechanism =
            "thread-dispatch-and-completion".into();
        scenarios
            .get_mut("worker_order_b")
            .unwrap()
            .worker_completion_order = vec![3, 2, 1, 0];
        scenarios.get_mut("worker_order_b").unwrap().max_in_flight = 4;
        M4FacadeScaleReport {
            schema: 3,
            status: "PASS".into(),
            closure_identity: digest(17),
            scale: M4FacadeScaleCounters {
                modules: 100,
                symbols: 1_000,
                package_modules: 100,
                package_symbols: 1_000,
                import_edges: 500,
                packages: 20,
            },
            pipeline: M4FacadePipelineEvidence {
                analyzer_runs: 42,
                invalid_analyzer_runs: 10,
                successful_check_analyzer_runs: 16,
                compile_analyzer_runs: 16,
                typed_compiler_runs: 16,
                verifier_runs: 16,
                module_encode_runs: 16,
                module_bytes_length: 1,
            },
            diagnostics: M4CanonicalDiagnosticEvidence {
                format: "canonical-ndjson".into(),
                scenario_runs: 10,
                records: 20,
            },
            query_cache: M4QueryCacheEvidence {
                cold: M4QueryRunEvidence {
                    parsed_sources: 100,
                    analyzed_modules: 100,
                    cumulative_hits: 1,
                    ..M4QueryRunEvidence::default()
                },
                hot: M4QueryRunEvidence {
                    reused_queries: 100,
                    cumulative_hits: 101,
                    ..M4QueryRunEvidence::default()
                },
            },
            scenarios,
        }
    }

    fn valid_reload_stress_report() -> M4ReloadStressReport {
        M4ReloadStressReport {
            schema: 2,
            status: "PASS".into(),
            classes: REQUIRED_RELOAD_CLASSES
                .into_iter()
                .map(|name| (name.into(), 100))
                .collect(),
            activation_fault_recovery: 10,
            development_pipeline: BTreeMap::from([
                ("change_detected".into(), 1_320),
                ("compile_queued".into(), 1_320),
                ("compile_started".into(), 1_320),
                ("late_results_observed".into(), 1_320),
                ("host_requests_exercised".into(), 1),
                ("candidate_superseded".into(), 400),
                ("reload_committed".into(), 510),
            ]),
            terminal_outcomes: BTreeMap::from([
                ("committed".into(), 510),
                ("compile_failed".into(), 200),
                ("verify_failed".into(), 100),
                ("rolled_back_before_commit".into(), 100),
                ("activation_faulted".into(), 10),
                ("superseded".into(), 400),
                ("host_rebuild_required".into(), 0),
            ]),
            failure_evidence: BTreeMap::from([
                ("syntax_parse_diagnostic".into(), 100),
                ("typecheck_diagnostic".into(), 100),
                ("verifier_diagnostic".into(), 100),
                ("migration_rollback".into(), 100),
                ("activation_fault".into(), 10),
            ]),
            safety: REQUIRED_RELOAD_SAFETY_COUNTERS
                .into_iter()
                .map(|name| (name.into(), 0))
                .collect(),
        }
    }

    fn noncanonical_incremental_scenario(name: &str) -> M4InvalidationScenario {
        let value = format!("{name}:query");
        let expected = M4InvalidationSets {
            parsed: vec![value.clone()],
            analyzed: Vec::new(),
            reused: vec![format!("{name}:reused")],
            invalidated: Vec::new(),
        };
        M4InvalidationScenario {
            name: name.into(),
            observed: expected.clone(),
            expected,
            hot_cache_hits: 1,
        }
    }

    #[test]
    fn analysis_scale_requires_real_thresholds_and_equal_pairs() {
        let mut report = M4AnalysisScaleReport {
            schema: 3,
            scale: M4ScaleCounters {
                modules: 100,
                symbols: 1_000,
                import_edges: 500,
                packages: 20,
            },
            closure_identity: digest(30),
            determinism: valid_analysis_determinism(),
            status: "PASS".into(),
        };
        report.validate().unwrap();

        report.scale.modules = 99;
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least 100")
        );
        report.scale.modules = 100;
        report.determinism.analysis_graph.second = digest(31);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("analysis graph determinism mismatch")
        );
    }

    #[test]
    fn facade_scale_requires_ten_equal_compiled_scenarios_and_real_pipeline_runs() {
        let mut report = valid_facade_report();
        report.validate().unwrap();

        report.pipeline.verifier_runs = 9;
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exact 10-scenario execution matrix")
        );
        report.pipeline.verifier_runs = 16;
        report
            .scenarios
            .get_mut("reverse")
            .unwrap()
            .artifact_bytes_digest = digest(99);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not byte/fingerprint/diagnostic identical")
        );
    }

    #[test]
    fn facade_scale_must_match_the_analysis_closure() {
        let analysis = M4ScaleCounters {
            modules: 100,
            symbols: 1_000,
            import_edges: 500,
            packages: 20,
        };
        let mut facade = M4FacadeScaleCounters {
            modules: 100,
            symbols: 1_000,
            package_modules: 100,
            package_symbols: 1_000,
            import_edges: 500,
            packages: 20,
        };
        validate_facade_scale_matches_analysis(&analysis, &facade).unwrap();

        facade.modules += 1;
        assert!(
            validate_facade_scale_matches_analysis(&analysis, &facade)
                .unwrap_err()
                .to_string()
                .contains("does not match analysis scale")
        );
    }

    #[test]
    fn incremental_evidence_requires_frozen_oracles_and_sorted_sets() {
        let evidence = M4IncrementalEvidence {
            scenarios: Vec::new(),
        };
        assert!(
            evidence
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exact 11-scenario matrix")
        );

        let scenario = noncanonical_incremental_scenario("private-body-change");
        assert!(
            scenario
                .validate()
                .unwrap_err()
                .to_string()
                .contains("frozen xtask oracle")
        );

        let sets = M4InvalidationSets {
            parsed: vec!["z".into(), "a".into()],
            ..M4InvalidationSets::default()
        };
        assert!(
            sets.validate_canonical("source-add", "observed")
                .unwrap_err()
                .to_string()
                .contains("not strictly sorted")
        );

        assert_eq!(
            REQUIRED_INCREMENTAL_ORACLE_DIGESTS
                .iter()
                .map(|(name, _)| *name)
                .collect::<BTreeSet<_>>(),
            REQUIRED_INCREMENTAL_SCENARIOS
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn reload_stress_requires_every_class_and_zero_safety_counters() {
        let mut report = valid_reload_stress_report();
        report.validate().unwrap();

        report.classes.insert("syntax_failure".into(), 99);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least 100")
        );
        report.classes.insert("syntax_failure".into(), 100);
        report
            .development_pipeline
            .insert("change_detected".into(), 1_319);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("does not account for all 1320 real generations")
        );
        report
            .development_pipeline
            .insert("change_detected".into(), 1_320);
        report
            .failure_evidence
            .insert("syntax_parse_diagnostic".into(), 99);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("does not match class count 100")
        );
        report
            .failure_evidence
            .insert("syntax_parse_diagnostic".into(), 100);
        report.terminal_outcomes.insert("superseded".into(), 399);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("stale-candidate evidence is inconsistent")
        );
        report.terminal_outcomes.insert("superseded".into(), 400);
        report.safety.insert("worker_residual".into(), 1);
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("worker_residual=1")
        );
        report.safety.insert("worker_residual".into(), 0);
        report.activation_fault_recovery = 9;
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("at least 10")
        );
        report.activation_fault_recovery = 10;
        report.classes.remove("aba_change");
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("keys are not exact")
        );
    }

    #[test]
    fn historical_tag_matrix_covers_every_prior_completion_tag() {
        let tags = PREVIOUS_MILESTONE_TAGS
            .into_iter()
            .map(|(name, target)| {
                (
                    name.to_owned(),
                    TagEvidence {
                        object_type: "tag".into(),
                        target: target.into(),
                    },
                )
            })
            .collect();
        validate_previous_tags(&tags).unwrap();
    }

    #[test]
    fn completion_requires_the_main_branch_at_the_exact_annotated_tag() {
        let completion_tag = TagEvidence {
            object_type: "tag".into(),
            target: "commit".into(),
        };
        assert!(
            completion_checkout_failures("main", "commit", "commit", &completion_tag, true)
                .is_empty()
        );

        let failures = completion_checkout_failures(
            "codex/language-scale-m4",
            "commit",
            "commit",
            &completion_tag,
            true,
        );
        assert_eq!(
            failures,
            ["M4 finalization must run on branch `main`, observed \
              `codex/language-scale-m4`"]
        );

        let failures = completion_checkout_failures(
            "main",
            "head",
            "main",
            &TagEvidence {
                object_type: "commit".into(),
                target: "other".into(),
            },
            false,
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("not local main"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("not an annotated tag"))
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.contains("requires a clean worktree"))
        );
    }

    #[test]
    fn remote_publication_requires_both_branches_and_the_exact_annotated_tag_object() {
        let mut publication = RemotePublicationEvidence {
            remote: "origin".into(),
            query_error: None,
            main_head: "commit".into(),
            milestone_branch_head: "commit".into(),
            local_completion_tag_object: "tag-object".into(),
            completion_tag_object: "tag-object".into(),
            completion_tag_target: "commit".into(),
        };
        publication.validate("commit").unwrap();

        publication.milestone_branch_head = "missing".into();
        assert!(
            publication
                .validate("commit")
                .unwrap_err()
                .to_string()
                .contains("codex/language-scale-m4")
        );
        publication.milestone_branch_head = "commit".into();
        publication.completion_tag_object = "other-tag-object".into();
        assert!(
            publication
                .validate("commit")
                .unwrap_err()
                .to_string()
                .contains("does not match local tag object")
        );
    }

    #[test]
    fn final_report_replaces_stale_evidence_only_after_atomic_publish() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "nexa-m4-final-report-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("final-report.json");
        let temporary = final_report_temporary_path(&output).unwrap();
        fs::write(&output, b"{\"status\":\"PASS\"}\n").unwrap();
        fs::write(&temporary, b"partial").unwrap();

        prepare_final_report(&output).unwrap();
        assert!(!output.exists());
        assert!(!temporary.exists());

        atomic_write_final_report(&output, b"{\"status\":\"FAIL\"}\n").unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"{\"status\":\"FAIL\"}\n");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn actual_scale_report_shape_deserializes_as_typed_evidence() {
        let pair = serde_json::json!({"first": digest(1), "second": digest(1)});
        let report: M4AnalysisScaleReport = serde_json::from_value(serde_json::json!({
            "schema": 3,
            "status": "PASS",
            "closure_identity": digest(2),
            "scale": {
                "modules": 105,
                "symbols": 1050,
                "import_edges": 500,
                "packages": 20
            },
            "determinism": {
                "fingerprint": pair.clone(),
                "lockfile": pair.clone(),
                "analysis_graph": pair.clone(),
                "analysis_diagnostics": pair.clone(),
                "query_cold_hot": pair.clone(),
                "hot_cache_hits": 105,
                "temporary_root": {
                    "first": digest(1),
                    "second": digest(1),
                    "mechanism": "filesystem-directory-loader-full-closure-analysis",
                    "first_root_digest": digest(3),
                    "second_root_digest": digest(4),
                    "first_packages": 20,
                    "second_packages": 20,
                    "first_modules": 105,
                    "second_modules": 105
                },
                "worker_order": {
                    "first": digest(1),
                    "second": digest(1),
                    "mechanism": "concurrent-thread-analysis-controlled-completion",
                    "first_completion_order": [0, 1, 2, 3],
                    "second_completion_order": [3, 2, 1, 0],
                    "builds_per_order": 4,
                    "first_max_in_flight": 4,
                    "second_max_in_flight": 4
                }
            }
        }))
        .unwrap();
        report.validate().unwrap();

        let mut document = serde_json::to_value(&report).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), serde_json::Value::Bool(true));
        let error = serde_json::from_value::<M4AnalysisScaleReport>(document).unwrap_err();
        assert!(error.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn typed_reports_reject_duplicate_recognized_fields() {
        let error = serde_json::from_str::<M4AnalysisScaleReport>(
            r#"{
                "schema": 1,
                "schema": 1,
                "status": "PASS",
                "scale": {
                    "modules": 100,
                    "symbols": 1000,
                    "import_edges": 500,
                    "packages": 20
                },
                "determinism": {}
            }"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate field `schema`"));

        let error = serde_json::from_str::<M4AnalysisScaleReport>(
            r#"{
                "schema": 1,
                "status": "PASS",
                "scale": {
                    "modules": 100,
                    "symbols": 1000,
                    "import_edges": 500,
                    "resolved_imports": 500,
                    "packages": 20
                },
                "determinism": {}
            }"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown field `resolved_imports`")
        );
    }

    #[test]
    fn language_scale_gate_requires_both_real_package_tests() {
        let mut output = serde_json::json!({
            "schema": 1,
            "command": "test",
            "status": "ok",
            "results": [
                {
                    "qualifiedName":
                        "example.language-scale::test.basic.scoring::library_is_linked",
                    "status": "PASS"
                },
                {
                    "qualifiedName":
                        "example.language-scale::test.basic.scoring::package_helper_uses_the_constant",
                    "status": "PASS"
                }
            ],
            "summary": {
                "total": 2,
                "passed": 2,
                "failed": 0,
                "errors": 0
            }
        });
        validate_language_scale_test_output(&serde_json::to_vec(&output).unwrap()).unwrap();

        output["summary"]["passed"] = serde_json::json!(1);
        assert!(
            validate_language_scale_test_output(&serde_json::to_vec(&output).unwrap())
                .unwrap_err()
                .to_string()
                .contains("both canonical tests passed")
        );
    }

    #[test]
    fn tooling_frozen_matrix_includes_canonical_language_scale_commands() {
        fn command(program: &str, args: &[&str]) -> M4CommandEvidence {
            M4CommandEvidence {
                program: program.into(),
                args: args.iter().map(|argument| (*argument).into()).collect(),
                environment: BTreeMap::new(),
                exit_code: Some(0),
                success: true,
                duration_ms: 1,
            }
        }

        let mut gate = M4GateRecord {
            name: "test-m4-tooling".into(),
            status: GateStatus::Pass,
            duration_ms: 1,
            tested_head: "head".into(),
            tested_branch: "branch".into(),
            completed_head: "head".into(),
            completed_branch: "branch".into(),
            evidence: M4GateEvidence {
                commands: vec![
                    command("cargo", &["test", "-p", "nexa-cli", "--test", "m4_tooling"]),
                    command(
                        "cargo",
                        &[
                            "run",
                            "-p",
                            "nexa-cli",
                            "--",
                            "check",
                            "--project",
                            LANGUAGE_SCALE_PROJECT_PATH,
                            "--diagnostic-format",
                            "json",
                        ],
                    ),
                    command(
                        "cargo",
                        &[
                            "run",
                            "-p",
                            "nexa-cli",
                            "--",
                            "build",
                            "--project",
                            LANGUAGE_SCALE_PROJECT_PATH,
                            "--output",
                            LANGUAGE_SCALE_ARTIFACT_PATH,
                            "--diagnostic-format",
                            "json",
                        ],
                    ),
                    command(
                        "cargo",
                        &[
                            "run",
                            "-p",
                            "nexa-cli",
                            "--",
                            "test",
                            "--project",
                            LANGUAGE_SCALE_PROJECT_PATH,
                            "--diagnostic-format",
                            "json",
                        ],
                    ),
                    command("cargo", &["test", "-p", "nexa-test-runner"]),
                    command("cargo", &["test", "-p", "nexa-cli", "lsp::tests"]),
                    command("pnpm", &["--dir", "editors", "generate"]),
                    command("pnpm", &["--dir", "editors", "check"]),
                    command("pnpm", &["--dir", "editors", "package"]),
                ],
                machine_reports: vec![
                    workspace_root()
                        .join(EDITOR_PACKAGE_REPORT_PATH)
                        .display()
                        .to_string(),
                ],
                failure: None,
            },
        };
        assert!(gate_has_frozen_command_matrix(&gate));
        gate.evidence.commands.remove(3);
        assert!(!gate_has_frozen_command_matrix(&gate));
    }

    #[test]
    fn frozen_gate_matrix_rejects_substituted_command_evidence() {
        let mut gate = M4GateRecord {
            name: "test-m4-source".into(),
            status: GateStatus::Pass,
            duration_ms: 1,
            tested_head: "head".into(),
            tested_branch: "branch".into(),
            completed_head: "head".into(),
            completed_branch: "branch".into(),
            evidence: M4GateEvidence {
                commands: vec![M4CommandEvidence {
                    program: "cargo".into(),
                    args: ["test", "-p", "nexa-analysis", "--test", "m4_source"]
                        .map(str::to_owned)
                        .to_vec(),
                    environment: BTreeMap::new(),
                    exit_code: Some(0),
                    success: true,
                    duration_ms: 1,
                }],
                machine_reports: Vec::new(),
                failure: None,
            },
        };
        assert!(gate_has_frozen_command_matrix(&gate));
        gate.evidence.commands[0].args[4] = "some_other_test".into();
        assert!(!gate_has_frozen_command_matrix(&gate));
    }
}
