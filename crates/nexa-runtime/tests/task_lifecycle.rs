use std::sync::{Arc, Mutex, OnceLock};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, ScriptExport, Signature, SourceMapEntry, StandardIntrinsic,
    StructField, StructType, ValueType,
};
use nexa_core::{FileId, SourceSpan, StableId, StateSchemaFingerprint};
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostCompletionResult, HostErrorPayload, HostFunctionAuthority,
    HostFunctionSlot, HostPayload, HostRegistry, HostRequestError, HostTrap, Object,
    PendingHostRequest, RealmConfig, RealmError, RealmRuntime, ReleaseKind, ResolvedHostFunction,
    ResourceContext, RuntimeError, RuntimeFailurePoint, RuntimeHost, RuntimeHostArgs, RuntimeValue,
    StepConfig, TaskHandle, TaskLimits, TaskPoll, TaskTerminalReason, TickBudget, YieldReason,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x5441_534b_484f_5354);
const ASYNC_EXPORT: StableId = StableId(0x544c_4153_594e_4301);
const NOMINAL_ASYNC_EXPORT: StableId = StableId(0x544c_4153_594e_4302);
const IMMEDIATE_EXPORT: StableId = StableId(0x544c_494d_4d45_4401);
const YIELDING_EXPORT: StableId = StableId(0x544c_5949_454c_4401);
const BUDGET_EXPORT: StableId = StableId(0x544c_4255_4447_4501);
fn schema() -> StateSchemaFingerprint {
    nexa_bytecode::StateSchema::default().fingerprint()
}

struct AsyncRegistry {
    contract_runtime_id: StableId,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
    panic: bool,
}

impl HostRegistry for AsyncRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        static REQUEST: OnceLock<HostFunctionAuthority> = OnceLock::new();
        static TYPED_ERROR: OnceLock<HostFunctionAuthority> = OnceLock::new();
        if id == StableId::from_name("TaskHost::request") {
            return Some(ResolvedHostFunction::new(
                HostFunctionSlot::new(0),
                REQUEST.get_or_init(|| {
                    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
                    HostFunctionAuthority::new(
                        id,
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
                }),
            ));
        }
        if id == StableId::from_name("TaskHost::typed_error") {
            return Some(ResolvedHostFunction::new(
                HostFunctionSlot::new(1),
                TYPED_ERROR.get_or_init(|| {
                    let success = ValueType::Named(StableId::from_name("Payload"));
                    let error = ValueType::Named(StableId::from_name("Failure"));
                    let result = nexa_bytecode::result_type(success, error);
                    HostFunctionAuthority::new(
                        id,
                        [0; 32],
                        &[],
                        Some(ValueType::Named(result.type_id)),
                        HostCallMode::Async,
                        1,
                        Some(AsyncResultType {
                            result_type: result.type_id,
                            success,
                            error,
                            cancel_policy: nexa_bytecode::CancelPolicy::CancelTask,
                            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
                            cancel_error: None,
                            abandon_error: None,
                        }),
                        &[],
                    )
                }),
            ));
        }
        None
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        assert!(!self.panic, "injected host panic");
        if slot.index() > 1 {
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
    let mut function = FunctionBuilder::new(signature.clone(), 2);
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
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false, false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![false, false],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema()).enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("TaskHost::request"),
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
        stable_id: ASYNC_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified async module")
}

