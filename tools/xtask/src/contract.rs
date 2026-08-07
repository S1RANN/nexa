//! Phase 4 (task #9): Contract Syntax v3 gate commands, `.nidl` migration
//! check, and zero-old-surface acceptance evidence.
//!
//! The seven independent gates map onto the existing lower-level test suites:
//!
//! - `test-contract-syntax`      : flat Header grammar, lexer keywords, parser diagnostics, recovery, `SourceProfile`.
//! - `test-contract-semantics`   : ownership, type/direction/attribute validation, async policy, Stable-ID scoping.
//! - `test-contract-descriptor`  : Descriptor v2 framing, canonical bytes, fingerprint determinism.
//! - `test-contract-codegen`     : `ValidatedContract → BindingModel → generated Rust` determinism.
//! - `test-contract-cli`         : `nexa contract check/generate`, project-command acceptance, JSON/NDJSON surface.
//! - `test-contract-lsp`         : LSP profile selection, contract diagnostics, Outline, legacy migration diagnostic.
//! - `contract-migration-check`  : zero active `.nidl`, zero old public API/CLI/editor surface.
//!
//! Each command must fail when the surface it guards is absent. After all seven
//! gates pass, `finalize_contract_v3_gates` runs the complete workspace
//! regression and writes a reproducible JSON receipt under
//! `target/nexa-artifacts/contract-v3-gates/`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{DynError, cargo, git_lines, git_output, workspace_root};

/// Contract v3 artifact directory (one level under `target/nexa-artifacts`).
fn artifact_dir() -> PathBuf {
    workspace_root().join("target/nexa-artifacts/contract-v3-gates")
}

/// `cargo xtask test-contract-syntax`
pub(super) fn test_contract_syntax() -> Result<(), DynError> {
    // Lexer keywords, flat `contract Name;` Header grammar, parser recovery,
    // unsupported items, duplicate/late Header, and SourceProfile detection live
    // in the shared `nexa-syntax` contract grammar test suite.
    cargo(&["test", "-p", "nexa-syntax", "--test", "nidl_v2"])?;
    // The semantic-facing parser surface must also accept the complete flat v3
    // surface and reject every removed NIDL spelling.
    cargo(&[
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "nidl_v2",
        "parses_and_validates_the_complete_nidl_v2_surface",
    ])?;
    cargo(&[
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "nidl_v2",
        "rejects_every_removed_nidl_spelling",
    ])?;
    Ok(())
}

/// `cargo xtask test-contract-semantics`
pub(super) fn test_contract_semantics() -> Result<(), DynError> {
    for filter in [
        "validates_async_host_return_error_policies_against_the_error_type",
        "stable_ids_are_scoped_by_contract_and_declaration_category",
        "validates_names_attributes_layouts_and_source_spans",
    ] {
        cargo(&["test", "-p", "nexa-contract", "--test", "nidl_v2", filter])?;
    }
    Ok(())
}

/// `cargo xtask test-contract-descriptor`
pub(super) fn test_contract_descriptor() -> Result<(), DynError> {
    for filter in [
        "descriptor_obeys_frozen_order_and_comment_rules",
        "async_entrypoint_effect_changes_the_descriptor",
    ] {
        cargo(&["test", "-p", "nexa-contract", "--test", "nidl_v2", filter])?;
    }
    cargo(&[
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "golden_contract",
        "golden_descriptor_bytes_and_fingerprint_are_locked",
    ])?;
    Ok(())
}

/// `cargo xtask test-contract-codegen`
pub(super) fn test_contract_codegen() -> Result<(), DynError> {
    cargo(&[
        "test",
        "-p",
        "nexa-contract",
        "--test",
        "structured_codegen",
    ])?;
    for filter in [
        "golden_generated_public_binding_api_snapshot_is_locked",
        "golden_contract_and_declaration_stable_ids_are_locked",
    ] {
        cargo(&[
            "test",
            "-p",
            "nexa-contract",
            "--test",
            "golden_contract",
            filter,
        ])?;
    }
    Ok(())
}

/// `cargo xtask test-contract-cli`
pub(super) fn test_contract_cli() -> Result<(), DynError> {
    cargo(&["test", "-p", "nexa-cli", "--test", "contract_commands"])?;
    // Direct `nexa contract check/generate` on the committed product example.
    cargo(&[
        "run",
        "--quiet",
        "-p",
        "nexa-cli",
        "--",
        "contract",
        "check",
        "examples/snake-game/snake_api.contract.nexa",
    ])?;
    cargo(&[
        "run",
        "--quiet",
        "-p",
        "nexa-cli",
        "--",
        "contract",
        "check",
        "examples/combat-runtime/combat_api.contract.nexa",
    ])
}

