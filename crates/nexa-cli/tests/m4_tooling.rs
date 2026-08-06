use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    app: PathBuf,
    contract: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-m4-cli-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let packages = root.join("packages");
        let app = packages.join("app");
        let library = packages.join("library");
        fs::create_dir_all(app.join("src/example")).expect("Application source directory");
        fs::create_dir_all(app.join("tests/basic")).expect("Application test directory");
        fs::create_dir_all(library.join("src/example")).expect("Library source directory");
        fs::write(
            app.join("package.toml"),
            "schema = 2\n\
             kind = \"application\"\n\
             id = \"example.app\"\n\
             name = \"Example App\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n\
             entry = \"example.main\"\n\
             activation = \"default-enabled\"\n\
             [dependencies]\n\
             helper = { path = \"../library\" }\n",
        )
        .expect("Application Manifest");
        fs::write(
            app.join("src/example/main.nexa"),
            "use helper::example::math as math;\n\
             pub fn value() -> i32 { return math::identity(7); }\n",
        )
        .expect("Application source");
        fs::write(
            app.join("tests/basic/value.nexa"),
            "use package::example::main as app;\n\
             @test\n\
             fn value_is_linked() -> bool { return app::value() == 7; }\n",
        )
        .expect("Application test");
        fs::write(
            library.join("package.toml"),
            "schema = 2\n\
             kind = \"library\"\n\
             id = \"example.library\"\n\
             name = \"Example Library\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n",
        )
        .expect("Library Manifest");
        fs::write(
            library.join("src/example/math.nexa"),
            "pub fn identity(value: i32) -> i32 { return value; }\n",
        )
        .expect("Library source");
        let contract = root.join("app_api.contract.nexa");
        fs::write(&contract, "contract EmptyHost;\n").expect("Host Contract");
        let project = root.join("nexa.dev.toml");
        fs::write(
            &project,
            "schema = 2\n\
             contract = \"app_api.contract.nexa\"\n\
             required_entrypoints = []\n\
             [[sources]]\n\
             id = \"fixture\"\n\
             root = \"packages\"\n\
             trust = \"first-party\"\n\
             activation = [\"default-enabled\"]\n\
             capabilities = []\n\
             allow_entitlement = false\n\
             max_packages = 4\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 100000\n\
             heap_objects = 1024\n\
             heap_bytes = 67108864\n\
             string_bytes = 1048576\n\
             collection_bytes = 33554432\n\
             host_resources = 32\n\
             tasks = 4\n\
             release_records = 64\n",
        )
        .expect("Project configuration");
        Self {
            root,
            app,
            contract,
            project,
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nexa"))
            .args(arguments)
            .current_dir(&self.root)
            .output()
            .expect("CLI starts")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("CLI output is UTF-8")
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temporary paths are UTF-8")
}