fn async_nominal_result_module() -> VerifiedModule {
    let trace_type = StableId::from_name("Trace");
    let payload_type = StableId::from_name("Payload");
    let failure_type = StableId::from_name("Failure");
    let payload = ValueType::Named(payload_type);
    let failure = ValueType::Named(failure_type);
    let result = nexa_bytecode::result_type(payload, failure);
    let signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::Named(result.type_id)),
    };
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: payload,
        error: failure,
        cancel_policy: nexa_bytecode::CancelPolicy::CancelTask,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: None,
        abandon_error: None,
    };
    let mut function = FunctionBuilder::new(signature.clone(), 3);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut function = function.finish().expect("async nominal error function");
    function.root_bitmap[2] = true;
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false, false, false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![false, false, true],
        },
    ];
    let mut module = ModuleBuilder::new();
    module
        .metadata(HOST, schema())
        .opaque_type(trace_type)
        .struct_type(StructType {
            type_id: payload_type,
            fields: vec![
                StructField {
                    stable_id: StableId::from_parts(&["Payload", "::ticket"]),
                    ty: ValueType::Named(trace_type),
                },
                StructField {
                    stable_id: StableId::from_parts(&["Payload", "::label"]),
                    ty: ValueType::String,
                },
            ],
        })
        .struct_type(StructType {
            type_id: failure_type,
            fields: vec![
                StructField {
                    stable_id: StableId::from_parts(&["Failure", "::trace"]),
                    ty: ValueType::Named(trace_type),
                },
                StructField {
                    stable_id: StableId::from_parts(&["Failure", "::message"]),
                    ty: ValueType::String,
                },
            ],
        })
        .enum_type(result)
        .host_import(HostImport {
            stable_id: StableId::from_name("TaskHost::typed_error"),
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
        stable_id: NOMINAL_ASYNC_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified nominal-result module")
}

fn immediate_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    let function = module.function(function.finish().expect("immediate function"));
    module.script_export(ScriptExport {
        stable_id: IMMEDIATE_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified immediate module")
}

fn yielding_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut function = FunctionBuilder::new(signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    let function = module.function(function.finish().expect("yielding function"));
    module.script_export(ScriptExport {
        stable_id: YIELDING_EXPORT,
        function,
        signature,
        effect: FunctionEffect::Task,
    });
    verify(module.finish(), VerifierLimits::default()).expect("verified yielding module")
}

fn nested_budget_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::F64],
        result: Some(ValueType::F64),
    };
    let mut entry = FunctionBuilder::new(signature.clone(), 2);
    entry
        .effect(FunctionEffect::Task)
        .emit(Instruction::Call {
            function: 1,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::Return { source: 1 });
    let mut callee = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::F64],
            result: Some(ValueType::F64),
        },
        2,
    );
    callee
        .emit(Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::F64Sin,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::Return { source: 1 });
    let caller_call = SourceSpan::new(FileId(4), 10, 20);
    let caller_return = SourceSpan::new(FileId(4), 21, 27);
    let callee_intrinsic = SourceSpan::new(FileId(5), 40, 52);
    let mut module = ModuleBuilder::new();
    module.metadata(HOST, schema());
    let entry = module.function(entry.finish().expect("entry"));
    module.function(callee.finish().expect("callee"));
    module.script_export(ScriptExport {
        stable_id: BUDGET_EXPORT,
        function: entry,
        signature,
        effect: FunctionEffect::Task,
    });
    module.source_map([
        SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: caller_call,
        },
        SourceMapEntry {
            function: 0,
            pc_start: 1,
            pc_end: 2,
            span: caller_return,
        },
        SourceMapEntry {
            function: 1,
            pc_start: 0,
            pc_end: 1,
            span: callee_intrinsic,
        },
    ]);
    verify(module.finish(), VerifierLimits::default()).expect("verified budget module")
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

#[test]
fn budget_exhaustion_retains_exact_leaf_to_root_stack_and_final_charge() {
    let (mut realm, module, _, _) = hosted(nested_budget_module(), RealmConfig::default(), false);
    let scope = realm.create_scope(None).expect("scope");
    let task = realm
        .spawn_task(
            module,
            BUDGET_EXPORT,
            &[RuntimeValue::F64(0.5_f64.to_bits())],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 64,
                cumulative_budget: 8,
                limits: TaskLimits::default(),
            },
        )
        .expect("task");
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Cancelled(CancelReason::BudgetExceeded))
    );

    let terminal = realm.terminal_record(task).expect("terminal record");
    assert_eq!(
        terminal.reason,
        TaskTerminalReason::Cancelled(CancelReason::BudgetExceeded)
    );
    assert_eq!(terminal.final_charge.instructions, 1);
    assert_eq!(terminal.final_charge.fuel_used, 2);
    let stack = terminal
        .script_call_stack
        .as_ref()
        .expect("budget exhaustion stack")
        .as_slice();
    assert_eq!(
        stack,
        [
            nexa_runtime::ScriptFrame {
                function: 1,
                pc: 0,
                call_site_pc: None,
                source_span: Some(SourceSpan::new(FileId(5), 40, 52)),
            },
            nexa_runtime::ScriptFrame {
                function: 0,
                pc: 1,
                call_site_pc: Some(0),
                source_span: Some(SourceSpan::new(FileId(4), 10, 20)),
            },
        ]
    );
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
    let contract_runtime_id = module.module().host_contract_id.unwrap_or(HOST);
    let pending = Arc::new(Mutex::new(None));
    let host = RuntimeHost::new(64);
    let mut realm = RealmRuntime::hosted(
        config,
        host.clone(),
        Box::new(AsyncRegistry {
            contract_runtime_id,
            pending: Arc::clone(&pending),
            panic,
        }),
    )
    .expect("hosted realm");
    let module = realm
        .load_module(module, contract_runtime_id, schema())
        .expect("loaded module");
    (realm, module, host, pending)
}