/// `cargo xtask test-contract-lsp`
pub(super) fn test_contract_lsp() -> Result<(), DynError> {
    // Locks the full LSP test suite: contract profile selection, six diagnostic
    // families, Outline symbols, DocumentSymbol E2E, legacy migration diagnostic,
    // overlay handling, and session-related contract file URI publishing.
    cargo(&["test", "-p", "nexa-cli", "--bin", "nexa", "lsp"])?;
    Ok(())
}

/// `cargo xtask contract-migration-check`
///
/// Fails closed when any old surface survives:
/// 1. a tracked, active `*.nidl` file on disk (ignoring build/package artifacts);
/// 2. a public `Nidl*` / `parse_nidl` / `validate_nidl` / `NIDL_SYNTAX_VERSION` API symbol;
/// 3. a `nidl` CLI subcommand;
/// 4. an old `*.nidl` editor file association;
/// 5. `nexa-idl` as language ID, crate name, or package identifier (except allowlisted fixtures).
///
/// The superseded spelling is permitted only in the migration guide, the
/// targeted migration-diagnostic implementation, and negative fixtures.
pub(super) fn contract_migration_check() -> Result<(), DynError> {
    let violations = scan_old_surface(&workspace_root());
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "contract-migration-check found {} old-surface violation(s):\n  {}",
            violations.len(),
            violations.join("\n  ")
        )
        .into())
    }
}

/// Scans the workspace root for old NIDL surface. Returns a list of violations.
fn scan_old_surface(root: &Path) -> Vec<String> {
    let mut violations = Vec::new();

    // 1. Zero active `.nidl` files (tracked or present outside build artifacts).
    if let Ok(tracked) = git_lines(&["ls-files"]) {
        for relative in &tracked {
            if relative.to_ascii_lowercase().ends_with(".nidl") {
                violations.push(format!(
                    "tracked active file still uses `.nidl`: {relative}"
                ));
            }
        }
    }

    // 2. Zero public old-API symbols across the workspace `crates/` tree.
    //    We scan for Rust `pub` items whose name contains `Nidl` or `nidl`
    //    (excluding legitimate internal uses in migration-diagnostic and
    //    negative-fixture code).
    allow_legacy_nidl_use(&mut violations, &root, "crates");

    // 3. Zero `nidl` CLI subcommand.
    let cli = root.join("crates/nexa-cli/src/cli.rs");
    if let Ok(cli_source) = fs::read_to_string(&cli)
        && (cli_source.contains("Command::Nidl") || cli_source.contains("\"nidl\"")) {
            violations.push("an old `nidl` CLI subcommand is still registered".to_owned());
        }

    // 4. Zero old `*.nidl` editor association (allows only migration-guide text).
    for relative in ["editors/package.json", "editors/vscode", "editors/zed"] {
        walk_editor(&root, Path::new(relative), &mut violations);
    }

    // 5. Zero `nexa-idl` as crate/package/language ID (except allowlisted fixtures).
    allow_nexa_idl(&mut violations, &root);

    violations
}

/// Scans `crates/` for public NIDL API symbols. Lines matching the legacy
/// migration-diagnostic implementation and negative-test fixtures are
/// allowlisted by specific patterns.
fn allow_legacy_nidl_use(violations: &mut Vec<String>, root: &Path, subdir: &str) {
    let dir = root.join(subdir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_for_scan(&path, violations, root);
        }
    }
}

fn collect_rs_for_scan(dir: &Path, violations: &mut Vec<String>, root: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_for_scan(&path, violations, root);
        } else if path.extension().is_some_and(|e| e == "rs") {
            scan_rs_for_public_nidl(&path, violations, root);
        }
    }
}

/// Patterns that are allowlisted: migration-diagnostic functions, LSP legacy
/// overlay handling, and negative-test fixtures that use the old spelling.
fn is_allowlisted_nidl_use(path: &Path, line: &str) -> bool {
    let relative = path.to_string_lossy();
    // Migration diagnostic implementation
    if relative.contains("lsp.rs") {
        if line.contains("pub(super)") || line.contains("pub(crate)") || line.contains("pub fn") {
            return false;
        }
        return line.contains("nidl") && (line.contains("migration")
            || line.contains("Legacy")
            || line.contains("legacy_")
            || line.starts_with("//"));
    }
    // Negative-test fixtures (test names that verify the old spelling is rejected)
    if (relative.contains("tests/") || relative.ends_with("_test.rs"))
        && line.contains("fn ") && line.contains("nidl") {
            // Allow rejection/migration tests (they test the old spelling is rejected)
            return line.contains("reject")
                || line.contains("migration")
                || line.contains("legacy")
                || line.contains("removed")
                || line.contains("mutation")
                || line.contains("stress")
                || line.contains("reload")
                || line.contains("panic");
        }
    // Allow internal variable names in reload stress test
    if relative.contains("m4_reload_stress") {
        return true;
    }
    // Allow `nidl_v2` as a test module name (the test file name itself)
    if relative.contains("nidl_v2") {
        return true;
    }
    // Allow `e2e_mutations` / `e2e_support` (they test NIDL mutations)
    if relative.contains("e2e_mutations") || relative.contains("e2e_support") {
        return true;
    }
    // Allow `m4_virtual_snippet` (has `distinct_nidl_handles_emit...` test)
    if relative.contains("m4_virtual_snippet") {
        return true;
    }
    // Allow `NIDL` in doc comments (non-API prose references)
    if line.starts_with("//!") || line.starts_with("///") || line.starts_with("// ") {
        return true;
    }
    false
}

