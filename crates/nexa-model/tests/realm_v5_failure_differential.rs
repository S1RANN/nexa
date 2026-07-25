use nexa_runtime::RuntimeFailurePoint;
use nexa_runtime::model_adapter::{
    RealmV5RuntimeAdapter, RealmV5RuntimeApplyError, RealmV5RuntimeEvent, RealmV5RuntimeExecution,
    RealmV5RuntimeReloadState, RealmV5RuntimeRetiredEpoch, RealmV5RuntimeSnapshot,
    RealmV5RuntimeTaskState,
};

#[test]
#[allow(clippy::too_many_lines)]
fn every_failure_point_preserves_its_declared_runtime_boundary() {
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
            RuntimeFailurePoint::ScopeSlot,
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
            ],
            RealmV5RuntimeEvent::HostWait,
        ),
        (
            RuntimeFailurePoint::CompletionSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::HostWait,
            ],
            RealmV5RuntimeEvent::HostComplete,
        ),
        (
            RuntimeFailurePoint::ReleaseSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
                RealmV5RuntimeEvent::TokenAcquire,
            ],
            RealmV5RuntimeEvent::TokenRelease,
        ),
        (
            RuntimeFailurePoint::SnapshotSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::PollTask,
            ],
            RealmV5RuntimeEvent::SnapshotAcquire,
        ),
        (
            RuntimeFailurePoint::MigrationObjectSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::Migration,
        ),
        (
            RuntimeFailurePoint::MigrationFieldSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
                RealmV5RuntimeEvent::BeginReload,
                RealmV5RuntimeEvent::Quiesce,
            ],
            RealmV5RuntimeEvent::Migration,
        ),
        (
            RuntimeFailurePoint::MigrationForwardingSlot,
            &[
                RealmV5RuntimeEvent::TaskAdmission,
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
        let before = runtime.snapshot().unwrap();
        runtime.failure_injector().arm_once(point);
        assert_eq!(
            runtime.apply(event),
            Err(RealmV5RuntimeApplyError::InjectedFailure(point)),
            "{point:?} did not fail at its declared boundary"
        );
        let after = runtime.snapshot().unwrap();
        assert_eq!(after, before, "{point:?} left a partial mutation");
        assert_accounting_is_closed(after);
        covered.push(point);
    }

    assert_migration_failure_is_recoverable();
    covered.extend([
        RuntimeFailurePoint::MigrationObjectSlot,
        RuntimeFailurePoint::MigrationFieldSlot,
        RuntimeFailurePoint::MigrationForwardingSlot,
    ]);

    assert_activation_trap_is_post_publication_and_exactly_once();
    covered.push(RuntimeFailurePoint::ActivationTrap);

    assert_cleanup_trap_is_terminal_and_exactly_once();
    covered.push(RuntimeFailurePoint::CleanupTrap);

    for point in RuntimeFailurePoint::ALL {
        assert!(
            covered.contains(&point),
            "missing v5 differential coverage for {point:?}"
        );
    }
}

fn assert_migration_failure_is_recoverable() {
    let path = [
        RealmV5RuntimeEvent::TaskAdmission,
        RealmV5RuntimeEvent::BeginReload,
        RealmV5RuntimeEvent::Quiesce,
    ];
    for point in [
        RuntimeFailurePoint::MigrationObjectSlot,
        RuntimeFailurePoint::MigrationFieldSlot,
        RuntimeFailurePoint::MigrationForwardingSlot,
    ] {
        let mut runtime = replay(&path);
        let before = runtime.snapshot().unwrap();
        runtime.failure_injector().arm_once(point);
        assert_eq!(
            runtime.apply(RealmV5RuntimeEvent::Migration),
            Err(RealmV5RuntimeApplyError::InjectedFailure(point))
        );
        assert_eq!(runtime.snapshot().unwrap(), before);

        runtime.apply(RealmV5RuntimeEvent::Migration).unwrap();
        runtime.apply(RealmV5RuntimeEvent::Rollback).unwrap();
        let recovered = runtime.snapshot().unwrap();
        assert_eq!(recovered.reload, RealmV5RuntimeReloadState::Idle);
        assert_eq!(recovered.candidate_epoch, None);
        assert_eq!(recovered.state_registry_objects[1], 0);
        assert!(recovered.tasks.iter().all(|task| {
            task.state == RealmV5RuntimeTaskState::Ready
                && task.execution == RealmV5RuntimeExecution::Ready
        }));
        assert!(recovered.scheduler.iter().all(|scheduled| *scheduled));
        assert_accounting_is_closed(recovered);
    }
}

fn assert_activation_trap_is_post_publication_and_exactly_once() {
    let mut runtime = replay(&[
        RealmV5RuntimeEvent::TaskAdmission,
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
    assert_eq!(failed.state_registry_objects[0], 1);
    assert_eq!(failed.state_registry_objects[1], 1);
    assert!(failed.tasks.iter().all(|task| {
        task.state == RealmV5RuntimeTaskState::Cancelled
            && task.execution == RealmV5RuntimeExecution::None
    }));
    assert!(failed.scheduler.iter().all(|scheduled| !*scheduled));
    assert_eq!(failed.terminal_records, 2);
    assert_accounting_is_closed(failed);

    assert!(matches!(
        runtime.apply(RealmV5RuntimeEvent::Commit),
        Err(RealmV5RuntimeApplyError::Rejected(_))
    ));
    assert_eq!(runtime.snapshot().unwrap(), failed);
}

fn assert_cleanup_trap_is_terminal_and_exactly_once() {
    let mut runtime = replay(&[
        RealmV5RuntimeEvent::TaskAdmission,
        RealmV5RuntimeEvent::Cancel,
        RealmV5RuntimeEvent::Cleanup,
    ]);
    runtime
        .failure_injector()
        .arm_once(RuntimeFailurePoint::CleanupTrap);
    assert_eq!(
        runtime.apply(RealmV5RuntimeEvent::Cleanup),
        Err(RealmV5RuntimeApplyError::InjectedFailure(
            RuntimeFailurePoint::CleanupTrap
        ))
    );

    let trapped = runtime.snapshot().unwrap();
    assert!(trapped.tasks.iter().all(|task| {
        task.state == RealmV5RuntimeTaskState::Trapped
            && task.execution == RealmV5RuntimeExecution::None
    }));
    assert!(trapped.scheduler.iter().all(|scheduled| !*scheduled));
    assert_eq!(trapped.terminal_records, 2);
    assert_accounting_is_closed(trapped);

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

fn assert_accounting_is_closed(snapshot: RealmV5RuntimeSnapshot) {
    assert_eq!(
        snapshot.ledger.scheduler_tokens,
        snapshot
            .scheduler
            .iter()
            .filter(|scheduled| **scheduled)
            .count()
    );
    assert_eq!(snapshot.ledger.terminal_records, snapshot.terminal_records);
    assert_eq!(
        snapshot.ledger.state_objects,
        snapshot.state_registry_objects.iter().sum()
    );
    assert_eq!(
        snapshot.ledger.release_records,
        snapshot.release_backlog.iter().sum()
    );
}
