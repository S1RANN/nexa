use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type AnyError = Box<dyn std::error::Error>;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");
pub const V2_ROOT: &str = "experiments/gate1-v2.4";
pub const MANIFEST: &str = "experiments/gate1-v2.4/manifest.json";
pub const TARGET_ROOT: &str = "target/gate1-v2.4";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MeasurementKind {
    RuntimeSnapshot,
    EventTrace,
    AllocatorCounter,
    GitDiff,
    CompilerResult,
    VerifierResult,
    ProcessResult,
    FileHash,
    DerivedCalculation,
    ExternalDecision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedMetric<T> {
    pub value: T,
    pub measurement: MeasurementKind,
    pub source_artifact: String,
    pub source_pointer: String,
    pub sample_count: u64,
    pub run_id: String,
}

impl<T> ObservedMetric<T> {
    pub fn new(
        value: T,
        measurement: MeasurementKind,
        source_artifact: impl Into<String>,
        source_pointer: impl Into<String>,
        sample_count: u64,
        run_id: impl Into<String>,
    ) -> Self {
        Self {
            value,
            measurement,
            source_artifact: source_artifact.into(),
            source_pointer: source_pointer.into(),
            sample_count,
            run_id: run_id.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessMetadata {
    pub role: String,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub process_nonce: String,
    pub start_unix_ns: u128,
    pub end_unix_ns: Option<u128>,
    pub elapsed_monotonic_ns: Option<u128>,
    pub executable_hash: String,
    pub command_line: Vec<String>,
    pub run_id: String,
    pub implementation_sha: String,
    pub implementation_tree: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEvent {
    pub sequence: u64,
    pub run_id: String,
    pub process_nonce: String,
    pub phase: String,
    pub operation: String,
    pub before_snapshot_hash: String,
    pub after_snapshot_hash: String,
    pub result: String,
}

pub struct ProcessRecorder {
    path: PathBuf,
    started: std::time::Instant,
    metadata: ProcessMetadata,
}

impl ProcessRecorder {
    pub fn start(
        directory: &Path,
        role: &str,
        run_id: &str,
        nonce: &str,
    ) -> Result<Self, AnyError> {
        std::fs::create_dir_all(directory)?;
        let executable = std::env::current_exe()?;
        let metadata = ProcessMetadata {
            role: role.to_owned(),
            pid: std::process::id(),
            parent_pid: None,
            process_nonce: nonce.to_owned(),
            start_unix_ns: unix_ns()?,
            end_unix_ns: None,
            elapsed_monotonic_ns: None,
            executable_hash: hash_file(&executable)?,
            command_line: std::env::args().collect(),
            run_id: run_id.to_owned(),
            implementation_sha: git(&["rev-parse", "HEAD"])?,
            implementation_tree: git(&["rev-parse", "HEAD^{tree}"])?,
        };
        let path = directory.join("process.json");
        write_json(&path, &metadata)?;
        Ok(Self {
            path,
            started: std::time::Instant::now(),
            metadata,
        })
    }

    pub fn finish(mut self) -> Result<ProcessMetadata, AnyError> {
        self.metadata.elapsed_monotonic_ns = Some(self.started.elapsed().as_nanos());
        self.metadata.end_unix_ns = Some(unix_ns()?);
        write_json(&self.path, &self.metadata)?;
        Ok(self.metadata)
    }
}

pub struct EventLog {
    path: PathBuf,
    run_id: String,
    nonce: String,
    next_sequence: u64,
}

impl EventLog {
    pub fn create(directory: &Path, run_id: &str, nonce: &str) -> Result<Self, AnyError> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join("events.ndjson");
        File::create(&path)?;
        Ok(Self {
            path,
            run_id: run_id.to_owned(),
            nonce: nonce.to_owned(),
            next_sequence: 0,
        })
    }

    pub fn record(
        &mut self,
        phase: &str,
        operation: &str,
        before: &Value,
        after: &Value,
        result: &str,
    ) -> Result<u64, AnyError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let event = RawEvent {
            sequence,
            run_id: self.run_id.clone(),
            process_nonce: self.nonce.clone(),
            phase: phase.to_owned(),
            operation: operation.to_owned(),
            before_snapshot_hash: stable_value_hash(before),
            after_snapshot_hash: stable_value_hash(after),
            result: result.to_owned(),
        };
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        serde_json::to_writer(&mut file, &event)?;
        file.write_all(b"\n")?;
        Ok(sequence)
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.next_sequence
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.next_sequence == 0
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sync(&self) -> Result<(), AnyError> {
        OpenOptions::new()
            .append(true)
            .open(&self.path)?
            .sync_all()?;
        Ok(())
    }
}

pub fn repository_root() -> PathBuf {
    Path::new(ROOT)
        .parent()
        .and_then(Path::parent)
        .expect("tool lives under repository/tools")
        .to_path_buf()
}

#[must_use]
pub fn output_root(label: &str) -> PathBuf {
    repository_root().join(TARGET_ROOT).join(label)
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), AnyError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    std::fs::write(path, bytes)?;
    Ok(())
}

pub fn read_json(path: impl AsRef<Path>) -> Result<Value, AnyError> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn command_output(command: &str, arguments: &[&str]) -> Result<Output, AnyError> {
    Ok(Command::new(command)
        .args(arguments)
        .current_dir(repository_root())
        .output()?)
}

pub fn command_text(command: &str, arguments: &[&str]) -> Result<String, AnyError> {
    let output = command_output(command, arguments)?;
    if !output.status.success() {
        return Err(format!(
            "{command} failed with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub fn git(arguments: &[&str]) -> Result<String, AnyError> {
    command_text("git", arguments)
}

pub fn git_clean_failures() -> Result<Vec<String>, AnyError> {
    let mut failures = Vec::new();
    let status = git(&["status", "--porcelain=v1", "--untracked-files=all"])?;
    if !status.is_empty() {
        failures.push(format!("git status is not clean: {status}"));
    }
    for (name, arguments) in [
        ("tracked diff", &["diff", "--exit-code"][..]),
        ("staged diff", &["diff", "--cached", "--exit-code"][..]),
    ] {
        let output = command_output("git", arguments)?;
        if !output.status.success() {
            failures.push(format!("{name} is not clean"));
        }
    }
    Ok(failures)
}

pub fn hash_file(path: impl AsRef<Path>) -> Result<String, AnyError> {
    let path = path.as_ref();
    let output = Command::new("git")
        .args(["hash-object", "--"])
        .arg(path)
        .current_dir(repository_root())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[must_use]
pub fn stable_value_hash(value: &Value) -> String {
    stable_bytes_hash(&serde_json::to_vec(value).expect("JSON serialization"))
}

#[must_use]
pub fn stable_bytes_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn nonce(role: &str) -> Result<String, AnyError> {
    Ok(format!("{role}-{}-{}", std::process::id(), unix_ns()?))
}

pub fn unix_ns() -> Result<u128, AnyError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())
}

pub fn observed_parent_pid() -> Result<u32, AnyError> {
    let pid = std::process::id().to_string();
    let parent = command_text("ps", &["-o", "ppid=", "-p", &pid])?;
    Ok(parent.trim().parse()?)
}

pub fn bound_hashes(paths: &[&str]) -> Result<BTreeMap<String, String>, AnyError> {
    paths
        .iter()
        .map(|path| Ok(((*path).to_owned(), hash_file(path)?)))
        .collect()
}

pub fn copy_tree(source: &Path, target: &Path) -> Result<(), AnyError> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if source_path.is_dir() {
            copy_tree(&source_path, &target_path)?;
        } else {
            std::fs::copy(source_path, target_path)?;
        }
    }
    Ok(())
}
