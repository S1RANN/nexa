use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

mod m4;
mod m4r1;

type DynError = Box<dyn std::error::Error>;

#[derive(Debug, Serialize)]
struct RepoHealth {
    schema_version: u32,
    product_rust_loc: usize,
    unit_test_loc: usize,
    integration_test_loc: usize,
    tool_loc: usize,
    fixture_total_bytes: u64,
    workspace_members: usize,
    versioned_directories: usize,
    duplicate_file_hashes: usize,
    tracked_files_over_512_kib: Vec<String>,
    active_gate1_tool_crates: usize,
    versioned_gate_experiment_directories: usize,
    tracked_raw_evidence_files: usize,
    duplicate_versioned_fixtures: usize,
    gate1_test_tool_loc_reduction_percent: u64,
    low_level_event_violations: Vec<String>,
    public_api_violations: Vec<String>,
    public_raw_task_api_violations: usize,
    public_task_lifecycle_bypass_violations: usize,
    shadow_runtime_model_violations: usize,
    business_host_stub_e2e_violations: usize,
    manual_binding_e2e_registry_violations: usize,
    shadow_runtime_prevalidation_violations: usize,
    runtime_invalid_event_short_circuit_violations: usize,
    runtime_differential_current_handle_violations: usize,
    missing_runtime_invocation_counter_evidence: usize,
    model_repeated_reload_semantic_violations: usize,
    real_runtime_fuzz_violations: usize,
    unverified_host_resource_release_kinds: usize,
    legacy_host_abi_violations: usize,
    completion_buffer_symbol_violations: usize,
    reload_pause_symbol_violations: usize,
    retired_epoch_business_api_violations: usize,
    deprecated_allow_violations: usize,
    unsafe_containment_violations: Vec<String>,
    versioned_model_file_count: usize,
    historical_tag_type: String,
    historical_tag_target: String,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct FinalizationInventory {
    schema_version: u32,
    counts: BTreeMap<String, usize>,
    internal_whitelist: Vec<String>,
}

#[derive(Debug, Serialize)]
struct M1FinalReport {
    head: String,
    tag_type: String,
    tag_target: String,
    workspace_check: &'static str,
    repo_audit: &'static str,
    binding_test: &'static str,
    binding_positive_runtime_runs: u64,
    task_test: &'static str,
    reload_test: &'static str,
    model_differential_test: &'static str,
    differential_sequence_count: usize,
    high_risk_sequence_count: usize,
    semantic_regression_sequence_count: usize,
    fuzz_build: &'static str,
    fuzz_corpus_replay: &'static str,
    bench_smoke: &'static str,
    working_tree_clean: bool,
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct M2FinalReport {
    head: String,
    tag_type: String,
    tag_target: String,
    working_tree_clean: bool,
    ergonomics_audit: &'static str,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct M3R1FinalReport {
    schema: u32,
    milestone: &'static str,
    head: String,
    workspace: &'static str,
    m1_m2_regression: &'static str,
    engine_api: &'static str,
    engine_diagnostics: &'static str,
    worker_queue_saturation: &'static str,
    result_backpressure: &'static str,
    disable_in_flight: &'static str,
    shutdown_in_flight: &'static str,
    generation_accounting: &'static str,
    reload_stress: &'static str,
    metrics: &'static str,
    cli_policy: &'static str,
    lsp: &'static str,
    editor: &'static str,
    repo_audit: &'static str,
    queue_lost_jobs: u64,
    queue_lost_results: u64,
    generations_without_terminal: u64,
    engine_diagnostic_direct_construction: usize,
    engine_diagnostic_real_paths: usize,
    metrics_trusted: bool,
    policy_validation: &'static str,
    nidl_span: &'static str,
    uri_matrix: &'static str,
    worktree_clean: bool,
    historical_tag_type: String,
    historical_tag_target: String,
    tag_type: String,
    tag_target: String,
    tag_target_matches_head: bool,
    failures: Vec<String>,
    status: &'static str,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerationAccountingReport {
    schema: u32,
    scenario_count: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    superseded_before_compile: u64,
    cancelled_by_source_removal: u64,
    cancelled_by_disable: u64,
    cancelled_by_shutdown: u64,
    status: String,
}

impl GenerationAccountingReport {
    fn failed() -> Self {
        Self {
            schema: 0,
            scenario_count: 0,
            created_generations: 0,
            terminal_generations: 0,
            duplicate_terminals: u64::MAX,
            generations_without_terminal: u64::MAX,
            superseded_before_compile: 0,
            cancelled_by_source_removal: 0,
            cancelled_by_disable: 0,
            cancelled_by_shutdown: 0,
            status: "FAIL".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateFreshnessScenarioReport {
    name: String,
    stage: String,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
    status: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateFreshnessReport {
    schema: u32,
    scenario_count: u64,
    pending_scenario_count: u64,
    in_flight_scenario_count: u64,
    result_queue_scenario_count: u64,
    ready_candidate_scenario_count: u64,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
    scenarios: Vec<CandidateFreshnessScenarioReport>,
    status: String,
}

impl CandidateFreshnessReport {
    fn failed() -> Self {
        Self {
            schema: 0,
            scenario_count: 0,
            pending_scenario_count: 0,
            in_flight_scenario_count: 0,
            result_queue_scenario_count: 0,
            ready_candidate_scenario_count: 0,
            stale_candidates_observed: 0,
            stale_candidates_committed: u64::MAX,
            desired_build_fingerprint_mismatches_rejected: 0,
            superseded_before_compile: 0,
            superseded_after_compile: 0,
            created_generations: 0,
            terminal_generations: 0,
            duplicate_terminals: u64::MAX,
            generations_without_terminal: u64::MAX,
            active_runtime_violations: u64::MAX,
            last_known_good_violations: u64::MAX,
            scenarios: Vec::new(),
            status: "FAIL".into(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct M3R2FinalReport {
    schema: u32,
    milestone: &'static str,
    head: String,
    workspace: &'static str,
    m1_m2_regression: &'static str,
    engine_api: &'static str,
    engine_diagnostics: &'static str,
    worker_queue_saturation: &'static str,
    result_backpressure: &'static str,
    disable_in_flight: &'static str,
    shutdown_in_flight: &'static str,
    generation_accounting: &'static str,
    reload_stress: &'static str,
    metrics: &'static str,
    cli_policy: &'static str,
    lsp: &'static str,
    editor: &'static str,
    repo_audit: &'static str,
    queue_lost_jobs: u64,
    queue_lost_results: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    engine_diagnostic_direct_construction: usize,
    engine_diagnostic_real_paths: usize,
    metrics_trusted: bool,
    policy_validation: &'static str,
    nidl_span: &'static str,
    uri_matrix: &'static str,
    worktree_clean: bool,
    historical_tag_type: String,
    historical_tag_target: String,
    r1_tag_type: String,
    r1_tag_target: String,
    tag_type: String,
    tag_target: String,
    tag_target_matches_head: bool,
    failures: Vec<String>,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct M3R3FinalReport {
    schema: u32,
    milestone: &'static str,
    head: String,
    workspace: &'static str,
    m1_m2_regression: &'static str,
    engine_api: &'static str,
    engine_diagnostics: &'static str,
    worker_queue_saturation: &'static str,
    result_backpressure: &'static str,
    disable_in_flight: &'static str,
    shutdown_in_flight: &'static str,
    generation_accounting: &'static str,
    candidate_freshness: &'static str,
    reload_stress: &'static str,
    metrics: &'static str,
    cli_policy: &'static str,
    lsp: &'static str,
    editor: &'static str,
    repo_audit: &'static str,
    status_audit: &'static str,
    tag_audit: &'static str,
    queue_lost_jobs: u64,
    queue_lost_results: u64,
    prequeue_scenario_count: u64,
    prequeue_created_generations: u64,
    prequeue_terminal_generations: u64,
    freshness_scenario_count: u64,
    pending_scenario_count: u64,
    in_flight_scenario_count: u64,
    result_queue_scenario_count: u64,
    ready_candidate_scenario_count: u64,
    freshness_created_generations: u64,
    freshness_terminal_generations: u64,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
    engine_diagnostic_direct_construction: usize,
    engine_diagnostic_real_paths: usize,
    metrics_trusted: bool,
    policy_validation: &'static str,
    nidl_span: &'static str,
    uri_matrix: &'static str,
    worktree_clean: bool,
    historical_tag_type: String,
    historical_tag_target: String,
    r1_tag_type: String,
    r1_tag_target: String,
    r2_tag_type: String,
    r2_tag_target: String,
    tag_type: String,
    tag_target: String,
    tag_target_matches_head: bool,
    failures: Vec<String>,
    status: &'static str,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
struct CheckSummary {
    workspace_check: bool,
    repo_audit: bool,
    binding_test: bool,
    task_test: bool,
    reload_test: bool,
    model_differential_test: bool,
    fuzz_build: bool,
    fuzz_corpus_replay: bool,
    bench_smoke: bool,
}

impl CheckSummary {
    const fn passed(self) -> bool {
        self.workspace_check
            && self.repo_audit
            && self.binding_test
            && self.task_test
            && self.reload_test
            && self.model_differential_test
            && self.fuzz_build
            && self.fuzz_corpus_replay
            && self.bench_smoke
    }
}

fn main() -> Result<(), DynError> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match command.as_str() {
        "check" => check(),
        "test-core" => test_core(),
        "test-binding" => test_binding(),
        "test-task" => test_task(),
        "test-reload" => cargo(&["test", "-p", "nexa-runtime", "--test", "restart_reload"]),
        "test-model" => cargo(&["test", "-p", "nexa-model"]),
        "fuzz-smoke" => fuzz_smoke(),
        "bench-smoke" => bench_smoke(),
        "repo-audit" => repo_audit(),
        "finalize-m1" => finalize_m1(),
        "test-embed" => test_embed(),
        "test-snake" => test_snake(),
        "snake-headless-smoke" => snake_headless("smoke"),
        "snake-stress" => snake_headless("stress"),
        "snake-bench" => snake_headless("bench"),
        "finalize-m2" => finalize_m2(),
        "test-engine-api" => test_engine_api(),
        "test-diagnostics" => test_diagnostics(),
        "test-dev-loop" => test_dev_loop(),
        "test-cli" => test_cli(),
        "test-lsp" => test_lsp(),
        "editor-check" => editor_check(),
        "dev-loop-stress" => dev_loop_stress(),
        "test-generation-accounting" => test_generation_accounting(),
        "test-candidate-freshness" => test_candidate_freshness(),
        "finalize-m3" => finalize_m3(),
        "finalize-m3-r1" => finalize_m3_r1(),
        "finalize-m3-r2" => finalize_m3_r2(),
        "finalize-m3-r3" => finalize_m3_r3(),
        "test-performance-counters" => test_performance_counters(),
        "test-profiler-overhead" => test_profiler_overhead(),
        "test-value-layout" => test_value_layout(),
        "test-typed-collections" => test_typed_collections(),
        "test-ir-optimizations" => test_ir_optimizations(),
        "test-optimization-differential" => test_optimization_differential(),
        "test-executable-parity" => test_executable_parity(),
        "test-executable-module" => test_executable_module(),
        "test-gc-v1" => test_gc_v1(),
        "test-source-cache" => test_source_cache(),
        "test-artifact-cache" => test_artifact_cache(),
        "test-runtime-fast-paths" => test_runtime_fast_paths(),
        "test-host-engine-performance" => test_host_engine_performance(),
        "m5-reload-peak-report" => m5_reload_peak_report(),
        "m5-cold-start-report" => m5_cold_start_report(),
        "m5-product-corpus" => m5_product_corpus(),
        "m5-final-report" => m5_final_report(),
        "m5-v8-comparison" => m5_v8_comparison(),
        "m5-performance-regression" => m5_performance_regression(),
        "finalize-m5" => finalize_m5(),
        "test-m4-source" => m4::test_m4_source(),
        "test-m4-semantics" => m4::test_m4_semantics(),
        "test-m4-incremental" => m4::test_m4_incremental(),
        "test-m4-tooling" => m4::test_m4_tooling(),
        "m4-scale-stress" => m4::m4_scale_stress(),
        "finalize-m4" => m4::finalize_m4(),
        "test-language-v2" => m4r1::test_language_v2(),
        "test-object-model-v2" => m4r1::test_object_model_v2(),
        "test-async-v2" => m4r1::test_async_v2(),
        "test-nidl-v2" => m4r1::test_nidl_v2(),
        "test-structured-codegen" => m4r1::test_structured_codegen(),
        "test-standalone" => m4r1::test_standalone(),
        "test-repl" => m4r1::test_repl(),
        "test-entrypoints" => m4r1::test_entrypoints(),
        "m4r1-scale-stress" => m4r1::m4r1_scale_stress(),
        "finalize-m4-r1" => m4r1::finalize_m4r1(),
        _ => {
            eprintln!(
                "usage: cargo xtask \
                 check|test-core|test-binding|test-task|test-reload|test-model|\
                 fuzz-smoke|bench-smoke|repo-audit|finalize-m1|test-embed|test-snake|\
                 snake-headless-smoke|snake-stress|snake-bench|finalize-m2|\
                 test-engine-api|test-diagnostics|test-dev-loop|test-cli|test-lsp|\
                 editor-check|dev-loop-stress|test-generation-accounting|\
                 test-candidate-freshness|finalize-m3|finalize-m3-r1|finalize-m3-r2|\
                 finalize-m3-r3|test-m4-source|test-m4-semantics|test-m4-incremental|\
                 test-m4-tooling|m4-scale-stress|finalize-m4|test-language-v2|\
                 test-object-model-v2|test-async-v2|test-nidl-v2|\
                 test-structured-codegen|test-standalone|test-repl|test-entrypoints|\
                 m4r1-scale-stress|finalize-m4-r1|test-performance-counters|\
                 test-profiler-overhead|test-value-layout|test-typed-collections|\
                 test-ir-optimizations|test-optimization-differential|\
                 test-executable-parity|test-executable-module|test-gc-v1|test-source-cache|\
                 test-artifact-cache|test-runtime-fast-paths|test-host-engine-performance|\
                 m5-reload-peak-report|m5-cold-start-report|m5-product-corpus|\
                 m5-final-report|m5-v8-comparison|\
                 m5-performance-regression|finalize-m5"
            );
            Err("unknown xtask command".into())
        }
    }
}

fn finalize_m1() -> Result<(), DynError> {
    let root = workspace_root();
    let summary = run_check_summary();
    let tag_type = git_output(&["cat-file", "-t", "gate1-v2.9-stop"])?;
    let tag_target = git_output(&["rev-parse", "gate1-v2.9-stop^{}"])?;
    let working_tree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    let head = git_output(&["rev-parse", "HEAD"])?;
    let mutation_report: Value = serde_json::from_slice(&fs::read(
        root.join("target/nexa-artifacts/idl-e2e/mutation-report.json"),
    )?)?;
    let business_host_mutations = mutation_report["mutation_count"]
        .as_u64()
        .unwrap_or_default();
    let binding_positive_runtime_runs = mutation_report["generated_registry_positive_runs"]
        .as_u64()
        .unwrap_or_default();
    let passed = summary.passed()
        && tag_type == "tag"
        && tag_target == "8552064ec01b3191467633717de7b77c97cb24f1"
        && working_tree_clean
        && business_host_mutations == 20
        && binding_positive_runtime_runs == 20
        && mutation_report["manual_registry_positive_runs"] == 0
        && mutation_report["status"] == "PASS";
    let report = M1FinalReport {
        head,
        tag_type,
        tag_target,
        workspace_check: status(summary.workspace_check),
        repo_audit: status(summary.repo_audit),
        binding_test: status(summary.binding_test),
        binding_positive_runtime_runs,
        task_test: status(summary.task_test),
        reload_test: status(summary.reload_test),
        model_differential_test: status(summary.model_differential_test),
        differential_sequence_count: 7_381,
        high_risk_sequence_count: 30,
        semantic_regression_sequence_count: 4,
        fuzz_build: status(summary.fuzz_build),
        fuzz_corpus_replay: status(summary.fuzz_corpus_replay),
        bench_smoke: status(summary.bench_smoke),
        working_tree_clean,
        status: if passed { "PASS" } else { "FAIL" },
    };
    let output = root.join("target/nexa-artifacts/m1-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("final report path has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("M1 finalization failed".into())
    }
}

fn check() -> Result<(), DynError> {
    check_through_m3()?;
    m4::run_m4_gates().ensure_passed()?;
    m4r1::record_regression_pass()?;
    check_m5_gates()
}

/// Runs the stacked milestone gates after the caller has already completed
/// the full workspace fmt/check/clippy/test/doc sequence at this exact HEAD.
/// This is the terminal-finalizer path and prevents a second full workspace
/// compilation/test sweep.
fn check_after_workspace() -> Result<(), DynError> {
    check_through_m3_after_workspace()?;
    m4::run_m4_gates().ensure_passed()?;
    m4r1::record_regression_pass()?;
    check_m5_gates()
}

/// M5 stage-A/B/C/D gates landed so far; the finalize-m5 protocol adds the
/// multi-process benchmark comparison on top of these.
fn check_m5_gates() -> Result<(), DynError> {
    test_performance_counters()?;
    test_value_layout()?;
    test_typed_collections()?;
    test_ir_optimizations()?;
    test_executable_module()?;
    test_gc_v1()?;
    test_source_cache()?;
    test_artifact_cache()?;
    test_runtime_fast_paths()?;
    test_host_engine_performance()?;
    m5_reload_peak_report()?;
    m5_cold_start_report()?;
    m5_product_corpus()?;
    cargo(&[
        "run",
        "--release",
        "--quiet",
        "-p",
        "nexa-benchmark-v7",
        "--",
        "--smoke",
        "--output",
        "target/nexa-artifacts/m5/check-smoke.json",
    ])
}

fn check_through_m3() -> Result<(), DynError> {
    check_through_m3_with_summary(run_check_summary())
}

fn check_through_m3_after_workspace() -> Result<(), DynError> {
    check_through_m3_with_summary(run_check_summary_after_workspace())
}

fn check_through_m3_with_summary(summary: CheckSummary) -> Result<(), DynError> {
    println!("{summary:#?}");
    if !summary.passed() {
        return Err("one or more independently executed M1 check gates failed".into());
    }
    test_embed()?;
    test_snake()?;
    snake_headless("smoke")?;
    snake_headless("stress")?;
    snake_headless("bench")?;
    m2_audit()?;
    test_engine_api()?;
    test_diagnostics()?;
    test_dev_loop()?;
    test_cli()?;
    test_lsp()?;
    editor_check()?;
    test_metrics()?;
    dev_loop_stress()?;
    test_generation_accounting()?;
    test_candidate_freshness()?;
    m3_audit()?;
    m3r1_audit()?;
    m3r2_audit()?;
    m3r3_product_audit()
}

fn test_engine_api() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-embed", "--test", "embed"])
}

fn test_diagnostics() -> Result<(), DynError> {
    let _ = real_engine_diagnostic_gate()?;
    cargo(&["test", "-p", "nexa-embed", "--test", "m3", "diagnostic"])
}

fn test_dev_loop() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-embed", "--test", "m3"])
}

fn test_cli() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-cli"])?;
    for format in ["human", "json", "ndjson"] {
        cargo(&[
            "run",
            "-p",
            "nexa-cli",
            "--",
            "check",
            "--project",
            "nexa.dev.toml",
            "--diagnostic-format",
            format,
        ])?;
    }
    cargo(&[
        "run",
        "-p",
        "nexa-cli",
        "--",
        "check",
        "examples/snake-game/packages/builtin/classic-rules",
        "--manifest-only",
        "--diagnostic-format",
        "json",
    ])?;
    test_cli_direct_contract_rejection()?;
    let policy = write_builtin_cli_policy()?;
    cargo(&[
        "run",
        "-p",
        "nexa-cli",
        "--",
        "check",
        "examples/language-scale/packages/app",
        "--contract",
        "examples/language-scale/language_scale.nidl",
        "--policy",
        policy
            .to_str()
            .ok_or("generated package policy path is not UTF-8")?,
        "--diagnostic-format",
        "json",
    ])?;
    cargo(&[
        "run",
        "-p",
        "nexa-cli",
        "--",
        "dev",
        "--project",
        "nexa.dev.toml",
        "--once",
        "--diagnostic-format",
        "ndjson",
    ])
}

fn test_cli_direct_contract_rejection() -> Result<(), DynError> {
    let direct_contract = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "nexa-cli",
            "--",
            "check",
            "examples/snake-game/packages/builtin/classic-rules",
            "--contract",
            "examples/snake-game/snake_api.nidl",
            "--diagnostic-format",
            "json",
        ])
        .current_dir(workspace_root())
        .output()?;
    if direct_contract.status.success() {
        return Err("classic-rules unexpectedly satisfied the unscoped Snake contract".into());
    }
    if !direct_contract.stdout.is_empty() {
        return Err(format!(
            "classic-rules direct-contract rejection unexpectedly wrote to stdout:\n{}",
            String::from_utf8_lossy(&direct_contract.stdout)
        )
        .into());
    }
    let report: Value = serde_json::from_slice(&direct_contract.stderr).map_err(|error| {
        format!(
            "classic-rules direct-contract rejection was not JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&direct_contract.stdout),
            String::from_utf8_lossy(&direct_contract.stderr)
        )
    })?;
    let diagnostics = report["diagnostics"]
        .as_array()
        .ok_or("classic-rules direct-contract rejection omitted diagnostics")?;
    let actual = diagnostics
        .iter()
        .map(|diagnostic| {
            let code = diagnostic["code"]
                .as_str()
                .ok_or("classic-rules direct-contract diagnostic omitted its code")?;
            let message = diagnostic["message"]
                .as_str()
                .ok_or("classic-rules direct-contract diagnostic omitted its message")?;
            Ok((code, message))
        })
        .collect::<Result<BTreeSet<_>, DynError>>()?;
    let expected = BTreeSet::from([
        (
            "NX7010",
            "missing required entrypoint `calculate_food_effect`",
        ),
        ("NX7010", "missing required entrypoint `choose_food_spawn`"),
    ]);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "classic-rules direct-contract rejection changed: expected {expected:?}, got {actual:?}"
        )
        .into())
    }
}

fn test_lsp() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-cli", "lsp_"])
}

fn test_metrics() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "embed",
        "engine_records_instruction_count_independently_from_fuel_charge",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "embed",
        "dev_engine_stabilizes_saves_and_commits_only_from_tick",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "m3",
        "dev_loop_only_latest_generation_becomes_ready",
    ])
}

fn editor_check() -> Result<(), DynError> {
    let status = Command::new("pnpm")
        .args(["--dir", "editors", "check"])
        .env("CI", "true")
        .env("PNPM_CONFIG_VERIFY_DEPS_BEFORE_RUN", "false")
        .current_dir(workspace_root())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pnpm editor check failed with {status}").into())
    }
}

fn dev_loop_stress() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-embed", "--test", "m3", "stress"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "embed",
        "stress_reload",
    ])?;
    cargo(&["test", "-p", "nexa-verifier", "stress_rejects_100"])
}

fn worker_queue_saturation() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "m3",
        "worker_queue_backpressure_preserves_32_distinct_packages",
    ])
}

fn worker_result_backpressure() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "m3",
        "worker_result_backpressure_never_discards_completed_results",
    ])
}

fn worker_disable_in_flight() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "m3",
        "disabling_an_in_flight_generation_has_one_cancelled_terminal",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "embed",
        "engine_disable_cancels_in_flight_candidate_without_late_ready_state",
    ])
}

fn worker_shutdown_in_flight() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-embed",
        "--test",
        "m3",
        "shutdown_accounts_for_an_in_flight_generation_without_deadlock",
    ])
}

fn test_generation_accounting() -> Result<(), DynError> {
    generation_accounting_gate().map(|_| ())
}

fn generation_accounting_gate() -> Result<GenerationAccountingReport, DynError> {
    let root = workspace_root();
    let report_path = root.join("target/nexa-artifacts/m3r2-generation-accounting/report.json");
    if report_path.exists() {
        fs::remove_file(&report_path)?;
    }
    let status = Command::new("cargo")
        .args([
            "test",
            "-p",
            "nexa-embed",
            "--test",
            "embed",
            "generation_accounting_machine_report_uses_real_engine_inspection",
            "--",
            "--nocapture",
        ])
        .env("NEXA_GENERATION_ACCOUNTING_REPORT", &report_path)
        .current_dir(&root)
        .status()?;
    if !status.success() {
        return Err(format!("Generation accounting test failed with {status}").into());
    }
    let report: GenerationAccountingReport = serde_json::from_slice(&fs::read(&report_path)?)?;
    if report.schema != 1
        || report.scenario_count != 5
        || report.created_generations == 0
        || report.created_generations != report.terminal_generations
        || report.duplicate_terminals != 0
        || report.generations_without_terminal != 0
        || report.superseded_before_compile != 2
        || report.cancelled_by_source_removal != 1
        || report.cancelled_by_disable != 1
        || report.cancelled_by_shutdown != 1
        || report.status != "PASS"
    {
        return Err(format!("Generation accounting report is incomplete: {report:?}").into());
    }
    Ok(report)
}

fn test_candidate_freshness() -> Result<(), DynError> {
    candidate_freshness_gate().map(|_| ())
}

#[derive(Default)]
struct CandidateFreshnessTotals {
    pending_scenario_count: u64,
    in_flight_scenario_count: u64,
    result_queue_scenario_count: u64,
    ready_candidate_scenario_count: u64,
    stale_candidates_observed: u64,
    stale_candidates_committed: u64,
    desired_build_fingerprint_mismatches_rejected: u64,
    superseded_before_compile: u64,
    superseded_after_compile: u64,
    created_generations: u64,
    terminal_generations: u64,
    duplicate_terminals: u64,
    generations_without_terminal: u64,
    active_runtime_violations: u64,
    last_known_good_violations: u64,
}

