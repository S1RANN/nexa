#![allow(deprecated)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, EnumType, EnumVariant, Function, FunctionBuilder,
    FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder, RootMap, Signature,
    StateField, StateSchema, StateType, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CancelReason, CopyBuffer, Heap, HeapError, HostArgs, HostCallOutcome, HostErrorPayload,
    HostPayload, HostRegistry, HostTrap, HostValue, MigrationAllocationPhase, Object,
    PendingHostRequest, PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime,
    ReleaseKind, ReleaseRecord, ResourceContext, RuntimeHost, RuntimeHostArgs, RuntimeHostDomain,
    RuntimeLimits, RuntimeResources, RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits,
    TaskRuntime, TaskState, TickBudget, set_migration_allocation_observer,
};
use nexa_verifier::{VerifierLimits, verify};

struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static HOST_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static FIRST_OPCODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static MIGRATION_COUNTS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

thread_local! {
    static ALLOCATION_OBSERVATION_ENABLED: Cell<bool> = const { Cell::new(false) };
    static HOST_ALLOCATION_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

fn allocation_observation_enabled() -> bool {
    ALLOCATION_OBSERVATION_ENABLED.get()
}

fn set_allocation_observation(enabled: bool) {
    ALLOCATION_OBSERVATION_ENABLED.set(enabled);
}

fn host_allocation_active() -> bool {
    HOST_ALLOCATION_ACTIVE.get()
}

fn set_host_allocation_active(active: bool) {
    HOST_ALLOCATION_ACTIVE.set(active);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if allocation_observation_enabled() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            if host_allocation_active() {
                HOST_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if allocation_observation_enabled() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            if host_allocation_active() {
                HOST_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if allocation_observation_enabled() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            if host_allocation_active() {
                HOST_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn observed(operation: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    set_allocation_observation(true);
    operation();
    set_allocation_observation(false);
    ALLOCATIONS.load(Ordering::SeqCst)
}

fn observed_host_split<T>(operation: impl FnOnce() -> T) -> (T, u64, u64) {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    HOST_ALLOCATIONS.store(0, Ordering::SeqCst);
    set_allocation_observation(true);
    let result = operation();
    set_allocation_observation(false);
    let total = ALLOCATIONS.load(Ordering::SeqCst);
    let host = HOST_ALLOCATIONS.load(Ordering::SeqCst);
    (result, total.saturating_sub(host), host)
}

fn host_owned<T>(operation: impl FnOnce() -> T) -> T {
    struct HostAllocationGuard;
    impl Drop for HostAllocationGuard {
        fn drop(&mut self) {
            set_host_allocation_active(false);
        }
    }
    set_host_allocation_active(true);
    let guard = HostAllocationGuard;
    let result = operation();
    drop(guard);
    result
}

#[allow(
    dead_code,
    clippy::extra_unused_lifetimes,
    clippy::identity_op,
    clippy::needless_question_mark,
    clippy::too_many_arguments
)]
mod host_matrix {
    include!(concat!(env!("OUT_DIR"), "/host_matrix.rs"));
}

fn release_buffer<const N: usize>() -> [ReleaseRecord; N] {
    [ReleaseRecord {
        realm_id: 0,
        module_id: 0,
        epoch: 0,
        kind: ReleaseKind::HostRequest,
        object_id: 0,
        domain: RuntimeHostDomain::VmThread,
    }; N]
}

fn migration_observer(
    phase: MigrationAllocationPhase,
    boundary: nexa_runtime::AllocationBoundary,
) {
    let index = migration_phase_index(phase);
    match boundary {
        nexa_runtime::AllocationBoundary::Begin => {
            if phase == MigrationAllocationPhase::FirstOpcode {
                FIRST_OPCODE_ACTIVE.store(true, Ordering::SeqCst);
                ALLOCATIONS.store(0, Ordering::SeqCst);
                set_allocation_observation(true);
            } else if !FIRST_OPCODE_ACTIVE.load(Ordering::SeqCst) {
                ALLOCATIONS.store(0, Ordering::SeqCst);
                set_allocation_observation(true);
            }
        }
        nexa_runtime::AllocationBoundary::End => {
            MIGRATION_COUNTS[index].store(ALLOCATIONS.load(Ordering::SeqCst), Ordering::SeqCst);
            if phase == MigrationAllocationPhase::FirstOpcode {
                set_allocation_observation(false);
                FIRST_OPCODE_ACTIVE.store(false, Ordering::SeqCst);
            } else if !FIRST_OPCODE_ACTIVE.load(Ordering::SeqCst) {
                set_allocation_observation(false);
            }
        }
    }
}

const fn migration_phase_index(phase: MigrationAllocationPhase) -> usize {
    match phase {
        MigrationAllocationPhase::ContextConstruction => 0,
        MigrationAllocationPhase::FirstOpcode => 1,
        MigrationAllocationPhase::OldGet => 2,
        MigrationAllocationPhase::OldFieldGet => 3,
        MigrationAllocationPhase::NewCreate => 4,
        MigrationAllocationPhase::NewSet => 5,
        MigrationAllocationPhase::Preserve => 6,
        MigrationAllocationPhase::Replace => 7,
        MigrationAllocationPhase::Delete => 8,
        MigrationAllocationPhase::StateFinish => 9,
        MigrationAllocationPhase::Finish => 10,
    }
}

fn migration_count(phase: MigrationAllocationPhase) -> u64 {
    MIGRATION_COUNTS[migration_phase_index(phase)].load(Ordering::SeqCst)
}

#[derive(Clone, Copy)]
enum ObservedCompletion {
    Error(u32),
    Cancelled,
    Abandoned,
    HeapFullSuccess,
}

fn observe_typed_writeback(spec: AsyncObserverSpec, completion: ObservedCompletion) -> u64 {
    let host = RuntimeHost::new(8);
    let pending = Arc::new(Mutex::new(None));
    let config = RealmConfig {
        max_heap_objects: if matches!(completion, ObservedCompletion::HeapFullSuccess) {
            0
        } else {
            8
        },
        ..RealmConfig::default()
    };
    let (mut realm, module) =
        make_async_host_realm_with_spec(config, host.clone(), Arc::clone(&pending), spec);
    drop(pending.lock().unwrap());
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .call(
            module,
            0,
            &[RuntimeValue::I32(11)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 16,
                cumulative_budget: 128,
                limits: TaskLimits::default(),
            },
        )
        .unwrap();
    assert_eq!(
        realm.poll_task(task, 64).unwrap(),
        PollResult::Pending(PendingReason::HostRequest)
    );
    let mut ticket = pending.lock().unwrap().take().unwrap().ticket;
    match completion {
        ObservedCompletion::Error(code) => {
            ticket.fail(HostErrorPayload { code }).unwrap();
        }
        ObservedCompletion::Cancelled => {
            ticket.cancelled().unwrap();
        }
        ObservedCompletion::Abandoned => {
            ticket.abandon().unwrap();
        }
        ObservedCompletion::HeapFullSuccess => {
            ticket.complete(HostPayload::I32(12)).unwrap();
        }
    }
    let allocations = observed(|| {
        let tick = realm.tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 64,
            collect_garbage: false,
        });
        if matches!(completion, ObservedCompletion::HeapFullSuccess) {
            assert_eq!(tick, Err(RealmError::Heap(HeapError::CapacityExhausted)));
        } else {
            tick.unwrap();
        }
    });
    if matches!(completion, ObservedCompletion::HeapFullSuccess) {
        assert_eq!(realm.task_snapshot(task).unwrap().state, TaskState::Waiting);
        assert!(realm.resource_invariants_hold());
        let ledger = realm.resource_ledger();
        assert_eq!(ledger.requests, 1);
        assert_eq!(ledger.completion_reservations, 1);
        realm
            .cancel_task(task, CancelReason::ScopeCancelled)
            .unwrap();
    }
    drop(realm);
    let _ = host.drain_releases();
    let _ = host.begin_close();
    host.try_finish_close().unwrap();
    allocations
}

fn make_cleanup_realm(cleanup_traps: bool) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host = StableId::from_name("allocation-observer-cleanup-host");
    let schema = StableId::from_name("allocation-observer-cleanup-schema");
    let mut cleanup = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    cleanup.effect(FunctionEffect::Cleanup);
    if cleanup_traps {
        cleanup.emit(Instruction::Trap);
    } else {
        cleanup.emit(Instruction::CleanupReturn);
    }
    let mut task = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    task.effect(FunctionEffect::Task)
        .emit(Instruction::DeferPush {
            function: 0,
            args_base: 0,
            args_count: 0,
        })
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut builder = ModuleBuilder::new();
    builder.metadata(host, schema);
    builder.function(cleanup.finish().unwrap());
    builder.function(task.finish().unwrap());
    let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    (realm, module)
}

