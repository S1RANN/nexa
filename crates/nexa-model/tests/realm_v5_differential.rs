use nexa_model::realm_v5::{
    REALM_V5_EVENTS, RealmV5ApplyError, RealmV5Config, RealmV5Event, RealmV5RejectionClass,
    RealmV5ReloadState, RealmV5RequestState, RealmV5RetiredEpoch, RealmV5TaskState, RealmV5World,
    explore_realm_v5,
};
use nexa_runtime::RuntimeHostState;
use nexa_runtime::model_adapter::{
    RealmV5RejectionClass as RuntimeRealmV5RejectionClass, RealmV5RuntimeAdapter,
    RealmV5RuntimeApplyError, RealmV5RuntimeEvent, RealmV5RuntimeExecution,
    RealmV5RuntimeReloadState, RealmV5RuntimeRequestState, RealmV5RuntimeRetiredEpoch,
    RealmV5RuntimeSnapshot, RealmV5RuntimeTaskState,
};

#[test]
fn every_real_realm_v5_shortest_path_matches_inspection() {
    let report = explore_realm_v5(RealmV5Config::default());
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!report.truncated);
    assert_eq!(report.shortest_paths.len(), report.visited_worlds);

    for path in &report.shortest_paths {
        let mut model = RealmV5World::default();
        let mut runtime = RealmV5RuntimeAdapter::new();
        assert_matches(model, runtime.snapshot().unwrap(), path);
        assert_inspection_matches(model, &runtime.realm().inspection_snapshot(), path);
        for event in path {
            let before = runtime.realm().inspection_snapshot();
            assert_inspection_matches(model, &before, path);
            model.apply(*event).unwrap();
            runtime
                .apply(runtime_event(*event))
                .unwrap_or_else(|error| {
                    panic!("runtime rejected {event:?} on {path:?}: {error:?}")
                });
            let after = runtime.realm().inspection_snapshot();
            assert_inspection_matches(model, &after, path);
            assert_matches(model, runtime.snapshot().unwrap(), path);
        }
    }
}

