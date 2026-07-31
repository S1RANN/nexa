use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use nexa::prelude::{
    HostCallOutcome, HostPayload, HostRegistry, HostTrap, PendingHostRequest, RealmConfig,
    RealmRuntime, ResourceContext, RestartReloadOutcome, RestartReloadPolicy, RuntimeHost,
    RuntimeHostArgs, RuntimeValue, ScopeHandle, StateObject, StateValue, StepConfig, TaskLimits,
    TaskPoll, TaskTerminalReason, TickBudget, ValueType, YieldReason,
};
use nexa::{
    CandidateIdentity, CompiledPackageArtifact, PackageBuildSession,
    canonical_package_build_fingerprint_input,
};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageManifest, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder,
};
use nexa_runtime::{ModuleLifecycle, Object};

const PACKAGE_ID: &str = "realm.v5.fixture";
const HOST_SOURCE: &str = include_str!("../../nexa-runtime/fixtures/realm_v5/host.idl");
const A_SOURCE: &str = include_str!("../../nexa-runtime/fixtures/realm_v5/a.nexa");
const B_SOURCE: &str = include_str!("../../nexa-runtime/fixtures/realm_v5/b.nexa");
const C_SOURCE: &str = include_str!("../../nexa-runtime/fixtures/realm_v5/c.nexa");
const D_SOURCE: &str = include_str!("../../nexa-runtime/fixtures/realm_v5/d.nexa");

const B_BASELINE: &str = r"
module realm.v5.b;

@stateful(1) class ModelState {
    value: i32;
    legacy: i32;
}
";

const C_BASELINE: &str = r"
module realm.v5.c;

@stateful(2) class ModelState {
    value: i32;
    replacement: i32;
}
";

const D_BASELINE: &str = r"
module realm.v5.d;

@stateful(3) class ModelState {
    value: i32;
    replacement: i32;
    generation: i32;
}
";

#[derive(Default)]
struct WaitObservation {
    pending: Option<PendingHostRequest>,
    arguments: Vec<i32>,
}

struct WaitRegistry {
    host_hash: nexa::StableId,
    observation: Arc<Mutex<WaitObservation>>,
}

impl HostRegistry for WaitRegistry {
    fn interface_hash(&self) -> Option<nexa::StableId> {
        Some(self.host_hash)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let argument = args.i32(0)?;
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::ResourceCapacity)?;
        let request = pending.request;
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.arguments.push(argument);
        assert!(
            observation.pending.replace(pending).is_none(),
            "fixture issues only one Host request at a time"
        );
        Ok(HostCallOutcome::Pending(request))
    }
}

fn build_input(module: &str, source: &str, contract: &nexa::Idl) -> ResolvedBuildInput {
    let manifest = Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{PACKAGE_ID}"
name = "Realm v5 canonical fixture"
version = "1.0.0"
source_root = "src"
entry = "{module}"
activation = "programmatic"
"#
        ))
        .expect("fixture manifest"),
    );
    let mut source_builder =
        SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    source_builder
        .add(
            NormalizedPackagePath::new(format!("src/{}.nexa", module.replace('.', "/")))
                .expect("fixture source path"),
            source,
            SourceRole::Production,
        )
        .expect("fixture source");
    let sources = Arc::new(source_builder.build().expect("fixture source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("realm-v5-canonical").expect("source ID"),
                directory: NormalizedPackagePath::new("packages/realm-v5")
                    .expect("package directory"),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint_input = canonical_package_build_fingerprint_input(
        &manifest,
        &sources,
        &BTreeMap::new(),
        &BTreeMap::new(),
        contract,
        None,
    );
    let host_contract_source_identity = fingerprint_input.host_contract_source.clone();
    let host_required_exports_identity = fingerprint_input.host_required_exports.clone();
    ResolvedBuildInput::new(
        manifest,
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Arc::<[u8]>::from(nexa::canonical_idl(contract).into_bytes()),
        Arc::<[u8]>::from(host_contract_source_identity),
        Arc::<[u8]>::from(host_required_exports_identity),
        nexa_analysis::CompilationOptions::default(),
        fingerprint_input,
    )
    .expect("resolved canonical fixture")
}

fn compile_fixture(
    session: &mut PackageBuildSession,
    module: &str,
    source: &str,
    contract: &nexa::Idl,
    generation: u64,
) -> CompiledPackageArtifact {
    let input = build_input(module, source, contract);
    let identity = CandidateIdentity::new(
        input.root_manifest.id.clone(),
        generation,
        input.build_fingerprint,
    )
    .expect("candidate identity");
    let artifact = session
        .compile_package(&input, contract, identity)
        .unwrap_or_else(|error| panic!("canonical build of {module} failed: {error:#?}"));
    artifact
        .verify_integrity()
        .unwrap_or_else(|error| panic!("canonical artifact integrity for {module}: {error}"));
    assert!(
        artifact.source_files.files().iter().any(|compiled| {
            compiled.module_path.as_deref() == Some(module) && compiled.text.as_ref() == source
        }),
        "artifact must retain the exact {module} fixture source"
    );
    assert_eq!(
        artifact.module().state_schema_fingerprint,
        artifact.state_schema_fingerprint
    );
    artifact
}

fn task_config(owner: ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 50_000,
        cumulative_budget: 200_000,
        limits: TaskLimits::default(),
    }
}

