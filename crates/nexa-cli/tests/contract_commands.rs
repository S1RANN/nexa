use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TestDir {
    root: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-cli-contract-{}-{}-{}",
            label,
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("test directory");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, content).expect("write test file");
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nexa"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .expect("nexa command")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("CLI output is UTF-8")
}

fn toml_limits() -> &'static str {
    "handler_fuel = 20000\n\
     cumulative_budget = 100000\n\
     heap_objects = 1024\n\
     heap_bytes = 67108864\n\
     string_bytes = 1048576\n\
     collection_bytes = 33554432\n\
     host_resources = 32\n\
     tasks = 4\n\
     release_records = 64\n"
}

#[test]
fn contract_check_json_stdout_has_structured_fields() {
    let dir = TestDir::new("json");
    let path = dir.write("valid.contract.nexa", "contract Test;\n");

    let output = dir.run(&[
        "contract", "check", path.to_str().unwrap(),
        "--diagnostic-format", "json",
    ]);
    assert_eq!(output.status.code(), Some(0),
        "exit 0: stderr={}", text(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr must be empty on success");
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is valid JSON");
    assert_eq!(stdout["status"], "ok");
    assert_eq!(stdout["command"], "contract");
    assert!(stdout["data"]["contractPath"].as_str().unwrap_or("").contains("valid.contract.nexa"));
    assert!(stdout["data"]["contractSyntaxVersion"].as_u64().unwrap_or(0) >= 3);
    assert!(stdout["data"]["contractFingerprint"].as_str().unwrap_or("").len() > 8);

    // NDJSON format.
    let output = dir.run(&[
        "contract", "check", path.to_str().unwrap(),
        "--diagnostic-format", "ndjson",
    ]);
    assert_eq!(output.status.code(), Some(0), "exit 0 NDJSON");
    assert!(output.stderr.is_empty());
    let ndjson: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is valid NDJSON");
    assert_eq!(ndjson["schema"], 1);
    assert_eq!(ndjson["status"], "ok");
    assert!(ndjson["data"]["contractPath"].as_str().unwrap_or("").contains("valid.contract.nexa"));
}

#[test]
fn contract_check_error_json_has_contract_diagnostic() {
    let dir = TestDir::new("err");
    let path = dir.write("bad.contract.nexa", "contract Test {\n");

    // JSON error format.
    let output = dir.run(&[
        "contract", "check", path.to_str().unwrap(),
        "--diagnostic-format", "json",
    ]);
    assert_eq!(output.status.code(), Some(1),
        "exit 1: stderr={}", text(&output.stderr));
    assert!(output.stdout.is_empty(), "stdout must be empty on error");
    let stderr: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr is valid JSON");
    assert_eq!(stderr["status"], "error");
    assert_eq!(stderr["command"], "contract");
    assert!(stderr["data"]["contractPath"].as_str().unwrap_or("").contains("bad.contract.nexa"));
    assert!(stderr["data"]["contractSyntaxVersion"].as_u64().unwrap_or(0) >= 3);
    assert!(stderr["data"]["contractDiagnostic"]["message"]
        .as_str().unwrap_or("").contains("invalid"));
    assert_eq!(stderr["data"]["contractDiagnostic"]["exitCode"], 1);

    // NDJSON error format.
    let output = dir.run(&[
        "contract", "check", path.to_str().unwrap(),
        "--diagnostic-format", "ndjson",
    ]);
    assert_eq!(output.status.code(), Some(1), "exit 1 NDJSON");
    assert!(output.stdout.is_empty());
    let ndjson: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr is valid NDJSON");
    assert_eq!(ndjson["schema"], 1);
    assert_eq!(ndjson["status"], "error");
    assert!(ndjson["data"]["contractPath"].as_str().unwrap_or("").contains("bad.contract.nexa"));
}

#[test]
fn contract_check_rejects_wrong_suffix() {
    let dir = TestDir::new("suffix");
    let path = dir.write("contract.wrong", "contract Test;\n");

    let output = dir.run(&[
        "contract", "check", path.to_str().unwrap(),
        "--diagnostic-format", "json",
    ]);
    assert_eq!(output.status.code(), Some(2), "exit 2 for wrong suffix");
    assert!(output.stdout.is_empty(), "stdout empty on error");
    let stderr: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr JSON");
    assert_eq!(stderr["status"], "error");
    assert!(stderr["data"]["contractDiagnostic"]["message"]
        .as_str().unwrap_or("").contains("*.contract.nexa"));
    assert!(stderr["data"]["contractPath"].as_str().unwrap_or("").contains("contract.wrong"));
}

#[test]
fn contract_check_migration_diagnostic_for_old_nidl() {
    let dir = TestDir::new("nidl");
    let path = dir.write("legacy.nidl", "contract Test;\n");

    let output = dir.run(&["contract", "check", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "exit 2 for .nidl");
    let stderr = text(&output.stderr);
    assert!(stderr.contains(".nidl"), "mentions .nidl: {stderr}");
    assert!(stderr.contains("contract.nexa"), "suggests .contract.nexa: {stderr}");
    assert!(stderr.contains("contract Name;"), "mentions flat syntax: {stderr}");
}

#[test]
fn contract_generate_path_validation() {
    let dir = TestDir::new("gen");

    // Wrong suffix.
    let wrong = dir.write("bad.txt", "contract Test;\n");
    let output = dir.run(&["contract", "generate", wrong.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "exit 2 on wrong suffix");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("*.contract.nexa"), "stderr: {stderr}");

    // Valid path.
    let valid = dir.write("good.contract.nexa",
        "contract Test;\nhost { fn log(m: string); }\nnexa { fn run(); }\n");
    let output = dir.run(&["contract", "generate", valid.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0),
        "exit 0: stderr={}", text(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr empty on success");
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Generated from good.contract.nexa"), "header: {stdout}");
}

#[test]
fn contract_check_project_config_legacy_nidl_gives_migration_diagnostic() {
    let dir = TestDir::new("proj");
    let toml = format!(
        "schema = 2\n\
         contract = \"missing.nidl\"\n\
         [[sources]]\n\
         id = \"a\"\n\
         root = \"src\"\n\
         trust = \"first-party\"\n\
         activation = [\"default-enabled\"]\n\
         capabilities = []\n\
         max_packages = 1\n\
         [sources.limits]\n\
         {}",
        toml_limits()
    );
    let project = dir.write("nexa.dev.toml", &toml);
    fs::create_dir_all(dir.path().join("src")).expect("src dir");

    let output = dir.run(&[
        "check", "--project", project.to_str().unwrap(),
        "--diagnostic-format", "json",
    ]);
    assert_eq!(output.status.code(), Some(2), "exit 2 for .nidl project");
    assert!(output.stdout.is_empty(), "stdout empty on error");
    let stderr: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr JSON");
    let message = stderr["data"]["contractDiagnostic"]["message"]
        .as_str().unwrap_or("");
    assert!(message.contains(".nidl"), "migration diagnostic: {message}");
    assert!(message.contains("contract.nexa"), "suggests rename: {message}");
    assert!(stderr["data"]["contractPath"].as_str().unwrap_or("").contains("missing.nidl"),
        "contractPath includes the legacy path");
}

#[test]
fn contract_flag_json_error_has_structured_fields() {
    // Processes: nexa check/build/test --contract <wrong.extension> --diagnostic-format json
    // must all emit structured error with contractPath/contractSyntaxVersion/contractDiagnostic.
    let dir = TestDir::new("flag");
    let src = dir.write("main.nexa", "fn main() -> i32 { return 1; }\n");
    let bad = dir.write("bad.txt", "contract Test;\n");
    let pkg = dir.path().join("pkg");
    fs::create_dir_all(pkg.join("src/example")).expect("pkg src");
    fs::write(pkg.join("package.toml"),
        "schema = 2\nkind = \"application\"\nid = \"example.test\"\nname = \"Test\"\nversion = \"1.0.0\"\nsource_root = \"src\"\nentry = \"example.test\"\nactivation = \"default-enabled\"\nhandler_fuel = 20000\ncapabilities = []\n").expect("manifest");
    fs::write(pkg.join("src/example/test.nexa"),
        "fn value() -> i32 { return 1; }\n").expect("source");

    for (cmd, label) in [("check", "check"), ("build", "build"), ("test", "test")] {
        for fmt in ["json", "ndjson"] {
            let output = dir.run(&[
                cmd, pkg.to_str().unwrap(),
                "--contract", bad.to_str().unwrap(),
                "--diagnostic-format", fmt,
            ]);
            // Wrong suffix is a usage error (exit 2).
            assert_eq!(output.status.code(), Some(2),
                "{} --contract bad.txt --diagnostic-format {}: stderr={}",
                cmd, fmt, text(&output.stderr));
            assert!(output.stdout.is_empty(),
                "{} stdout must be empty on error: {}", cmd, text(&output.stdout));
            let stderr: serde_json::Value =
                serde_json::from_slice(&output.stderr).expect("stderr JSON");
            assert_eq!(stderr["status"], "error", "{cmd} {fmt}");
            assert!(stderr["data"]["contractPath"].as_str().unwrap_or("").contains("bad.txt"),
                "{cmd} {fmt} contractPath");
            assert!(stderr["data"]["contractSyntaxVersion"].as_u64().unwrap_or(0) >= 3,
                "{cmd} {fmt} version");
            assert!(stderr["data"]["contractDiagnostic"]["message"]
                .as_str().unwrap_or("").contains("*.contract.nexa"),
                "{cmd} {fmt} diagnostic");
            assert_eq!(stderr["data"]["contractDiagnostic"]["exitCode"], 2,
                "{cmd} {fmt} exitCode");
        }
    }
}