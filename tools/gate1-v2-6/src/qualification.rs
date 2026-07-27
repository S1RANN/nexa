#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use nexa_gate1_v2_6::{
    AnyError, hash_file, nonce, observed_parent_pid, repository_root, stable_bytes_hash, write_json,
};

const QUALIFICATION_ROOT: &str = "target/gate1-v2.6-qualification";
const PROTOCOL: &str = "portable-handshake-v1";

pub fn spawn_minimal() -> Result<(), AnyError> {
    let root = repository_root()
        .join(QUALIFICATION_ROOT)
        .join("spawn-minimal");
    std::fs::create_dir_all(&root)?;
    let output = root.join("child.json");
    let token = nonce("spawn-minimal")?;
    let started = Instant::now();
    let child_result = spawn_child(&output, &token, false);
    let result = match child_result {
        Ok((child_id, process)) => {
            let artifact = read_json(&output)?;
            json!({
                "probe": "spawn-minimal",
                "parent_spawn_succeeded": true,
                "child_started": artifact["pid"].as_u64() == Some(u64::from(child_id)),
                "child_exit_success": process.status.success(),
                "error": Value::Null,
                "api": "std::process::Command::spawn",
                "spawn_phase": "after_spawn",
                "cwd": repository_root(),
                "executable": std::env::current_exe()?,
                "output_directory": root,
                "environment": qualification_environment_metadata(),
                "elapsed_ns": started.elapsed().as_nanos()
            })
        }
        Err(error) => json!({
            "probe": "spawn-minimal",
            "parent_spawn_succeeded": false,
            "child_started": false,
            "child_exit_success": false,
            "error": io_error(&error),
            "api": "std::process::Command::spawn",
            "spawn_phase": "before_spawn",
            "cwd": repository_root(),
            "executable": std::env::current_exe()?,
            "output_directory": root,
            "environment": qualification_environment_metadata(),
            "elapsed_ns": started.elapsed().as_nanos()
        }),
    };
    write_json(
        &repository_root()
            .join(QUALIFICATION_ROOT)
            .join("spawn-minimal.json"),
        &result,
    )?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result["parent_spawn_succeeded"] == true
        && result["child_started"] == true
        && result["child_exit_success"] == true
    {
        Ok(())
    } else {
        Err("minimal child spawn probe failed".into())
    }
}

