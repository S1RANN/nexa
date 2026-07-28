use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nexa_core::StableId;
use nexa_runtime::{
    HostCallOutcome, HostRegistry, HostTrap, RealmConfig, RealmError, RealmRuntime,
    ResourceContext, RuntimeHost, RuntimeHostArgs, RuntimeValue, StepConfig, TaskLimits, TaskPoll,
};
use serde_json::Value;

pub const BASE_NIDL: &str = include_str!("fixtures/business_host/interface.nidl");
pub const BUSINESS_HOST_V1: &str = include_str!("fixtures/business_host/business_host.rs");

pub struct MutationCase {
    pub id: &'static str,
    pub name: &'static str,
    pub mutated_nidl: String,
    pub unchanged_business_host_should_compile: bool,
    pub expected_diagnostic_symbols: &'static [&'static str],
    pub patch_business_host: fn(&str) -> String,
    pub expected_changed_interface_hash: bool,
}

pub struct MutationEvidence {
    pub id: &'static str,
    pub name: &'static str,
    pub base_interface_hash: StableId,
    pub changed_interface_hash: StableId,
    pub base_generated_hash: u64,
    pub changed_generated_hash: u64,
    pub unchanged_business_host_should_compile: bool,
    pub patch_insertions: usize,
    pub patch_deletions: usize,
    pub old_bytecode_rejected: bool,
    pub changed_heartbeat_executed: bool,
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mutations() -> Vec<MutationCase> {
    vec![
        changed(
            "01",
            "function-rename",
            "fn update(",
            "fn advance(",
            false,
            &["update", "advance"],
            patch_function_rename,
        ),
        changed(
            "02",
            "parameter-type-change",
            "update(entity: i32,",
            "update(entity: i64,",
            false,
            &["update", "i64"],
            patch_parameter_type,
        ),
        changed(
            "03",
            "return-type-change",
            "update(entity: i32, delta: i32) -> i32",
            "update(entity: i32, delta: i32) -> i64",
            false,
            &["update", "i64"],
            patch_return_type,
        ),
        changed(
            "04",
            "sync-to-request",
            "sync fuel 5 fn update(entity: i32, delta: i32) -> i32;",
            "request(return_error, trap) fuel 5 fn update(entity: i32, delta: i32)\n        -> request<Result<i32, AnimationError>>;",
            false,
            &["update", "HostRequestHandle"],
            patch_sync_to_request,
        ),
        changed(
            "05",
            "fuel-cost-change",
            "fuel 5",
            "fuel 7",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "06",
            "cancel-policy-change",
            "request(return_error, trap) fn animation",
            "request(cancel_task, trap) fn animation",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "07",
            "abandon-policy-change",
            "request(return_error, trap) fn animation",
            "request(return_error, return_error) fn animation",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "08",
            "enum-variant-rename",
            "MissingClip",
            "MissingAsset",
            false,
            &["MissingClip", "AnimationError"],
            patch_variant_rename,
        ),
        changed(
            "09",
            "struct-field-rename",
            "EnemyView { health:",
            "EnemyView { hit_points:",
            false,
            &["health", "EnemyView"],
            patch_field_rename,
        ),
        changed(
            "10",
            "snapshot-content-type-change",
            "snapshot<EnemyView>",
            "snapshot<EnemyState>",
            false,
            &["EnemyViewSnapshot", "EnemyStateSnapshot"],
            patch_snapshot_content,
        ),
        changed(
            "11",
            "resource-token-domain-change",
            "token<ActionLock>",
            "token<MotionLock>",
            false,
            &["ActionLockToken", "MotionLockToken"],
            patch_token_domain,
        ),
        changed(
            "12",
            "interface-rename",
            "interface GameHost",
            "interface CombatHost",
            false,
            &["GameHost", "CombatHost"],
            patch_interface_rename,
        ),
        changed(
            "13",
            "opaque-handle-rename",
            "opaque Entity;",
            "opaque Actor;",
            false,
            &["Entity", "Actor"],
            patch_opaque_rename,
        )
        .with_replacement("inspect(entity: Entity)", "inspect(entity: Actor)"),
        changed(
            "14",
            "struct-rename",
            "struct EnemyView",
            "struct EnemyStats",
            false,
            &["EnemyView", "EnemyStats"],
            patch_struct_rename,
        )
        .with_replacement("snapshot<EnemyView>", "snapshot<EnemyStats>")
        .with_replacement("score(view: EnemyView)", "score(view: EnemyStats)"),
        changed(
            "15",
            "enum-rename",
            "enum AnimationError",
            "enum PlaybackError",
            false,
            &["AnimationError", "PlaybackError"],
            patch_enum_rename,
        )
        .with_replacement("Result<i32, AnimationError>", "Result<i32, PlaybackError>")
        .with_replacement(
            "classify(error: AnimationError)",
            "classify(error: PlaybackError)",
        ),
        changed(
            "16",
            "enum-payload-type-change",
            "Code(i32)",
            "Code(i64)",
            false,
            &["Code", "i64"],
            patch_payload_type,
        ),
        changed(
            "17",
            "parameter-rename",
            "delta: i32",
            "step: i32",
            true,
            &[],
            identity_patch,
        ),
        changed(
            "18",
            "parameter-addition",
            "update(entity: i32, delta: i32)",
            "update(entity: i32, delta: i32, flags: i32)",
            false,
            &["update", "flags"],
            patch_parameter_addition,
        ),
        changed(
            "19",
            "struct-field-addition",
            "EnemyView { health: i32; }",
            "EnemyView { health: i32; armor: i32; }",
            false,
            &["EnemyView", "armor"],
            patch_struct_field_addition,
        ),
        changed(
            "20",
            "export-addition",
            "export Update(entity: i32) -> i32;",
            "export Update(entity: i32) -> i32;\n    export Reset() -> i32;",
            true,
            &[],
            identity_patch,
        ),
    ]
}

impl MutationCase {
    fn with_replacement(mut self, from: &str, to: &str) -> Self {
        assert!(
            self.mutated_nidl.contains(from),
            "missing mutation token {from}"
        );
        self.mutated_nidl = self.mutated_nidl.replacen(from, to, 1);
        self
    }
}

fn changed(
    id: &'static str,
    name: &'static str,
    from: &str,
    to: &str,
    unchanged_business_host_should_compile: bool,
    expected_diagnostic_symbols: &'static [&'static str],
    patch_business_host: fn(&str) -> String,
) -> MutationCase {
    assert!(BASE_NIDL.contains(from), "missing mutation token {from}");
    MutationCase {
        id,
        name,
        mutated_nidl: BASE_NIDL.replacen(from, to, 1),
        unchanged_business_host_should_compile,
        expected_diagnostic_symbols,
        patch_business_host,
        expected_changed_interface_hash: true,
    }
}

