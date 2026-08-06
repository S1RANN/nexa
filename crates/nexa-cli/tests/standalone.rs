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
        r"use host::console;

fn main(args: Array<string>) -> i32 {
    console::write_line(args[0]);
    console::write_error_line(args[1]);
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
        r#"use host::console;

async fn announce() {
    console::write_line("awaited");
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
        r"use host::console;

console::write_line(args[0]);
",
    );
    let output = fixture.run(&["run", path(&source), "script-argument"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "script-argument\n");
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
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
        ("no-args.nexa", "fn main() -> i32 { return 0; }\n"),
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