fn observe_cleanup(cleanup_traps: bool) -> u64 {
    let (mut realm, module) = make_cleanup_realm(cleanup_traps);
    let scope = realm.create_scope(None).unwrap();
    let task = realm
        .call(
            module,
            1,
            &[RuntimeValue::I32(19)],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 16,
                cumulative_budget: 128,
                limits: TaskLimits::default(),
            },
        )
        .unwrap();
    assert_eq!(
        realm.poll_task(task, 64).unwrap(),
        PollResult::Pending(PendingReason::ExplicitYield)
    );
    let allocations = observed(|| {
        realm
            .cancel_task(task, CancelReason::ScopeCancelled)
            .unwrap();
    });
    assert_eq!(
        realm.terminal_record(task).unwrap().state,
        if cleanup_traps {
            TaskState::Trapped
        } else {
            TaskState::Cancelled
        }
    );
    allocations
}

struct MatrixHost;

impl host_matrix::AllocationMatrixHost for MatrixHost {
    fn inspect<'a>(
        &mut self,
        _: &mut ResourceContext<'_>,
        name: &'a str,
        record: host_matrix::RecordRef<'a>,
        event: host_matrix::EventRef<'a>,
        option: Option<host_matrix::RecordRef<'a>>,
        result: Result<host_matrix::RecordRef<'a>, host_matrix::EventRef<'a>>,
        array: nexa_runtime::HostArrayRef<'a>,
        buffer: nexa_runtime::HostBufferRef<'a>,
        nested: host_matrix::EventRef<'a>,
    ) -> Result<i32, host_matrix::HostError> {
        Ok(host_owned(|| {
            let _ = (name, record, event, option, result, array, buffer, nested);
            1
        }))
    }

    fn inspect_scalar_collections<'a>(
        &mut self,
        _: &mut ResourceContext<'_>,
        array: nexa_runtime::HostArrayRef<'a>,
        buffer: nexa_runtime::HostBufferRef<'a>,
    ) -> Result<i32, host_matrix::HostError> {
        let mut total = 0;
        for value in array.iter().chain(buffer.iter()) {
            total += value.i32().map_err(|_| host_matrix::HostError(String::new()))?;
        }
        Ok(total)
    }

    fn return_string(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<String, host_matrix::HostError> {
        Ok(String::new())
    }

    fn return_struct(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<host_matrix::Record, host_matrix::HostError> {
        Ok(host_matrix::Record {
            label: host_owned(|| "record".to_owned()),
            value: 7,
        })
    }

    fn return_enum(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<host_matrix::Event, host_matrix::HostError> {
        Ok(host_matrix::Event::Record(host_matrix::Record {
            label: host_owned(|| "event".to_owned()),
            value: 11,
        }))
    }

    fn return_option(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<Option<host_matrix::Record>, host_matrix::HostError> {
        Ok(Some(host_matrix::Record {
            label: host_owned(|| "option".to_owned()),
            value: 13,
        }))
    }

    fn return_result(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<Result<host_matrix::Record, host_matrix::Event>, host_matrix::HostError> {
        Ok(Ok(host_matrix::Record {
            label: host_owned(|| "result".to_owned()),
            value: 17,
        }))
    }

    fn return_array(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<Vec<i32>, host_matrix::HostError> {
        Ok(Vec::new())
    }

    fn return_buffer(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<CopyBuffer<i32>, host_matrix::HostError> {
        Ok(CopyBuffer::new(Vec::new()))
    }

    fn return_scalar(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<i32, host_matrix::HostError> {
        Ok(1)
    }

    fn baseline8(
        &mut self,
        _: &mut ResourceContext<'_>,
        a: i32,
        b: i32,
        c: i32,
        d: i32,
        e: i32,
        f: i32,
        g: i32,
        h: i32,
    ) -> Result<i32, host_matrix::HostError> {
        Ok(a + b + c + d + e + f + g + h)
    }

    fn panic_host(
        &mut self,
        _: &mut ResourceContext<'_>,
    ) -> Result<i32, host_matrix::HostError> {
        host_owned(|| panic!("matrix host panic"))
    }
}

#[allow(clippy::too_many_lines)]
fn complex_host_allocation_matrix() {
    let mut tasks = TaskRuntime::new(1, RuntimeLimits::default());
    let scope = tasks.create_scope(None).unwrap();
    let task = tasks.admit_task(scope, 1, true).unwrap();
    let mut resources = RuntimeResources::new(1, 4, 4);
    let mut context = resources.context(task, 0, 1);
    let mut registry = host_matrix::GeneratedHostRegistry::new(MatrixHost);
    let mut heap = Heap::new(64);
    let string_reference = heap.allocate_string("nested").unwrap();
    let string = RuntimeValue::String {
        reference: string_reference,
        hash: heap.string_hash(string_reference).unwrap(),
    };
    let record_type = StableId::from_name("Record");
    let record = heap
        .allocate_struct(record_type, &[string, RuntimeValue::I32(5)])
        .unwrap();
    let event_type = StableId::from_name("Event");
    let event = heap
        .allocate_enum(
            event_type,
            StableId::from_parts(&["Event", "::", "Record"]),
            1,
            Some(record),
        )
        .unwrap();
    let option = nexa_bytecode::option_type(ValueType::Named(record_type));
    let option_value = heap
        .allocate_enum(
            option.type_id,
            option.variants[1].stable_id,
            option.variants[1].tag,
            Some(record),
        )
        .unwrap();
    let result = nexa_bytecode::result_type(ValueType::Named(record_type), ValueType::Named(event_type));
    let result_value = heap
        .allocate_enum(
            result.type_id,
            result.variants[0].stable_id,
            result.variants[0].tag,
            Some(record),
        )
        .unwrap();
    let array_type = nexa_bytecode::array_type(ValueType::Named(record_type));
    let array_reference = heap
        .allocate(Object::Array {
            type_id: array_type,
            element_type: ValueType::Named(record_type),
            values: vec![record],
        })
        .unwrap();
    let array = RuntimeValue::NamedRef {
        reference: array_reference,
        type_id: array_type,
    };
    let buffer_type = nexa_bytecode::buffer_type(ValueType::Named(record_type));
    let buffer = heap
        .allocate_buffer(buffer_type, ValueType::Named(record_type), &[record])
        .unwrap();
    let arguments = [
        string,
        record,
        event,
        option_value,
        result_value,
        array,
        buffer,
        event,
    ];
    let manual_decode_allocations = [
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.str_ref(0).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(1).unwrap().struct_ref(record_type).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(2).unwrap().enum_ref(event_type).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(3).unwrap().enum_ref(option.type_id).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(4).unwrap().enum_ref(result.type_id).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(5).unwrap().array_ref(array_type).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(6).unwrap().buffer_ref(buffer_type).unwrap();
        })
        .1,
        observed_host_split(|| {
            let args = RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap();
            let _ = args.value_ref(7).unwrap().enum_ref(event_type).unwrap();
        })
        .1,
    ];
    assert_eq!(manual_decode_allocations, [0; 8]);
    registry
        .call_runtime(
            host_matrix::THUNK_RETURN_SCALAR,
            &mut context,
            RuntimeHostArgs::new(&[], Some(&mut heap)).unwrap(),
        )
        .unwrap();
    let (_, baseline_allocations, baseline_host_allocations) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_RETURN_SCALAR,
            &mut context,
            RuntimeHostArgs::new(&[], Some(&mut heap)).unwrap(),
        )
    });
    assert_eq!(baseline_host_allocations, 0);
    let scalar_arguments = [
        RuntimeValue::I32(1),
        RuntimeValue::I32(2),
        RuntimeValue::I32(3),
        RuntimeValue::I32(4),
        RuntimeValue::I32(5),
        RuntimeValue::I32(6),
        RuntimeValue::I32(7),
        RuntimeValue::I32(8),
    ];
    registry
        .call_runtime(
            host_matrix::THUNK_BASELINE8,
            &mut context,
            RuntimeHostArgs::new(&scalar_arguments, Some(&mut heap)).unwrap(),
        )
        .unwrap();
    let (_, eight_argument_baseline, _) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_BASELINE8,
            &mut context,
            RuntimeHostArgs::new(&scalar_arguments, Some(&mut heap)).unwrap(),
        )
    });
    registry
        .call_runtime(
            host_matrix::THUNK_INSPECT,
            &mut context,
            RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap(),
        )
        .unwrap();
    let (outcome, thunk_allocations, host_allocations) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_INSPECT,
            &mut context,
            RuntimeHostArgs::new(&arguments, Some(&mut heap)).unwrap(),
        )
    });
    assert!(matches!(
        outcome,
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(_)))
    ));
    assert_eq!(
        thunk_allocations.saturating_sub(eight_argument_baseline),
        0,
        "complex borrowed input decode"
    );
    assert_eq!(host_allocations, 0, "input-only host implementation");

    let scalar_array_type = nexa_bytecode::array_type(ValueType::I32);
    let scalar_array_reference = heap
        .allocate(Object::Array {
            type_id: scalar_array_type,
            element_type: ValueType::I32,
            values: vec![RuntimeValue::I32(2), RuntimeValue::I32(3)],
        })
        .unwrap();
    let scalar_buffer_type = nexa_bytecode::buffer_type(ValueType::I32);
    let scalar_buffer = heap
        .allocate_buffer(
            scalar_buffer_type,
            ValueType::I32,
            &[RuntimeValue::I32(5), RuntimeValue::I32(7)],
        )
        .unwrap();
    let scalar_collections = [
        RuntimeValue::NamedRef {
            reference: scalar_array_reference,
            type_id: scalar_array_type,
        },
        scalar_buffer,
    ];
    registry
        .call_runtime(
            host_matrix::THUNK_INSPECT_SCALAR_COLLECTIONS,
            &mut context,
            RuntimeHostArgs::new(&scalar_collections, Some(&mut heap)).unwrap(),
        )
        .unwrap();
    let (outcome, allocations, _) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_INSPECT_SCALAR_COLLECTIONS,
            &mut context,
            RuntimeHostArgs::new(&scalar_collections, Some(&mut heap)).unwrap(),
        )
    });
    assert_eq!(
        outcome,
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(17)))
    );
    assert_eq!(allocations, 0, "scalar array and buffer inputs");

    let return_cases = [
        host_matrix::THUNK_RETURN_STRING,
        host_matrix::THUNK_RETURN_STRUCT,
        host_matrix::THUNK_RETURN_ENUM,
        host_matrix::THUNK_RETURN_OPTION,
        host_matrix::THUNK_RETURN_RESULT,
        host_matrix::THUNK_RETURN_ARRAY,
        host_matrix::THUNK_RETURN_BUFFER,
    ];
    let mut separated_host_allocations = 0;
    for id in return_cases {
        registry
            .call_runtime(
                id,
                &mut context,
                RuntimeHostArgs::new(&[], Some(&mut heap)).unwrap(),
            )
            .unwrap();
        let (outcome, thunk_allocations, host_allocations) = observed_host_split(|| {
            registry.call_runtime(
                id,
                &mut context,
                RuntimeHostArgs::new(&[], Some(&mut heap)).unwrap(),
            )
        });
        assert!(outcome.is_ok(), "return case {id}");
        assert_eq!(
            thunk_allocations.saturating_sub(baseline_allocations),
            0,
            "return case {id}"
        );
        separated_host_allocations += host_allocations;
    }
    assert!(separated_host_allocations > 0);

    let wrong_record = heap
        .allocate_struct(StableId::from_name("WrongRecord"), &[string, RuntimeValue::I32(5)])
        .unwrap();
    let mut wrong_arguments = arguments;
    wrong_arguments[1] = wrong_record;
    let (outcome, allocations, _) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_INSPECT,
            &mut context,
            RuntimeHostArgs::new(&wrong_arguments, Some(&mut heap)).unwrap(),
        )
    });
    assert_eq!(outcome, Err(HostTrap::Type));
    assert_eq!(allocations, 0, "wrong struct type id");

    let wrong_event = heap
        .allocate_enum(
            event_type,
            StableId::from_parts(&["Event", "::", "Record"]),
            99,
            Some(record),
        )
        .unwrap();
    wrong_arguments = arguments;
    wrong_arguments[2] = wrong_event;
    let (outcome, allocations, _) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_INSPECT,
            &mut context,
            RuntimeHostArgs::new(&wrong_arguments, Some(&mut heap)).unwrap(),
        )
    });
    assert_eq!(outcome, Err(HostTrap::Type));
    assert_eq!(allocations, 0, "wrong enum tag");

    let bad_fields = heap
        .allocate_struct(record_type, &[string, RuntimeValue::Bool(true)])
        .unwrap();
    let bad_field_arguments = [bad_fields];
    let (outcome, allocations, _) = observed_host_split(|| {
        RuntimeHostArgs::new(&bad_field_arguments, Some(&mut heap))?
            .value_ref(0)?
            .struct_ref(record_type)?
            .field(1)?
            .i32()
    });
    assert_eq!(outcome, Err(HostTrap::Type));
    assert_eq!(allocations, 0, "wrong struct field type");

    let mut full_heap = Heap::new(0);
    let _ = RuntimeHostArgs::new(&[], Some(&mut full_heap))
        .unwrap()
        .return_writer(1);
    let (outcome, allocations, _) = observed_host_split(|| {
        RuntimeHostArgs::new(&[], Some(&mut full_heap))
            .unwrap()
            .return_writer(1)
    });
    assert!(outcome.is_err());
    assert_eq!(allocations, 0, "heap full");

    let mut limited_heap = Heap::new_with_limits(4, usize::MAX, 0);
    let _ = RuntimeHostArgs::new(&[], Some(&mut limited_heap))
        .unwrap()
        .return_writer(1);
    let array_values = vec![RuntimeValue::I32(1)];
    let (outcome, allocations, _) = observed_host_split(|| {
        let args = RuntimeHostArgs::new(&[], Some(&mut limited_heap)).unwrap();
        let mut writer = args.return_writer(1).unwrap();
        writer.write_array(
            nexa_bytecode::array_type(ValueType::I32),
            ValueType::I32,
            array_values,
        )
    });
    assert_eq!(outcome, Err(HostTrap::Type));
    assert_eq!(allocations, 0, "array length limit");
    let buffer_values = vec![RuntimeValue::I32(1)];
    let (outcome, allocations, _) = observed_host_split(|| {
        let args = RuntimeHostArgs::new(&[], Some(&mut limited_heap)).unwrap();
        let mut writer = args.return_writer(1).unwrap();
        writer.write_buffer(
            nexa_bytecode::buffer_type(ValueType::I32),
            ValueType::I32,
            buffer_values,
        )
    });
    assert_eq!(outcome, Err(HostTrap::Type));
    assert_eq!(allocations, 0, "buffer length limit");

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let (outcome, allocations, _) = observed_host_split(|| {
        registry.call_runtime(
            host_matrix::THUNK_PANIC_HOST,
            &mut context,
            RuntimeHostArgs::new(&[], Some(&mut heap)).unwrap(),
        )
    });
    std::panic::set_hook(previous_hook);
    assert_eq!(outcome, Err(HostTrap::Panicked));
    assert_eq!(allocations, 0, "host panic");

    println!("complex_host_allocation_matrix=ok thunk_allocations=0");
}

