use nexa_model::realm_v4::{
    RealmV4CancelKind, RealmV4CompletionState, RealmV4Config, RealmV4Event, RealmV4ReloadState,
    RealmV4RoutingEvent, RealmV4RoutingWorld, RealmV4TaskState, RealmV4World, explore_realm_v4,
    explore_realm_v4_routing,
};
use nexa_runtime::model_adapter::{
    RealmV4ExecutionKind, RealmV4RoutingRuntimeAdapter, RealmV4RoutingRuntimeCompletionState,
    RealmV4RoutingRuntimeEvent, RealmV4RoutingRuntimeReloadState, RealmV4RoutingRuntimeSnapshot,
    RealmV4RuntimeAdapter, RealmV4RuntimeCancelKind, RealmV4RuntimeEvent, RealmV4RuntimeSnapshot,
    RealmV4RuntimeTaskState, RealmV4RuntimeTerminalReason,
};

#[test]
fn every_realm_v4_shortest_path_replays_against_runtime() {
    let report = explore_realm_v4(RealmV4Config {
        max_depth: 16,
        max_worlds: 4_096,
    });
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!report.truncated);
    assert_eq!(report.shortest_paths.len(), report.visited_worlds);

    for path in report.shortest_paths {
        let replay_path = path.clone();
        let mut model = RealmV4World::default();
        let mut runtime = RealmV4RuntimeAdapter::new();
        assert_matches(model, runtime.snapshot().unwrap()).unwrap();
        for event in path {
            model.apply(event).unwrap();
            if let Err(error) = runtime.apply(runtime_event(event)) {
                write_failure(&replay_path, event, model, None, &error);
                panic!("{error}; event={event:?}; path={replay_path:?}");
            }
            let snapshot = runtime.snapshot().unwrap();
            if let Err(error) = assert_matches(model, snapshot) {
                write_failure(&replay_path, event, model, Some(snapshot), &error);
                panic!("{error}; event={event:?}; path={replay_path:?}");
            }
        }
    }
}

#[test]
fn every_realm_v4_dual_module_routing_path_replays_against_realm_runtime() {
    let report = explore_realm_v4_routing(RealmV4Config {
        max_depth: 8,
        max_worlds: 256,
    });
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert!(!report.truncated);
    assert_eq!(report.shortest_paths.len(), report.visited_worlds);

    for path in report.shortest_paths {
        let replay_path = path.clone();
        let mut model = RealmV4RoutingWorld::default();
        let mut runtime = RealmV4RoutingRuntimeAdapter::new();
        assert_routing_matches(model, runtime.snapshot()).unwrap();
        for event in path {
            model.apply(event).unwrap();
            if let Err(error) = runtime.apply(routing_runtime_event(event)) {
                write_routing_failure(&replay_path, event, model, None, &error);
                panic!("{error}; event={event:?}; path={replay_path:?}");
            }
            let snapshot = runtime.snapshot();
            if let Err(error) = assert_routing_matches(model, snapshot) {
                write_routing_failure(&replay_path, event, model, Some(snapshot), &error);
                panic!("{error}; event={event:?}; path={replay_path:?}");
            }
        }
    }
}