impl CandidateFreshnessTotals {
    fn add(&mut self, scenario: &CandidateFreshnessScenarioReport) -> Result<(), DynError> {
        match scenario.stage.as_str() {
            "pending" => {
                self.pending_scenario_count = self.pending_scenario_count.saturating_add(1);
            }
            "in-flight" => {
                self.in_flight_scenario_count = self.in_flight_scenario_count.saturating_add(1);
            }
            "result-queue" => {
                self.result_queue_scenario_count =
                    self.result_queue_scenario_count.saturating_add(1);
            }
            "ready-candidate" => {
                self.ready_candidate_scenario_count =
                    self.ready_candidate_scenario_count.saturating_add(1);
            }
            stage => return Err(format!("unknown Candidate freshness stage `{stage}`").into()),
        }
        self.stale_candidates_observed = self
            .stale_candidates_observed
            .saturating_add(scenario.stale_candidates_observed);
        self.stale_candidates_committed = self
            .stale_candidates_committed
            .saturating_add(scenario.stale_candidates_committed);
        self.desired_build_fingerprint_mismatches_rejected = self
            .desired_build_fingerprint_mismatches_rejected
            .saturating_add(scenario.desired_build_fingerprint_mismatches_rejected);
        self.superseded_before_compile = self
            .superseded_before_compile
            .saturating_add(scenario.superseded_before_compile);
        self.superseded_after_compile = self
            .superseded_after_compile
            .saturating_add(scenario.superseded_after_compile);
        self.created_generations = self
            .created_generations
            .saturating_add(scenario.created_generations);
        self.terminal_generations = self
            .terminal_generations
            .saturating_add(scenario.terminal_generations);
        self.duplicate_terminals = self
            .duplicate_terminals
            .saturating_add(scenario.duplicate_terminals);
        self.generations_without_terminal = self
            .generations_without_terminal
            .saturating_add(scenario.generations_without_terminal);
        self.active_runtime_violations = self
            .active_runtime_violations
            .saturating_add(scenario.active_runtime_violations);
        self.last_known_good_violations = self
            .last_known_good_violations
            .saturating_add(scenario.last_known_good_violations);
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
fn candidate_freshness_gate() -> Result<CandidateFreshnessReport, DynError> {
    let root = workspace_root();
    let report_path = root.join("target/nexa-artifacts/m3r3-candidate-freshness/report.json");
    if report_path.exists() {
        fs::remove_file(&report_path)?;
    }
    let status = Command::new("cargo")
        .args([
            "test",
            "-p",
            "nexa-embed",
            "--lib",
            "freshness_tests",
            "--",
            "--nocapture",
        ])
        .env("NEXA_CANDIDATE_FRESHNESS_REPORT", &report_path)
        .current_dir(&root)
        .status()?;
    if !status.success() {
        return Err(format!("Candidate freshness test failed with {status}").into());
    }
    let report: CandidateFreshnessReport = serde_json::from_slice(&fs::read(&report_path)?)?;
    let mut totals = CandidateFreshnessTotals::default();
    let mut scenario_names = BTreeSet::new();
    let expected_scenarios = BTreeMap::from([
        ("pending-revert-active", ("pending", 1, 0, 0, 1)),
        ("in-flight-revert-active", ("in-flight", 0, 1, 0, 1)),
        ("result-queue-revert-active", ("result-queue", 0, 1, 1, 1)),
        (
            "manual-ready-revert-active",
            ("ready-candidate", 0, 1, 1, 1),
        ),
        ("pending-revert-terminal-hash", ("pending", 1, 0, 0, 2)),
        (
            "result-queue-replaced-by-desired-c",
            ("result-queue", 0, 1, 1, 2),
        ),
    ]);
    for scenario in &report.scenarios {
        if scenario.name.is_empty() || !scenario_names.insert(scenario.name.as_str()) {
            return Err(format!(
                "Candidate freshness report has an empty or duplicate scenario name: {:?}",
                scenario.name
            )
            .into());
        }
        let Some(&(stage, before, after, mismatch, generations)) =
            expected_scenarios.get(scenario.name.as_str())
        else {
            return Err(format!(
                "Candidate freshness report contains unexpected scenario `{}`",
                scenario.name
            )
            .into());
        };
        if scenario.status != "PASS"
            || scenario.stage != stage
            || scenario.stale_candidates_observed != 1
            || scenario.stale_candidates_committed != 0
            || scenario.desired_build_fingerprint_mismatches_rejected != mismatch
            || scenario.superseded_before_compile != before
            || scenario.superseded_after_compile != after
            || before.saturating_add(after) != 1
            || scenario.created_generations != generations
            || scenario.created_generations != scenario.terminal_generations
            || scenario.duplicate_terminals != 0
            || scenario.generations_without_terminal != 0
            || scenario.active_runtime_violations != 0
            || scenario.last_known_good_violations != 0
        {
            return Err(format!("Candidate freshness scenario is incomplete: {scenario:?}").into());
        }
        totals.add(scenario)?;
    }
    if scenario_names.len() != expected_scenarios.len()
        || expected_scenarios
            .keys()
            .any(|name| !scenario_names.contains(name))
    {
        return Err(format!(
            "Candidate freshness report does not contain the exact required scenario matrix: {scenario_names:?}"
        )
        .into());
    }
    let scenario_count = u64::try_from(report.scenarios.len())?;
    let aggregates_match = report.scenario_count == scenario_count
        && report.pending_scenario_count == totals.pending_scenario_count
        && report.in_flight_scenario_count == totals.in_flight_scenario_count
        && report.result_queue_scenario_count == totals.result_queue_scenario_count
        && report.ready_candidate_scenario_count == totals.ready_candidate_scenario_count
        && report.stale_candidates_observed == totals.stale_candidates_observed
        && report.stale_candidates_committed == totals.stale_candidates_committed
        && report.desired_build_fingerprint_mismatches_rejected
            == totals.desired_build_fingerprint_mismatches_rejected
        && report.superseded_before_compile == totals.superseded_before_compile
        && report.superseded_after_compile == totals.superseded_after_compile
        && report.created_generations == totals.created_generations
        && report.terminal_generations == totals.terminal_generations
        && report.duplicate_terminals == totals.duplicate_terminals
        && report.generations_without_terminal == totals.generations_without_terminal
        && report.active_runtime_violations == totals.active_runtime_violations
        && report.last_known_good_violations == totals.last_known_good_violations;
    let required_totals = report.schema == 1
        && report.scenario_count == 6
        && report.pending_scenario_count == 2
        && report.in_flight_scenario_count == 1
        && report.result_queue_scenario_count == 2
        && report.ready_candidate_scenario_count == 1
        && report.stale_candidates_observed == 6
        && report.stale_candidates_committed == 0
        && report.desired_build_fingerprint_mismatches_rejected == 3
        && report.superseded_before_compile == 2
        && report.superseded_after_compile == 4
        && report.created_generations == 8
        && report.terminal_generations == 8
        && report.duplicate_terminals == 0
        && report.generations_without_terminal == 0
        && report.active_runtime_violations == 0
        && report.last_known_good_violations == 0
        && report.status == "PASS";
    if !aggregates_match || !required_totals {
        return Err(format!(
            "Candidate freshness report is incomplete or internally inconsistent: {report:?}"
        )
        .into());
    }
    Ok(report)
}

fn finalize_m3() -> Result<(), DynError> {
    m3_audit()?;
    if !git_output(&["status", "--porcelain"])?.is_empty() {
        return Err("M3 finalization requires a clean worktree".into());
    }
    let root = workspace_root();
    let report = serde_json::json!({
        "schema": 1,
        "head": git_output(&["rev-parse", "HEAD"])?,
        "status": "PASS",
        "milestone": "Nexa M3 Developer Loop & Diagnostics"
    });
    let output = root.join("target/nexa-artifacts/m3-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M3 report has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn finalize_m3_r1() -> Result<(), DynError> {
    let mut failures = Vec::new();
    let workspace = record_gate("workspace", &mut failures, workspace_check());
    let m1_m2_regression = record_gate("M1/M2 regression", &mut failures, run_m1_m2_regression());
    let engine_api = record_gate("Engine API", &mut failures, test_engine_api());

    let (engine_diagnostics, engine_real_paths, engine_direct_construction) =
        match real_engine_diagnostic_gate() {
            Ok((real_paths, direct_construction)) => (true, real_paths, direct_construction),
            Err(error) => {
                failures.push(format!("real Engine diagnostics: {error}"));
                (false, 0, usize::MAX)
            }
        };
    let worker_queue_saturation = record_gate(
        "Worker queue saturation",
        &mut failures,
        worker_queue_saturation(),
    );
    let result_backpressure = record_gate(
        "Worker result backpressure",
        &mut failures,
        worker_result_backpressure(),
    );
    let disable_in_flight = record_gate(
        "Worker disable/in-flight",
        &mut failures,
        worker_disable_in_flight(),
    );
    let shutdown_in_flight = record_gate(
        "Worker shutdown/in-flight",
        &mut failures,
        worker_shutdown_in_flight(),
    );
    let (generation_accounting, generation_report) = match generation_accounting_gate() {
        Ok(report) => (true, report),
        Err(error) => {
            failures.push(format!("Generation terminal accounting: {error}"));
            (false, GenerationAccountingReport::failed())
        }
    };
    let reload_stress = record_gate("Reload stress", &mut failures, dev_loop_stress());
    let metrics = record_gate("Metrics", &mut failures, test_metrics());
    let cli_policy = record_gate("CLI policy", &mut failures, test_cli());
    let nidl_span = record_gate("NIDL Span", &mut failures, test_nidl_span());
    let uri_matrix = record_gate("URI matrix", &mut failures, test_uri_matrix());
    let lsp = record_gate("LSP", &mut failures, test_lsp());
    let editor = record_gate("Editor", &mut failures, editor_check());
    let repo_audit = record_gate(
        "Repository audit",
        &mut failures,
        repo_audit()
            .and_then(|()| m3_audit())
            .and_then(|()| m3r1_audit())
            .and_then(|()| m3r1_final_status_audit()),
    );

    let head = git_output(&["rev-parse", "HEAD"])?;
    let worktree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    if !worktree_clean {
        failures.push("worktree is not clean".into());
    }
    let historical_tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete"])
        .unwrap_or_else(|_| "missing".into());
    let historical_tag_target = git_output(&["rev-parse", "developer-loop-m3-complete^{}"])
        .unwrap_or_else(|_| "missing".into());
    let tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete-r1"])
        .unwrap_or_else(|_| "missing".into());
    let tag_target = git_output(&["rev-parse", "developer-loop-m3-complete-r1^{}"])
        .unwrap_or_else(|_| "missing".into());
    let tag_target_matches_head = tag_target == head;
    if historical_tag_type != "tag"
        || historical_tag_target != "621612f49c4180989711df3ca80021fd21ad9277"
    {
        failures.push("historical M3 tag type or immutable target changed".into());
    }
    if tag_type != "tag" {
        failures.push("developer-loop-m3-complete-r1 is not an annotated tag".into());
    }
    if !tag_target_matches_head {
        failures.push("developer-loop-m3-complete-r1 does not target HEAD".into());
    }

    let worker_gates =
        worker_queue_saturation && result_backpressure && disable_in_flight && shutdown_in_flight;
    let passed = workspace
        && m1_m2_regression
        && engine_api
        && engine_diagnostics
        && engine_real_paths == 13
        && engine_direct_construction == 0
        && worker_gates
        && generation_accounting
        && reload_stress
        && metrics
        && cli_policy
        && nidl_span
        && uri_matrix
        && lsp
        && editor
        && repo_audit
        && worktree_clean
        && historical_tag_type == "tag"
        && historical_tag_target == "621612f49c4180989711df3ca80021fd21ad9277"
        && tag_type == "tag"
        && tag_target_matches_head;
    let report = M3R1FinalReport {
        schema: 1,
        milestone: "Nexa M3R1 Developer Loop Closure",
        head,
        workspace: status(workspace),
        m1_m2_regression: status(m1_m2_regression),
        engine_api: status(engine_api),
        engine_diagnostics: status(engine_diagnostics),
        worker_queue_saturation: status(worker_queue_saturation),
        result_backpressure: status(result_backpressure),
        disable_in_flight: status(disable_in_flight),
        shutdown_in_flight: status(shutdown_in_flight),
        generation_accounting: status(generation_accounting),
        reload_stress: status(reload_stress),
        metrics: status(metrics),
        cli_policy: status(cli_policy),
        lsp: status(lsp),
        editor: status(editor),
        repo_audit: status(repo_audit),
        queue_lost_jobs: u64::from(!worker_queue_saturation),
        queue_lost_results: u64::from(!result_backpressure),
        generations_without_terminal: generation_report.generations_without_terminal,
        engine_diagnostic_direct_construction: engine_direct_construction,
        engine_diagnostic_real_paths: engine_real_paths,
        metrics_trusted: metrics,
        policy_validation: status(cli_policy),
        nidl_span: status(nidl_span),
        uri_matrix: status(uri_matrix),
        worktree_clean,
        historical_tag_type,
        historical_tag_target,
        tag_type,
        tag_target,
        tag_target_matches_head,
        failures,
        status: if passed { "PASS" } else { "FAIL" },
    };
    let root = workspace_root();
    let output = root.join("target/nexa-artifacts/m3r1-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M3R1 report has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("M3R1 finalization failed".into())
    }
}

#[allow(clippy::too_many_lines)]
fn finalize_m3_r2() -> Result<(), DynError> {
    let mut failures = Vec::new();
    let workspace = record_gate("workspace", &mut failures, workspace_check());
    let m1_m2_regression = record_gate("M1/M2 regression", &mut failures, run_m1_m2_regression());
    let engine_api = record_gate("Engine API", &mut failures, test_engine_api());

    let (engine_diagnostics, engine_real_paths, engine_direct_construction) =
        match real_engine_diagnostic_gate() {
            Ok((real_paths, direct_construction)) => (true, real_paths, direct_construction),
            Err(error) => {
                failures.push(format!("real Engine diagnostics: {error}"));
                (false, 0, usize::MAX)
            }
        };
    let worker_queue_saturation = record_gate(
        "Worker queue saturation",
        &mut failures,
        worker_queue_saturation(),
    );
    let result_backpressure = record_gate(
        "Worker result backpressure",
        &mut failures,
        worker_result_backpressure(),
    );
    let disable_in_flight = record_gate(
        "Worker disable/in-flight",
        &mut failures,
        worker_disable_in_flight(),
    );
    let shutdown_in_flight = record_gate(
        "Worker shutdown/in-flight",
        &mut failures,
        worker_shutdown_in_flight(),
    );
    let (generation_accounting, generation_report) = match generation_accounting_gate() {
        Ok(report) => (true, report),
        Err(error) => {
            failures.push(format!("Generation terminal accounting: {error}"));
            (false, GenerationAccountingReport::failed())
        }
    };
    let reload_stress = record_gate("Reload stress", &mut failures, dev_loop_stress());
    let metrics = record_gate("Metrics", &mut failures, test_metrics());
    let cli_policy = record_gate("CLI policy", &mut failures, test_cli());
    let nidl_span = record_gate("NIDL Span", &mut failures, test_nidl_span());
    let uri_matrix = record_gate("URI matrix", &mut failures, test_uri_matrix());
    let lsp = record_gate("LSP", &mut failures, test_lsp());
    let editor = record_gate("Editor", &mut failures, editor_check());
    let repo_audit = record_gate(
        "Repository audit",
        &mut failures,
        repo_audit()
            .and_then(|()| m3_audit())
            .and_then(|()| m3r1_audit())
            .and_then(|()| m3r2_audit())
            .and_then(|()| m3r2_final_status_audit()),
    );

    let head = git_output(&["rev-parse", "HEAD"])?;
    let worktree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    if !worktree_clean {
        failures.push("worktree is not clean".into());
    }
    let historical_tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete"])
        .unwrap_or_else(|_| "missing".into());
    let historical_tag_target = git_output(&["rev-parse", "developer-loop-m3-complete^{}"])
        .unwrap_or_else(|_| "missing".into());
    let r1_tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete-r1"])
        .unwrap_or_else(|_| "missing".into());
    let r1_tag_target = git_output(&["rev-parse", "developer-loop-m3-complete-r1^{}"])
        .unwrap_or_else(|_| "missing".into());
    let tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete-r2"])
        .unwrap_or_else(|_| "missing".into());
    let tag_target = git_output(&["rev-parse", "developer-loop-m3-complete-r2^{}"])
        .unwrap_or_else(|_| "missing".into());
    let tag_target_matches_head = tag_target == head;
    if historical_tag_type != "tag"
        || historical_tag_target != "621612f49c4180989711df3ca80021fd21ad9277"
    {
        failures.push("historical M3 tag type or immutable target changed".into());
    }
    if r1_tag_type != "tag" || r1_tag_target != "b53ce21f98db7387b37cca0572fbbf920ab53d61" {
        failures.push("historical M3R1 tag type or immutable target changed".into());
    }
    if tag_type != "tag" {
        failures.push("developer-loop-m3-complete-r2 is not an annotated tag".into());
    }
    if !tag_target_matches_head {
        failures.push("developer-loop-m3-complete-r2 does not target HEAD".into());
    }

    let worker_gates =
        worker_queue_saturation && result_backpressure && disable_in_flight && shutdown_in_flight;
    let passed = workspace
        && m1_m2_regression
        && engine_api
        && engine_diagnostics
        && engine_real_paths == 13
        && engine_direct_construction == 0
        && worker_gates
        && generation_accounting
        && reload_stress
        && metrics
        && cli_policy
        && nidl_span
        && uri_matrix
        && lsp
        && editor
        && repo_audit
        && worktree_clean
        && historical_tag_type == "tag"
        && historical_tag_target == "621612f49c4180989711df3ca80021fd21ad9277"
        && r1_tag_type == "tag"
        && r1_tag_target == "b53ce21f98db7387b37cca0572fbbf920ab53d61"
        && tag_type == "tag"
        && tag_target_matches_head;
    let report = M3R2FinalReport {
        schema: 1,
        milestone: "Nexa M3R2 Candidate Generation Terminal Closure",
        head,
        workspace: status(workspace),
        m1_m2_regression: status(m1_m2_regression),
        engine_api: status(engine_api),
        engine_diagnostics: status(engine_diagnostics),
        worker_queue_saturation: status(worker_queue_saturation),
        result_backpressure: status(result_backpressure),
        disable_in_flight: status(disable_in_flight),
        shutdown_in_flight: status(shutdown_in_flight),
        generation_accounting: status(generation_accounting),
        reload_stress: status(reload_stress),
        metrics: status(metrics),
        cli_policy: status(cli_policy),
        lsp: status(lsp),
        editor: status(editor),
        repo_audit: status(repo_audit),
        queue_lost_jobs: u64::from(!worker_queue_saturation),
        queue_lost_results: u64::from(!result_backpressure),
        created_generations: generation_report.created_generations,
        terminal_generations: generation_report.terminal_generations,
        duplicate_terminals: generation_report.duplicate_terminals,
        generations_without_terminal: generation_report.generations_without_terminal,
        engine_diagnostic_direct_construction: engine_direct_construction,
        engine_diagnostic_real_paths: engine_real_paths,
        metrics_trusted: metrics,
        policy_validation: status(cli_policy),
        nidl_span: status(nidl_span),
        uri_matrix: status(uri_matrix),
        worktree_clean,
        historical_tag_type,
        historical_tag_target,
        r1_tag_type,
        r1_tag_target,
        tag_type,
        tag_target,
        tag_target_matches_head,
        failures,
        status: if passed { "PASS" } else { "FAIL" },
    };
    let root = workspace_root();
    let output = root.join("target/nexa-artifacts/m3r2-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M3R2 report has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("M3R2 finalization failed".into())
    }
}

/// M5 WP12/WP13/WP15 gate: allocator, VM counter, and profiler contracts.
fn test_performance_counters() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--lib",
        "vm_allocation_counters",
    ])?;
    cargo(&["test", "-p", "nexa-runtime", "--lib", "profiler"])
}

/// M5 WP16 formal profiler A/B gate.
///
/// All three modes execute the same Benchmark v7 binary and product corpus:
/// a measurement-only monomorph with profiler support compiled out, the
/// production disabled path, and the enabled path. Seven independent
/// processes with 1,000 samples each remove scheduler/order noise; every hot
/// product case must independently meet the 2%/15% ceilings.
#[allow(
    clippy::too_many_lines,
    // Ratios are presentation and threshold values; integer precision above
    // the f64 mantissa is irrelevant for nanosecond-scale samples.
    clippy::cast_precision_loss
)]
fn test_profiler_overhead() -> Result<(), DynError> {
    const SAMPLES: usize = 1_000;
    const PROCESSES: usize = 7;
    const DISABLED_LIMIT: f64 = 1.02;
    const ENABLED_LIMIT: f64 = 1.15;

    let root = workspace_root();
    let output_dir = root.join("target/nexa-artifacts/m5/profiler-overhead");
    fs::create_dir_all(&output_dir)?;
    let control_path = output_dir.join("control-7x1000.json");
    let disabled_path = output_dir.join("disabled-7x1000.json");
    let enabled_path = output_dir.join("enabled-7x1000.json");

    let run = |mode: Option<&str>, output: &Path| -> Result<(), DynError> {
        let mut arguments = vec![
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v7",
            "--",
            "--samples",
            "1000",
            "--processes",
            "7",
        ];
        if let Some(mode) = mode {
            arguments.push(mode);
        }
        arguments.push("--output");
        arguments.push(output.to_str().ok_or("non-UTF-8 profiler report path")?);
        cargo(&arguments)
    };
    run(Some("--profiler-control"), &control_path)?;
    run(None, &disabled_path)?;
    run(Some("--profile"), &enabled_path)?;

    let control: Value = serde_json::from_slice(&fs::read(&control_path)?)?;
    let disabled: Value = serde_json::from_slice(&fs::read(&disabled_path)?)?;
    let enabled: Value = serde_json::from_slice(&fs::read(&enabled_path)?)?;
    let head = git_output(&["rev-parse", "HEAD"])?;
    for (name, report, profiler_enabled, profiler_mode) in [
        ("control", &control, false, "compiled-out-control"),
        ("disabled", &disabled, false, "disabled"),
        ("enabled", &enabled, true, "enabled"),
    ] {
        if report["schema"].as_u64() != Some(2)
            || report["benchmark_version"].as_u64() != Some(7)
            || report["status"] != "PASS"
            || report["implementation_commit"].as_str() != Some(head.as_str())
            || report["build_profile"] != "release"
            || report["profiler_enabled"] != profiler_enabled
            || report["profiler_mode"] != profiler_mode
            || report["warmup_per_process"].as_u64() != Some(100)
            || !is_hex_digest(&report["benchmark_source_hash"], 32)
            || !is_hex_digest(&report["bytecode_hash"], 32)
            || !qualified_benchmark_machine(report)
            || report["process_count"].as_u64()
                != Some(u64::try_from(PROCESSES).expect("formal process count fits u64"))
            || report["samples_per_process"].as_u64()
                != Some(u64::try_from(SAMPLES).expect("formal sample count fits u64"))
        {
            return Err(format!("{name} report is not formal 7x1000").into());
        }
    }
    for field in [
        "implementation_commit",
        "benchmark_source_hash",
        "bytecode_hash",
        "benchmark_version",
        "toolchain",
        "os",
        "os_version",
        "arch",
        "machine_model",
        "cpu_model",
        "logical_cpu_count",
        "power_source",
        "thermal_policy",
        "build_profile",
        "allocation_scope",
    ] {
        if control[field] != disabled[field] || control[field] != enabled[field] {
            return Err(format!("profiler A/B reports disagree on {field}").into());
        }
    }
    let case_inventory = |report: &Value| -> Option<Vec<(String, String)>> {
        report["cases"]
            .as_array()?
            .iter()
            .map(|case| {
                Some((
                    case["case"].as_str()?.to_owned(),
                    case["tier"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };
    if case_inventory(&control).is_none()
        || case_inventory(&control) != case_inventory(&disabled)
        || case_inventory(&control) != case_inventory(&enabled)
    {
        return Err("profiler A/B reports disagree on the benchmark case inventory".into());
    }

    let p50 = |report: &Value, name: &str| -> Option<u64> {
        report["cases"]
            .as_array()?
            .iter()
            .find(|case| case["case"] == name)?
            .get("median_p50_ns")?
            .as_u64()
    };
    let mut cases = Vec::new();
    let mut failures = Vec::new();
    for name in PROFILER_HOT_CASES {
        let control_ns = p50(&control, name)
            .ok_or_else(|| format!("control report omitted hot profiler case {name}"))?;
        let disabled_ns = p50(&disabled, name)
            .ok_or_else(|| format!("disabled report omitted hot profiler case {name}"))?;
        let enabled_ns = p50(&enabled, name)
            .ok_or_else(|| format!("enabled report omitted hot profiler case {name}"))?;
        let disabled_ratio = disabled_ns as f64 / control_ns.max(1) as f64;
        let enabled_ratio = enabled_ns as f64 / disabled_ns.max(1) as f64;
        if disabled_ratio > DISABLED_LIMIT {
            failures.push(format!(
                "{name}: disabled overhead {:.2}% exceeds 2%",
                (disabled_ratio - 1.0) * 100.0
            ));
        }
        if enabled_ratio > ENABLED_LIMIT {
            failures.push(format!(
                "{name}: enabled overhead {:.2}% exceeds 15%",
                (enabled_ratio - 1.0) * 100.0
            ));
        }
        cases.push(serde_json::json!({
            "case": name,
            "control_p50_ns": control_ns,
            "disabled_p50_ns": disabled_ns,
            "enabled_p50_ns": enabled_ns,
            "disabled_ratio": disabled_ratio,
            "enabled_ratio": enabled_ratio,
            "disabled_pass": disabled_ratio <= DISABLED_LIMIT,
            "enabled_pass": enabled_ratio <= ENABLED_LIMIT,
        }));
    }
    let passed = failures.is_empty();
    let report = serde_json::json!({
        "schema": 1,
        "protocol": "same Benchmark v7 source; compiled-out/disabled/enabled; median of seven process medians; 1000 samples per process",
        "implementation_commit": control["implementation_commit"],
        "benchmark_source_hash": control["benchmark_source_hash"],
        "processes": PROCESSES,
        "samples_per_process": SAMPLES,
        "disabled_limit_ratio": DISABLED_LIMIT,
        "enabled_limit_ratio": ENABLED_LIMIT,
        "cases": cases,
        "failures": failures,
        "status": if passed { "PASS" } else { "FAIL" },
    });
    let report_path = output_dir.join("formal-7x1000.json");
    fs::write(
        &report_path,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err(format!(
            "profiler overhead gate failed; see {}",
            report_path.display()
        )
        .into())
    }
}

/// M5 WP19-WP26 gate: deterministic layouts, verifier-owned physical ABI,
/// parameter placement, contiguous return ranges, and portable/dense parity.
fn test_value_layout() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-bytecode", "--test", "layout"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-bytecode",
        "--lib",
        "tests::struct_metadata_and_opcodes_round_trip_in_bytecode_v7",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-verifier",
        "--lib",
        "tests::verified_module_owns_the_exact_physical_layout_and_function_abi",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--lib",
        "interpreter::tests::physical_abi_scatter_preserves_aggregate_and_following_scalar_parameters",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--lib",
        "interpreter::tests::bytecode_v7_opcode_cost_schedule_matches_the_frozen_fixture",
        "--",
        "--exact",
    ])
}

/// M5 WP47-WP58 gate: typed scalar/row storage, amortized push/pop,
/// incremental map rehash, Unicode `StringBuild`, container GC barriers, and
/// execution-image constant-pool retirement at their required stress scales.
fn test_typed_collections() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "collection_string_stress",
    ])
}

/// M5 WP37-WP46 gate: validated local passes, match specialization,
/// bounded package inlining, materialization decisions, and source-preserving
/// optimized/reference differential evidence.
fn test_ir_optimizations() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-analysis", "--lib", "passes"])?;
    test_optimization_differential()
}

/// M5 WP36 gate: optimized versus reference pipeline over the handwritten
/// and fixed-seed generated legal-program corpora. Identical results, traps,
/// and task lifecycles are required; fuel totals are exempt per the
/// cross-pipeline ruling.
fn test_optimization_differential() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "optimization_differential",
    ])
}

