use std::collections::BTreeMap;

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, RootMap, ScriptExport, Signature,
    StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot, HostRegistry, HostTrap,
    Object, RealmConfig, RealmRuntime, ResolvedHostFunction, ResourceContext, RuntimeHost,
    RuntimeHostArgs, RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits, TaskPoll,
    YieldReason,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x524f_4f54_4d41_5053);
const NESTED_CALL_EXPORT: StableId = StableId(0x524f_4f54_4341_4c4c);
const STATE_HANDLE_EXPORT: StableId = StableId(0x524f_4f54_5354_4154);

fn task_config(owner: nexa_runtime::ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    }
}

fn compiler_task_export(
    verified: &VerifiedModule,
    function: usize,
    expected_name: &str,
) -> StableId {
    let function_index = u32::try_from(function).expect("test function index fits u32");
    let emitted_function = &verified.module().functions[function];
    let mut matching = verified.module().exports.iter().filter(|export| {
        export.function == function_index
            && export.signature == emitted_function.signature
            && export.effect == FunctionEffect::Task
    });
    let stable_id = matching
        .next()
        .unwrap_or_else(|| panic!("pub async `{expected_name}` has an exact bytecode export"))
        .stable_id;
    assert!(
        matching.next().is_none(),
        "pub async `{expected_name}` has exactly one bytecode export"
    );
    stable_id
}

fn export_compiled_task(
    verified: VerifiedModule,
    function: usize,
    stable_id: StableId,
) -> VerifiedModule {
    let mut module = verified.into_module();
    let function_index = u32::try_from(function).expect("test function index fits u32");
    let emitted_function = &module.functions[function];
    assert_eq!(emitted_function.effect, FunctionEffect::Task);
    module.exports.push(ScriptExport {
        stable_id,
        function: function_index,
        signature: emitted_function.signature.clone(),
        effect: emitted_function.effect,
    });
    verify(module, VerifierLimits::default()).expect("verify explicit test ScriptExport")
}

#[test]
fn compiler_branch_only_string_is_dead_at_joined_yield_during_realm_gc() {
    let contract = nexa_contract::parse_contract(
        r"
            contract BranchRootMap;
            nexa {
                async fn branch_then_yield(condition: bool) -> i32;
            }
        ",
    )
    .expect("parse branch root-map contract");
    let source = r#"
        pub async fn branch_then_yield(condition: bool) -> i32 {
            if condition {
                let branch_only: string = "dead after the join";
                if branch_only.byte_len() == 0 {
                    return 1;
                }
            }
            yield;
            return 7;
        }
    "#;
    let verified = nexa_compiler::compile_with_contract(source, &contract)
        .expect("compile branch-only root case");
    let task_export = compiler_task_export(&verified, 0, "branch_then_yield");
    let function = &verified.module().functions[0];
    assert!(
        function.root_bitmap.iter().any(|is_root| *is_root),
        "compiler must allocate a reference register for the branch-only string"
    );
    let yield_pc = function
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Yield))
        .expect("compiled task has a yield");
    let yield_pc = u32::try_from(yield_pc).expect("test bytecode position fits u32");
    let yield_roots = function
        .root_maps
        .iter()
        .find(|root_map| root_map.pc == yield_pc)
        .expect("yield has an exact root map");
    assert!(
        yield_roots.bitmap.iter().all(|is_root| !is_root),
        "a reference initialized on only one predecessor is dead after the join"
    );

    let contract_runtime_id = nexa_contract::contract_runtime_id(&contract);
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, contract_runtime_id, schema)
        .expect("load compiler-produced module");
    let scope = realm.create_scope(None).expect("create scope");
    let task = realm
        .spawn_task(
            module,
            task_export,
            &[RuntimeValue::Bool(true)],
            task_config(scope),
        )
        .expect("spawn branch-only task");

    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        realm
            .collect_garbage()
            .expect("collect joined continuation"),
        nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 1,
            live: 0,
        }
    );
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(RuntimeValue::I32(7)))
    );
}

struct HandleHost {
    contract_runtime_id: StableId,
    ticket_type: StableId,
    authority: HostFunctionAuthority,
}

