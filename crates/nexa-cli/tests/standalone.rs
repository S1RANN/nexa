use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-m4r1-standalone-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("standalone fixture directory");
        Self { root }
    }

    fn source(&self, name: &str, source: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, source).expect("standalone source");
        path
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

fn path(path: &Path) -> &str {
    path.to_str().expect("temporary paths are UTF-8")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("CLI output is UTF-8")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn single_file_sync_main_receives_arguments_routes_console_and_returns_exit_code() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "console-main.nexa",
        r"fn main(args: Array<string>) -> i32 {
    host::write_line(args[0]);
    host::write_error_line(args[1]);
    return args.len();
}
",
    );

    let output = fixture.run(&["run", path(&source), "Alice", "Bob"]);
    assert_exit(&output, 2);
    assert_eq!(text(&output.stdout), "Alice\n");
    assert_eq!(text(&output.stderr), "Bob\n");
}

#[test]
fn async_main_and_top_level_await_use_the_runtime_task_path() {
    let fixture = Fixture::new();
    let async_main = fixture.source(
        "async-main.nexa",
        r"async fn main(args: Array<string>) -> i32 {
    return args.len();
}
",
    );
    assert_exit(&fixture.run(&["run", path(&async_main), "one"]), 1);

    let top_level = fixture.source(
        "top-level-await.nexa",
        r#"async fn announce() {
    host::write_line("awaited");
}

announce().await;
"#,
    );
    let output = fixture.run(&["run", path(&top_level)]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "awaited\n");
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
}

#[test]
fn top_level_scripts_receive_implicit_args() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "top-level-args.nexa",
        r"host::write_line(args[0]);
",
    );
    let output = fixture.run(&["run", path(&source), "script-argument"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "script-argument\n");
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
}

#[test]
fn standalone_accepts_relaxed_main_forms_and_formats_console_values() {
    let fixture = Fixture::new();
    for (name, source, arguments, expected_exit, expected_stdout) in [
        (
            "sync-unit-no-args.nexa",
            "fn main() { host::write_line(42); }\n",
            Vec::<&str>::new(),
            0,
            "42\n",
        ),
        (
            "sync-unit-args.nexa",
            "fn main(args: Array<string>) { host::write_line(args); }\n",
            vec!["alpha", "beta"],
            0,
            "[alpha, beta]\n",
        ),
        (
            "sync-i32-no-args.nexa",
            "fn main() -> i32 { return 7; }\n",
            Vec::<&str>::new(),
            7,
            "",
        ),
        (
            "async-unit-no-args.nexa",
            "async fn main() { host::write_line(true); }\n",
            Vec::<&str>::new(),
            0,
            "true\n",
        ),
    ] {
        let source = fixture.source(name, source);
        let mut command = vec!["run", path(&source)];
        command.extend(arguments);
        let output = fixture.run(&command);
        assert_exit(&output, expected_exit);
        assert_eq!(text(&output.stdout), expected_stdout);
        assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    }
}

#[test]
fn standalone_rejects_main_conflicts_missing_main_and_wrong_signatures() {
    let fixture = Fixture::new();
    let conflict = fixture.source(
        "conflict.nexa",
        r"fn main(args: Array<string>) -> i32 {
    return 0;
}

1 + 1;
",
    );
    assert_exit(&fixture.run(&["run", path(&conflict)]), 1);

    for (name, source) in [
        (
            "wrong-argument.nexa",
            "fn main(args: Array<i32>) -> i32 { return 0; }\n",
        ),
        (
            "wrong-result.nexa",
            "fn main(args: Array<string>) -> bool { return true; }\n",
        ),
    ] {
        let source = fixture.source(name, source);
        let output = fixture.run(&["run", path(&source)]);
        assert_exit(&output, 1);
        assert!(
            text(&output.stderr).contains("main"),
            "{}",
            text(&output.stderr)
        );
    }

    let package = write_package_fixture(&fixture.root);
    assert_exit(&fixture.run(&["lock", path(&package)]), 0);
    fs::write(package.join("src/main.nexa"), "fn helper() {}\n").expect("main-less package");
    let output = fixture.run(&["run", path(&package)]);
    assert_exit(&output, 1);
    assert!(
        text(&output.stderr).contains("main"),
        "{}",
        text(&output.stderr)
    );
}

