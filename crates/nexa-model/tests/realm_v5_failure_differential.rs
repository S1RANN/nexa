use nexa_runtime::model_adapter::{
    RealmV5RuntimeAdapter, RealmV5RuntimeApplyError, RealmV5RuntimeEvent,
    RealmV5RuntimeReloadState, RealmV5RuntimeRetiredEpoch, RealmV5RuntimeTaskState,
};
use nexa_runtime::{
    FailurePointStats, HeapError, Object, RealmError, RuntimeError, RuntimeFailureMode,
    RuntimeFailurePoint,
};

#[test]
fn production_failure_modes_and_stats_are_complete() {
    let mut runtime = RealmV5RuntimeAdapter::new();
    let injector = runtime.failure_injector().clone();

    injector.arm_once(RuntimeFailurePoint::HeapSlot);
    assert_eq!(
        runtime.realm_mut().allocate(Object::String("once".into())),
        Err(RealmError::Heap(HeapError::InjectedFailure(
            RuntimeFailurePoint::HeapSlot
        )))
    );
    assert!(
        runtime
            .realm_mut()
            .allocate(Object::String("once-recovered".into()))
            .is_ok()
    );
    assert_eq!(
        injector.stats(RuntimeFailurePoint::HeapSlot),
        FailurePointStats {
            attempted: 2,
            injected: 1,
        }
    );

    injector.arm_at(RuntimeFailurePoint::HeapSlot, 2).unwrap();
    assert!(
        runtime
            .realm_mut()
            .allocate(Object::String("at-first".into()))
            .is_ok()
    );
    assert_eq!(
        runtime
            .realm_mut()
            .allocate(Object::String("at-second".into())),
        Err(RealmError::Heap(HeapError::InjectedFailure(
            RuntimeFailurePoint::HeapSlot
        )))
    );

    injector.arm_always(RuntimeFailurePoint::HeapSlot);
    for value in ["always-first", "always-second"] {
        assert_eq!(
            runtime.realm_mut().allocate(Object::String(value.into())),
            Err(RealmError::Heap(HeapError::InjectedFailure(
                RuntimeFailurePoint::HeapSlot
            )))
        );
    }
    injector.disarm(RuntimeFailurePoint::HeapSlot);
    assert_eq!(
        injector.mode(RuntimeFailurePoint::HeapSlot),
        RuntimeFailureMode::Off
    );
    assert!(
        runtime
            .realm_mut()
            .allocate(Object::String("disarmed".into()))
            .is_ok()
    );
    assert_eq!(
        injector.stats(RuntimeFailurePoint::HeapSlot),
        FailurePointStats {
            attempted: 3,
            injected: 2,
        }
    );

    injector.arm_once(RuntimeFailurePoint::ScopeSlot);
    injector.arm_once(RuntimeFailurePoint::HeapSlot);
    assert!(matches!(
        runtime.realm_mut().create_scope(None),
        Err(RealmError::Runtime(RuntimeError::InjectedFailure(
            RuntimeFailurePoint::ScopeSlot
        )))
    ));
    assert!(matches!(
        runtime
            .realm_mut()
            .allocate(Object::String("ordered".into())),
        Err(RealmError::Heap(HeapError::InjectedFailure(
            RuntimeFailurePoint::HeapSlot
        )))
    ));
    injector.clear();
    for point in RuntimeFailurePoint::ALL {
        assert_eq!(injector.mode(point), RuntimeFailureMode::Off);
        assert_eq!(injector.stats(point), FailurePointStats::default());
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_failure_point_runs_inside_production_realm_boundaries() {
    let atomic_cases: &[(
        RuntimeFailurePoint,
        &[RealmV5RuntimeEvent],
        RealmV5RuntimeEvent,
    )] = &[
        (
            RuntimeFailurePoint::TaskSlot,
            &[],
            RealmV5RuntimeEvent::TaskAdmission,
        ),
        (
            RuntimeFailurePoint::SchedulerSlot,
            &[],
            RealmV5RuntimeEvent::TaskAdmission,
        ),
        (
            RuntimeFailurePoint::FrameSlot,
            &[],
            RealmV5RuntimeEvent::TaskAdmission,
        ),
        (
            RuntimeFailurePoint::HeapSlot,
            &[],
            RealmV5RuntimeEvent::GcRootAttach,
        ),
        (
            RuntimeFailurePoint::RequestSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::ExplicitYield,
            ],
            RealmV5RuntimeEvent::HostWait,
        ),
        (
            RuntimeFailurePoint::CompletionSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::ExplicitYield,
            ],
            RealmV5RuntimeEvent::HostWait,
        ),
        (
            RuntimeFailurePoint::ReleaseSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::ExplicitYield,
            ],
            RealmV5RuntimeEvent::HostWait,
        ),
        (
            RuntimeFailurePoint::SnapshotSlot,
            &[RealmV5RuntimeEvent::TaskAdmission],
            RealmV5RuntimeEvent::SnapshotAcquire,
        ),
        (
            RuntimeFailurePoint::MigrationObjectSlot,
            &[
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::Migration,
        ),
        (
            RuntimeFailurePoint::MigrationFieldSlot,
            &[
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::Migration,
        ),
        (
            RuntimeFailurePoint::MigrationForwardingSlot,
            &[
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::Migration,
        ),
        (
            RuntimeFailurePoint::ReloadCompletionSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::ExplicitYield,
                RealmV5RuntimeEvent::HostWait,
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::HostComplete,
        ),
    ];

    let mut covered = Vec::new();
    for &(point, path, event) in atomic_cases {
        let mut runtime = replay(path);
        let before = runtime.realm().inspection_snapshot();
        runtime.failure_injector().arm_once(point);
        assert_eq!(
            runtime.apply(event),
            Err(RealmV5RuntimeApplyError::InjectedFailure(point)),
            "{point:?} did not fail at its production boundary"
        );
        assert_eq!(
            runtime.realm().inspection_snapshot(),
            before,
            "{point:?} left a partial production mutation"
        );
        assert_eq!(runtime.failure_injector().stats(point).injected, 1);
        runtime
            .apply(event)
            .unwrap_or_else(|error| panic!("{point:?} did not recover: {error:?}"));
        covered.push(point);
    }

    assert_scope_slot_is_injected_by_realm_runtime();
    covered.push(RuntimeFailurePoint::ScopeSlot);
    assert_activation_trap_is_post_publication_and_exactly_once();
    covered.push(RuntimeFailurePoint::ActivationTrap);
    assert_cleanup_trap_is_terminal_and_exactly_once();
    covered.push(RuntimeFailurePoint::CleanupTrap);

    for point in RuntimeFailurePoint::ALL {
        assert!(
            covered.contains(&point),
            "missing production coverage for {point:?}"
        );
    }
}

fn assert_scope_slot_is_injected_by_realm_runtime() {
    let mut runtime = RealmV5RuntimeAdapter::new();
    let before = runtime.realm().inspection_snapshot();
    runtime
        .failure_injector()
        .arm_once(RuntimeFailurePoint::ScopeSlot);
    assert_eq!(
        runtime.realm_mut().create_scope(None),
        Err(RealmError::Runtime(RuntimeError::InjectedFailure(
            RuntimeFailurePoint::ScopeSlot
        )))
    );
    assert_eq!(runtime.realm().inspection_snapshot(), before);
    assert!(runtime.realm_mut().create_scope(None).is_ok());
}

fn assert_activation_trap_is_post_publication_and_exactly_once() {
    let mut runtime = replay(&[
        RealmV5RuntimeEvent::BeginReload,
        RealmV5RuntimeEvent::Quiesce,
        RealmV5RuntimeEvent::Migration,
    ]);
    runtime
        .failure_injector()
        .arm_once(RuntimeFailurePoint::ActivationTrap);
    assert_eq!(
        runtime.apply(RealmV5RuntimeEvent::Commit),
        Err(RealmV5RuntimeApplyError::InjectedFailure(
            RuntimeFailurePoint::ActivationTrap
        ))
    );

    let failed = runtime.snapshot().unwrap();
    assert_eq!(failed.active_epoch, 1);
    assert_eq!(failed.candidate_epoch, None);
    assert_eq!(failed.reload, RealmV5RuntimeReloadState::ActivationFaulted);
    assert_eq!(
        failed.retired_epochs[0],
        RealmV5RuntimeRetiredEpoch::Retired(0)
    );
    assert_eq!(failed.state_registry_objects[0], 0);
    assert_eq!(failed.state_registry_objects[1], 1);
    assert_eq!(
        runtime
            .realm()
            .inspection_snapshot()
            .reload
            .root_publications
            .len(),
        1
    );
    assert!(matches!(
        runtime.apply(RealmV5RuntimeEvent::Commit),
        Err(RealmV5RuntimeApplyError::Rejected(_))
    ));
    assert_eq!(runtime.snapshot().unwrap(), failed);
}

fn assert_cleanup_trap_is_terminal_and_exactly_once() {
    let mut runtime = replay(&[
        RealmV5RuntimeEvent::TaskAdmission,
        RealmV5RuntimeEvent::PollTask,
    ]);
    runtime
        .failure_injector()
        .arm_always(RuntimeFailurePoint::CleanupTrap);
    assert_eq!(
        runtime.apply(RealmV5RuntimeEvent::Cancel),
        Err(RealmV5RuntimeApplyError::InjectedFailure(
            RuntimeFailurePoint::CleanupTrap
        ))
    );

    let trapped = runtime.snapshot().unwrap();
    assert!(
        trapped
            .tasks
            .iter()
            .all(|task| task.state == RealmV5RuntimeTaskState::Trapped)
    );
    assert!(trapped.scheduler.iter().all(|scheduled| !*scheduled));
    assert_eq!(trapped.terminal_records, 2);
    assert_eq!(
        runtime
            .failure_injector()
            .stats(RuntimeFailurePoint::CleanupTrap)
            .injected,
        2
    );
    assert!(matches!(
        runtime.apply(RealmV5RuntimeEvent::Cleanup),
        Err(RealmV5RuntimeApplyError::Rejected(_))
    ));
    assert_eq!(runtime.snapshot().unwrap(), trapped);
}

fn replay(path: &[RealmV5RuntimeEvent]) -> RealmV5RuntimeAdapter {
    let mut runtime = RealmV5RuntimeAdapter::new();
    for &event in path {
        runtime
            .apply(event)
            .unwrap_or_else(|error| panic!("fixture rejected {event:?}: {error:?}"));
    }
    runtime
}