fn main() {
    complex_host_allocation_matrix();
    let mut runs = Vec::new();
    let mut migration_runs = Vec::new();
    for repeat in 1..=3 {
        let (mut realm, module) = make_realm(vec![
            Instruction::Safepoint,
            Instruction::Yield,
            Instruction::Return { source: 0 },
        ]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let promotion = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Pending(PendingReason::ExplicitYield)
            ));
        });
        let explicit_resume = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![Instruction::Return { source: 0 }]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(21)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 0).unwrap(),
            PollResult::Pending(PendingReason::Fuel)
        );
        let fuel_resume = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![Instruction::Return { source: 0 }]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(22)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let task_completed = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![Instruction::Trap]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(23)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let task_trapped = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Trapped(_)
            ));
        });

        let (mut realm, module) = make_realm(vec![
            Instruction::Yield,
            Instruction::Return { source: 0 },
        ]);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(24)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 64).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        );
        let task_cancelled = observed(|| {
            realm
                .cancel_task(task, CancelReason::ScopeCancelled)
                .unwrap();
        });
        let cleanup_success = observe_cleanup(false);
        let cleanup_trap = observe_cleanup(true);
        let mut task_runtime = TaskRuntime::new(91, RuntimeLimits::default());
        let reload_scope = task_runtime.create_scope(None).unwrap();
        let reload_task = task_runtime
            .admit_task(reload_scope, 1, true)
            .unwrap();
        task_runtime.poll_task(reload_task).unwrap();
        task_runtime.pause_task_for_reload(reload_task).unwrap();
        let reload_commit_cancel = observed(|| {
            task_runtime.commit_reload_cancel(reload_task).unwrap();
        });

        let (mut realm, module) = make_realm(vec![Instruction::Return { source: 0 }]);
        realm.set_trace_enabled(false);
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let trace_off = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_immediate_host_realm(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[
                    RuntimeValue::I32(1),
                    RuntimeValue::I32(2),
                    RuntimeValue::I32(3),
                    RuntimeValue::I32(4),
                    RuntimeValue::I32(5),
                    RuntimeValue::I32(6),
                    RuntimeValue::I32(7),
                    RuntimeValue::I32(8),
                ],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let immediate_host_call = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(Some(RuntimeValue::I32(36)))
            ));
        });
        drop(realm);
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(16);
        let pending = Arc::new(Mutex::new(None));
        let (mut realm, module) =
            make_async_host_realm(RealmConfig::default(), host.clone(), Arc::clone(&pending));
        drop(pending.lock().unwrap());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let async_admission = observed(|| {
            assert_eq!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Pending(PendingReason::HostRequest)
            );
        });
        pending
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .ticket
            .complete(HostPayload::I32(8))
            .unwrap();
        let success_result_writeback = observed(|| {
            realm
                .tick(TickBudget {
                    max_tasks: 1,
                    frame_fuel_budget: 64,
                    collect_garbage: false,
                })
                .unwrap();
        });
        let failed = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(9)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(failed, 64).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        pending
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .ticket
            .fail(HostErrorPayload { code: 9 })
            .unwrap();
        let error_result_writeback = observed(|| {
            realm
                .tick(TickBudget {
                    max_tasks: 1,
                    frame_fuel_budget: 64,
                    collect_garbage: false,
                })
                .unwrap();
        });
        let host_resume = success_result_writeback;
        drop(realm);
        let _releases = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let error_type = StableId::from_name("allocation-observer-host-error");
        let error_enum_writeback = observe_typed_writeback(
            AsyncObserverSpec {
                error: ValueType::Named(error_type),
                error_enum: Some(EnumType {
                    type_id: error_type,
                    variants: vec![EnumVariant {
                        stable_id: StableId::from_name("AllocationObserverError::Rejected"),
                        tag: 7,
                        payload_type: None,
                    }],
                }),
                ..AsyncObserverSpec::default()
            },
            ObservedCompletion::Error(7),
        );
        let cancel_return_error = observe_typed_writeback(
            AsyncObserverSpec {
                cancel_policy: CancelPolicy::ReturnError,
                cancel_error: Some(13),
                ..AsyncObserverSpec::default()
            },
            ObservedCompletion::Cancelled,
        );
        let abandon_return_error = observe_typed_writeback(
            AsyncObserverSpec {
                abandon_policy: AbandonPolicy::ReturnError,
                abandon_error: Some(17),
                ..AsyncObserverSpec::default()
            },
            ObservedCompletion::Abandoned,
        );
        let heap_full_writeback = observe_typed_writeback(
            AsyncObserverSpec::default(),
            ObservedCompletion::HeapFullSuccess,
        );

        let host = RuntimeHost::new(4);
        let pending = Arc::new(Mutex::new(None));
        let config = RealmConfig {
            max_host_resources: 1,
            ..RealmConfig::default()
        };
        let (mut realm, module) =
            make_async_host_realm(config, host.clone(), Arc::clone(&pending));
        drop(pending.lock().unwrap());
        let scope = realm.create_scope(None).unwrap();
        let first = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(1)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(first, 64).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        let rejected = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(2)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let async_admission_capacity_failure = observed(|| {
            assert!(realm.poll_task(rejected, 64).is_err());
        });
        drop(realm);
        drop(pending.lock().unwrap().take());
        let _ = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(4);
        let pending = Arc::new(Mutex::new(None));
        let (mut realm, module) =
            make_async_host_realm(RealmConfig::default(), host.clone(), Arc::clone(&pending));
        drop(pending.lock().unwrap());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(3)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 64).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        let async_admission_cancellation = observed(|| {
            realm
                .cancel_task(task, CancelReason::ScopeCancelled)
                .unwrap();
        });
        drop(pending.lock().unwrap().take());
        drop(realm);
        let _ = host.drain_releases();
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        realm
            .create_resource_token(task, RuntimeHostDomain::Render)
            .unwrap();
        let pending = realm.create_host_request(task).unwrap();
        realm.wait_for_request(task, pending.request).unwrap();
        let realm_drop_transfer = observed(|| drop(realm));
        assert_eq!(host.pending_releases(), 2);
        drop(pending);
        assert_eq!(host.drain_releases().len(), 2);
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(31)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        realm
            .create_resource_token(task, RuntimeHostDomain::Render)
            .unwrap();
        let token_release = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });
        drop(realm);
        let mut drain_records = release_buffer::<2>();
        let runtime_host_drain = observed(|| {
            assert_eq!(host.drain_into(&mut drain_records), 1);
        });
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(32)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        realm
            .create_typed_snapshot(
                task,
                nexa_runtime::EncodedSnapshot::copy_i32_slice(
                    StableId::from_name("ObserverSnapshot"),
                    StableId::from_name("ObserverSnapshot::snapshot-schema"),
                    &[1_i32, 2, 3],
                )
                .unwrap(),
            )
            .unwrap();
        let snapshot_release = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(_)
            ));
        });
        drop(realm);
        let mut drain_records = release_buffer::<2>();
        assert_eq!(host.drain_into(&mut drain_records), 1);
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let host = RuntimeHost::new(4);
        let (mut realm, module) = make_realm_with_host(host.clone());
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(33)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        let detached = realm.create_host_request(task).unwrap();
        realm.wait_for_request(task, detached.request).unwrap();
        let detached_request_release = observed(|| drop(realm));
        drop(detached);
        let mut drain_records = release_buffer::<2>();
        assert_eq!(host.drain_into(&mut drain_records), 1);
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        let retired_host_hash = StableId::from_name("allocation-observer-retired-host");
        let retired_schema = StableId::from_name("allocation-observer-retired-schema");
        let host = RuntimeHost::new(4);
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            host.clone(),
            Box::new(NoHost(retired_host_hash)),
        )
        .unwrap();
        let old = realm
            .load_module(
                build_retired_epoch_module(retired_host_hash, retired_schema),
                retired_host_hash,
                retired_schema,
            )
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                old,
                2,
                &[RuntimeValue::I32(34)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 16,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 64).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        );
        realm
            .create_resource_token(task, RuntimeHostDomain::Io)
            .unwrap();
        realm
            .prepare_reload(
                old,
                build_retired_epoch_module(retired_host_hash, retired_schema),
                retired_host_hash,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm
            .stage_reload(&[RuntimeValue::I32(1)])
            .unwrap();
        realm.commit_reload(&[], 32).unwrap();
        let retired_epoch_final_transfer = observed(|| {
            realm
                .tick(TickBudget {
                    max_tasks: 0,
                    frame_fuel_budget: 0,
                    collect_garbage: false,
                })
                .unwrap();
        });
        assert!(realm.resource_invariants_hold());
        drop(realm);
        let mut drain_records = release_buffer::<2>();
        assert_eq!(host.drain_into(&mut drain_records), 1);
        let _ = host.begin_close();
        host.try_finish_close().unwrap();

        for count in &MIGRATION_COUNTS {
            count.store(0, Ordering::SeqCst);
        }
        let mut migration_realm = make_migration_realm();
        set_migration_allocation_observer(Some(migration_observer));
        assert_eq!(
            migration_realm.stage_reload(&[]).unwrap(),
            Some(RuntimeValue::I32(7))
        );
        set_migration_allocation_observer(None);
        migration_runs.push((
            repeat,
            migration_count(MigrationAllocationPhase::ContextConstruction),
            migration_count(MigrationAllocationPhase::FirstOpcode),
            migration_count(MigrationAllocationPhase::OldGet),
            migration_count(MigrationAllocationPhase::NewCreate),
            migration_count(MigrationAllocationPhase::NewSet),
            migration_count(MigrationAllocationPhase::Preserve),
            migration_count(MigrationAllocationPhase::Replace),
            migration_count(MigrationAllocationPhase::Delete),
            migration_count(MigrationAllocationPhase::StateFinish),
            migration_count(MigrationAllocationPhase::Finish),
        ));
        runs.push((
            repeat,
            promotion,
            explicit_resume,
            fuel_resume,
            host_resume,
            cleanup_success,
            cleanup_trap,
            task_completed,
            task_cancelled,
            task_trapped,
            reload_commit_cancel,
            trace_off,
            immediate_host_call,
            async_admission,
            async_admission_capacity_failure,
            async_admission_cancellation,
            success_result_writeback,
            error_result_writeback,
            error_enum_writeback,
            cancel_return_error,
            abandon_return_error,
            heap_full_writeback,
            token_release,
            snapshot_release,
            detached_request_release,
            runtime_host_drain,
            retired_epoch_final_transfer,
            realm_drop_transfer,
        ));
    }

    let required_paths_zero = runs
        .iter()
        .all(
            |(
                _,
                promotion,
                explicit_resume,
                fuel_resume,
                host_resume,
                cleanup_success,
                cleanup_trap,
                task_completed,
                task_cancelled,
                task_trapped,
                reload_commit_cancel,
                trace_off,
                immediate_host_call,
                async_admission,
                async_admission_capacity_failure,
                async_admission_cancellation,
                success_result_writeback,
                error_result_writeback,
                error_enum_writeback,
                cancel_return_error,
                abandon_return_error,
                heap_full_writeback,
                token_release,
                snapshot_release,
                detached_request_release,
                runtime_host_drain,
                retired_epoch_final_transfer,
                realm_drop_transfer,
            )| {
                *promotion
                    + *explicit_resume
                    + *fuel_resume
                    + *host_resume
                    + *cleanup_success
                    + *cleanup_trap
                    + *task_completed
                    + *task_cancelled
                    + *task_trapped
                    + *reload_commit_cancel
                    + *trace_off
                    + *immediate_host_call
                    + *async_admission
                    + *async_admission_capacity_failure
                    + *async_admission_cancellation
                    + *success_result_writeback
                    + *error_result_writeback
                    + *error_enum_writeback
                    + *cancel_return_error
                    + *abandon_return_error
                    + *heap_full_writeback
                    + *token_release
                    + *snapshot_release
                    + *detached_request_release
                    + *runtime_host_drain
                    + *retired_epoch_final_transfer
                    + *realm_drop_transfer
                    == 0
            },
        );
    let all_measured_paths_zero = runs.iter().all(
        |(
            _,
            promotion,
            explicit_resume,
            fuel_resume,
            host_resume,
            cleanup_success,
            cleanup_trap,
            task_completed,
            task_cancelled,
            task_trapped,
            reload_commit_cancel,
            trace_off,
            immediate_host_call,
            async_admission,
            async_admission_capacity_failure,
            async_admission_cancellation,
            success_result_writeback,
            error_result_writeback,
            error_enum_writeback,
            cancel_return_error,
            abandon_return_error,
            heap_full_writeback,
            token_release,
            snapshot_release,
            detached_request_release,
            runtime_host_drain,
            retired_epoch_final_transfer,
            realm_drop_transfer,
        )| {
            *promotion
                + *explicit_resume
                + *fuel_resume
                + *host_resume
                + *cleanup_success
                + *cleanup_trap
                + *task_completed
                + *task_cancelled
                + *task_trapped
                + *reload_commit_cancel
                + *trace_off
                + *immediate_host_call
                + *async_admission
                + *async_admission_capacity_failure
                + *async_admission_cancellation
                + *success_result_writeback
                + *error_result_writeback
                + *error_enum_writeback
                + *cancel_return_error
                + *abandon_return_error
                + *heap_full_writeback
                + *token_release
                + *snapshot_release
                + *detached_request_release
                + *runtime_host_drain
                + *retired_epoch_final_transfer
                + *realm_drop_transfer
                == 0
        },
    );
    let migration_hot_paths_zero = migration_runs.iter().all(
        |(
            _,
            construction,
            first_opcode,
            old_get,
            new_create,
            new_set,
            preserve,
            replace,
            delete,
            state_finish,
            finish,
        )| {
            *construction > 0
                && *first_opcode
                    + *old_get
                    + *new_create
                    + *new_set
                    + *preserve
                    + *replace
                    + *delete
                    + *state_finish
                    + *finish
                    == 0
        },
    );
    println!(
        "{{\"observer\":\"global_allocator\",\"toolchain\":\"rustc-1.97.1\",\"runs\":[{}],\"migration_runs\":[{}],\"allocation_free_contract_paths_zero\":{required_paths_zero},\"all_measured_paths_zero\":{all_measured_paths_zero},\"migration_hot_paths_zero\":{migration_hot_paths_zero}}}",
        runs.iter()
            .map(|(repeat, promotion, explicit_resume, fuel_resume, host_resume, cleanup_success, cleanup_trap, task_completed, task_cancelled, task_trapped, reload_commit_cancel, trace_off, immediate_host_call, async_admission, async_admission_capacity_failure, async_admission_cancellation, success_result_writeback, error_result_writeback, error_enum_writeback, cancel_return_error, abandon_return_error, heap_full_writeback, token_release, snapshot_release, detached_request_release, runtime_host_drain, retired_epoch_final_transfer, realm_drop_transfer)| format!(
                "{{\"repeat\":{repeat},\"promotion\":{promotion},\"explicit_resume\":{explicit_resume},\"fuel_resume\":{fuel_resume},\"host_resume\":{host_resume},\"cleanup_success\":{cleanup_success},\"cleanup_trap\":{cleanup_trap},\"task_completed\":{task_completed},\"task_cancelled\":{task_cancelled},\"task_trapped\":{task_trapped},\"reload_commit_cancel\":{reload_commit_cancel},\"trace_off\":{trace_off},\"immediate_host_call\":{immediate_host_call},\"async_admission\":{async_admission},\"async_admission_capacity_failure\":{async_admission_capacity_failure},\"async_admission_cancellation\":{async_admission_cancellation},\"success_result_writeback\":{success_result_writeback},\"error_result_writeback\":{error_result_writeback},\"error_enum_writeback\":{error_enum_writeback},\"cancel_return_error\":{cancel_return_error},\"abandon_return_error\":{abandon_return_error},\"heap_full_writeback\":{heap_full_writeback},\"token_release\":{token_release},\"snapshot_release\":{snapshot_release},\"detached_request_release\":{detached_request_release},\"runtime_host_drain\":{runtime_host_drain},\"retired_epoch_final_transfer\":{retired_epoch_final_transfer},\"realm_drop_transfer\":{realm_drop_transfer}}}"
            ))
            .collect::<Vec<_>>()
            .join(","),
        migration_runs
            .iter()
            .map(
                |(
                    repeat,
                    construction,
                    first_opcode,
                    old_get,
                    new_create,
                    new_set,
                    preserve,
                    replace,
                    delete,
                    state_finish,
                    finish,
                )| format!(
                    "{{\"repeat\":{repeat},\"construction\":{construction},\"first_opcode\":{first_opcode},\"old_get\":{old_get},\"new_create\":{new_create},\"new_set\":{new_set},\"preserve\":{preserve},\"replace\":{replace},\"delete\":{delete},\"state_finish\":{state_finish},\"finish\":{finish}}}"
                ),
            )
            .collect::<Vec<_>>()
            .join(","),
    );
    assert!(
        required_paths_zero,
        "an allocation-free contract path allocated"
    );
    assert!(
        migration_hot_paths_zero,
        "a migration opcode or finish allocated"
    );
}

