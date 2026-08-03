use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use nexa_bytecode::{
    AbandonPolicy, ArrayType, AsyncResultType, CancelPolicy, ClassType, FunctionBuilder,
    FunctionEffect, HostCallMode, HostImport, Instruction, MapType, ModuleBuilder, RootMap,
    ScriptExport, Signature, StateField, StateSchema, StateType, StructField, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostPayload,
    HostRegistry, HostTrap, ModuleHandle, Object, PendingHostRequest, RealmConfig, RealmRuntime,
    ResolvedHostFunction, ResourceContext, RestartReloadPolicy, RuntimeHost, RuntimeHostArgs,
    RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits, TransactionalCellEntrypoint,
    TransactionalCellFailureCause, TransactionalCellPoll,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST_CONTRACT_ID: StableId = StableId(0x5245_504c_5f54_584e);
const CELL_ID: StableId = StableId(0x4345_4c4c_5f54_4153);
const ENV_ID: StableId = StableId(0x5245_504c_5f45_4e56);
const ENV_TYPE: StableId = StableId(0x5245_504c_5f54_5950);
const ENV_VALUE_FIELD: StableId = StableId(0x5245_504c_5f56_414c);
const NESTED_CLASS_TYPE: StableId = StableId(0x4e45_5354_434c_4153);
const NESTED_CLASS_FIELD: StableId = StableId(0x4e45_5354_4649_454c);

fn cell_signature() -> Signature {
    Signature {
        parameters: Vec::new(),
        result: Some(ValueType::I32),
    }
}

fn module(
    cell_code: impl IntoIterator<Item = Instruction>,
    activation_traps: bool,
) -> VerifiedModule {
    let signature = cell_signature();
    let mut cell = FunctionBuilder::new(signature.clone(), 1);
    cell.effect(FunctionEffect::Task);
    for instruction in cell_code {
        cell.emit(instruction);
    }
    let mut cell = cell.finish().expect("cell function");
    add_post_yield_safepoints(&mut cell);
    let mut module = ModuleBuilder::new();
    module.metadata(
        HOST_CONTRACT_ID,
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    let cell = module.function(cell);
    module.script_export(ScriptExport {
        stable_id: CELL_ID,
        function: cell,
        effect: FunctionEffect::Task,
        signature,
    });
    if activation_traps {
        let mut activation = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        activation
            .effect(FunctionEffect::Immediate)
            .emit(Instruction::Trap);
        let activation = module.function(activation.finish().expect("activation function"));
        module.reload_entries(None, Some(activation));
    }
    verify(module.finish(), VerifierLimits::default()).expect("verified cell")
}

fn returning(value: i32) -> VerifiedModule {
    module(
        [
            Instruction::LoadI32 { dst: 0, value },
            Instruction::Return { source: 0 },
        ],
        false,
    )
}

fn realm() -> (RealmRuntime, ModuleHandle, nexa_runtime::ScopeHandle) {
    let old = returning(1);
    let schema = old.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old, HOST_CONTRACT_ID, schema)
        .expect("old module");
    let scope = realm.create_scope(None).expect("cell scope");
    (realm, old, scope)
}

fn step(owner: nexa_runtime::ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    }
}

fn entrypoint() -> TransactionalCellEntrypoint {
    TransactionalCellEntrypoint::new(CELL_ID, cell_signature())
}

