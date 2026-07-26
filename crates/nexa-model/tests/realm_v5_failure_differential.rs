use nexa_model::artifact::{
    MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, write_model_failure_artifact,
};
use nexa_runtime::model_adapter::{
    RealmV5RuntimeAdapter, RealmV5RuntimeApplyError, RealmV5RuntimeEvent,
    RealmV5RuntimeReloadState, RealmV5RuntimeRetiredEpoch, RealmV5RuntimeTaskState,
};
use nexa_runtime::{
    FailurePointStats, HeapError, Object, RealmError, RuntimeError, RuntimeFailureMode,
    RuntimeFailurePoint,
};
use serde_json::json;

#[test]
#[allow(clippy::too_many_lines)]
fn failure_artifact_serializes_real_realm_inspection() {
    let mut runtime = replay(&[
        RealmV5RuntimeEvent::BeginReload,
        RealmV5RuntimeEvent::Quiesce,
        RealmV5RuntimeEvent::Migration,
        RealmV5RuntimeEvent::Commit,
    ]);
    let before = runtime.realm().inspection_snapshot();
    runtime
        .failure_injector()
        .arm_once(RuntimeFailurePoint::HeapSlot);
    assert!(matches!(
        runtime
            .realm_mut()
            .allocate(Object::String("artifact".into())),
        Err(RealmError::Heap(HeapError::InjectedFailure(
            RuntimeFailurePoint::HeapSlot
        )))
    ));
    let after = runtime.realm().inspection_snapshot();
    assert_eq!(after, before);

    let runtime_before = inspection_value(&before);
    let runtime_after = inspection_value(&after);
    let failure_point_stats = json!(runtime.failure_injector().all_stats().map(
        |(point, stats)| json!({
            "point": format!("{point:?}"),
            "attempted": stats.attempted,
            "injected": stats.injected
        })
    ));
    let artifact = ModelFailureArtifact {
        format_version: MODEL_FAILURE_ARTIFACT_VERSION,
        commit_sha: "test".into(),
        runtime_kind: "RealmRuntime".into(),
        shadow_state_fields: 0,
        model_config: json!({"model": "realm-v5", "source": "production-inspection"}),
        path: vec![
            "BeginReload".into(),
            "Quiesce".into(),
            "Migration".into(),
            "Commit".into(),
        ],
        failure_event: "HeapSlot".into(),
        model_before: runtime_before.clone(),
        model_after: runtime_after.clone(),
        runtime_before,
        runtime_after,
        ledger: resource_ledger_value(before.resources),
        epochs: json!({
            "active": before.active_root.as_ref().map(|module| module.epoch),
            "retired": before.retired_epochs.iter().map(|epoch| epoch.epoch).collect::<Vec<_>>()
        }),
        tasks: json!(
            before
                .tasks
                .iter()
                .map(|task| format!("{:?}", task.state))
                .collect::<Vec<_>>()
        ),
        requests: json!(before.resources.requests),
        completions: completion_value(before.completion_accounting),
        releases: json!(
            before
                .runtime_host_releases
                .iter()
                .map(|release| release.epoch)
                .collect::<Vec<_>>()
        ),
        heap: json!({"objects": before.heap.live_objects, "capacity": before.heap.capacity}),
        roots: json!({
            "module_globals": before.roots.module_globals,
            "stateful_registry": before.roots.stateful_registry,
            "staging_heap": before.roots.staging_heap,
            "suspended_tasks": before.roots.suspended_tasks
        }),
        root_publications: json!(
            before
                .reload
                .root_publications
                .iter()
                .map(|publication| {
                    json!({
                        "publication_id": publication.publication_id,
                        "candidate_epoch": publication.candidate_epoch
                    })
                })
                .collect::<Vec<_>>()
        ),
        module_handles: json!(
            before
                .modules
                .iter()
                .map(|module| {
                    json!({
                        "module_id": module.module_id,
                        "generation": module.generation,
                        "epoch": module.epoch,
                        "lifecycle": format!("{:?}", module.lifecycle)
                    })
                })
                .collect::<Vec<_>>()
        ),
        completion_accounting: completion_value(before.completion_accounting),
        failure_point_stats,
        trace: json!(["production.heap.preflight"]),
        error_code: "NEXA_RUNTIME_INJECTED_FAILURE".into(),
    };
    let mut encoded = Vec::new();
    write_model_failure_artifact(&mut encoded, &artifact).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(parsed["runtime_kind"], "RealmRuntime");
    assert_eq!(parsed["shadow_state_fields"], 0);
    assert_eq!(parsed["runtime_before"], parsed["runtime_after"]);
    assert_eq!(parsed["root_publications"].as_array().unwrap().len(), 1);
    assert!(!parsed["module_handles"].as_array().unwrap().is_empty());
    let heap_stats = parsed["failure_point_stats"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stats| stats["point"] == "HeapSlot")
        .unwrap();
    assert_eq!(heap_stats["injected"], 1);
}

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
fn every_production_failure_point_is_differentially_atomic() {
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
        if matches!(
            point,
            RuntimeFailurePoint::MigrationObjectSlot
                | RuntimeFailurePoint::MigrationFieldSlot
                | RuntimeFailurePoint::MigrationForwardingSlot
        ) {
            runtime.apply(RealmV5RuntimeEvent::Rollback).unwrap();
            let recovered = runtime.realm().inspection_snapshot();
            assert_eq!(
                recovered.reload.state,
                nexa_runtime::ReloadInspectionState::Idle
            );
            assert!(recovered.candidate_root.is_none());
        }
        covered.push(point);
    }

    assert_scope_slot_is_injected_by_realm_runtime();
    covered.push(RuntimeFailurePoint::ScopeSlot);
    assert_activation_trap_is_post_publication_and_exactly_once();
    covered.push(RuntimeFailurePoint::ActivationTrap);
    assert_cleanup_trap_is_terminal_and_exactly_once();
    covered.push(RuntimeFailurePoint::CleanupTrap);

    for point in RuntimeFailurePoint::REALM_PRODUCTION {
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
    assert_eq!(failed.state_registry_objects[0], 0);
    assert_eq!(failed.state_registry_objects[1], 1);
    assert_eq!(failed.terminal_records, 2);
    assert_eq!(failed.ledger.task_slots, 0);
    assert_eq!(failed.ledger.continuations, 0);
    assert_eq!(failed.ledger.scheduler_tokens, 0);
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
        RealmV5RuntimeEvent::TokenAcquire,
        RealmV5RuntimeEvent::SnapshotAcquire,
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
    assert_eq!(trapped.ledger.tokens, 0);
    assert_eq!(trapped.ledger.snapshots, 0);
    assert_eq!(trapped.ledger.release_records, 2);
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
    runtime.apply(RealmV5RuntimeEvent::ReleaseDrain).unwrap();
    assert_eq!(runtime.snapshot().unwrap().ledger.release_records, 0);
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

fn inspection_value(inspection: &nexa_runtime::RealmInspectionSnapshot) -> serde_json::Value {
    json!({
        "active_epoch": inspection.active_root.as_ref().map(|module| module.epoch),
        "candidate_epoch": inspection.candidate_root.as_ref().map(|module| module.epoch),
        "module_count": inspection.modules.len(),
        "retired_count": inspection.retired_epochs.len(),
        "task_count": inspection.tasks.len(),
        "resource_ledger": resource_ledger_value(inspection.resources),
        "completion_accounting": completion_value(inspection.completion_accounting),
        "reload_state": format!("{:?}", inspection.reload.state),
        "reload_buffer": inspection.reload.completion_buffer,
        "root_publications": inspection.reload.root_publications.len(),
        "heap_objects": inspection.heap.live_objects,
        "terminal_records": inspection.terminal_records.len(),
        "runtime_host": format!("{:?}", inspection.runtime_host)
    })
}

fn resource_ledger_value(ledger: nexa_runtime::RuntimeResourceLedger) -> serde_json::Value {
    json!({
        "tasks": ledger.tasks,
        "scopes": ledger.scopes,
        "continuations": ledger.continuations,
        "scheduler_tokens": ledger.scheduler_tokens,
        "requests": ledger.requests,
        "completion_reservations": ledger.completion_reservations,
        "tokens": ledger.tokens,
        "snapshots": ledger.snapshots,
        "release_reservations": ledger.release_reservations,
        "queued_releases": ledger.queued_releases,
        "heap_objects": ledger.heap_objects,
        "state_objects": ledger.state_objects,
        "retired_epochs": ledger.retired_epochs
    })
}

fn completion_value(accounting: nexa_runtime::CompletionAccounting) -> serde_json::Value {
    json!({
        "reserved": accounting.reserved,
        "queued": accounting.queued,
        "delivered": accounting.delivered,
        "cancelled": accounting.cancelled,
        "abandoned": accounting.abandoned,
        "reload_discarded": accounting.reload_discarded,
        "late_discarded": accounting.late_discarded
    })
}
