use std::fs;
use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nexa-m4r1-repl-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("REPL fixture directory");
        Self { root }
    }

    fn repl(&self, options: &[&str], input: &str) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nexa"))
            .arg("repl")
            .arg("--no-prompt")
            .args(options)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("REPL starts");
        child
            .stdin
            .take()
            .expect("REPL stdin")
            .write_all(input.as_bytes())
            .expect("REPL batch input");
        child.wait_with_output().expect("REPL exits")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("REPL output is UTF-8")
}

fn output_lines(output: &Output) -> Vec<String> {
    text(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn has_output_line(output: &Output, expected: &str) -> bool {
    output_lines(output).iter().any(|line| line == expected)
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
fn repl_persists_bindings_mutation_shadowing_functions_and_async_cells() {
    let fixture = Fixture::new();
    let output = fixture.repl(
        &[],
        r"1 + 2
let value = 4;
value
let mut count = 1;
count = 2;
count
let value = 9;
value
fn double(input: i32) -> i32 { return input * 2; }
double(value)
async fn async_value() -> i32 { return 23; }
async_value().await
:quit
",
    );
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    for expected in ["3", "4", "2", "9", "18", "23"] {
        assert!(
            has_output_line(&output, expected),
            "missing `{expected}` in stdout:\n{}",
            text(&output.stdout)
        );
    }
}

#[test]
fn repl_inspection_and_maintenance_commands_use_the_compiled_session() {
    let fixture = Fixture::new();
    let output = fixture.repl(
        &[],
        r"fn double(input: i32) -> i32 { return input * 2; }
:type double(6)
:ast let inspected = 1;
:bytecode double
:gc
:memory
:help
:quit
",
    );
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.lines().any(|line| line.trim() == "i32"), "{stdout}");
    assert!(stdout.contains("inspected"), "{stdout}");
    assert!(stdout.contains("double"), "{stdout}");
    assert!(stdout.contains("gc:"), "{stdout}");
    assert!(stdout.contains("memory:"), "{stdout}");
    for command in [
        ":type",
        ":ast",
        ":bytecode",
        ":gc",
        ":memory",
        ":load",
        ":reset",
        ":help",
        ":quit",
    ] {
        assert!(
            stdout.contains(command),
            "help omitted `{command}`:\n{stdout}"
        );
    }
}

#[test]
fn failed_cells_do_not_commit_and_reset_discards_the_old_environment() {
    let fixture = Fixture::new();
    let output = fixture.repl(
        &[],
        r"let keep = 7;
let failed = missing_name;
keep
failed
keep
struct Point { x: i32, }
struct Point { x: i32, }
40 + 2
:reset
keep
2 + 3
:quit
",
    );
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    assert!(
        output_lines(&output)
            .iter()
            .filter(|line| *line == "7")
            .count()
            >= 2,
        "{stdout}"
    );
    assert!(has_output_line(&output, "42"), "{stdout}");
    assert!(has_output_line(&output, "5"), "{stdout}");
    assert!(stderr.contains("error:"), "{stderr}");
    assert!(stderr.contains("missing_name"), "{stderr}");
    assert!(stderr.contains("failed"), "{stderr}");
    assert!(stderr.contains("Point"), "{stderr}");
    assert!(stderr.contains("keep"), "{stderr}");
}

#[test]
fn repl_load_compiles_file_cells_into_the_current_session() {
    let fixture = Fixture::new();
    let loaded = fixture.root.join("loaded.nexa");
    fs::write(
        &loaded,
        "fn loaded_value() -> i32 { return 11; }\nlet loaded_binding = 13;\n",
    )
    .expect("loaded REPL source");
    let output = fixture.repl(
        &[],
        &format!(
            ":load {}\nloaded_value()\nloaded_binding\n:quit\n",
            path(&loaded)
        ),
    );
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "{}", text(&output.stderr));
    assert!(has_output_line(&output, "11"), "{}", text(&output.stdout));
    assert!(has_output_line(&output, "13"), "{}", text(&output.stdout));
}

#[test]
fn repl_resource_failures_recover_and_limit_flags_are_validated() {
    let fixture = Fixture::new();
    let output = fixture.repl(
        &["--fuel", "256"],
        "for step in 0..1000 { step + 1; }\n2 + 2\n:quit\n",
    );
    assert_exit(&output, 0);
    assert!(
        text(&output.stderr).contains("error:"),
        "{}",
        text(&output.stderr)
    );
    assert!(has_output_line(&output, "4"), "{}", text(&output.stdout));

    for option in [
        "--heap-objects",
        "--fuel",
        "--max-cells",
        "--diagnostic-history",
        "--max-output-bytes",
    ] {
        let invalid = fixture.repl(&[option, "0"], "");
        assert_exit(&invalid, 2);
        assert!(
            text(&invalid.stderr).contains(option)
                || text(&invalid.stderr).contains("greater than zero"),
            "{}",
            text(&invalid.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_only_the_current_cell_and_keeps_the_session_alive() {
    let fixture = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nexa"))
        .args(["repl", "--no-prompt", "--fuel", "1000000000"])
        .current_dir(&fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("REPL starts");
    let mut stdin = child.stdin.take().expect("REPL stdin");
    let stdout = child.stdout.take().expect("REPL stdout");
    let (line_sender, line_receiver) = std::sync::mpsc::channel();
    let stdout_reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut captured = String::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => return captured,
                Ok(_) => {
                    captured.push_str(&line);
                    let _ = line_sender.send(line);
                }
                Err(error) => panic!("read REPL stdout: {error}"),
            }
        }
    });
    stdin
        .write_all(b"async fn spin() { while true { yield; } }\n1\n")
        .expect("REPL readiness Cells");
    stdin.flush().expect("flush REPL readiness Cells");
    loop {
        let line = line_receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("REPL becomes ready");
        if line.trim() == "1" {
            break;
        }
    }
    stdin
        .write_all(b"spin().await\n")
        .expect("long-running REPL Cell");
    stdin.flush().expect("flush long-running REPL Cell");
    std::thread::sleep(Duration::from_millis(50));
    let signal = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status()
        .expect("send SIGINT to REPL");
    assert!(signal.success(), "SIGINT command failed");
    std::thread::sleep(Duration::from_millis(50));
    stdin
        .write_all(b"6 * 7\n:quit\n")
        .expect("post-cancellation REPL Cells");
    drop(stdin);
    let output = child.wait_with_output().expect("REPL exits after :quit");
    let stdout = stdout_reader.join().expect("REPL stdout reader");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{}",
        text(&output.stderr)
    );
    assert!(
        text(&output.stderr).contains("cancelled"),
        "{}",
        text(&output.stderr)
    );
    assert!(
        stdout.lines().any(|line| line.trim() == "42"),
        "stdout:\n{stdout}\nstderr:\n{}",
        text(&output.stderr)
    );
}

fn path(path: &Path) -> &str {
    path.to_str().expect("temporary paths are UTF-8")
}