#[test]
fn standalone_reports_bounds_and_type_context_with_actual_values() {
    let fixture = Fixture::new();
    let bounds = fixture.source(
        "bounds.nexa",
        "fn main() { let values = [10, 20]; host::write_line(values[5]); }\n",
    );
    let output = fixture.run(&["run", path(&bounds)]);
    assert_exit(&output, 4);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("array index 5"), "{stderr}");
    assert!(stderr.contains("length 2"), "{stderr}");
    assert!(stderr.contains("valid indices are 0..1"), "{stderr}");

    let mismatch = fixture.source(
        "type-context.nexa",
        "fn consume(value: string) {}\nfn main() { consume(42); }\n",
    );
    let output = fixture.run(&["run", path(&mismatch)]);
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("argument 1 to `consume`"), "{stderr}");
    assert!(stderr.contains("expected string"), "{stderr}");
    assert!(stderr.contains("found i32"), "{stderr}");
}

#[test]
fn package_and_project_entrypoints_receive_arguments_without_function_indices() {
    let fixture = Fixture::new();
    let package = write_package_fixture(&fixture.root);
    assert_exit(&fixture.run(&["lock", path(&package)]), 0);

    assert_exit(
        &fixture.run(&["run", path(&package), "one", "two", "three"]),
        3,
    );
    let project = fixture.root.join("nexa.dev.toml");
    assert_exit(
        &fixture.run(&[
            "run",
            "--project",
            path(&project),
            "--package",
            "standalone.app",
            "one",
            "two",
            "three",
            "four",
        ]),
        4,
    );
}

#[test]
fn bytecode_execution_is_only_available_through_exec() {
    let fixture = Fixture::new();
    let source = fixture.source("low-level.nexa", "fn answer() -> i32 { return 7; }\n");
    let bytecode = fixture.root.join("low-level.nxb");
    assert_exit(
        &fixture.run(&["build", path(&source), "-o", path(&bytecode)]),
        0,
    );

    let ordinary_run = fixture.run(&["run", path(&bytecode), "--function", "0"]);
    assert_exit(&ordinary_run, 2);
    let executed = fixture.run(&["exec", path(&bytecode), "--function", "0"]);
    assert_exit(&executed, 0);
    assert!(
        text(&executed.stdout).contains('7'),
        "{}",
        text(&executed.stdout)
    );
}

#[test]
fn standalone_traps_use_the_fixed_tool_exit_code() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "trap.nexa",
        r#"use std::debug;

fn main(args: Array<string>) -> i32 {
    debug::trap("standalone trap");
    return 0;
}
"#,
    );
    let output = fixture.run(&["run", path(&source)]);
    assert_exit(&output, 4);
    assert!(
        text(&output.stderr).contains("standalone trap"),
        "{}",
        text(&output.stderr)
    );
}

fn write_package_fixture(root: &Path) -> PathBuf {
    let package = root.join("packages/app");
    fs::create_dir_all(package.join("src")).expect("standalone Package source directory");
    fs::write(
        package.join("package.toml"),
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"standalone.app\"\n\
         name = \"Standalone App\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"main\"\n\
         activation = \"default-enabled\"\n",
    )
    .expect("standalone Package Manifest");
    fs::write(
        package.join("src/main.nexa"),
        "fn main(args: Array<string>) -> i32 { return args.len(); }\n",
    )
    .expect("standalone Package entry module");
    fs::write(root.join("app_api.contract.nexa"), "contract EmptyHost;\n")
        .expect("standalone project Contract");
    fs::write(
        root.join("nexa.dev.toml"),
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
         max_packages = 2\n\
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
    .expect("standalone project configuration");
    package
}