fn assert_matches(model: RealmV4World, runtime: RealmV4RuntimeSnapshot) -> Result<(), String> {
    if runtime.task_state != task_state(model.task) {
        return Err(format!(
            "task state mismatch: model={:?}, runtime={:?}",
            model.task, runtime.task_state
        ));
    }
    if runtime.execution != execution(model.task) {
        return Err(format!(
            "execution mismatch: model={:?}, runtime={:?}",
            execution(model.task),
            runtime.execution
        ));
    }
    if runtime.scheduler_tokens != usize::from(model.scheduler_tokens) {
        return Err(format!(
            "scheduler mismatch: model={}, runtime={}",
            model.scheduler_tokens, runtime.scheduler_tokens
        ));
    }
    if runtime.request_owned != model.has_request() {
        return Err(format!(
            "request mismatch: model={}, runtime={}",
            model.has_request(),
            runtime.request_owned
        ));
    }
    if runtime.continuation_owned != model.has_continuation() {
        return Err(format!(
            "continuation mismatch: model={}, runtime={}",
            model.has_continuation(),
            runtime.continuation_owned
        ));
    }
    if runtime.reload_checkpoint != model.reload_active() {
        return Err(format!(
            "reload checkpoint mismatch: model={}, runtime={}",
            model.reload_active(),
            runtime.reload_checkpoint
        ));
    }
    if runtime.cancel_kind != cancel_kind(model.cancel_kind()) {
        return Err(format!(
            "cancel kind mismatch: model={:?}, runtime={:?}",
            model.cancel_kind(),
            runtime.cancel_kind
        ));
    }
    if runtime.user_defer != model.has_user_defer() {
        return Err(format!(
            "user defer mismatch: model={}, runtime={}",
            model.has_user_defer(),
            runtime.user_defer
        ));
    }
    if runtime.terminal_reason != terminal_reason(model.task) {
        return Err(format!(
            "terminal mismatch: model={:?}, runtime={:?}",
            terminal_reason(model.task),
            runtime.terminal_reason
        ));
    }
    if runtime.vm_resources.task_slots != usize::from(model.has_continuation()) {
        return Err(format!(
            "task resource mismatch: model={}, runtime={}",
            usize::from(model.has_continuation()),
            runtime.vm_resources.task_slots
        ));
    }
    if runtime.vm_resources.requests != usize::from(model.has_request()) {
        return Err(format!(
            "request resource mismatch: model={}, runtime={}",
            usize::from(model.has_request()),
            runtime.vm_resources.requests
        ));
    }
    Ok(())
}

fn assert_routing_matches(
    model: RealmV4RoutingWorld,
    runtime: RealmV4RoutingRuntimeSnapshot,
) -> Result<(), String> {
    if runtime.reload != routing_reload_state(model.reload) {
        return Err(format!(
            "reload mismatch: model={:?}, runtime={:?}",
            model.reload, runtime.reload
        ));
    }
    if runtime.completion_a != routing_completion_state(model.completion_a) {
        return Err(format!(
            "module A completion mismatch: model={:?}, runtime={:?}",
            model.completion_a, runtime.completion_a
        ));
    }
    if runtime.completion_b != routing_completion_state(model.completion_b) {
        return Err(format!(
            "module B completion mismatch: model={:?}, runtime={:?}",
            model.completion_b, runtime.completion_b
        ));
    }
    if runtime.pending_completions != runtime.request_reservations {
        return Err(format!(
            "completion reservation mismatch: host={}, resources={}",
            runtime.pending_completions, runtime.request_reservations
        ));
    }
    if model.completion_a == RealmV4CompletionState::Buffered && runtime.buffered == 0 {
        return Err("buffered module A completion lacks accounting".into());
    }
    if model.completion_a == RealmV4CompletionState::Replayed && runtime.replayed == 0 {
        return Err("replayed module A completion lacks accounting".into());
    }
    if model.completion_a == RealmV4CompletionState::Discarded
        && matches!(
            model.reload,
            RealmV4ReloadState::Committed | RealmV4ReloadState::ActivationFaulted
        )
        && runtime.buffered != 0
        && runtime.discarded_after_commit == 0
    {
        return Err("buffered module A completion lacks discard accounting".into());
    }
    Ok(())
}

