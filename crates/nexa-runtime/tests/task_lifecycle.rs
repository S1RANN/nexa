use std::sync::{Arc, Mutex};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, Signature, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostCompletionResult, HostErrorPayload, HostRegistry, HostTrap,
    PendingHostRequest, RealmConfig, RealmRuntime, ResourceContext, RuntimeFailurePoint,
    RuntimeHost, RuntimeHostArgs, RuntimeValue, StepConfig, TaskHandle, TaskLimits, TaskPoll,
    TaskTerminalReason, TickBudget, YieldReason,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x5441_534b_484f_5354);
const SCHEMA: StableId = StableId(0x5441_534b_5354_4154);

struct AsyncRegistry {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
    panic: bool,
}

impl HostRegistry for AsyncRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(HOST)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        assert!(!self.panic, "injected host panic");
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
    module.metadata(HOST, SCHEMA).enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("TaskHost::request"),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    module.function(function);
    verify(module.finish(), VerifierLimits::default()).expect("verified async module")
}

fn immediate_module() -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, SCHEMA)
        .function(function.finish().expect("immediate function"));
    verify(module.finish(), VerifierLimits::default()).expect("verified immediate module")
}

fn yielding_module() -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, SCHEMA)
        .function(function.finish().expect("yielding function"));
    verify(module.finish(), VerifierLimits::default()).expect("verified yielding module")
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

fn hosted(
    module: VerifiedModule,
    config: RealmConfig,
    panic: bool,
) -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    RuntimeHost,
    Arc<Mutex<Option<PendingHostRequest>>>,
) {
    let pending = Arc::new(Mutex::new(None));
    let host = RuntimeHost::new(64);
    let mut realm = RealmRuntime::hosted(
        config,
        host.clone(),
        Box::new(AsyncRegistry {
            pending: Arc::clone(&pending),
            panic,
        }),
    )
    .expect("hosted realm");
    let module = realm
        .load_module(module, HOST, SCHEMA)
        .expect("loaded module");
    (realm, module, host, pending)
}

fn spawn(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    arguments: &[RuntimeValue],
) -> TaskHandle {
    let scope = realm.create_scope(None).expect("scope");
    realm
        .spawn_task(module, 0, arguments, config(scope))
        .expect("task")
}

fn assert_terminal_invariants(realm: &mut RealmRuntime, task: TaskHandle) {
    realm
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .expect("terminal flush");
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.requests, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
    assert_eq!(ledger.completion_reservations, 0);
    assert_eq!(ledger.release_reservations, 0);
    assert!(realm.terminal_record(task).is_some());
}

