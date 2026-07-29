use nexa_model::realm::{
    CURRENT_REALM_EVENTS, RealmEvent, RealmModel, RealmRejection, RealmSnapshot,
};
use nexa_runtime::model_adapter::{
    RealmRuntimeModelAdapter, RuntimeInvocationCounters, RuntimeRealmEvent, RuntimeRealmRejection,
    RuntimeRealmSnapshot, RuntimeRequestLifecycle,
};

#[test]
fn all_realm_event_sequences_through_length_four_match_real_runtime() {
    let mut executed = 0;
    let mut sequence = Vec::with_capacity(4);
    for length in 0..=4 {
        visit_sequences(length, &mut sequence, &mut executed);
    }
    assert_eq!(executed, 1 + 9 + 81 + 729 + 6_561);
}

#[test]
#[allow(clippy::too_many_lines)]
fn high_risk_long_sequences_match_real_runtime() {
    let sequences = [
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::RestartReload,
            RealmEvent::LateCompletion,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::MigrationFailure,
            RealmEvent::LateCompletion,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::Cancel,
            RealmEvent::LateCompletion,
        ],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
        ],
        vec![RealmEvent::ActivationFailure, RealmEvent::Spawn],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
            RealmEvent::CompleteRequest,
        ],
        vec![RealmEvent::Spawn, RealmEvent::Cancel, RealmEvent::Cancel],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::RealmDrop,
            RealmEvent::LateCompletion,
        ],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::RealmDrop,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
            RealmEvent::Poll,
        ],
        vec![
            RealmEvent::Poll,
            RealmEvent::Cancel,
            RealmEvent::CompleteRequest,
        ],
        vec![
            RealmEvent::MigrationFailure,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::MigrationFailure,
            RealmEvent::ActivationFailure,
        ],
        vec![
            RealmEvent::ActivationFailure,
            RealmEvent::RestartReload,
            RealmEvent::RealmDrop,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::LateCompletion,
            RealmEvent::CompleteRequest,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Cancel,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::MigrationFailure,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::Cancel,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::RealmDrop,
            RealmEvent::RealmDrop,
            RealmEvent::RestartReload,
        ],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
            RealmEvent::Cancel,
            RealmEvent::RealmDrop,
        ],
        vec![
            RealmEvent::MigrationFailure,
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
            RealmEvent::RealmDrop,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::Cancel,
            RealmEvent::LateCompletion,
            RealmEvent::RealmDrop,
        ],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::RestartReload,
            RealmEvent::LateCompletion,
        ],
        vec![RealmEvent::RestartReload, RealmEvent::RestartReload],
        vec![
            RealmEvent::ActivationFailure,
            RealmEvent::RestartReload,
            RealmEvent::Spawn,
        ],
        vec![
            RealmEvent::Spawn,
            RealmEvent::Poll,
            RealmEvent::CompleteRequest,
            RealmEvent::Spawn,
        ],
        vec![RealmEvent::Spawn, RealmEvent::Cancel, RealmEvent::Spawn],
        vec![RealmEvent::Spawn, RealmEvent::Spawn],
        vec![
            RealmEvent::RestartReload,
            RealmEvent::LateCompletion,
            RealmEvent::RestartReload,
        ],
    ];

    assert!(sequences.len() >= 30);
    for sequence in sequences {
        compare_sequence(&sequence);
    }
}

fn visit_sequences(remaining: usize, sequence: &mut Vec<RealmEvent>, executed: &mut usize) {
    if remaining == 0 {
        compare_sequence(sequence);
        *executed += 1;
        return;
    }
    for event in CURRENT_REALM_EVENTS {
        sequence.push(event);
        visit_sequences(remaining - 1, sequence, executed);
        sequence.pop();
    }
}