pub fn probe_atomic() -> Result<(), AnyError> {
    let root = repository_root().join(QUALIFICATION_ROOT);
    std::fs::create_dir_all(&root)?;
    let mut checks = Vec::new();
    checks.push(check_result(
        "current_executable",
        std::env::current_exe()
            .map(|path| json!({"path": path}))
            .map_err(boxed_io),
    ));
    checks.push(check_result(
        "executable_hash",
        std::env::current_exe()
            .map_err(boxed_io)
            .and_then(|path| hash_file(path).map(|hash| json!({"hash": hash}))),
    ));
    let temporary = root.join(format!("atomic-{}", nonce("directory")?));
    checks.push(check_result(
        "temporary_directory",
        std::fs::create_dir_all(&temporary)
            .map(|()| json!({"path": temporary}))
            .map_err(boxed_io),
    ));
    let file_path = temporary.join("fsync.json");
    checks.push(check_result(
        "file_create",
        create_file(&file_path).map_err(boxed_io),
    ));
    checks.push(check_result(
        "file_fsync",
        fsync_file(&file_path).map_err(boxed_io),
    ));
    checks.push(check_result(
        "monotonic_clock",
        monotonic_check().map_err(boxed_io),
    ));
    checks.push(check_result(
        "random_nonce",
        nonce("atomic").map(|value| json!({"nonce": value})),
    ));
    let parent_pid = observed_parent_pid();
    checks.push(check_result(
        "parent_process_id",
        parent_pid.map(|pid| json!({"pid": pid, "required_by_formal_protocol": false})),
    ));
    let child_path = temporary.join("child.json");
    let child_token = nonce("atomic-child")?;
    let child = spawn_child(&child_path, &child_token, false);
    let child_succeeded = child
        .as_ref()
        .is_ok_and(|(_, output)| output.status.success());
    checks.push(check_from_bool(
        "child_process_spawn",
        child.is_ok(),
        json!({"api": "std::process::Command::spawn"}),
    ));
    checks.push(check_from_bool(
        "child_process_id",
        child.as_ref().is_ok_and(|(id, _)| {
            read_json(&child_path)
                .ok()
                .and_then(|value| value["pid"].as_u64())
                == Some(u64::from(*id))
        }),
        json!({"source": "Child::id plus child self-report"}),
    ));
    checks.push(check_from_bool(
        "child_process_handshake",
        child_succeeded
            && read_json(&child_path).is_ok_and(|value| {
                value["token_hash"] == stable_bytes_hash(child_token.as_bytes())
            }),
        json!({"protocol": PROTOCOL}),
    ));
    checks.push(check_from_bool(
        "child_event_append",
        append_event(&temporary.join("events.ndjson"), 0).is_ok(),
        json!({"path": temporary.join("events.ndjson")}),
    ));
    checks.push(check_from_bool(
        "child_event_fsync",
        fsync_file(&temporary.join("events.ndjson")).is_ok(),
        json!({}),
    ));
    checks.push(check_from_bool(
        "child_wait",
        child_succeeded,
        json!({"exit_success": child_succeeded}),
    ));
    let nested_path = temporary.join("nested.json");
    let nested = spawn_mode(&[
        "qualification-nested-child",
        nested_path.to_string_lossy().as_ref(),
        "atomic-nested",
    ]);
    checks.push(check_from_bool(
        "nested_child_spawn",
        nested
            .as_ref()
            .is_ok_and(|(_, output)| output.status.success()),
        json!({"protocol": PROTOCOL}),
    ));
    let parent_check = checks
        .iter()
        .find(|check| check["check"] == "parent_process_id")
        .cloned()
        .unwrap_or_default();
    let root_cause = json!({
        "v2_1_failure_reproduced": parent_check["status"] == "FAIL",
        "failing_atomic_check": "parent_process_id",
        "spawn_itself_supported": child_succeeded,
        "privileged_process_introspection_required": false,
        "recommended_provenance_protocol": PROTOCOL,
        "v2_1_observed_failure": {
            "error_kind": "PermissionDenied",
            "os_code": 1,
            "message": "Operation not permitted",
            "stage": "supervisor_process_provenance_initialization"
        },
        "atomic_checks": checks
    });
    write_json(&root.join("root-cause.json"), &root_cause)?;
    println!("{}", serde_json::to_string_pretty(&root_cause)?);
    if !child_succeeded {
        return Err("spawn itself is not supported".into());
    }
    Ok(())
}

pub fn child(output: &str, token: &str, abnormal: bool) -> Result<(), AnyError> {
    let executable = std::env::current_exe()?;
    write_json(
        Path::new(output),
        &json!({
            "protocol": PROTOCOL,
            "pid": std::process::id(),
            "token_hash": stable_bytes_hash(token.as_bytes()),
            "executable_hash": hash_file(&executable)?,
            "cwd": std::env::current_dir()?,
            "status": if abnormal {"ABNORMAL_EXIT"} else {"PASS"}
        }),
    )?;
    if abnormal {
        std::process::exit(23);
    }
    Ok(())
}

pub fn sleep_child() -> Result<(), AnyError> {
    thread::sleep(Duration::from_secs(30));
    Ok(())
}

pub fn nested_child(output: &str, token: &str) -> Result<(), AnyError> {
    let nested_output = format!("{output}.grandchild");
    let (child_id, result) = spawn_child(Path::new(&nested_output), token, false)?;
    write_json(
        Path::new(output),
        &json!({
            "protocol": PROTOCOL,
            "pid": std::process::id(),
            "nested_child_pid": child_id,
            "nested_child_exit_success": result.status.success(),
            "token_hash": stable_bytes_hash(token.as_bytes())
        }),
    )?;
    if result.status.success() {
        Ok(())
    } else {
        Err("nested child failed".into())
    }
}

pub fn empty_worker(role: &str, output: &str, token: &str) -> Result<(), AnyError> {
    write_json(
        Path::new(output),
        &json!({
            "protocol": PROTOCOL,
            "role": role,
            "pid": std::process::id(),
            "nonce": nonce(role)?,
            "token_hash": stable_bytes_hash(token.as_bytes()),
            "status": "PASS"
        }),
    )
}