const fn runtime_event(event: RealmV4Event) -> RealmV4RuntimeEvent {
    match event {
        RealmV4Event::Poll => RealmV4RuntimeEvent::Poll,
        RealmV4Event::FuelExhaust => RealmV4RuntimeEvent::FuelExhaust,
        RealmV4Event::ResumeFuel => RealmV4RuntimeEvent::ResumeFuel,
        RealmV4Event::ExplicitYield => RealmV4RuntimeEvent::ExplicitYield,
        RealmV4Event::ResumeExplicit => RealmV4RuntimeEvent::ResumeExplicit,
        RealmV4Event::BeginRequest => RealmV4RuntimeEvent::BeginRequest,
        RealmV4Event::CompleteRequest => RealmV4RuntimeEvent::CompleteRequest,
        RealmV4Event::BeginReload => RealmV4RuntimeEvent::BeginReload,
        RealmV4Event::RollbackReload => RealmV4RuntimeEvent::RollbackReload,
        RealmV4Event::RequestCancel => RealmV4RuntimeEvent::RequestCancel,
        RealmV4Event::ReloadCommitCancel => RealmV4RuntimeEvent::ReloadCommitCancel,
        RealmV4Event::ReachSafepoint => RealmV4RuntimeEvent::ReachSafepoint,
        RealmV4Event::CleanupSuccess => RealmV4RuntimeEvent::CleanupSuccess,
        RealmV4Event::CleanupTrap => RealmV4RuntimeEvent::CleanupTrap,
        RealmV4Event::Complete => RealmV4RuntimeEvent::Complete,
        RealmV4Event::Trap => RealmV4RuntimeEvent::Trap,
    }
}

const fn routing_runtime_event(event: RealmV4RoutingEvent) -> RealmV4RoutingRuntimeEvent {
    match event {
        RealmV4RoutingEvent::CompleteA => RealmV4RoutingRuntimeEvent::CompleteA,
        RealmV4RoutingEvent::CompleteB => RealmV4RoutingRuntimeEvent::CompleteB,
        RealmV4RoutingEvent::RollbackA => RealmV4RoutingRuntimeEvent::RollbackA,
        RealmV4RoutingEvent::CommitA => RealmV4RoutingRuntimeEvent::CommitA,
        RealmV4RoutingEvent::ActivationFaultA => RealmV4RoutingRuntimeEvent::ActivationFaultA,
    }
}

const fn routing_reload_state(state: RealmV4ReloadState) -> RealmV4RoutingRuntimeReloadState {
    match state {
        RealmV4ReloadState::Reloading => RealmV4RoutingRuntimeReloadState::Reloading,
        RealmV4ReloadState::RolledBack => RealmV4RoutingRuntimeReloadState::RolledBack,
        RealmV4ReloadState::Committed => RealmV4RoutingRuntimeReloadState::Committed,
        RealmV4ReloadState::ActivationFaulted => {
            RealmV4RoutingRuntimeReloadState::ActivationFaulted
        }
    }
}

const fn routing_completion_state(
    state: RealmV4CompletionState,
) -> RealmV4RoutingRuntimeCompletionState {
    match state {
        RealmV4CompletionState::Pending => RealmV4RoutingRuntimeCompletionState::Pending,
        RealmV4CompletionState::Buffered => RealmV4RoutingRuntimeCompletionState::Buffered,
        RealmV4CompletionState::Delivered => RealmV4RoutingRuntimeCompletionState::Delivered,
        RealmV4CompletionState::Replayed => RealmV4RoutingRuntimeCompletionState::Replayed,
        RealmV4CompletionState::Discarded => RealmV4RoutingRuntimeCompletionState::Discarded,
    }
}

const fn task_state(state: RealmV4TaskState) -> RealmV4RuntimeTaskState {
    match state {
        RealmV4TaskState::Ready => RealmV4RuntimeTaskState::Ready,
        RealmV4TaskState::Running => RealmV4RuntimeTaskState::Running,
        RealmV4TaskState::FuelYielded => RealmV4RuntimeTaskState::FuelYielded,
        RealmV4TaskState::ExplicitYielded => RealmV4RuntimeTaskState::ExplicitYielded,
        RealmV4TaskState::Waiting => RealmV4RuntimeTaskState::Waiting,
        RealmV4TaskState::ReloadPaused => RealmV4RuntimeTaskState::ReloadPaused,
        RealmV4TaskState::Cancelling => RealmV4RuntimeTaskState::Cancelling,
        RealmV4TaskState::Cleanup => RealmV4RuntimeTaskState::Cleanup,
        RealmV4TaskState::Completed => RealmV4RuntimeTaskState::Completed,
        RealmV4TaskState::Cancelled => RealmV4RuntimeTaskState::Cancelled,
        RealmV4TaskState::Trapped => RealmV4RuntimeTaskState::Trapped,
    }
}

