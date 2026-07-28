use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::Value;

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
    legacy_host_abi_violations: usize,
    completion_buffer_symbol_violations: usize,
    reload_pause_symbol_violations: usize,
    retired_epoch_business_api_violations: usize,
    deprecated_allow_violations: usize,
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

fn main() -> Result<(), DynError> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match command.as_str() {
        "check" => check(),
        "test-core" => test_core(),
        "test-binding" => test_binding(),
        "test-task" => cargo(&["test", "-p", "nexa-runtime", "--test", "task_lifecycle"]),
        "test-reload" => cargo(&["test", "-p", "nexa-runtime", "--test", "restart_reload"]),
        "test-model" => cargo(&["test", "-p", "nexa-model"]),
        "fuzz-smoke" => fuzz_smoke(),
        "bench-smoke" => bench_smoke(),
        "repo-audit" => repo_audit(),
        _ => {
            eprintln!(
                "usage: cargo xtask \
                 check|test-core|test-binding|test-task|test-reload|test-model|\
                 fuzz-smoke|bench-smoke|repo-audit"
            );
            Err("unknown xtask command".into())
        }
    }
}

fn check() -> Result<(), DynError> {
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
    repo_audit()?;
    test_core()?;
    test_binding()?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "task_lifecycle"])?;
    cargo(&["test", "-p", "nexa-runtime", "--test", "restart_reload"])?;
    cargo(&["test", "-p", "nexa-model"])?;
    fuzz_smoke()?;
    bench_smoke()
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
        "run",
        "-p",
        "nexa-cli",
        "--",
        "idl",
        "check",
        "examples/combat-runtime/combat_api.nidl",
    ])?;
    cargo(&["check", "-p", "combat-runtime"])
}

fn fuzz_smoke() -> Result<(), DynError> {
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
        cargo(&[
            "check",
            "--quiet",
            "--manifest-path",
            &format!("{directory}/Cargo.toml"),
        ])?;
    }
    Ok(())
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
        && legacy_host_abi_violations == 0
        && completion_buffer_symbol_violations == 0
        && reload_pause_symbol_violations == 0
        && retired_epoch_business_api_violations == 0
        && deprecated_allow_violations == 0
        && versioned_model_file_count == 0
        && tag_valid;
    let report = RepoHealth {
        schema_version: 2,
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
        legacy_host_abi_violations,
        completion_buffer_symbol_violations,
        reload_pause_symbol_violations,
        retired_epoch_business_api_violations,
        deprecated_allow_violations,
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
    let status = Command::new("cargo")
        .args(arguments)
        .current_dir(workspace_root())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo {} failed with {status}", arguments.join(" ")).into())
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
    const FORBIDDEN: [&str; 6] = [
        "pub fn poll_task_raw",
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
        "RealmV5RuntimeEvent::TaskAdmission",
        "RealmV5RuntimeEvent::FuelYield",
        "RealmV5RuntimeEvent::HostWait",
        "RealmV5RuntimeEvent::TaskComplete",
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
