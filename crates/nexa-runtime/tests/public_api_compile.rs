use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn task_lifecycle_bypasses_are_not_available_to_external_crates() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/nexa-artifacts/public-api-compile");
    if root.exists() {
        fs::remove_dir_all(&root).expect("clear public API compile artifact");
    }
    fs::create_dir_all(root.join("src")).expect("create public API compile crate");
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("runtime path");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname=\"nexa-public-api-compile\"\nversion=\"0.0.0\"\n\
             edition=\"2024\"\n[workspace]\n[dependencies]\n\
             nexa-runtime={{path=\"{}\"}}\n",
            runtime.display()
        ),
    )
    .expect("write compile-fail manifest");
    fs::write(
        root.join("src/lib.rs"),
        r"use nexa_runtime::{HostRequestHandle, RealmRuntime, TaskHandle};

pub fn forbidden(
    realm: &mut RealmRuntime,
    task: TaskHandle,
    request: HostRequestHandle,
) {
    let _ = realm.create_host_request(task);
    let _ = realm.wait_for_request(task, request);
    let _ = realm.poll_task_raw(task, 1);
    let _ = realm.call();
    let _ = realm.spawn();
}
",
    )
    .expect("write compile-fail source");
    let output = Command::new("cargo")
        .args(["+1.97.1", "check", "--offline", "--message-format=json"])
        .env("CARGO_TARGET_DIR", root.join("target"))
        .current_dir(&root)
        .output()
        .expect("run public API compile-fail");
    assert!(
        !output.status.success(),
        "bypass source unexpectedly compiled"
    );
    let diagnostics = String::from_utf8_lossy(&output.stdout);
    for method in [
        "create_host_request",
        "wait_for_request",
        "poll_task_raw",
        "call",
        "spawn",
    ] {
        assert!(
            diagnostics.contains(method),
            "missing compile failure for {method}:\n{diagnostics}"
        );
    }
    assert!(
        diagnostics.contains("E0624") && diagnostics.contains("E0599"),
        "expected private-method and missing-method diagnostics:\n{diagnostics}"
    );
    assert!(!diagnostics.contains("unresolved import"));
}