fn diagnostic_label_at(batch: &Value, start: usize, end: usize) -> &Value {
    batch["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .flat_map(|diagnostic| diagnostic["labels"].as_array().expect("diagnostic labels"))
        .find(|label| {
            label["byteRange"]["start"].as_u64() == u64::try_from(start).ok()
                && label["byteRange"]["end"].as_u64() == u64::try_from(end).ok()
        })
        .expect("diagnostic label at the exact source byte range")
}

#[test]
fn lock_check_build_test_and_dev_share_the_schema2_closure() {
    let fixture = Fixture::new();
    let lock = fixture.run(&["lock", path(&fixture.app), "--diagnostic-format", "json"]);
    assert_exit(&lock, 0);
    let lock_value: Value = serde_json::from_slice(&lock.stdout).expect("lock JSON");
    assert_eq!(lock_value["status"], "ok");
    let lock_path = fixture.app.join("nexa.lock");
    let canonical_lock = fs::read(&lock_path).expect("generated lock");
    assert!(text(&canonical_lock).contains("path = \"library\""));
    assert!(!text(&canonical_lock).contains(path(&fixture.root)));

    for arguments in [
        vec![
            "check",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ],
        vec![
            "test",
            path(&fixture.app),
            "--contract",
            path(&fixture.contract),
            "--diagnostic-format",
            "json",
        ],
        vec![
            "test",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "ndjson",
        ],
        vec![
            "dev",
            "--once",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "ndjson",
        ],
    ] {
        let output = fixture.run(&arguments);
        assert_exit(&output, 0);
        assert_eq!(
            fs::read(&lock_path).expect("lock remains readable"),
            canonical_lock,
            "non-lock command changed nexa.lock"
        );
    }

    let bytecode = fixture.root.join("app.nxb");
    let build = fixture.run(&[
        "build",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "-o",
        path(&bytecode),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&build, 0);
    assert!(bytecode.is_file());
    assert_eq!(fs::read(&lock_path).unwrap(), canonical_lock);
}

#[test]
fn check_reports_compile_phase_type_mismatch_as_unified_diagnostic() {
    let fixture = Fixture::new();
    let source = fixture.root.join("main.nexa");
    // `main` has no declared result type (unit), so the tail literal `1` (i32)
    // survives analysis and fails in the typed lowering with a TypeMismatch.
    fs::write(&source, "fn main() { 1 }").expect("write type-mismatch snippet");

    // Human format: the typed-lowering failure must render through the unified
    // diagnostic pipeline (stable code, user message, exact source line) and
    // must never leak the internal Debug structure of `CompileError`.
    let human = fixture.run(&["check", path(&source)]);
    assert_exit(&human, 1);
    let stderr = text(&human.stderr);
    assert!(
        stderr.contains("error[NX2101]: type mismatch"),
        "human output must show the unified header, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("fn main() { 1 }"),
        "human output must include the exact source line, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("TypeMismatch {"),
        "human output leaked the internal variant structure, stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("SourceSpan {") && !stderr.contains("FileId("),
        "human output leaked internal span structure, stderr:\n{stderr}"
    );

    // JSON format: the machine renderer reports the same source-backed batch.
    let json = fixture.run(&["check", path(&source), "--diagnostic-format", "json"]);
    assert_exit(&json, 1);
    let batch: Value = serde_json::from_slice(&json.stderr).expect("JSON batch on stderr");
    let diagnostic = &batch["diagnostics"][0];
    assert_eq!(diagnostic["code"], "NX2101");
    assert_eq!(diagnostic["severity"], "error");
    assert!(
        diagnostic["message"]
            .as_str()
            .expect("diagnostic message")
            .contains("type mismatch"),
        "JSON message: {diagnostic}"
    );
    let label = diagnostic_label_at(&batch, 12, 13);
    assert_eq!(
        label["source"]["path"],
        path(&source),
        "label must point at the display source, not the virtual package path"
    );
    assert_eq!(label["source"]["packageId"], Value::Null);
}

#[test]
fn dev_compile_failed_events_carry_a_minimal_diagnostic_summary() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    fs::write(
        fixture.app.join("src/example/main.nexa"),
        concat!(
            "pub fn value() -> i32 { return 7; }\n",
            "pub fn broken( -> i32 { return 1; }\n"
        ),
    )
    .expect("write syntax-error Application");

    let output = fixture.run(&[
        "dev",
        "--once",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "ndjson",
    ]);
    assert_exit(&output, 1);
    let compile_failed = text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value["event"] == "compile-failed")
        .unwrap_or_else(|| {
            panic!(
                "dev must emit a compile-failed event, stdout:\n{}",
                text(&output.stdout)
            )
        });
    let message = compile_failed["message"]
        .as_str()
        .expect("compile-failed event message");
    assert_ne!(
        message, "Last Known Good Candidate retained",
        "the event must not drop the diagnostic summary"
    );
    assert!(
        message.contains("NX1002") && message.contains("Last Known Good Candidate retained"),
        "compile-failed event must carry the first diagnostic code plus the retention notice, got: {message}"
    );
}

#[test]
fn dev_reports_real_verifier_failures_after_the_freshness_gate() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    fs::write(
        fixture.app.join("src/example/main.nexa"),
        concat!(
            "@immediate\n",
            "fn expensive() -> i32 {\n\
                 for step in 0..1025 { continue; }\n\
                 return 7;\n\
             }\n"
        ),
    )
    .expect("write verifier-failing Application");

    let output = fixture.run(&[
        "dev",
        "--once",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "ndjson",
    ]);
    assert_exit(&output, 1);
    let events = text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| value["event"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| event == "verify-failed"),
        "dev must retain the façade verifier phase, stdout:\n{}",
        text(&output.stdout)
    );
    assert!(
        !events.iter().any(|event| event == "compile-failed"),
        "a verifier failure must not be mislabeled as a compile failure"
    );
}

#[test]
fn product_check_and_build_ignore_invalid_root_and_dependency_tests() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);

    let baseline_path = fixture.root.join("baseline.nxb");
    let baseline = fixture.run(&[
        "build",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "-o",
        path(&baseline_path),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&baseline, 0);
    let baseline_json: Value =
        serde_json::from_slice(&baseline.stdout).expect("baseline build JSON");

    fs::write(
        fixture.app.join("tests/basic/value.nexa"),
        "@test fn malformed( -> bool { return true; }\n",
    )
    .expect("write invalid root test syntax");
    let dependency_tests = fixture.root.join("packages/library/tests/broken");
    fs::create_dir_all(&dependency_tests).expect("dependency test directory");
    fs::write(
        dependency_tests.join("syntax.nexa"),
        "@test fn malformed( -> bool { return true; }\n",
    )
    .expect("write malformed dependency test");

    let check = fixture.run(&[
        "check",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&check, 0);

    let isolated_path = fixture.root.join("isolated.nxb");
    let isolated = fixture.run(&[
        "build",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "-o",
        path(&isolated_path),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&isolated, 0);
    let isolated_json: Value =
        serde_json::from_slice(&isolated.stdout).expect("isolated build JSON");
    assert_eq!(
        baseline_json["buildFingerprint"], isolated_json["buildFingerprint"],
        "test-only snapshots must not enter the product BuildFingerprint"
    );
    assert_eq!(
        fs::read(&baseline_path).expect("baseline product bytes"),
        fs::read(&isolated_path).expect("isolated product bytes"),
        "root and dependency tests must not enter product bytecode"
    );

    let invalid_root_test = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&invalid_root_test, 1);
    let invalid_root_diagnostics = text(&invalid_root_test.stderr);
    assert!(
        invalid_root_diagnostics.contains("NX1002"),
        "{}",
        invalid_root_diagnostics
    );
    assert!(
        !invalid_root_diagnostics.contains("tests/broken"),
        "dependency Test sources must not enter the root Test target: {invalid_root_diagnostics}"
    );

    fs::write(
        fixture.app.join("tests/basic/value.nexa"),
        "@test fn root_test() -> bool { return true; }\n",
    )
    .expect("restore valid root test");
    let dependency_tests_are_not_part_of_root_test_target = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&dependency_tests_are_not_part_of_root_test_target, 0);
}