impl HostRegistry for HandleHost {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        (id == self.authority.stable_id()).then_some(ResolvedHostFunction::new(
            HostFunctionSlot::new(0),
            &self.authority,
        ))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        if !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::Opaque {
            value: 77,
            type_id: self.ticket_type,
        }))
    }
}

#[test]
fn compiler_host_handle_remains_live_across_yield_and_realm_gc() {
    let contract = nexa_contract::parse_contract(
        r"
            contract RootMapHost;
            handle Ticket;
            host {
                fn issue() -> Ticket;
            }
            nexa {
                async fn hold_ticket() -> Ticket;
            }
        ",
    )
    .expect("parse Host handle contract");
    let source = r"
        use host::root_map_host as api;

        pub async fn hold_ticket() -> api::Ticket {
            let ticket: api::Ticket = api::issue();
            yield;
            return ticket;
        }
    ";
    let verified =
        nexa_compiler::compile_with_contract(source, &contract).expect("compile Host handle task");
    let task_export = compiler_task_export(&verified, 0, "hold_ticket");
    let function = &verified.module().functions[0];
    let yield_pc = function
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Yield))
        .expect("handle task has a yield");
    let yield_pc = u32::try_from(yield_pc).expect("test bytecode position fits u32");
    let yield_roots = &function
        .root_maps
        .iter()
        .find(|root_map| root_map.pc == yield_pc)
        .expect("handle yield has a root map")
        .bitmap;
    assert!(
        yield_roots.iter().all(|is_root| !is_root),
        "Host handles are registry-owned scalar slots, not GC references"
    );

    let contract_fingerprint = nexa_contract::contract_fingerprint(&contract);
    let contract_runtime_id = nexa_contract::contract_runtime_id(&contract);
    assert_eq!(
        contract_runtime_id,
        nexa_runtime::contract_runtime_id_from_fingerprint(contract_fingerprint.into_bytes())
    );
    let schema = verified.module().state_schema_fingerprint;
    let ticket_type = contract.handles[0].stable_id;
    let host_function = verified.module().host_imports[0].clone();
    assert!(host_function.parameters.is_empty());
    assert!(host_function.capabilities.is_empty());
    let authority = HostFunctionAuthority::from_import(&host_function);
    let runtime_host = RuntimeHost::new(16);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(HandleHost {
            contract_runtime_id,
            ticket_type,
            authority,
        }),
    )
    .expect("create hosted realm");
    let module = realm
        .load_module(verified, contract_runtime_id, schema)
        .expect("load Host handle module");
    let scope = realm.create_scope(None).expect("create handle scope");
    let task = realm
        .spawn_task(module, task_export, &[], task_config(scope))
        .expect("spawn Host handle task");

    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        realm.collect_garbage().expect("collect handle task"),
        nexa_runtime::CollectionStats::default()
    );
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(RuntimeValue::Opaque {
            value: 77,
            type_id: ticket_type,
        }))
    );
    drop(realm);
    let _ = runtime_host.begin_close();
    runtime_host
        .try_finish_close()
        .expect("close Host handle runtime");
}