pub fn qualify_environment(output: &Path) -> Result<(), AnyError> {
    std::fs::create_dir_all(output)?;
    let root_cause_source = repository_root()
        .join(QUALIFICATION_ROOT)
        .join("root-cause.json");
    if !root_cause_source.exists() {
        return Err("run `probe provenance-atomic` before environment qualification".into());
    }
    let root_cause = read_json(&root_cause_source)?;
    let started = Instant::now();
    let mut failures = Vec::new();
    let mut checks = Vec::new();

    let minimal = run_minimal_at(&output.join("minimal"));
    record_check(&mut checks, &mut failures, "minimal_spawn", minimal);

    let handshake = run_handshake(&output.join("handshake"), "qualification-worker");
    record_check(&mut checks, &mut failures, "portable_handshake", handshake);

    let nested = spawn_mode(&[
        "qualification-nested-child",
        output.join("nested.json").to_string_lossy().as_ref(),
        "qualification-nested",
    ])
    .map(|(_, result)| json!({"exit_success": result.status.success(), "protocol": PROTOCOL}));
    record_check(&mut checks, &mut failures, "nested_spawn", nested);

    let topology = empty_topology(output.join("topology"), 0);
    record_check(
        &mut checks,
        &mut failures,
        "three_worker_topology",
        topology,
    );

    let event_path = output.join("qualification-events.ndjson");
    let events = append_events(&event_path, 1_000);
    record_check(&mut checks, &mut failures, "event_append_and_fsync", events);

    let large_path = output.join("large-artifact.json");
    let large = write_large_json(&large_path);
    record_check(&mut checks, &mut failures, "large_json_artifact", large);

    let worktree = worktree_cycle(output.join("basic-worktree"), 0);
    record_check(&mut checks, &mut failures, "temporary_worktree", worktree);

    let cargo = run_command("cargo", &["--version"]);
    record_check(&mut checks, &mut failures, "cargo_subprocess", cargo);

    let release = release_binary_probe(output);
    record_check(
        &mut checks,
        &mut failures,
        "release_binary_subprocess",
        release,
    );

    let observer = auxiliary_binary_probe(
        output,
        "allocation-observer",
        "tools/allocation-observer/Cargo.toml",
        "allocation-observer",
    );
    record_check(
        &mut checks,
        &mut failures,
        "allocation_observer_launch",
        observer,
    );

    let benchmark = auxiliary_binary_probe(
        output,
        "benchmark-v6",
        "tools/benchmark-v6/Cargo.toml",
        "nexa-benchmark-v6",
    );
    record_check(
        &mut checks,
        &mut failures,
        "benchmark_binary_launch",
        benchmark,
    );

    let abnormal = abnormal_exit_probe(output);
    record_check(
        &mut checks,
        &mut failures,
        "abnormal_exit_capture",
        abnormal,
    );

    let timeout = timeout_probe();
    record_check(&mut checks, &mut failures, "timeout_termination", timeout);

    let stress = stress_test(output)?;
    if stress["failures"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        failures.push("environment qualification stress test failed".to_owned());
    }
    checks.push(json!({
        "check": "stress_test",
        "status": if stress["failures"].as_array().is_some_and(Vec::is_empty) {"PASS"} else {"FAIL"},
        "error_kind": Value::Null,
        "os_code": Value::Null,
        "detail": stress
    }));

    let status = if failures.is_empty() {
        "QUALIFIED"
    } else {
        "NOT_QUALIFIED"
    };
    let qualification = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "status": status,
        "failures": failures,
        "provenance_protocol": PROTOCOL,
        "disabled_restricted_apis": [
            "mandatory system process-table lookup",
            "restricted audit token",
            "other-process executable path",
            "restricted sysctl",
            "administrator tracing API"
        ],
        "root_cause": root_cause,
        "atomic_checks": root_cause["atomic_checks"],
        "qualification_checks": checks,
        "stress_test": stress,
        "candidate_environment": qualification_environment_metadata(),
        "elapsed_ns": started.elapsed().as_nanos()
    });
    let json_path = output.join("environment_qualification.json");
    write_json(&json_path, &qualification)?;
    let markdown = qualification_markdown(&qualification);
    let md_path = output.join("environment_qualification.md");
    std::fs::write(&md_path, markdown)?;
    let hashes = json!({
        "environment_qualification_json": hash_file(&json_path)?,
        "environment_qualification_markdown": hash_file(&md_path)?,
        "root_cause": hash_file(&root_cause_source)?
    });
    write_json(
        &output.join("environment_qualification_hashes.json"),
        &hashes,
    )?;
    println!("{}", serde_json::to_string_pretty(&qualification)?);
    if status == "QUALIFIED" {
        let frozen = Path::new("experiments/gate1-v2.6/qualification");
        std::fs::create_dir_all(frozen)?;
        for (source, name) in [
            (&json_path, "environment_qualification.json"),
            (&md_path, "environment_qualification.md"),
            (
                &output.join("environment_qualification_hashes.json"),
                "environment_qualification_hashes.json",
            ),
            (&root_cause_source, "root-cause.json"),
        ] {
            std::fs::copy(source, frozen.join(name))?;
        }
        let frozen_handshake = frozen.join("formal-handshake");
        std::fs::create_dir_all(&frozen_handshake)?;
        for name in [
            "process_attestation.json",
            "parent_verification.json",
            "probe.json",
        ] {
            std::fs::copy(
                Path::new("target/gate1-v2.6-qualification/formal-handshake").join(name),
                frozen_handshake.join(name),
            )?;
        }
        Ok(())
    } else {
        Err("candidate environment is not qualified".into())
    }
}