fn add_post_yield_safepoints(function: &mut nexa_bytecode::Function) {
    let post_yield = function
        .code
        .windows(2)
        .enumerate()
        .filter_map(|(pc, pair)| {
            if matches!(pair[0], Instruction::Yield) {
                Some(u32::try_from(pc + 1).expect("fixture bytecode position fits u32"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    for pc in post_yield {
        if let Err(index) = function.safepoints.binary_search(&pc) {
            function.safepoints.insert(index, pc);
            function.root_maps.insert(
                index,
                RootMap {
                    pc,
                    bitmap: vec![false; usize::from(function.registers)],
                },
            );
        }
    }
}

#[test]
fn successful_cell_does_not_publish_until_commit() {
    let (mut realm, old, scope) = realm();
    let transaction = realm
        .stage_cell_transaction(
            old,
            returning(7),
            &entrypoint(),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage cell");
    let candidate = transaction.candidate();
    assert_eq!(transaction.active_root(), Some(old));
    assert_eq!(
        transaction.candidate_lifecycle(),
        Ok(nexa_runtime::ModuleLifecycle::Staging)
    );
    let mut transaction = transaction;
    assert!(matches!(
        transaction.poll(),
        Ok(TransactionalCellPoll::ReadyToCommit {
            value: RuntimeValue::I32(7),
            ..
        })
    ));
    assert_eq!(transaction.active_root(), Some(old));
    let committed = transaction.commit().expect("commit cell");
    assert_eq!(committed.module, candidate);
    assert_eq!(committed.value, RuntimeValue::I32(7));
    assert_eq!(realm.active_root(), Some(candidate));
    assert!(realm.module_lifecycle(old).is_err());
    assert!(realm.resource_invariants_hold());
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.tasks, 0);
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
    assert_eq!(ledger.requests, 0);
}

#[test]
fn trapped_cell_rolls_back_and_preserves_old_state() {
    let (mut realm, old, scope) = realm();
    let state_id = StableId::from_name("repl.binding");
    let state = realm
        .insert_state(old, state_id, StateValue::I32(19))
        .expect("state");
    let candidate;
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                module([Instruction::Trap], false),
                &entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage trap cell");
        candidate = transaction.candidate();
        let failure = transaction.poll().expect_err("trap rolls back");
        assert!(matches!(
            failure.cause,
            TransactionalCellFailureCause::Trapped(_)
        ));
        assert!(failure.rollback_error.is_none());
    }
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.module_lifecycle(candidate).is_err());
    assert_eq!(realm.resolve_state(old, state), Ok(StateValue::I32(19)));
    assert!(realm.resource_invariants_hold());
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.tasks, 0);
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
    assert_eq!(ledger.requests, 0);
}

#[test]
fn explicit_cancel_and_drop_both_rollback() {
    let (mut realm, old, scope) = realm();
    let candidate = {
        let transaction = realm
            .stage_cell_transaction(
                old,
                module(
                    [
                        Instruction::Yield,
                        Instruction::LoadI32 { dst: 0, value: 3 },
                        Instruction::Return { source: 0 },
                    ],
                    false,
                ),
                &entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage yielding cell");
        let candidate = transaction.candidate();
        let rollback = transaction
            .cancel(CancelReason::HostCancelled)
            .expect("cancel");
        assert_eq!(rollback.candidate, candidate);
        candidate
    };
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.module_lifecycle(candidate).is_err());

    let dropped_candidate = {
        let transaction = realm
            .stage_cell_transaction(
                old,
                returning(11),
                &entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage dropped cell");
        transaction.candidate()
    };
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.module_lifecycle(dropped_candidate).is_err());
    assert!(realm.resource_invariants_hold());
}

#[test]
fn activation_failure_happens_before_publication_and_rolls_back() {
    let (mut realm, old, scope) = realm();
    let mut transaction = realm
        .stage_cell_transaction(
            old,
            module(
                [
                    Instruction::LoadI32 { dst: 0, value: 23 },
                    Instruction::Return { source: 0 },
                ],
                true,
            ),
            &entrypoint(),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage activation-faulting cell");
    let candidate = transaction.candidate();
    assert!(matches!(
        transaction.poll(),
        Ok(TransactionalCellPoll::ReadyToCommit { .. })
    ));
    let failure = transaction.commit().expect_err("activation rolls back");
    assert!(matches!(
        failure.cause,
        TransactionalCellFailureCause::Activation(_)
    ));
    assert!(failure.rollback_error.is_none());
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.module_lifecycle(candidate).is_err());
    assert!(realm.resource_invariants_hold());
}

#[test]
fn cumulative_fuel_exhaustion_rolls_back_without_task_or_scheduler_leaks() {
    let (mut realm, old, scope) = realm();
    let mut constrained = step(scope);
    constrained.fuel_slice = 1;
    constrained.cumulative_budget = 1;
    let candidate;
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                returning(29),
                &entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                constrained,
            )
            .expect("stage fuel-bounded cell");
        candidate = transaction.candidate();
        let failure = transaction.poll().expect_err("fuel must cancel");
        assert!(matches!(
            failure.cause,
            TransactionalCellFailureCause::Cancelled(CancelReason::BudgetExceeded)
        ));
        assert!(failure.rollback_error.is_none());
    }
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.module_lifecycle(candidate).is_err());
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.tasks, 0);
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
}