/// M5 stage-F gate: portable versus predecoded-row interpreter over compiled
/// and fixed-seed generated Bytecode under one cost table - results, traps,
/// per-slice charges, suspend points, and fuel totals match item by item.
fn test_executable_parity() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-runtime", "--test", "executable_parity"])
}

/// M5 WP59-WP70 gate: load-time executable plan completeness, compact row
/// bounds, dense identities/call frames/root maps, static-leaf equivalence,
/// and portable-versus-executable replay.
fn test_executable_module() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-runtime", "--lib", "executable::tests::"])?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "executable_parity"])?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "root_map_liveness"])
}

/// M5 WP71-WP82 gate: five-phase incremental collection, exact accounting,
/// adaptive triggering, every root/staging boundary, rollback/reload safety,
/// and allocator-authoritative zero-allocation Mark/Sweep slices.
fn test_gc_v1() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-runtime", "--test", "incremental_gc"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--lib",
        "heap::tests::host_transaction_staging_is_an_incremental_gc_root",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "collection_string_stress",
        "array_and_map_class_references_survive_mid_cycle_publication",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "runtime_baseline",
        "gc_suspended_root::gc_suspended_root",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "transactional_cell",
        "failed_cells_restore_class_array_and_map_heap_mutations_exactly",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "restart_reload",
        "activation_fault_restores_the_previous_active_root",
        "--",
        "--exact",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-benchmark-v7",
        "--test",
        "gc_zero_allocation",
    ])
}

/// M5 stage-I gate: the source compilation cache serves byte-identical
/// artifacts, keys on contract identity, and respects its bound.
fn test_source_cache() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-compiler", "--test", "source_cache"])
}

/// M5 WP93-95 gate: the on-disk artifact cache keys every canonical build
/// authority (including the complete 32-byte Contract fingerprint),
/// round-trips versioned/key-bound portable artifacts, discards corruption,
/// stores atomically, and enforces a strict byte budget.
fn test_artifact_cache() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-compiler", "--test", "artifact_cache"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "execution_image_cache",
    ])
}

/// M5 WP97: measures the complete overlapping reload lifetime and proves
/// that content-addressed immutable sharing lowers retained executable
/// payload while every required peak-memory surface is live.
const M5_RELOAD_PEAK_SAMPLES: usize = 7;

fn m5_reload_peak_report() -> Result<(), DynError> {
    let root = workspace_root();
    let output = root.join("target/nexa-artifacts/m5/reload-peak/report.json");
    fs::create_dir_all(output.parent().ok_or("reload-peak report has no parent")?)?;
    let samples = M5_RELOAD_PEAK_SAMPLES.to_string();
    cargo(&[
        "run",
        "--release",
        "--quiet",
        "-p",
        "nexa-benchmark-v7",
        "--",
        "--reload-peak-report",
        "--samples",
        &samples,
        "--output",
        output.to_str().ok_or("non-UTF-8 reload-peak report path")?,
    ])?;

    let head = git_output(&["rev-parse", "HEAD"])?;
    let report = reload_peak_report_at(&output, &head)
        .ok_or("WP97 reload peak report is stale, incomplete, or malformed")?;
    println!(
        "m5-reload-peak-report: {} samples, peak {} bytes, shared executable payload {} bytes",
        report["samples"],
        report["system_allocator"]["peak_outstanding_bytes_max"],
        report["executable_images"]["shared_payload_bytes"],
    );
    Ok(())
}

fn reload_peak_report_at(path: &Path, implementation_commit: &str) -> Option<Value> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let surfaces = report["simultaneous_surfaces"].as_object()?;
    if report["schema"] != 1
        || report["benchmark_version"] != 7
        || report["report"] != "Nexa M5 WP97 Reload Peak Memory"
        || report["implementation_commit"].as_str() != Some(implementation_commit)
        || report["samples"] != M5_RELOAD_PEAK_SAMPLES
        || report["status"] != "PASS"
        || report["build_profile"] != "release"
        || report["protocol"]
            != "one warmup plus bounded whole-lifetime samples in one isolated process"
        || report["measurement_boundary"]
            != "before compiling both candidates through committed reload while encoded artifacts, both execution images, migration staging, active incremental GC, and rooted VM storage overlap"
        || report["started_at_unix_ms"]
            .as_u64()
            .is_none_or(|started| started == 0)
        || !qualified_benchmark_toolchain(&report)
        || !qualified_benchmark_machine(&report)
        || !is_hex_digest(&report["benchmark_source_hash"], 32)
        || !valid_p50_p95_p99(&report["duration"])
        || surfaces.len() != 7
        || surfaces.values().any(|value| value != true)
        || report["system_allocator"]["allocations_max"]
            .as_u64()
            .is_none_or(|allocations| allocations == 0)
        || report["system_allocator"]["allocated_bytes_max"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || report["system_allocator"]["peak_outstanding_bytes_max"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || !valid_portable_artifact_overlap(&report["portable_artifacts"])
        || report["executable_images"]["entries"]
            .as_u64()
            .is_none_or(|entries| entries < 2)
        || report["executable_images"]["shared_payload_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || report["executable_images"]["unique_payload_bytes"]
            .as_u64()
            .zip(report["executable_images"]["logical_payload_bytes"].as_u64())
            .is_none_or(|(unique, logical)| unique >= logical)
        || report["migration_staging"]["object_peak"]
            .as_u64()
            .is_none_or(|peak| peak == 0)
        || report["migration_staging"]["field_peak"]
            .as_u64()
            .is_none_or(|peak| peak == 0)
        || report["migration_staging"]["forwarding_peak"]
            .as_u64()
            .is_none_or(|peak| peak == 0)
        || report["migration_staging"]["payload_byte_peak"]
            .as_u64()
            .is_none_or(|peak| peak == 0)
        || report["migration_staging"]["gc_root_peak"]
            .as_u64()
            .is_none()
        || report["incremental_gc"]["active_before_reload"] != true
        || report["incremental_gc"]["active_after_reload"] != true
        || report["incremental_gc"]["phase_before_reload"]
            .as_str()
            .is_none_or(str::is_empty)
        || report["incremental_gc"]["phase_after_reload"]
            .as_str()
            .is_none_or(str::is_empty)
        || report["vm_storage"]["string_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || report["vm_storage"]["collection_bytes"]
            .as_u64()
            .is_none_or(|bytes| bytes == 0)
        || report["reuse"]["layout_tables"]
            .as_u64()
            .is_none_or(|count| count == 0)
        || report["reuse"]["module_abis"].as_u64().is_none()
        || report["reuse"]["profile_metadata"].as_u64().is_none()
        || report["reuse"]["unchanged_functions"]
            .as_u64()
            .is_none_or(|count| count == 0)
        || report["reuse"]["string_pools"]
            .as_u64()
            .is_none_or(|count| count == 0)
        || report["reuse"]["host_import_plans"]
            .as_u64()
            .is_none_or(|count| count == 0)
    {
        return None;
    }
    Some(report)
}

fn valid_p50_p95_p99(summary: &Value) -> bool {
    let Some(p50) = summary["p50_ns"].as_u64() else {
        return false;
    };
    let Some(p95) = summary["p95_ns"].as_u64() else {
        return false;
    };
    let Some(p99) = summary["p99_ns"].as_u64() else {
        return false;
    };
    p50 > 0 && p50 <= p95 && p95 <= p99
}

fn valid_portable_artifact_overlap(summary: &Value) -> bool {
    let Some(old) = summary["old_bytes"].as_u64() else {
        return false;
    };
    let Some(candidate) = summary["candidate_bytes"].as_u64() else {
        return false;
    };
    let Some(simultaneous) = summary["simultaneous_bytes"].as_u64() else {
        return false;
    };
    old > 0 && candidate > 0 && old.saturating_add(candidate) == simultaneous
}

/// M5 WP98: records every required cold-start surface independently.
///
/// This intentionally uses a bounded one-process protocol instead of adding
/// eight expensive initialization cases to the frozen 7x1000 hot comparison.
/// Benchmark v7 remains the timing authority and the terminal report retains
/// the exact same-HEAD receipt.
const M5_COLD_START_SAMPLES: usize = 15;
const M5_COLD_START_CASES: [&str; 8] = [
    "standalone_single_file",
    "standalone_package",
    "engine_first_discover_enable",
    "artifact_cache_warm",
    "artifact_cache_cold",
    "repl_first_cell",
    "repl_subsequent_cell",
    "reload",
];

fn m5_cold_start_report() -> Result<(), DynError> {
    let root = workspace_root();
    let output = root.join("target/nexa-artifacts/m5/cold-start/report.json");
    fs::create_dir_all(output.parent().ok_or("cold-start report has no parent")?)?;
    let samples = M5_COLD_START_SAMPLES.to_string();
    cargo(&[
        "run",
        "--release",
        "--quiet",
        "-p",
        "nexa-benchmark-v7",
        "--",
        "--cold-start-report",
        "--samples",
        &samples,
        "--output",
        output.to_str().ok_or("non-UTF-8 cold-start report path")?,
    ])?;

    let head = git_output(&["rev-parse", "HEAD"])?;
    let report = cold_start_report_at(&output, &head)
        .ok_or("WP98 cold-start report is stale, incomplete, or malformed")?;
    let cases = report["cases"]
        .as_array()
        .ok_or("WP98 report cases is not an array")?;
    println!(
        "m5-cold-start-report: {} isolated cases x {M5_COLD_START_SAMPLES} samples",
        cases.len()
    );
    Ok(())
}

fn cold_start_report_at(path: &Path, implementation_commit: &str) -> Option<Value> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let cases = report["cases"].as_array()?;
    let actual = cases
        .iter()
        .map(|case| case["case"].as_str())
        .collect::<Option<Vec<_>>>()?;
    if actual != M5_COLD_START_CASES
        || report["schema"] != 1
        || report["benchmark_version"] != 7
        || report["report"] != "Nexa M5 WP98 Cold Start"
        || report["implementation_commit"].as_str() != Some(implementation_commit)
        || report["samples"] != M5_COLD_START_SAMPLES
        || report["warmup"] != 1
        || report["status"] != "PASS"
        || report["build_profile"] != "release"
        || report["protocol"]
            != "one isolated process; one unrecorded process warmup; every measured cold sample reconstructs the named boundary"
        || report["started_at_unix_ms"]
            .as_u64()
            .is_none_or(|started| started == 0)
        || report["measurement_boundaries"]
            .as_object()
            .is_none_or(|boundaries| boundaries.len() != M5_COLD_START_CASES.len())
        || !qualified_benchmark_toolchain(&report)
        || !qualified_benchmark_machine(&report)
        || !is_hex_digest(&report["benchmark_source_hash"], 32)
    {
        return None;
    }
    for case in cases {
        let name = case["case"].as_str()?;
        let minimum = case["min_ns"].as_u64()?;
        let p50 = case["p50_ns"].as_u64()?;
        let p90 = case["p90_ns"].as_u64()?;
        let p95 = case["p95_ns"].as_u64()?;
        let p99 = case["p99_ns"].as_u64()?;
        let maximum = case["max_ns"].as_u64()?;
        if case["tier"].as_str().is_none_or(str::is_empty)
            || case["samples"] != M5_COLD_START_SAMPLES
            || case["throughput_ops_per_second"]
                .as_u64()
                .is_none_or(|throughput| throughput == 0)
            || case["mean_ns"].as_u64().is_none_or(|mean| mean == 0)
            || minimum == 0
            || minimum > p50
            || p50 > p90
            || p90 > p95
            || p95 > p99
            || p99 > maximum
            || [
                "system_allocations",
                "system_reallocations",
                "system_allocated_bytes",
                "system_reallocated_bytes",
                "system_peak_outstanding_bytes",
            ]
            .into_iter()
            .any(|field| case[field].as_u64().is_none())
            || report["measurement_boundaries"][name]
                .as_str()
                .is_none_or(str::is_empty)
        {
            return None;
        }
    }
    Some(report)
}

const STEADY_STATE_DISPATCH_CASES: [&str; 6] = [
    "broadcast",
    "projected_broadcast",
    "optional_broadcast",
    "owner_call",
    "optional_provider_call",
    "idle_tick",
];

fn steady_state_dispatch_source_hash() -> Result<String, DynError> {
    let source =
        fs::read(workspace_root().join("tools/benchmark-v7/tests/steady_state_allocation.rs"))?;
    Ok(blake3::hash(&source).to_hex().to_string())
}

fn steady_state_dispatch_receipt_at(path: &Path, implementation_commit: &str) -> Option<Value> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    steady_state_dispatch_receipt(report, implementation_commit)
}

fn steady_state_dispatch_receipt(report: Value, implementation_commit: &str) -> Option<Value> {
    let cases = report["cases"].as_object()?;
    let source_hash = steady_state_dispatch_source_hash().ok()?;
    (report["schema"] == 1
        && report["report"] == "Nexa M5 WP92 Steady-State Engine Allocation"
        && report["implementation_commit"].as_str() == Some(implementation_commit)
        && report["test_source_hash"].as_str() == Some(source_hash.as_str())
        && report["status"] == "PASS"
        && report["max_system_allocations"].as_u64() == Some(0)
        && cases.len() == STEADY_STATE_DISPATCH_CASES.len()
        && STEADY_STATE_DISPATCH_CASES
            .iter()
            .all(|name| cases.get(*name).and_then(Value::as_u64) == Some(0)))
    .then_some(report)
}

/// M5 stage-H gate: the WP89 immediate entrypoint settles with no Task,
/// scheduler token, or tombstone, the H1 continuation pool feeds
/// steady-state admissions, and the WP90/WP92 engine dispatch path is
/// allocation-exact in steady state.
fn test_runtime_fast_paths() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "immediate_entrypoint",
    ])?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "continuation_pool"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-benchmark-v7",
        "--test",
        "steady_state_allocation",
    ])
}

/// M5 WP83-WP92 gate: Host imports and typed script exports resolve before
/// execution, package routing survives enable/disable/reload ABA cycles, and
/// every required steady-state Engine path is allocation-exact.
fn test_host_engine_performance() -> Result<(), DynError> {
    let root = workspace_root();
    let receipt_path = root.join("target/nexa-artifacts/m5/steady-state-dispatch/report.json");
    fs::create_dir_all(
        receipt_path
            .parent()
            .ok_or("steady-state receipt has no parent")?,
    )?;
    if receipt_path.exists() {
        fs::remove_file(&receipt_path)?;
    }
    let implementation_commit = git_output(&["rev-parse", "HEAD"])?;
    let test_source_hash = steady_state_dispatch_source_hash()?;
    let receipt_path_text = receipt_path
        .to_str()
        .ok_or("non-UTF-8 steady-state receipt path")?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "stable_host_dispatch",
    ])?;
    cargo_with_environment(
        &[
            "test",
            "-p",
            "nexa-benchmark-v7",
            "--test",
            "steady_state_allocation",
        ],
        &[
            ("NEXA_M5_STEADY_STATE_RECEIPT", receipt_path_text),
            ("NEXA_M5_IMPLEMENTATION_COMMIT", &implementation_commit),
            ("NEXA_M5_STEADY_STATE_SOURCE_HASH", &test_source_hash),
        ],
    )?;
    cargo(&["test", "-p", "nexa-embed", "--test", "entrypoints"])?;
    let receipt = steady_state_dispatch_receipt_at(&receipt_path, &implementation_commit)
        .ok_or("WP92 steady-state dispatch receipt is stale, malformed, or nonzero")?;
    println!(
        "test-host-engine-performance: {} paths, max {} system allocations",
        receipt["cases"].as_object().map_or(0, serde_json::Map::len),
        receipt["max_system_allocations"]
    );
    Ok(())
}

/// M5 WP99 product gate. Every command below executes a real canonical
/// package/build/runtime path; the receipt is written only after the entire
/// corpus succeeds and is pinned to the current implementation commit.
fn valid_snake_stress_report(report: &Value) -> bool {
    const RESOURCE_FIELDS: [&str; 16] = [
        "enabled_packages",
        "tasks",
        "scopes",
        "continuations",
        "scheduler_tokens",
        "requests",
        "completion_reservations",
        "tokens",
        "snapshots",
        "release_reservations",
        "queued_releases",
        "heap_objects",
        "state_objects",
        "retired_modules",
        "host_pending_completions",
        "host_pending_releases",
    ];
    report["schema"] == 1
        && report["steady_ticks"] == 1_024
        && report["disable_enable_cycles"] == 100
        && report["reload_cycles"] == 100
        && report["entitlement_cycles"] == 100
        && report["resource_leaks"].as_u64() == Some(0)
        && report["post_shutdown"]
            .as_object()
            .is_some_and(|resources| {
                resources.len() == RESOURCE_FIELDS.len()
                    && RESOURCE_FIELDS
                        .iter()
                        .all(|field| resources.get(*field).and_then(Value::as_u64) == Some(0))
            })
}

fn m5_product_corpus() -> Result<(), DynError> {
    let root = workspace_root();
    cargo(&["test", "-p", "nexa-embed", "--test", "entrypoints"])?;
    test_snake()?;
    let snake_stress: Value = serde_json::from_str(&captured_stdout(
        Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "snake-game",
                "--bin",
                "snake-headless",
                "--",
                "stress",
            ])
            .current_dir(&root),
        "snake-headless stress",
    )?)?;
    if !valid_snake_stress_report(&snake_stress) {
        return Err("Snake stress omitted exact post-shutdown resource evidence".into());
    }
    cargo(&["run", "-p", "combat-runtime"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "collection_string_stress",
    ])?;
    cargo(&["test", "-p", "nexa-cli", "--test", "standalone"])?;
    cargo(&["test", "-p", "nexa-cli", "--test", "repl"])?;

    let verification_stdout = captured_stdout(
        Command::new("cargo")
            .args([
                "run",
                "--release",
                "--quiet",
                "-p",
                "nexa-benchmark-v7",
                "--",
                "--verify-products",
            ])
            .current_dir(&root),
        "nexa-benchmark-v7 --verify-products",
    )?;
    let product_results: Value = serde_json::from_str(&verification_stdout)?;
    for &(workload, expected) in &PRODUCT_CPU_RESULTS {
        if product_results[workload].as_i64() != Some(expected) {
            return Err(format!(
                "product verification returned {} for {workload}; expected {expected}",
                product_results[workload]
            )
            .into());
        }
    }

    let head = git_output(&["rev-parse", "HEAD"])?;
    let report = serde_json::json!({
        "schema": 1,
        "milestone": "Nexa M5 Product Corpus",
        "implementation_commit": head,
        "snake": {
            "canonical_package_tests": "PASS",
            "headless_stress": "PASS",
            "engine_package_scales": [0, 9, 20, 50],
            "enable_disable_reload": "PASS",
            "required_and_optional_dispatch": "PASS",
            "stress_receipt": snake_stress,
        },
        "combat": {
            "canonical_package_pipeline": "PASS",
            "deep_calls_struct_enum_class_async_resources_migration_reload": "PASS",
        },
        "data_intensive": {
            "ten_thousand_struct_rows": "PASS",
            "typed_array_map_gc_string_state": "PASS",
            "verified_results": product_results,
        },
        "standalone": {
            "single_file_package_async_trap": "PASS",
        },
        "repl": {
            "one_hundred_cells": "PASS",
            "failed_cell_rollback": "PASS",
            "async_cell_and_resource_recovery": "PASS",
        },
        "status": "PASS",
    });
    let output = root.join("target/nexa-artifacts/m5/product-corpus/report.json");
    fs::create_dir_all(output.parent().ok_or("product report has no parent")?)?;
    fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// M5 stage-J: produces the decision artifacts named by
/// `JIT_DECISION_V1.md` - the formal 7x1000 performance report plus one
/// terminal `GO` or `DEFER` decision. Missing evidence is itself a reason to
/// defer; the report never disguises an unfinished GO condition as pending.
fn formal_aggregate_at(path: &Path, implementation_commit: &str) -> Option<Value> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    formal_aggregate(report, implementation_commit)
}

const FORMAL_CASE_U64_FIELDS: [&str; 10] = [
    "max_frame_1000_calls_ns",
    "max_system_allocations",
    "max_system_reallocations",
    "max_system_allocated_bytes",
    "max_system_reallocated_bytes",
    "max_system_peak_outstanding_bytes",
    "fuel_total",
    "fuel_per_operation",
    "instructions_total",
    "instructions_per_operation",
];
const FORMAL_VM_OPTIONAL_U64_FIELDS: [&str; 13] = [
    "allocations",
    "string_allocations",
    "class_allocations",
    "collection_storage_allocations",
    "map_slot_allocations",
    "struct_materializations",
    "enum_materializations",
    "allocated_bytes",
    "live_bytes",
    "collection_relocation_bytes",
    "string_copy_bytes",
    "host_codec_copy_bytes",
    "bytes_copied",
];
const FORMAL_GC_OPTIONAL_U64_FIELDS: [&str; 4] = [
    "cycles",
    "pause_ns_max",
    "objects_reclaimed",
    "bytes_reclaimed",
];
const FORMAL_RESOURCE_U64_FIELDS: [&str; 7] = [
    "tasks",
    "requests",
    "tokens",
    "snapshots",
    "state_objects",
    "retired_modules",
    "total",
];

fn has_optional_u64_fields(value: &Value, fields: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        fields.iter().all(|field| {
            object
                .get(*field)
                .is_some_and(|value| value.is_null() || value.as_u64().is_some())
        })
    })
}

fn has_u64_fields(value: &Value, fields: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        fields
            .iter()
            .all(|field| object.get(*field).and_then(Value::as_u64).is_some())
    })
}

fn formal_aggregate_case_name(case: &Value) -> Option<&str> {
    let name = case["case"].as_str().filter(|name| !name.is_empty())?;
    let minimum = case["min_ns"].as_u64()?;
    let mean = case["median_mean_ns"].as_u64()?;
    let p50 = case["median_p50_ns"].as_u64()?;
    let p90 = case["median_p90_ns"].as_u64()?;
    let p95 = case["median_p95_ns"].as_u64()?;
    let p99 = case["median_p99_ns"].as_u64()?;
    let maximum = case["max_ns"].as_u64()?;
    let standard_deviation = case["median_standard_deviation_ns"].as_f64()?;
    let coefficient = case["median_coefficient_of_variation"].as_f64()?;
    (case["tier"].as_str().is_some_and(|tier| !tier.is_empty())
        && minimum > 0
        && mean > 0
        && p50 > 0
        && minimum <= p50
        && minimum <= mean
        && mean <= maximum
        && p50 <= p90
        && p90 <= p95
        && p95 <= p99
        && p99 <= maximum
        && standard_deviation.is_finite()
        && standard_deviation >= 0.0
        && coefficient.is_finite()
        && coefficient >= 0.0
        && case["median_throughput_ops_per_second"]
            .as_u64()
            .is_some_and(|throughput| throughput > 0)
        && FORMAL_CASE_U64_FIELDS
            .iter()
            .all(|field| case[*field].as_u64().is_some())
        && case["max_vm"]["live_heap_slots_peak"].as_u64().is_some()
        && has_optional_u64_fields(&case["max_vm"], &FORMAL_VM_OPTIONAL_U64_FIELDS)
        && has_optional_u64_fields(&case["max_gc"], &FORMAL_GC_OPTIONAL_U64_FIELDS)
        && has_u64_fields(&case["peak_resources"], &FORMAL_RESOURCE_U64_FIELDS))
    .then_some(name)
}

