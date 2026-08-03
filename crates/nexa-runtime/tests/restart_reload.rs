use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, ScriptExport, Signature, ValueType,
};
use nexa_core::{CanonicalSymbolIdentity, FileId, StableId, SymbolKind};
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostPayload,
    HostRegistry, HostTrap, PendingHostRequest, RealmConfig, RealmRuntime, ReleaseKind,
    ResolvedHostFunction, ResourceContext, RestartReloadOutcome, RestartReloadPolicy,
    RuntimeFailurePoint, RuntimeHost, RuntimeHostArgs, RuntimeValue, StateObject, StateValue,
    StepConfig, TaskLimits, TaskPoll, TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x5245_4c4f_4144_484f);
const ASYNC_MODULE_EXPORT: StableId = StableId(0x5252_4153_594e_4301);
const SNIPPET_PACKAGE: &str = "nexa.snippet";
const SNIPPET_MODULE: &str = "main";
const RELOAD_CONTRACT: &str = "
contract ReloadHost {
    nexa {
        async fn update(value: i32) -> i32;
    }
}
";

const V1: &str = include_str!("../../../examples/combat-runtime/reload/v1.nexa");
const V2: &str = include_str!("../../../examples/combat-runtime/reload/v2.nexa");

struct Host {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for Host {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
            HostFunctionAuthority::new(
                StableId::from_name("ReloadHost::request"),
                [0; 32],
                &[],
                Some(ValueType::Named(result.type_id)),
                HostCallMode::Async,
                1,
                Some(AsyncResultType {
                    result_type: result.type_id,
                    success: ValueType::I32,
                    error: ValueType::I32,
                    cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
                    abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
                    cancel_error: Some(1),
                    abandon_error: None,
                }),
                &[],
            )
        });
        (id == authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            authority,
        ))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        if !arguments.is_empty() {
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

fn realm(module: VerifiedModule) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let state_schema_fingerprint = module.module().state_schema_fingerprint;
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
        .load_module(module, HOST, state_schema_fingerprint)
        .expect("load module");
    (realm, module)
}

fn compile(source: &str) -> VerifiedModule {
    let contract = nexa_idl::parse(RELOAD_CONTRACT).expect("parse reload Contract");
    let mut module =
        nexa_compiler::compile_module_with_contract_file(source, FileId::default(), &contract)
            .expect("compile reload module");
    module.host_contract_id = Some(HOST);
    verify(module, VerifierLimits::default()).expect("verify reload module")
}

fn update_export_id() -> StableId {
    let contract = nexa_idl::parse(RELOAD_CONTRACT).expect("parse reload Contract");
    let update = contract
        .nexa_functions
        .iter()
        .find(|entrypoint| entrypoint.name == "update")
        .expect("reload Contract declares update");
    nexa_idl::entrypoint_stable_id(update)
}

fn state_type_id(name: &str) -> StableId {
    CanonicalSymbolIdentity::automatic(SNIPPET_PACKAGE, SNIPPET_MODULE, SymbolKind::Type, name)
        .runtime_id()
        .0
}