fn make_migration_realm() -> RealmRuntime {
    let host = StableId::from_name("allocation-observer-migration-host");
    let old_schema_hash = StableId::from_name("allocation-observer-state-v1");
    let new_schema_hash = StableId::from_name("allocation-observer-state-v2");
    let new_type = StableId::from_name("ObserverState");
    let new_field = StableId::from_name("ObserverState::value");
    let preserved_type = StableId::from_name("PreservedState");
    let preserved_field = StableId::from_name("PreservedState::value");
    let replaced_id = StableId::from_name("migration-replaced");
    let preserved_id = StableId::from_name("migration-preserved");
    let deleted_id = StableId::from_name("migration-deleted");
    let target_id = StableId::from_name("migration-target");

    let preserved_schema = StateType {
        stable_id: preserved_type,
        version: 1,
        fields: vec![StateField {
            stable_id: preserved_field,
            ty: ValueType::I32,
        }],
    };
    let mut old_entry = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    old_entry
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    let mut old_module = ModuleBuilder::new();
    old_module
        .metadata(host, old_schema_hash)
        .state_schema(StateSchema {
            types: vec![preserved_schema.clone()],
        })
        .function(old_entry.finish().unwrap());
    let old_module = verify(old_module.finish(), VerifierLimits::default()).unwrap();

    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        2,
    );
    migration
        .effect(FunctionEffect::Migration)
        .emit(Instruction::StateOldGet {
            stable_id: replaced_id,
            ty: ValueType::I32,
            dst: 0,
        })
        .emit(Instruction::StateNewCreate {
            stable_id: target_id,
            type_id: new_type,
            dst: 1,
        })
        .emit(Instruction::StateNewSet {
            object: 1,
            field_id: new_field,
            source: 0,
        })
        .emit(Instruction::StatePreserve {
            stable_id: preserved_id,
        })
        .emit(Instruction::StateReplace {
            old_id: replaced_id,
            target: 1,
        })
        .emit(Instruction::StateDelete {
            stable_id: deleted_id,
        })
        .emit(Instruction::StateFinish)
        .emit(Instruction::Return { source: 0 });
    let mut migration = migration.finish().unwrap();
    migration.root_bitmap = vec![false, true];
    migration.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false, false],
        },
        RootMap {
            pc: 7,
            bitmap: vec![false, true],
        },
    ];
    let mut candidate = ModuleBuilder::new();
    candidate
        .metadata(host, new_schema_hash)
        .state_schema(StateSchema {
            types: vec![
                StateType {
                    stable_id: new_type,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: new_field,
                        ty: ValueType::I32,
                    }],
                },
                preserved_schema,
            ],
        })
        .function(migration);
    let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();

    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let old = realm
        .load_module(old_module, host, old_schema_hash)
        .unwrap();
    realm
        .insert_state(old, replaced_id, StateValue::I32(7))
        .unwrap();
    realm
        .insert_state(
            old,
            preserved_id,
            StateValue::Object(StateObject {
                type_id: preserved_type,
                version: 1,
                fields: BTreeMap::from([(preserved_field, StateValue::I32(3))]),
            }),
        )
        .unwrap();
    realm
        .insert_state(old, deleted_id, StateValue::I32(9))
        .unwrap();
    realm
        .prepare_reload(old, candidate, host)
        .unwrap();
    realm.quiesce_reload().unwrap();
    realm
}

