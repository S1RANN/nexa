use nexa_model::realm::{RealmEvent, RealmModel, RealmSnapshot};
use nexa_runtime::model_adapter::{
    RealmRuntimeModelAdapter, RuntimeRealmEvent, RuntimeRealmSnapshot,
};

#[test]
fn current_realm_restart_sequences_match() {
    let sequences: &[&[RealmEvent]] = &[
        &[
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
        ],
        &[
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::RestartReload,
            RealmEvent::LateCompletion,
        ],
        &[
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::MigrationFailure,
            RealmEvent::LateCompletion,
        ],
        &[
            RealmEvent::Spawn,
            RealmEvent::ActivationFailure,
            RealmEvent::RealmDrop,
        ],
    ];

    for sequence in sequences {
        let mut model = RealmModel::default();
        let mut runtime = RealmRuntimeModelAdapter::default();
        for event in *sequence {
            assert_eq!(
                model.apply(*event).is_ok(),
                runtime.apply(runtime_event(*event)).is_ok()
            );
            assert_eq!(model.snapshot(), runtime_snapshot(runtime.snapshot()));
            assert!(model.invariants_hold());
        }
    }
}

fn runtime_event(event: RealmEvent) -> RuntimeRealmEvent {
    match event {
        RealmEvent::Spawn => RuntimeRealmEvent::Spawn,
        RealmEvent::Poll => RuntimeRealmEvent::Poll,
        RealmEvent::CompleteRequest => RuntimeRealmEvent::CompleteRequest,
        RealmEvent::Cancel => RuntimeRealmEvent::Cancel,
        RealmEvent::RestartReload => RuntimeRealmEvent::RestartReload,
        RealmEvent::MigrationFailure => RuntimeRealmEvent::MigrationFailure,
        RealmEvent::ActivationFailure => RuntimeRealmEvent::ActivationFailure,
        RealmEvent::LateCompletion => RuntimeRealmEvent::LateCompletion,
        RealmEvent::RealmDrop => RuntimeRealmEvent::RealmDrop,
    }
}

fn runtime_snapshot(snapshot: RuntimeRealmSnapshot) -> RealmSnapshot {
    use nexa_model::realm::{ReloadLifecycle, RequestLifecycle, TaskLifecycle};
    use nexa_runtime::model_adapter::{
        RuntimeReloadLifecycle, RuntimeRequestLifecycle, RuntimeTaskLifecycle,
    };
    RealmSnapshot {
        task: match snapshot.task {
            RuntimeTaskLifecycle::Vacant => TaskLifecycle::Vacant,
            RuntimeTaskLifecycle::Ready => TaskLifecycle::Ready,
            RuntimeTaskLifecycle::Waiting => TaskLifecycle::Waiting,
            RuntimeTaskLifecycle::Terminal => TaskLifecycle::Terminal,
        },
        request: match snapshot.request {
            RuntimeRequestLifecycle::Vacant => RequestLifecycle::Vacant,
            RuntimeRequestLifecycle::Pending => RequestLifecycle::Pending,
            RuntimeRequestLifecycle::Detached => RequestLifecycle::Detached,
            RuntimeRequestLifecycle::Completed => RequestLifecycle::Completed,
        },
        reload: match snapshot.reload {
            RuntimeReloadLifecycle::Idle => ReloadLifecycle::Idle,
            RuntimeReloadLifecycle::Staging => ReloadLifecycle::Staging,
            RuntimeReloadLifecycle::Active => ReloadLifecycle::Active,
            RuntimeReloadLifecycle::ActivationFaulted => ReloadLifecycle::ActivationFaulted,
        },
        epoch: snapshot.epoch,
        task_resources: snapshot.task_resources,
        request_resources: snapshot.request_resources,
        cancelled_tasks: snapshot.cancelled_tasks,
        detached_requests: snapshot.detached_requests,
        late_completions_discarded: snapshot.late_completions_discarded,
        publications: snapshot.publications,
    }
}