fn compare_sequence(sequence: &[RealmEvent]) {
    let mut model = RealmModel::default();
    let mut runtime = RealmRuntimeModelAdapter::default();
    assert_state(sequence, 0, &model, &runtime);

    for (index, event) in sequence.iter().copied().enumerate() {
        let model_before = model.snapshot();
        let runtime_before = runtime.state_fingerprint();
        let counters_before = runtime.invocation_counters();
        let had_physical_ticket = runtime.has_physical_completion_ticket();
        let had_detached_ticket = runtime.has_detached_physical_ticket();
        let model_result = model.apply(event);
        let runtime_result = runtime.apply(runtime_event(event));
        let counters_after = runtime.invocation_counters();
        let prefix = &sequence[..=index];

        assert_eq!(
            rejection_name(model_result.as_ref().err()),
            runtime_rejection_name(runtime_result.as_ref().err()),
            "accept/reject mismatch\nprefix: {prefix:?}\nmodel: {model_result:?}\nruntime: \
             {runtime_result:?}\nmodel snapshot: {:?}\nruntime snapshot: {:?}",
            model.snapshot(),
            runtime_snapshot(runtime.snapshot()),
        );
        if model_result.is_err() {
            assert_eq!(
                model.snapshot(),
                model_before,
                "rejected model event mutated state\nprefix: {prefix:?}"
            );
            assert_eq!(
                runtime.state_fingerprint(),
                runtime_before,
                "rejected Runtime API mutated snapshot, ledger, or Host queues\nprefix: {prefix:?}"
            );
        }
        let realm_was_dropped = matches!(runtime_result, Err(RuntimeRealmRejection::RealmDropped));
        let expected_attempt = !realm_was_dropped
            && !matches!(event, RealmEvent::RealmDrop)
            && (had_physical_ticket || event != RealmEvent::LateCompletion);
        assert_eq!(
            counters_after.total(),
            counters_before.total() + u64::from(expected_attempt),
            "Runtime API attempt evidence mismatch\nprefix: {prefix:?}\nbefore: \
             {counters_before:?}\nafter: {counters_after:?}"
        );
        if expected_attempt {
            assert_eq!(
                event_counter(
                    counters_after,
                    event,
                    runtime_before.snapshot.request,
                    had_detached_ticket,
                ),
                event_counter(
                    counters_before,
                    event,
                    runtime_before.snapshot.request,
                    had_detached_ticket,
                ) + 1,
                "corresponding Runtime API counter did not advance\nprefix: {prefix:?}"
            );
        }
        assert_state(prefix, index + 1, &model, &runtime);
    }
}

fn assert_state(
    prefix: &[RealmEvent],
    step: usize,
    model: &RealmModel,
    runtime: &RealmRuntimeModelAdapter,
) {
    let model_snapshot = model.snapshot();
    let real_snapshot = runtime_snapshot(runtime.snapshot());
    assert_eq!(
        model_snapshot, real_snapshot,
        "snapshot mismatch at step {step}\nprefix: {prefix:?}\nmodel snapshot: \
         {model_snapshot:?}\nruntime snapshot: {real_snapshot:?}"
    );
    assert!(
        model.invariants_hold(),
        "model invariant failed at step {step}\nprefix: {prefix:?}\nsnapshot: {model_snapshot:?}"
    );
    assert!(
        runtime.invariants_hold(),
        "real resource invariant failed at step {step}\nprefix: {prefix:?}\nsnapshot: \
         {real_snapshot:?}"
    );
}

fn event_counter(
    counters: RuntimeInvocationCounters,
    event: RealmEvent,
    request: RuntimeRequestLifecycle,
    had_detached_ticket: bool,
) -> u64 {
    match event {
        RealmEvent::Spawn => counters.spawn_attempts,
        RealmEvent::Poll => counters.poll_attempts,
        RealmEvent::Cancel => counters.cancel_attempts,
        RealmEvent::RestartReload
        | RealmEvent::MigrationFailure
        | RealmEvent::ActivationFailure => counters.reload_attempts,
        RealmEvent::CompleteRequest if request == RuntimeRequestLifecycle::Pending => {
            counters.physical_completion_attempts
        }
        RealmEvent::LateCompletion
            if request == RuntimeRequestLifecycle::Detached && had_detached_ticket =>
        {
            counters.physical_completion_attempts
        }
        RealmEvent::CompleteRequest | RealmEvent::LateCompletion => counters.completion_attempts,
        RealmEvent::RealmDrop => counters.total(),
    }
}

const fn rejection_name(rejection: Option<&RealmRejection>) -> Option<&'static str> {
    match rejection {
        None => None,
        Some(RealmRejection::InvalidTaskState) => Some("InvalidTaskState"),
        Some(RealmRejection::InvalidRequestState) => Some("InvalidRequestState"),
        Some(RealmRejection::InvalidReloadState) => Some("InvalidReloadState"),
        Some(RealmRejection::RealmDropped) => Some("RealmDropped"),
    }
}

const fn runtime_rejection_name(rejection: Option<&RuntimeRealmRejection>) -> Option<&'static str> {
    match rejection {
        None => None,
        Some(RuntimeRealmRejection::InvalidTaskState) => Some("InvalidTaskState"),
        Some(RuntimeRealmRejection::InvalidRequestState) => Some("InvalidRequestState"),
        Some(RuntimeRealmRejection::InvalidReloadState) => Some("InvalidReloadState"),
        Some(RuntimeRealmRejection::RealmDropped) => Some("RealmDropped"),
    }
}

const fn runtime_event(event: RealmEvent) -> RuntimeRealmEvent {
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

const fn runtime_snapshot(snapshot: RuntimeRealmSnapshot) -> RealmSnapshot {
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
            RuntimeRequestLifecycle::Completed => RequestLifecycle::Completed,
            RuntimeRequestLifecycle::Cancelled => RequestLifecycle::Cancelled,
            RuntimeRequestLifecycle::Detached => RequestLifecycle::Detached,
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
        cancelled_requests: snapshot.cancelled_requests,
        detached_requests: snapshot.detached_requests,
        late_completions_discarded: snapshot.late_completions_discarded,
        publications: snapshot.publications,
    }
}