#[test]
fn every_real_realm_v5_rejection_class_matches_without_mutation() {
    let report = explore_realm_v5(RealmV5Config::default());
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!report.truncated);
    let required_classes = [
        RealmV5RejectionClass::InvalidTaskState,
        RealmV5RejectionClass::InvalidReloadState,
        RealmV5RejectionClass::InvalidRequestState,
        RealmV5RejectionClass::HostNotOpen,
        RealmV5RejectionClass::Capacity,
        RealmV5RejectionClass::RootConflict,
        RealmV5RejectionClass::EpochConflict,
    ];
    assert_eq!(report.rejection_reasons.len(), required_classes.len());
    for class in required_classes {
        assert!(
            report.rejection_reasons.contains_key(&class),
            "rejection class {class:?} is unreachable"
        );
    }
    for path in report.shortest_paths {
        let mut model = RealmV5World::default();
        let mut runtime = RealmV5RuntimeAdapter::new();
        for event in &path {
            model.apply(*event).unwrap();
            runtime.apply(runtime_event(*event)).unwrap();
        }
        let before = runtime.snapshot().unwrap();
        let before_inspection = runtime.realm().inspection_snapshot();
        for event in REALM_V5_EVENTS {
            let mut rejected_model = model;
            if let Err(RealmV5ApplyError::Rejected(reason)) = rejected_model.apply(event) {
                let Err(runtime_error) = runtime.apply(runtime_event(event)) else {
                    panic!(
                        "runtime accepted {event:?} after model path {path:?}; \
                         model rejection={reason:?}; model={model:#?}"
                    );
                };
                assert_eq!(
                    runtime_error,
                    RealmV5RuntimeApplyError::Rejected(runtime_rejection(reason)),
                    "rejection mismatch for {event:?} after {path:?}"
                );
                assert_eq!(
                    runtime.snapshot().unwrap(),
                    before,
                    "rejected runtime event mutated state: {event:?} after {path:?}"
                );
                assert_eq!(
                    runtime.realm().inspection_snapshot(),
                    before_inspection,
                    "rejected runtime event mutated production inspection: \
                     {event:?} after {path:?}"
                );
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn assert_inspection_matches(
    model: RealmV5World,
    inspection: &nexa_runtime::RealmInspectionSnapshot,
    path: &[RealmV5Event],
) {
    let active = inspection.active_root.as_ref().expect("active root exists");
    assert_eq!(
        normalize_runtime_epoch(active.epoch),
        model.active_epoch,
        "inspection active root: {path:?}"
    );
    assert_eq!(
        active.lifecycle,
        if model.reload == RealmV5ReloadState::ActivationFaulted {
            nexa_runtime::ModuleLifecycle::ActivationFaulted
        } else {
            nexa_runtime::ModuleLifecycle::Active
        },
        "inspection active lifecycle: {path:?}"
    );
    assert_eq!(
        inspection
            .candidate_root
            .as_ref()
            .map(|candidate| normalize_runtime_epoch(candidate.epoch)),
        model.candidate_epoch,
        "inspection candidate root: {path:?}"
    );
    if let Some(candidate) = &inspection.candidate_root {
        assert_eq!(
            candidate.lifecycle,
            nexa_runtime::ModuleLifecycle::Staging,
            "inspection candidate lifecycle: {path:?}"
        );
    }

    for module in &inspection.modules {
        let epoch = normalize_runtime_epoch(module.epoch);
        assert_eq!(
            module.state_objects,
            usize::from(model.state_registry_objects[usize::from(epoch)]),
            "inspection state registry at epoch {epoch}: {path:?}"
        );
        let expected = if epoch == model.active_epoch {
            if model.reload == RealmV5ReloadState::ActivationFaulted {
                nexa_runtime::ModuleLifecycle::ActivationFaulted
            } else {
                nexa_runtime::ModuleLifecycle::Active
            }
        } else if model.candidate_epoch == Some(epoch) {
            nexa_runtime::ModuleLifecycle::Staging
        } else if model.retired_epochs.iter().any(
            |retired| matches!(retired, RealmV5RetiredEpoch::Retired(value) if *value == epoch),
        ) {
            nexa_runtime::ModuleLifecycle::Retired
        } else {
            panic!("unexpected live module epoch {epoch} after {path:?}");
        };
        assert_eq!(
            module.lifecycle, expected,
            "inspection module lifecycle at epoch {epoch}: {path:?}"
        );
        assert_eq!(
            module.generation,
            module.handle.raw().generation,
            "inspection module generation at epoch {epoch}: {path:?}"
        );
        assert_eq!(
            module.module_id,
            module.handle.raw().index,
            "inspection module identity at epoch {epoch}: {path:?}"
        );
    }

    assert_eq!(
        inspection.reload.root_publications.len(),
        model
            .retired_epochs
            .iter()
            .filter(|retired| !matches!(retired, RealmV5RetiredEpoch::Vacant))
            .count(),
        "inspection root publication count: {path:?}"
    );
    for (index, publication) in inspection.reload.root_publications.iter().enumerate() {
        assert_eq!(
            normalize_runtime_epoch(publication.candidate_epoch),
            u8::try_from(index + 1).expect("bounded publication index"),
            "inspection root publication order: {path:?}"
        );
    }
    assert_eq!(
        inspection.reload.completion_buffer,
        usize::from(model.reload_completion_buffer),
        "inspection reload completion buffer: {path:?}"
    );
    assert_eq!(
        inspection.heap.live_objects,
        usize::from(model.heap_objects),
        "inspection heap objects: {path:?}"
    );
    assert_eq!(
        inspection.roots.module_globals,
        usize::from(model.gc_root),
        "inspection module GC roots: {path:?}"
    );
    assert_eq!(
        inspection.terminal_records.len(),
        usize::from(model.terminal_records),
        "inspection terminal records: {path:?}"
    );
    assert_eq!(
        inspection.runtime_host,
        Some(runtime_host_state(model.runtime_host)),
        "inspection RuntimeHost state: {path:?}"
    );
    assert_eq!(
        inspection.tasks.len(),
        model
            .tasks
            .iter()
            .filter(|task| task_is_live(task.state))
            .count(),
        "inspection live task count: {path:?}"
    );
    assert_eq!(
        usize::try_from(inspection.resources.tasks).expect("bounded task ledger"),
        inspection.tasks.len(),
        "inspection task ledger: {path:?}"
    );
    assert_eq!(
        usize::try_from(inspection.resources.heap_objects).expect("bounded heap ledger"),
        inspection.heap.live_objects,
        "inspection heap ledger: {path:?}"
    );
}

fn normalize_runtime_epoch(epoch: u64) -> u8 {
    u8::try_from(epoch.checked_sub(1).expect("runtime epochs are positive"))
        .expect("Realm v5 epoch is bounded")
}

#[allow(clippy::too_many_lines)]
fn assert_matches(model: RealmV5World, runtime: RealmV5RuntimeSnapshot, path: &[RealmV5Event]) {
    assert_eq!(
        runtime.active_epoch, model.active_epoch,
        "active epoch: {path:?}"
    );
    assert_eq!(
        runtime.candidate_epoch, model.candidate_epoch,
        "candidate epoch: {path:?}"
    );
    for index in 0..model.retired_epochs.len() {
        assert_eq!(
            runtime.retired_epochs[index],
            retired_epoch(model.retired_epochs[index]),
            "retired epoch {index}: {path:?}"
        );
    }
    for index in 0..model.tasks.len() {
        assert_eq!(
            runtime.tasks[index].state,
            task_state(model.tasks[index].state),
            "task state {index}: {path:?}"
        );
        assert_eq!(
            runtime.tasks[index].execution,
            task_execution(model.tasks[index].state),
            "task execution {index}: {path:?}"
        );
        assert_eq!(
            runtime.tasks[index].epoch, model.tasks[index].epoch,
            "task epoch {index}: {path:?}"
        );
        assert_eq!(
            runtime.scheduler[index], model.scheduler[index],
            "scheduler {index}: {path:?}"
        );
    }
    for index in 0..model.requests.len() {
        assert_eq!(
            runtime.requests[index].state,
            request_state(model.requests[index].state),
            "request state {index}: {path:?}"
        );
        assert_eq!(
            runtime.requests[index].task, model.requests[index].task,
            "request owner {index}: {path:?}"
        );
        assert_eq!(
            runtime.requests[index].epoch, model.requests[index].epoch,
            "request epoch {index}: {path:?}"
        );
    }
    assert_eq!(runtime.token_live, model.token.live, "token: {path:?}");
    assert_eq!(
        runtime.token_owner, model.token.owner,
        "token owner: {path:?}"
    );
    assert_eq!(
        runtime.token_epoch, model.token.epoch,
        "token epoch: {path:?}"
    );
    assert_eq!(
        runtime.token_consumed, model.token_consumed,
        "token consumption: {path:?}"
    );
    assert_eq!(
        runtime.snapshot_live, model.snapshot.live,
        "snapshot: {path:?}"
    );
    assert_eq!(
        runtime.snapshot_owner, model.snapshot.owner,
        "snapshot owner: {path:?}"
    );
    assert_eq!(
        runtime.snapshot_epoch, model.snapshot.epoch,
        "snapshot epoch: {path:?}"
    );
    assert_eq!(
        runtime.snapshot_consumed, model.snapshot_consumed,
        "snapshot consumption: {path:?}"
    );
    assert_eq!(
        runtime.heap_object, model.heap_object,
        "heap object: {path:?}"
    );
    assert_eq!(
        runtime.heap_objects,
        usize::from(model.heap_objects),
        "heap object count: {path:?}"
    );
    assert_eq!(runtime.gc_root, model.gc_root, "GC root: {path:?}");
    assert_eq!(runtime.gc_epoch, model.gc_epoch, "GC epoch: {path:?}");
    assert_eq!(
        runtime.gc_consumed, model.gc_consumed,
        "GC lifecycle: {path:?}"
    );
    assert_eq!(
        runtime.reload,
        reload_state(model.reload),
        "reload: {path:?}"
    );
    assert_eq!(
        runtime.reload_completion_buffer,
        usize::from(model.reload_completion_buffer),
        "reload completion buffer: {path:?}"
    );
    assert_eq!(
        runtime.release_backlog,
        model.release_backlog.map(usize::from),
        "release backlog: {path:?}"
    );
    assert_eq!(
        runtime.state_registry_objects,
        model.state_registry_objects.map(usize::from),
        "state registry: {path:?}"
    );
    assert_eq!(
        runtime.runtime_host,
        runtime_host_state(model.runtime_host),
        "RuntimeHost: {path:?}"
    );
    assert_eq!(
        runtime.terminal_records,
        usize::from(model.terminal_records),
        "terminal records: {path:?}"
    );

    let live_tasks = model
        .tasks
        .iter()
        .filter(|task| task_is_live(task.state))
        .count();
    let scheduler_tokens = model
        .scheduler
        .iter()
        .filter(|scheduled| **scheduled)
        .count();
    let live_requests = model
        .requests
        .iter()
        .filter(|request| request.state == RealmV5RequestState::Pending)
        .count();
    let completion_reservations = model
        .requests
        .iter()
        .filter(|request| {
            request.state == RealmV5RequestState::Pending
                || (request.state == RealmV5RequestState::Late
                    && model.retired_epochs.iter().any(|retired| {
                        matches!(
                            retired,
                            RealmV5RetiredEpoch::Retired(epoch)
                                | RealmV5RetiredEpoch::Drained(epoch)
                                if *epoch == request.epoch
                        )
                    }))
        })
        .count();
    let release_records: usize = model
        .release_backlog
        .iter()
        .map(|count| usize::from(*count))
        .sum();
    let state_objects: usize = model
        .state_registry_objects
        .iter()
        .map(|count| usize::from(*count))
        .sum();
    let retired_epochs = model
        .retired_epochs
        .iter()
        .filter(|epoch| matches!(epoch, RealmV5RetiredEpoch::Retired(_)))
        .count();
    assert_eq!(
        runtime.ledger.task_slots, live_tasks,
        "task ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.continuations, live_tasks,
        "continuation ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.scheduler_tokens, scheduler_tokens,
        "scheduler ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.requests, live_requests,
        "request ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.completion_reservations, completion_reservations,
        "completion ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.completion_queued, 0,
        "queued completion ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.tokens,
        usize::from(model.token.live),
        "token ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.snapshots,
        usize::from(model.snapshot.live),
        "snapshot ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.release_records, release_records,
        "release ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.heap_objects,
        usize::from(model.heap_objects),
        "heap ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.state_objects, state_objects,
        "state ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.retired_epochs, retired_epochs,
        "retired ledger: {path:?}"
    );
    assert_eq!(
        runtime.ledger.terminal_records,
        usize::from(model.terminal_records),
        "terminal ledger: {path:?}"
    );
}

const fn runtime_event(event: RealmV5Event) -> RealmV5RuntimeEvent {
    match event {
        RealmV5Event::TaskAdmission => RealmV5RuntimeEvent::TaskAdmission,
        RealmV5Event::PollTask => RealmV5RuntimeEvent::PollTask,
        RealmV5Event::FuelYield => RealmV5RuntimeEvent::FuelYield,
        RealmV5Event::ExplicitYield => RealmV5RuntimeEvent::ExplicitYield,
        RealmV5Event::ResumeTask => RealmV5RuntimeEvent::ResumeTask,
        RealmV5Event::TaskComplete => RealmV5RuntimeEvent::TaskComplete,
        RealmV5Event::HostWait => RealmV5RuntimeEvent::HostWait,
        RealmV5Event::HostComplete => RealmV5RuntimeEvent::HostComplete,
        RealmV5Event::Cancel => RealmV5RuntimeEvent::Cancel,
        RealmV5Event::Cleanup => RealmV5RuntimeEvent::Cleanup,
        RealmV5Event::BeginReload => RealmV5RuntimeEvent::BeginReload,
        RealmV5Event::Quiesce => RealmV5RuntimeEvent::Quiesce,
        RealmV5Event::Migration => RealmV5RuntimeEvent::Migration,
        RealmV5Event::Rollback => RealmV5RuntimeEvent::Rollback,
        RealmV5Event::Commit => RealmV5RuntimeEvent::Commit,
        RealmV5Event::ActivationFault => RealmV5RuntimeEvent::ActivationFault,
        RealmV5Event::LateCompletion => RealmV5RuntimeEvent::LateCompletion,
        RealmV5Event::TokenAcquire => RealmV5RuntimeEvent::TokenAcquire,
        RealmV5Event::TokenRelease => RealmV5RuntimeEvent::TokenRelease,
        RealmV5Event::SnapshotAcquire => RealmV5RuntimeEvent::SnapshotAcquire,
        RealmV5Event::SnapshotRelease => RealmV5RuntimeEvent::SnapshotRelease,
        RealmV5Event::ReleaseDrain => RealmV5RuntimeEvent::ReleaseDrain,
        RealmV5Event::GcRootAttach => RealmV5RuntimeEvent::GcRootAttach,
        RealmV5Event::GcRootDrop => RealmV5RuntimeEvent::GcRootDrop,
        RealmV5Event::GcCollect => RealmV5RuntimeEvent::GcCollect,
        RealmV5Event::RetiredEpochReap(index) => RealmV5RuntimeEvent::RetiredEpochReap(index),
        RealmV5Event::RuntimeHostBeginClose => RealmV5RuntimeEvent::RuntimeHostBeginClose,
        RealmV5Event::RuntimeHostFinishClose => RealmV5RuntimeEvent::RuntimeHostFinishClose,
    }
}

const fn runtime_rejection(rejection: RealmV5RejectionClass) -> RuntimeRealmV5RejectionClass {
    match rejection {
        RealmV5RejectionClass::InvalidTaskState => RuntimeRealmV5RejectionClass::InvalidTaskState,
        RealmV5RejectionClass::InvalidReloadState => {
            RuntimeRealmV5RejectionClass::InvalidReloadState
        }
        RealmV5RejectionClass::InvalidRequestState => {
            RuntimeRealmV5RejectionClass::InvalidRequestState
        }
        RealmV5RejectionClass::HostNotOpen => RuntimeRealmV5RejectionClass::HostNotOpen,
        RealmV5RejectionClass::Capacity => RuntimeRealmV5RejectionClass::Capacity,
        RealmV5RejectionClass::RootConflict => RuntimeRealmV5RejectionClass::RootConflict,
        RealmV5RejectionClass::EpochConflict => RuntimeRealmV5RejectionClass::EpochConflict,
    }
}

const fn task_state(state: RealmV5TaskState) -> RealmV5RuntimeTaskState {
    match state {
        RealmV5TaskState::Vacant => RealmV5RuntimeTaskState::Vacant,
        RealmV5TaskState::Ready => RealmV5RuntimeTaskState::Ready,
        RealmV5TaskState::Running => RealmV5RuntimeTaskState::Running,
        RealmV5TaskState::FuelYielded => RealmV5RuntimeTaskState::FuelYielded,
        RealmV5TaskState::ExplicitYielded => RealmV5RuntimeTaskState::ExplicitYielded,
        RealmV5TaskState::Waiting => RealmV5RuntimeTaskState::Waiting,
        RealmV5TaskState::ReloadPaused => RealmV5RuntimeTaskState::ReloadPaused,
        RealmV5TaskState::Cancelling => RealmV5RuntimeTaskState::Cancelling,
        RealmV5TaskState::Cleanup => RealmV5RuntimeTaskState::Cleanup,
        RealmV5TaskState::Completed => RealmV5RuntimeTaskState::Completed,
        RealmV5TaskState::Cancelled => RealmV5RuntimeTaskState::Cancelled,
    }
}

const fn task_execution(state: RealmV5TaskState) -> RealmV5RuntimeExecution {
    match state {
        RealmV5TaskState::Vacant | RealmV5TaskState::Completed | RealmV5TaskState::Cancelled => {
            RealmV5RuntimeExecution::None
        }
        RealmV5TaskState::Ready => RealmV5RuntimeExecution::Ready,
        RealmV5TaskState::Running => RealmV5RuntimeExecution::Running,
        RealmV5TaskState::FuelYielded => RealmV5RuntimeExecution::FuelYielded,
        RealmV5TaskState::ExplicitYielded => RealmV5RuntimeExecution::ExplicitYielded,
        RealmV5TaskState::Waiting => RealmV5RuntimeExecution::Waiting,
        RealmV5TaskState::ReloadPaused => RealmV5RuntimeExecution::ReloadPaused,
        RealmV5TaskState::Cancelling => RealmV5RuntimeExecution::Cancelling,
        RealmV5TaskState::Cleanup => RealmV5RuntimeExecution::Cleanup,
    }
}

const fn request_state(state: RealmV5RequestState) -> RealmV5RuntimeRequestState {
    match state {
        RealmV5RequestState::Vacant => RealmV5RuntimeRequestState::Vacant,
        RealmV5RequestState::Pending => RealmV5RuntimeRequestState::Pending,
        RealmV5RequestState::Buffered => RealmV5RuntimeRequestState::Buffered,
        RealmV5RequestState::Completed => RealmV5RuntimeRequestState::Completed,
        RealmV5RequestState::Late => RealmV5RuntimeRequestState::Late,
    }
}

const fn reload_state(state: RealmV5ReloadState) -> RealmV5RuntimeReloadState {
    match state {
        RealmV5ReloadState::Idle => RealmV5RuntimeReloadState::Idle,
        RealmV5ReloadState::Prepared => RealmV5RuntimeReloadState::Prepared,
        RealmV5ReloadState::Quiesced => RealmV5RuntimeReloadState::Quiesced,
        RealmV5ReloadState::Migrated => RealmV5RuntimeReloadState::Migrated,
        RealmV5ReloadState::ActivationFaulted => RealmV5RuntimeReloadState::ActivationFaulted,
    }
}

const fn retired_epoch(epoch: RealmV5RetiredEpoch) -> RealmV5RuntimeRetiredEpoch {
    match epoch {
        RealmV5RetiredEpoch::Vacant => RealmV5RuntimeRetiredEpoch::Vacant,
        RealmV5RetiredEpoch::Retired(epoch) => RealmV5RuntimeRetiredEpoch::Retired(epoch),
        RealmV5RetiredEpoch::Drained(epoch) => RealmV5RuntimeRetiredEpoch::Drained(epoch),
    }
}

const fn runtime_host_state(
    state: nexa_model::realm_v5::RealmV5RuntimeHostState,
) -> RuntimeHostState {
    match state {
        nexa_model::realm_v5::RealmV5RuntimeHostState::Open => RuntimeHostState::Open,
        nexa_model::realm_v5::RealmV5RuntimeHostState::Closing => RuntimeHostState::Closing,
        nexa_model::realm_v5::RealmV5RuntimeHostState::Closed => RuntimeHostState::Closed,
    }
}

const fn task_is_live(state: RealmV5TaskState) -> bool {
    !matches!(
        state,
        RealmV5TaskState::Vacant | RealmV5TaskState::Completed | RealmV5TaskState::Cancelled
    )
}