fn function_index(artifact: &CompiledPackageArtifact, module: &str, name: &str) -> u32 {
    artifact
        .debug_info
        .functions
        .iter()
        .find(|function| {
            function.package_id == PACKAGE_ID
                && function.module_path == module
                && function.name == name
        })
        .unwrap_or_else(|| panic!("missing function {module}::{name}"))
        .function_index
}

struct LoadedFixture {
    realm: RealmRuntime,
    module: nexa::prelude::ModuleHandle,
    owner: ScopeHandle,
    observation: Arc<Mutex<WaitObservation>>,
    runtime_host: RuntimeHost,
}

impl LoadedFixture {
    fn shutdown(mut self) {
        self.realm
            .tick(TickBudget {
                max_tasks: 0,
                frame_fuel_budget: 0,
                collect_garbage: false,
            })
            .expect("flush terminal fixture releases");
        let _observed_releases = self.runtime_host.drain_releases();
        assert_eq!(self.runtime_host.pending_completions(), 0);
        assert_eq!(self.runtime_host.pending_releases(), 0);
        drop(self.realm);
        assert!(self.runtime_host.begin_close().is_drained());
        assert!(
            self.runtime_host
                .try_finish_close()
                .expect("close fixture RuntimeHost")
                .is_drained()
        );
    }
}

fn load_fixture(artifact: &CompiledPackageArtifact, contract: &nexa::Idl) -> LoadedFixture {
    let host_hash = nexa::exact_idl_hash(contract);
    assert_eq!(artifact.module().host_interface_hash, Some(host_hash));
    let observation = Arc::new(Mutex::new(WaitObservation::default()));
    let runtime_host = RuntimeHost::new(32);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(WaitRegistry {
            host_hash,
            observation: Arc::clone(&observation),
        }),
    )
    .expect("hosted fixture Realm");
    let module = realm
        .load_module(
            artifact.verified.clone(),
            host_hash,
            artifact.state_schema_fingerprint,
        )
        .expect("load verified fixture");
    let owner = realm.create_scope(None).expect("fixture scope");
    LoadedFixture {
        realm,
        module,
        owner,
        observation,
        runtime_host,
    }
}

fn assert_host_result_shape(artifact: &CompiledPackageArtifact, module: &str) {
    let import = artifact
        .module()
        .host_imports
        .first()
        .expect("fixture Host import");
    assert_eq!(artifact.module().host_imports.len(), 1);
    let result = import.async_result.expect("async Host Result metadata");
    assert_eq!(import.result, Some(ValueType::Named(result.result_type)));
    assert_eq!(result.success, ValueType::I32);
    assert!(
        matches!(result.error, ValueType::Named(_)),
        "WaitError must remain a nominal error type"
    );
    let request = &artifact.module().functions
        [usize::try_from(function_index(artifact, module, "request")).expect("function index")];
    assert_eq!(
        request.signature.result,
        Some(ValueType::Named(result.result_type)),
        "await Host request must produce Result<i32, WaitError>, not Unit"
    );
}

fn exercise_normal(
    loaded: &mut LoadedFixture,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
) {
    let normal = loaded
        .realm
        .spawn_task(
            loaded.module,
            function_index(artifact, module_name, "normal"),
            &[RuntimeValue::I32(17)],
            task_config(loaded.owner),
        )
        .expect("spawn normal");
    assert_eq!(
        loaded.realm.poll_task(normal, 50_000),
        Ok(TaskPoll::Completed(RuntimeValue::I32(17)))
    );
}