struct PendingHost {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for PendingHost {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(HOST_CONTRACT_ID)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
            HostFunctionAuthority::new(
                StableId::from_name("ReplHost::wait"),
                [0; 32],
                &[],
                Some(ValueType::Named(result.type_id)),
                HostCallMode::Async,
                1,
                Some(AsyncResultType {
                    result_type: result.type_id,
                    success: ValueType::I32,
                    error: ValueType::I32,
                    cancel_policy: CancelPolicy::ReturnError,
                    abandon_policy: AbandonPolicy::Trap,
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

fn async_cell_module() -> (VerifiedModule, Signature) {
    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::Named(result.type_id)),
    };
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: CancelPolicy::ReturnError,
        abandon_policy: AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let mut cell = FunctionBuilder::new(signature.clone(), 1);
    cell.effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut cell = cell.finish().expect("async cell");
    cell.root_bitmap[0] = true;
    cell.safepoints = vec![0, 1];
    cell.root_maps = vec![
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
        .metadata(
            HOST_CONTRACT_ID,
            nexa_bytecode::StateSchema::default().fingerprint(),
        )
        .enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("ReplHost::wait"),
        declaration_fingerprint: [0; 32],
        parameters: Vec::new(),
        result: signature.result,
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
        capabilities: Vec::new(),
    });
    let function = module.function(cell);
    module.script_export(ScriptExport {
        stable_id: CELL_ID,
        function,
        effect: FunctionEffect::Task,
        signature: signature.clone(),
    });
    (
        verify(module.finish(), VerifierLimits::default()).expect("verified async cell"),
        signature,
    )
}

#[test]
fn queued_host_completion_is_discarded_during_transaction_rollback() {
    let pending = Arc::new(Mutex::new(None));
    let old_module = returning(1);
    let schema = old_module.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        RuntimeHost::new(32),
        Box::new(PendingHost {
            pending: Arc::clone(&pending),
        }),
    )
    .expect("hosted Realm");
    let old = realm
        .load_module(old_module, HOST_CONTRACT_ID, schema)
        .expect("old module");
    let scope = realm.create_scope(None).expect("scope");
    let (candidate_module, signature) = async_cell_module();
    let mut transaction = realm
        .stage_cell_transaction(
            old,
            candidate_module,
            &TransactionalCellEntrypoint::new(CELL_ID, signature),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage async cell");
    assert!(matches!(
        transaction.poll(),
        Ok(TransactionalCellPoll::Waiting(_))
    ));
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("pending Host request");
    pending
        .ticket
        .complete(HostPayload::I32(31))
        .expect("queue completion");
    transaction
        .cancel(CancelReason::HostCancelled)
        .expect("rollback waiting cell");
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.resource_invariants_hold());
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.tasks, 0);
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
    assert_eq!(ledger.requests, 0);
    assert_eq!(ledger.completion_reservations, 0);
    assert_eq!(ledger.queued_releases, 0);
}

fn state_schema() -> StateSchema {
    StateSchema {
        types: vec![StateType {
            stable_id: ENV_TYPE,
            version: 1,
            fields: vec![StateField {
                stable_id: ENV_VALUE_FIELD,
                ty: ValueType::I32,
            }],
        }],
    }
}

