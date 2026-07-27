use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use nexa_gate1_v2_8::{AnyError, write_json};
use serde_json::{Value, json};

const I25: &str = "c00665630871dc813541e2bb4506141f57cd51da";
const E25: &str = "d3876db85f2c031a51b6a4f87abdd51515c5fbdf";
const I25_TREE: &str = "6b246f8c0415de704228e0109adb959ca727dd7c";
const E25_TREE: &str = "497fde0c945ab02066205a025370faab6b164ec4";
const RAW_PREFIX: &str = "reports/raw/gate1_v2_5/";
const HISTORY_ROOT: &str = "reports/history/gate1/v2_5";
const FIXTURE_ROOT: &str = "experiments/gate1-v2.8/fixtures";

pub fn archive() -> Result<Value, AnyError> {
    verify_commit(I25, I25_TREE)?;
    verify_commit(E25, E25_TREE)?;
    let manifest = raw_manifest()?;
    let terminal = terminal_record();
    let commits = commit_record();
    std::fs::create_dir_all(HISTORY_ROOT)?;
    write_json(
        &Path::new(HISTORY_ROOT).join("raw_manifest.json"),
        &manifest,
    )?;
    write_json(&Path::new(HISTORY_ROOT).join("terminal.json"), &terminal)?;
    write_json(&Path::new(HISTORY_ROOT).join("commits.json"), &commits)?;
    std::fs::write(
        Path::new(HISTORY_ROOT).join("terminal.md"),
        "# Gate 1 v2.5 Terminal Record\n\n\
Gate 1 v2.5 is `STRUCTURAL_CLOSURE_FAILED` and is not decision-usable. Its three apparatus-valid \
formal executions produced stable H1/H2/H3 `FAIL` outcomes and stable H2 semantic/allocation \
projections, while both performance comparisons were legitimately `INCONCLUSIVE`. Raw-to-Gate \
rebuild passed, but the frozen Gate generator incorrectly mapped legal `INCONCLUSIVE` outcomes to \
Contract `FAIL`; D2.5, R2.5, F2.5, Receipt, finalization, and push do not exist.\n",
    )?;
    materialize_regression_fixtures()?;
    Ok(json!({
        "schema_version": 1,
        "version": "gate1-v2.5",
        "status": "PASS",
        "artifact_count": manifest["artifact_count"],
        "formal_evidence_usable": false
    }))
}

fn raw_manifest() -> Result<Value, AnyError> {
    let output = command(&["ls-tree", "-rl", "-r", E25, "reports/raw/gate1_v2_5"])?;
    let mut artifacts = Vec::new();
    for line in output.lines() {
        let (metadata, path) = line
            .split_once('\t')
            .ok_or("unexpected git ls-tree output")?;
        let fields = metadata.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 || fields[1] != "blob" {
            return Err(format!("unexpected git ls-tree record: {line}").into());
        }
        let relative_path = path
            .strip_prefix(RAW_PREFIX)
            .ok_or("v2.5 Raw path has an unexpected prefix")?;
        let parts = relative_path.split('/').collect::<Vec<_>>();
        let (artifact_type, run_id, gate_id) = match parts.as_slice() {
            ["runs", run, ..] => ("run", Some(*run), None),
            ["comparisons", ..] => ("comparison", None, None),
            ["gates", file] if *file != "manifest.json" => {
                ("gate", None, file.strip_suffix(".json"))
            }
            ["gates", "manifest.json"] => ("gate_manifest", None, None),
            ["static", ..] => ("static_input", None, None),
            _ => ("raw", None, None),
        };
        artifacts.push(json!({
            "path": relative_path,
            "size": fields[3].parse::<u64>()?,
            "content_hash": fields[2],
            "hash_algorithm": "git-blob-sha1",
            "artifact_type": artifact_type,
            "run_id": run_id,
            "gate_id": gate_id
        }));
    }
    if artifacts.len() != 331 {
        return Err(format!(
            "v2.5 Raw manifest contains {}, expected 331",
            artifacts.len()
        )
        .into());
    }
    Ok(json!({
        "schema_version": 1,
        "version": "gate1-v2.5",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "source_commit": E25,
        "source_root": "reports/raw/gate1_v2_5",
        "artifact_count": artifacts.len(),
        "formal_evidence_usable": false,
        "artifacts": artifacts
    }))
}

fn terminal_record() -> Value {
    json!({
        "schema_version": 1,
        "version": "gate1-v2.5",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "formal_execution_complete": true,
        "apparatus_valid": true,
        "product_outcomes": {"H1a": "FAIL", "H2a": "FAIL", "H3a": "FAIL"},
        "comparison": "INCONCLUSIVE",
        "replay": "INCONCLUSIVE",
        "raw_to_gate_rebuild": "PASS",
        "gate_contracts_passed": 19,
        "gate_contract_count": 21,
        "decision_usable": false,
        "decision_commit_created": false,
        "receipt_created": false,
        "finalization_created": false,
        "retries_used": 0,
        "pushed": false,
        "superseded_by": "gate1-v2.8"
    })
}

fn commit_record() -> Value {
    json!({
        "schema_version": 1,
        "version": "gate1-v2.5",
        "implementation": {"sha": I25, "tree": I25_TREE},
        "evidence": {"sha": E25, "tree": E25_TREE, "parent": I25},
        "decision": null,
        "receipt": null,
        "finalization": null
    })
}

fn materialize_regression_fixtures() -> Result<(), AnyError> {
    let h2_root = Path::new(FIXTURE_ROOT).join("v2_5_h2");
    let comparison_root = Path::new(FIXTURE_ROOT).join("v2_5_comparisons");
    std::fs::create_dir_all(&h2_root)?;
    std::fs::create_dir_all(&comparison_root)?;
    for run in ["formal-run-1", "formal-run-2", "replay"] {
        let mut h2 = git_json(&format!("{RAW_PREFIX}runs/{run}/h2/result.json"))?;
        h2["fixture_source_version"] = json!("gate1-v2.5");
        h2["formal_evidence_usable"] = json!(false);
        h2["source_commit"] = json!(E25);
        write_json(&h2_root.join(format!("{run}.json")), &h2)?;
    }
    for (name, path) in BTreeMap::from([
        ("formal", "comparisons/formal-run-1__formal-run-2.json"),
        ("replay", "comparisons/formal-run-1__replay.json"),
    ]) {
        let mut comparison = git_json(&format!("{RAW_PREFIX}{path}"))?;
        comparison["fixture_source_version"] = json!("gate1-v2.5");
        comparison["formal_evidence_usable"] = json!(false);
        comparison["source_commit"] = json!(E25);
        write_json(&comparison_root.join(format!("{name}.json")), &comparison)?;
    }
    Ok(())
}

fn git_json(path: &str) -> Result<Value, AnyError> {
    Ok(serde_json::from_str(&command(&[
        "show",
        &format!("{E25}:{path}"),
    ])?)?)
}

fn verify_commit(commit: &str, expected_tree: &str) -> Result<(), AnyError> {
    if command(&["rev-parse", &format!("{commit}^{{tree}}")])? != expected_tree {
        return Err(format!("{commit} tree does not match the frozen history").into());
    }
    Ok(())
}

fn command(arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new("git").args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