fn identity_patch(source: &str) -> String {
    source.to_owned()
}

fn patch_function_rename(source: &str) -> String {
    source.replacen("    fn update(", "    fn advance(", 1)
}

fn patch_parameter_type(source: &str) -> String {
    source
        .replacen(
            "        entity: i32,\n        delta: i32,",
            "        entity: i64,\n        delta: i32,",
            1,
        )
        .replacen(
            "Ok(entity + delta)",
            "Ok((entity + i64::from(delta)) as i32)",
            1,
        )
}

fn patch_return_type(source: &str) -> String {
    source.replacen(
        "    ) -> Result<i32, HostError> {\n        Ok(entity + delta)",
        "    ) -> Result<i64, HostError> {\n        Ok(i64::from(entity + delta))",
        1,
    )
}

fn patch_sync_to_request(source: &str) -> String {
    replace_block(
        source,
        "    fn update(",
        "    fn animation(",
        r"    fn update(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
        _entity: i32,
        _delta: i32,
    ) -> Result<nexa_runtime::HostRequestHandle, HostError> {
        context
            .create_request()
            .map(|pending| pending.request)
            .map_err(|error| HostError(error.to_string()))
    }

",
    )
}

fn patch_variant_rename(source: &str) -> String {
    source.replacen(
        "AnimationErrorRef::MissingClip",
        "AnimationErrorRef::MissingAsset",
        1,
    )
}