#[derive(Clone)]
struct AsyncObserverSpec {
    error: ValueType,
    error_enum: Option<EnumType>,
    cancel_policy: CancelPolicy,
    abandon_policy: AbandonPolicy,
    cancel_error: Option<u32>,
    abandon_error: Option<u32>,
}

impl Default for AsyncObserverSpec {
    fn default() -> Self {
        Self {
            error: ValueType::I32,
            error_enum: None,
            cancel_policy: CancelPolicy::CancelTask,
            abandon_policy: AbandonPolicy::Trap,
            cancel_error: None,
            abandon_error: None,
        }
    }
}

fn make_async_host_realm(
    config: RealmConfig,
    host: RuntimeHost,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    make_async_host_realm_with_spec(config, host, pending, AsyncObserverSpec::default())
}

fn make_async_host_realm_with_spec(
    config: RealmConfig,
    host: RuntimeHost,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
    spec: AsyncObserverSpec,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-async-host");
    let schema = StableId::from_name("allocation-observer-async-schema");
    let result = nexa_bytecode::result_type(ValueType::I32, spec.error);
    let function = Function {
        signature: Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::Named(result.type_id)),
        },
        registers: 2,
        frame_bytes: 16,
        root_bitmap: vec![false, true],
        root_maps: vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
        ],
        safepoints: vec![0, 1],
        loop_bounds: Vec::new(),
        effect: FunctionEffect::Task,
        max_static_call_depth: 1,
        code: vec![
            Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 1,
                dst: 1,
            },
            Instruction::Return { source: 1 },
        ],
    };
    let mut builder = ModuleBuilder::new();
    builder.metadata(host_hash, schema);
    if let Some(error_enum) = spec.error_enum {
        builder.enum_type(error_enum);
    }
    builder.enum_type(result.clone());
    builder.host_import(HostImport {
        stable_id: StableId::from_name("Observer::async_increment"),
        parameters: vec![ValueType::I32],
        result: Some(ValueType::Named(result.type_id)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(AsyncResultType {
            result_type: result.type_id,
            success: ValueType::I32,
            error: spec.error,
            cancel_policy: spec.cancel_policy,
            abandon_policy: spec.abandon_policy,
            cancel_error: spec.cancel_error,
            abandon_error: spec.abandon_error,
        }),
    });
    builder.function(function);
    let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
    let mut realm =
        RealmRuntime::hosted(config, host, Box::new(AsyncHost { host_hash, pending })).unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_immediate_host_realm(
    host: RuntimeHost,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-immediate-host");
    let schema = StableId::from_name("allocation-observer-immediate-schema");
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32; 8],
            result: Some(ValueType::I32),
        },
        9,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 8,
            dst: 8,
        })
        .emit(Instruction::Return { source: 8 });
    let mut builder = ModuleBuilder::new();
    builder.metadata(host_hash, schema);
    builder.host_import(HostImport {
        stable_id: StableId::from_name("Observer::increment"),
        parameters: vec![ValueType::I32; 8],
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    builder.function(function.finish().unwrap());
    let verified = verify(builder.finish(), VerifierLimits::default()).unwrap();
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        host,
        Box::new(ImmediateHost(host_hash)),
    )
    .unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_realm_with_host(
    host: RuntimeHost,
) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host_hash = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host_hash, schema, vec![Instruction::Return { source: 0 }]);
    let mut realm =
        RealmRuntime::hosted(RealmConfig::default(), host, Box::new(NoHost(host_hash))).unwrap();
    let module = realm.load_module(verified, host_hash, schema).unwrap();
    (realm, module)
}

