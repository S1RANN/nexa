use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostPayload, HostRegistry, HostTrap, PendingHostRequest,
    RealmConfig, RealmRuntime, ReleaseKind, ResourceContext, RestartReloadOutcome,
    RestartReloadPolicy, RuntimeFailurePoint, RuntimeHost, RuntimeHostArgs, RuntimeValue,
    StateObject, StateValue, StepConfig, TaskLimits, TaskPoll, TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x5245_4c4f_4144_484f);
const SCHEMA_V1: StableId = StableId(0x5245_4c4f_4144_5331);
const SCHEMA_V2: StableId = StableId(0x5245_4c4f_4144_5332);

const V1: &str = include_str!("../../../examples/combat-runtime/reload/v1.nexa");
const V2: &str = include_str!("../../../examples/combat-runtime/reload/v2.nexa");

struct Host {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for Host {
    fn interface_hash(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::ResourceCapacity)?;
        let request = pending.request;
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
        Ok(HostCallOutcome::Pending(request))
    }
}

fn realm(module: VerifiedModule, schema: StableId) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host = RuntimeHost::new(128);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host,
        Box::new(Host {
            pending: Arc::new(Mutex::new(None)),
        }),
    )
    .expect("hosted realm");
    let module = realm
        .load_module(module, HOST, schema)
        .expect("load module");
    (realm, module)
}

fn compile(source: &str, schema: StableId) -> VerifiedModule {
    nexa_compiler::compile_with_metadata(source, HOST, schema).expect("compile reload module")
}

fn config(owner: nexa_runtime::ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    }
}

fn policy() -> RestartReloadPolicy {
    RestartReloadPolicy {
        migration_arguments: vec![RuntimeValue::I32(10)],
        activation_arguments: vec![RuntimeValue::I32(10)],
        activation_fuel: 4_096,
    }
}

struct StatefulScenario {
    realm: RealmRuntime,
    old: nexa_runtime::ModuleHandle,
    boss: StableId,
    preserved: StableId,
    deleted: StableId,
    boss_handle: nexa_runtime::StateHandle,
    preserved_handle: nexa_runtime::StateHandle,
    deleted_handle: nexa_runtime::StateHandle,
}

fn stateful_scenario() -> StatefulScenario {
    let (mut realm, old) = realm(compile(V1, SCHEMA_V1), SCHEMA_V1);
    let boss = StableId::from_name("boss");
    let preserved = StableId::from_name("preserved");
    let deleted = StableId::from_name("deleted");
    let enemy = |phase, legacy| {
        StateValue::Object(StateObject {
            type_id: StableId::from_name("EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    StableId::from_name("EnemyBrain::phase"),
                    StateValue::I32(phase),
                ),
                (
                    StableId::from_name("EnemyBrain::legacy_target"),
                    StateValue::I32(legacy),
                ),
            ]),
        })
    };
    let boss_handle = realm.insert_state(old, boss, enemy(3, 17)).expect("boss");
    let preserved_handle = realm
        .insert_state(
            old,
            preserved,
            StateValue::Object(StateObject {
                type_id: StableId::from_name("StableBrain"),
                version: 1,
                fields: BTreeMap::from([(
                    StableId::from_name("StableBrain::phase"),
                    StateValue::I32(5),
                )]),
            }),
        )
        .expect("preserved");
    let deleted_handle = realm
        .insert_state(old, deleted, enemy(9, 23))
        .expect("deleted");
    StatefulScenario {
        realm,
        old,
        boss,
        preserved,
        deleted,
        boss_handle,
        preserved_handle,
        deleted_handle,
    }
}

fn commit_stateful(scenario: &mut StatefulScenario) -> nexa_runtime::ModuleHandle {
    let outcome = scenario
        .realm
        .restart_reload(scenario.old, compile(V2, SCHEMA_V2), policy())
        .expect("stateful restart reload");
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("stateful reload must commit");
    };
    candidate
}

fn simple_yielding(schema: StableId) -> VerifiedModule {
    compile(
        "task fn update(value: i32) -> i32 { yield; return value; }",
        schema,
    )
}

#[test]
fn schema_unchanged_commits() {
    let module = simple_yielding(SCHEMA_V1);
    let (mut realm, old) = realm(module.clone(), SCHEMA_V1);
    assert!(matches!(
        realm
            .restart_reload(old, module, RestartReloadPolicy::default())
            .expect("reload"),
        RestartReloadOutcome::Committed(_)
    ));
}

#[test]
fn appended_field_receives_migration_default() {
    let mut scenario = stateful_scenario();
    let candidate = commit_stateful(&mut scenario);
    let handle = scenario
        .realm
        .state_handles(candidate)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id == scenario.boss)
        .expect("migrated boss");
    let StateValue::Object(object) = scenario
        .realm
        .resolve_state(candidate, handle)
        .expect("boss state")
    else {
        panic!("boss is object");
    };
    assert_eq!(
        object
            .fields
            .get(&StableId::from_name("EnemyBrain::aggression")),
        Some(&StateValue::I32(0))
    );
}

#[test]
fn preserve_keeps_handle_identity() {
    let mut scenario = stateful_scenario();
    let candidate = commit_stateful(&mut scenario);
    assert_eq!(
        scenario
            .realm
            .state_handles(candidate)
            .expect("state handles")
            .into_iter()
            .find(|handle| handle.stable_id == scenario.preserved),
        Some(scenario.preserved_handle)
    );
}

#[test]
fn replace_invalidates_old_handle_and_creates_new_generation() {
    let mut scenario = stateful_scenario();
    let candidate = commit_stateful(&mut scenario);
    assert!(
        scenario
            .realm
            .resolve_state(candidate, scenario.boss_handle)
            .is_err()
    );
    let replacement = scenario
        .realm
        .state_handles(candidate)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id == scenario.boss)
        .expect("replacement");
    assert_ne!(replacement, scenario.boss_handle);
}