fn exercise_fuel(
    loaded: &mut LoadedFixture,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
) {
    let fuel = loaded
        .realm
        .spawn_task(
            loaded.module,
            function_index(artifact, module_name, "fuel"),
            &[RuntimeValue::I32(23)],
            task_config(loaded.owner),
        )
        .expect("spawn fuel fixture");
    assert_eq!(
        loaded.realm.poll_task(fuel, 4),
        Ok(TaskPoll::Yielded(YieldReason::Fuel)),
        "fixture must exercise a real fuel suspension"
    );
    assert_eq!(
        loaded.realm.poll_task(fuel, 50_000),
        Ok(TaskPoll::Completed(RuntimeValue::I32(23)))
    );
}

fn exercise_explicit_yield(
    loaded: &mut LoadedFixture,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
) {
    let explicit = loaded
        .realm
        .spawn_task(
            loaded.module,
            function_index(artifact, module_name, "explicit"),
            &[RuntimeValue::I32(29)],
            task_config(loaded.owner),
        )
        .expect("spawn explicit-yield fixture");
    assert_eq!(
        loaded.realm.poll_task(explicit, 50_000),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        loaded.realm.poll_task(explicit, 50_000),
        Ok(TaskPoll::Completed(RuntimeValue::I32(29)))
    );
}

fn exercise_host_await(
    loaded: &mut LoadedFixture,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
) {
    let request = loaded
        .realm
        .spawn_task(
            loaded.module,
            function_index(artifact, module_name, "request"),
            &[RuntimeValue::I32(31)],
            task_config(loaded.owner),
        )
        .expect("spawn Host-await fixture");
    assert_eq!(
        loaded.realm.poll_task(request, 50_000),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    let TaskPoll::Waiting(request_handle) = loaded
        .realm
        .poll_task(request, 50_000)
        .expect("poll Host request")
    else {
        panic!("fixture request must wait for Host completion");
    };
    let mut pending = loaded
        .observation
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending
        .take()
        .expect("captured pending Host request");
    assert_eq!(pending.request, request_handle);
    pending
        .ticket
        .complete(HostPayload::I32(32))
        .expect("complete fixture Host request");
    let tick = loaded
        .realm
        .tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 50_000,
            collect_garbage: false,
        })
        .expect("deliver Host completion");
    assert_eq!(tick.completed, 1);
    assert_eq!(
        loaded
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .arguments,
        [31]
    );
    let terminal = loaded
        .realm
        .terminal_record(request)
        .expect("request terminal record");
    let TaskTerminalReason::Completed(Some(RuntimeValue::NamedRef { reference, type_id })) =
        terminal.reason
    else {
        panic!(
            "Host-await fixture must return Result::Ok, got {:?}",
            terminal.reason
        );
    };
    let Object::Enum {
        type_id: object_type,
        tag,
        payload,
        ..
    } = loaded
        .realm
        .resolve_heap_object(reference)
        .expect("Result object")
    else {
        panic!("Host-await result must be represented as the verified Result enum");
    };
    assert_eq!(*object_type, type_id);
    assert_eq!(*tag, 0, "Host success must flow through Result::Ok");
    assert_eq!(*payload, Some(RuntimeValue::I32(32)));
}

fn exercise_tasks(artifact: &CompiledPackageArtifact, contract: &nexa::Idl, module_name: &str) {
    assert_host_result_shape(artifact, module_name);
    let mut loaded = load_fixture(artifact, contract);
    exercise_normal(&mut loaded, artifact, module_name);
    exercise_fuel(&mut loaded, artifact, module_name);
    exercise_explicit_yield(&mut loaded, artifact, module_name);
    exercise_host_await(&mut loaded, artifact, module_name);
    assert!(loaded.realm.resource_invariants_hold());
    loaded.shutdown();
}

fn state_ids(
    artifact: &CompiledPackageArtifact,
    module: &str,
) -> (nexa::StableId, BTreeMap<String, nexa::StableId>, u32) {
    let state = artifact
        .state_type(PACKAGE_ID, module, "ModelState")
        .expect("canonical ModelState surface");
    (
        state.stable_id.0,
        state
            .fields
            .iter()
            .map(|field| (field.name.clone(), field.stable_id.0))
            .collect(),
        state.version,
    )
}

fn insert_model_state(
    realm: &mut RealmRuntime,
    module_handle: nexa::prelude::ModuleHandle,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
    stable_id: nexa::StableId,
    fields: &[(&str, i32)],
) {
    let (type_id, field_ids, version) = state_ids(artifact, module_name);
    realm
        .insert_state(
            module_handle,
            stable_id,
            StateValue::Object(StateObject {
                type_id,
                version,
                fields: fields
                    .iter()
                    .map(|(name, value)| {
                        (
                            *field_ids
                                .get(*name)
                                .unwrap_or_else(|| panic!("missing Stateful field {name}")),
                            StateValue::I32(*value),
                        )
                    })
                    .collect(),
            }),
        )
        .expect("insert old model state");
}