fn patch_field_rename(source: &str) -> String {
    source
        .replace("EnemyView { health: 40 }", "EnemyView { hit_points: 40 }")
        .replace("view.health()", "view.hit_points()")
}

fn patch_snapshot_content(source: &str) -> String {
    replace_block(
        source,
        "    fn view(",
        "    fn inspect(",
        r#"    fn view(
        &mut self,
        context: &mut nexa_runtime::ResourceContext<'_>,
    ) -> Result<EnemyStateSnapshot, HostError> {
        let encoded = EnemyStateSnapshotEncoder::encode(&EnemyState { health: 40 })?;
        let handle = context
            .create_typed_snapshot(encoded)
            .map_err(|error| HostError(error.to_string()))?;
        EnemyStateSnapshot::try_from_raw(handle)
            .map_err(|error| HostError(format!("{error:?}")))
    }

"#,
    )
}

fn patch_token_domain(source: &str) -> String {
    source
        .replacen(
            "Result<ActionLockToken, HostError>",
            "Result<MotionLockToken, HostError>",
            1,
        )
        .replacen(
            ".map(ActionLockToken::from_raw)",
            ".map(MotionLockToken::from_raw)",
            1,
        )
}

fn patch_interface_rename(source: &str) -> String {
    source.replacen(
        "impl GameHost for BusinessHostV1",
        "impl CombatHost for BusinessHostV1",
        1,
    )
}

fn patch_opaque_rename(source: &str) -> String {
    source.replacen("        entity: Entity,", "        entity: Actor,", 1)
}

fn patch_struct_rename(source: &str) -> String {
    source.replace("EnemyView", "EnemyStats")
}

fn patch_enum_rename(source: &str) -> String {
    source.replace("AnimationErrorRef", "PlaybackErrorRef")
}

fn patch_payload_type(source: &str) -> String {
    source.replacen(
        "AnimationErrorRef::Code(code) => code,",
        "AnimationErrorRef::Code(code) => i32::try_from(code).unwrap_or_default(),",
        1,
    )
}

fn patch_parameter_addition(source: &str) -> String {
    source
        .replacen(
            "        delta: i32,\n    )",
            "        delta: i32,\n        flags: i32,\n    )",
            1,
        )
        .replacen("Ok(entity + delta)", "Ok(entity + delta + flags)", 1)
}

fn patch_struct_field_addition(source: &str) -> String {
    source.replacen(
        "EnemyView { health: 40 }",
        "EnemyView { health: 40, armor: 2 }",
        1,
    )
}

fn replace_block(source: &str, start: &str, end: &str, replacement: &str) -> String {
    let start_index = source.find(start).expect("business Host block start");
    let end_index = source[start_index..]
        .find(end)
        .map(|offset| start_index + offset)
        .expect("business Host block end");
    format!(
        "{}{}{}",
        &source[..start_index],
        replacement,
        &source[end_index..]
    )
}

#[must_use]
pub fn artifact_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nexa-artifacts/idl-e2e")
}