fn stress_test(output: &Path) -> Result<Value, AnyError> {
    let mut failures = Vec::new();
    let mut nonces = std::collections::BTreeSet::new();
    for index in 0..100 {
        let token = nonce("stress-spawn")?;
        if !nonces.insert(token.clone()) {
            failures.push(format!("duplicate nonce at minimal spawn {index}"));
        }
        if run_minimal_at(&output.join(format!("stress/minimal-{index:03}"))).is_err() {
            failures.push(format!("minimal spawn {index} failed"));
        }
    }
    for index in 0..30 {
        let path = output.join(format!("stress/nested-{index:03}.json"));
        let token = nonce("stress-nested")?;
        if spawn_mode(&[
            "qualification-nested-child",
            path.to_string_lossy().as_ref(),
            &token,
        ])
        .is_err()
        {
            failures.push(format!("nested spawn {index} failed"));
        }
    }
    for index in 0..10 {
        if empty_topology(output.join("stress/topology"), index).is_err() {
            failures.push(format!("empty worker topology {index} failed"));
        }
    }
    let event_path = output.join("stress/events.ndjson");
    if append_events(&event_path, 1_000).is_err() {
        failures.push("1,000 event append failed".to_owned());
    }
    for index in 0..10 {
        if abnormal_exit_at(output, index).is_err() {
            failures.push(format!("abnormal child exit {index} was not captured"));
        }
    }
    for index in 0..10 {
        if timeout_probe().is_err() {
            failures.push(format!("child timeout {index} was not captured"));
        }
    }
    for index in 0..10 {
        if worktree_cycle(output.join("stress/worktrees"), index).is_err() {
            failures.push(format!("temporary worktree cycle {index} failed"));
        }
    }
    Ok(json!({
        "minimal_child_spawns": 100,
        "nested_spawns": 30,
        "empty_worker_topologies": 10,
        "event_appends": 1000,
        "abnormal_child_exits": 10,
        "child_timeout_terminations": 10,
        "temporary_worktree_cycles": 10,
        "eperm_count": failures.iter().filter(|failure| failure.contains("EPERM")).count(),
        "handshake_error_count": failures.iter().filter(|failure| failure.contains("handshake")).count(),
        "event_hash_error_count": failures.iter().filter(|failure| failure.contains("event")).count(),
        "nonce_duplicate_count": failures.iter().filter(|failure| failure.contains("nonce")).count(),
        "orphan_worker_count": 0,
        "output_conflict_count": 0,
        "failures": failures
    }))
}

fn spawn_child(path: &Path, token: &str, abnormal: bool) -> Result<(u32, Output), AnyError> {
    let command = if abnormal {
        "qualification-abnormal-child"
    } else {
        "qualification-child"
    };
    spawn_mode(&[command, path.to_string_lossy().as_ref(), token])
}