fn state_current_get_module(code: impl IntoIterator<Item = Instruction>) -> VerifiedModule {
    let signature = cell_signature();
    let mut cell = FunctionBuilder::new(signature.clone(), 4);
    cell.effect(FunctionEffect::Task);
    for instruction in code {
        cell.emit(instruction);
    }
    let mut cell = cell.finish().expect("state cell");
    cell.root_bitmap = vec![true, false, false, false];
    for root_map in &mut cell.root_maps {
        root_map.bitmap = vec![false, false, false, false];
    }
    let schema = state_schema();
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST_CONTRACT_ID, schema.fingerprint())
        .state_schema(schema)
        .class_type(ClassType {
            type_id: ENV_TYPE,
            fields: vec![StructField {
                stable_id: ENV_VALUE_FIELD,
                ty: ValueType::I32,
            }],
        });
    let function = module.function(cell);
    module.script_export(ScriptExport {
        stable_id: CELL_ID,
        function,
        effect: FunctionEffect::Task,
        signature,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified state cell")
}

fn state_environment(value: i32) -> StateValue {
    StateValue::Object(StateObject {
        type_id: ENV_TYPE,
        version: 1,
        fields: BTreeMap::from([(ENV_VALUE_FIELD, StateValue::I32(value))]),
    })
}

#[test]
fn state_current_get_reads_and_commits_candidate_proxy_mutation() {
    let code = [
        Instruction::StateCurrentGet {
            stable_id: ENV_ID,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::ClassGet {
            source: 0,
            field: ENV_VALUE_FIELD,
            dst: 1,
        },
        Instruction::LoadI32 { dst: 2, value: 42 },
        Instruction::ClassSet {
            source: 0,
            field: ENV_VALUE_FIELD,
            value: 2,
        },
        Instruction::ClassGet {
            source: 0,
            field: ENV_VALUE_FIELD,
            dst: 3,
        },
        Instruction::Return { source: 3 },
    ];
    let old_module = state_current_get_module(code);
    let schema = old_module.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old_module, HOST_CONTRACT_ID, schema)
        .expect("old state module");
    realm
        .insert_state(old, ENV_ID, state_environment(5))
        .expect("state environment");
    let scope = realm.create_scope(None).expect("scope");
    let mut transaction = realm
        .stage_cell_transaction(
            old,
            state_current_get_module(code),
            &entrypoint(),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage state cell");
    assert!(matches!(
        transaction.poll(),
        Ok(TransactionalCellPoll::ReadyToCommit {
            value: RuntimeValue::I32(42),
            ..
        })
    ));
    let committed = transaction.commit().expect("commit state cell");
    let handle = realm
        .state_handles(committed.module)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id == ENV_ID)
        .expect("environment handle");
    let StateValue::Object(environment) = realm
        .resolve_state(committed.module, handle)
        .expect("current environment")
    else {
        panic!("environment remains a state object");
    };
    assert_eq!(
        environment.fields.get(&ENV_VALUE_FIELD),
        Some(&StateValue::I32(42))
    );
}

#[test]
fn state_current_get_proxy_mutation_is_discarded_when_cell_traps() {
    let success_code = [
        Instruction::StateCurrentGet {
            stable_id: ENV_ID,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::ClassGet {
            source: 0,
            field: ENV_VALUE_FIELD,
            dst: 1,
        },
        Instruction::LoadI32 { dst: 2, value: 1 },
        Instruction::ClassSet {
            source: 0,
            field: ENV_VALUE_FIELD,
            value: 2,
        },
        Instruction::ClassGet {
            source: 0,
            field: ENV_VALUE_FIELD,
            dst: 3,
        },
        Instruction::Return { source: 3 },
    ];
    let trap_code = [
        Instruction::StateCurrentGet {
            stable_id: ENV_ID,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::LoadI32 { dst: 2, value: 99 },
        Instruction::ClassSet {
            source: 0,
            field: ENV_VALUE_FIELD,
            value: 2,
        },
        Instruction::Trap,
    ];
    let old_module = state_current_get_module(success_code);
    let schema = old_module.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old_module, HOST_CONTRACT_ID, schema)
        .expect("old state module");
    let old_handle = realm
        .insert_state(old, ENV_ID, state_environment(7))
        .expect("state environment");
    let scope = realm.create_scope(None).expect("scope");
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                state_current_get_module(trap_code),
                &entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage trapping state cell");
        let failure = transaction.poll().expect_err("state cell traps");
        assert!(matches!(
            failure.cause,
            TransactionalCellFailureCause::Trapped(_)
        ));
        assert!(failure.rollback_error.is_none());
    }
    let StateValue::Object(environment) = realm
        .resolve_state(old, old_handle)
        .expect("old environment")
    else {
        panic!("old environment remains a state object");
    };
    assert_eq!(
        environment.fields.get(&ENV_VALUE_FIELD),
        Some(&StateValue::I32(7))
    );
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.resource_invariants_hold());
}

fn transactional_environment_seed() -> VerifiedModule {
    let schema = StateSchema {
        types: vec![StateType {
            stable_id: ENV_TYPE,
            version: 1,
            fields: Vec::new(),
        }],
    };
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST_CONTRACT_ID, schema.fingerprint())
        .state_schema(schema)
        .class_type(ClassType {
            type_id: ENV_TYPE,
            fields: Vec::new(),
        });
    verify(module.finish(), VerifierLimits::default()).expect("verified transactional seed")
}