#[test]
fn delete_removes_state_and_invalidates_handle() {
    let mut scenario = stateful_scenario();
    let candidate = commit_stateful(&mut scenario);
    assert!(
        scenario
            .realm
            .resolve_state(candidate, scenario.deleted_handle)
            .is_err()
    );
    assert!(
        scenario
            .realm
            .state_handles(candidate)
            .expect("state handles")
            .into_iter()
            .all(|handle| handle.stable_id != scenario.deleted)
    );
}

#[test]
fn migration_error_rolls_back_before_commit() {
    let mut scenario = stateful_scenario();
    let failing = compile(
        "@stateful(2) class EnemyBrain { phase: i32; aggression: i32; }
         @stateful(1) class StableBrain { phase: i32; }
         migration fn migrate(value: i32) -> i32 {
             let failure: i32 = 1 / 0;
             finish_migration();
             return value + failure;
         }
         task fn update(value: i32) -> i32 { return value; }",
        SCHEMA_V2,
    );
    assert!(matches!(
        scenario
            .realm
            .restart_reload(scenario.old, failing, policy())
            .expect("rollback outcome"),
        RestartReloadOutcome::RolledBackBeforeCommit { .. }
    ));
    assert_eq!(scenario.realm.active_root(), Some(scenario.old));
    assert!(
        scenario
            .realm
            .resolve_state(scenario.old, scenario.boss_handle)
            .is_ok()
    );
}

#[test]
fn restart_cancels_every_old_task() {
    let module = simple_yielding(SCHEMA_V1);
    let (mut realm, old) = realm(module.clone(), SCHEMA_V1);
    let scope = realm.create_scope(None).expect("scope");
    let first = realm
        .spawn_task(old, 0, &[RuntimeValue::I32(1)], config(scope))
        .expect("first");
    let second = realm
        .spawn_task(old, 0, &[RuntimeValue::I32(2)], config(scope))
        .expect("second");
    assert!(matches!(
        realm.poll_task(first, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    assert!(matches!(
        realm.poll_task(second, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    realm
        .restart_reload(old, module, RestartReloadPolicy::default())
        .expect("restart");
    for task in [first, second] {
        assert!(matches!(
            realm.terminal_record(task).map(|record| &record.reason),
            Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
        ));
    }
}

fn async_module() -> VerifiedModule {
    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(result.type_id)),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut function = function.finish().expect("async function");
    function.root_bitmap[0] = true;
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![true],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, SCHEMA_V1).enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("ReloadHost::request"),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    module.function(function);
    verify(module.finish(), VerifierLimits::default()).expect("verified async module")
}

fn async_realm() -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    RuntimeHost,
    Arc<Mutex<Option<PendingHostRequest>>>,
) {
    let pending = Arc::new(Mutex::new(None));
    let host = RuntimeHost::new(128);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host.clone(),
        Box::new(Host {
            pending: Arc::clone(&pending),
        }),
    )
    .expect("async realm");
    let module = realm
        .load_module(async_module(), HOST, SCHEMA_V1)
        .expect("async module");
    (realm, module, host, pending)
}

#[test]
fn late_completion_from_old_epoch_is_discarded() {
    let (mut realm, old, _, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let task = realm.spawn_task(old, 0, &[], config(scope)).expect("task");
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    realm
        .restart_reload(old, async_module(), RestartReloadPolicy::default())
        .expect("restart");
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("pending request")
        .ticket
        .complete(HostPayload::I32(9))
        .expect("physical late completion");
    realm.tick(TickBudget::default()).expect("discard late");
    assert_eq!(realm.discarded_late_host_results(), 1);
}

#[test]
fn activation_fault_is_observable_after_commit() {
    let old_module = simple_yielding(SCHEMA_V1);
    let (mut realm, old) = realm(old_module, SCHEMA_V1);
    let candidate = compile(
        "task fn update(value: i32) -> i32 { return value; }
         @activation fn activate() -> i32 { return 1; }",
        SCHEMA_V1,
    );
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::ActivationTrap);
    let outcome = realm
        .restart_reload(old, candidate, RestartReloadPolicy::default())
        .expect("activation outcome");
    let RestartReloadOutcome::ActivationFaulted { candidate, .. } = outcome else {
        panic!("activation must fault after commit");
    };
    probe.require_consumed().expect("activation reached");
    assert_eq!(realm.active_root(), Some(candidate));
    assert_eq!(
        realm.module_lifecycle(candidate).expect("lifecycle"),
        nexa_runtime::ModuleLifecycle::ActivationFaulted
    );
}

#[test]
fn old_request_releases_once_and_new_entry_starts() {
    let (mut realm, old, host, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let old_task = realm
        .spawn_task(old, 0, &[], config(scope))
        .expect("old task");
    assert!(matches!(
        realm.poll_task(old_task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    let outcome = realm
        .restart_reload(old, async_module(), RestartReloadPolicy::default())
        .expect("restart");
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("reload committed");
    };
    let releases = host.drain_releases();
    assert_eq!(
        releases
            .iter()
            .filter(|release| release.kind == ReleaseKind::HostRequest)
            .count(),
        1
    );
    drop(
        pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(),
    );
    let new_task = realm
        .spawn_task(candidate, 0, &[], config(scope))
        .expect("new entry");
    assert!(matches!(
        realm.poll_task(new_task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    realm
        .cancel_task(new_task, CancelReason::RuntimeShutdown)
        .expect("cleanup");
}