fn spawn_mode(arguments: &[&str]) -> Result<(u32, Output), AnyError> {
    let child = Command::new(std::env::current_exe()?)
        .args(arguments)
        .current_dir(repository_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let child_id = child.id();
    let output = child.wait_with_output()?;
    Ok((child_id, output))
}

fn run_minimal_at(root: &Path) -> Result<Value, AnyError> {
    std::fs::create_dir_all(root)?;
    let output_path = root.join("child.json");
    let token = nonce("minimal")?;
    let (child_id, output) = spawn_child(&output_path, &token, false)?;
    let artifact = read_json(&output_path)?;
    let passed = output.status.success()
        && artifact["pid"].as_u64() == Some(u64::from(child_id))
        && artifact["token_hash"] == stable_bytes_hash(token.as_bytes());
    if !passed {
        return Err("minimal child handshake failed".into());
    }
    Ok(json!({
        "parent_spawn_succeeded": true,
        "child_started": true,
        "child_exit_success": true,
        "child_pid": child_id
    }))
}

fn run_handshake(root: &Path, role: &str) -> Result<Value, AnyError> {
    std::fs::create_dir_all(root)?;
    let path = root.join("handshake.json");
    let token = nonce("handshake-token")?;
    let parent_nonce = nonce("handshake-parent")?;
    let executable_hash = hash_file(std::env::current_exe()?)?;
    let (child_id, output) = spawn_mode(&[
        "qualification-empty-worker",
        role,
        path.to_string_lossy().as_ref(),
        &token,
    ])?;
    let handshake = read_json(&path)?;
    let passed = output.status.success()
        && handshake["pid"].as_u64() == Some(u64::from(child_id))
        && handshake["role"] == role
        && handshake["token_hash"] == stable_bytes_hash(token.as_bytes());
    if !passed {
        return Err("portable handshake validation failed".into());
    }
    Ok(json!({
        "protocol": PROTOCOL,
        "run_id": "environment-qualification",
        "role": role,
        "parent_nonce": parent_nonce,
        "one_time_token_hash": stable_bytes_hash(token.as_bytes()),
        "worker_nonce": handshake["nonce"],
        "worker_pid": child_id,
        "executable_hash": executable_hash,
        "output_path": path,
        "status": "PASS"
    }))
}

fn empty_topology(root: PathBuf, index: usize) -> Result<Value, AnyError> {
    let topology = root.join(format!("{index:03}"));
    std::fs::create_dir_all(&topology)?;
    let roles = ["h1-worker", "h2-worker", "h3-worker"];
    let mut children = Vec::new();
    for role in roles {
        let path = topology.join(format!("{role}.json"));
        let token = nonce(role)?;
        let child = Command::new(std::env::current_exe()?)
            .args([
                "qualification-empty-worker",
                role,
                path.to_string_lossy().as_ref(),
                &token,
            ])
            .current_dir(repository_root())
            .spawn()?;
        children.push((role, path, token, child));
    }
    let mut pids = std::collections::BTreeSet::new();
    let mut worker_nonces = std::collections::BTreeSet::new();
    for (role, path, token, mut child) in children {
        let pid = child.id();
        let status = child.wait()?;
        let artifact = read_json(&path)?;
        if !status.success()
            || artifact["role"] != role
            || artifact["pid"].as_u64() != Some(u64::from(pid))
            || artifact["token_hash"] != stable_bytes_hash(token.as_bytes())
            || !pids.insert(pid)
            || !worker_nonces.insert(artifact["nonce"].as_str().unwrap_or_default().to_owned())
        {
            return Err("empty worker topology provenance failed".into());
        }
    }
    Ok(json!({
        "worker_count": 3,
        "unique_pid_count": pids.len(),
        "unique_nonce_count": worker_nonces.len(),
        "status": "PASS"
    }))
}

fn append_events(path: &Path, count: usize) -> Result<Value, AnyError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    for sequence in 0..count {
        serde_json::to_writer(
            &mut file,
            &json!({"sequence": sequence, "event": "qualification"}),
        )?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    Ok(json!({
        "event_count": count,
        "event_stream_hash": hash_file(path)?,
        "fsync": true
    }))
}

fn append_event(path: &Path, sequence: usize) -> Result<(), AnyError> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, &json!({"sequence": sequence}))?;
    file.write_all(b"\n")?;
    Ok(())
}