#[must_use]
pub fn prepare_case(
    root: &Path,
    mutation: &MutationCase,
    base_generated: &str,
    changed_generated: &str,
) -> PathBuf {
    let case = root.join(format!("{}-{}", mutation.id, mutation.name));
    for directory in ["base", "mutated", "host/src", "script"] {
        fs::create_dir_all(case.join(directory)).expect("create E2E artifact directory");
    }
    fs::write(case.join("base/interface.nidl"), BASE_NIDL).expect("write base NIDL");
    fs::write(case.join("base/bindings.rs"), base_generated).expect("write base binding");
    fs::write(case.join("mutated/interface.nidl"), &mutation.mutated_nidl)
        .expect("write changed NIDL");
    for (index, generated) in [changed_generated, changed_generated, changed_generated]
        .iter()
        .enumerate()
    {
        fs::write(
            case.join(format!("mutated/binding-{}.rs", index + 1)),
            generated,
        )
        .expect("write deterministic binding");
    }
    fs::write(
        case.join("script/module.nexa"),
        "module idl_e2e;\nimport engine;\ntask fn update(entity: i32) -> i32 { return engine.heartbeat(entity); }\nfn reset() -> i32 { return 0; }\n",
    )
    .expect("write positive script");
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../nexa-runtime")
        .canonicalize()
        .expect("runtime path");
    fs::write(
        case.join("host/Cargo.toml"),
        format!(
            "[package]\nname=\"idl-e2e-{}\"\nversion=\"0.0.0\"\nedition=\"2024\"\n\
             [workspace]\n[dependencies]\nnexa-runtime={{path=\"{}\"}}\n",
            mutation.id,
            runtime.display()
        ),
    )
    .expect("write host Cargo manifest");
    fs::write(
        case.join("host/src/lib.rs"),
        "include!(\"bindings.rs\");\ninclude!(\"business_host.rs\");\n",
    )
    .expect("write host crate root");
    case
}

#[must_use]
pub fn check_business_host(
    case: &Path,
    changed_generated: &str,
    business_host: &str,
    shared_target: &Path,
) -> Output {
    fs::write(case.join("host/src/bindings.rs"), changed_generated).expect("write bindings");
    fs::write(case.join("host/src/business_host.rs"), business_host).expect("write business Host");
    Command::new("cargo")
        .args(["+1.97.1", "check", "--offline", "--message-format=json"])
        .env("CARGO_TARGET_DIR", shared_target)
        .current_dir(case.join("host"))
        .output()
        .expect("run business Host cargo check")
}

pub fn assert_expected_business_diagnostic(mutation: &MutationCase, output: &Output) {
    let mut business_diagnostic = false;
    let mut symbol_diagnostic = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-message" {
            continue;
        }
        let message = &value["message"];
        let in_business_host = message["spans"].as_array().is_some_and(|spans| {
            spans.iter().any(|span| {
                span["file_name"]
                    .as_str()
                    .is_some_and(|file| file.ends_with("business_host.rs"))
            })
        });
        if !in_business_host {
            continue;
        }
        business_diagnostic = true;
        let diagnostic = format!(
            "{}\n{}",
            message["message"].as_str().unwrap_or_default(),
            message["rendered"].as_str().unwrap_or_default()
        );
        symbol_diagnostic |= mutation
            .expected_diagnostic_symbols
            .iter()
            .any(|symbol| diagnostic.contains(symbol));
    }
    assert!(
        business_diagnostic,
        "{} must fail in business_host.rs, stdout:\n{}",
        mutation.name,
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        symbol_diagnostic,
        "{} diagnostic must name one of {:?}",
        mutation.name, mutation.expected_diagnostic_symbols
    );
}

#[must_use]
pub fn patch_delta(before: &str, after: &str) -> (usize, usize) {
    let changed = before
        .lines()
        .zip(after.lines())
        .filter(|(left, right)| left != right)
        .count();
    (
        changed + after.lines().count().saturating_sub(before.lines().count()),
        changed + before.lines().count().saturating_sub(after.lines().count()),
    )
}

pub fn assert_pre_interpreter_rejection(base_idl: &nexa_idl::Idl, changed_hash: StableId) {
    let schema_hash = StableId::from_name("idl-e2e-schema");
    let old_module = nexa_compiler::compile_with_interface(
        "module idl_e2e;\nimport engine;\ntask fn update(entity: i32) -> i32 { return engine.heartbeat(entity); }\nfn reset() -> i32 { return 0; }",
        base_idl,
        schema_hash,
    )
    .expect("compile base module");
    let runtime_host = RuntimeHost::new(8);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(HeartbeatRegistry(changed_hash)),
    )
    .expect("hosted realm");
    let before = realm.inspection_snapshot();
    assert_eq!(
        realm.load_module(old_module, changed_hash, schema_hash),
        Err(RealmError::HostHashMismatch)
    );
    let after = realm.inspection_snapshot();
    assert_eq!(after.active_root, before.active_root);
    assert_eq!(after.modules.len(), before.modules.len());
    assert!(after.tasks.is_empty());
    assert!(after.terminal_tasks.is_empty());
    drop(realm);
    let _ = runtime_host.begin_close();
    runtime_host
        .try_finish_close()
        .expect("close rejection Host");
}