#[test]
fn compiler_state_handle_remains_live_across_yield_and_realm_gc() {
    let source = r"
        @state(version = 1)
        pub class Model {
            mut value: i32,
        }

        pub async fn hold_handle(handle: StateHandle<Model>) -> StateHandle<Model> {
            yield;
            return handle;
        }
    ";
    let verified =
        nexa_compiler::compile_with_contract_id(source, HOST).expect("compile StateHandle task");
    let task_function = verified
        .module()
        .functions
        .iter()
        .position(|function| function.effect == FunctionEffect::Task)
        .expect("compiled StateHandle task function");
    let verified = export_compiled_task(verified, task_function, STATE_HANDLE_EXPORT);
    let task_export = compiler_task_export(&verified, task_function, "hold_handle");
    let function = &verified.module().functions[task_function];
    let handle_type = function.signature.parameters[0];
    let yield_pc = function
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Yield))
        .expect("StateHandle task has a yield");
    let yield_pc = u32::try_from(yield_pc).expect("test bytecode position fits u32");
    let yield_roots = &function
        .root_maps
        .iter()
        .find(|root_map| root_map.pc == yield_pc)
        .expect("StateHandle yield has a root map")
        .bitmap;
    assert!(
        yield_roots.iter().all(|is_root| !is_root),
        "StateHandle values are rooted by the state registry, not frame GC maps"
    );
    let state_type = verified.module().state_schema.types[0].clone();
    let field_id = state_type.fields[0].stable_id;
    let schema = verified.module().state_schema_fingerprint;

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, HOST, schema)
        .expect("load StateHandle module");
    let handle = realm
        .insert_state(
            module,
            StableId::from_name("root-map-model"),
            StateValue::Object(StateObject {
                type_id: state_type.stable_id,
                version: state_type.version,
                fields: BTreeMap::from([(field_id, StateValue::I32(9))]),
            }),
        )
        .expect("insert typed state");
    let runtime_handle = realm
        .state_handle_value(module, handle)
        .expect("materialize StateHandle runtime value");
    assert!(matches!(
        runtime_handle,
        RuntimeValue::StateHandle {
            handle_type: actual,
            ..
        } if nexa_bytecode::ValueType::Named(actual) == handle_type
    ));

    let scope = realm.create_scope(None).expect("create StateHandle scope");
    let task = realm
        .spawn_task(module, task_export, &[runtime_handle], task_config(scope))
        .expect("spawn StateHandle task");
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        realm.collect_garbage().expect("collect StateHandle task"),
        nexa_runtime::CollectionStats::default()
    );
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(runtime_handle))
    );
}

fn exercise_defer_capture_cleanup(
    realm: &mut RealmRuntime,
    module: nexa_runtime::ModuleHandle,
    export: StableId,
    cancel: bool,
) {
    let scope = realm.create_scope(None).expect("create defer scope");
    let task = realm
        .spawn_task(
            module,
            export,
            &[RuntimeValue::Bool(true)],
            task_config(scope),
        )
        .expect("spawn defer task");
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        realm.collect_garbage().expect("collect defer continuation"),
        nexa_runtime::CollectionStats {
            marked: 1,
            reclaimed: 0,
            live: 1,
        }
    );
    if cancel {
        assert_eq!(
            realm.cancel_task(task, CancelReason::HostCancelled),
            Ok(TaskPoll::Cancelled(CancelReason::HostCancelled))
        );
    } else {
        assert_eq!(
            realm.poll_task(task, 64),
            Ok(TaskPoll::Completed(RuntimeValue::I32(7)))
        );
    }
    assert_eq!(
        realm.collect_garbage().expect("reclaim defer capture"),
        nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 1,
            live: 0,
        }
    );
}

fn nested_call_transition_module(input: ValueType, result: ValueType) -> VerifiedModule {
    let mut parent_function = FunctionBuilder::new(
        Signature {
            parameters: vec![input],
            result: Some(result),
        },
        1,
    );
    parent_function
        .effect(FunctionEffect::Task)
        .emit(Instruction::Call {
            function: 1,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    if input.is_reference() || result.is_reference() {
        parent_function.set_root(0).unwrap();
    }
    let mut parent_function = parent_function.finish().unwrap();
    parent_function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![result.is_reference()],
        },
    ];

    let mut child_function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(result),
        },
        1,
    );
    match result {
        ValueType::I32 => {
            child_function.emit(Instruction::LoadI32 { dst: 0, value: 7 });
        }
        ValueType::String => {
            child_function.set_root(0).unwrap();
            child_function.emit(Instruction::LoadString { dst: 0, string: 0 });
        }
        _ => panic!("test helper supports only i32 and string results"),
    }
    child_function.emit(Instruction::Return { source: 0 });
    let mut child_function = child_function.finish().unwrap();
    child_function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![result.is_reference()],
        },
    ];

    let mut module = ModuleBuilder::new();
    module.metadata(HOST, StateSchema::default().fingerprint());
    if result == ValueType::String {
        module.string("callee result");
    }
    let parent_signature = parent_function.signature.clone();
    let parent = module.function(parent_function);
    module.script_export(ScriptExport {
        stable_id: NESTED_CALL_EXPORT,
        function: parent,
        signature: parent_signature,
        effect: FunctionEffect::Task,
    });
    module.function(child_function);
    verify(module.finish(), VerifierLimits::default()).expect("verify nested call transition")
}

fn nested_call_realm(verified: VerifiedModule) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, HOST, schema)
        .expect("load nested call transition");
    (realm, module)
}