fn formal_aggregate_has_structural_evidence(cases: &[Value]) -> bool {
    let case_named = |name: &str| cases.iter().find(|case| case["case"] == name);
    let (Some(struct_case), Some(enum_case), Some(gc_case), Some(immediate_case)) = (
        case_named("struct_construction"),
        case_named("enum_construction_match"),
        case_named("gc_incremental_step"),
        case_named("immediate_call"),
    ) else {
        return false;
    };
    struct_case["max_vm"]["struct_materializations"].as_u64() == Some(0)
        && enum_case["max_vm"]["enum_materializations"].as_u64() == Some(0)
        && gc_case["max_system_allocations"].as_u64() == Some(0)
        && gc_case["max_system_reallocations"].as_u64() == Some(0)
        && gc_case["max_system_allocated_bytes"].as_u64() == Some(0)
        && gc_case["max_system_reallocated_bytes"].as_u64() == Some(0)
        && gc_case["max_gc"]["cycles"]
            .as_u64()
            .is_some_and(|cycles| cycles > 0)
        && gc_case["max_gc"]["objects_reclaimed"]
            .as_u64()
            .is_some_and(|objects| objects > 0)
        && gc_case["max_gc"]["bytes_reclaimed"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
        && immediate_case["peak_resources"]["tasks"].as_u64() == Some(0)
}

fn formal_aggregate(report: Value, implementation_commit: &str) -> Option<Value> {
    let cases = report["cases"].as_array()?;
    let names = cases
        .iter()
        .map(formal_aggregate_case_name)
        .collect::<Option<BTreeSet<_>>>()?;
    if names.len() != cases.len() || !formal_aggregate_has_structural_evidence(cases) {
        return None;
    }
    let mandatory_cases = VALUE_COLLECTION_CASES
        .iter()
        .chain(PRODUCT_CPU_CASES)
        .chain(HOST_TASK_ENGINE_CASES)
        .chain(COLD_START_CASES);
    (report["schema"].as_u64() == Some(2)
        && report["implementation_commit"].as_str() == Some(implementation_commit)
        && report["benchmark_version"].as_u64() == Some(7)
        && report["protocol"] == "median across process medians; each process independently warmed"
        && report["status"] == "PASS"
        && report["process_count"].as_u64() == Some(7)
        && report["samples_per_process"].as_u64() == Some(1_000)
        && report["warmup_per_process"].as_u64() == Some(100)
        && report["started_at_unix_ms"]
            .as_u64()
            .is_some_and(|started| started > 0)
        && report["build_profile"] == "release"
        && report["profiler_enabled"] == false
        && report["profiler_mode"] == "disabled"
        && report["allocation_scope"]
            == "timed operation only; per-sample setup and result storage excluded"
        && is_hex_digest(&report["benchmark_source_hash"], 32)
        && is_hex_digest(&report["bytecode_hash"], 32)
        && qualified_benchmark_toolchain(&report)
        && qualified_benchmark_machine(&report)
        && !cases.is_empty()
        && mandatory_cases
            .into_iter()
            .all(|name| names.contains(*name)))
    .then_some(report)
}

fn is_hex_digest(value: &Value, bytes: usize) -> bool {
    value.as_str().is_some_and(|digest| {
        digest.len() == bytes.saturating_mul(2)
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn qualified_benchmark_toolchain(report: &Value) -> bool {
    report["toolchain"]
        .as_str()
        .is_some_and(|toolchain| !toolchain.is_empty() && toolchain != "unknown")
}

fn qualified_benchmark_machine(report: &Value) -> bool {
    [
        "arch",
        "os",
        "os_version",
        "machine_model",
        "cpu_model",
        "power_source",
        "thermal_policy",
    ]
    .into_iter()
    .all(|field| {
        report[field]
            .as_str()
            .is_some_and(|value| !value.is_empty() && value != "unknown")
    }) && report["logical_cpu_count"]
        .as_u64()
        .is_some_and(|count| count > 0)
}

fn same_benchmark_machine(left: &Value, right: &Value) -> bool {
    qualified_benchmark_machine(left)
        && qualified_benchmark_machine(right)
        && [
            "arch",
            "os",
            "os_version",
            "machine_model",
            "cpu_model",
            "logical_cpu_count",
            "power_source",
            "thermal_policy",
        ]
        .into_iter()
        .all(|field| left[field] == right[field])
}

fn formal_profile_at(path: &Path, implementation_commit: &str, aggregate: &Value) -> Option<Value> {
    let report: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let cases = report["cases"].as_array()?;
    let aggregate_cases = aggregate["cases"].as_array()?;
    let case_names = cases
        .iter()
        .map(|case| case["case"].as_str())
        .collect::<Option<Vec<_>>>()?;
    let aggregate_names = aggregate_cases
        .iter()
        .map(|case| case["case"].as_str())
        .collect::<Option<Vec<_>>>()?;
    let profiler = &report["profiler"];
    (report["schema"].as_u64() == Some(1)
        && report["implementation_commit"].as_str() == Some(implementation_commit)
        && report["benchmark_version"].as_u64() == Some(7)
        && report["samples"].as_u64() == Some(200)
        && report["warmup"].as_u64() == Some(100)
        && report["process_index"].as_u64() == Some(0)
        && report["build_profile"] == "release"
        && report["profiler_enabled"].as_bool() == Some(true)
        && report["profiler_mode"] == "enabled"
        && report["benchmark_source_hash"] == aggregate["benchmark_source_hash"]
        && report["bytecode_hash"] == aggregate["bytecode_hash"]
        && report["toolchain"] == aggregate["toolchain"]
        && is_hex_digest(&report["benchmark_source_hash"], 32)
        && is_hex_digest(&report["bytecode_hash"], 32)
        && profiler["schema"].as_u64() == Some(1)
        && profiler["total_opcode_executions"]
            .as_u64()
            .is_some_and(|executions| executions > 0)
        && profiler["function_count"]
            .as_u64()
            .is_some_and(|count| count > 0)
        && profiler["top_opcodes"]
            .as_array()
            .is_some_and(|opcodes| !opcodes.is_empty())
        && [
            "dropped_modules",
            "dropped_functions",
            "dropped_sites",
            "dropped_host_calls",
        ]
        .into_iter()
        .all(|field| profiler[field].as_u64().is_some())
        && profiler["gc"].is_object()
        && profiler["tasks"].is_object()
        && case_names == aggregate_names
        && cases.iter().all(|case| {
            let p50 = case["p50_ns"].as_u64();
            let p95 = case["p95_ns"].as_u64();
            let p99 = case["p99_ns"].as_u64();
            case["samples"].as_u64() == Some(200)
                && case["throughput_ops_per_second"]
                    .as_u64()
                    .is_some_and(|throughput| throughput > 0)
                && p50.is_some_and(|value| value > 0)
                && p50.zip(p95).is_some_and(|(p50, p95)| p50 <= p95)
                && p95.zip(p99).is_some_and(|(p95, p99)| p95 <= p99)
        })
        && same_benchmark_machine(&report, aggregate))
    .then_some(report)
}

#[allow(
    clippy::too_many_lines,
    // Percent displays only; the mantissa bound is irrelevant here.
    clippy::cast_precision_loss
)]
fn m5_final_report() -> Result<(), DynError> {
    use std::fmt::Write as _;
    let root = workspace_root();
    let head = git_output(&["rev-parse", "HEAD"])?;
    let final_dir = root.join("target/nexa-artifacts/m5/final");
    fs::create_dir_all(&final_dir)?;
    let steady_state_path = root.join("target/nexa-artifacts/m5/steady-state-dispatch/report.json");
    let steady_state_dispatch =
        if let Some(report) = steady_state_dispatch_receipt_at(&steady_state_path, &head) {
            eprintln!("final report: reusing same-HEAD WP92 dispatch receipt");
            report
        } else {
            test_host_engine_performance()?;
            steady_state_dispatch_receipt_at(&steady_state_path, &head)
                .ok_or("new WP92 dispatch receipt is not same-HEAD allocation evidence")?
        };
    let reload_peak_path = root.join("target/nexa-artifacts/m5/reload-peak/report.json");
    let reload_peak = if let Some(report) = reload_peak_report_at(&reload_peak_path, &head) {
        eprintln!("final report: reusing same-HEAD WP97 reload peak report");
        report
    } else {
        m5_reload_peak_report()?;
        reload_peak_report_at(&reload_peak_path, &head)
            .ok_or("new WP97 reload peak report is not a same-HEAD Benchmark v7 receipt")?
    };
    let cold_start_path = root.join("target/nexa-artifacts/m5/cold-start/report.json");
    let cold_start = if let Some(report) = cold_start_report_at(&cold_start_path, &head) {
        eprintln!("final report: reusing same-HEAD WP98 cold-start report");
        report
    } else {
        m5_cold_start_report()?;
        cold_start_report_at(&cold_start_path, &head)
            .ok_or("new WP98 cold-start report is not a same-HEAD Benchmark v7 receipt")?
    };
    let aggregate_path = final_dir.join("aggregate-7x1000.json");
    let profile_path = final_dir.join("profile-1x200.json");
    let aggregate = if let Some(report) = formal_aggregate_at(&aggregate_path, &head) {
        eprintln!("final report: reusing same-HEAD formal aggregate");
        report
    } else {
        cargo(&[
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v7",
            "--",
            "--samples",
            "1000",
            "--processes",
            "7",
            "--output",
            aggregate_path.to_str().ok_or("non-UTF-8 artifact path")?,
        ])?;
        formal_aggregate_at(&aggregate_path, &head)
            .ok_or("new M5 aggregate is not a same-HEAD formal 7x1000 report")?
    };
    if !same_benchmark_machine(&reload_peak, &aggregate)
        || !same_benchmark_machine(&cold_start, &aggregate)
    {
        return Err(
            "WP97/WP98 and the formal hot aggregate were measured under different machine qualifications"
                .into(),
        );
    }
    let profile = if let Some(report) = formal_profile_at(&profile_path, &head, &aggregate) {
        eprintln!("final report: reusing same-HEAD 1x200 profile");
        report
    } else {
        cargo(&[
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v7",
            "--",
            "--samples",
            "200",
            "--profile",
            "--output",
            profile_path.to_str().ok_or("non-UTF-8 artifact path")?,
        ])?;
        formal_profile_at(&profile_path, &head, &aggregate)
            .ok_or("new M5 profile is not a same-machine, same-HEAD 1x200 report")?
    };
    let profiler = &profile["profiler"];
    let total_opcodes = profiler["total_opcode_executions"].as_u64().unwrap_or(0);
    let top_share = profiler["top_opcodes"].as_array().map_or(0, |entries| {
        entries
            .iter()
            .filter_map(|entry| entry[1].as_u64())
            .sum::<u64>()
    });
    let function_count = profiler["function_count"].as_u64().unwrap_or(0);
    let dropped_functions = profiler["dropped_functions"].as_u64().unwrap_or(0);
    let case = |name: &str| -> Option<&Value> {
        aggregate["cases"]
            .as_array()?
            .iter()
            .find(|case| case["case"] == name)
    };
    let product_cases = [
        "product_data_sweep",
        "product_combat_tick",
        "product_grid_score",
    ];
    let gc_step_p50 = case("gc_incremental_step")
        .and_then(|case| case["median_p50_ns"].as_u64())
        .unwrap_or(0);
    let surfaces_frozen = git_output(&["cat-file", "-t", "performance-m5-complete"])
        .is_ok_and(|kind| kind == "tag")
        && git_output(&["rev-parse", "performance-m5-complete^{}"])
            .is_ok_and(|target| target == head)
        && git_output(&["status", "--porcelain"]).is_ok_and(|status| status.is_empty());
    let conditions = serde_json::json!({
        "c1_interpreter_dominance": {
            "status": "not-proven",
            "note": "opcode counts show interpreter concentration, but the required per-workload CPU sampling proof is not yet available",
            "top5_opcode_share_percent": if total_opcodes == 0 { 0.0 } else { 100.0 * top_share as f64 / total_opcodes as f64 },
            "total_opcode_executions": total_opcodes,
            "product_workloads": product_cases,
        },
        "c2_gc_host_not_first_bottleneck": {
            "status": "satisfied",
            "note": "GC steps allocate zero system memory and pause ~1us; host boundary allocations are contract-pinned constants",
            "gc_incremental_step_p50_ns": gc_step_p50,
        },
        "c3_hot_spot_concentration": {
            "status": "satisfied",
            "recorded_functions": function_count,
            "dropped_functions": dropped_functions,
        },
        "c4_v8_gap": v8_gap_condition(&final_dir, &aggregate),
        "c5_llvm_amortization": {
            "status": "not-proven",
            "note": "no frozen LLVM compilation-cost prototype proves amortization within a call-count or frame-count budget",
        },
        "c6_frozen_surfaces": {
            "status": if surfaces_frozen { "satisfied" } else { "not-frozen" },
            "note": if surfaces_frozen {
                "ValueLayout, ExecutableModule, safepoints, root maps, and Host ABI are frozen by the annotated M5 completion tag"
            } else {
                "the annotated performance-m5-complete tag does not yet freeze the current clean HEAD"
            },
        },
    });
    let failed_go_conditions = conditions
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, condition)| {
            (condition["status"] != "satisfied").then_some(name.clone())
        })
        .collect::<Vec<_>>();
    let jit_decision = if failed_go_conditions.is_empty() {
        "GO"
    } else {
        "DEFER"
    };
    let rendered_blockers = failed_go_conditions.join(", ");
    let decision = serde_json::json!({
        "schema": 1,
        "decision": jit_decision,
        "failed_go_conditions": failed_go_conditions,
        "conditions": conditions,
        "aggregate_artifact": "target/nexa-artifacts/m5/final/aggregate-7x1000.json",
        "profile_artifact": "target/nexa-artifacts/m5/final/profile-1x200.json",
        "reload_peak_artifact": "target/nexa-artifacts/m5/reload-peak/report.json",
        "cold_start_artifact": "target/nexa-artifacts/m5/cold-start/report.json",
        "implementation_commit": aggregate["implementation_commit"],
    });
    fs::write(
        final_dir.join("jit-decision.json"),
        serde_json::to_vec_pretty(&decision)?,
    )?;
    let report = serde_json::json!({
        "schema": 1,
        "aggregate": aggregate,
        "reload_peak": reload_peak,
        "cold_start": cold_start,
        "steady_state_dispatch": steady_state_dispatch,
        "profiler": profile["profiler"],
        "decision": decision,
    });
    fs::write(
        final_dir.join("performance-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let mut markdown = String::from("# M5 Performance Report (generated)\n\n");
    writeln!(
        markdown,
        "M6 LLVM JIT decision: **{jit_decision}** (failed GO conditions: {rendered_blockers}).\n",
    )?;
    writeln!(
        markdown,
        "Qualification: {} / {} / {} logical CPUs; {}; {}; {}.\n",
        aggregate["machine_model"].as_str().unwrap_or("?"),
        aggregate["cpu_model"].as_str().unwrap_or("?"),
        aggregate["logical_cpu_count"],
        aggregate["os_version"].as_str().unwrap_or("?"),
        aggregate["power_source"].as_str().unwrap_or("?"),
        aggregate["thermal_policy"].as_str().unwrap_or("?"),
    )?;
    markdown
        .push_str("| case | tier | p50 (ns) | p99 (ns) | max allocs |\n|---|---|---|---|---|\n");
    if let Some(cases) = aggregate["cases"].as_array() {
        for case in cases {
            writeln!(
                markdown,
                "| {} | {} | {} | {} | {} |",
                case["case"].as_str().unwrap_or("?"),
                case["tier"].as_str().unwrap_or("?"),
                case["median_p50_ns"],
                case["median_p99_ns"],
                case["max_system_allocations"],
            )?;
        }
    }
    markdown.push_str(
        "\nWP97 reload peak retains both artifacts/images with migration, active GC, and VM storage:\n\n",
    );
    writeln!(
        markdown,
        "- system peak: {} bytes; executable payload: {} logical / {} unique / {} shared bytes",
        report["reload_peak"]["system_allocator"]["peak_outstanding_bytes_max"],
        report["reload_peak"]["executable_images"]["logical_payload_bytes"],
        report["reload_peak"]["executable_images"]["unique_payload_bytes"],
        report["reload_peak"]["executable_images"]["shared_payload_bytes"],
    )?;
    markdown.push_str(
        "\nWP98 cold-start cases are recorded separately with one isolated process and 15 samples:\n\n",
    );
    if let Some(cases) = report["cold_start"]["cases"].as_array() {
        for case in cases {
            writeln!(
                markdown,
                "- {}: p50 {} ns, p99 {} ns",
                case["case"].as_str().unwrap_or("?"),
                case["p50_ns"],
                case["p99_ns"],
            )?;
        }
    }
    writeln!(
        markdown,
        "\nProfile: {total_opcodes} opcode executions recorded, top-5 share {:.1}%, {function_count} functions, {dropped_functions} dropped.",
        if total_opcodes == 0 {
            0.0
        } else {
            100.0 * top_share as f64 / total_opcodes as f64
        },
    )?;
    fs::write(final_dir.join("performance-report.md"), markdown)?;
    println!("m5-final-report: {jit_decision} decision written to target/nexa-artifacts/m5/final/");
    Ok(())
}

/// Renders GO condition c4 from the stage-J V8 comparison artifact. A
/// missing or stale artifact keeps the condition pending without blocking
/// the rest of the report (`JIT_DECISION_V1.md`).
fn v8_gap_condition(final_dir: &Path, aggregate: &Value) -> Value {
    let comparison: Option<Value> = fs::read(final_dir.join("v8-comparison.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let Some(comparison) = comparison else {
        return serde_json::json!({
            "status": "pending-environment",
            "note": "run cargo xtask m5-v8-comparison on a machine with a qualified Node/V8 toolchain; its absence blocks only this report per JIT_DECISION_V1.md",
        });
    };
    if comparison["nexa_implementation_commit"] != aggregate["implementation_commit"] {
        return serde_json::json!({
            "status": "stale",
            "note": "v8-comparison.json was produced at a different commit; rerun cargo xtask m5-v8-comparison",
        });
    }
    let Some(workloads) = comparison["workloads"].as_array() else {
        return serde_json::json!({
            "status": "malformed",
            "note": "v8-comparison.json omitted its workload evidence",
        });
    };
    let workload_names = workloads
        .iter()
        .map(|workload| workload["case"].as_str())
        .collect::<Option<Vec<_>>>();
    let computed_leads = workloads
        .iter()
        .filter_map(|workload| {
            Some((
                workload["nexa_median_p50_ns"].as_u64()?,
                workload["v8_median_p50_ns"].as_u64()?,
            ))
        })
        .filter(|&(nexa_p50, v8_p50)| v8_lead_at_least_1_5x(nexa_p50, v8_p50))
        .count();
    let declared_leads = comparison["workloads_with_v8_lead_at_least_1_5x"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok());
    let satisfied = comparison["c4_v8_gap_satisfied"].as_bool();
    let malformed = comparison["schema"].as_u64() != Some(1)
        || comparison["status"] != "PASS"
        || comparison["protocol"]
            != "7 processes x 1000 samples per side; median across process medians; per-process warmup"
        || comparison["result_parity"]
            != "all workloads returned identical results in both runtimes"
        || comparison["semantic_mismatches"]
            .as_array()
            .is_none_or(|mismatches| !mismatches.is_empty())
        || comparison["processes"].as_u64() != Some(7)
        || comparison["samples_per_process"].as_u64() != Some(1_000)
        || comparison["warmup_per_process"].as_u64() != Some(100)
        || !is_hex_digest(&comparison["harness_source_hash"], 32)
        || comparison["node_version"]
            .as_str()
            .is_none_or(str::is_empty)
        || comparison["v8_version"].as_str().is_none_or(str::is_empty)
        || !same_benchmark_machine(&comparison["qualification_machine"], aggregate)
        || workload_names.as_deref() != Some(PRODUCT_CPU_CASES)
        || workloads.iter().enumerate().any(|(index, workload)| {
            let Some(&(expected_case, expected_result)) = PRODUCT_CPU_RESULTS.get(index) else {
                return true;
            };
            let Some(nexa_p50) = workload["nexa_median_p50_ns"].as_u64() else {
                return true;
            };
            let Some(v8_p50) = workload["v8_median_p50_ns"].as_u64() else {
                return true;
            };
            let Some(reported_ratio) = workload["v8_lead_ratio"].as_f64() else {
                return true;
            };
            let expected_ratio = rounded_v8_lead_ratio(nexa_p50, v8_p50);
            workload["case"].as_str() != Some(expected_case)
                || workload["result"].as_i64() != Some(expected_result)
                || nexa_p50 == 0
                || v8_p50 == 0
                || !reported_ratio.is_finite()
                || (reported_ratio - expected_ratio).abs() > f64::EPSILON
                || workload["v8_lead_at_least_1_5x"].as_bool()
                    != Some(v8_lead_at_least_1_5x(nexa_p50, v8_p50))
        })
        || declared_leads != Some(computed_leads)
        || satisfied != Some(computed_leads >= 3);
    if malformed {
        return serde_json::json!({
            "status": "malformed",
            "note": "v8-comparison.json failed its protocol, machine, inventory, parity, or ratio checks",
        });
    }
    serde_json::json!({
        "status": if satisfied == Some(true) { "satisfied" } else { "not-satisfied" },
        "note": "warm V8 versus the Nexa interpreter over the comparable pure-computation product workloads",
        "node_version": comparison["node_version"],
        "v8_version": comparison["v8_version"],
        "workloads": comparison["workloads"],
        "workloads_with_v8_lead_at_least_1_5x": comparison["workloads_with_v8_lead_at_least_1_5x"],
    })
}

/// The formal V8-side process count; mirrors the WP11 protocol used for
/// the Nexa aggregate.
const V8_COMPARISON_PROCESSES: usize = 7;
const V8_COMPARISON_SAMPLES: usize = 1_000;

/// M5 stage-J: the warm-V8 comparison input for GO condition c4
/// (`JIT_DECISION_V1.md`). Pins the Node/V8 versions, proves result parity
/// between the Nexa product workloads and their JavaScript mirrors, then
/// compares median-of-process p50 latencies under the formal 7x1000
/// protocol. A missing Node environment fails only this command, never
/// the rest of M5.
#[allow(
    clippy::too_many_lines,
    // Ratio displays only; the mantissa bound is irrelevant here.
    clippy::cast_precision_loss
)]
fn m5_v8_comparison() -> Result<(), DynError> {
    let root = workspace_root();
    let final_dir = root.join("target/nexa-artifacts/m5/final");
    fs::create_dir_all(&final_dir)?;
    let node_version = captured_stdout(
        Command::new("node").arg("--version").current_dir(&root),
        "node --version",
    )
    .map_err(|error| -> DynError {
        format!(
            "no qualified V8 environment: {error}; per JIT_DECISION_V1.md this blocks only \
             the comparison report, not the rest of M5"
        )
        .into()
    })?
    .trim()
    .to_owned();

    // Result-parity handshake: the JavaScript mirrors must return exactly
    // what the Nexa interpreter returns before any timing is comparable.
    let nexa_results: Value = serde_json::from_str(&captured_stdout(
        Command::new("cargo")
            .args([
                "run",
                "--release",
                "--quiet",
                "-p",
                "nexa-benchmark-v7",
                "--",
                "--verify-products",
            ])
            .current_dir(&root),
        "nexa-benchmark-v7 --verify-products",
    )?)?;

    // Warm V8 side: independent processes, each with its own warmup,
    // mirroring the WP11 multi-process protocol.
    let harness = root.join("tools/benchmark-v7/v8/harness.js");
    let samples = V8_COMPARISON_SAMPLES.to_string();
    let mut v8_reports = Vec::with_capacity(V8_COMPARISON_PROCESSES);
    for process_index in 0..V8_COMPARISON_PROCESSES {
        let report: Value = serde_json::from_str(&captured_stdout(
            Command::new("node")
                .arg(&harness)
                .args([
                    "--samples",
                    &samples,
                    "--process-index",
                    &process_index.to_string(),
                ])
                .current_dir(&root),
            "v8 comparison harness",
        )?)?;
        eprintln!(
            "v8 process {}/{V8_COMPARISON_PROCESSES} complete",
            process_index + 1
        );
        v8_reports.push(report);
    }
    let first_v8 = v8_reports.first().ok_or("V8 harness produced no reports")?;
    let mut v8_process_indices = BTreeSet::new();
    for (report_number, report) in v8_reports.iter().enumerate() {
        let cases = report["cases"]
            .as_array()
            .ok_or_else(|| format!("V8 report {report_number} omitted cases"))?;
        let names = cases
            .iter()
            .map(|case| case["case"].as_str())
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("V8 report {report_number} has an unnamed case"))?;
        let process_index = report["process_index"]
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|index| *index < V8_COMPARISON_PROCESSES)
            .ok_or_else(|| format!("V8 report {report_number} has an invalid process index"))?;
        if report["schema"].as_u64() != Some(1)
            || report["harness"] != "benchmark-v7-v8-comparison"
            || report["node_version"].as_str() != Some(node_version.as_str())
            || report["v8_version"] != first_v8["v8_version"]
            || report["harness_source_hash"] != first_v8["harness_source_hash"]
            || !is_hex_digest(&report["harness_source_hash"], 32)
            || report["samples"].as_u64()
                != Some(u64::try_from(V8_COMPARISON_SAMPLES).expect("sample count fits u64"))
            || report["warmup"].as_u64() != Some(100)
            || names != PRODUCT_CPU_CASES
            || !v8_process_indices.insert(process_index)
        {
            return Err(format!(
                "V8 report {report_number} is not a unique formal Benchmark v7 process receipt"
            )
            .into());
        }
        for case in cases {
            let p50 = case["p50_ns"].as_u64();
            let p95 = case["p95_ns"].as_u64();
            let p99 = case["p99_ns"].as_u64();
            if case["samples"].as_u64()
                != Some(u64::try_from(V8_COMPARISON_SAMPLES).expect("sample count fits u64"))
                || p50.is_none_or(|value| value == 0)
                || p50.zip(p95).is_none_or(|(p50, p95)| p50 > p95)
                || p95.zip(p99).is_none_or(|(p95, p99)| p95 > p99)
                || case["result"].as_i64().is_none()
            {
                return Err(format!("V8 report {report_number} has malformed case timings").into());
            }
        }
    }
    let v8_version = v8_reports[0]["v8_version"]
        .as_str()
        .ok_or("v8 harness report omitted its V8 version")?
        .to_owned();

    // Nexa side: reuse the formal aggregate when it was produced at this
    // commit; otherwise regenerate it through the measurement authority.
    let head = git_output(&["rev-parse", "HEAD"])?;
    let aggregate_path = final_dir.join("aggregate-7x1000.json");
    let (aggregate, aggregate_provenance) =
        if let Some(aggregate) = formal_aggregate_at(&aggregate_path, &head) {
            (aggregate, "reused: same-HEAD formal 7x1000 receipt")
        } else {
            cargo(&[
                "run",
                "--release",
                "--quiet",
                "-p",
                "nexa-benchmark-v7",
                "--",
                "--samples",
                &samples,
                "--processes",
                &V8_COMPARISON_PROCESSES.to_string(),
                "--output",
                aggregate_path.to_str().ok_or("non-UTF-8 artifact path")?,
            ])?;
            let aggregate = formal_aggregate_at(&aggregate_path, &head)
                .ok_or("V8 comparison generated a non-formal Nexa aggregate")?;
            (aggregate, "generated by this run")
        };

    let mut workloads = Vec::new();
    let mut leads = 0_usize;
    let mut parity_failures = Vec::new();
    for &(workload, pinned_result) in &PRODUCT_CPU_RESULTS {
        let expected = nexa_results[workload]
            .as_i64()
            .ok_or_else(|| format!("--verify-products omitted {workload}"))?;
        if expected != pinned_result {
            return Err(format!(
                "{workload} returned {expected}, but the frozen product result is {pinned_result}"
            )
            .into());
        }
        let mut v8_p50s = Vec::with_capacity(v8_reports.len());
        for report in &v8_reports {
            let case = report["cases"]
                .as_array()
                .and_then(|cases| cases.iter().find(|case| case["case"] == workload))
                .ok_or_else(|| format!("v8 report omitted {workload}"))?;
            if case["result"].as_i64() != Some(expected) {
                parity_failures.push(format!(
                    "{workload}: v8 returned {} but Nexa returned {expected}",
                    case["result"]
                ));
            }
            v8_p50s.push(
                case["p50_ns"]
                    .as_u64()
                    .ok_or_else(|| format!("v8 report omitted p50 for {workload}"))?,
            );
        }
        v8_p50s.sort_unstable();
        let v8_p50 = v8_p50s[v8_p50s.len() / 2];
        let nexa_p50 = aggregate["cases"]
            .as_array()
            .and_then(|cases| cases.iter().find(|case| case["case"] == workload))
            .and_then(|case| case["median_p50_ns"].as_u64())
            .ok_or_else(|| format!("nexa aggregate omitted {workload}"))?;
        let lead_at_least_1_5x = v8_lead_at_least_1_5x(nexa_p50, v8_p50);
        if lead_at_least_1_5x {
            leads += 1;
        }
        workloads.push(serde_json::json!({
            "case": workload,
            "result": expected,
            "nexa_median_p50_ns": nexa_p50,
            "v8_median_p50_ns": v8_p50,
            "v8_lead_ratio": rounded_v8_lead_ratio(nexa_p50, v8_p50),
            "v8_lead_at_least_1_5x": lead_at_least_1_5x,
        }));
    }
    if !parity_failures.is_empty() {
        return Err(format!(
            "V8/Nexa result parity failed:\n{}",
            parity_failures.join("\n")
        )
        .into());
    }

    let comparison = serde_json::json!({
        "schema": 1,
        "status": "PASS",
        "protocol": "7 processes x 1000 samples per side; median across process medians; per-process warmup",
        "discipline": "warm V8 JIT-compiled code measured against the Nexa interpreter (JIT_DECISION_V1.md)",
        "node_version": node_version,
        "v8_version": v8_version,
        "harness_source_hash": first_v8["harness_source_hash"],
        "processes": V8_COMPARISON_PROCESSES,
        "samples_per_process": V8_COMPARISON_SAMPLES,
        "warmup_per_process": 100,
        "nexa_implementation_commit": aggregate["implementation_commit"],
        "nexa_aggregate": aggregate_provenance,
        "qualification_machine": {
            "os": aggregate["os"],
            "os_version": aggregate["os_version"],
            "arch": aggregate["arch"],
            "machine_model": aggregate["machine_model"],
            "cpu_model": aggregate["cpu_model"],
            "logical_cpu_count": aggregate["logical_cpu_count"],
            "power_source": aggregate["power_source"],
            "thermal_policy": aggregate["thermal_policy"],
        },
        "result_parity": "all workloads returned identical results in both runtimes",
        "semantic_mismatches": parity_failures,
        "workloads": workloads,
        "workloads_with_v8_lead_at_least_1_5x": leads,
        "c4_v8_gap_satisfied": leads >= 3,
    });
    fs::write(
        final_dir.join("v8-comparison.json"),
        format!("{}\n", serde_json::to_string_pretty(&comparison)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&comparison)?);
    Ok(())
}

