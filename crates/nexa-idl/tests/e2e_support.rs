use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nexa_core::StableId;
use nexa_runtime::{
    HostCallOutcome, HostRegistry, HostTrap, RealmConfig, RealmError, RealmRuntime,
    ResourceContext, RuntimeHost, RuntimeHostArgs,
};

pub struct Mutation {
    pub id: &'static str,
    pub name: &'static str,
    pub source: String,
    pub unchanged_host_should_compile: bool,
}

pub struct MutationEvidence {
    pub id: &'static str,
    pub name: &'static str,
    pub base_interface_hash: StableId,
    pub changed_interface_hash: StableId,
    pub base_generated_hash: u64,
    pub changed_generated_hash: u64,
    pub unchanged_host_should_compile: bool,
}

pub const BASE_NIDL: &str = r"interface GameHost {
    opaque Entity;
    opaque ActionLock;
    opaque MotionLock;
    struct EnemyView { health: i32; }
    struct EnemyState { health: i32; }
    enum AnimationError { MissingClip, Code(i32), Cancelled, Abandoned }
    sync fuel 5 fn update(entity: i32, delta: i32) -> i32;
    request(return_error, trap) fn animation(entity: i32)
        -> request<Result<i32, AnimationError>>;
    sync fn lock(entity: i32) -> token<ActionLock>;
    sync fn view() -> snapshot<EnemyView>;
    sync fn inspect(entity: Entity) -> i32;
    sync fn score(view: EnemyView) -> i32;
    sync fn classify(error: AnimationError) -> i32;
    export Update(entity: i32) -> i32;
}";

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn mutations() -> Vec<Mutation> {
    vec![
        changed("01", "function-rename", "fn update(", "fn advance(", false),
        changed(
            "02",
            "parameter-type-change",
            "update(entity: i32,",
            "update(entity: i64,",
            false,
        ),
        changed(
            "03",
            "return-type-change",
            "update(entity: i32, delta: i32) -> i32",
            "update(entity: i32, delta: i32) -> i64",
            false,
        ),
        changed(
            "04",
            "sync-to-request",
            "sync fuel 5 fn update(entity: i32, delta: i32) -> i32;",
            "request(return_error, trap) fuel 5 fn update(entity: i32, delta: i32)\n        -> request<Result<i32, AnimationError>>;",
            false,
        ),
        changed("05", "fuel-cost-change", "fuel 5", "fuel 7", true),
        changed(
            "06",
            "cancel-policy-change",
            "request(return_error, trap) fn animation",
            "request(cancel_task, trap) fn animation",
            true,
        ),
        changed(
            "07",
            "abandon-policy-change",
            "request(return_error, trap) fn animation",
            "request(return_error, return_error) fn animation",
            true,
        ),
        changed(
            "08",
            "enum-variant-rename",
            "MissingClip",
            "MissingAsset",
            true,
        ),
        changed(
            "09",
            "struct-field-rename",
            "EnemyView { health:",
            "EnemyView { hit_points:",
            true,
        ),
        changed(
            "10",
            "snapshot-content-type-change",
            "snapshot<EnemyView>",
            "snapshot<EnemyState>",
            false,
        ),
        changed(
            "11",
            "resource-token-domain-change",
            "token<ActionLock>",
            "token<MotionLock>",
            false,
        ),
        changed(
            "12",
            "interface-rename",
            "interface GameHost",
            "interface CombatHost",
            false,
        ),
        changed(
            "13",
            "opaque-handle-rename",
            "opaque Entity;",
            "opaque Actor;",
            false,
        )
        .with_replacement("inspect(entity: Entity)", "inspect(entity: Actor)"),
        changed(
            "14",
            "struct-rename",
            "struct EnemyView",
            "struct EnemyStats",
            false,
        )
        .with_replacement("snapshot<EnemyView>", "snapshot<EnemyStats>")
        .with_replacement("score(view: EnemyView)", "score(view: EnemyStats)"),
        changed(
            "15",
            "enum-rename",
            "enum AnimationError",
            "enum PlaybackError",
            false,
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
            true,
        ),
        changed("17", "parameter-rename", "delta: i32", "step: i32", true),
        changed(
            "18",
            "parameter-addition",
            "update(entity: i32, delta: i32)",
            "update(entity: i32, delta: i32, flags: i32)",
            false,
        ),
        changed(
            "19",
            "struct-field-addition",
            "EnemyView { health: i32; }",
            "EnemyView { health: i32; armor: i32; }",
            true,
        ),
        changed(
            "20",
            "export-addition",
            "export Update(entity: i32) -> i32;",
            "export Update(entity: i32) -> i32;\n    export Reset() -> i32;",
            true,
        ),
    ]
}