#[test]
fn compiler_defer_capture_is_a_gc_root_for_cancel_and_complete_cleanup() {
    let contract = nexa_contract::parse_contract(
        r"
            contract DeferRootMap;
            nexa {
                async fn deferred(condition: bool) -> i32;
            }
        ",
    )
    .expect("parse defer root-map contract");
    let source = r#"
        fn retain(value: string) -> string {
            return value;
        }

        pub async fn deferred(condition: bool) -> i32 {
            if condition {
                let captured: string = "captured only by defer";
                defer retain(captured);
            }
            yield;
            return 7;
        }
    "#;
    let verified =
        nexa_compiler::compile_with_contract(source, &contract).expect("compile defer root task");
    let task_function = verified
        .module()
        .functions
        .iter()
        .position(|function| function.effect == FunctionEffect::Task)
        .expect("compiled task function");
    let task_export = compiler_task_export(&verified, task_function, "deferred");
    let function = &verified.module().functions[task_function];
    assert!(function.root_bitmap.iter().any(|is_root| *is_root));
    let yield_pc = function
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Yield))
        .expect("defer task has a yield");
    let yield_pc = u32::try_from(yield_pc).expect("test bytecode position fits u32");
    assert!(
        function
            .root_maps
            .iter()
            .find(|root_map| root_map.pc == yield_pc)
            .expect("defer yield has a root map")
            .bitmap
            .iter()
            .all(|is_root| !is_root),
        "the branch-only register is dead at the join; only the defer record owns the string"
    );
    let contract_runtime_id = nexa_contract::contract_runtime_id(&contract);
    let schema = verified.module().state_schema_fingerprint;

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, contract_runtime_id, schema)
        .expect("load defer root module");
    exercise_defer_capture_cleanup(&mut realm, module, task_export, true);
    exercise_defer_capture_cleanup(&mut realm, module, task_export, false);
}

#[test]
fn nested_call_fuel_suspend_reclaims_dead_pre_call_string_before_scalar_return() {
    let (mut realm, module) = nested_call_realm(nested_call_transition_module(
        ValueType::String,
        ValueType::I32,
    ));
    let reference = realm
        .allocate(Object::String("pre-call destination".into()))
        .expect("allocate caller string");
    let scope = realm.create_scope(None).expect("create nested call scope");
    let task = realm
        .spawn_task(
            module,
            NESTED_CALL_EXPORT,
            &[RuntimeValue::String { reference, hash: 0 }],
            task_config(scope),
        )
        .expect("spawn string-to-scalar call");

    assert_eq!(
        realm.poll_task(task, 1),
        Ok(TaskPoll::Yielded(YieldReason::Fuel))
    );
    assert_eq!(
        realm
            .collect_garbage()
            .expect("collect suspended string-to-scalar call"),
        nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 1,
            live: 0,
        }
    );
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(RuntimeValue::I32(7)))
    );
    assert_eq!(
        realm.collect_garbage().expect("reclaim old destination"),
        nexa_runtime::CollectionStats::default()
    );
}

#[test]
fn nested_call_fuel_suspend_uses_pre_call_scalar_map_until_string_return() {
    let (mut realm, module) = nested_call_realm(nested_call_transition_module(
        ValueType::I32,
        ValueType::String,
    ));
    let scope = realm.create_scope(None).expect("create nested call scope");
    let task = realm
        .spawn_task(
            module,
            NESTED_CALL_EXPORT,
            &[RuntimeValue::I32(11)],
            task_config(scope),
        )
        .expect("spawn scalar-to-string call");

    assert_eq!(
        realm.poll_task(task, 1),
        Ok(TaskPoll::Yielded(YieldReason::Fuel))
    );
    assert_eq!(
        realm
            .collect_garbage()
            .expect("collect suspended scalar-to-string call"),
        nexa_runtime::CollectionStats::default()
    );
    let TaskPoll::Completed(RuntimeValue::String { reference, .. }) = realm
        .poll_task(task, 64)
        .expect("resume string result call")
    else {
        panic!("callee string result must atomically replace the caller destination");
    };
    assert!(matches!(
        realm.resolve_heap_object(reference),
        Ok(Object::SharedString(value)) if value.as_ref() == "callee result"
    ));
}