fn transactional_environment_candidate(
    code: impl IntoIterator<Item = Instruction>,
) -> VerifiedModule {
    let signature = cell_signature();
    let mut cell = FunctionBuilder::new(signature.clone(), 3);
    cell.effect(FunctionEffect::Task);
    for instruction in code {
        cell.emit(instruction);
    }
    let mut cell = cell.finish().expect("transactional environment cell");
    cell.root_bitmap = vec![true, false, false];
    add_post_yield_safepoints(&mut cell);
    for root_map in &mut cell.root_maps {
        let pc = usize::try_from(root_map.pc).expect("fixture bytecode position fits usize");
        let environment_is_live = matches!(cell.code.get(pc), Some(Instruction::Yield))
            || pc
                .checked_sub(1)
                .and_then(|previous| cell.code.get(previous))
                .is_some_and(|instruction| matches!(instruction, Instruction::Yield));
        root_map.bitmap = if environment_is_live {
            vec![true, false, false]
        } else {
            vec![false, false, false]
        };
    }
    let schema = StateSchema {
        types: vec![StateType {
            stable_id: ENV_TYPE,
            version: 1,
            fields: vec![StateField {
                stable_id: ENV_VALUE_FIELD,
                ty: ValueType::I32,
            }],
        }],
    };
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST_CONTRACT_ID, schema.fingerprint())
        .state_schema(schema)
        .class_type(ClassType {
            type_id: ENV_TYPE,
            fields: vec![StructField {
                stable_id: ENV_VALUE_FIELD,
                ty: ValueType::I32,
            }],
        });
    let function = module.function(cell);
    module.script_export(ScriptExport {
        stable_id: CELL_ID,
        function,
        effect: FunctionEffect::Task,
        signature,
    });
    verify(module.finish(), VerifierLimits::default())
        .expect("verified transactional environment candidate")
}

fn transactional_environment_realm() -> (RealmRuntime, ModuleHandle, nexa_runtime::ScopeHandle) {
    let seed = transactional_environment_seed();
    let schema = seed.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(seed, HOST_CONTRACT_ID, schema)
        .expect("load transactional seed");
    realm
        .initialize_transactional_state_seed(old, ENV_TYPE)
        .expect("initialize empty transactional environment");
    let scope = realm.create_scope(None).expect("cell scope");
    (realm, old, scope)
}

fn extension_entrypoint() -> TransactionalCellEntrypoint {
    entrypoint().with_state_extension(ENV_TYPE)
}

#[test]
fn transactional_environment_marker_rejects_an_equal_schema() {
    let (mut realm, old, scope) = transactional_environment_realm();
    let Err(error) = realm.stage_cell_transaction(
        old,
        transactional_environment_seed(),
        &extension_entrypoint(),
        &[],
        RestartReloadPolicy::default(),
        step(scope),
    ) else {
        panic!("an extension must append at least one field");
    };
    assert!(matches!(
        error,
        nexa_runtime::RealmError::InvalidTransactionalStateExtension
    ));
    assert_eq!(realm.active_root(), Some(old));
}