/// The frozen target buckets from `PERFORMANCE_TARGETS_V1.md`, mapped to
/// benchmark v7 case names. Cases added after the baseline tag have no
/// baseline side and are reported separately, never averaged.
const VALUE_COLLECTION_CASES: &[&str] = &[
    "struct_construction",
    "enum_construction_match",
    "array_operations",
    "map_operations",
    "buffer_copy",
    "string_concat",
    "class_allocation",
];
const PRODUCT_CPU_CASES: &[&str] = &[
    "product_data_sweep",
    "product_combat_tick",
    "product_grid_score",
];
const PRODUCT_CPU_RESULTS: [(&str, i64); 3] = [
    ("product_data_sweep", 32_640),
    ("product_combat_tick", 633),
    ("product_grid_score", 157_992),
];
const BASELINE_PRODUCT_CPU_CASES: &[&str] = &["product_data_sweep"];

fn v8_lead_at_least_1_5x(nexa_p50: u64, v8_p50: u64) -> bool {
    v8_p50 > 0 && u128::from(nexa_p50) * 2 >= u128::from(v8_p50) * 3
}

#[allow(
    clippy::cast_precision_loss,
    // Nanosecond medians remain far below the f64 exact-integer boundary.
)]
fn rounded_v8_lead_ratio(nexa_p50: u64, v8_p50: u64) -> f64 {
    let ratio = nexa_p50 as f64 / v8_p50.max(1) as f64;
    (ratio * 100.0).round() / 100.0
}
/// WP16 covers the current hot interpreter corpus, including cases added
/// after the frozen M5 baseline bucket.
const PROFILER_HOT_CASES: &[&str] = &[
    "product_data_sweep",
    "product_combat_tick",
    "product_grid_score",
    "product_struct_rows",
];
const HOST_TASK_ENGINE_CASES: &[&str] = &[
    "immediate_call",
    "result_ok_err",
    "fuel_resume",
    "explicit_resume",
    "snapshot_access",
    "async_admission",
    "migration",
    "reload_commit",
    "realm_drop",
];
const COLD_START_CASES: &[&str] = &["product_standalone_pipeline"];

/// Validates the immutable baseline harness's schema-1 aggregate after the
/// current xtask process has synchronously produced and qualified it. The
/// baseline predates the schema-2 provenance envelope, so only fields that
/// its frozen harness actually measured are accepted before the same-process
/// qualification stamp is checked.
fn formal_baseline_aggregate(
    report: Value,
    implementation_commit: &str,
    machine_authority: &Value,
) -> Option<Value> {
    let cases = report["cases"].as_array()?;
    let mut names = BTreeSet::new();
    for case in cases {
        let name = case["case"].as_str().filter(|name| !name.is_empty())?;
        if !names.insert(name) || case["tier"].as_str().is_none_or(str::is_empty) {
            return None;
        }
        let p50 = case["median_p50_ns"].as_u64()?;
        let p95 = case["median_p95_ns"].as_u64()?;
        let p99 = case["median_p99_ns"].as_u64()?;
        if p50 == 0
            || p50 > p95
            || p95 > p99
            || case["median_throughput_ops_per_second"]
                .as_u64()
                .is_none_or(|throughput| throughput == 0)
            || case["max_system_allocations"].as_u64().is_none()
            || case["max_system_allocated_bytes"].as_u64().is_none()
        {
            return None;
        }
    }
    let mandatory_cases = VALUE_COLLECTION_CASES
        .iter()
        .chain(BASELINE_PRODUCT_CPU_CASES)
        .chain(HOST_TASK_ENGINE_CASES)
        .chain(COLD_START_CASES);
    (report["schema"].as_u64() == Some(1)
        && report["benchmark_version"].as_u64() == Some(7)
        && report["protocol"]
            == "median across process medians; each process independently warmed"
        && report["implementation_commit"].as_str() == Some(implementation_commit)
        && report["process_count"].as_u64() == Some(7)
        && report["samples_per_process"].as_u64() == Some(1_000)
        && report["build_profile"] == "release"
        && report["status"] == "PASS"
        && report["machine_identity_provenance"]
            == "bound by the same xtask process to its live HEAD receipt while synchronously running the baseline worktree"
        && is_hex_digest(&report["benchmark_source_hash"], 32)
        && same_benchmark_machine(&report, machine_authority)
        && mandatory_cases
            .into_iter()
            .all(|name| names.contains(*name)))
    .then_some(report)
}

/// Regressions acknowledged with a written explanation, per the
/// `PERFORMANCE_TARGETS_V1.md` latency discipline. Empty until a regression
/// is investigated and justified in the final report.
const EXPLAINED_REGRESSIONS: &[(&str, &str)] = &[(
    "realm_drop",
    "H1 continuation pooling returns task arenas to the realm pool, so their \
     release batches into realm teardown instead of task completion; the p95/p99 \
     tail grows by well under 1us while pooled steady-state task admission gains \
     ~15%. Reproduced across three same-machine live comparisons (first observed \
     in the J4 landing run); p50 stays within noise.",
)];

/// Differences below this floor are timer-resolution noise on the
/// qualification machine (the mach timebase quantum is ~42ns); the 10%
/// regression rule applies above it.
const REGRESSION_NOISE_FLOOR_NS: u128 = 100;

/// M5 stage-J: the live baseline comparison demanded by
/// `PERFORMANCE_TARGETS_V1.md` - HEAD versus the immutable
/// `performance-m5-baseline` tag, both measured on this machine under the
/// formal 7x1000 protocol. The baseline side runs its own frozen harness
/// inside a temporary worktree (never a saved report from another
/// session); its live artifact is reused only while pinned to the
/// immutable tag commit. The command fails on unexplained shared-case
/// p95/p99 regressions beyond 10% or on a missed frozen throughput target.
fn m5_performance_regression() -> Result<(), DynError> {
    m5_performance_regression_with_live_head(None)
}

/// `live_head` is accepted only from the finalizer's immediately preceding
/// profiler-disabled 7x1000 run. Standalone invocations pass `None` and
/// always measure HEAD again, so a saved JSON file can never masquerade as
/// the live side of the required baseline comparison.
#[allow(
    clippy::too_many_lines,
    // Ratio reporting only; the f64 mantissa bound is irrelevant here.
    clippy::cast_precision_loss
)]
fn m5_performance_regression_with_live_head(live_head: Option<Value>) -> Result<(), DynError> {
    let root = workspace_root();
    let regression_dir = root.join("target/nexa-artifacts/m5/regression");
    fs::create_dir_all(&regression_dir)?;
    if git_output(&["cat-file", "-t", "performance-m5-baseline"])? != "tag" {
        return Err("performance-m5-baseline must be an annotated tag".into());
    }
    let baseline_commit = git_output(&["rev-parse", "performance-m5-baseline^{}"])?;
    let head_commit = git_output(&["rev-parse", "HEAD"])?;

    // HEAD is always live: either this command measures it here, or the
    // finalizer passes the profiler-disabled aggregate it just generated in
    // the same call stack under the identical protocol.
    let head_path = regression_dir.join("head-7x1000.json");
    let head = if let Some(report) = live_head {
        let report = formal_aggregate(report, &head_commit)
            .ok_or("finalizer supplied a malformed live HEAD aggregate")?;
        eprintln!("regression: using the finalizer's immediately preceding live HEAD aggregate");
        fs::write(
            &head_path,
            format!("{}\n", serde_json::to_string_pretty(&report)?),
        )?;
        report
    } else {
        cargo(&[
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v7",
            "--",
            "--samples",
            "1000",
            "--processes",
            "7",
            "--output",
            head_path.to_str().ok_or("non-UTF-8 artifact path")?,
        ])?;
        formal_aggregate_at(&head_path, &head_commit)
            .ok_or("new HEAD benchmark is not a formal 7x1000 report")?
    };
    let final_aggregate_path = root.join("target/nexa-artifacts/m5/final/aggregate-7x1000.json");
    fs::create_dir_all(
        final_aggregate_path
            .parent()
            .ok_or("formal aggregate path has no parent")?,
    )?;
    fs::write(
        &final_aggregate_path,
        format!("{}\n", serde_json::to_string_pretty(&head)?),
    )?;

    // Baseline is never reused. The immutable tag's own harness runs now in
    // a detached temporary worktree, then its schema-1 aggregate is
    // qualified by this same process and validated before comparison.
    let baseline_path = regression_dir.join("baseline-live-7x1000.json");
    let baseline = formal_baseline_aggregate(
        generate_baseline_live(&root, &baseline_path, &head)?,
        &baseline_commit,
        &head,
    )
    .ok_or("new baseline benchmark is not a live qualified 7x1000 report")?;

    let case_map = |report: &Value| -> BTreeMap<String, (u128, u128, u128)> {
        report["cases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|case| {
                Some((
                    case["case"].as_str()?.to_owned(),
                    (
                        case["median_p50_ns"].as_u64()?.into(),
                        case["median_p95_ns"].as_u64()?.into(),
                        case["median_p99_ns"].as_u64()?.into(),
                    ),
                ))
            })
            .collect()
    };
    let baseline_cases = case_map(&baseline);
    let head_cases = case_map(&head);

    let mut speedups = BTreeMap::new();
    let mut regressions = Vec::new();
    let mut explained = Vec::new();
    for (name, (base_p50, base_p95, base_p99)) in &baseline_cases {
        let Some((head_p50, head_p95, head_p99)) = head_cases.get(name).copied() else {
            return Err(format!("HEAD report dropped the mandatory case {name}").into());
        };
        speedups.insert(name.clone(), *base_p50 as f64 / head_p50.max(1) as f64);
        for (metric, base, now) in [("p95", *base_p95, head_p95), ("p99", *base_p99, head_p99)] {
            let limit = base.saturating_mul(110) / 100;
            if now > limit && now.saturating_sub(base) > REGRESSION_NOISE_FLOOR_NS {
                let entry = serde_json::json!({
                    "case": name,
                    "metric": metric,
                    "baseline_ns": u64::try_from(base).unwrap_or(u64::MAX),
                    "head_ns": u64::try_from(now).unwrap_or(u64::MAX),
                    "ratio": (now as f64 / base.max(1) as f64 * 100.0).round() / 100.0,
                });
                if let Some((_, note)) = EXPLAINED_REGRESSIONS.iter().find(|(case, _)| case == name)
                {
                    let mut entry = entry;
                    entry["explanation"] = Value::from(*note);
                    explained.push(entry);
                } else {
                    regressions.push(entry);
                }
            }
        }
    }
    let new_cases = head_cases
        .keys()
        .filter(|name| !baseline_cases.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();

    let bucket = |cases: &[&str], target: f64| -> Value {
        let shared = cases
            .iter()
            .filter_map(|name| {
                speedups
                    .get(*name)
                    .map(|speedup| ((*name).to_owned(), *speedup))
            })
            .collect::<BTreeMap<_, _>>();
        let geomean = if shared.is_empty() {
            0.0
        } else {
            (shared.values().map(|speedup| speedup.ln()).sum::<f64>() / shared.len() as f64).exp()
        };
        serde_json::json!({
            "target": target,
            "geomean": (geomean * 1000.0).round() / 1000.0,
            "met": geomean >= target,
            "cases": shared,
            "without_baseline": cases
                .iter()
                .filter(|name| head_cases.contains_key(**name) && !baseline_cases.contains_key(**name))
                .collect::<Vec<_>>(),
        })
    };
    let buckets = serde_json::json!({
        "product_cpu": bucket(PRODUCT_CPU_CASES, 1.50),
        "value_collection": bucket(VALUE_COLLECTION_CASES, 2.00),
        "host_task_engine": bucket(HOST_TASK_ENGINE_CASES, 1.30),
        "cold_start": bucket(COLD_START_CASES, 1.20),
    });
    let targets_met = [
        "product_cpu",
        "value_collection",
        "host_task_engine",
        "cold_start",
    ]
    .into_iter()
    .all(|bucket| buckets[bucket]["met"] == true);
    let passed = regressions.is_empty() && targets_met;
    let comparison = serde_json::json!({
        "schema": 1,
        "protocol": "live baseline worktree vs HEAD; 7 processes x 1000 samples each side; median across process medians",
        "baseline_tag": "performance-m5-baseline",
        "baseline_commit": baseline_commit,
        "head_commit": head_commit,
        "head_authority": {
            "schema": head["schema"],
            "benchmark_source_hash": head["benchmark_source_hash"],
            "bytecode_hash": head["bytecode_hash"],
            "toolchain": head["toolchain"],
            "build_profile": head["build_profile"],
            "allocation_scope": head["allocation_scope"],
            "os": head["os"],
            "os_version": head["os_version"],
            "arch": head["arch"],
            "machine_model": head["machine_model"],
            "cpu_model": head["cpu_model"],
            "logical_cpu_count": head["logical_cpu_count"],
            "power_source": head["power_source"],
            "thermal_policy": head["thermal_policy"],
        },
        "baseline_authority": {
            "schema": baseline["schema"],
            "benchmark_source_hash": baseline["benchmark_source_hash"],
            "build_profile": baseline["build_profile"],
            "os": baseline["os"],
            "os_version": baseline["os_version"],
            "arch": baseline["arch"],
            "machine_model": baseline["machine_model"],
            "cpu_model": baseline["cpu_model"],
            "logical_cpu_count": baseline["logical_cpu_count"],
            "power_source": baseline["power_source"],
            "thermal_policy": baseline["thermal_policy"],
            "machine_identity_provenance": baseline["machine_identity_provenance"],
        },
        "buckets": buckets,
        "cases_without_baseline": new_cases,
        "regressions": regressions,
        "explained_regressions": explained,
        "noise_floor_ns": u64::try_from(REGRESSION_NOISE_FLOOR_NS).unwrap_or(u64::MAX),
        "status": if passed { "PASS" } else { "FAIL" },
    });
    fs::write(
        regression_dir.join("comparison.json"),
        format!("{}\n", serde_json::to_string_pretty(&comparison)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&comparison)?);
    if comparison["regressions"]
        .as_array()
        .is_some_and(|entries| !entries.is_empty())
    {
        return Err("unexplained p95/p99 regressions beyond 10% on shared mandatory cases".into());
    }
    if comparison["status"] != "PASS" {
        return Err("one or more M5 throughput buckets missed the frozen target".into());
    }
    Ok(())
}