#[test]
fn exits_and_machine_failures_are_typed_and_not_duplicated() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);

    let usage = fixture.run(&["test", "--diagnostic-format", "json"]);
    assert_exit(&usage, 2);
    let usage_json: Value = serde_json::from_slice(&usage.stderr).expect("one usage JSON object");
    assert_eq!(usage_json["exitCode"], 2);

    let missing = fixture.run(&[
        "build",
        path(&fixture.root.join("missing.nexa")),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&missing, 3);
    let missing_json: Value =
        serde_json::from_slice(&missing.stderr).expect("one internal/IO JSON object");
    assert_eq!(missing_json["exitCode"], 3);

    fs::write(
        fixture.app.join("tests/basic/value.nexa"),
        "@test\n\
         fn value_is_linked() -> bool { return false; }\n",
    )
    .expect("failing test");
    let failure = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&failure, 1);
    assert!(
        failure.stderr.is_empty(),
        "rendered test failure must not receive a duplicate generic error"
    );
    let failure_json: Value =
        serde_json::from_slice(&failure.stdout).expect("one test-result JSON document");
    assert_eq!(failure_json["status"], "failed");
    assert_eq!(failure_json["summary"]["failed"], 1);
}

#[test]
fn package_test_cli_reports_real_pass_fail_and_trap_outcomes() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    let test_source = fixture.app.join("tests/basic/value.nexa");

    fs::write(
        &test_source,
        r#"use std::debug as debug;

@test
fn a_passes() -> bool {
    return true;
}

@test
fn b_fails() -> bool {
    return false;
}

@test
fn c_traps() -> bool {
    return debug::trap("cli package-test trap");
}

@test
fn d_runs_after_trap() -> bool {
    return true;
}
"#,
    )
    .expect("mixed package tests");
    let mixed = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&mixed, 1);
    assert!(
        mixed.stderr.is_empty(),
        "the structured result is the only failure document: {}",
        text(&mixed.stderr)
    );
    let mixed: Value = serde_json::from_slice(&mixed.stdout).expect("mixed package-test JSON");
    assert_eq!(mixed["summary"]["total"], 4);
    assert_eq!(mixed["summary"]["passed"], 2);
    assert_eq!(mixed["summary"]["failed"], 1);
    assert_eq!(mixed["summary"]["errors"], 1);
    let results = mixed["results"].as_array().expect("test results");
    assert_eq!(
        results
            .iter()
            .map(|result| (
                result["name"].as_str().expect("test name"),
                result["status"].as_str().expect("test status"),
            ))
            .collect::<Vec<_>>(),
        [
            ("a_passes", "PASS"),
            ("b_fails", "FAIL"),
            ("c_traps", "ERROR"),
            ("d_runs_after_trap", "PASS"),
        ]
    );
    let trap = &results[2];
    assert!(
        trap["error"]
            .as_str()
            .is_some_and(|error| error.contains("cli package-test trap"))
    );
    assert!(
        trap["stack"]
            .as_array()
            .is_some_and(|stack| !stack.is_empty())
    );
    assert!(trap["instructions"].as_u64().is_some_and(|count| count > 0));
    assert!(trap["fuel"].as_u64().is_some_and(|fuel| fuel > 0));
}