fn state_value(
    realm: &RealmRuntime,
    module: nexa::prelude::ModuleHandle,
    stable_id: nexa::StableId,
) -> Option<StateValue> {
    realm
        .state_handles(module)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id() == stable_id)
        .map(|handle| realm.resolve_state(module, handle).expect("resolve state"))
}

fn assert_model_state(
    realm: &RealmRuntime,
    module_handle: nexa::prelude::ModuleHandle,
    artifact: &CompiledPackageArtifact,
    module_name: &str,
    stable_id: nexa::StableId,
    expected: &[(&str, i32)],
) {
    let (type_id, field_ids, version) = state_ids(artifact, module_name);
    let Some(StateValue::Object(object)) = state_value(realm, module_handle, stable_id) else {
        panic!("missing migrated model state {stable_id}");
    };
    assert_eq!(object.type_id, type_id);
    assert_eq!(object.version, version);
    assert_eq!(object.fields.len(), expected.len());
    for (name, value) in expected {
        assert_eq!(
            object.fields.get(
                field_ids
                    .get(*name)
                    .unwrap_or_else(|| panic!("missing candidate field {name}"))
            ),
            Some(&StateValue::I32(*value))
        );
    }
}

fn reload_with_state(
    old: &CompiledPackageArtifact,
    old_source_module: &str,
    candidate: &CompiledPackageArtifact,
    candidate_module: &str,
    contract: &nexa::Idl,
    old_fields: &[(&str, i32)],
    migration_arguments: Vec<RuntimeValue>,
) {
    assert_eq!(old_source_module, candidate_module);
    let mut loaded = load_fixture(old, contract);
    let seed = nexa::StableId::from_name("seed");
    insert_model_state(
        &mut loaded.realm,
        loaded.module,
        old,
        old_source_module,
        seed,
        old_fields,
    );
    let outcome = loaded
        .realm
        .restart_reload(
            loaded.module,
            candidate.verified.clone(),
            RestartReloadPolicy {
                migration_arguments,
                activation_arguments: vec![RuntimeValue::Bool(false)],
                activation_fuel: 50_000,
            },
        )
        .expect("canonical Stateful reload");
    let RestartReloadOutcome::Committed(candidate_handle) = outcome else {
        panic!("Stateful fixture reload must commit, got {outcome:?}");
    };
    assert_eq!(loaded.realm.active_root(), Some(candidate_handle));
    assert_eq!(
        loaded.realm.module_lifecycle(candidate_handle),
        Ok(ModuleLifecycle::Active)
    );
    assert!(loaded.realm.last_migration_hash().is_some());
    assert!(
        loaded
            .realm
            .last_migration_usage_report()
            .is_some_and(|usage| usage.objects_created > 0 && usage.fields_written > 0)
    );

    let expected = match candidate_module {
        "realm.v5.c" => &[("value", 8), ("replacement", 9), ("generation", 3)][..],
        "realm.v5.d" => &[
            ("value", 8),
            ("replacement", 9),
            ("generation", 3),
            ("final_value", 4),
        ][..],
        other => panic!("unexpected seed migration fixture {other}"),
    };
    assert_model_state(
        &loaded.realm,
        candidate_handle,
        candidate,
        candidate_module,
        seed,
        expected,
    );
    assert!(loaded.realm.resource_invariants_hold());
    loaded.shutdown();
}