fn spawn(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    export: StableId,
    arguments: &[RuntimeValue],
) -> TaskHandle {
    let scope = realm.create_scope(None).expect("scope");
    realm
        .spawn_task(module, export, arguments, config(scope))
        .expect("task")
}

#[derive(Clone, Copy, Debug)]
struct RetainedNominalResult {
    task: TaskHandle,
    result: nexa_runtime::GcRef,
    payload: nexa_runtime::GcRef,
    text: nexa_runtime::GcRef,
}

fn complete_nominal_result_and_collect(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    ticket: u64,
    label: &str,
) -> (RetainedNominalResult, nexa_runtime::CollectionStats) {
    let task = spawn(realm, module, NOMINAL_ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait for nominal result")
    else {
        panic!("nominal result task must wait for its Host request");
    };
    assert_eq!(
        realm
            .complete_request(
                request,
                HostCompletionResult::Success(HostPayload::structure([
                    HostPayload::Opaque(ticket),
                    HostPayload::String(label.into()),
                ])),
            )
            .expect("complete nominal result"),
        nexa_runtime::CompletionDisposition::Delivered
    );
    let report = realm
        .tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 64,
            collect_garbage: true,
        })
        .expect("complete and collect nominal result");
    assert_eq!(report.completed, 1);
    let collection = report.collection.expect("tick requested collection");

    let value = match &realm
        .terminal_record(task)
        .expect("completed task retains a tombstone")
        .reason
    {
        TaskTerminalReason::Completed(Some(value)) => *value,
        reason => panic!("unexpected nominal terminal reason: {reason:?}"),
    };
    let RuntimeValue::NamedRef {
        reference: result, ..
    } = value
    else {
        panic!("nominal task must return its Result object");
    };
    let Object::Enum {
        payload: Some(RuntimeValue::Struct {
            reference: payload, ..
        }),
        ..
    } = realm
        .resolve_heap_object(result)
        .expect("retained Result remains resolvable")
    else {
        panic!("Result::Ok must retain its Struct payload");
    };
    let payload = *payload;
    let Object::Struct { field_count, .. } = realm
        .resolve_heap_object(payload)
        .expect("retained Struct remains resolvable")
    else {
        panic!("Result payload must be a Struct");
    };
    assert_eq!(*field_count, 2);
    let fields = realm
        .resolve_heap_fields(payload)
        .expect("retained Struct fields remain resolvable");
    let RuntimeValue::String {
        reference: text, ..
    } = fields.get(1).expect("second struct field")
    else {
        panic!("Struct payload must retain its String field");
    };
    assert!(matches!(
        realm
            .resolve_heap_object(text)
            .expect("retained String remains resolvable"),
        Object::String(value) if value == label
    ));

    (
        RetainedNominalResult {
            task,
            result,
            payload,
            text,
        },
        collection,
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ExpectedReleases {
    requests: usize,
    tokens: usize,
    snapshots: usize,
}

impl ExpectedReleases {
    const NONE: Self = Self {
        requests: 0,
        tokens: 0,
        snapshots: 0,
    };
    const ONE_REQUEST: Self = Self {
        requests: 1,
        ..Self::NONE
    };
}

fn assert_terminal_invariants(
    realm: &mut RealmRuntime,
    runtime_host: &RuntimeHost,
    task: TaskHandle,
    expected: ExpectedReleases,
) {
    realm
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .expect("terminal flush");
    let releases = runtime_host.drain_releases();
    let ledger = realm.resource_ledger();
    assert_eq!(ledger.continuations, 0);
    assert_eq!(ledger.requests, 0);
    assert_eq!(ledger.tokens, 0);
    assert_eq!(ledger.snapshots, 0);
    assert_eq!(ledger.scheduler_tokens, 0);
    assert_eq!(ledger.completion_reservations, 0);
    assert_eq!(ledger.release_reservations, 0);
    assert!(realm.terminal_record(task).is_some());
    for (kind, expected_count) in [
        (ReleaseKind::HostRequest, expected.requests),
        (ReleaseKind::ResourceToken, expected.tokens),
        (ReleaseKind::Snapshot, expected.snapshots),
    ] {
        assert_eq!(
            releases.iter().filter(|record| record.kind == kind).count(),
            expected_count,
            "wrong release count for {kind:?}"
        );
    }
    assert_eq!(
        releases.len(),
        expected.requests + expected.tokens + expected.snapshots,
        "unexpected release kind"
    );
    assert!(runtime_host.drain_releases().is_empty());
    assert!(runtime_host.drain_releases().is_empty());
}

fn assert_request_error(
    result: Result<nexa_runtime::CompletionDisposition, RuntimeError>,
    expected: HostRequestError,
) {
    match result {
        Err(RuntimeError::Realm(error)) => {
            assert_eq!(*error, RealmError::Host(expected));
        }
        other => panic!("unexpected request result: {other:?}"),
    }
}

#[test]
fn normal_completion() {
    let (mut realm, module, host, _) = hosted(immediate_module(), RealmConfig::default(), false);
    let task = spawn(
        &mut realm,
        module,
        IMMEDIATE_EXPORT,
        &[RuntimeValue::I32(7)],
    );
    assert_eq!(
        realm.poll_task(task, 64).expect("poll"),
        TaskPoll::Completed(RuntimeValue::I32(7))
    );
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

#[test]
fn host_error_is_a_typed_completion() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("request handle must only come from Waiting");
    };
    assert_eq!(
        realm
            .complete_request(
                request,
                HostCompletionResult::Error(HostErrorPayload::Code(7)),
            )
            .expect("host error"),
        nexa_runtime::CompletionDisposition::Delivered
    );
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(_))
    ));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn host_error_preserves_the_declared_nominal_payload() {
    let (mut realm, module, host, _) =
        hosted(async_nominal_result_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, NOMINAL_ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("request handle must only come from Waiting");
    };
    realm
        .complete_request(
            request,
            HostCompletionResult::Error(HostErrorPayload::Value(HostPayload::structure([
                HostPayload::Opaque(41),
                HostPayload::String("typed failure".into()),
            ]))),
        )
        .expect("typed Host error");
    let TaskPoll::Completed(RuntimeValue::NamedRef { reference, type_id }) = realm
        .poll_task(task, 64)
        .expect("completed typed Host error")
    else {
        panic!("typed Host error must complete with Result::Err");
    };
    let Object::Enum {
        type_id: stored_result_type,
        tag,
        payload:
            Some(RuntimeValue::Struct {
                reference: failure,
                type_id: failure_type,
                ..
            }),
        ..
    } = realm.resolve_heap_object(reference).expect("Result object")
    else {
        panic!("typed Host error must materialize the verified Result and struct layouts");
    };
    assert_eq!(*stored_result_type, type_id);
    assert_eq!(*tag, 1);
    let Object::Struct {
        type_id: stored_failure_type,
        field_count,
        ..
    } = realm.resolve_heap_object(*failure).expect("Failure object")
    else {
        panic!("Result::Err payload must retain the nominal Failure struct");
    };
    assert_eq!(*stored_failure_type, *failure_type);
    assert_eq!(*field_count, 2);
    let fields = realm
        .resolve_heap_fields(*failure)
        .expect("Failure fields remain resolvable");
    assert_eq!(
        fields.get(0),
        Some(RuntimeValue::Opaque {
            value: 41,
            type_id: StableId::from_name("Trace"),
        })
    );
    assert!(matches!(fields.get(1), Some(RuntimeValue::String { .. })));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn host_success_preserves_the_declared_nominal_payload() {
    let (mut realm, module, host, _) =
        hosted(async_nominal_result_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, NOMINAL_ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("request handle must only come from Waiting");
    };
    realm
        .complete_request(
            request,
            HostCompletionResult::Success(HostPayload::structure([
                HostPayload::Opaque(73),
                HostPayload::String("typed success".into()),
            ])),
        )
        .expect("typed Host success");
    let TaskPoll::Completed(RuntimeValue::NamedRef { reference, type_id }) = realm
        .poll_task(task, 64)
        .expect("completed typed Host success")
    else {
        panic!("typed Host success must complete with Result::Ok");
    };
    let Object::Enum {
        type_id: stored_result_type,
        tag,
        payload:
            Some(RuntimeValue::Struct {
                reference: payload,
                type_id: payload_type,
                ..
            }),
        ..
    } = realm.resolve_heap_object(reference).expect("Result object")
    else {
        panic!("typed Host success must materialize the verified Result and struct layouts");
    };
    assert_eq!(*stored_result_type, type_id);
    assert_eq!(*tag, 0);
    let Object::Struct {
        type_id: stored_payload_type,
        field_count,
        ..
    } = realm.resolve_heap_object(*payload).expect("Payload object")
    else {
        panic!("Result::Ok payload must retain the nominal Payload struct");
    };
    assert_eq!(*stored_payload_type, *payload_type);
    assert_eq!(*field_count, 2);
    let fields = realm
        .resolve_heap_fields(*payload)
        .expect("Payload fields remain resolvable");
    assert_eq!(
        fields.get(0),
        Some(RuntimeValue::Opaque {
            value: 73,
            type_id: StableId::from_name("Trace"),
        })
    );
    assert!(matches!(fields.get(1), Some(RuntimeValue::String { .. })));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn completed_tombstones_root_nested_results_until_eviction() {
    let config = RealmConfig {
        tombstone_capacity: 1,
        ..RealmConfig::default()
    };
    let (mut realm, module, host, _) = hosted(async_nominal_result_module(), config, false);

    let (first, first_collection) =
        complete_nominal_result_and_collect(&mut realm, module, 11, "first retained result");
    assert_eq!(
        first_collection,
        nexa_runtime::CollectionStats {
            marked: 3,
            reclaimed: 0,
            live: 3,
        }
    );

    let eviction_module = realm
        .load_module(immediate_module(), HOST, schema())
        .expect("load tombstone eviction module");
    let eviction_task = spawn(
        &mut realm,
        eviction_module,
        IMMEDIATE_EXPORT,
        &[RuntimeValue::I32(7)],
    );
    let eviction_report = realm
        .tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 64,
            collect_garbage: true,
        })
        .expect("evict and collect the older tombstone");
    assert_eq!(eviction_report.completed, 1);
    assert_eq!(
        eviction_report.collection,
        Some(nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 3,
            live: 0,
        })
    );
    assert!(matches!(
        realm
            .terminal_record(eviction_task)
            .map(|record| &record.reason),
        Some(TaskTerminalReason::Completed(Some(RuntimeValue::I32(7))))
    ));
    assert_eq!(
        realm.collect_garbage().expect("repeat empty collection"),
        nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 0,
            live: 0,
        }
    );
    assert!(
        realm.terminal_record(first.task).is_none(),
        "capacity-one tombstones evict the older terminal result"
    );
    for reclaimed in [first.result, first.payload, first.text] {
        assert!(
            realm.resolve_heap_object(reclaimed).is_err(),
            "the evicted terminal object graph must be reclaimable"
        );
    }
    let _ = host.drain_releases();
    drop(realm);
    let _ = host.begin_close();
    host.try_finish_close().expect("close runtime host");
}