fn make_realm(code: Vec<Instruction>) -> (RealmRuntime, nexa_runtime::ModuleHandle) {
    let host = StableId::from_name("allocation-observer-host");
    let schema = StableId::from_name("allocation-observer-schema");
    let verified = build_module(host, schema, code);
    let mut realm = RealmRuntime::isolated(RealmConfig::default());
    let module = realm.load_module(verified, host, schema).unwrap();
    (realm, module)
}

fn build_module(
    host: StableId,
    schema: StableId,
    code: Vec<Instruction>,
) -> nexa_verifier::VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function.effect(FunctionEffect::Task);
    for instruction in code {
        function.emit(instruction);
    }
    let mut builder = ModuleBuilder::new();
    builder
        .metadata(host, schema)
        .function(function.finish().unwrap());
    verify(builder.finish(), VerifierLimits::default()).unwrap()
}

fn build_retired_epoch_module(
    host: StableId,
    schema: StableId,
) -> nexa_verifier::VerifiedModule {
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    migration
        .effect(FunctionEffect::Migration)
        .emit(Instruction::Return { source: 0 });
    let mut activation = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    activation
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    let mut task = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        1,
    );
    task.effect(FunctionEffect::Task)
        .emit(Instruction::Yield)
        .emit(Instruction::Return { source: 0 });
    let mut builder = ModuleBuilder::new();
    builder.metadata(host, schema);
    builder.function(migration.finish().unwrap());
    builder.function(activation.finish().unwrap());
    builder.function(task.finish().unwrap());
    verify(builder.finish(), VerifierLimits::default()).unwrap()
}

