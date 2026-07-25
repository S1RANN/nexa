use std::alloc::{GlobalAlloc, Layout, System};
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
    CancelReason, HeapError, HostArgs, HostCallOutcome, HostErrorPayload, HostPayload, HostRegistry,
    HostTrap, HostValue, MigrationAllocationPhase, PendingHostRequest, PendingReason, PollResult,
    RealmConfig, RealmError, RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostDomain,
    RuntimeLimits, RuntimeValue, StateObject, StateValue, StepConfig, TaskLimits, TaskRuntime,
    TaskState, TickBudget, set_migration_allocation_observer,
};
use nexa_verifier::{VerifierLimits, verify};

struct CountingAllocator;

static ENABLED: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static FIRST_OPCODE_ACTIVE: AtomicBool = AtomicBool::new(false);
static MIGRATION_COUNTS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn observed(operation: impl FnOnce()) -> u64 {
    ALLOCATIONS.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    operation();
    ENABLED.store(false, Ordering::SeqCst);
    ALLOCATIONS.load(Ordering::SeqCst)
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
                ENABLED.store(true, Ordering::SeqCst);
            } else if !FIRST_OPCODE_ACTIVE.load(Ordering::SeqCst) {
                ALLOCATIONS.store(0, Ordering::SeqCst);
                ENABLED.store(true, Ordering::SeqCst);
            }
        }
        nexa_runtime::AllocationBoundary::End => {
            MIGRATION_COUNTS[index].store(ALLOCATIONS.load(Ordering::SeqCst), Ordering::SeqCst);
            if phase == MigrationAllocationPhase::FirstOpcode {
                ENABLED.store(false, Ordering::SeqCst);
                FIRST_OPCODE_ACTIVE.store(false, Ordering::SeqCst);
            } else if !FIRST_OPCODE_ACTIVE.load(Ordering::SeqCst) {
                ENABLED.store(false, Ordering::SeqCst);
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

fn main() {
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
        let immediate_host_call = observed(|| {
            assert!(matches!(
                realm.poll_task(task, 64).unwrap(),
                PollResult::Completed(Some(RuntimeValue::I32(8)))
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

        for count in &MIGRATION_COUNTS {
            count.store(0, Ordering::SeqCst);
        }
        let mut migration_realm = make_migration_realm();
        set_migration_allocation_observer(Some(migration_observer));
        assert_eq!(
            migration_realm.stage_reload(0, &[]).unwrap(),
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
            .map(|(repeat, promotion, explicit_resume, fuel_resume, host_resume, cleanup_success, cleanup_trap, task_completed, task_cancelled, task_trapped, reload_commit_cancel, trace_off, immediate_host_call, async_admission, async_admission_capacity_failure, async_admission_cancellation, success_result_writeback, error_result_writeback, error_enum_writeback, cancel_return_error, abandon_return_error, heap_full_writeback, realm_drop_transfer)| format!(
                "{{\"repeat\":{repeat},\"promotion\":{promotion},\"explicit_resume\":{explicit_resume},\"fuel_resume\":{fuel_resume},\"host_resume\":{host_resume},\"cleanup_success\":{cleanup_success},\"cleanup_trap\":{cleanup_trap},\"task_completed\":{task_completed},\"task_cancelled\":{task_cancelled},\"task_trapped\":{task_trapped},\"reload_commit_cancel\":{reload_commit_cancel},\"trace_off\":{trace_off},\"immediate_host_call\":{immediate_host_call},\"async_admission\":{async_admission},\"async_admission_capacity_failure\":{async_admission_capacity_failure},\"async_admission_cancellation\":{async_admission_cancellation},\"success_result_writeback\":{success_result_writeback},\"error_result_writeback\":{error_result_writeback},\"error_enum_writeback\":{error_enum_writeback},\"cancel_return_error\":{cancel_return_error},\"abandon_return_error\":{abandon_return_error},\"heap_full_writeback\":{heap_full_writeback},\"realm_drop_transfer\":{realm_drop_transfer}}}"
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
        .prepare_reload_migrating(old, candidate, host)
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
            parameters: vec![ValueType::I32],
            result: Some(ValueType::I32),
        },
        2,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 1,
            dst: 1,
        })
        .emit(Instruction::Return { source: 1 });
    let mut builder = ModuleBuilder::new();
    builder.metadata(host_hash, schema);
    builder.host_import(HostImport {
        stable_id: StableId::from_name("Observer::increment"),
        parameters: vec![ValueType::I32],
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
        if id != 0 || args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let HostValue::I32(value) = args.get(0)? else {
            return Err(HostTrap::Type);
        };
        Ok(HostCallOutcome::Immediate(HostValue::I32(value + 1)))
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
