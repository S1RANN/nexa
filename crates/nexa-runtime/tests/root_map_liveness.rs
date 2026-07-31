use std::collections::BTreeMap;

use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, RootMap, Signature, StateSchema,
    ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, HostCallOutcome, HostRegistry, HostTrap, Object, RealmConfig, RealmRuntime,
    ResourceContext, RuntimeHost, RuntimeHostArgs, RuntimeValue, StateObject, StateValue,
    StepConfig, TaskLimits, TaskPoll, YieldReason,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const HOST: StableId = StableId(0x524f_4f54_4d41_5053);

fn task_config(owner: nexa_runtime::ScopeHandle) -> StepConfig {
    StepConfig {
        owner,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    }
}

#[test]
fn compiler_branch_only_string_is_dead_at_joined_yield_during_realm_gc() {
    let source = r#"
        task fn branch_then_yield(condition: bool) -> i32 {
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
    let verified =
        nexa_compiler::compile_with_metadata(source, HOST).expect("compile branch-only root case");
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

    let schema = verified.module().state_schema_fingerprint;
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, HOST, schema)
        .expect("load compiler-produced module");
    let scope = realm.create_scope(None).expect("create scope");
    let task = realm
        .spawn_task(module, 0, &[RuntimeValue::Bool(true)], task_config(scope))
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

struct OpaqueHost {
    hash: StableId,
    ticket_type: StableId,
}

impl HostRegistry for OpaqueHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.hash)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        _context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::Opaque {
            value: 77,
            type_id: self.ticket_type,
        }))
    }
}

#[test]
fn compiler_host_opaque_remains_live_across_yield_and_realm_gc() {
    let contract = nexa_idl::parse(
        r"
            interface RootMapHost {
                opaque Ticket;
                sync fn issue() -> Ticket;
            }
        ",
    )
    .expect("parse Host opaque contract");
    let source = r"
        module root.map.opaque;
        import host as api;

        task fn hold_ticket() -> api.Ticket {
            let ticket: api.Ticket = api.issue();
            yield;
            return ticket;
        }
    ";
    let verified =
        nexa_compiler::compile_with_interface(source, &contract).expect("compile Host opaque task");
    let function = &verified.module().functions[0];
    let yield_pc = function
        .code
        .iter()
        .position(|instruction| matches!(instruction, Instruction::Yield))
        .expect("opaque task has a yield");
    let yield_pc = u32::try_from(yield_pc).expect("test bytecode position fits u32");
    assert!(
        function
            .root_maps
            .iter()
            .find(|root_map| root_map.pc == yield_pc)
            .expect("opaque yield has a root map")
            .bitmap
            .iter()
            .any(|is_root| *is_root),
        "the verifier-visible Named Host value is live at the yield"
    );

    let host_hash = nexa_idl::exact_hash(&contract);
    let schema = verified.module().state_schema_fingerprint;
    let ticket_type = StableId::from_name("Ticket");
    let runtime_host = RuntimeHost::new(16);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(OpaqueHost {
            hash: host_hash,
            ticket_type,
        }),
    )
    .expect("create hosted realm");
    let module = realm
        .load_module(verified, host_hash, schema)
        .expect("load Host opaque module");
    let scope = realm.create_scope(None).expect("create opaque scope");
    let task = realm
        .spawn_task(module, 0, &[], task_config(scope))
        .expect("spawn Host opaque task");

    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Yielded(YieldReason::Explicit))
    );
    assert_eq!(
        realm.collect_garbage().expect("collect opaque task"),
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
        .expect("close Host opaque runtime");
}

#[test]
fn compiler_state_handle_remains_live_across_yield_and_realm_gc() {
    let source = r"
        module root.map.state_handle;

        @stateful(1) class Model {
            value: i32;
        }

        task fn hold_handle(handle: StateHandle<Model>) -> StateHandle<Model> {
            yield;
            return handle;
        }
    ";
    let verified =
        nexa_compiler::compile_with_metadata(source, HOST).expect("compile StateHandle task");
    let function = &verified.module().functions[0];
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
    assert!(yield_roots[0]);
    assert!(yield_roots[1..].iter().all(|is_root| !is_root));
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
        .spawn_task(module, 0, &[runtime_handle], task_config(scope))
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
    function: u32,
    cancel: bool,
) {
    let scope = realm.create_scope(None).expect("create defer scope");
    let task = realm
        .spawn_task(
            module,
            function,
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
            bitmap: vec![input.is_reference()],
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
    module.function(parent_function);
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
    let source = r#"
        fn retain(value: string) -> string {
            return value;
        }

        task fn deferred(condition: bool) -> i32 {
            if condition {
                let captured: string = "captured only by defer";
                defer retain(captured);
            }
            yield;
            return 7;
        }
    "#;
    let verified =
        nexa_compiler::compile_with_metadata(source, HOST).expect("compile defer root task");
    let task_function = verified
        .module()
        .functions
        .iter()
        .position(|function| function.effect == FunctionEffect::Task)
        .expect("compiled task function");
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
    let schema = verified.module().state_schema_fingerprint;

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm
        .load_module(verified, HOST, schema)
        .expect("load defer root module");
    let task_function = u32::try_from(task_function).expect("test function index fits u32");
    exercise_defer_capture_cleanup(&mut realm, module, task_function, true);
    exercise_defer_capture_cleanup(&mut realm, module, task_function, false);
}

#[test]
fn nested_call_fuel_suspend_keeps_pre_call_string_until_scalar_return() {
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
            0,
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
            marked: 1,
            reclaimed: 0,
            live: 1,
        }
    );
    assert_eq!(
        realm.poll_task(task, 64),
        Ok(TaskPoll::Completed(RuntimeValue::I32(7)))
    );
    assert_eq!(
        realm.collect_garbage().expect("reclaim old destination"),
        nexa_runtime::CollectionStats {
            marked: 0,
            reclaimed: 1,
            live: 0,
        }
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
        .spawn_task(module, 0, &[RuntimeValue::I32(11)], task_config(scope))
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
        Ok(Object::String(value)) if value == "callee result"
    ));
}