impl Mutation {
    fn with_replacement(mut self, from: &str, to: &str) -> Self {
        assert!(self.source.contains(from), "missing mutation token {from}");
        self.source = self.source.replacen(from, to, 1);
        self
    }
}

fn changed(
    id: &'static str,
    name: &'static str,
    from: &str,
    to: &str,
    unchanged_host_should_compile: bool,
) -> Mutation {
    assert!(BASE_NIDL.contains(from), "missing mutation token {from}");
    Mutation {
        id,
        name,
        source: BASE_NIDL.replacen(from, to, 1),
        unchanged_host_should_compile,
    }
}

#[must_use]
pub fn artifact_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nexa-artifacts/idl-e2e")
}

#[must_use]
pub fn prepare_case(
    root: &Path,
    mutation: &Mutation,
    base_generated: &str,
    changed_generated: &str,
) -> PathBuf {
    let case = root.join(format!("{}-{}", mutation.id, mutation.name));
    for directory in ["base", "mutated", "host/src", "script"] {
        fs::create_dir_all(case.join(directory)).expect("create E2E artifact directory");
    }
    fs::write(case.join("base/interface.nidl"), BASE_NIDL).expect("write base NIDL");
    fs::write(case.join("base/bindings.rs"), base_generated).expect("write base binding");
    fs::write(case.join("mutated/interface.nidl"), &mutation.source).expect("write changed NIDL");
    fs::write(case.join("mutated/bindings.rs"), changed_generated).expect("write changed binding");
    fs::write(
        case.join("script/module.nexa"),
        "module idl_e2e;\nimport engine;\ntask fn update(entity: i32) -> i32 { return entity; }\n",
    )
    .expect("write old script");
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
    case
}

#[must_use]
pub fn generated_host_impl(generated: &str, host_name: &str) -> String {
    let start = generated
        .find("pub struct GeneratedHostStub;")
        .expect("generated test stub");
    let end = generated[start..]
        .find("pub const THUNK_")
        .map(|offset| start + offset)
        .expect("generated function constants after test stub");
    generated[start..end].replace("GeneratedHostStub", host_name)
}

#[must_use]
pub fn check_host(
    case: &Path,
    changed_generated: &str,
    host_impl: &str,
    shared_target: &Path,
) -> Output {
    fs::write(
        case.join("host/src/lib.rs"),
        format!("{changed_generated}\n{host_impl}\n"),
    )
    .expect("write host crate");
    Command::new("cargo")
        .args(["+1.97.1", "check", "--offline", "--quiet"])
        .env("CARGO_TARGET_DIR", shared_target)
        .current_dir(case.join("host"))
        .output()
        .expect("run generated host cargo check")
}

pub fn assert_pre_interpreter_rejection(base_idl: &nexa_idl::Idl, changed_hash: StableId) {
    struct Registry(StableId);

    impl HostRegistry for Registry {
        fn interface_hash(&self) -> Option<StableId> {
            Some(self.0)
        }

        fn call_runtime(
            &mut self,
            id: u32,
            _: &mut ResourceContext<'_>,
            _: RuntimeHostArgs<'_>,
        ) -> Result<HostCallOutcome, HostTrap> {
            Err(HostTrap::UnknownFunction(id))
        }
    }

    let schema_hash = StableId::from_name("idl-e2e-schema");
    let old_module = nexa_compiler::compile_with_interface(
        "module idl_e2e;\nimport engine;\ntask fn update(entity: i32) -> i32 { return entity; }",
        base_idl,
        schema_hash,
    )
    .expect("compile base module");
    let runtime_host = RuntimeHost::new(8);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(Registry(changed_hash)),
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
    runtime_host.try_finish_close().expect("close E2E host");
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
             \"changed_generated_hash\":\"{:016x}\",\"unchanged_host_should_compile\":{}}}",
            item.id,
            item.name,
            item.base_interface_hash.0,
            item.changed_interface_hash.0,
            item.base_generated_hash,
            item.changed_generated_hash,
            item.unchanged_host_should_compile,
        )
        .expect("write JSON evidence row");
    }
    fs::write(
        root.join("mutation-report.json"),
        format!(
            "{{\n  \"schema_version\":1,\n  \"mutation_count\":{},\n  \"status\":\"PASS\",\
             \n  \"mutations\":[\n{rows}\n  ]\n}}\n",
            evidence.len()
        ),
    )
    .expect("write mutation report");
}