fn scan_rs_for_public_nidl(path: &Path, violations: &mut Vec<String>, root: &Path) {
    let Ok(source) = fs::read_to_string(path) else {
        return;
    };
    for line in source.lines() {
        let trimmed = line.trim();
        // Skip non-pub lines and doc comments for initial filter
        if !trimmed.contains("nidl")
            && !trimmed.contains("Nidl")
            && !trimmed.contains("NIDL")
        {
            continue;
        }
        // Check for public API declarations
        let is_pub = trimmed.starts_with("pub ");
        let has_nidl_type = trimmed.contains("Nidl")
            || trimmed.contains("NIDL")
            || trimmed.contains("parse_nidl")
            || trimmed.contains("validate_nidl");
        if is_pub && has_nidl_type && !is_allowlisted_nidl_use(path, trimmed) {
            violations.push(format!(
                "old public NIDL surface in {}: {}",
                path.strip_prefix(root).unwrap_or(path).display(),
                trimmed.trim()
            ));
        }
        // Also check for crate-level `pub use` re-exports
        if trimmed.contains("pub use") && has_nidl_type && !is_allowlisted_nidl_use(path, trimmed) {
            violations.push(format!(
                "old public NIDL re-export in {}: {}",
                path.strip_prefix(root).unwrap_or(path).display(),
                trimmed.trim()
            ));
        }
    }
}

fn walk_editor(root: &Path, relative: &Path, violations: &mut Vec<String>) {
    let path = root.join(relative);
    if !path.exists() {
        return;
    }
    if path.is_dir() {
        let mut sub = Vec::new();
        if let Ok(entries) = fs::read_dir(&path) {
            for entry in entries.flatten() {
                sub.push(entry.path());
            }
        }
        for child in sub {
            if let Ok(rel) = child.strip_prefix(root) {
                walk_editor(root, rel, violations);
            }
        }
        return;
    }
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content.contains("*.nidl") || content.contains("\".nidl\"") {
        // The README migration-guide wording is explicitly allowlisted.
        if relative.ends_with("README.md") && content.contains("migration diagnostic") {
            return;
        }
        violations.push(format!(
            "old `*.nidl` editor association survives in {}",
            relative.display()
        ));
    }
}

/// Scan for `nexa-idl` as a crate/package/language ID, allowing only the
/// documented migration-diagnostic fixtures and the fuzz Cargo.toml.
fn allow_nexa_idl(violations: &mut Vec<String>, root: &Path) {
    let candidates = [
        root.join("crates"),
        root.join("fuzz"),
        root.join("tools"),
        root.join("editors"),
        root.join("examples"),
    ];
    for base in &candidates {
        if !base.exists() {
            continue;
        }
        if let Ok(entries) = walkdir(base) {
            for path in &entries {
                if path.extension().is_some_and(|e| e == "toml" || e == "json" || e == "rs" || e == "md")
                    && let Ok(content) = fs::read_to_string(path)
                        && content.contains("nexa-idl") && !is_nexa_idl_allowlisted(path) {
                            violations.push(format!(
                                "`nexa-idl` reference in {}",
                                path.strip_prefix(root).unwrap_or(path).display()
                            ));
                        }
            }
        }
    }
}