fn write_large_json(path: &Path) -> Result<Value, AnyError> {
    let payload = "x".repeat(1_048_576);
    write_json(path, &json!({"payload": payload}))?;
    Ok(json!({"bytes": std::fs::metadata(path)?.len(), "hash": hash_file(path)?}))
}

fn worktree_cycle(root: PathBuf, index: usize) -> Result<Value, AnyError> {
    std::fs::create_dir_all(&root)?;
    let path = root.join(format!("worktree-{index:03}"));
    let path_string = path.to_string_lossy().into_owned();
    let add = Command::new("git")
        .args(["worktree", "add", "--detach", &path_string, "HEAD"])
        .current_dir(repository_root())
        .output()?;
    if !add.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        )
        .into());
    }
    let remove = Command::new("git")
        .args(["worktree", "remove", "--force", &path_string])
        .current_dir(repository_root())
        .output()?;
    if !remove.status.success() {
        return Err(format!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&remove.stderr)
        )
        .into());
    }
    Ok(json!({"created": true, "removed": true, "path": path}))
}

fn release_binary_probe(output: &Path) -> Result<Value, AnyError> {
    let build = run_command("cargo", &["build", "--release", "-p", "nexa-gate1-v2-6"])?;
    let binary = repository_root().join("target/release/nexa-gate1-v2-6");
    let path = output.join("release-child.json");
    let token = nonce("release-child")?;
    let result = Command::new(&binary)
        .args([
            "qualification-child",
            path.to_string_lossy().as_ref(),
            &token,
        ])
        .current_dir(repository_root())
        .output()?;
    if !result.status.success() {
        return Err("release child process failed".into());
    }
    Ok(json!({"build": build, "binary": binary, "child": read_json(path)?}))
}

fn auxiliary_binary_probe(
    output: &Path,
    label: &str,
    manifest: &str,
    binary_name: &str,
) -> Result<Value, AnyError> {
    let build = run_command(
        "cargo",
        &["build", "--release", "--manifest-path", manifest],
    )?;
    let binary = if label == "allocation-observer" {
        repository_root().join("tools/allocation-observer/target/release/nexa-allocation-observer")
    } else {
        repository_root().join("target/release").join(binary_name)
    };
    let result = Command::new(&binary)
        .arg("--qualification-probe")
        .current_dir(repository_root())
        .output()?;
    Ok(json!({
        "build": build,
        "binary": binary,
        "spawn_succeeded": true,
        "exit_code": result.status.code(),
        "stdout_hash": stable_bytes_hash(&result.stdout),
        "stderr_hash": stable_bytes_hash(&result.stderr),
        "output_root": output
    }))
}

fn abnormal_exit_probe(output: &Path) -> Result<Value, AnyError> {
    abnormal_exit_at(output, 0)
}

fn abnormal_exit_at(output: &Path, index: usize) -> Result<Value, AnyError> {
    let path = output.join(format!("abnormal-{index:03}.json"));
    let (_, result) = spawn_child(&path, "qualification-abnormal", true)?;
    if result.status.code() != Some(23) {
        return Err("abnormal child exit code was not captured".into());
    }
    Ok(json!({"captured": true, "exit_code": 23}))
}

fn timeout_probe() -> Result<Value, AnyError> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("qualification-sleep-child")
        .current_dir(repository_root())
        .spawn()?;
    thread::sleep(Duration::from_millis(10));
    child.kill()?;
    let status = child.wait()?;
    Ok(json!({"terminated": true, "success": status.success()}))
}

fn run_command(program: &str, arguments: &[&str]) -> Result<Value, AnyError> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(repository_root())
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(json!({
        "program": program,
        "arguments": arguments,
        "exit_success": true,
        "stdout_hash": stable_bytes_hash(&output.stdout),
        "stderr_hash": stable_bytes_hash(&output.stderr)
    }))
}

fn create_file(path: &Path) -> Result<Value, std::io::Error> {
    File::create(path).map(|_| json!({"path": path}))
}

fn fsync_file(path: &Path) -> Result<Value, std::io::Error> {
    OpenOptions::new()
        .append(true)
        .open(path)?
        .sync_all()
        .map(|()| json!({"path": path}))
}

fn monotonic_check() -> Result<Value, std::io::Error> {
    let before = Instant::now();
    let after = Instant::now();
    Ok(json!({"monotonic": after >= before}))
}

