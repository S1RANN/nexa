use std::path::Path;
use std::process::Command;

use nexa_gate1_v2_9::{AnyError, write_json};
use serde_json::{Value, json};

const I26: &str = "e83579984056226737202fad455ef9e312ae2a22";
const E26: &str = "a9c3ddd2d10c793bff21bd51dccb79ccf7f65f5c";
const D26: &str = "3907e0056c9c0325b7adf5623b664ac8c7438bf9";
const R26: &str = "7d2ea923543a8bfb90e1c164da2656bbde0cbb2b";
const F26: &str = "a59e028ad1067bab6284df3fe83311c938a6c903";
const I26_TREE: &str = "368eac5443586952e0a9da04ed080df59a3d1229";
const E26_TREE: &str = "b65a55a6c62b28557dee9faf773e5585e6cd0a75";
const D26_TREE: &str = "f2c74c13e257d26c98802e622be8fadc32838be4";
const R26_TREE: &str = "d5001c7f5e574a36edd206d910488d004bfed2e8";
const F26_TREE: &str = "df05c608f5345fb7cc0b9defd826a58a9825c743";
const RAW_PREFIX: &str = "reports/raw/gate1_v2_6/";
const HISTORY_ROOT: &str = "reports/history/gate1/v2_6";

pub fn archive() -> Result<Value, AnyError> {
    for (commit, tree) in [
        (I26, I26_TREE),
        (E26, E26_TREE),
        (D26, D26_TREE),
        (R26, R26_TREE),
        (F26, F26_TREE),
    ] {
        verify_commit(commit, tree)?;
    }
    verify_parent(E26, I26)?;
    verify_parent(D26, E26)?;
    verify_parent(R26, D26)?;
    verify_parent(F26, R26)?;
    let manifest = raw_manifest()?;
    let terminal = terminal_record();
    std::fs::create_dir_all(HISTORY_ROOT)?;
    write_json(
        &Path::new(HISTORY_ROOT).join("raw_manifest.json"),
        &manifest,
    )?;
    write_json(&Path::new(HISTORY_ROOT).join("terminal.json"), &terminal)?;
    write_json(
        &Path::new(HISTORY_ROOT).join("commits.json"),
        &commit_record(),
    )?;
    std::fs::write(
        Path::new(HISTORY_ROOT).join("terminal.md"),
        "# Gate 1 v2.6 Terminal Record\n\n\
Gate 1 v2.6 is `STRUCTURAL_CLOSURE_FAILED`, `INCOMPLETE`, and not decision-usable. \
Its Raw H2 results contain the required 32 snapshot scenarios, 12 cleanup cases, zero \
invariant violations, three absolute allocator-observer repeats, and three benchmark \
processes. The frozen Gate generator read obsolete root-level paths, emitted zero, null, \
and sentinel measurements, and failed to compare each derived component outcome with its \
recorded component outcome. The Contract Manifest then trusted Gate self-status instead of \
directly asserting the Raw facts. Consequently D2.6 `STOP`, R2.6, and F2.6 exist as files \
but have no decision authority; v2.6 is superseded by Gate 1 v2.9.\n",
    )?;
    Ok(json!({
        "schema_version": 1,
        "version": "gate1-v2.6",
        "status": "PASS",
        "artifact_count": manifest["artifact_count"],
        "formal_evidence_usable": false,
        "decision_usable": false
    }))
}

fn raw_manifest() -> Result<Value, AnyError> {
    let output = command(&["ls-tree", "-rl", "-r", E26, "reports/raw/gate1_v2_6"])?;
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
            .ok_or("v2.6 Raw path has an unexpected prefix")?;
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
    if artifacts.len() != 337 {
        return Err(format!(
            "v2.6 Raw manifest contains {}, expected 337",
            artifacts.len()
        )
        .into());
    }
    Ok(json!({
        "schema_version": 1,
        "version": "gate1-v2.6",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "source_commit": E26,
        "source_root": "reports/raw/gate1_v2_6",
        "artifact_count": artifacts.len(),
        "formal_evidence_usable": false,
        "decision_usable": false,
        "artifacts": artifacts
    }))
}

fn terminal_record() -> Value {
    json!({
        "schema_version": 1,
        "version": "gate1-v2.6",
        "status": "STRUCTURAL_CLOSURE_FAILED",
        "gate1_status": "INCOMPLETE",
        "formal_execution_complete": true,
        "apparatus_valid": false,
        "raw_h2_facts": {
            "snapshot_scenarios": 32,
            "cleanup_cases": 12,
            "invariant_violations": 0,
            "allocator_observer_repeats": 3,
            "allocation_count_semantics": "ABSOLUTE_GLOBAL_ALLOCATOR_DELTAS",
            "benchmark_processes": 3
        },
        "structural_failures": [
            "H2 Gate generator read obsolete root-level Raw paths",
            "per-run derived outcomes were not compared with recorded component outcomes",
            "Contract Manifest lacked direct Raw assertions for H2 counts and absolute zero allocations"
        ],
        "recorded_product_decision": "STOP",
        "recorded_product_decision_authorized": false,
        "decision_usable": false,
        "formal_evidence_usable": false,
        "decision_commit_created": true,
        "receipt_created": true,
        "finalization_created": true,
        "retries_used": 0,
        "pushed": true,
        "superseded_by": "gate1-v2.9"
    })
}

fn commit_record() -> Value {
    json!({
        "schema_version": 1,
        "version": "gate1-v2.6",
        "implementation": {"sha": I26, "tree": I26_TREE},
        "evidence": {"sha": E26, "tree": E26_TREE, "parent": I26},
        "decision": {"sha": D26, "tree": D26_TREE, "parent": E26, "authorized": false},
        "receipt": {"sha": R26, "tree": R26_TREE, "parent": D26, "authorized": false},
        "finalization": {"sha": F26, "tree": F26_TREE, "parent": R26, "authorized": false}
    })
}

fn verify_commit(commit: &str, expected_tree: &str) -> Result<(), AnyError> {
    if command(&["rev-parse", &format!("{commit}^{{tree}}")])? != expected_tree {
        return Err(format!("{commit} tree does not match the frozen history").into());
    }
    Ok(())
}

fn verify_parent(commit: &str, expected_parent: &str) -> Result<(), AnyError> {
    if command(&["rev-parse", &format!("{commit}^")])? != expected_parent {
        return Err(format!("{commit} parent does not match the frozen history").into());
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