#[test]
fn normal_completion() {
    let (mut realm, module, _, _) = hosted(immediate_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[RuntimeValue::I32(7)]);
    assert_eq!(
        realm.poll_task(task, 64).expect("poll"),
        TaskPoll::Completed(RuntimeValue::I32(7))
    );
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn host_error_is_a_typed_completion() {
    let (mut realm, module, _, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("request handle must only come from Waiting");
    };
    assert_eq!(
        realm
            .complete_request(
                request,
                HostCompletionResult::Error(HostErrorPayload { code: 7 }),
            )
            .expect("host error"),
        nexa_runtime::CompletionDisposition::Delivered
    );
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(_))
    ));
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn host_panic_is_isolated_as_a_trap() {
    let (mut realm, module, _, _) = hosted(async_module(), RealmConfig::default(), true);
    let task = spawn(&mut realm, module, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn task_cancel_returns_terminal_poll() {
    let (mut realm, module, _, _) = hosted(yielding_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[RuntimeValue::I32(1)]);
    assert_eq!(
        realm.poll_task(task, 64).expect("yield"),
        TaskPoll::Yielded(YieldReason::Explicit)
    );
    assert_eq!(
        realm
            .cancel_task(task, CancelReason::HostCancelled)
            .expect("cancel"),
        TaskPoll::Cancelled(CancelReason::HostCancelled)
    );
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn request_abandon_traps_without_invalid_task_state() {
    let (mut realm, module, _, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("expected host request");
    };
    realm.abandon_request(request).expect("abandon");
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Trapped(_))
    ));
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn task_capacity_is_reported_at_admission() {
    let limits = nexa_runtime::RuntimeLimits {
        max_tasks: 1,
        max_scheduler_tokens: 1,
        ..nexa_runtime::RuntimeLimits::default()
    };
    let (mut realm, module, _, _) = hosted(
        yielding_module(),
        RealmConfig {
            runtime_limits: limits,
            ..RealmConfig::default()
        },
        false,
    );
    let first = spawn(&mut realm, module, &[RuntimeValue::I32(1)]);
    let scope = realm.create_scope(None).expect("second scope");
    assert!(
        realm
            .spawn_task(module, 0, &[RuntimeValue::I32(2)], config(scope))
            .is_err()
    );
    realm
        .cancel_task(first, CancelReason::RuntimeShutdown)
        .expect("cleanup first");
    assert_terminal_invariants(&mut realm, first);
}

#[test]
fn request_capacity_probe_is_consumed() {
    let (mut realm, module, _, _) = hosted(async_module(), RealmConfig::default(), false);
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::RequestSlot);
    let task = spawn(&mut realm, module, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    probe.require_consumed().expect("request scenario reached");
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn completion_capacity_probe_is_consumed() {
    let (mut realm, module, _, _) = hosted(async_module(), RealmConfig::default(), false);
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::CompletionSlot);
    let task = spawn(&mut realm, module, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    probe
        .require_consumed()
        .expect("completion scenario reached");
    assert_terminal_invariants(&mut realm, task);
}

fn cleanup_realm() -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let source = "
        fn finalize(value: i32) -> i32 { return value; }
        task fn work(value: i32) -> i32 {
            defer finalize(value);
            yield;
            let next: i32 = value + 1;
            return next;
        }
    ";
    let module =
        nexa_compiler::compile_with_metadata(source, HOST, SCHEMA).expect("cleanup module");
    let (realm, handle, _, _) = hosted(module, RealmConfig::default(), false);
    (realm, handle)
}

#[test]
fn cleanup_succeeds_and_balances_resources() {
    let (mut realm, module) = cleanup_realm();
    let scope = realm.create_scope(None).expect("cleanup scope");
    let task = realm
        .spawn_task(module, 1, &[RuntimeValue::I32(1)], config(scope))
        .expect("cleanup task");
    let first = realm.poll_task(task, 64);
    assert!(
        matches!(first, Ok(TaskPoll::Yielded(YieldReason::Explicit))),
        "{first:?}"
    );
    assert!(matches!(
        realm.cancel_task(task, CancelReason::HostCancelled),
        Ok(TaskPoll::Cancelled(_))
    ));
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn cleanup_trap_probe_is_consumed() {
    let (mut realm, module) = cleanup_realm();
    let scope = realm.create_scope(None).expect("cleanup scope");
    let task = realm
        .spawn_task(module, 1, &[RuntimeValue::I32(1)], config(scope))
        .expect("cleanup task");
    let first = realm.poll_task(task, 64);
    assert!(
        matches!(first, Ok(TaskPoll::Yielded(YieldReason::Explicit))),
        "{first:?}"
    );
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::CleanupTrap);
    assert!(matches!(
        realm.cancel_task(task, CancelReason::HostCancelled),
        Ok(TaskPoll::Trapped(_))
    ));
    probe.require_consumed().expect("cleanup scenario reached");
    assert_terminal_invariants(&mut realm, task);
}

#[test]
fn realm_drop_releases_live_task_resources_once() {
    let (mut realm, module, host, _) = hosted(yielding_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[RuntimeValue::I32(1)]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    drop(realm);
    assert_eq!(host.pending_completions(), 0);
}

#[test]
fn module_restart_cancels_old_task_and_starts_new_module() {
    let module_definition = yielding_module();
    let (mut realm, module, _, _) =
        hosted(module_definition.clone(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, &[RuntimeValue::I32(1)]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    let outcome = realm
        .restart_reload(
            module,
            module_definition,
            nexa_runtime::RestartReloadPolicy::default(),
        )
        .expect("restart reload");
    let nexa_runtime::RestartReloadOutcome::Committed(candidate) = outcome else {
        panic!("restart must commit");
    };
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
    ));
    let new_task = spawn(&mut realm, candidate, &[RuntimeValue::I32(2)]);
    assert!(matches!(
        realm.poll_task(new_task, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    realm
        .cancel_task(new_task, CancelReason::RuntimeShutdown)
        .expect("new task cleanup");
    assert_terminal_invariants(&mut realm, new_task);
}

#[test]
fn stale_and_cross_realm_task_handles_are_distinct() {
    let (mut first, module, _, _) = hosted(immediate_module(), RealmConfig::default(), false);
    let task = spawn(&mut first, module, &[RuntimeValue::I32(1)]);
    first.poll_task(task, 64).expect("complete");
    assert_eq!(
        first.poll_task(task, 64),
        Err(nexa_runtime::RuntimeError::TerminalTask)
    );

    let (mut second, _, _, _) = hosted(
        immediate_module(),
        RealmConfig {
            realm_id: 2,
            ..RealmConfig::default()
        },
        false,
    );
    assert_eq!(
        second.poll_task(task, 64),
        Err(nexa_runtime::RuntimeError::CrossRealmTaskHandle)
    );
}
