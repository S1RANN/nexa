use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    project: PathBuf,
    limits: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-verifier-limits-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let source = root.join("standalone.nexa");
        let packages = root.join("packages");
        let app = packages.join("app");
        fs::create_dir_all(app.join("src/example")).expect("Application source directory");

        let parameters = (0..8_193)
            .map(|index| format!("parameter_{index}: i32"))
            .collect::<Vec<_>>()
            .join(", ");
        let oversized_frame = format!(
            "fn main(args: Array<string>) -> i32 {{ return 0; }}\n\
             fn oversized_frame({parameters}) -> i32 {{ return 7; }}\n"
        );
        fs::write(&source, &oversized_frame).expect("standalone source");
        fs::write(
            app.join("package.toml"),
            "schema = 2\n\
             kind = \"application\"\n\
             id = \"example.app\"\n\
             name = \"Example App\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n\
             entry = \"example.main\"\n\
             activation = \"default-enabled\"\n",
        )
        .expect("Application Manifest");
        fs::write(app.join("src/example/main.nexa"), &oversized_frame).expect("Application source");
        fs::write(root.join("app_api.nidl"), "contract EmptyHost {}\n").expect("Host Contract");

        let project = root.join("nexa.dev.toml");
        fs::write(
            &project,
            "schema = 2\n\
             contract = \"app_api.nidl\"\n\
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
        .expect("Project configuration");

        let limits = root.join("relaxed-limits.json");
        fs::write(
            &limits,
            r#"{"max_frame_bytes":131072,"max_immediate_cost":1024,"max_wcet_states":100000}"#,
        )
        .expect("Verifier limits");

        Self {
            root,
            source,
            project,
            limits,
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

fn path(path: &Path) -> &str {
    path.to_str().expect("temporary paths are UTF-8")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn source_and_project_builds_apply_custom_limits_in_the_facade_verifier() {
    let fixture = Fixture::new();

    assert_exit(&fixture.run(&["check", path(&fixture.source)]), 1);
    assert_exit(
        &fixture.run(&[
            "check",
            path(&fixture.source),
            "--limits-file",
            path(&fixture.limits),
        ]),
        0,
    );
    assert_exit(
        &fixture.run(&[
            "run",
            "--limits-file",
            path(&fixture.limits),
            path(&fixture.source),
        ]),
        0,
    );

    let output = fixture.root.join("target");
    assert_exit(
        &fixture.run(&[
            "build",
            "--project",
            path(&fixture.project),
            "-o",
            path(&output),
        ]),
        1,
    );
    assert_exit(
        &fixture.run(&[
            "build",
            "--project",
            path(&fixture.project),
            "-o",
            path(&output),
            "--limits-file",
            path(&fixture.limits),
        ]),
        0,
    );

    let module = output.join("example.app.nxb");
    assert!(module.is_file());
    assert_exit(&fixture.run(&["qa", "verify", path(&module)]), 1);
    assert_exit(
        &fixture.run(&[
            "qa",
            "verify",
            path(&module),
            "--limits-file",
            path(&fixture.limits),
        ]),
        0,
    );
}

#[test]
fn relaxed_immediate_wcet_limits_handle_a_large_static_loop_without_aborting() {
    let fixture = Fixture::new();
    let source = fixture.root.join("immediate-static-loop.nexa");
    let limits = fixture.root.join("relaxed-immediate-limits.json");
    fs::write(
        &source,
        concat!(
            "fn main(args: Array<string>) -> i32 { return expensive(); }\n\
             @immediate\n",
            "fn expensive() -> i32 {\n\
                 for step in 0..1025 {\n\
                     continue;\n\
                 }\n\
                 return 7;\n\
             }\n"
        ),
    )
    .expect("immediate static-loop source");
    fs::write(
        &limits,
        r#"{"max_frame_bytes":65536,"max_immediate_cost":100000,"max_wcet_states":1000000}"#,
    )
    .expect("relaxed immediate verifier limits");

    let default = fixture.run(&["check", path(&source)]);
    assert_exit(&default, 1);
    let default_error = String::from_utf8_lossy(&default.stderr);
    assert!(
        default_error.contains("verify error")
            && (default_error.contains("InvalidLoopBound")
                || default_error.contains("ImmediateCostLimit")),
        "default limits must return a verifier diagnostic, stderr:\n{default_error}"
    );
    assert!(!default_error.contains("stack overflow"));
    assert!(!default_error.contains("fatal runtime error"));

    let relaxed = fixture.run(&["check", path(&source), "--limits-file", path(&limits)]);
    assert_exit(&relaxed, 0);
    let relaxed_error = String::from_utf8_lossy(&relaxed.stderr);
    assert!(!relaxed_error.contains("stack overflow"));
    assert!(!relaxed_error.contains("fatal runtime error"));
}