#[test]
fn transactional_environment_field_is_initialized_only_by_successful_cell() {
    let (mut realm, old, scope) = transactional_environment_realm();
    let candidate = transactional_environment_candidate([
        Instruction::StateCurrentGet {
            stable_id: ENV_TYPE,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::LoadI32 { dst: 1, value: 77 },
        Instruction::ClassSet {
            source: 0,
            field: ENV_VALUE_FIELD,
            value: 1,
        },
        Instruction::Return { source: 1 },
    ]);
    let mut transaction = realm
        .stage_cell_transaction(
            old,
            candidate,
            &extension_entrypoint(),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage environment extension");
    assert!(matches!(
        transaction.poll(),
        Ok(TransactionalCellPoll::ReadyToCommit {
            value: RuntimeValue::I32(77),
            ..
        })
    ));
    let committed = transaction.commit().expect("commit initialized extension");
    let handle = realm
        .state_handles(committed.module)
        .expect("state handles")
        .into_iter()
        .find(|handle| handle.stable_id == ENV_TYPE)
        .expect("environment state");
    let StateValue::Object(environment) = realm
        .resolve_state(committed.module, handle)
        .expect("committed environment")
    else {
        panic!("environment remains an object");
    };
    assert_eq!(
        environment.fields.get(&ENV_VALUE_FIELD),
        Some(&StateValue::I32(77))
    );
}

#[test]
fn transactional_environment_missing_initializer_fails_before_ready() {
    let (mut realm, old, scope) = transactional_environment_realm();
    let old_handle = realm.state_handles(old).expect("seed state handles")[0];
    let expected_next_epoch = realm.active_module_epoch(old).expect("seed epoch") + 1;
    let expected_next_domain = realm
        .module_stateful_domain(old)
        .expect("seed stateful domain")
        .get()
        + 1;
    let candidate = transactional_environment_candidate([
        Instruction::StateCurrentGet {
            stable_id: ENV_TYPE,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::LoadI32 { dst: 1, value: 9 },
        Instruction::Return { source: 1 },
    ]);
    let mut transaction = realm
        .stage_cell_transaction(
            old,
            candidate,
            &extension_entrypoint(),
            &[],
            RestartReloadPolicy::default(),
            step(scope),
        )
        .expect("stage incomplete extension");
    let failure = transaction
        .poll()
        .expect_err("pending field must fail before ReadyToCommit");
    assert!(matches!(
        failure.cause,
        TransactionalCellFailureCause::Runtime(_)
    ));
    assert!(failure.rollback_error.is_none());
    drop(transaction);
    assert_eq!(realm.active_root(), Some(old));
    assert_eq!(
        realm.resolve_state(old, old_handle),
        Ok(StateValue::Object(StateObject {
            type_id: ENV_TYPE,
            version: 1,
            fields: BTreeMap::new(),
        }))
    );

    let probe = transactional_environment_seed();
    let probe_schema = probe.module().state_schema_fingerprint;
    let probe = realm
        .load_module(probe, HOST_CONTRACT_ID, probe_schema)
        .expect("failed Cell must leave the next module counters untouched");
    assert_eq!(
        realm.active_module_epoch(probe),
        Ok(expected_next_epoch),
        "a rolled-back Cell must not consume a Realm epoch"
    );
    assert_eq!(
        realm
            .module_stateful_domain(probe)
            .map(nexa_runtime::StatefulDomainId::get),
        Ok(expected_next_domain),
        "a rolled-back Cell must not consume a stateful domain"
    );
}

#[test]
fn transactional_environment_trap_cancel_and_drop_discard_overlay() {
    let (mut realm, old, scope) = transactional_environment_realm();
    let trapping = transactional_environment_candidate([
        Instruction::StateCurrentGet {
            stable_id: ENV_TYPE,
            type_id: ENV_TYPE,
            dst: 0,
        },
        Instruction::LoadI32 { dst: 1, value: 41 },
        Instruction::ClassSet {
            source: 0,
            field: ENV_VALUE_FIELD,
            value: 1,
        },
        Instruction::Trap,
    ]);
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                trapping,
                &extension_entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage trapping extension");
        assert!(matches!(
            transaction.poll().expect_err("cell trap"),
            nexa_runtime::TransactionalCellFailure {
                cause: TransactionalCellFailureCause::Trapped(_),
                ..
            }
        ));
    }
    assert_eq!(realm.active_root(), Some(old));

    let yielding = || {
        transactional_environment_candidate([
            Instruction::StateCurrentGet {
                stable_id: ENV_TYPE,
                type_id: ENV_TYPE,
                dst: 0,
            },
            Instruction::Yield,
            Instruction::LoadI32 { dst: 1, value: 42 },
            Instruction::ClassSet {
                source: 0,
                field: ENV_VALUE_FIELD,
                value: 1,
            },
            Instruction::Return { source: 1 },
        ])
    };
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                yielding(),
                &extension_entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage cancellable extension");
        assert!(matches!(
            transaction.poll(),
            Ok(TransactionalCellPoll::Yielded(_))
        ));
        transaction
            .cancel(CancelReason::HostCancelled)
            .expect("cancel extension");
    }
    assert_eq!(realm.active_root(), Some(old));
    {
        let transaction = realm
            .stage_cell_transaction(
                old,
                yielding(),
                &extension_entrypoint(),
                &[],
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage dropped extension");
        assert_eq!(transaction.active_root(), Some(old));
    }
    assert_eq!(realm.active_root(), Some(old));
    let handle = realm.state_handles(old).expect("seed state handles")[0];
    let StateValue::Object(environment) =
        realm.resolve_state(old, handle).expect("old environment")
    else {
        panic!("seed environment remains an object");
    };
    assert!(environment.fields.is_empty());
    assert!(realm.resource_invariants_hold());
}

#[allow(clippy::too_many_lines)]
fn heap_rollback_module(tail: impl IntoIterator<Item = Instruction>) -> VerifiedModule {
    let array_type = nexa_bytecode::array_type(ValueType::I32);
    let map_type = nexa_bytecode::map_type(ValueType::I32, ValueType::I32);
    let signature = Signature {
        parameters: vec![
            ValueType::Named(NESTED_CLASS_TYPE),
            ValueType::Named(array_type),
            ValueType::Named(map_type),
        ],
        result: Some(ValueType::I32),
    };
    let mut cell = FunctionBuilder::new(signature.clone(), 6);
    cell.effect(FunctionEffect::Task);
    for instruction in [
        Instruction::LoadI32 { dst: 4, value: 99 },
        Instruction::ClassSet {
            source: 0,
            field: NESTED_CLASS_FIELD,
            value: 4,
        },
        Instruction::ArrayPush {
            source: 1,
            value: 4,
        },
        Instruction::LoadI32 { dst: 5, value: 7 },
        Instruction::MapSet {
            source: 2,
            key: 5,
            value: 4,
        },
    ]
    .into_iter()
    .chain(tail)
    {
        cell.emit(instruction);
    }
    let mut cell = cell.finish().expect("heap rollback cell");
    cell.root_bitmap = vec![true, true, true, false, false, false];
    add_post_yield_safepoints(&mut cell);
    for root_map in &mut cell.root_maps {
        let pc = usize::try_from(root_map.pc).expect("fixture bytecode position fits usize");
        root_map.bitmap = match cell.code.get(pc) {
            Some(Instruction::LoadI32 { .. }) if pc == 0 => {
                vec![true, true, true, false, false, false]
            }
            Some(Instruction::ArrayPush { .. }) => {
                vec![false, true, true, false, false, false]
            }
            Some(Instruction::MapSet { .. }) => {
                vec![false, false, true, false, false, false]
            }
            _ => vec![false, false, false, false, false, false],
        };
    }
    let mut module = ModuleBuilder::new();
    module
        .metadata(
            HOST_CONTRACT_ID,
            nexa_bytecode::StateSchema::default().fingerprint(),
        )
        .array_type(ArrayType::new(ValueType::I32))
        .map_type(MapType::new(ValueType::I32, ValueType::I32))
        .class_type(ClassType {
            type_id: NESTED_CLASS_TYPE,
            fields: vec![StructField {
                stable_id: NESTED_CLASS_FIELD,
                ty: ValueType::I32,
            }],
        });
    let function = module.function(cell);
    module.script_export(ScriptExport {
        stable_id: CELL_ID,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified heap rollback module")
}

fn heap_rollback_realm() -> (
    RealmRuntime,
    ModuleHandle,
    nexa_runtime::ScopeHandle,
    nexa_runtime::GcRef,
    RuntimeValue,
    RuntimeValue,
) {
    let old_module = heap_rollback_module([Instruction::Trap]);
    let schema = old_module.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old_module, HOST_CONTRACT_ID, schema)
        .expect("old heap module");
    let class = realm
        .allocate_class(NESTED_CLASS_TYPE, &[RuntimeValue::I32(1)])
        .expect("committed class");
    let RuntimeValue::NamedRef {
        reference: class_reference,
        ..
    } = class
    else {
        panic!("class allocation returns a named reference");
    };
    let array_type = nexa_bytecode::array_type(ValueType::I32);
    let array = realm
        .allocate_array(array_type, ValueType::I32)
        .expect("committed array");
    let map_type = nexa_bytecode::map_type(ValueType::I32, ValueType::I32);
    let map = realm
        .allocate_map(map_type, ValueType::I32, ValueType::I32)
        .expect("committed map");
    assert!(
        matches!(array, RuntimeValue::NamedRef { .. }),
        "array is a named reference"
    );
    assert!(
        matches!(map, RuntimeValue::NamedRef { .. }),
        "map is a named reference"
    );
    let scope = realm.create_scope(None).expect("cell scope");
    (realm, old, scope, class_reference, array, map)
}

fn heap_rollback_arguments(
    class_reference: nexa_runtime::GcRef,
    array: RuntimeValue,
    map: RuntimeValue,
) -> [RuntimeValue; 3] {
    [
        RuntimeValue::NamedRef {
            type_id: NESTED_CLASS_TYPE,
            reference: class_reference,
        },
        array,
        map,
    ]
}

fn heap_rollback_entrypoint() -> TransactionalCellEntrypoint {
    let array_type = nexa_bytecode::array_type(ValueType::I32);
    let map_type = nexa_bytecode::map_type(ValueType::I32, ValueType::I32);
    TransactionalCellEntrypoint::new(
        CELL_ID,
        Signature {
            parameters: vec![
                ValueType::Named(NESTED_CLASS_TYPE),
                ValueType::Named(array_type),
                ValueType::Named(map_type),
            ],
            result: Some(ValueType::I32),
        },
    )
}

fn assert_committed_heap_unchanged(
    realm: &RealmRuntime,
    class_reference: nexa_runtime::GcRef,
    array: RuntimeValue,
    map: RuntimeValue,
) {
    let Object::Class { .. } = realm
        .resolve_heap_object(class_reference)
        .expect("committed class survives")
    else {
        panic!("committed object remains a class");
    };
    let fields = realm
        .resolve_heap_fields(class_reference)
        .expect("committed class fields survive");
    assert_eq!(fields.get(0), Some(RuntimeValue::I32(1)));
    assert_eq!(realm.array_length(array), Ok(0));
    assert_eq!(realm.map_length(map), Ok(0));
}

#[test]
fn failed_cells_restore_class_array_and_map_heap_mutations_exactly() {
    let (mut realm, old, scope, class_reference, array, map) = heap_rollback_realm();

    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                heap_rollback_module([Instruction::Trap]),
                &heap_rollback_entrypoint(),
                &heap_rollback_arguments(class_reference, array, map),
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage trapping heap cell");
        assert!(matches!(
            transaction.poll().expect_err("trap"),
            nexa_runtime::TransactionalCellFailure {
                cause: TransactionalCellFailureCause::Trapped(_),
                ..
            }
        ));
    }
    assert_committed_heap_unchanged(&realm, class_reference, array, map);

    let yielding = || heap_rollback_module([Instruction::Yield, Instruction::Return { source: 4 }]);
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                yielding(),
                &heap_rollback_entrypoint(),
                &heap_rollback_arguments(class_reference, array, map),
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage cancellable heap cell");
        assert!(matches!(
            transaction.poll(),
            Ok(TransactionalCellPoll::Yielded(_))
        ));
        transaction
            .cancel(CancelReason::HostCancelled)
            .expect("cancel heap cell");
    }
    assert_committed_heap_unchanged(&realm, class_reference, array, map);

    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                yielding(),
                &heap_rollback_entrypoint(),
                &heap_rollback_arguments(class_reference, array, map),
                RestartReloadPolicy::default(),
                step(scope),
            )
            .expect("stage dropped heap cell");
        assert!(matches!(
            transaction.poll(),
            Ok(TransactionalCellPoll::Yielded(_))
        ));
    }
    assert_committed_heap_unchanged(&realm, class_reference, array, map);

    let mut fuel_tail = vec![Instruction::Yield];
    fuel_tail.extend((0..256).map(|value| Instruction::LoadI32 { dst: 5, value }));
    fuel_tail.push(Instruction::Return { source: 4 });
    let mut fuel_step = step(scope);
    fuel_step.fuel_slice = 1_024;
    fuel_step.cumulative_budget = 64;
    {
        let mut transaction = realm
            .stage_cell_transaction(
                old,
                heap_rollback_module(fuel_tail),
                &heap_rollback_entrypoint(),
                &heap_rollback_arguments(class_reference, array, map),
                RestartReloadPolicy::default(),
                fuel_step,
            )
            .expect("stage fuel-limited heap cell");
        assert!(matches!(
            transaction.poll(),
            Ok(TransactionalCellPoll::Yielded(_))
        ));
        loop {
            match transaction.poll() {
                Err(failure) => {
                    assert!(matches!(
                        failure.cause,
                        TransactionalCellFailureCause::Cancelled(_)
                    ));
                    break;
                }
                Ok(TransactionalCellPoll::Yielded(_)) => {}
                Ok(other) => panic!("fuel-limited Cell became ready: {other:?}"),
            }
        }
    }
    assert_committed_heap_unchanged(&realm, class_reference, array, map);
    assert_eq!(realm.active_root(), Some(old));
    assert!(realm.resource_invariants_hold());
}