#[test]
fn package_test_cli_resets_the_budget_per_test() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    let test_source = fixture.app.join("tests/basic/value.nexa");

    fs::write(
        &test_source,
        r"@test
fn a_exhausts_its_budget() -> bool {
    for step in 0..64 {
        step + 1;
    }
    return true;
}

@test
fn b_receives_a_fresh_budget() -> bool {
    return true;
}
",
    )
    .expect("fuel package tests");
    let fuel = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--fuel",
        "4",
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&fuel, 1);
    let fuel: Value = serde_json::from_slice(&fuel.stdout).expect("fuel package-test JSON");
    assert_eq!(fuel["summary"]["total"], 2);
    assert_eq!(fuel["summary"]["passed"], 1);
    assert_eq!(fuel["summary"]["failed"], 0);
    assert_eq!(fuel["summary"]["errors"], 1);
    let results = fuel["results"].as_array().expect("fuel test results");
    assert_eq!(results[0]["name"], "a_exhausts_its_budget");
    assert_eq!(results[0]["status"], "ERROR");
    assert!(
        results[0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("FuelExhaustion"))
    );
    assert_eq!(results[1]["name"], "b_receives_a_fresh_budget");
    assert_eq!(results[1]["status"], "PASS");
}

#[test]
fn package_test_cli_rejects_an_indirect_host_call_before_execution() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    fs::write(
        &fixture.contract,
        "contract TestHost;\n    host {\n        fn clock() -> i32;\n    }\n",
    )
    .expect("Host contract");
    fs::write(
        fixture.app.join("src/example/main.nexa"),
        r"use host::test_host as host;

pub(package) fn forbidden_clock() -> i32 {
    return host::clock();
}
",
    )
    .expect("indirect Host wrapper");
    fs::write(
        fixture.app.join("tests/basic/value.nexa"),
        r"use package::example::main as app;

@test
fn indirect_host() -> bool {
    return app::forbidden_clock() == 0;
}
",
    )
    .expect("indirect Host package test");

    let output = fixture.run(&[
        "test",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&output, 1);
    assert!(
        output.stdout.is_empty(),
        "eligibility failure happens before result execution"
    );
    let diagnostic: Value =
        serde_json::from_slice(&output.stderr).expect("indirect Host diagnostic JSON");
    let diagnostic = &diagnostic["diagnostics"][0];
    assert_eq!(diagnostic["code"], "NX2730");
    assert!(
        diagnostic["message"]
            .as_str()
            .is_some_and(|message| message.contains("indirect_host -> forbidden_clock")),
        "{diagnostic}"
    );
}

