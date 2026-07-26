use std::io::Write;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MODEL_FAILURE_ARTIFACT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFailureArtifact {
    pub format_version: u32,
    pub commit_sha: String,
    pub model_config: Value,
    pub path: Vec<String>,
    pub failure_event: String,
    pub model_before: Value,
    pub model_after: Value,
    pub runtime_before: Value,
    pub runtime_after: Value,
    pub ledger: Value,
    pub epochs: Value,
    pub tasks: Value,
    pub requests: Value,
    pub completions: Value,
    pub releases: Value,
    pub heap: Value,
    pub roots: Value,
    pub trace: Value,
    pub error_code: String,
}

pub fn write_model_failure_artifact(
    writer: impl Write,
    artifact: &ModelFailureArtifact,
) -> Result<(), serde_json::Error> {
    serde_json::to_writer_pretty(writer, artifact)
}

#[must_use]
pub fn current_commit_sha() -> String {
    std::env::var("GITHUB_SHA")
        .ok()
        .filter(|sha| !sha.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|sha| sha.trim().to_owned())
                .filter(|sha| !sha.is_empty())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, write_model_failure_artifact,
    };

    #[test]
    fn artifact_is_valid_json_and_preserves_every_required_section() {
        let artifact = ModelFailureArtifact {
            format_version: MODEL_FAILURE_ARTIFACT_VERSION,
            commit_sha: "0123456789abcdef".into(),
            model_config: json!({"max_depth": 32, "max_worlds": 32768}),
            path: vec!["TaskAdmission".into(), "event with \"quotes\"".into()],
            failure_event: "Cleanup\nTrap".into(),
            model_before: json!({"task": "Cleanup"}),
            model_after: json!({"task": "Trapped"}),
            runtime_before: json!({"task": "Cleanup"}),
            runtime_after: json!({"task": "Trapped"}),
            ledger: json!({"terminal_records": 1}),
            epochs: json!({"active": 0, "retired": []}),
            tasks: json!([{"state": "Trapped"}]),
            requests: json!([]),
            completions: json!([]),
            releases: json!([]),
            heap: json!({"objects": 0}),
            roots: json!([]),
            trace: json!([{"transition": "cleanup.trap"}]),
            error_code: "NEXA_MODEL_DIFFERENTIAL_MISMATCH".into(),
        };
        let mut output = Vec::new();
        write_model_failure_artifact(&mut output, &artifact).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&output).unwrap();

        assert_eq!(parsed["format_version"], MODEL_FAILURE_ARTIFACT_VERSION);
        assert_eq!(parsed["path"][1], "event with \"quotes\"");
        assert_eq!(parsed["failure_event"], "Cleanup\nTrap");
        for field in [
            "commit_sha",
            "model_config",
            "model_before",
            "model_after",
            "runtime_before",
            "runtime_after",
            "ledger",
            "epochs",
            "tasks",
            "requests",
            "completions",
            "releases",
            "heap",
            "roots",
            "trace",
            "error_code",
        ] {
            assert!(parsed.get(field).is_some(), "missing field {field}");
        }
    }
}