#[test]
fn completion_is_idempotent_and_releases_once() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("expected request");
    };
    assert_eq!(
        realm
            .complete_request(
                request,
                HostCompletionResult::Error(HostErrorPayload::Code(3)),
            )
            .expect("first completion"),
        nexa_runtime::CompletionDisposition::Delivered
    );
    assert_request_error(
        realm.complete_request(
            request,
            HostCompletionResult::Error(HostErrorPayload::Code(4)),
        ),
        HostRequestError::AlreadyCompleted,
    );
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(_))
    ));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn request_handle_errors_are_distinct() {
    let limits = RealmConfig {
        max_host_resources: 1,
        ..RealmConfig::default()
    };
    let (mut first, module, host, pending) = hosted(async_module(), limits, false);
    let first_task = spawn(&mut first, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(first_request) = first.poll_task(first_task, 64).expect("first wait")
    else {
        panic!("expected first request");
    };

    let (mut foreign, _, _, _) = hosted(
        immediate_module(),
        RealmConfig {
            realm_id: 2,
            ..RealmConfig::default()
        },
        false,
    );
    assert_request_error(
        foreign.complete_request(
            first_request,
            HostCompletionResult::Error(HostErrorPayload::Code(1)),
        ),
        HostRequestError::CrossRealmHostRequestHandle,
    );
    first
        .complete_request(
            first_request,
            HostCompletionResult::Error(HostErrorPayload::Code(1)),
        )
        .expect("complete first");
    let mut first_physical = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("first physical request");
    assert!(first_physical.ticket.cancelled().is_err());
    assert!(matches!(
        first.poll_task(first_task, 64),
        Ok(TaskPoll::Completed(_))
    ));

    let second_task = spawn(&mut first, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(second_request) = first.poll_task(second_task, 64).expect("second wait")
    else {
        panic!("expected second request");
    };
    first
        .complete_request(
            second_request,
            HostCompletionResult::Error(HostErrorPayload::Code(2)),
        )
        .expect("complete second");
    assert!(matches!(
        first.poll_task(second_task, 64),
        Ok(TaskPoll::Completed(_))
    ));
    assert_request_error(
        first.complete_request(
            first_request,
            HostCompletionResult::Error(HostErrorPayload::Code(9)),
        ),
        HostRequestError::StaleHostRequestHandle,
    );
    first
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .expect("flush releases");
    let releases = host.drain_releases();
    assert_eq!(
        releases
            .iter()
            .filter(|record| record.kind == ReleaseKind::HostRequest)
            .count(),
        2
    );
    assert!(host.drain_releases().is_empty());
}

#[test]
fn reload_detached_request_is_reported_distinctly() {
    let definition = async_module();
    let (mut realm, module, host, pending) =
        hosted(definition.clone(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("expected request");
    };
    assert!(matches!(
        realm
            .restart_reload(
                module,
                definition,
                nexa_runtime::RestartReloadPolicy::default()
            )
            .expect("reload"),
        nexa_runtime::RestartReloadOutcome::Committed(_)
    ));
    assert_request_error(
        realm.complete_request(
            request,
            HostCompletionResult::Error(HostErrorPayload::Code(8)),
        ),
        HostRequestError::DetachedByReload,
    );
    let mut physical = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("physical request");
    physical
        .ticket
        .complete(nexa_runtime::HostPayload::I32(5))
        .expect("late physical completion");
    realm
        .tick(TickBudget {
            max_tasks: 0,
            frame_fuel_budget: 0,
            collect_garbage: false,
        })
        .expect("discard late completion");
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn host_panic_is_isolated_as_a_trap() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), true);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

#[test]
fn task_cancel_returns_terminal_poll() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    assert!(matches!(
        realm.poll_task(task, 64).expect("wait"),
        TaskPoll::Waiting(_)
    ));
    assert_eq!(
        realm
            .cancel_task(task, CancelReason::HostCancelled)
            .expect("cancel"),
        TaskPoll::Cancelled(CancelReason::HostCancelled)
    );
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn request_abandon_traps_without_invalid_task_state() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("wait") else {
        panic!("expected host request");
    };
    realm.abandon_request(request).expect("abandon");
    assert!(matches!(
        realm.terminal_record(task).map(|record| &record.reason),
        Some(TaskTerminalReason::Trapped(_))
    ));
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::ONE_REQUEST);
}