const fn execution(state: RealmV4TaskState) -> RealmV4ExecutionKind {
    match state {
        RealmV4TaskState::Ready => RealmV4ExecutionKind::Ready,
        RealmV4TaskState::Running => RealmV4ExecutionKind::Running,
        RealmV4TaskState::FuelYielded => RealmV4ExecutionKind::FuelYielded,
        RealmV4TaskState::ExplicitYielded => RealmV4ExecutionKind::ExplicitYielded,
        RealmV4TaskState::Waiting => RealmV4ExecutionKind::Waiting,
        RealmV4TaskState::ReloadPaused => RealmV4ExecutionKind::ReloadPaused,
        RealmV4TaskState::Cancelling => RealmV4ExecutionKind::Cancelling,
        RealmV4TaskState::Cleanup => RealmV4ExecutionKind::Cleanup,
        RealmV4TaskState::Completed | RealmV4TaskState::Cancelled | RealmV4TaskState::Trapped => {
            RealmV4ExecutionKind::None
        }
    }
}

const fn cancel_kind(kind: RealmV4CancelKind) -> RealmV4RuntimeCancelKind {
    match kind {
        RealmV4CancelKind::None => RealmV4RuntimeCancelKind::None,
        RealmV4CancelKind::Ordinary => RealmV4RuntimeCancelKind::Ordinary,
        RealmV4CancelKind::ReloadCommit => RealmV4RuntimeCancelKind::ReloadCommit,
    }
}

const fn terminal_reason(state: RealmV4TaskState) -> RealmV4RuntimeTerminalReason {
    match state {
        RealmV4TaskState::Completed => RealmV4RuntimeTerminalReason::Completed,
        RealmV4TaskState::Cancelled => RealmV4RuntimeTerminalReason::Cancelled,
        RealmV4TaskState::Trapped => RealmV4RuntimeTerminalReason::Trapped,
        RealmV4TaskState::Ready
        | RealmV4TaskState::Running
        | RealmV4TaskState::FuelYielded
        | RealmV4TaskState::ExplicitYielded
        | RealmV4TaskState::Waiting
        | RealmV4TaskState::ReloadPaused
        | RealmV4TaskState::Cancelling
        | RealmV4TaskState::Cleanup => RealmV4RuntimeTerminalReason::None,
    }
}

fn write_failure(
    path: &[RealmV4Event],
    event: RealmV4Event,
    model: RealmV4World,
    runtime: Option<RealmV4RuntimeSnapshot>,
    error: &str,
) {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/model-artifacts/realm-v4-failure.json");
    if let Some(parent) = artifact.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let path = path
        .iter()
        .map(|event| format!("\"{event:?}\""))
        .collect::<Vec<_>>()
        .join(",");
    let runtime_json = runtime.map_or_else(
        || String::from("null"),
        |value| {
            format!(
                "{{\"task_state\":\"{:?}\",\"execution\":\"{:?}\",\"scheduler_tokens\":{},\"request_owned\":{},\"continuation_owned\":{},\"reload_checkpoint\":{},\"cancel_kind\":\"{:?}\",\"user_defer\":{},\"terminal_reason\":\"{:?}\",\"vm_resources\":\"{:?}\"}}",
                value.task_state,
                value.execution,
                value.scheduler_tokens,
                value.request_owned,
                value.continuation_owned,
                value.reload_checkpoint,
                value.cancel_kind,
                value.user_defer,
                value.terminal_reason,
                value.vm_resources,
            )
        },
    );
    let (task_state, execution, scheduler, request, reload, terminal) = runtime.map_or_else(
        || {
            (
                "null".into(),
                "null".into(),
                0,
                false,
                false,
                String::from("null"),
            )
        },
        |value| {
            (
                format!("\"{:?}\"", value.task_state),
                format!("\"{:?}\"", value.execution),
                value.scheduler_tokens,
                value.request_owned,
                value.reload_checkpoint,
                format!("\"{:?}\"", value.terminal_reason),
            )
        },
    );
    std::fs::write(
        artifact,
        format!(
            "{{\n  \"path\": [{path}],\n  \"event\": \"{event:?}\",\n  \"model\": {{\"task_state\":\"{:?}\",\"scheduler_tokens\":{},\"request_owned\":{},\"continuation_owned\":{},\"reload_checkpoint\":{},\"cancel_kind\":\"{:?}\",\"user_defer\":{}}},\n  \"runtime\": {runtime_json},\n  \"task_state\": {task_state},\n  \"execution\": {execution},\n  \"scheduler\": {{\"tokens\": {scheduler}}},\n  \"requests\": {{\"owned\": {request}}},\n  \"reload_buffer\": {{\"checkpoint\": {reload}}},\n  \"terminal_records\": [{terminal}],\n  \"error\": \"{}\"\n}}\n",
            model.task,
            model.scheduler_tokens,
            model.has_request(),
            model.has_continuation(),
            model.reload_active(),
            model.cancel_kind(),
            model.has_user_defer(),
            error.replace('\\', "\\\\").replace('"', "\\\"")
        ),
    )
    .unwrap();
}