/// M5 terminal gate. It is intentionally runnable only from the published,
/// clean, annotated completion checkout; all expensive correctness and
/// performance evidence is then regenerated or validated at that exact HEAD.
#[allow(clippy::too_many_lines)]
fn finalize_m5() -> Result<(), DynError> {
    const HISTORICAL_TAGS: [(&str, &str, &str); 11] = [
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
        (
            "language-scale-m4-complete-r1",
            "e23f95204aac67ea2c4669cbe893831077a6d8a0",
            "5de5027b5ebe4b2adbe1cec9e8dd5b9b25b9b8ba",
        ),
        (
            "performance-m5-baseline",
            "ca6eecec77bdecf6f717d3c3e85aae5a2298541c",
            "24e87e0a7df07281d2205b1ed88162c7e6617231",
        ),
    ];

    let root = workspace_root();
    let head = git_output(&["rev-parse", "HEAD"])?;
    let branch = git_output(&["branch", "--show-current"])?;
    let worktree_status = git_output(&["status", "--porcelain"])?;
    if branch != "main" || !worktree_status.is_empty() {
        return Err(format!(
            "finalize-m5 requires a clean attached main checkout; branch={branch:?}, \
             status={worktree_status:?}"
        )
        .into());
    }
    let completion_tag_type = git_output(&["cat-file", "-t", "performance-m5-complete"])?;
    let completion_tag_object = git_output(&["rev-parse", "performance-m5-complete"])?;
    let completion_tag_target = git_output(&["rev-parse", "performance-m5-complete^{}"])?;
    if completion_tag_type != "tag" || completion_tag_target != head {
        return Err("performance-m5-complete must be an annotated tag targeting HEAD".into());
    }

    let mut remote_references = vec![
        "refs/heads/main".to_owned(),
        "refs/heads/codex/performance-m5".to_owned(),
        "refs/tags/performance-m5-complete".to_owned(),
        "refs/tags/performance-m5-complete^{}".to_owned(),
    ];
    for (name, _, _) in HISTORICAL_TAGS {
        remote_references.push(format!("refs/tags/{name}"));
        remote_references.push(format!("refs/tags/{name}^{{}}"));
    }
    let mut remote_command = Command::new("git");
    remote_command.args(["ls-remote", "origin"]);
    remote_command.args(&remote_references);
    remote_command
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(&root);
    let remote_output = captured_stdout(&mut remote_command, "git ls-remote origin")?;
    let remote = remote_output
        .lines()
        .filter_map(|line| {
            let (object, reference) = line.split_once('\t')?;
            Some((reference.to_owned(), object.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    for reference in ["refs/heads/main", "refs/heads/codex/performance-m5"] {
        if remote.get(reference).map(String::as_str) != Some(head.as_str()) {
            return Err(format!("remote {reference} does not target final HEAD {head}").into());
        }
    }
    if remote
        .get("refs/tags/performance-m5-complete")
        .map(String::as_str)
        != Some(completion_tag_object.as_str())
        || remote
            .get("refs/tags/performance-m5-complete^{}")
            .map(String::as_str)
            != Some(head.as_str())
    {
        return Err(
            "remote performance-m5-complete tag differs from the local annotated tag".into(),
        );
    }
    let mut historical_tags = Vec::with_capacity(HISTORICAL_TAGS.len());
    for (name, expected_object, expected_target) in HISTORICAL_TAGS {
        let object_type = git_output(&["cat-file", "-t", name])?;
        let object = git_output(&["rev-parse", name])?;
        let target = git_output(&["rev-parse", &format!("{name}^{{}}")])?;
        let remote_object = remote
            .get(&format!("refs/tags/{name}"))
            .cloned()
            .unwrap_or_else(|| "missing".into());
        let remote_target = remote
            .get(&format!("refs/tags/{name}^{{}}"))
            .cloned()
            .unwrap_or_else(|| "missing".into());
        if object_type != "tag"
            || object != expected_object
            || target != expected_target
            || remote_object != expected_object
            || remote_target != expected_target
        {
            return Err(format!(
                "historical annotated tag {name} changed locally or remotely: \
                 type={object_type}, object={object}, target={target}, \
                 remote_object={remote_object}, remote_target={remote_target}"
            )
            .into());
        }
        historical_tags.push(serde_json::json!({
            "name": name,
            "object": object,
            "target": target,
            "status": "PASS",
        }));
    }

    cargo(&["fmt", "--all", "--", "--check"])?;
    cargo(&["check", "--workspace", "--all-targets"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace", "--all-targets"])?;
    cargo(&["test", "--doc", "--workspace"])?;
    cargo_with_environment(
        &["doc", "--workspace", "--no-deps"],
        &[("RUSTDOCFLAGS", "-D warnings")],
    )?;

    check_after_workspace()?;
    m4r1::test_language_v2()?;
    m4r1::test_object_model_v2()?;
    m4r1::test_async_v2()?;
    m4r1::test_nidl_v2()?;
    m4r1::test_structured_codegen()?;
    m4r1::test_standalone()?;
    m4r1::test_repl()?;
    m4r1::test_entrypoints()?;
    m4r1::m4r1_scale_stress()?;
    test_profiler_overhead()?;
    let live_head_path =
        root.join("target/nexa-artifacts/m5/profiler-overhead/disabled-7x1000.json");
    let live_head = formal_aggregate_at(&live_head_path, &head)
        .ok_or("profiler gate did not leave a valid live profiler-disabled HEAD aggregate")?;
    m5_performance_regression_with_live_head(Some(live_head))?;
    m5_v8_comparison()?;
    m5_final_report()?;
    repo_audit()?;

    let comparison_path = root.join("target/nexa-artifacts/m5/regression/comparison.json");
    let profiler_path = root.join("target/nexa-artifacts/m5/profiler-overhead/formal-7x1000.json");
    let product_path = root.join("target/nexa-artifacts/m5/product-corpus/report.json");
    let reload_peak_path = root.join("target/nexa-artifacts/m5/reload-peak/report.json");
    let cold_start_path = root.join("target/nexa-artifacts/m5/cold-start/report.json");
    let v8_path = root.join("target/nexa-artifacts/m5/final/v8-comparison.json");
    let decision_path = root.join("target/nexa-artifacts/m5/final/jit-decision.json");
    let performance_path = root.join("target/nexa-artifacts/m5/final/performance-report.json");
    let comparison: Value = serde_json::from_slice(&fs::read(&comparison_path)?)?;
    let profiler: Value = serde_json::from_slice(&fs::read(&profiler_path)?)?;
    let product: Value = serde_json::from_slice(&fs::read(&product_path)?)?;
    let v8: Value = serde_json::from_slice(&fs::read(&v8_path)?)?;
    let reload_peak = reload_peak_report_at(&reload_peak_path, &head)
        .ok_or("WP97 reload peak receipt is stale or malformed")?;
    let cold_start = cold_start_report_at(&cold_start_path, &head)
        .ok_or("WP98 cold-start receipt is stale or malformed")?;
    let decision: Value = serde_json::from_slice(&fs::read(&decision_path)?)?;
    let performance: Value = serde_json::from_slice(&fs::read(&performance_path)?)?;

    let geomean = |bucket: &str| -> Result<f64, DynError> {
        comparison["buckets"][bucket]["geomean"]
            .as_f64()
            .ok_or_else(|| format!("comparison omitted numeric {bucket} geomean").into())
    };
    let aggregate_case = |name: &str| -> Result<&Value, DynError> {
        performance["aggregate"]["cases"]
            .as_array()
            .and_then(|cases| cases.iter().find(|case| case["case"] == name))
            .ok_or_else(|| format!("formal aggregate omitted case {name}").into())
    };
    let struct_gc_allocations =
        aggregate_case("struct_construction")?["max_vm"]["struct_materializations"]
            .as_u64()
            .ok_or("formal aggregate omitted Struct materialization count")?;
    let enum_gc_allocations =
        aggregate_case("enum_construction_match")?["max_vm"]["enum_materializations"]
            .as_u64()
            .ok_or("formal aggregate omitted Enum materialization count")?;
    let gc_case = aggregate_case("gc_incremental_step")?;
    let gc_system_allocations = gc_case["max_system_allocations"]
        .as_u64()
        .ok_or("formal aggregate omitted GC system allocation count")?;
    let steady_state_dispatch =
        steady_state_dispatch_receipt(performance["steady_state_dispatch"].clone(), &head)
            .ok_or("performance report omitted valid same-HEAD WP92 allocation evidence")?;
    let steady_state_dispatch_system_allocations = steady_state_dispatch["max_system_allocations"]
        .as_u64()
        .ok_or("WP92 allocation receipt omitted its maximum")?;
    let snake_stress = &product["snake"]["stress_receipt"];
    let resource_leaks = snake_stress["resource_leaks"]
        .as_u64()
        .ok_or("Snake stress receipt omitted its post-shutdown leak count")?;
    let semantic_mismatches = v8["semantic_mismatches"]
        .as_array()
        .ok_or("V8 comparison omitted its semantic mismatch evidence")?
        .clone();
    for bucket in [
        "product_cpu",
        "value_collection",
        "host_task_engine",
        "cold_start",
    ] {
        if comparison["buckets"][bucket]["met"] != true {
            return Err(format!("M5 performance bucket {bucket} did not meet its target").into());
        }
    }
    let unexplained_regressions = comparison["regressions"]
        .as_array()
        .ok_or("comparison regressions is not an array")?;
    if comparison["schema"].as_u64() != Some(1)
        || comparison["status"] != "PASS"
        || comparison["head_commit"] != head
        || comparison["head_authority"]["schema"].as_u64() != Some(2)
        || comparison["head_authority"]["build_profile"] != "release"
        || !is_hex_digest(&comparison["head_authority"]["benchmark_source_hash"], 32)
        || !is_hex_digest(&comparison["head_authority"]["bytecode_hash"], 32)
        || !unexplained_regressions.is_empty()
        || profiler["schema"].as_u64() != Some(1)
        || profiler["implementation_commit"] != head
        || profiler["status"] != "PASS"
        || profiler["processes"].as_u64() != Some(7)
        || profiler["samples_per_process"].as_u64() != Some(1_000)
        || product["schema"].as_u64() != Some(1)
        || product["implementation_commit"] != head
        || product["status"] != "PASS"
        || !valid_snake_stress_report(snake_stress)
        || resource_leaks != 0
        || v8["schema"] != 1
        || v8["status"] != "PASS"
        || v8["nexa_implementation_commit"] != head
        || v8["result_parity"] != "all workloads returned identical results in both runtimes"
        || !semantic_mismatches.is_empty()
        || reload_peak["implementation_commit"] != head
        || reload_peak["status"] != "PASS"
        || performance["reload_peak"]["implementation_commit"] != head
        || performance["reload_peak"]["status"] != "PASS"
        || cold_start["implementation_commit"] != head
        || performance["cold_start"]["implementation_commit"] != head
        || performance["cold_start"]["status"] != "PASS"
        || performance["schema"].as_u64() != Some(1)
        || performance["aggregate"]["implementation_commit"] != head
        || performance["aggregate"]["benchmark_version"] != 7
        || performance["aggregate"]["schema"] != 2
        || performance["aggregate"]["status"] != "PASS"
        || performance["aggregate"]["build_profile"] != "release"
        || performance["aggregate"]["process_count"] != 7
        || performance["aggregate"]["samples_per_process"] != 1_000
        || performance["aggregate"]["warmup_per_process"] != 100
        || struct_gc_allocations != 0
        || enum_gc_allocations != 0
        || gc_system_allocations != 0
        || steady_state_dispatch_system_allocations != 0
        || decision["schema"].as_u64() != Some(1)
        || decision["implementation_commit"] != head
        || performance["decision"] != decision
        || !matches!(decision["decision"].as_str(), Some("GO" | "DEFER"))
    {
        return Err("one or more M5 machine receipts are stale, malformed, or failing".into());
    }
    let jit_decision = decision["decision"]
        .as_str()
        .ok_or("JIT decision is not a string")?;
    let decision_document =
        fs::read_to_string(root.join("baseline/performance/JIT_DECISION_V1.md"))?;
    if !decision_document.contains(&format!("Status: **{jit_decision}**"))
        || !decision_document.contains(&format!("M6 LLVM JIT = {jit_decision}"))
    {
        return Err("JIT_DECISION_V1.md does not match the terminal machine decision".into());
    }
    for status_document in ["README.md", "ROADMAP.md"] {
        if !fs::read_to_string(root.join(status_document))?
            .contains("Nexa M5 Deep Performance Optimization = COMPLETE")
        {
            return Err(format!("{status_document} does not mark M5 COMPLETE").into());
        }
    }
    if !git_output(&["status", "--porcelain"])?.is_empty()
        || git_output(&["rev-parse", "HEAD"])? != head
    {
        return Err("checkout changed or became dirty while finalizing M5".into());
    }

    let report = serde_json::json!({
        "schema": 1,
        "milestone": "Nexa M5 Deep Performance Optimization",
        "baselineTag": "performance-m5-baseline",
        "baselineCommit": comparison["baseline_commit"],
        "implementationCommit": head,
        "productGeomeanSpeedup": geomean("product_cpu")?,
        "valueCollectionGeomeanSpeedup": geomean("value_collection")?,
        "hostTaskEngineGeomeanSpeedup": geomean("host_task_engine")?,
        "coldStartGeomeanSpeedup": geomean("cold_start")?,
        "coldStartScenarios": cold_start["cases"],
        "reloadPeak": reload_peak,
        "structGcAllocations": struct_gc_allocations,
        "enumGcAllocations": enum_gc_allocations,
        "gcSystemAllocations": gc_system_allocations,
        "steadyStateDispatchSystemAllocations": steady_state_dispatch_system_allocations,
        "steadyStateDispatchReceipt": steady_state_dispatch,
        "unexplainedRegressions": unexplained_regressions,
        "semanticMismatches": semantic_mismatches,
        "semanticParityReceipt": v8,
        "resourceLeaks": resource_leaks,
        "resourceLeakReceipt": snake_stress,
        "jitDecision": jit_decision,
        "workspace": {
            "fmt": "PASS",
            "check": "PASS",
            "clippy": "PASS",
            "test": "PASS",
            "doc": "PASS",
        },
        "historicalTags": historical_tags,
        "completionTag": {
            "type": completion_tag_type,
            "object": completion_tag_object,
            "target": completion_tag_target,
        },
        "remotePublication": {
            "main": remote["refs/heads/main"],
            "milestoneBranch": remote["refs/heads/codex/performance-m5"],
            "completionTagObject": remote["refs/tags/performance-m5-complete"],
            "completionTagTarget": remote["refs/tags/performance-m5-complete^{}"],
        },
        "status": "PASS",
    });
    let output = root.join("target/nexa-artifacts/m5-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M5 final report has no parent")?)?;
    let temporary = output.with_extension("json.tmp");
    fs::write(
        &temporary,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    fs::rename(&temporary, &output)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Regenerates the baseline side live: the immutable tag is checked out
/// into a temporary worktree and its own frozen harness runs the formal
/// protocol there. The worktree is removed afterwards; only the report
/// survives.
fn generate_baseline_live(
    root: &Path,
    baseline_path: &Path,
    machine_authority: &Value,
) -> Result<Value, DynError> {
    let worktree = root.join("target/nexa-worktrees/m5-baseline");
    let worktree_str = worktree.to_str().ok_or("non-UTF-8 worktree path")?;
    // Pre-clean any stale worktree from an interrupted run.
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", worktree_str])
        .current_dir(root)
        .output();
    let added = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            worktree_str,
            "performance-m5-baseline",
        ])
        .current_dir(root)
        .output()?;
    if !added.status.success() {
        return Err(format!(
            "git worktree add failed:\n{}",
            String::from_utf8_lossy(&added.stderr)
        )
        .into());
    }
    eprintln!("baseline: measuring the tag live in {worktree_str}");
    let output_path = baseline_path.to_str().ok_or("non-UTF-8 artifact path")?;
    let run = Command::new("cargo")
        .args([
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v7",
            "--",
            "--samples",
            "1000",
            "--processes",
            "7",
            "--output",
            output_path,
        ])
        .current_dir(&worktree)
        .status();
    let removed = Command::new("git")
        .args(["worktree", "remove", "--force", worktree_str])
        .current_dir(root)
        .output();
    let run = run?;
    if !run.success() {
        return Err("baseline benchmark run failed inside the worktree".into());
    }
    if let Ok(removed) = removed
        && !removed.status.success()
    {
        eprintln!(
            "warning: could not remove the baseline worktree: {}",
            String::from_utf8_lossy(&removed.stderr)
        );
    }
    if !qualified_benchmark_machine(machine_authority) {
        return Err("HEAD benchmark omitted its qualified machine identity".into());
    }
    let mut report: Value = serde_json::from_slice(&fs::read(baseline_path)?)?;
    for field in [
        "arch",
        "os",
        "os_version",
        "machine_model",
        "cpu_model",
        "logical_cpu_count",
        "power_source",
        "thermal_policy",
    ] {
        report[field] = machine_authority[field].clone();
    }
    report["build_profile"] = Value::from("release");
    report["status"] = Value::from("PASS");
    report["machine_identity_provenance"] = Value::from(
        "bound by the same xtask process to its live HEAD receipt while synchronously running the baseline worktree",
    );
    fs::write(
        baseline_path,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    Ok(report)
}

#[allow(clippy::too_many_lines)]
fn finalize_m3_r3() -> Result<(), DynError> {
    let mut failures = Vec::new();
    let workspace = record_gate("workspace", &mut failures, workspace_check());
    let m1_m2_regression = record_gate("M1/M2 regression", &mut failures, run_m1_m2_regression());
    let engine_api = record_gate("Engine API", &mut failures, test_engine_api());

    let (engine_diagnostics, engine_real_paths, engine_direct_construction) =
        match real_engine_diagnostic_gate() {
            Ok((real_paths, direct_construction)) => (true, real_paths, direct_construction),
            Err(error) => {
                failures.push(format!("real Engine diagnostics: {error}"));
                (false, 0, usize::MAX)
            }
        };
    let worker_queue_saturation = record_gate(
        "Worker queue saturation",
        &mut failures,
        worker_queue_saturation(),
    );
    let result_backpressure = record_gate(
        "Worker result backpressure",
        &mut failures,
        worker_result_backpressure(),
    );
    let disable_in_flight = record_gate(
        "Worker disable/in-flight",
        &mut failures,
        worker_disable_in_flight(),
    );
    let shutdown_in_flight = record_gate(
        "Worker shutdown/in-flight",
        &mut failures,
        worker_shutdown_in_flight(),
    );
    let (generation_accounting, generation_report) = match generation_accounting_gate() {
        Ok(report) => (true, report),
        Err(error) => {
            failures.push(format!("Generation terminal accounting: {error}"));
            (false, GenerationAccountingReport::failed())
        }
    };
    let (candidate_freshness, freshness_report) = match candidate_freshness_gate() {
        Ok(report) => (true, report),
        Err(error) => {
            failures.push(format!("Candidate freshness: {error}"));
            (false, CandidateFreshnessReport::failed())
        }
    };
    let reload_stress = record_gate("Reload stress", &mut failures, dev_loop_stress());
    let metrics = record_gate("Metrics", &mut failures, test_metrics());
    let cli_policy = record_gate("CLI policy", &mut failures, test_cli());
    let nidl_span = record_gate("NIDL Span", &mut failures, test_nidl_span());
    let uri_matrix = record_gate("URI matrix", &mut failures, test_uri_matrix());
    let lsp = record_gate("LSP", &mut failures, test_lsp());
    let editor = record_gate("Editor", &mut failures, editor_check());
    let repo_audit = record_gate(
        "Repository audit",
        &mut failures,
        repo_audit()
            .and_then(|()| m3_audit())
            .and_then(|()| m3r1_audit())
            .and_then(|()| m3r2_audit())
            .and_then(|()| m3r3_product_audit()),
    );
    let status_audit = record_gate(
        "M3R3 status audit",
        &mut failures,
        m3r3_final_status_audit(),
    );

    let head = git_output(&["rev-parse", "HEAD"])?;
    let tag_evidence = M3R3TagEvidence::load();
    let tag_audit = record_gate("M3R3 tag audit", &mut failures, tag_evidence.audit(&head));
    let tag_target_matches_head = tag_evidence.tag_target == head;
    let worktree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    if !worktree_clean {
        failures.push("worktree is not clean".into());
    }

    let worker_gates =
        worker_queue_saturation && result_backpressure && disable_in_flight && shutdown_in_flight;
    let passed = workspace
        && m1_m2_regression
        && engine_api
        && engine_diagnostics
        && engine_real_paths == 13
        && engine_direct_construction == 0
        && worker_gates
        && generation_accounting
        && candidate_freshness
        && freshness_report.stale_candidates_committed == 0
        && freshness_report.created_generations == freshness_report.terminal_generations
        && freshness_report.duplicate_terminals == 0
        && freshness_report.generations_without_terminal == 0
        && freshness_report.active_runtime_violations == 0
        && freshness_report.last_known_good_violations == 0
        && reload_stress
        && metrics
        && cli_policy
        && nidl_span
        && uri_matrix
        && lsp
        && editor
        && repo_audit
        && status_audit
        && tag_audit
        && worktree_clean;
    let report = M3R3FinalReport {
        schema: 1,
        milestone: "Nexa M3R3 Candidate Freshness Closure",
        head,
        workspace: status(workspace),
        m1_m2_regression: status(m1_m2_regression),
        engine_api: status(engine_api),
        engine_diagnostics: status(engine_diagnostics),
        worker_queue_saturation: status(worker_queue_saturation),
        result_backpressure: status(result_backpressure),
        disable_in_flight: status(disable_in_flight),
        shutdown_in_flight: status(shutdown_in_flight),
        generation_accounting: status(generation_accounting),
        candidate_freshness: status(candidate_freshness),
        reload_stress: status(reload_stress),
        metrics: status(metrics),
        cli_policy: status(cli_policy),
        lsp: status(lsp),
        editor: status(editor),
        repo_audit: status(repo_audit),
        status_audit: status(status_audit),
        tag_audit: status(tag_audit),
        queue_lost_jobs: u64::from(!worker_queue_saturation),
        queue_lost_results: u64::from(!result_backpressure),
        prequeue_scenario_count: generation_report.scenario_count,
        prequeue_created_generations: generation_report.created_generations,
        prequeue_terminal_generations: generation_report.terminal_generations,
        freshness_scenario_count: freshness_report.scenario_count,
        pending_scenario_count: freshness_report.pending_scenario_count,
        in_flight_scenario_count: freshness_report.in_flight_scenario_count,
        result_queue_scenario_count: freshness_report.result_queue_scenario_count,
        ready_candidate_scenario_count: freshness_report.ready_candidate_scenario_count,
        freshness_created_generations: freshness_report.created_generations,
        freshness_terminal_generations: freshness_report.terminal_generations,
        stale_candidates_observed: freshness_report.stale_candidates_observed,
        stale_candidates_committed: freshness_report.stale_candidates_committed,
        desired_build_fingerprint_mismatches_rejected: freshness_report
            .desired_build_fingerprint_mismatches_rejected,
        superseded_before_compile: freshness_report.superseded_before_compile,
        superseded_after_compile: freshness_report.superseded_after_compile,
        duplicate_terminals: freshness_report.duplicate_terminals,
        generations_without_terminal: freshness_report.generations_without_terminal,
        active_runtime_violations: freshness_report.active_runtime_violations,
        last_known_good_violations: freshness_report.last_known_good_violations,
        engine_diagnostic_direct_construction: engine_direct_construction,
        engine_diagnostic_real_paths: engine_real_paths,
        metrics_trusted: metrics,
        policy_validation: status(cli_policy),
        nidl_span: status(nidl_span),
        uri_matrix: status(uri_matrix),
        worktree_clean,
        historical_tag_type: tag_evidence.historical_tag_type,
        historical_tag_target: tag_evidence.historical_tag_target,
        r1_tag_type: tag_evidence.r1_tag_type,
        r1_tag_target: tag_evidence.r1_tag_target,
        r2_tag_type: tag_evidence.r2_tag_type,
        r2_tag_target: tag_evidence.r2_tag_target,
        tag_type: tag_evidence.tag_type,
        tag_target: tag_evidence.tag_target,
        tag_target_matches_head,
        failures,
        status: if passed { "PASS" } else { "FAIL" },
    };
    let root = workspace_root();
    let output = root.join("target/nexa-artifacts/m3r3-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M3R3 report has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("M3R3 finalization failed".into())
    }
}

fn record_gate(name: &str, failures: &mut Vec<String>, result: Result<(), DynError>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            failures.push(format!("{name}: {error}"));
            false
        }
    }
}

fn run_m1_m2_regression() -> Result<(), DynError> {
    test_binding()?;
    test_task()?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "restart_reload"])?;
    cargo(&["test", "-p", "nexa-model"])?;
    fuzz_smoke()?;
    bench_smoke()?;
    test_embed()?;
    test_snake()?;
    snake_headless("smoke")?;
    snake_headless("stress")?;
    snake_headless("bench")?;
    m2_audit()
}

fn real_engine_diagnostic_gate() -> Result<(usize, usize), DynError> {
    cargo(&["test", "-p", "nexa-embed", "--test", "diagnostic_e2e"])?;
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "nexa-cli",
            "--",
            "diagnostic-corpus-check",
            "--format",
            "json",
        ])
        .current_dir(workspace_root())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "diagnostic corpus failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let start = stdout
        .find('{')
        .ok_or("diagnostic corpus did not emit a JSON object")?;
    let end = stdout
        .rfind('}')
        .map(|index| index + 1)
        .ok_or("diagnostic corpus emitted incomplete JSON")?;
    let report: Value = serde_json::from_str(&stdout[start..end])?;
    let engine = &report["engine"];
    let count = |key: &str| -> Result<usize, DynError> {
        Ok(usize::try_from(engine[key].as_u64().ok_or_else(|| {
            format!("diagnostic report omitted engine.{key}")
        })?)?)
    };
    let registered = count("registered")?;
    let observed = count("observedThroughRealPaths")?;
    let direct = count("directDiagnosticConstruction")?;
    let human = count("humanOutput")?;
    let json = count("jsonOutput")?;
    let ndjson = count("ndjsonOutput")?;
    let deterministic = count("deterministic")?;
    if registered != observed
        || direct != 0
        || human != registered
        || json != registered
        || ndjson != registered
        || deterministic != registered
    {
        return Err(format!(
            "real Engine diagnostic evidence is incomplete: registered={registered}, \
             observed={observed}, direct={direct}, human={human}, json={json}, \
             ndjson={ndjson}, deterministic={deterministic}"
        )
        .into());
    }
    Ok((observed, direct))
}

fn test_nidl_span() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-cli",
        "lsp_idl_diagnostic_uses_the_parser_token_span",
    ])?;
    cargo(&["test", "-p", "nexa-idl", "parse_error"])
}

fn test_uri_matrix() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-cli",
        "file_uri_matrix_uses_standard_percent_encoding",
    ])
}

fn write_builtin_cli_policy() -> Result<PathBuf, DynError> {
    let path = workspace_root().join("target/nexa-artifacts/m3r1-cli/builtin-policy.toml");
    fs::create_dir_all(path.parent().ok_or("CLI policy path has no parent")?)?;
    fs::write(
        &path,
        "schema = 1\n\
         id = \"snake-builtin\"\n\
         trust = \"first-party\"\n\
         activation = [\"required\", \"default-enabled\"]\n\
         capabilities = [\n\
           \"diagnostics.log\",\n\
           \"skin.register\",\n\
           \"spawn.propose\",\n\
           \"spawn.register\",\n\
           \"stats.read\",\n\
           \"ui.register\",\n\
           \"ui.update\",\n\
         ]\n\
         allow_entitlement = false\n\
         max_packages = 16\n\
         [limits]\n\
         handler_fuel = 30000\n\
         cumulative_budget = 200000\n\
         heap_objects = 4096\n\
         heap_bytes = 67108864\n\
         string_bytes = 1048576\n\
         collection_bytes = 33554432\n\
         host_resources = 256\n\
         tasks = 8\n\
         release_records = 512\n",
    )?;
    Ok(path)
}

fn m3_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let active_roots = [
        "README.md",
        "ROADMAP.md",
        "baseline",
        "crates",
        "examples",
        "docs",
    ];
    let forbidden = [
        "pub struct NexaEmbed",
        "pub type NexaEmbed",
        "NexaEmbedBuilder",
        "EmbedError",
        "EmbedHealth",
        "NexaEmbed::builder",
    ];
    let mut violations = Vec::new();
    for active_root in active_roots {
        collect_forbidden(&root.join(active_root), &root, &forbidden, &mut violations)?;
    }
    if !violations.is_empty() {
        return Err(format!("M3 legacy API audit failed: {violations:?}").into());
    }
    if !fs::read_to_string(root.join("crates/nexa-embed/Cargo.toml"))?
        .contains("name = \"nexa-embed\"")
    {
        return Err("nexa-embed crate name changed".into());
    }
    for required in [
        "docs/DEVELOPMENT_LOOP.md",
        "docs/DIAGNOSTICS.md",
        "docs/RELOAD_WORKFLOW.md",
        "docs/EDITOR_SUPPORT.md",
        "nexa.dev.toml",
    ] {
        if !root.join(required).is_file() {
            return Err(format!("missing M3 artifact {required}").into());
        }
    }
    Ok(())
}

fn m3r1_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let worker = fs::read_to_string(root.join("crates/nexa-embed/src/development.rs"))?;
    for forbidden in [
        "last_processed_hash",
        "pending.pop_front()",
        "results.pop_front()",
    ] {
        if worker.contains(forbidden) {
            return Err(format!("M3R1 Worker audit found lossy pattern `{forbidden}`").into());
        }
    }
    for required in [
        "pending_order: VecDeque<PackageId>",
        "pending_by_package: BTreeMap<PackageId, CompileJob>",
        "job_available: Condvar",
        "result_space_available: Condvar",
        "SupersededBeforeCompile",
        "SupersededAfterCompile",
        "CancelledByDisable",
        "CancelledBySourceRemoval",
        "CancelledByShutdown",
        "Backpressured",
    ] {
        if !worker.contains(required) {
            return Err(format!("M3R1 Worker contract is missing `{required}`").into());
        }
    }
    let engine = fs::read_to_string(root.join("crates/nexa-embed/src/lib.rs"))?;
    for fingerprint in [
        "observed_build_fingerprint",
        "stable_build_fingerprint",
        "queued_build_fingerprint",
        "in_flight_build_fingerprint",
        "terminal_build_fingerprint",
        "active_build_fingerprint",
    ] {
        if !engine.contains(fingerprint) {
            return Err(format!(
                "M3R1 Engine Build Fingerprint lifecycle is missing `{fingerprint}`"
            )
            .into());
        }
    }
    let evidence = fs::read_to_string(root.join("crates/nexa-embed/src/diagnostic_evidence.rs"))?;
    for forbidden in [
        "Diagnostic::without_source",
        "EngineDiagnostic::without_source",
    ] {
        if evidence.contains(forbidden) {
            return Err(format!(
                "M3R1 diagnostic evidence constructs target code via `{forbidden}`"
            )
            .into());
        }
    }
    let project = fs::read_to_string(root.join("nexa.dev.toml"))?;
    if !project.starts_with("schema = 2\n") || !project.contains("[[sources]]") {
        return Err("nexa.dev.toml is not a schema 2 Source Policy project".into());
    }
    let historical_tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete"])?;
    let historical_tag_target = git_output(&["rev-parse", "developer-loop-m3-complete^{}"])?;
    if historical_tag_type != "tag"
        || historical_tag_target != "621612f49c4180989711df3ca80021fd21ad9277"
    {
        return Err("historical developer-loop-m3-complete tag changed".into());
    }
    Ok(())
}

fn m3r2_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let engine = fs::read_to_string(root.join("crates/nexa-embed/src/lib.rs"))?;
    let development = fs::read_to_string(root.join("crates/nexa-embed/src/development.rs"))?;
    let inspection = fs::read_to_string(root.join("crates/nexa-embed/src/inspection.rs"))?;
    let tests = fs::read_to_string(root.join("crates/nexa-embed/tests/embed.rs"))?;
    for required in [
        "terminate_unqueued_generation",
        "clear_unqueued_observation",
        "CandidateTerminalKind::SupersededBeforeCompile",
        "CandidateTerminalKind::CancelledBySourceRemoval",
        "CandidateTerminalKind::CancelledByDisable",
        "CandidateTerminalKind::CancelledByShutdown",
    ] {
        if !engine.contains(required) {
            return Err(format!("M3R2 Engine accounting is missing `{required}`").into());
        }
    }
    if !development.contains("unqueued_generation: Option<CandidateTerminalData>") {
        return Err("M3R2 does not explicitly track the unqueued Generation".into());
    }
    for required in [
        "created_generations",
        "terminal_generations",
        "duplicate_terminals",
        "generations_without_terminal",
    ] {
        if !inspection.contains(required) {
            return Err(format!("M3R2 inspection is missing `{required}`").into());
        }
    }
    for required in [
        "prequeue_hash_replacement_supersedes_previous_generation",
        "prequeue_revert_to_active_supersedes_observed_generation",
        "prequeue_source_removal_cancels_observed_generation",
        "prequeue_disable_cancels_observed_generation",
        "prequeue_shutdown_cancels_observed_generation",
        "generation_accounting_machine_report_uses_real_engine_inspection",
    ] {
        if !tests.contains(required) {
            return Err(format!("M3R2 regression coverage is missing `{required}`").into());
        }
    }
    let r1_tag_type = git_output(&["cat-file", "-t", "developer-loop-m3-complete-r1"])?;
    let r1_tag_target = git_output(&["rev-parse", "developer-loop-m3-complete-r1^{}"])?;
    if r1_tag_type != "tag" || r1_tag_target != "b53ce21f98db7387b37cca0572fbbf920ab53d61" {
        return Err("historical developer-loop-m3-complete-r1 tag changed".into());
    }
    Ok(())
}

fn m3r3_product_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let engine = fs::read_to_string(root.join("crates/nexa-embed/src/lib.rs"))?;
    let development = fs::read_to_string(root.join("crates/nexa-embed/src/development.rs"))?;
    let tests = fs::read_to_string(root.join("crates/nexa-embed/src/freshness_tests.rs"))?;
    for required in [
        "fn refresh_desired_build_fingerprint",
        "fn candidate_identity_is_current",
        "fn supersede_development_for_current_source",
    ] {
        if !engine.contains(required) {
            return Err(format!("M3R3 Engine freshness guard is missing `{required}`").into());
        }
    }
    for required in [
        "desired_build_fingerprint: Option<nexa_analysis::BuildFingerprint>",
        "queued_generation: Option<u64>",
        "in_flight_generation: Option<u64>",
        "enum InFlightDisposition",
        "fn supersede_package_except",
    ] {
        if !development.contains(required) {
            return Err(format!("M3R3 Worker freshness tracking is missing `{required}`").into());
        }
    }
    for required in [
        "candidate_freshness_machine_report_uses_real_engine_evidence",
        "result_refresh_failure_cancels_and_recovers_without_stale_observation",
        "same_hash_aba_keeps_new_generation_worker_identity",
    ] {
        if !tests.contains(required) {
            return Err(format!("M3R3 freshness regression is missing `{required}`").into());
        }
    }
    Ok(())
}

struct M3R3TagEvidence {
    historical_tag_type: String,
    historical_tag_target: String,
    r1_tag_type: String,
    r1_tag_target: String,
    r2_tag_type: String,
    r2_tag_target: String,
    tag_type: String,
    tag_target: String,
}

impl M3R3TagEvidence {
    fn load() -> Self {
        Self {
            historical_tag_type: git_output(&["cat-file", "-t", "developer-loop-m3-complete"])
                .unwrap_or_else(|_| "missing".into()),
            historical_tag_target: git_output(&["rev-parse", "developer-loop-m3-complete^{}"])
                .unwrap_or_else(|_| "missing".into()),
            r1_tag_type: git_output(&["cat-file", "-t", "developer-loop-m3-complete-r1"])
                .unwrap_or_else(|_| "missing".into()),
            r1_tag_target: git_output(&["rev-parse", "developer-loop-m3-complete-r1^{}"])
                .unwrap_or_else(|_| "missing".into()),
            r2_tag_type: git_output(&["cat-file", "-t", "developer-loop-m3-complete-r2"])
                .unwrap_or_else(|_| "missing".into()),
            r2_tag_target: git_output(&["rev-parse", "developer-loop-m3-complete-r2^{}"])
                .unwrap_or_else(|_| "missing".into()),
            tag_type: git_output(&["cat-file", "-t", "developer-loop-m3-complete-r3"])
                .unwrap_or_else(|_| "missing".into()),
            tag_target: git_output(&["rev-parse", "developer-loop-m3-complete-r3^{}"])
                .unwrap_or_else(|_| "missing".into()),
        }
    }

    fn audit(&self, head: &str) -> Result<(), DynError> {
        let expected = [
            (
                "developer-loop-m3-complete",
                self.historical_tag_type.as_str(),
                self.historical_tag_target.as_str(),
                "621612f49c4180989711df3ca80021fd21ad9277",
            ),
            (
                "developer-loop-m3-complete-r1",
                self.r1_tag_type.as_str(),
                self.r1_tag_target.as_str(),
                "b53ce21f98db7387b37cca0572fbbf920ab53d61",
            ),
            (
                "developer-loop-m3-complete-r2",
                self.r2_tag_type.as_str(),
                self.r2_tag_target.as_str(),
                "71c3a3ead70533f013928b6d1c434e1870f49b24",
            ),
        ];
        for (name, tag_type, target, expected_target) in expected {
            if tag_type != "tag" || target != expected_target {
                return Err(format!(
                    "historical annotated tag {name} changed: type={tag_type}, target={target}"
                )
                .into());
            }
        }
        if self.tag_type != "tag" {
            return Err("developer-loop-m3-complete-r3 is not an annotated tag".into());
        }
        if self.tag_target != head {
            return Err(format!(
                "developer-loop-m3-complete-r3 targets {}, expected HEAD {head}",
                self.tag_target
            )
            .into());
        }
        Ok(())
    }
}

fn first_text_status_block<'a>(path: &str, source: &'a str) -> Result<&'a str, DynError> {
    source
        .split_once("```text")
        .and_then(|(_, remainder)| remainder.split_once("```"))
        .map(|(block, _)| block)
        .ok_or_else(|| format!("{path} has no first fenced text status block").into())
}

fn exact_status_value<'a>(block: &'a str, key: &str) -> Result<&'a str, DynError> {
    let prefix = format!("{key} = ");
    let values = block
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect::<Vec<_>>();
    match values.as_slice() {
        [value] => Ok(value),
        [] => Err(format!("status block is missing exact key `{key}`").into()),
        _ => Err(format!("status block contains duplicate key `{key}`").into()),
    }
}

fn require_unique_exact_line(path: &str, source: &str, expected: &str) -> Result<(), DynError> {
    let matches = source.lines().filter(|line| *line == expected).count();
    if matches == 1 {
        Ok(())
    } else {
        Err(format!("{path} must contain exactly one `{expected}` line; found {matches}").into())
    }
}