struct NoHost(StableId);

struct ImmediateHost(StableId);

struct AsyncHost {
    host_hash: StableId,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for AsyncHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.host_hash)
    }

    fn call(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 1 || !matches!(args.get(0)?, HostValue::I32(_)) {
            return Err(HostTrap::Type);
        }
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::Panicked)?;
        let request = pending.request;
        *self.pending.lock().unwrap() = Some(pending);
        Ok(HostCallOutcome::Pending(request))
    }
}

impl HostRegistry for ImmediateHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 8 {
            return Err(HostTrap::Arity);
        }
        let mut sum = 0;
        for index in 0..8 {
            let HostValue::I32(value) = args.get(index)? else {
                return Err(HostTrap::Type);
            };
            sum += value;
        }
        Ok(HostCallOutcome::Immediate(HostValue::I32(sum)))
    }

    fn call_runtime(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || args.len() != 8 {
            return Err(HostTrap::Arity);
        }
        Ok(HostCallOutcome::Immediate(HostValue::I32(
            args.i32(0)?
                + args.i32(1)?
                + args.i32(2)?
                + args.i32(3)?
                + args.i32(4)?
                + args.i32(5)?
                + args.i32(6)?
                + args.i32(7)?,
        )))
    }
}

impl HostRegistry for NoHost {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.0)
    }

    fn call(
        &mut self,
        id: u32,
        _: &mut ResourceContext<'_>,
        _: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::UnknownFunction(id))
    }
}