#[test]
fn project_check_accepts_nidl_comments_and_documentation() {
    let fixture = Fixture::new();
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    let source = "/// 空 Host contract。\ncontract EmptyHost;\n// 界面\n";
    fs::write(&fixture.contract, source).expect("commented Host Contract");
    let output = fixture.run(&[
        "check",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
}

#[test]
fn non_lock_commands_reject_missing_and_stale_locks_without_mutation() {
    let fixture = Fixture::new();
    let lock_path = fixture.app.join("nexa.lock");

    let missing = fixture.run(&[
        "check",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&missing, 2);
    assert!(!lock_path.exists(), "check must not create nexa.lock");
    let missing_json: Value =
        serde_json::from_slice(&missing.stderr).expect("one missing-lock JSON object");
    assert_eq!(missing_json["exitCode"], 2);

    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);
    let canonical_lock = fs::read(&lock_path).expect("generated lock");
    let library_manifest = fixture.root.join("packages/library/package.toml");
    let changed = fs::read_to_string(&library_manifest)
        .expect("Library Manifest")
        .replace("version = \"1.0.0\"", "version = \"2.0.0\"");
    fs::write(&library_manifest, changed).expect("changed Library Manifest");

    let stale = fixture.run(&[
        "build",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&stale, 2);
    assert_eq!(
        fs::read(&lock_path).expect("stale lock remains readable"),
        canonical_lock,
        "build must not repair a stale nexa.lock"
    );
    let stale_json: Value =
        serde_json::from_slice(&stale.stderr).expect("one stale-lock JSON object");
    assert_eq!(stale_json["exitCode"], 2);
}

#[test]
fn single_file_commands_use_the_virtual_package_and_cannot_target_the_lockfile() {
    let fixture = Fixture::new();
    let source = fixture.root.join("snippet.nexa");
    fs::write(&source, "fn answer() -> i32 { return 42; }\n").expect("single-file source");

    let checked = fixture.run(&["check", path(&source), "--diagnostic-format", "json"]);
    assert_exit(&checked, 0);
    let checked_json: Value =
        serde_json::from_slice(&checked.stdout).expect("single-file check JSON");
    assert_eq!(checked_json["data"]["packageId"], "nexa.snippet");
    assert_eq!(checked_json["data"]["module"], "main");

    let forbidden = fixture.run(&[
        "build",
        path(&source),
        "-o",
        path(&fixture.root.join("nexa.lock")),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&forbidden, 2);
    assert!(
        !fixture.root.join("nexa.lock").exists(),
        "build must not create a file or directory named nexa.lock"
    );
    let forbidden_json: Value =
        serde_json::from_slice(&forbidden.stderr).expect("one forbidden-output JSON object");
    assert_eq!(forbidden_json["exitCode"], 2);
}

#[test]
fn single_file_diagnostics_keep_original_bytes_path_crlf_and_utf16_positions() {
    let fixture = Fixture::new();
    let syntax_path = fixture.root.join("first-line.nexa");
    fs::write(&syntax_path, "#\nfn recovered() {}\n").expect("invalid single-file source");
    let syntax = fixture.run(&["check", path(&syntax_path), "--diagnostic-format", "json"]);
    assert_exit(&syntax, 1);
    assert!(
        syntax.stdout.is_empty(),
        "diagnostic output has one stderr document"
    );
    let syntax_json: Value =
        serde_json::from_slice(&syntax.stderr).expect("single syntax diagnostic JSON document");
    let first_byte = diagnostic_label_at(&syntax_json, 0, 1);
    assert_eq!(first_byte["source"]["packageId"], Value::Null);
    assert_eq!(first_byte["source"]["path"], path(&syntax_path));
    assert_eq!(first_byte["range"]["start"]["line"], 0);
    assert_eq!(first_byte["range"]["start"]["character"], 0);
    assert_eq!(first_byte["range"]["end"]["line"], 0);
    assert_eq!(first_byte["range"]["end"]["character"], 1);

    let type_path = fixture.root.join("crlf-astral.nexa");
    let type_source = "fn wrong() {\r\n    /* 🚀 */ if 1 { return; }\r\n}\r\n";
    fs::write(&type_path, type_source).expect("type-invalid single-file source");
    let type_error = fixture.run(&["check", path(&type_path), "--diagnostic-format", "json"]);
    assert_exit(&type_error, 1);
    let type_json: Value =
        serde_json::from_slice(&type_error.stderr).expect("single type diagnostic JSON document");
    let expected_start = type_source.find("1 {").expect("invalid integer token");
    let integer = diagnostic_label_at(&type_json, expected_start, expected_start + 1);
    assert_eq!(integer["source"]["packageId"], Value::Null);
    assert_eq!(integer["source"]["path"], path(&type_path));
    assert_eq!(integer["range"]["start"]["line"], 1);
    assert_eq!(integer["range"]["start"]["character"], 16);
    assert_eq!(integer["range"]["end"]["line"], 1);
    assert_eq!(integer["range"]["end"]["character"], 17);
}

#[test]
fn single_file_rejects_legacy_module_headers() {
    let fixture = Fixture::new();
    let source = fixture.root.join("explicit-module.nexa");
    fs::write(&source, "module main;\nfn answer() -> i32 { return 42; }\n")
        .expect("legacy module source");
    let rejected = fixture.run(&["check", path(&source), "--diagnostic-format", "json"]);
    assert_exit(&rejected, 1);
    let rejection_json: Value =
        serde_json::from_slice(&rejected.stderr).expect("one legacy syntax diagnostic document");
    assert!(
        rejection_json["diagnostics"]
            .as_array()
            .expect("diagnostics array")
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .is_some_and(|message| message.contains("module"))),
        "legacy module headers must be rejected"
    );
}

#[test]
fn manifest_only_is_strictly_schema2() {
    let fixture = Fixture::new();
    let legacy = fixture.root.join("legacy");
    fs::create_dir_all(&legacy).expect("legacy package directory");
    fs::write(
        legacy.join("package.toml"),
        "schema = 1\n\
         id = \"legacy.package\"\n\
         name = \"Legacy\"\n\
         version = \"1.0.0\"\n\
         entry = \"main.nexa\"\n\
         activation = \"default-enabled\"\n",
    )
    .expect("legacy Manifest");
    let output = fixture.run(&[
        "check",
        path(&legacy),
        "--manifest-only",
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&output, 1);
    let value: Value =
        serde_json::from_slice(&output.stderr).expect("one schema diagnostic JSON object");
    assert_eq!(value["exitCode"], 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn project_required_entrypoints_are_an_exact_subset_while_direct_contracts_require_all() {
    let fixture = Fixture::new();
    fs::write(
        &fixture.contract,
        "contract Host;\n\
             nexa {\n\
                 fn run() -> i32;\n\
                 fn reset();\n\
             }\n",
    )
    .expect("Host Contract with two Nexa entrypoints");
    fs::write(
        fixture.app.join("src/example/main.nexa"),
        "use helper::example::math as math;\n\
         pub fn value() -> i32 { return math::identity(7); }\n\
         pub fn run() -> i32 { return value(); }\n",
    )
    .expect("Application implements only the configured entrypoint");
    let configured = fs::read_to_string(&fixture.project)
        .expect("project configuration")
        .replace(
            "required_entrypoints = []",
            "required_entrypoints = [\"run\"]",
        );
    fs::write(&fixture.project, configured).expect("configured required-entrypoint subset");
    assert_exit(&fixture.run(&["lock", path(&fixture.app)]), 0);

    for arguments in [
        vec![
            "check",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ],
        vec![
            "build",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ],
        vec![
            "test",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ],
        vec![
            "dev",
            "--once",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ],
    ] {
        assert_exit(&fixture.run(&arguments), 0);
    }

    let direct = fixture.run(&[
        "check",
        path(&fixture.app),
        "--contract",
        path(&fixture.contract),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&direct, 1);
    assert!(
        text(&direct.stderr).contains("reset"),
        "direct --contract must require every declared Nexa entrypoint: {}",
        text(&direct.stderr)
    );

    let explicitly_empty = fs::read_to_string(&fixture.project)
        .expect("configured project")
        .replace(
            "required_entrypoints = [\"run\"]",
            "required_entrypoints = []",
        );
    fs::write(&fixture.project, explicitly_empty).expect("explicit empty entrypoint subset");
    fs::write(
        fixture.app.join("src/example/main.nexa"),
        "use helper::example::math as math;\n\
         pub fn value() -> i32 { return math::identity(7); }\n",
    )
    .expect("Application implements no Nexa entrypoint");
    assert_exit(
        &fixture.run(&[
            "check",
            "--project",
            path(&fixture.project),
            "--diagnostic-format",
            "json",
        ]),
        0,
    );

    let omitted = fs::read_to_string(&fixture.project)
        .expect("explicit-empty project")
        .replace("required_entrypoints = []\n", "");
    fs::write(&fixture.project, omitted).expect("omitted required_entrypoints setting");
    let default_all = fixture.run(&[
        "check",
        "--project",
        path(&fixture.project),
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&default_all, 1);
    assert!(
        text(&default_all.stderr).contains("run") || text(&default_all.stderr).contains("reset"),
        "omitting required_entrypoints must restore the complete NIDL surface: {}",
        text(&default_all.stderr)
    );
}

#[test]
fn standalone_compound_assignment_interpolation_and_receiver_methods_execute() {
    let fixture = Fixture::new();
    let source = fixture.root.join("language-features.nexa");
    fs::write(
        &source,
        r#"use host::console;

fn next_index(counter: Array<i32>) -> i32 {
    counter[0] += 1;
    return 1;
}

fn main(args: Array<string>) -> i32 {
    let mut number = 20;
    number += 2;
    number -= 3;
    number *= 4;
    number /= 2;
    number %= 7;
    console::write_line("${number}");

    let values = [10, 2];
    let counter = [0];
    values[next_index(counter)] += 5;
    console::write_line("${values}");
    console::write_line("${counter[0]}");
    console::write_line("${[[1, 2], [3]]}");

    let name = args.get(0).unwrap_or("world");
    console::write_line("hello, ${name}");
    let text = "  nexa  ".trim();
    console::write_line(text);
    console::write_line("${text.contains("nexa")}");
    let parts = "a,b".split(",");
    console::write_line("${parts}");
    let outcome: Result<i32, string> = Result::Ok(9);
    console::write_line("${outcome.unwrap_or(0)}");
    return 0;
}
"#,
    )
    .expect("write standalone feature script");

    let output = fixture.run(&["run", path(&source)]);
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        concat!(
            "3\n",
            "[10, 7]\n",
            "1\n",
            "[[1, 2], [3]]\n",
            "hello, world\n",
            "nexa\n",
            "true\n",
            "[a, b]\n",
            "9\n",
        )
    );
    assert!(
        output.stderr.is_empty(),
        "stderr:\n{}",
        text(&output.stderr)
    );
}

#[test]
fn test_rejects_zero_fuel_before_discovering_an_empty_project() {
    let fixture = Fixture::new();
    let empty_packages = fixture.root.join("empty-packages");
    fs::create_dir_all(&empty_packages).expect("empty Package source root");
    let empty_project = fixture.root.join("empty.dev.toml");
    let source = fs::read_to_string(&fixture.project)
        .expect("project configuration")
        .replace("root = \"packages\"", "root = \"empty-packages\"");
    fs::write(&empty_project, source).expect("empty project configuration");

    let output = fixture.run(&[
        "test",
        "--project",
        path(&empty_project),
        "--fuel",
        "0",
        "--diagnostic-format",
        "json",
    ]);
    assert_exit(&output, 2);
    let error: Value = serde_json::from_slice(&output.stderr).expect("usage JSON");
    assert_eq!(error["exitCode"], 2);
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("greater than zero"))
    );
}