fn exercise_b_migration(
    old: &CompiledPackageArtifact,
    candidate: &CompiledPackageArtifact,
    contract: &nexa::Idl,
) {
    let mut loaded = load_fixture(old, contract);
    let boss = nexa::StableId::from_name("boss");
    let preserved = nexa::StableId::from_name("preserved");
    let deleted = nexa::StableId::from_name("deleted");
    insert_model_state(
        &mut loaded.realm,
        loaded.module,
        old,
        "realm.v5.b",
        boss,
        &[("value", 11), ("legacy", 99)],
    );
    loaded
        .realm
        .insert_state(loaded.module, preserved, StateValue::I32(17))
        .expect("insert preserved state");
    loaded
        .realm
        .insert_state(loaded.module, deleted, StateValue::I32(23))
        .expect("insert deleted state");

    let outcome = loaded
        .realm
        .restart_reload(
            loaded.module,
            candidate.verified.clone(),
            RestartReloadPolicy {
                migration_arguments: vec![RuntimeValue::Bool(true)],
                activation_arguments: vec![RuntimeValue::Bool(false)],
                activation_fuel: 50_000,
            },
        )
        .expect("B graph migration");
    let RestartReloadOutcome::Committed(candidate_handle) = outcome else {
        panic!("B migration must commit, got {outcome:?}");
    };
    assert_model_state(
        &loaded.realm,
        candidate_handle,
        candidate,
        "realm.v5.b",
        boss,
        &[("value", 11), ("replacement", 1)],
    );
    assert_model_state(
        &loaded.realm,
        candidate_handle,
        candidate,
        "realm.v5.b",
        nexa::StableId::from_name("seed"),
        &[("value", 1), ("replacement", 2)],
    );
    assert_eq!(
        state_value(&loaded.realm, candidate_handle, preserved),
        Some(StateValue::I32(17))
    );
    assert_eq!(state_value(&loaded.realm, candidate_handle, deleted), None);
    assert_eq!(
        loaded.realm.module_lifecycle(candidate_handle),
        Ok(ModuleLifecycle::Active)
    );
    assert!(loaded.realm.resource_invariants_hold());
    loaded.shutdown();
}

fn exercise_a_activation(artifact: &CompiledPackageArtifact, contract: &nexa::Idl) {
    let mut loaded = load_fixture(artifact, contract);
    let committed = loaded
        .realm
        .restart_reload(
            loaded.module,
            artifact.verified.clone(),
            RestartReloadPolicy {
                migration_arguments: Vec::new(),
                activation_arguments: vec![RuntimeValue::Bool(false)],
                activation_fuel: 50_000,
            },
        )
        .expect("A successful activation");
    let RestartReloadOutcome::Committed(active) = committed else {
        panic!("A reload must commit, got {committed:?}");
    };
    assert_eq!(
        loaded.realm.module_lifecycle(active),
        Ok(ModuleLifecycle::Active)
    );

    let faulted = loaded
        .realm
        .restart_reload(
            active,
            artifact.verified.clone(),
            RestartReloadPolicy {
                migration_arguments: Vec::new(),
                activation_arguments: vec![RuntimeValue::Bool(true)],
                activation_fuel: 50_000,
            },
        )
        .expect("activation faults are post-commit outcomes");
    let RestartReloadOutcome::ActivationFaulted { candidate, .. } = faulted else {
        panic!("failing A activation must be observable, got {faulted:?}");
    };
    assert_eq!(loaded.realm.active_root(), Some(candidate));
    assert_eq!(
        loaded.realm.module_lifecycle(candidate),
        Ok(ModuleLifecycle::ActivationFaulted)
    );
    loaded.shutdown();
}

#[test]
fn realm_v5_fixtures_cross_the_canonical_pipeline_and_execute_in_realm() {
    let contract = nexa::parse_idl(HOST_SOURCE).expect("realm v5 Host NIDL");
    let mut session = PackageBuildSession::new();
    let a = compile_fixture(&mut session, "realm.v5.a", A_SOURCE, &contract, 1);
    let b = compile_fixture(&mut session, "realm.v5.b", B_SOURCE, &contract, 2);
    let c = compile_fixture(&mut session, "realm.v5.c", C_SOURCE, &contract, 3);
    let d = compile_fixture(&mut session, "realm.v5.d", D_SOURCE, &contract, 4);

    for (artifact, module) in [
        (&a, "realm.v5.a"),
        (&b, "realm.v5.b"),
        (&c, "realm.v5.c"),
        (&d, "realm.v5.d"),
    ] {
        exercise_tasks(artifact, &contract, module);
    }

    exercise_a_activation(&a, &contract);

    let b_old = compile_fixture(&mut session, "realm.v5.b", B_BASELINE, &contract, 5);
    exercise_b_migration(&b_old, &b, &contract);

    let c_old = compile_fixture(&mut session, "realm.v5.c", C_BASELINE, &contract, 6);
    reload_with_state(
        &c_old,
        "realm.v5.c",
        &c,
        "realm.v5.c",
        &contract,
        &[("value", 8), ("replacement", 9)],
        Vec::new(),
    );

    let d_old = compile_fixture(&mut session, "realm.v5.d", D_BASELINE, &contract, 7);
    reload_with_state(
        &d_old,
        "realm.v5.d",
        &d,
        "realm.v5.d",
        &contract,
        &[("value", 8), ("replacement", 9), ("generation", 3)],
        Vec::new(),
    );
}
