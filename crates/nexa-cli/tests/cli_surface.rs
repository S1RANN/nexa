use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
    script: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-cli-surface-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("CLI surface fixture directory");
        let script = root.join("main.nexa");
        fs::write(
            &script,
            "fn main(args: Array<string>) -> i32 { return args.len(); }\n",
        )
        .expect("CLI surface script");
        Self { root, script }
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
fn naked_nexa_runs_a_non_interactive_repl_for_piped_stdin() {
    let fixture = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nexa"))
        .current_dir(&fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("naked CLI starts");
    child
        .stdin
        .take()
        .expect("REPL stdin")
        .write_all(b"1 + 2\n:quit\n")
        .expect("REPL input");
    let output = child.wait_with_output().expect("REPL exits");
    assert_exit(&output, 0);
    assert!(text(&output.stdout).lines().any(|line| line.trim() == "3"));
    assert!(!text(&output.stdout).contains("nexa>"));
}

#[test]
fn naked_script_and_run_have_the_same_argument_semantics() {
    let fixture = Fixture::new();
    let naked = fixture.run(&[path(&fixture.script), "argument"]);
    let explicit = fixture.run(&["run", path(&fixture.script), "argument"]);
    assert_exit(&naked, 1);
    assert_exit(&explicit, 1);
    assert_eq!(naked.stdout, explicit.stdout);
    assert_eq!(naked.stderr, explicit.stderr);
}

#[test]
fn options_after_the_script_path_are_program_arguments() {
    let fixture = Fixture::new();
    assert_exit(
        &fixture.run(&["--fuel", "100000", path(&fixture.script), "--fuel", "1"]),
        2,
    );
    assert_exit(
        &fixture.run(&[
            "run",
            "--fuel",
            "100000",
            path(&fixture.script),
            "--fuel",
            "1",
        ]),
        2,
    );
}

#[test]
fn public_help_hides_qa_and_reports_command_typos() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"]);
    assert_exit(&help, 0);
    let help = text(&help.stdout);
    for command in [
        "check", "build", "run", "repl", "exec", "migrate", "contract",
    ] {
        assert!(help.contains(command), "help omitted `{command}`:\n{help}");
    }
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("qa "))
    );

    let typo = fixture.run(&["rpel"]);
    assert_exit(&typo, 2);
    let stderr = text(&typo.stderr);
    assert!(stderr.contains("similar subcommand"), "{stderr}");
    assert!(stderr.contains("repl"), "{stderr}");
}

#[test]
fn usage_errors_keep_the_json_machine_envelope() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--diagnostic-format", "json", "exec"]);
    assert_exit(&output, 2);
    let envelope: Value = serde_json::from_slice(&output.stderr).expect("usage JSON envelope");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["exitCode"], 2);
    assert!(
        envelope["message"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
}

#[test]
fn clap_enforces_conflicts_dependencies_and_removed_commands() {
    let fixture = Fixture::new();
    assert_exit(
        &fixture.run(&["build", "--project", "nexa.dev.toml", "main.nexa"]),
        2,
    );
    assert_exit(
        &fixture.run(&["exec", "module.nxb", "--trace-output", "trace.json"]),
        2,
    );
    for arguments in [
        &["trace", "module.nxb"][..],
        &["migrate-check"][..],
        &["verify", "module.nxb"][..],
    ] {
        assert_exit(&fixture.run(arguments), 2);
    }
}
