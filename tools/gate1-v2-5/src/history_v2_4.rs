use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use nexa_gate1_v2_5::{AnyError, repository_root, write_json};

const ARCHIVE_ROOT: &str = "reports/history/gate1/v2_4/raw";
const HISTORY_ROOT: &str = "reports/history/gate1/v2_4";
const FIXTURE_ROOT: &str = "experiments/gate1-v2.5/fixtures/v2_4_h2";

pub fn archive() -> Result<Value, AnyError> {
    let root = repository_root();
    let archive = root.join(ARCHIVE_ROOT);
    let mut files = Vec::new();
    collect_files(&archive, &mut files)?;
    files.sort();
    if files.len() != 311 {
        return Err(format!("v2.4 archive contains {} files, expected 311", files.len()).into());
    }
    let artifacts = files
        .iter()
        .map(|path| manifest_entry(&root, path))
        .collect::<Result<Vec<_>, AnyError>>()?;
    let manifest = json!({
        "schema_version": 1,
        "version": "gate1-v2.4",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "formal_evidence_usable": false,
        "artifact_count": artifacts.len(),
        "archive_root": ARCHIVE_ROOT,
        "original_root": "reports/raw/gate1_v2_4",
        "artifacts": artifacts
    });
    write_json(
        &root.join(HISTORY_ROOT).join("raw_hash_manifest.json"),
        &manifest,
    )?;
    write_terminal(&root)?;
    write_fixtures(&root)?;
    Ok(json!({
        "status": "PASS",
        "archived_files": files.len(),
        "manifest": format!("{HISTORY_ROOT}/raw_hash_manifest.json"),
        "fixtures": 3
    }))
}

fn manifest_entry(root: &Path, path: &Path) -> Result<Value, AnyError> {
    let relative = path.strip_prefix(root)?.to_string_lossy().to_string();
    let archived_relative = path
        .strip_prefix(root.join(ARCHIVE_ROOT))?
        .to_string_lossy()
        .to_string();
    let original_path = format!("reports/raw/gate1_v2_4/{archived_relative}");
    let blob = Command::new("git")
        .args(["hash-object", "--no-filters"])
        .arg(path)
        .current_dir(root)
        .output()?;
    if !blob.status.success() {
        return Err(format!("git hash-object failed for {relative}").into());
    }
    let run = archived_relative
        .strip_prefix("runs/")
        .and_then(|value| value.split('/').next())
        .map(str::to_owned);
    let gate = archived_relative
        .strip_prefix("gates/")
        .and_then(|value| value.strip_suffix(".json"))
        .map(str::to_owned);
    let artifact_type = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("unknown");
    Ok(json!({
        "archived_path": relative,
        "original_path": original_path,
        "size_bytes": std::fs::metadata(path)?.len(),
        "git_blob_hash": String::from_utf8(blob.stdout)?.trim(),
        "artifact_type": artifact_type,
        "run": run,
        "gate": gate
    }))
}

fn write_terminal(root: &Path) -> Result<(), AnyError> {
    let terminal = json!({
        "schema_version": 1,
        "version": "gate1-v2.4",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "implementation_sha": "b31ab22149253248beed3dca8217392673420f73",
        "implementation_tree": "7ab2147091c750e5a33f5c6a0c19f43197df7b59",
        "formal_execution_complete": true,
        "formal_execution_count": 3,
        "retries_used": 0,
        "product_outcomes": {"H1": "FAIL", "H2": "FAIL", "H3": "FAIL"},
        "stable_signatures": {
            "H1": "af6600462d0eb4df",
            "H3": "98cf6409b1e2e084"
        },
        "h2_signatures": [
            "0869939cb3bd4cc3",
            "5c47d5d73d0816a9",
            "513efea3163b5acd"
        ],
        "h2_signature_contaminated_by_timing_noise": true,
        "contracts_satisfied": 42,
        "contract_count": 44,
        "decision_usable": false,
        "evidence_commit_created": false,
        "receipt_created": false,
        "pushed": false,
        "superseded_by": "gate1-v2.5"
    });
    write_json(&root.join(HISTORY_ROOT).join("terminal.json"), &terminal)?;
    std::fs::write(
        root.join(HISTORY_ROOT).join("terminal.md"),
        "# Gate 1 v2.4 Terminal Record\n\n\
Status: **STRUCTURAL_CLOSURE_FAILED**\n\n\
The three zero-retry formal executions completed with H1/H2/H3 all reporting valid FAIL outcomes. \
H1 and H3 were stable. H2 semantic signatures were contaminated by timing fields. \
Only 42 of 44 contracts were satisfied; no Evidence or Receipt commit exists, the decision is unusable, and nothing was pushed.\n",
    )?;
    Ok(())
}

fn write_fixtures(root: &Path) -> Result<(), AnyError> {
    let fixture_root = root.join(FIXTURE_ROOT);
    std::fs::create_dir_all(&fixture_root)?;
    for run in ["formal-run-1", "formal-run-2", "replay"] {
        let path = root
            .join(ARCHIVE_ROOT)
            .join("runs")
            .join(run)
            .join("worker/result.json");
        let worker: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let mut h2 = worker["h2"].clone();
        h2["formal_evidence_usable"] = json!(false);
        h2["fixture_source_version"] = json!("gate1-v2.4");
        h2["fixture_source_run"] = json!(run);
        write_json(&fixture_root.join(format!("{run}.json")), &h2)?;
    }
    Ok(())
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), AnyError> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}
