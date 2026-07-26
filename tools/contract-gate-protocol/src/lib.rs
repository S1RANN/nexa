use std::path::Path;

use serde::{Deserialize, Serialize};

pub const GATE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Passed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFailure {
    pub check: String,
    pub reason: String,
}

impl GateFailure {
    #[must_use]
    pub fn new(check: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            check: check.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateArtifact<T> {
    pub schema_version: u32,
    pub gate: String,
    pub implementation_sha: String,
    pub implementation_tree: String,
    pub command: String,
    pub status: GateStatus,
    pub metrics: T,
    pub failures: Vec<GateFailure>,
}

impl<T> GateArtifact<T> {
    #[must_use]
    pub fn new(
        gate: impl Into<String>,
        implementation_sha: impl Into<String>,
        implementation_tree: impl Into<String>,
        command: impl Into<String>,
        metrics: T,
        failures: Vec<GateFailure>,
    ) -> Self {
        let status = if failures.is_empty() {
            GateStatus::Passed
        } else {
            GateStatus::Failed
        };
        Self {
            schema_version: GATE_SCHEMA_VERSION,
            gate: gate.into(),
            implementation_sha: implementation_sha.into(),
            implementation_tree: implementation_tree.into(),
            command: command.into(),
            status,
            metrics,
            failures,
        }
    }

    #[must_use]
    pub const fn passed(&self) -> bool {
        matches!(self.status, GateStatus::Passed)
    }
}

pub fn write_artifact<T: Serialize>(path: &Path, artifact: &GateArtifact<T>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut bytes = serde_json::to_vec_pretty(artifact).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).map_err(|error| error.to_string())
}

pub fn read_artifact<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<GateArtifact<T>, String> {
    serde_json::from_slice(
        &std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}