fn write_routing_failure(
    path: &[RealmV4RoutingEvent],
    event: RealmV4RoutingEvent,
    model: RealmV4RoutingWorld,
    runtime: Option<RealmV4RoutingRuntimeSnapshot>,
    error: &str,
) {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/model-artifacts/realm-v4-failure.json");
    if let Some(parent) = artifact.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let path = path
        .iter()
        .map(|event| format!("\"{event:?}\""))
        .collect::<Vec<_>>()
        .join(",");
    let runtime_json = runtime.map_or_else(
        || String::from("null"),
        |value| {
            format!(
                "{{\"reload\":\"{:?}\",\"completion_a\":\"{:?}\",\"completion_b\":\"{:?}\",\"buffered\":{},\"replayed\":{},\"discarded_after_commit\":{},\"pending_completions\":{},\"request_reservations\":{}}}",
                value.reload,
                value.completion_a,
                value.completion_b,
                value.buffered,
                value.replayed,
                value.discarded_after_commit,
                value.pending_completions,
                value.request_reservations,
            )
        },
    );
    let (task_state, execution, requests, reload_buffer, terminal_records) = runtime.map_or_else(
        || {
            (
                String::from("null"),
                String::from("null"),
                String::from("null"),
                String::from("null"),
                String::from("[]"),
            )
        },
        |value| {
            (
                "\"dual-module-routing\"".into(),
                "\"RealmRuntime\"".into(),
                format!(
                    "{{\"pending_completions\":{},\"reservations\":{}}}",
                    value.pending_completions, value.request_reservations
                ),
                format!(
                    "{{\"buffered\":{},\"replayed\":{},\"discarded_after_commit\":{}}}",
                    value.buffered, value.replayed, value.discarded_after_commit
                ),
                format!(
                    "[\"module_a={:?}\",\"module_b={:?}\"]",
                    value.completion_a, value.completion_b
                ),
            )
        },
    );
    std::fs::write(
        artifact,
        format!(
            "{{\n  \"path\": [{path}],\n  \"event\": \"{event:?}\",\n  \"model\": {{\"reload\":\"{:?}\",\"completion_a\":\"{:?}\",\"completion_b\":\"{:?}\"}},\n  \"runtime\": {runtime_json},\n  \"task_state\": {task_state},\n  \"execution\": {execution},\n  \"scheduler\": {{}},\n  \"requests\": {requests},\n  \"reload_buffer\": {reload_buffer},\n  \"terminal_records\": {terminal_records},\n  \"error\": \"{}\"\n}}\n",
            model.reload,
            model.completion_a,
            model.completion_b,
            error.replace('\\', "\\\\").replace('"', "\\\"")
        ),
    )
    .unwrap();
}