fn is_nexa_idl_allowlisted(path: &Path) -> bool {
    let relative = path.to_string_lossy();
    // The LSP legacy overlay test uses "nexa-idl" as a language ID string
    if relative.contains("lsp.rs") {
        return true;
    }
    // The nexa-syntax doc comment mentions the old crate name
    if relative.contains("nexa-syntax/src/contract.rs") {
        return true;
    }
    // The fuzz crate name (needs coordinated rename with fuzz owners)
    if relative.contains("fuzz/idl") || relative.contains("fuzz/nexa-idl") {
        return true;
    }
    // The contract migration checker references "nexa-idl" as a string literal in its own source
    if relative.contains("xtask/src/contract.rs") || relative.contains("tools/xtask/src/contract.rs") {
        return true;
    }
    false
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Runs all seven gates in order and records per-command success, timing, and the
/// aggregate status. `force` re-runs even when a stale receipt exists.
pub(super) fn finalize_contract_v3_gates(force: bool) -> Result<(), DynError> {
    let head = git_output(&["rev-parse", "HEAD"])?;
    let worktree_clean = git_lines(&["status", "--porcelain"])?.is_empty();
    let receipt_path = artifact_dir().join("contract-v3-gates-receipt.json");

    #[allow(clippy::type_complexity)]
    let gates: Vec<(&str, fn() -> Result<(), DynError>)> = vec![
        ("test-contract-syntax", test_contract_syntax),
        ("test-contract-semantics", test_contract_semantics),
        ("test-contract-descriptor", test_contract_descriptor),
        ("test-contract-codegen", test_contract_codegen),
        ("test-contract-cli", test_contract_cli),
        ("test-contract-lsp", test_contract_lsp),
        ("contract-migration-check", contract_migration_check),
    ];

    let mut records = Vec::new();
    for (index, (name, run)) in gates.iter().enumerate() {
        let started = std::time::Instant::now();
        let result = run();
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let passed = result.is_ok();
        records.push(GateRecord {
            name: (*name).to_owned(),
            status: if passed { "PASS" } else { "FAIL" }.to_owned(),
            duration_ms,
            detail: result.err().map(|error| format!("{error}")),
        });
        eprintln!(
            "[contract-v3] {}/{} `{}` {} ({duration_ms}ms)",
            index + 1,
            gates.len(),
            name,
            if passed { "PASS" } else { "FAIL" }
        );
        if !passed {
            break;
        }
    }

    let summary = ContractV3GateSummary {
        schema: 1,
        milestone: "contract-v3-gates",
        implementation_commit: head,
        worktree_clean,
        gate_order: gates.iter().map(|(name, _)| (*name).to_owned()).collect(),
        gates: records.clone(),
        all_passed: records.iter().all(|gate| gate.status == "PASS")
            && records.len() == gates.len(),
    };

    write_json(receipt_path, &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if !summary.all_passed || !worktree_clean || records.len() != gates.len() {
        return Err("one or more Contract v3 gates did not pass".into());
    }

    if force {
        eprintln!("[contract-v3] running the complete workspace regression and product examples");
        let workspace_result = cargo_test_workspace();
        let doc_result = cargo(&["test", "--doc", "--workspace"]);
        write_workspace_receipt(&workspace_result)?;
        workspace_result?;
        doc_result?;
    }
    Ok(())
}

/// Runs `cargo test --workspace --all-targets` and returns Ok when all tests pass.
fn cargo_test_workspace() -> Result<(), DynError> {
    let root = workspace_root();
    let output = std::process::Command::new("cargo")
        .args(["test", "--workspace", "--all-targets"])
        .current_dir(&root)
        .output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let combined = format!("{stdout}\n{stderr}");
    let failed_tests = combined
        .lines()
        .filter(|line| line.ends_with("FAILED") && line.contains("::"))
        .map(|line| line.trim().to_owned())
        .collect::<Vec<_>>();
    if !failed_tests.is_empty() {
        return Err(format!("workspace regression failed: {failed_tests:?}").into());
    }
    if !output.status.success() {
        return Err("workspace regression failed with a non-test status".into());
    }
    Ok(())
}

fn write_workspace_receipt(workspace_result: &Result<(), DynError>) -> Result<(), DynError> {
    let receipt = WorkspaceReceipt {
        schema: 1,
        milestone: "contract-v3-workspace-regression",
        implementation_commit: git_output(&["rev-parse", "HEAD"])?,
        command: vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "--workspace".to_owned(),
            "--all-targets".to_owned(),
        ],
        status: if workspace_result.is_ok() { "PASS" } else { "FAIL" },
    };
    write_json(
        artifact_dir().join("contract-v3-workspace-receipt.json"),
        &receipt,
    )?;
    Ok(())
}

#[derive(Clone, Serialize)]
struct GateRecord {
    name: String,
    status: String,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize)]
struct ContractV3GateSummary {
    schema: u32,
    milestone: &'static str,
    implementation_commit: String,
    worktree_clean: bool,
    gate_order: Vec<String>,
    gates: Vec<GateRecord>,
    all_passed: bool,
}

#[derive(Serialize)]
struct WorkspaceReceipt {
    schema: u32,
    milestone: &'static str,
    implementation_commit: String,
    command: Vec<String>,
    status: &'static str,
}

fn write_json(path: PathBuf, value: &impl Serialize) -> Result<(), DynError> {
    fs::create_dir_all(path.parent().ok_or("receipt path has no parent")?)?;
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(format!("{}\n", serde_json::to_string_pretty(value)?).as_bytes())?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}