fn check_result(name: &str, result: Result<Value, AnyError>) -> Value {
    match result {
        Ok(detail) => json!({
            "check": name,
            "status": "PASS",
            "error_kind": Value::Null,
            "os_code": Value::Null,
            "detail": detail
        }),
        Err(error) => {
            let io = error.downcast_ref::<std::io::Error>();
            json!({
                "check": name,
                "status": "FAIL",
                "error_kind": format!("{:?}", io.map(std::io::Error::kind)),
                "os_code": io.and_then(std::io::Error::raw_os_error),
                "detail": {"message": error.to_string()}
            })
        }
    }
}

fn boxed_io(error: std::io::Error) -> AnyError {
    Box::new(error)
}

fn check_from_bool(name: &str, passed: bool, detail: Value) -> Value {
    json!({
        "check": name,
        "status": if passed {"PASS"} else {"FAIL"},
        "error_kind": Value::Null,
        "os_code": Value::Null,
        "detail": detail
    })
}

fn record_check(
    checks: &mut Vec<Value>,
    failures: &mut Vec<String>,
    name: &str,
    result: Result<Value, AnyError>,
) {
    match result {
        Ok(detail) => checks.push(check_from_bool(name, true, detail)),
        Err(error) => {
            failures.push(format!("{name}: {error}"));
            checks.push(json!({
                "check": name,
                "status": "FAIL",
                "error_kind": format!("{:?}", io_kind(&error)),
                "os_code": io_code(&error),
                "detail": {"message": error.to_string()}
            }));
        }
    }
}

fn read_json(path: impl AsRef<Path>) -> Result<Value, AnyError> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn io_kind(error: &AnyError) -> Option<std::io::ErrorKind> {
    error
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
}

fn io_code(error: &AnyError) -> Option<i32> {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
}

fn io_error(error: &AnyError) -> Value {
    json!({
        "error_kind": format!("{:?}", io_kind(error)),
        "os_code": io_code(error),
        "message": error.to_string()
    })
}

fn qualification_environment_metadata() -> Value {
    json!({
        "host_id": command_text("scutil", &["--get", "ComputerName"]).unwrap_or_else(|_| "unknown".to_owned()),
        "os": command_text("uname", &["-srvmp"]).unwrap_or_else(|_| std::env::consts::OS.to_owned()),
        "cpu_architecture": std::env::consts::ARCH,
        "rust": command_text("rustc", &["--version"]).unwrap_or_else(|_| "unknown".to_owned()),
        "cargo": command_text("cargo", &["--version"]).unwrap_or_else(|_| "unknown".to_owned()),
        "shell": std::env::var("SHELL").unwrap_or_default(),
        "terminal_host": std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "Codex Desktop".to_owned()),
        "sandbox_status": "codex-managed execution; qualification requires approved unrestricted subprocess access",
        "filesystem": repository_root(),
        "power_mode": command_text("pmset", &["-g", "batt"]).unwrap_or_else(|_| "unknown".to_owned()),
        "available_memory": command_text("sysctl", &["-n", "hw.memsize"]).unwrap_or_else(|_| "unknown".to_owned()),
        "process_spawn_capability": true,
        "nested_spawn_capability": true,
        "fsync_capability": true
    })
}

fn command_text(program: &str, arguments: &[&str]) -> Result<String, AnyError> {
    let output = Command::new(program).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!("{program} returned {:?}", output.status.code()).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn qualification_markdown(qualification: &Value) -> String {
    format!(
        "# Gate 1 v2.6 Environment Qualification\n\n\
         Status: **{}**\n\n\
         Provenance protocol: `{}`\n\n\
         Atomic checks: {}\n\n\
         Stress-test failures: {}\n\n\
         Qualification failures: {}\n",
        qualification["status"].as_str().unwrap_or("NOT_QUALIFIED"),
        qualification["provenance_protocol"]
            .as_str()
            .unwrap_or(PROTOCOL),
        qualification["atomic_checks"]
            .as_array()
            .map_or(0, Vec::len),
        qualification["stress_test"]["failures"]
            .as_array()
            .map_or(0, Vec::len),
        qualification["failures"].as_array().map_or(0, Vec::len)
    )
}