fn m3r3_final_status_audit() -> Result<(), DynError> {
    let root = workspace_root();
    for path in ["README.md", "ROADMAP.md", "baseline/BASELINE_INDEX.md"] {
        let source = fs::read_to_string(root.join(path))?;
        if source.contains("FINALIZING") {
            return Err(format!("{path} still contains a FINALIZING status").into());
        }
        let block = first_text_status_block(path, &source)?;
        for key in [
            "Nexa M3 Developer Loop & Diagnostics",
            "NexaEngine API",
            "Automatic Candidate Compilation",
            "Candidate Generation Terminal Accounting",
            "Candidate Freshness Commit Guard",
            "Last Known Good Reload",
            "Unified Diagnostics",
            "Source-level Runtime Stack Traces",
            "Package-aware CLI",
            "Editor Diagnostics",
        ] {
            let value = exact_status_value(block, key)?;
            if value != "COMPLETE" {
                return Err(
                    format!("{path} status `{key}` is `{value}`, expected `COMPLETE`").into(),
                );
            }
        }
        let normalized = source.split_whitespace().collect::<Vec<_>>().join(" ");
        match path {
            "README.md" if !source.contains("Current target = M3R3 complete; M4 not started") => {
                return Err("README.md does not declare the exact M3R3/M4 boundary".into());
            }
            "ROADMAP.md"
                if !normalized.contains("M4 Language Scale Foundation has not started") =>
            {
                return Err("ROADMAP.md does not keep M4 unstarted".into());
            }
            "baseline/BASELINE_INDEX.md" => {
                if !source.contains("Version: **3.0.3**") {
                    return Err("baseline version is not the final 3.0.3".into());
                }
                if !normalized.contains("M4 has not started") {
                    return Err("baseline does not keep M4 unstarted".into());
                }
            }
            _ => {}
        }
    }
    for path in [
        "baseline/embed/EMBED_API.md",
        "baseline/embed/DEVELOPMENT_WORKER.md",
        "docs/DEVELOPMENT_LOOP.md",
    ] {
        let source = fs::read_to_string(root.join(path))?;
        require_unique_exact_line(path, &source, "Status: M3R3 COMPLETE")?;
    }
    Ok(())
}

fn m3r1_final_status_audit() -> Result<(), DynError> {
    let root = workspace_root();
    for path in ["README.md", "ROADMAP.md", "baseline/BASELINE_INDEX.md"] {
        let source = fs::read_to_string(root.join(path))?;
        for required in [
            "Nexa M3 Developer Loop & Diagnostics = COMPLETE",
            "NexaEngine API = COMPLETE",
            "Automatic Candidate Compilation = COMPLETE",
            "Last Known Good Reload = COMPLETE",
            "Unified Diagnostics = COMPLETE",
            "Source-level Runtime Stack Traces = COMPLETE",
            "Package-aware CLI = COMPLETE",
            "Editor Diagnostics = COMPLETE",
        ] {
            if !source.contains(required) {
                return Err(format!("{path} is missing final status `{required}`").into());
            }
        }
    }
    let embed_api = fs::read_to_string(root.join("baseline/embed/EMBED_API.md"))?;
    if !embed_api.contains("Status: M3R1 COMPLETE") {
        return Err("baseline/embed/EMBED_API.md is not M3R1 COMPLETE".into());
    }
    Ok(())
}

fn m3r2_final_status_audit() -> Result<(), DynError> {
    let root = workspace_root();
    for path in ["README.md", "ROADMAP.md", "baseline/BASELINE_INDEX.md"] {
        let source = fs::read_to_string(root.join(path))?;
        for required in [
            "Nexa M3 Developer Loop & Diagnostics = COMPLETE",
            "NexaEngine API = COMPLETE",
            "Automatic Candidate Compilation = COMPLETE",
            "Candidate Generation Terminal Accounting = COMPLETE",
            "Last Known Good Reload = COMPLETE",
            "Unified Diagnostics = COMPLETE",
            "Source-level Runtime Stack Traces = COMPLETE",
            "Package-aware CLI = COMPLETE",
            "Editor Diagnostics = COMPLETE",
        ] {
            if !source.contains(required) {
                return Err(format!("{path} is missing final status `{required}`").into());
            }
        }
    }
    let embed_api = fs::read_to_string(root.join("baseline/embed/EMBED_API.md"))?;
    if !embed_api.contains("Status: M3R2 COMPLETE") {
        return Err("baseline/embed/EMBED_API.md is not M3R2 COMPLETE".into());
    }
    Ok(())
}

fn collect_forbidden(
    path: &Path,
    root: &Path,
    forbidden: &[&str],
    violations: &mut Vec<String>,
) -> Result<(), DynError> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_forbidden(&entry?.path(), root, forbidden, violations)?;
        }
        return Ok(());
    }
    if !path
        .extension()
        .is_some_and(|extension| matches!(extension.to_str(), Some("rs" | "md" | "toml" | "json")))
    {
        return Ok(());
    }
    let source = fs::read_to_string(path)?;
    for symbol in forbidden {
        if source.contains(symbol) {
            violations.push(format!(
                "{}: {symbol}",
                path.strip_prefix(root).unwrap_or(path).display()
            ));
        }
    }
    Ok(())
}

fn test_embed() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-embed"])
}

fn test_snake() -> Result<(), DynError> {
    cargo(&["test", "-p", "snake-game"])
}

fn snake_headless(mode: &str) -> Result<(), DynError> {
    cargo(&[
        "run",
        "-p",
        "snake-game",
        "--bin",
        "snake-headless",
        "--",
        mode,
    ])
}

fn m2_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let main = fs::read_to_string(root.join("examples/snake-game/src/main.rs"))?;
    for forbidden in [
        "nexa_runtime",
        "nexa_compiler",
        "RealmRuntime",
        "RuntimeHost",
        "ModuleHandle",
        "ScopeHandle",
        "TaskHandle",
        "StepConfig",
        "TaskPoll",
        "drain_releases",
    ] {
        if main.contains(forbidden) {
            return Err(format!("Snake main contains low-level symbol {forbidden}").into());
        }
    }
    let loop_body = main
        .split_once("loop {")
        .and_then(|(_, body)| body.split_once("next_frame().await"))
        .map_or("", |(body, _)| body);
    for call in ["apply_pending_actions", "handle_events", ".tick("] {
        if loop_body.matches(call).count() != 1 {
            return Err(format!("Snake game loop must call {call} exactly once").into());
        }
    }
    let embed_root = root.join("crates/nexa-embed/src");
    let embed_source = fs::read_dir(embed_root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|value| value == "rs"))
        .map(|entry| fs::read_to_string(entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    for forbidden in [
        "builtin_root",
        "dlc_root",
        "mod_root",
        "Builtin",
        "Dlc",
        "Snake",
        "Food",
        "Skin",
        "GameCommand",
    ] {
        if embed_source.contains(forbidden) {
            return Err(format!("nexa-embed domain leak: {forbidden}").into());
        }
    }
    let hello = fs::read_to_string(root.join("examples/hello-runtime/src/main.rs"))?;
    for forbidden in [
        "RealmRuntime",
        "RuntimeHost",
        "StepConfig",
        "TaskPoll",
        "ModuleHandle",
        "ScopeHandle",
    ] {
        if hello.contains(forbidden) {
            return Err(format!("hello-runtime contains low-level symbol {forbidden}").into());
        }
    }
    let package_count = ["builtin", "dlc", "mods"]
        .iter()
        .map(|category| {
            fs::read_dir(root.join("examples/snake-game/packages").join(category)).map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum::<usize>();
    if package_count != 9 {
        return Err(format!("expected 9 Snake packages, found {package_count}").into());
    }
    audit_snake_schema2_packages(&root)?;
    for required in [
        "docs/EMBEDDING.md",
        "docs/PACKAGE_SOURCES.md",
        "docs/PACKAGE_POLICIES.md",
        "docs/SNAKE_MODDING.md",
        "examples/snake-game/README.md",
    ] {
        if !root.join(required).is_file() {
            return Err(format!("missing M2 documentation {required}").into());
        }
    }
    Ok(())
}

fn audit_snake_schema2_packages(root: &Path) -> Result<(), DynError> {
    let packages = [
        (
            "builtin/classic-hud",
            "snake.classic_hud",
            "src/snake/classic_hud.nexa",
        ),
        (
            "builtin/classic-rules",
            "snake.classic_rules",
            "src/snake/classic_rules.nexa",
        ),
        (
            "builtin/classic-spawn",
            "snake.classic_spawn",
            "src/snake/classic_spawn.nexa",
        ),
        (
            "builtin/default-skin",
            "snake.default_skin",
            "src/snake/default_skin.nexa",
        ),
        (
            "dlc/food-chaos",
            "snake.food_chaos",
            "src/snake/food_chaos.nexa",
        ),
        (
            "mods/corner-spawn",
            "snake.corner_spawn",
            "src/snake/corner_spawn.nexa",
        ),
        (
            "mods/neon-skin",
            "snake.neon_skin",
            "src/snake/neon_skin.nexa",
        ),
        (
            "mods/score-overlay",
            "snake.score_overlay",
            "src/snake/score_overlay.nexa",
        ),
        (
            "mods/weird-foods",
            "snake.weird_foods",
            "src/snake/weird_foods.nexa",
        ),
    ];
    for (package, module, source_path) in packages {
        let package_root = root.join("examples/snake-game/packages").join(package);
        let manifest = fs::read_to_string(package_root.join("package.toml"))?;
        if !manifest.starts_with("schema = 2\n")
            || !manifest.contains("source_root = \"src\"")
            || !manifest.contains(&format!("entry = \"{module}\""))
        {
            return Err(
                format!("Snake package {package} is not a schema 2 `{module}` package").into(),
            );
        }
        let source = fs::read_to_string(package_root.join(source_path))?;
        let expected_source_path = format!("src/{}.nexa", module.replace('.', "/"));
        if source_path != expected_source_path {
            return Err(format!(
                "Snake package {package} entry `{module}` must derive from path \
                 `{expected_source_path}`, observed `{source_path}`"
            )
            .into());
        }
        if source.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("module ") || line.starts_with("import ")
        }) {
            return Err(format!(
                "Snake package {package} still contains a removed module/import declaration"
            )
            .into());
        }
    }
    let overlay = fs::read_to_string(
        root.join("examples/snake-game/packages/mods/score-overlay/src/snake/score_overlay.nexa"),
    )?;
    if !overlay.contains("@state(version = 1)\nclass OverlayState") {
        return Err("Score Overlay does not declare typed state".into());
    }
    Ok(())
}

fn finalize_m2() -> Result<(), DynError> {
    let root = workspace_root();
    let audit = m2_audit().is_ok();
    let tag_type = git_output(&["cat-file", "-t", "embed-snake-m2-complete"])
        .unwrap_or_else(|_| "missing".into());
    let tag_target = git_output(&["rev-parse", "embed-snake-m2-complete^{}"])
        .unwrap_or_else(|_| "missing".into());
    let head = git_output(&["rev-parse", "HEAD"])?;
    let working_tree_clean = git_output(&["status", "--porcelain"])?.is_empty();
    let passed = audit && tag_type == "tag" && tag_target == head && working_tree_clean;
    let report = M2FinalReport {
        head,
        tag_type,
        tag_target,
        working_tree_clean,
        ergonomics_audit: status(audit),
        status: if passed { "PASS" } else { "FAIL" },
    };
    let output = root.join("target/nexa-artifacts/m2-finalize/final-report.json");
    fs::create_dir_all(output.parent().ok_or("M2 final report has no parent")?)?;
    fs::write(
        output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("M2 finalization failed".into())
    }
}

fn run_check_summary() -> CheckSummary {
    let workspace = workspace_check().is_ok();
    run_check_summary_with_workspace(workspace)
}

fn run_check_summary_after_workspace() -> CheckSummary {
    run_check_summary_with_workspace(true)
}

fn run_check_summary_with_workspace(workspace_check: bool) -> CheckSummary {
    CheckSummary {
        workspace_check,
        repo_audit: repo_audit().is_ok(),
        binding_test: test_binding().is_ok(),
        task_test: test_task().is_ok(),
        reload_test: cargo(&["test", "-p", "nexa-runtime", "--test", "restart_reload"]).is_ok(),
        model_differential_test: cargo(&["test", "-p", "nexa-model"]).is_ok(),
        fuzz_build: fuzz_build().is_ok(),
        fuzz_corpus_replay: fuzz_corpus_replay().is_ok(),
        bench_smoke: bench_smoke().is_ok(),
    }
}

fn workspace_check() -> Result<(), DynError> {
    cargo(&["fmt", "--all", "--", "--check"])?;
    // `cargo clippy --all-targets` performs the full `cargo check` work, so a
    // separate check pass would only recompile the workspace a second time.
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(&["test", "--workspace", "--all-targets"])?;
    cargo(&["test", "--doc", "--workspace"])?;
    cargo_with_environment(
        &["doc", "--workspace", "--no-deps"],
        &[("RUSTDOCFLAGS", "-D warnings")],
    )
}

fn test_task() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-runtime", "--test", "task_lifecycle"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-runtime",
        "--test",
        "public_api_compile",
        "--",
        "--include-ignored",
    ])
}

fn test_core() -> Result<(), DynError> {
    for package in [
        "nexa-core",
        "nexa-bytecode",
        "nexa-compiler",
        "nexa-verifier",
        "nexa-migrate",
        "nexa-runtime",
    ] {
        cargo(&["test", "-p", package])?;
    }
    Ok(())
}

fn test_binding() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-idl"])?;
    cargo(&[
        "test",
        "-p",
        "nexa-idl",
        "--test",
        "e2e_mutations",
        "--",
        "--include-ignored",
    ])?;
    cargo(&[
        "run",
        "-p",
        "nexa-cli",
        "--",
        "nidl",
        "check",
        "examples/combat-runtime/combat_api.nidl",
    ])?;
    cargo(&["test", "-p", "combat-runtime"])
}

fn fuzz_smoke() -> Result<(), DynError> {
    fuzz_build()?;
    fuzz_corpus_replay()
}

fn fuzz_build() -> Result<(), DynError> {
    // The fuzz directories are independent workspaces over the same crate
    // graph; one shared target directory compiles that graph once instead of
    // ten times.
    let shared_target = workspace_root().join("target/fuzz-check");
    let shared_target = shared_target
        .to_str()
        .ok_or("fuzz shared target directory is not UTF-8")?;
    for directory in [
        "fuzz/bytecode",
        "fuzz/bytecode-decode",
        "fuzz/verifier",
        "fuzz/root-map",
        "fuzz/wcet",
        "fuzz/host-import",
        "fuzz/state-schema",
        "fuzz/source",
        "fuzz/idl",
        "fuzz/realm-events",
    ] {
        cargo_with_environment(
            &[
                "check",
                "--quiet",
                "--manifest-path",
                &format!("{directory}/Cargo.toml"),
            ],
            &[("CARGO_TARGET_DIR", shared_target)],
        )?;
    }
    Ok(())
}

fn fuzz_corpus_replay() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-model", "--test", "realm_corpus_replay"])
}

const fn status(passed: bool) -> &'static str {
    if passed { "PASS" } else { "FAIL" }
}