fn state_field_id(owner: &str, name: &str) -> StableId {
    CanonicalSymbolIdentity::automatic(
        SNIPPET_PACKAGE,
        SNIPPET_MODULE,
        SymbolKind::Field,
        format!("{owner}.{name}"),
    )
    .runtime_id()
    .0
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
    let (mut realm, old) = realm(compile(V1));
    let boss = StableId::from_name("boss");
    let preserved = StableId::from_name("preserved");
    let deleted = StableId::from_name("deleted");
    let enemy = |phase, legacy| {
        StateValue::Object(StateObject {
            type_id: state_type_id("EnemyBrain"),
            version: 1,
            fields: BTreeMap::from([
                (
                    state_field_id("EnemyBrain", "phase"),
                    StateValue::I32(phase),
                ),
                (
                    state_field_id("EnemyBrain", "legacy_target"),
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
                type_id: state_type_id("StableBrain"),
                version: 1,
                fields: BTreeMap::from([(
                    state_field_id("StableBrain", "phase"),
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
        .restart_reload(scenario.old, compile(V2), policy())
        .expect("stateful restart reload");
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("stateful reload must commit, got {outcome:?}");
    };
    candidate
}

fn simple_yielding() -> VerifiedModule {
    compile("pub async fn update(value: i32) -> i32 { yield; return value; }")
}

#[test]
fn schema_unchanged_commits() {
    let module = simple_yielding();
    let (mut realm, old) = realm(module.clone());
    assert!(matches!(
        realm
            .restart_reload(old, module, RestartReloadPolicy::default())
            .expect("reload"),
        RestartReloadOutcome::Committed(_)
    ));
}

#[test]
fn schema_unchanged_restart_preserves_typed_state_without_migration_entry() {
    let module = compile(V1);
    let (mut realm, old) = realm(module.clone());
    let state_id = StableId::from_name("overlay");
    realm
        .insert_state(
            old,
            state_id,
            StateValue::Object(StateObject {
                type_id: state_type_id("EnemyBrain"),
                version: 1,
                fields: BTreeMap::from([
                    (state_field_id("EnemyBrain", "phase"), StateValue::I32(7)),
                    (
                        state_field_id("EnemyBrain", "legacy_target"),
                        StateValue::I32(11),
                    ),
                ]),
            }),
        )
        .expect("insert state");
    let outcome = realm
        .restart_reload(old, module, RestartReloadPolicy::default())
        .expect("reload");
    let RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("schema-identical reload must commit, got {outcome:?}");
    };
    let handle = realm
        .state_handles(candidate)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id == state_id)
        .expect("preserved state");
    let StateValue::Object(state) = realm
        .resolve_state(candidate, handle)
        .expect("resolve preserved state")
    else {
        panic!("state remains typed object");
    };
    assert_eq!(
        state.fields.get(&state_field_id("EnemyBrain", "phase")),
        Some(&StateValue::I32(7))
    );
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
            .get(&state_field_id("EnemyBrain", "aggression")),
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
        "@state(version = 2) class EnemyBrain { mut phase: i32, mut aggression: i32, }
         @state(version = 1) class StableBrain { mut phase: i32, }
         @migration
         pub fn migrate(value: i32) -> i32 {
             let failure: i32 = 1 / 0;
             finish_migration();
             return value + failure;
         }
         pub async fn update(value: i32) -> i32 { return value; }",
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
    let module = simple_yielding();
    let (mut realm, old) = realm(module.clone());
    let scope = realm.create_scope(None).expect("scope");
    let first = realm
        .spawn_task(
            old,
            update_export_id(),
            &[RuntimeValue::I32(1)],
            config(scope),
        )
        .expect("first");
    let second = realm
        .spawn_task(
            old,
            update_export_id(),
            &[RuntimeValue::I32(2)],
            config(scope),
        )
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
    let signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::Named(result.type_id)),
    };
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
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
    module
        .metadata(HOST, nexa_bytecode::StateSchema::default().fingerprint())
        .enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("ReloadHost::request"),
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let function = module.function(function);
    module.script_export(ScriptExport {
        stable_id: ASYNC_MODULE_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
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
    let module = async_module();
    let state_schema_fingerprint = module.module().state_schema_fingerprint;
    let module = realm
        .load_module(module, HOST, state_schema_fingerprint)
        .expect("async module");
    (realm, module, host, pending)
}

#[test]
fn late_completion_from_old_epoch_is_discarded() {
    let (mut realm, old, _, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(old, ASYNC_MODULE_EXPORT, &[], config(scope))
        .expect("task");
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
fn activation_fault_restores_the_previous_active_root() {
    let old_module = simple_yielding();
    let (mut realm, old) = realm(old_module);
    let candidate = compile(
        "pub async fn update(value: i32) -> i32 { return value; }
         @activation pub fn activate() -> i32 { return 1; }",
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
    assert_eq!(realm.active_root(), Some(old));
    assert!(
        realm.module_lifecycle(candidate).is_err(),
        "the released activation-fault Candidate remained addressable"
    );
    assert_eq!(realm.resource_ledger().retired_modules, 0);
}

#[test]
fn old_request_releases_once_and_new_entry_starts() {
    let (mut realm, old, host, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let old_task = realm
        .spawn_task(old, ASYNC_MODULE_EXPORT, &[], config(scope))
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
        .spawn_task(candidate, ASYNC_MODULE_EXPORT, &[], config(scope))
        .expect("new entry");
    assert!(matches!(
        realm.poll_task(new_task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    realm
        .cancel_task(new_task, CancelReason::RuntimeShutdown)
        .expect("cleanup");
}

#[test]
fn migration_rollback_does_not_restore_cancelled_old_tasks() {
    let old_definition = simple_yielding();
    let (mut realm, old) = realm(old_definition);
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(
            old,
            update_export_id(),
            &[RuntimeValue::I32(1)],
            config(scope),
        )
        .expect("old task");
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    let failing = compile(
        "@migration
         pub fn migrate(value: i32) -> i32 {
             let failure: i32 = 1 / 0;
             finish_migration();
             return value + failure;
         }
         pub async fn update(value: i32) -> i32 { return value; }",
    );
    assert!(matches!(
        realm
            .restart_reload(old, failing, policy())
            .expect("rollback"),
        RestartReloadOutcome::RolledBackBeforeCommit { .. }
    ));
    assert_eq!(realm.active_root(), Some(old));
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
    ));
}

#[test]
fn migration_rollback_discards_late_old_request_completion() {
    let (mut realm, old, _, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(old, ASYNC_MODULE_EXPORT, &[], config(scope))
        .expect("task");
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    let failing = compile(
        "@migration
         pub fn migrate(value: i32) -> i32 {
             let failure: i32 = 1 / 0;
             finish_migration();
             return value + failure;
         }
         pub async fn update(value: i32) -> i32 { return value; }",
    );
    assert!(matches!(
        realm
            .restart_reload(old, failing, policy())
            .expect("rollback"),
        RestartReloadOutcome::RolledBackBeforeCommit { .. }
    ));
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("physical request")
        .ticket
        .complete(HostPayload::I32(11))
        .expect("late completion");
    realm.tick(TickBudget::default()).expect("discard");
    assert_eq!(realm.discarded_late_host_results(), 1);
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
    ));
}

#[test]
fn restart_implementation_has_no_intermediate_completion_queue() {
    let source = include_str!("../src/reload.rs");
    let removed_type = ["Reload", "Completion", "Buffer"].concat();
    let removed_route = ["Buffered", "For", "Reload"].concat();
    assert!(!source.contains(&removed_type));
    assert!(!source.contains(&removed_route));
}

#[test]
fn old_module_slot_is_released_after_publication() {
    let definition = simple_yielding();
    let (mut realm, old) = realm(definition.clone());
    assert!(matches!(
        realm
            .restart_reload(old, definition, RestartReloadPolicy::default())
            .expect("commit"),
        RestartReloadOutcome::Committed(_)
    ));
    assert!(realm.module_lifecycle(old).is_err());
}

#[test]
fn late_completion_cannot_resume_cancelled_old_task() {
    let (mut realm, old, _, pending) = async_realm();
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(old, ASYNC_MODULE_EXPORT, &[], config(scope))
        .expect("task");
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
        .expect("physical request")
        .ticket
        .complete(HostPayload::I32(12))
        .expect("late completion");
    realm.tick(TickBudget::default()).expect("discard");
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
    ));
    assert_eq!(
        realm.poll_task(task, 64),
        Err(nexa_runtime::RuntimeError::TerminalTask)
    );
}

#[test]
fn restart_task_machine_contains_no_pause_state() {
    let source = include_str!("../src/generated/machines.rs");
    let removed_state = ["Reload", "Paused"].concat();
    assert!(!source.contains(&removed_state));
}