#[test]
fn task_capacity_is_reported_at_admission() {
    let limits = nexa_runtime::RuntimeLimits {
        max_tasks: 1,
        max_scheduler_tokens: 1,
        ..nexa_runtime::RuntimeLimits::default()
    };
    let (mut realm, module, host, _) = hosted(
        yielding_module(),
        RealmConfig {
            runtime_limits: limits,
            ..RealmConfig::default()
        },
        false,
    );
    let first = spawn(&mut realm, module, YIELDING_EXPORT, &[RuntimeValue::I32(1)]);
    let scope = realm.create_scope(None).expect("second scope");
    assert!(
        realm
            .spawn_task(
                module,
                YIELDING_EXPORT,
                &[RuntimeValue::I32(2)],
                config(scope),
            )
            .is_err()
    );
    realm
        .cancel_task(first, CancelReason::RuntimeShutdown)
        .expect("cleanup first");
    assert_terminal_invariants(&mut realm, &host, first, ExpectedReleases::NONE);
}

#[test]
fn request_capacity_probe_is_consumed() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::RequestSlot);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    probe.require_consumed().expect("request scenario reached");
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

#[test]
fn completion_capacity_probe_is_consumed() {
    let (mut realm, module, host, _) = hosted(async_module(), RealmConfig::default(), false);
    let probe = realm
        .failure_injector()
        .arm_once(RuntimeFailurePoint::CompletionSlot);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Trapped(_))
    ));
    probe
        .require_consumed()
        .expect("completion scenario reached");
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