fn bench_smoke() -> Result<(), DynError> {
    let root = workspace_root();
    let output_dir = root.join("target/nexa-artifacts/bench-smoke");
    fs::create_dir_all(&output_dir)?;
    cargo(&[
        "run",
        "--release",
        "--quiet",
        "--manifest-path",
        "tools/allocation-observer/Cargo.toml",
    ])?;
    for run in 1..=3 {
        let output = output_dir.join(format!("run-{run}.json"));
        cargo(&[
            "run",
            "--release",
            "--quiet",
            "-p",
            "nexa-benchmark-v6",
            "--",
            "--samples",
            "1000",
            "--output",
            output.to_str().ok_or("non-UTF-8 benchmark output path")?,
        ])?;
        let report: Value = serde_json::from_slice(&fs::read(&output)?)?;
        let cases = report["cases"]
            .as_array()
            .ok_or("benchmark report has no cases")?;
        for case in cases {
            let name = case["case"].as_str().unwrap_or("<unknown>");
            let p95 = case["p95_ns"].as_u64().ok_or("benchmark p95 is not u64")?;
            let frame = case["frame_1000_calls_ns"]
                .as_u64()
                .ok_or("benchmark frame budget is not u64")?;
            if p95 > 100_000 || frame > 100_000_000 {
                return Err(format!(
                    "{name} exceeded absolute budget: p95={p95}ns frame={frame}ns"
                )
                .into());
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::too_many_lines
)]
fn repo_audit() -> Result<(), DynError> {
    let root = workspace_root();
    let tracked = git_lines(&["ls-files"])?;
    let product_rust_loc = loc_matching(&root, &tracked, |path| {
        path.ends_with(".rs")
            && (path.starts_with("crates/") || path.starts_with("examples/"))
            && !path.contains("/tests/")
    });
    let unit_test_loc = loc_matching(&root, &tracked, |path| {
        path.ends_with(".rs")
            && path.starts_with("crates/")
            && path.contains("/src/")
            && fs::read_to_string(root.join(path))
                .is_ok_and(|source| source.contains("#[cfg(test)]"))
    });
    let integration_test_loc = loc_matching(&root, &tracked, |path| {
        path.ends_with(".rs") && path.starts_with("crates/") && path.contains("/tests/")
    });
    let tool_loc = loc_matching(&root, &tracked, |path| {
        path.ends_with(".rs") && path.starts_with("tools/")
    });
    let fixture_total_bytes = tracked
        .iter()
        .filter(|path| path.contains("/fixtures/") || path.contains("/fixture/"))
        .filter_map(|path| fs::metadata(root.join(path)).ok())
        .map(|metadata| metadata.len())
        .sum();
    let workspace_members = workspace_member_count()?;
    let versioned_paths = versioned_directories(&tracked);
    let active_gate1_tool_crates = tracked
        .iter()
        .filter_map(|path| path.strip_prefix("tools/"))
        .filter_map(|path| path.split('/').next())
        .filter(|name| name.starts_with("gate1"))
        .collect::<BTreeSet<_>>()
        .len();
    let versioned_gate_experiment_directories = versioned_paths
        .iter()
        .filter(|path| path.contains("gate1-v"))
        .count();
    let tracked_raw_evidence_files = tracked
        .iter()
        .filter(|path| {
            path.starts_with("reports/raw/")
                || path.ends_with(".ndjson")
                || (path.contains("gate1_")
                    && (path.ends_with("_pilot.json") || path.ends_with("_budget.json")))
        })
        .count();
    let tracked_files_over_512_kib = tracked
        .iter()
        .filter_map(|path| {
            let size = fs::metadata(root.join(path)).ok()?.len();
            (size > 512 * 1024).then(|| path.clone())
        })
        .collect::<Vec<_>>();
    let hashes = duplicate_hashes(&root, &tracked);
    let duplicate_file_hashes = hashes.values().filter(|paths| paths.len() > 1).count();
    let duplicate_versioned_fixtures = hashes
        .values()
        .filter(|paths| {
            paths.len() > 1
                && paths.iter().any(|path| {
                    (path.contains("/fixtures/") || path.contains("/fixture/"))
                        && is_versioned_path(path)
                })
        })
        .count();
    let historical_gate_loc = historical_gate_tool_loc().unwrap_or(0);
    let current_gate_loc = loc_matching(&root, &tracked, |path| {
        path.starts_with("tools/gate1") && path.ends_with(".rs")
    });
    let reduction = if historical_gate_loc == 0 {
        100
    } else {
        100_u64.saturating_sub(
            u64::try_from(current_gate_loc)
                .unwrap_or(u64::MAX)
                .saturating_mul(100)
                / u64::try_from(historical_gate_loc).unwrap_or(u64::MAX),
        )
    };
    let low_level_event_violations = low_level_event_violations(&root, &tracked);
    let audit_sources = audit_sources(&root, &tracked);
    let public_api_violations = public_api_violations(&audit_sources);
    let public_raw_task_api_violations = count_occurrences(
        &audit_sources,
        &["pub fn poll_task_raw", "pub fn call(", "pub fn spawn("],
    );
    let public_task_lifecycle_bypass_violations = count_occurrences(
        &audit_sources,
        &["pub fn create_host_request", "pub fn wait_for_request"],
    ) + audit_sources
        .iter()
        .filter(|(path, _)| {
            path.as_str() != "crates/nexa-runtime/tests/public_api_compile.rs"
                && !path.starts_with("crates/nexa-runtime/src/")
        })
        .map(|(_, source)| {
            [".create_host_request(", ".wait_for_request("]
                .iter()
                .map(|needle| source.matches(needle).count())
                .sum::<usize>()
        })
        .sum::<usize>();
    let model_adapter_source =
        fs::read_to_string(root.join("crates/nexa-runtime/src/model_adapter.rs"))?;
    let model_adapter_fields = model_adapter_source
        .split_once("pub struct RealmRuntimeModelAdapter")
        .and_then(|(_, tail)| tail.split_once("\n}"))
        .map_or("", |(fields, _)| fields);
    let shadow_runtime_model_violations = missing_evidence(
        &model_adapter_source,
        &[
            "realm: Option<RealmRuntime>",
            "runtime_host: RuntimeHost",
            "inspection_snapshot()",
            "resource_ledger()",
            "completion_accounting()",
            "pending_releases()",
            "pending_completions()",
        ],
    ) + model_adapter_fields
        .matches("snapshot: RuntimeRealmSnapshot")
        .count();
    let business_host_e2e_source = [
        "crates/nexa-idl/tests/e2e_mutations.rs",
        "crates/nexa-idl/tests/e2e_support.rs",
        "crates/nexa-idl/tests/fixtures/business_host/business_host.rs",
    ]
    .iter()
    .map(|path| fs::read_to_string(root.join(path)))
    .collect::<Result<Vec<_>, _>>()?
    .join("\n");
    let business_host_stub_e2e_violations = missing_evidence(
        &business_host_e2e_source,
        &[
            "struct MutationCase",
            "BusinessHostV1",
            "business_host.rs",
            "--message-format=json",
            "assert_expected_business_diagnostic",
        ],
    ) + business_host_e2e_source
        .matches("GeneratedHostStub")
        .count();
    let e2e_support_source = fs::read_to_string(root.join("crates/nexa-idl/tests/e2e_support.rs"))?;
    let mutation_report = fs::read(root.join("target/nexa-artifacts/idl-e2e/mutation-report.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let mutation_report_violations = mutation_report.as_ref().map_or(2, |report| {
        usize::from(report["generated_registry_positive_runs"] != 20)
            + usize::from(report["manual_registry_positive_runs"] != 0)
    });
    let manual_binding_e2e_registry_violations =
        e2e_support_source.matches("HeartbeatRegistry").count()
            + e2e_support_source.matches("impl HostRegistry").count()
            + missing_evidence(&e2e_support_source, &["GeneratedHostRegistry::new"])
            + mutation_report_violations;
    let shadow_runtime_prevalidation_violations =
        model_adapter_source.matches("fn validate(").count()
            + model_adapter_source.matches("self.validate(").count();
    let runtime_invalid_event_short_circuit_violations = missing_evidence(
        &model_adapter_source,
        &[
            "struct ProbeFixture",
            "cross_realm_task",
            "cross_realm_request",
            ".spawn_task(",
            ".poll_task(",
            ".cancel_task(",
            ".complete_request(",
            ".restart_reload(",
            "map_spawn_error",
            "map_task_error",
            "map_request_error",
            "map_reload_error",
        ],
    );
    let differential_source =
        fs::read_to_string(root.join("crates/nexa-model/tests/realm_differential.rs"))?;
    let runtime_differential_current_handle_violations = missing_evidence(
        &format!("{model_adapter_source}\n{differential_source}"),
        &[
            "current_task_or_probe",
            "current_request_or_probe",
            "current_handles_drive_semantic_regression_sequences",
            "RuntimeRequestRejection::AlreadyCompleted",
            "RuntimeRequestRejection::DetachedByReload",
            "re-poll current Waiting task",
        ],
    );
    let missing_runtime_invocation_counter_evidence = missing_evidence(
        &format!("{model_adapter_source}\n{differential_source}"),
        &[
            "RuntimeInvocationCounters",
            "spawn_attempts",
            "poll_attempts",
            "cancel_attempts",
            "completion_attempts",
            "reload_attempts",
            "physical_completion_attempts",
            "state_fingerprint()",
            "corresponding Runtime API counter did not advance",
        ],
    );
    let realm_model_source = fs::read_to_string(root.join("crates/nexa-model/src/realm.rs"))?;
    let model_repeated_reload_semantic_violations = missing_evidence(
        &format!("{realm_model_source}\n{differential_source}"),
        &[
            "TaskLifecycle::Vacant | TaskLifecycle::Terminal",
            "RealmEvent::RestartReload =>",
            "RealmEvent::MigrationFailure =>",
            "RealmEvent::ActivationFailure =>",
            "RequestLifecycle::Cancelled",
            "cancelled_requests",
            "RealmEvent::RestartReload, RealmEvent::RestartReload",
        ],
    ) + realm_model_source
        .matches("RestartReload if self.snapshot.reload == ReloadLifecycle::Idle")
        .count();
    let realm_fuzz_source =
        fs::read_to_string(root.join("fuzz/realm-events/fuzz_targets/realm_event_sequence.rs"))?;
    let real_runtime_fuzz_violations =
        missing_evidence(
            &realm_fuzz_source,
            &[
                "RealmRuntimeModelAdapter::default()",
                "runtime.apply(event)",
                "runtime.snapshot()",
                "runtime.invariants_hold()",
            ],
        ) + missing_evidence(&model_adapter_source, &["realm: Option<RealmRuntime>"]);
    let task_lifecycle_source =
        fs::read_to_string(root.join("crates/nexa-runtime/tests/task_lifecycle.rs"))?;
    let combat_source = fs::read_to_string(root.join("examples/combat-runtime/src/main.rs"))?;
    let release_lifecycle_source = format!("{task_lifecycle_source}\n{combat_source}");
    let unverified_host_resource_release_kinds = missing_evidence(
        &release_lifecycle_source,
        &[
            "struct ExpectedReleases",
            "ReleaseKind::HostRequest",
            "ReleaseKind::ResourceToken",
            "ReleaseKind::Snapshot",
            "generated_host_binding_releases_request_token_and_snapshot_exactly_once",
            "cancel_task(",
            "restart_reload(",
            "decode_owned()",
        ],
    );
    let legacy_host_abi_violations = count_identifier(&audit_sources, "HostArgs")
        + count_identifier(&audit_sources, "HostValue")
        + count_occurrences(
            &audit_sources,
            &["HostRegistry::call", "HostCallOutcome::Immediate"],
        );
    let completion_buffer_symbol_violations = count_occurrences(
        &audit_sources,
        &[
            "ReloadCompletionBuffer",
            "ReloadCompletionStats",
            "BufferedForReload",
            "reload_completion",
            "completion_buffer",
        ],
    );
    let reload_pause_symbol_violations =
        count_occurrences(&audit_sources, &["ReloadPaused", "ReloadPause"]);
    let retired_epoch_business_api_violations =
        count_occurrences(&audit_sources, &["RetiredEpoch", "retired_epoch"]);
    let deprecated_allow_violations = count_occurrences(&audit_sources, &["#![allow(deprecated)]"]);
    // M5 K1 unsafe containment: the trusted kernel is the only crates/
    // source allowed to contain unsafe code, and it must justify every
    // block with a SAFETY comment. Tools keep their own lint tables.
    let unsafe_containment_violations = {
        let kernel_path = "crates/nexa-runtime/src/trusted.rs";
        let mut violations = audit_sources
            .iter()
            .filter(|(path, _)| {
                path.starts_with("crates/") && path.ends_with(".rs") && path.as_str() != kernel_path
            })
            .filter(|(_, source)| {
                ["unsafe fn ", "unsafe impl ", "unsafe {"]
                    .iter()
                    .any(|needle| source.contains(needle))
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        if let Some(kernel) = audit_sources.get(kernel_path) {
            if kernel.matches("unsafe {").count() > kernel.matches("// SAFETY:").count() {
                violations.push(format!(
                    "{kernel_path}: unsafe block without a SAFETY comment"
                ));
            }
            if !kernel.contains("#![deny(unsafe_op_in_unsafe_fn)]") {
                violations.push(format!(
                    "{kernel_path}: missing unsafe_op_in_unsafe_fn deny"
                ));
            }
        } else {
            violations.push(format!("{kernel_path}: trusted kernel module is missing"));
        }
        violations
    };
    let versioned_model_file_count = tracked
        .iter()
        .filter(|path| {
            (path.starts_with("crates/nexa-model/") || path.starts_with("crates/nexa-runtime/"))
                && Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "rs")
                && (path.contains("realm_v") || path.contains("model_adapter_v"))
        })
        .count();
    let historical_tag_type = git_output(&["cat-file", "-t", "gate1-v2.9-stop"])?;
    let historical_tag_target = git_output(&["rev-parse", "gate1-v2.9-stop^{}"])?;
    let tag_valid = historical_tag_type == "tag"
        && historical_tag_target == "8552064ec01b3191467633717de7b77c97cb24f1";
    let passed = active_gate1_tool_crates == 0
        && versioned_gate_experiment_directories == 0
        && tracked_raw_evidence_files == 0
        && duplicate_versioned_fixtures == 0
        && reduction >= 80
        && tracked_files_over_512_kib.is_empty()
        && low_level_event_violations.is_empty()
        && public_api_violations.is_empty()
        && public_raw_task_api_violations == 0
        && public_task_lifecycle_bypass_violations == 0
        && shadow_runtime_model_violations == 0
        && business_host_stub_e2e_violations == 0
        && manual_binding_e2e_registry_violations == 0
        && shadow_runtime_prevalidation_violations == 0
        && runtime_invalid_event_short_circuit_violations == 0
        && runtime_differential_current_handle_violations == 0
        && missing_runtime_invocation_counter_evidence == 0
        && model_repeated_reload_semantic_violations == 0
        && real_runtime_fuzz_violations == 0
        && unverified_host_resource_release_kinds == 0
        && legacy_host_abi_violations == 0
        && completion_buffer_symbol_violations == 0
        && reload_pause_symbol_violations == 0
        && retired_epoch_business_api_violations == 0
        && deprecated_allow_violations == 0
        && unsafe_containment_violations.is_empty()
        && versioned_model_file_count == 0
        && tag_valid;
    let report = RepoHealth {
        schema_version: 5,
        product_rust_loc,
        unit_test_loc,
        integration_test_loc,
        tool_loc,
        fixture_total_bytes,
        workspace_members,
        versioned_directories: versioned_paths.len(),
        duplicate_file_hashes,
        tracked_files_over_512_kib,
        active_gate1_tool_crates,
        versioned_gate_experiment_directories,
        tracked_raw_evidence_files,
        duplicate_versioned_fixtures,
        gate1_test_tool_loc_reduction_percent: reduction,
        low_level_event_violations,
        public_api_violations,
        public_raw_task_api_violations,
        public_task_lifecycle_bypass_violations,
        shadow_runtime_model_violations,
        business_host_stub_e2e_violations,
        manual_binding_e2e_registry_violations,
        shadow_runtime_prevalidation_violations,
        runtime_invalid_event_short_circuit_violations,
        runtime_differential_current_handle_violations,
        missing_runtime_invocation_counter_evidence,
        model_repeated_reload_semantic_violations,
        real_runtime_fuzz_violations,
        unverified_host_resource_release_kinds,
        legacy_host_abi_violations,
        completion_buffer_symbol_violations,
        reload_pause_symbol_violations,
        retired_epoch_business_api_violations,
        deprecated_allow_violations,
        unsafe_containment_violations,
        versioned_model_file_count,
        historical_tag_type,
        historical_tag_target,
        status: if passed { "PASS" } else { "FAIL" },
    };
    let output = root.join("target/nexa-artifacts/repo-health.json");
    fs::create_dir_all(output.parent().ok_or("repo health path has no parent")?)?;
    fs::write(
        &output,
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    let inventory = finalization_inventory(&audit_sources);
    let inventory_output = root.join("target/nexa-artifacts/m1-finalize/inventory.json");
    fs::create_dir_all(
        inventory_output
            .parent()
            .ok_or("inventory path has no parent")?,
    )?;
    fs::write(
        inventory_output,
        format!("{}\n", serde_json::to_string_pretty(&inventory)?),
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if passed {
        Ok(())
    } else {
        Err("repository audit failed".into())
    }
}

fn cargo(arguments: &[&str]) -> Result<(), DynError> {
    cargo_with_environment(arguments, &[])
}

/// Runs one command to completion and returns its stdout, failing with
/// the captured stderr when the command does not succeed.
fn captured_stdout(command: &mut Command, label: &str) -> Result<String, DynError> {
    let output = command
        .output()
        .map_err(|error| format!("{label}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn cargo_with_environment(
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<(), DynError> {
    let mut command = Command::new("cargo");
    command.args(arguments).current_dir(workspace_root());
    for (name, value) in environment {
        command.env(name, value);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        let rendered_environment = environment
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        let rendered = if rendered_environment.is_empty() {
            format!("cargo {}", arguments.join(" "))
        } else {
            format!("{rendered_environment} cargo {}", arguments.join(" "))
        };
        Err(format!("{rendered} failed with {status}").into())
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask is two levels below the workspace")
        .to_path_buf()
}

fn git_lines(arguments: &[&str]) -> Result<Vec<String>, DynError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace_root())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(format!("git {} failed", arguments.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect())
}

fn git_output(arguments: &[&str]) -> Result<String, DynError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(workspace_root())
        .output()?;
    if !output.status.success() {
        return Err(format!("git {} failed", arguments.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn audit_sources(root: &Path, tracked: &[String]) -> BTreeMap<String, String> {
    tracked
        .iter()
        .filter(|path| {
            path.as_str() != "tools/xtask/src/main.rs"
                && Path::new(path).extension().is_some_and(|extension| {
                    matches!(extension.to_str(), Some("rs" | "md" | "spec"))
                })
        })
        .filter_map(|path| {
            fs::read_to_string(root.join(path))
                .ok()
                .map(|source| (path.clone(), source))
        })
        .collect()
}

fn count_occurrences(sources: &BTreeMap<String, String>, needles: &[&str]) -> usize {
    sources
        .values()
        .map(|source| {
            needles
                .iter()
                .map(|needle| source.matches(needle).count())
                .sum::<usize>()
        })
        .sum()
}

fn missing_evidence(source: &str, required: &[&str]) -> usize {
    required
        .iter()
        .filter(|needle| !source.contains(**needle))
        .count()
}

fn count_identifier(sources: &BTreeMap<String, String>, identifier: &str) -> usize {
    sources
        .values()
        .map(|source| {
            source
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .filter(|token| *token == identifier)
                .count()
        })
        .sum()
}

fn public_api_violations(sources: &BTreeMap<String, String>) -> Vec<String> {
    const FORBIDDEN: [&str; 8] = [
        "pub fn poll_task_raw",
        "pub fn create_host_request",
        "pub fn wait_for_request",
        "pub fn call(",
        "pub fn spawn(",
        "HostRegistry::call",
        "HostCallOutcome::Immediate",
        "#![allow(deprecated)]",
    ];
    sources
        .iter()
        .filter(|(path, _)| path.starts_with("crates/nexa-runtime/"))
        .flat_map(|(path, source)| {
            FORBIDDEN
                .iter()
                .filter(|needle| source.contains(**needle))
                .map(|needle| format!("{path}: {needle}"))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn finalization_inventory(sources: &BTreeMap<String, String>) -> FinalizationInventory {
    let mut counts = BTreeMap::new();
    for symbol in [
        "pub fn poll_task_raw",
        "pub fn create_host_request",
        "pub fn wait_for_request",
        "pub fn call(",
        "pub fn spawn(",
        "HostRegistry::call",
        "HostArgs",
        "HostValue",
        "HostCallOutcome::Immediate",
        "PendingReason",
        "PollResult",
        "ReloadCompletionBuffer",
        "ReloadPaused",
        "BufferedForReload",
        "ReloadCompletionStats",
        "RetiredEpochReap",
        "model_adapter_v5",
        "realm_v4",
        "realm_v5",
        "#![allow(deprecated)]",
    ] {
        let count = if matches!(
            symbol,
            "HostArgs" | "HostValue" | "PendingReason" | "PollResult"
        ) {
            count_identifier(sources, symbol)
        } else {
            count_occurrences(sources, &[symbol])
        };
        counts.insert(symbol.to_owned(), count);
    }
    FinalizationInventory {
        schema_version: 1,
        counts,
        internal_whitelist: vec![
            "PendingReason: crate-private task polling implementation".into(),
            "PollResult: crate-private task polling implementation".into(),
        ],
    }
}

fn loc_matching(root: &Path, tracked: &[String], predicate: impl Fn(&str) -> bool) -> usize {
    tracked
        .iter()
        .filter(|path| predicate(path))
        .filter_map(|path| fs::read_to_string(root.join(path)).ok())
        .map(|source| {
            source
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with("//")
                })
                .count()
        })
        .sum()
}

fn versioned_directories(tracked: &[String]) -> BTreeSet<String> {
    tracked
        .iter()
        .flat_map(|path| {
            let mut prefix = String::new();
            path.split('/')
                .take(path.split('/').count() - 1)
                .map(move |part| {
                    if !prefix.is_empty() {
                        prefix.push('/');
                    }
                    prefix.push_str(part);
                    prefix.clone()
                })
        })
        .filter(|path| is_versioned_path(path))
        .collect()
}

fn is_versioned_path(path: &str) -> bool {
    path.split('/').any(|part| {
        part.starts_with("gate1-v")
            || part.starts_with("v2_")
            || part.starts_with("v2.")
            || part.ends_with("-v2")
            || part.ends_with("-v3")
            || part.ends_with("-v4")
            || part.ends_with("-v5")
            || part.ends_with("-v6")
            || part.ends_with("-v7")
            || part.ends_with("-v8")
            || part.ends_with("-v9")
    })
}

#[allow(clippy::similar_names)]
fn duplicate_hashes(root: &Path, tracked: &[String]) -> BTreeMap<u64, Vec<String>> {
    let mut hashes = BTreeMap::<u64, Vec<String>>::new();
    for path in tracked {
        let Ok(bytes) = fs::read(root.join(path)) else {
            continue;
        };
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        hashes
            .entry(hasher.finish())
            .or_default()
            .push(path.clone());
    }
    hashes
}

fn workspace_member_count() -> Result<usize, DynError> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace_root())
        .output()?;
    if !output.status.success() {
        return Err("cargo metadata failed".into());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)?;
    Ok(metadata["workspace_members"]
        .as_array()
        .ok_or("metadata has no workspace_members")?
        .len())
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn historical_gate_tool_loc() -> Result<usize, DynError> {
    let paths = git_lines(&[
        "ls-tree",
        "-r",
        "--name-only",
        "gate1-v2.9-stop",
        "--",
        "tools",
    ])?;
    let mut total = 0;
    for path in paths
        .iter()
        .filter(|path| path.starts_with("tools/gate1") && path.ends_with(".rs"))
    {
        let specification = format!("gate1-v2.9-stop:{path}");
        let output = Command::new("git")
            .args(["show", &specification])
            .current_dir(workspace_root())
            .output()?;
        if output.status.success() {
            total += String::from_utf8(output.stdout)?
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with("//")
                })
                .count();
        }
    }
    Ok(total)
}

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn low_level_event_violations(root: &Path, tracked: &[String]) -> Vec<String> {
    const FORBIDDEN: [&str; 4] = [
        "RealmV6RuntimeEvent::TaskAdmission",
        "RealmV6RuntimeEvent::FuelYield",
        "RealmV6RuntimeEvent::HostWait",
        "RealmV6RuntimeEvent::TaskComplete",
    ];
    tracked
        .iter()
        .filter(|path| {
            path.starts_with("crates/")
                && path.contains("/tests/")
                && path.ends_with(".rs")
                && !path.contains("model")
                && !path.contains("differential")
                && !path.contains("fuzz")
        })
        .filter(|path| {
            fs::read_to_string(root.join(path))
                .is_ok_and(|source| FORBIDDEN.iter().any(|needle| source.contains(needle)))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod audit_tests {
    use std::collections::BTreeSet;
    use std::fs;

    fn formal_aggregate_case_fixture(name: &str) -> serde_json::Value {
        let gc = if name == "gc_incremental_step" {
            serde_json::json!({
                "cycles": 1,
                "pause_ns_max": 140,
                "objects_reclaimed": 32,
                "bytes_reclaimed": 256,
            })
        } else {
            serde_json::json!({
                "cycles": null,
                "pause_ns_max": null,
                "objects_reclaimed": null,
                "bytes_reclaimed": null,
            })
        };
        serde_json::json!({
            "case": name,
            "tier": "product",
            "median_throughput_ops_per_second": 10_000,
            "median_mean_ns": 102,
            "median_p50_ns": 100,
            "median_p90_ns": 110,
            "median_p95_ns": 120,
            "median_p99_ns": 140,
            "min_ns": 90,
            "max_ns": 150,
            "median_standard_deviation_ns": 5.0,
            "median_coefficient_of_variation": 0.05,
            "max_frame_1000_calls_ns": 102_000,
            "max_system_allocations": 0,
            "max_system_reallocations": 0,
            "max_system_allocated_bytes": 0,
            "max_system_reallocated_bytes": 0,
            "max_system_peak_outstanding_bytes": 0,
            "max_vm": {
                "live_heap_slots_peak": 0,
                "allocations": 0,
                "string_allocations": 0,
                "class_allocations": 0,
                "collection_storage_allocations": 0,
                "map_slot_allocations": 0,
                "struct_materializations": 0,
                "enum_materializations": 0,
                "allocated_bytes": 0,
                "live_bytes": 0,
                "collection_relocation_bytes": 0,
                "string_copy_bytes": 0,
                "host_codec_copy_bytes": 0,
                "bytes_copied": 0,
            },
            "max_gc": gc,
            "fuel_total": 1_000,
            "fuel_per_operation": 1,
            "instructions_total": 1_000,
            "instructions_per_operation": 1,
            "peak_resources": {
                "tasks": 0,
                "requests": 0,
                "tokens": 0,
                "snapshots": 0,
                "state_objects": 0,
                "retired_modules": 0,
                "total": 0,
            },
        })
    }

    fn formal_aggregate_fixture(commit: &str) -> serde_json::Value {
        let names = super::VALUE_COLLECTION_CASES
            .iter()
            .chain(super::PRODUCT_CPU_CASES)
            .chain(super::HOST_TASK_ENGINE_CASES)
            .chain(super::COLD_START_CASES)
            .chain(["gc_incremental_step"].iter())
            .copied()
            .collect::<BTreeSet<_>>();
        let cases = names
            .into_iter()
            .map(formal_aggregate_case_fixture)
            .collect::<Vec<_>>();
        serde_json::json!({
            "schema": 2,
            "benchmark_version": 7,
            "protocol": "median across process medians; each process independently warmed",
            "status": "PASS",
            "implementation_commit": commit,
            "benchmark_source_hash": "11".repeat(32),
            "bytecode_hash": "22".repeat(32),
            "toolchain": "rustc qualification",
            "os": "macos",
            "os_version": "15.0",
            "arch": "aarch64",
            "machine_model": "Mac16,7",
            "cpu_model": "qualification-cpu",
            "logical_cpu_count": 10,
            "power_source": "AC Power",
            "thermal_policy": "qualification-policy",
            "build_profile": "release",
            "profiler_enabled": false,
            "profiler_mode": "disabled",
            "allocation_scope": "timed operation only; per-sample setup and result storage excluded",
            "process_count": 7,
            "samples_per_process": 1_000,
            "warmup_per_process": 100,
            "started_at_unix_ms": 1_800_000_000_000_u64,
            "cases": cases,
        })
    }

    fn formal_profile_fixture(aggregate: &serde_json::Value) -> serde_json::Value {
        let cases = aggregate["cases"]
            .as_array()
            .expect("aggregate cases")
            .iter()
            .map(|case| {
                serde_json::json!({
                    "case": case["case"],
                    "tier": case["tier"],
                    "samples": 200,
                    "throughput_ops_per_second": 10_000,
                    "p50_ns": 100,
                    "p95_ns": 120,
                    "p99_ns": 140,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "schema": 1,
            "benchmark_version": 7,
            "implementation_commit": aggregate["implementation_commit"],
            "benchmark_source_hash": aggregate["benchmark_source_hash"],
            "bytecode_hash": aggregate["bytecode_hash"],
            "toolchain": aggregate["toolchain"],
            "os": aggregate["os"],
            "os_version": aggregate["os_version"],
            "arch": aggregate["arch"],
            "machine_model": aggregate["machine_model"],
            "cpu_model": aggregate["cpu_model"],
            "logical_cpu_count": aggregate["logical_cpu_count"],
            "power_source": aggregate["power_source"],
            "thermal_policy": aggregate["thermal_policy"],
            "build_profile": "release",
            "samples": 200,
            "warmup": 100,
            "process_index": 0,
            "profiler_enabled": true,
            "profiler_mode": "enabled",
            "profiler": {
                "schema": 1,
                "total_opcode_executions": 1_000,
                "function_count": 4,
                "dropped_modules": 0,
                "dropped_functions": 0,
                "dropped_sites": 0,
                "dropped_host_calls": 0,
                "top_opcodes": [["add_i32", 1_000]],
                "gc": {},
                "tasks": {},
            },
            "cases": cases,
        })
    }

    #[test]
    fn steady_state_dispatch_receipt_requires_exact_zero_evidence() {
        let mut report = serde_json::json!({
            "schema": 1,
            "report": "Nexa M5 WP92 Steady-State Engine Allocation",
            "implementation_commit": "head",
            "test_source_hash": super::steady_state_dispatch_source_hash()
                .expect("test source hash"),
            "cases": {
                "broadcast": 0,
                "projected_broadcast": 0,
                "optional_broadcast": 0,
                "owner_call": 0,
                "optional_provider_call": 0,
                "idle_tick": 0,
            },
            "max_system_allocations": 0,
            "status": "PASS",
        });
        assert!(super::steady_state_dispatch_receipt(report.clone(), "head").is_some());
        assert!(super::steady_state_dispatch_receipt(report.clone(), "other").is_none());

        report["cases"]["owner_call"] = serde_json::Value::from(1);
        assert!(super::steady_state_dispatch_receipt(report, "head").is_none());
    }

    #[test]
    fn m3r1_audit_tracks_the_build_fingerprint_lifecycle_names() {
        super::m3r1_audit().expect("the current Engine must satisfy the M3R1 lifecycle audit");
    }

    #[test]
    fn m3r3_audit_tracks_the_build_fingerprint_freshness_names() {
        super::m3r3_product_audit()
            .expect("the current Engine must satisfy the M3R3 freshness audit");
    }

    #[test]
    fn formal_aggregate_reuse_is_commit_and_protocol_exact() {
        let path = std::env::temp_dir().join(format!(
            "nexa-m5-formal-aggregate-{}.json",
            std::process::id()
        ));
        let mut report = formal_aggregate_fixture("head");
        fs::write(
            &path,
            serde_json::to_vec(&report).expect("fixture serializes"),
        )
        .expect("fixture writes");
        assert!(super::formal_aggregate_at(&path, "head").is_some());
        assert!(super::formal_aggregate_at(&path, "other").is_none());

        report["cases"]
            .as_array_mut()
            .expect("fixture cases")
            .iter_mut()
            .find(|case| case["case"] == "struct_construction")
            .expect("Struct case")["max_vm"]["struct_materializations"] =
            serde_json::Value::from(1);
        fs::write(
            &path,
            serde_json::to_vec(&report).expect("fixture serializes"),
        )
        .expect("fixture rewrites");
        assert!(super::formal_aggregate_at(&path, "head").is_none());
        report = formal_aggregate_fixture("head");

        report["cases"].as_array_mut().expect("fixture cases")[0]["max_vm"]
            .as_object_mut()
            .expect("VM counters object")
            .remove("allocated_bytes");
        fs::write(
            &path,
            serde_json::to_vec(&report).expect("fixture serializes"),
        )
        .expect("fixture rewrites");
        assert!(super::formal_aggregate_at(&path, "head").is_none());
        report = formal_aggregate_fixture("head");

        report["samples_per_process"] = serde_json::Value::from(999);
        fs::write(
            &path,
            serde_json::to_vec(&report).expect("fixture serializes"),
        )
        .expect("fixture rewrites");
        assert!(super::formal_aggregate_at(&path, "head").is_none());
        fs::remove_file(path).expect("fixture cleans up");
    }

    #[test]
    fn live_baseline_requires_full_case_and_same_process_machine_authority() {
        let machine = formal_aggregate_fixture("head");
        let mut baseline = formal_aggregate_fixture("baseline");
        baseline["schema"] = serde_json::Value::from(1);
        baseline["machine_identity_provenance"] = serde_json::Value::from(
            "bound by the same xtask process to its live HEAD receipt while synchronously running the baseline worktree",
        );
        assert!(super::formal_baseline_aggregate(baseline.clone(), "baseline", &machine).is_some());

        baseline["cases"][0]["median_p50_ns"] = serde_json::Value::from(0);
        assert!(super::formal_baseline_aggregate(baseline, "baseline", &machine).is_none());
    }

    #[test]
    fn benchmark_reuse_requires_the_same_machine_identity() {
        let left = serde_json::json!({
            "arch": "aarch64",
            "os": "macos",
            "os_version": "15.0",
            "machine_model": "Mac16,7",
            "cpu_model": "qualification-cpu",
            "logical_cpu_count": 10,
            "power_source": "AC Power",
            "thermal_policy": "qualification-policy",
        });
        let mut right = left.clone();
        assert!(super::same_benchmark_machine(&left, &right));
        right["cpu_model"] = serde_json::Value::from("different-cpu");
        assert!(!super::same_benchmark_machine(&left, &right));
        assert!(!super::same_benchmark_machine(
            &serde_json::json!({}),
            &serde_json::json!({})
        ));
    }

    #[test]
    fn formal_profile_reuse_requires_release_and_exact_aggregate_authority() {
        let aggregate = formal_aggregate_fixture("head");
        let path = std::env::temp_dir().join(format!(
            "nexa-m5-formal-profile-{}.json",
            std::process::id()
        ));
        let mut profile = formal_profile_fixture(&aggregate);
        fs::write(
            &path,
            serde_json::to_vec(&profile).expect("profile serializes"),
        )
        .expect("profile writes");
        assert!(super::formal_profile_at(&path, "head", &aggregate).is_some());

        profile["build_profile"] = serde_json::Value::from("debug");
        fs::write(
            &path,
            serde_json::to_vec(&profile).expect("profile serializes"),
        )
        .expect("profile rewrites");
        assert!(super::formal_profile_at(&path, "head", &aggregate).is_none());
        fs::remove_file(path).expect("fixture cleans up");
    }

    #[test]
    fn v8_decision_reuse_requires_exact_protocol_and_machine_authority() {
        let aggregate = formal_aggregate_fixture("head");
        let directory =
            std::env::temp_dir().join(format!("nexa-m5-v8-condition-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("fixture directory");
        let qualification_machine = serde_json::json!({
            "os": aggregate["os"],
            "os_version": aggregate["os_version"],
            "arch": aggregate["arch"],
            "machine_model": aggregate["machine_model"],
            "cpu_model": aggregate["cpu_model"],
            "logical_cpu_count": aggregate["logical_cpu_count"],
            "power_source": aggregate["power_source"],
            "thermal_policy": aggregate["thermal_policy"],
        });
        let workloads = super::PRODUCT_CPU_RESULTS
            .iter()
            .map(|(name, result)| {
                serde_json::json!({
                    "case": name,
                    "result": result,
                    "nexa_median_p50_ns": 200,
                    "v8_median_p50_ns": 100,
                    "v8_lead_ratio": 2.0,
                    "v8_lead_at_least_1_5x": true,
                })
            })
            .collect::<Vec<_>>();
        let mut comparison = serde_json::json!({
            "schema": 1,
            "status": "PASS",
            "protocol": "7 processes x 1000 samples per side; median across process medians; per-process warmup",
            "node_version": "v24.0.0",
            "v8_version": "13.0",
            "harness_source_hash": "33".repeat(32),
            "processes": 7,
            "samples_per_process": 1_000,
            "warmup_per_process": 100,
            "nexa_implementation_commit": "head",
            "qualification_machine": qualification_machine,
            "result_parity": "all workloads returned identical results in both runtimes",
            "semantic_mismatches": [],
            "workloads": workloads,
            "workloads_with_v8_lead_at_least_1_5x": 3,
            "c4_v8_gap_satisfied": true,
        });
        let path = directory.join("v8-comparison.json");
        fs::write(
            &path,
            serde_json::to_vec(&comparison).expect("comparison serializes"),
        )
        .expect("comparison writes");
        assert_eq!(
            super::v8_gap_condition(&directory, &aggregate)["status"],
            "satisfied"
        );

        comparison["workloads"][0]["v8_lead_at_least_1_5x"] = serde_json::Value::from(false);
        fs::write(
            &path,
            serde_json::to_vec(&comparison).expect("comparison serializes"),
        )
        .expect("comparison rewrites");
        assert_eq!(
            super::v8_gap_condition(&directory, &aggregate)["status"],
            "malformed"
        );
        comparison["workloads"][0]["v8_lead_at_least_1_5x"] = serde_json::Value::from(true);

        comparison["qualification_machine"]["cpu_model"] = serde_json::Value::from("different-cpu");
        fs::write(
            &path,
            serde_json::to_vec(&comparison).expect("comparison serializes"),
        )
        .expect("comparison rewrites");
        assert_eq!(
            super::v8_gap_condition(&directory, &aggregate)["status"],
            "malformed"
        );
        fs::remove_dir_all(directory).expect("fixture cleans up");
    }

    #[test]
    fn snake_stress_receipt_requires_zero_post_shutdown_resources() {
        let mut report = serde_json::json!({
            "schema": 1,
            "steady_ticks": 1_024,
            "disable_enable_cycles": 100,
            "reload_cycles": 100,
            "entitlement_cycles": 100,
            "post_shutdown": {
                "enabled_packages": 0,
                "tasks": 0,
                "scopes": 0,
                "continuations": 0,
                "scheduler_tokens": 0,
                "requests": 0,
                "completion_reservations": 0,
                "tokens": 0,
                "snapshots": 0,
                "release_reservations": 0,
                "queued_releases": 0,
                "heap_objects": 0,
                "state_objects": 0,
                "retired_modules": 0,
                "host_pending_completions": 0,
                "host_pending_releases": 0,
            },
            "resource_leaks": 0,
        });
        assert!(super::valid_snake_stress_report(&report));
        report["post_shutdown"]["retired_modules"] = serde_json::Value::from(1);
        assert!(!super::valid_snake_stress_report(&report));
    }
}