pub fn assert_changed_heartbeat_executes(changed_idl: &nexa_idl::Idl, changed_hash: StableId) {
    let schema_hash = StableId::from_name("idl-e2e-schema");
    let module = nexa_compiler::compile_with_interface(
        "module idl_e2e;\nimport engine;\ntask fn update(entity: i32) -> i32 { return engine.heartbeat(entity); }\nfn reset() -> i32 { return 0; }",
        changed_idl,
        schema_hash,
    )
    .expect("compile changed heartbeat module");
    let runtime_host = RuntimeHost::new(8);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(HeartbeatRegistry(changed_hash)),
    )
    .expect("hosted realm");
    let module = realm
        .load_module(module, changed_hash, schema_hash)
        .expect("changed module loads");
    let scope = realm.create_scope(None).expect("create heartbeat scope");
    let task = realm
        .spawn_task(
            module,
            0,
            &[RuntimeValue::I32(41)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 1_024,
                limits: TaskLimits::default(),
            },
        )
        .expect("spawn heartbeat");
    assert_eq!(
        realm.poll_task(task, 64).expect("poll heartbeat"),
        TaskPoll::Completed(RuntimeValue::I32(42))
    );
    realm.cancel_scope(scope).expect("close heartbeat scope");
    realm
        .destroy_empty_scope(scope)
        .expect("destroy heartbeat scope");
    drop(realm);
    let _ = runtime_host.drain_releases();
    let _ = runtime_host.begin_close();
    runtime_host
        .try_finish_close()
        .expect("close heartbeat Host");
}

struct HeartbeatRegistry(StableId);

impl HostRegistry for HeartbeatRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 7 || args.len() != 1 {
            return Err(HostTrap::UnknownFunction(id));
        }
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(
            args.i32(0)? + 1,
        )))
    }
}

#[must_use]
pub fn stable_bytes_hash(bytes: &str) -> u64 {
    bytes.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub fn write_report(root: &Path, evidence: &[MutationEvidence]) {
    let mut interface_hashes = BTreeSet::new();
    let mut rows = String::new();
    for (index, item) in evidence.iter().enumerate() {
        assert!(
            interface_hashes.insert(item.changed_interface_hash),
            "mutations must have distinct Exact Hash values"
        );
        if index != 0 {
            rows.push_str(",\n");
        }
        write!(
            rows,
            "    {{\"id\":\"{}\",\"name\":\"{}\",\"base_interface_hash\":\"{:016x}\",\
             \"changed_interface_hash\":\"{:016x}\",\"base_generated_hash\":\"{:016x}\",\
             \"changed_generated_hash\":\"{:016x}\",\"unchanged_business_host_should_compile\":{},\
             \"patch_insertions\":{},\"patch_deletions\":{},\"old_bytecode_rejected\":{},\
             \"changed_heartbeat_executed\":{}}}",
            item.id,
            item.name,
            item.base_interface_hash.0,
            item.changed_interface_hash.0,
            item.base_generated_hash,
            item.changed_generated_hash,
            item.unchanged_business_host_should_compile,
            item.patch_insertions,
            item.patch_deletions,
            item.old_bytecode_rejected,
            item.changed_heartbeat_executed,
        )
        .expect("write JSON evidence row");
    }
    fs::write(
        root.join("mutation-report.json"),
        format!(
            "{{\n  \"schema_version\":2,\n  \"business_host\":\"BusinessHostV1\",\
             \n  \"mutation_count\":{},\n  \"status\":\"PASS\",\
             \n  \"mutations\":[\n{rows}\n  ]\n}}\n",
            evidence.len()
        ),
    )
    .expect("write mutation report");
}