fn cleanup_realm() -> (
    RealmRuntime,
    nexa_runtime::ModuleHandle,
    RuntimeHost,
    StableId,
) {
    let contract = nexa_idl::parse(
        r"
            contract CleanupTask {
                nexa {
                    async fn work(value: i32) -> i32;
                }
            }
        ",
    )
    .expect("cleanup contract");
    let export = contract.nexa_functions[0].stable_id;
    let source = "
        fn finalize(value: i32) -> i32 { return value; }
        pub async fn work(value: i32) -> i32 {
            defer finalize(value);
            yield;
            let next: i32 = value + 1;
            return next;
        }
    ";
    let module = nexa_compiler::compile_with_contract(source, &contract).expect("cleanup module");
    let (realm, handle, host, _) = hosted(module, RealmConfig::default(), false);
    (realm, handle, host, export)
}

#[test]
fn cleanup_succeeds_and_balances_resources() {
    let (mut realm, module, host, export) = cleanup_realm();
    let scope = realm.create_scope(None).expect("cleanup scope");
    let task = realm
        .spawn_task(module, export, &[RuntimeValue::I32(1)], config(scope))
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
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

#[test]
fn cleanup_trap_probe_is_consumed() {
    let (mut realm, module, host, export) = cleanup_realm();
    let scope = realm.create_scope(None).expect("cleanup scope");
    let task = realm
        .spawn_task(module, export, &[RuntimeValue::I32(1)], config(scope))
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
    assert_terminal_invariants(&mut realm, &host, task, ExpectedReleases::NONE);
}

#[test]
fn realm_drop_releases_live_task_resources_once() {
    let (mut realm, module, host, pending) = hosted(async_module(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, ASYNC_EXPORT, &[]);
    assert!(matches!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Waiting(_))
    ));
    let mut physical_request = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("physical request");
    drop(realm);
    assert!(physical_request.ticket.cancelled().is_err());
    assert_eq!(host.pending_completions(), 0);
    let releases = host.drain_releases();
    assert_eq!(
        releases
            .iter()
            .filter(|record| record.kind == ReleaseKind::HostRequest)
            .count(),
        1
    );
    assert!(host.drain_releases().is_empty());
    assert!(host.drain_releases().is_empty());
}

#[test]
fn module_restart_cancels_old_task_and_starts_new_module() {
    let module_definition = yielding_module();
    let (mut realm, module, host, _) =
        hosted(module_definition.clone(), RealmConfig::default(), false);
    let task = spawn(&mut realm, module, YIELDING_EXPORT, &[RuntimeValue::I32(1)]);
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
    let new_task = spawn(
        &mut realm,
        candidate,
        YIELDING_EXPORT,
        &[RuntimeValue::I32(2)],
    );
    assert!(matches!(
        realm.poll_task(new_task, 64),
        Ok(TaskPoll::Yielded(_))
    ));
    realm
        .cancel_task(new_task, CancelReason::RuntimeShutdown)
        .expect("new task cleanup");
    assert_terminal_invariants(&mut realm, &host, new_task, ExpectedReleases::NONE);
}

#[test]
fn stale_and_cross_realm_task_handles_are_distinct() {
    let (mut first, module, _, _) = hosted(immediate_module(), RealmConfig::default(), false);
    let task = spawn(
        &mut first,
        module,
        IMMEDIATE_EXPORT,
        &[RuntimeValue::I32(1)],
    );
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
