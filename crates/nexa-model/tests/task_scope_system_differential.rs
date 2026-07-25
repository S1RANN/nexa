use nexa_core::{MachineKind, machine_invariant_hash};
use nexa_model::system::{
    SystemConfig, SystemEvent, SystemScopeState, SystemTaskState, TaskScopeWorld, all_events,
    explore_task_scope,
};
use nexa_runtime::{
    RuntimeError, RuntimeLimits, ScopeHandle, ScopeState, TaskHandle, TaskRuntime, TaskState,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorKind {
    Missing,
    Occupied,
    State,
    Admission,
    Capacity,
}

#[derive(Clone, Copy, Debug)]
enum ScopeBinding {
    Live(ScopeHandle),
    Destroyed(ScopeHandle),
}

#[derive(Clone, Copy, Debug)]
enum TaskBinding {
    Live(TaskHandle),
    Terminal(TaskHandle, TaskState),
}

struct RuntimeAdapter {
    runtime: TaskRuntime,
    scopes: Vec<Option<ScopeBinding>>,
    tasks: Vec<Option<TaskBinding>>,
}

impl RuntimeAdapter {
    fn new(config: SystemConfig) -> Self {
        Self {
            runtime: TaskRuntime::new(
                7,
                RuntimeLimits {
                    max_scopes: u32::try_from(config.max_scopes).unwrap(),
                    max_tasks: u32::try_from(config.max_tasks).unwrap(),
                    max_frame_segments: u32::try_from(config.max_tasks).unwrap(),
                    max_scheduler_tokens: u32::try_from(config.max_tasks).unwrap(),
                    max_trace_records: 10_000,
                    max_transient_children_per_scope: u32::try_from(config.max_tasks).unwrap(),
                    max_persistent_children_per_scope: u32::try_from(config.max_tasks).unwrap(),
                },
            ),
            scopes: vec![None; config.max_scopes],
            tasks: vec![None; config.max_tasks],
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, event: SystemEvent) -> Result<(), ErrorKind> {
        match event {
            SystemEvent::CreateScope(index) => {
                let binding = self.scopes.get_mut(index).ok_or(ErrorKind::Missing)?;
                if binding.is_some() {
                    return Err(ErrorKind::Occupied);
                }
                let handle = self
                    .runtime
                    .create_scope(None)
                    .map_err(runtime_error_kind)?;
                *binding = Some(ScopeBinding::Live(handle));
            }
            SystemEvent::AdmitTask { task, owner } => {
                let owner = self.admission_scope(owner)?;
                if self.tasks.get(task).ok_or(ErrorKind::Missing)?.is_some() {
                    return Err(ErrorKind::Occupied);
                }
                let handle = self
                    .runtime
                    .admit_task(owner, 1, true)
                    .map_err(runtime_error_kind)?;
                self.tasks[task] = Some(TaskBinding::Live(handle));
            }
            SystemEvent::PollTask(task) => {
                let task = self.live_task(task)?;
                self.runtime.poll_task(task).map_err(runtime_error_kind)?;
            }
            SystemEvent::YieldFuel(task) => {
                let task = self.live_task(task)?;
                self.runtime
                    .yield_fuel_task(task)
                    .map_err(runtime_error_kind)?;
            }
            SystemEvent::AwaitHost(task) => {
                let task = self.live_task(task)?;
                self.runtime.await_task(task).map_err(runtime_error_kind)?;
            }
            SystemEvent::ResumeTask(task) => {
                let task = self.live_task(task)?;
                self.runtime
                    .resume_fuel_task(task)
                    .map_err(runtime_error_kind)?;
            }
            SystemEvent::FinishTask(task_index) => {
                let task = self.live_task(task_index)?;
                self.runtime.finish_task(task).map_err(runtime_error_kind)?;
                self.tasks[task_index] = Some(TaskBinding::Terminal(task, TaskState::Completed));
            }
            SystemEvent::CancelScope(scope_index) => {
                let scope = self.live_scope(scope_index)?;
                self.runtime
                    .cancel_scope(scope)
                    .and_then(|()| self.runtime.begin_scope_cancellation(scope))
                    .map_err(runtime_error_kind)?;
                let snapshot = self
                    .runtime
                    .scope_snapshot(scope)
                    .map_err(runtime_error_kind)?;
                if snapshot.transient_children == 0 && snapshot.persistent_children == 0 {
                    self.runtime
                        .finish_scope_cancellation(scope)
                        .map_err(runtime_error_kind)?;
                }
            }
            SystemEvent::ReachSafepoint(task_index) => {
                let task = self.live_task(task_index)?;
                let owner = self
                    .runtime
                    .task_snapshot(task)
                    .map_err(runtime_error_kind)?
                    .owner;
                if self
                    .runtime
                    .scope_snapshot(owner)
                    .map_err(runtime_error_kind)?
                    .state
                    != ScopeState::Cancelling
                {
                    return Err(ErrorKind::State);
                }
                self.runtime
                    .request_task_cancel(task)
                    .and_then(|()| self.runtime.reach_task_safepoint(task))
                    .and_then(|()| self.runtime.finish_cancel_without_cleanup(task))
                    .map_err(runtime_error_kind)?;
                self.tasks[task_index] = Some(TaskBinding::Terminal(task, TaskState::Cancelled));
                let scope = self
                    .runtime
                    .scope_snapshot(owner)
                    .map_err(runtime_error_kind)?;
                if scope.transient_children == 0 && scope.persistent_children == 0 {
                    self.runtime
                        .finish_scope_cancellation(owner)
                        .map_err(runtime_error_kind)?;
                }
            }
            SystemEvent::DestroyScope(scope_index) => {
                let scope = self.live_scope(scope_index)?;
                self.runtime
                    .destroy_scope(scope)
                    .map_err(runtime_error_kind)?;
                self.scopes[scope_index] = Some(ScopeBinding::Destroyed(scope));
            }
        }
        Ok(())
    }

    fn live_scope(&self, index: usize) -> Result<ScopeHandle, ErrorKind> {
        match self.scopes.get(index).copied().flatten() {
            Some(ScopeBinding::Live(handle)) => Ok(handle),
            Some(ScopeBinding::Destroyed(_)) => Err(ErrorKind::State),
            None => Err(ErrorKind::Missing),
        }
    }

    fn admission_scope(&self, index: usize) -> Result<ScopeHandle, ErrorKind> {
        match self.scopes.get(index).copied().flatten() {
            Some(ScopeBinding::Live(handle))
                if self
                    .runtime
                    .scope_snapshot(handle)
                    .is_ok_and(|scope| scope.state == ScopeState::Active) =>
            {
                Ok(handle)
            }
            Some(ScopeBinding::Live(_) | ScopeBinding::Destroyed(_)) => Err(ErrorKind::Admission),
            None => Err(ErrorKind::Missing),
        }
    }

    fn live_task(&self, index: usize) -> Result<TaskHandle, ErrorKind> {
        match self.tasks.get(index).copied().flatten() {
            Some(TaskBinding::Live(handle)) => Ok(handle),
            Some(TaskBinding::Terminal(_, _)) => Err(ErrorKind::State),
            None => Err(ErrorKind::Missing),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn assert_matches(&self, reference: &TaskScopeWorld) {
        for (index, binding) in self.scopes.iter().enumerate() {
            match (reference.scope_snapshot(index), binding) {
                (None, None) => {}
                (
                    Some((expected_state, transient, persistent)),
                    Some(ScopeBinding::Live(handle)),
                ) => {
                    let actual = self.runtime.scope_snapshot(*handle).unwrap();
                    assert_eq!(actual.state, runtime_scope_state(expected_state));
                    assert_eq!(actual.transient_children, u32::from(transient));
                    assert_eq!(actual.persistent_children, u32::from(persistent));
                    let expected_hash = machine_invariant_hash(
                        "Scope",
                        scope_state_name(actual.state),
                        Some(handle.raw()),
                        &[
                            (
                                "active_scope",
                                i64::from(actual.state != ScopeState::Destroyed),
                            ),
                            ("persistent_child", i64::from(persistent)),
                            ("transient_child", i64::from(transient)),
                        ],
                    );
                    assert_latest_hash(
                        &self.runtime,
                        MachineKind::Scope,
                        handle.raw().index,
                        handle.raw().generation,
                        expected_hash,
                    );
                }
                (
                    Some((SystemScopeState::Destroyed, 0, 0)),
                    Some(ScopeBinding::Destroyed(handle)),
                ) => {
                    assert!(self.runtime.scope_snapshot(*handle).is_err());
                    let expected_hash = machine_invariant_hash(
                        "Scope",
                        "Destroyed",
                        Some(handle.raw()),
                        &[
                            ("active_scope", 0),
                            ("persistent_child", 0),
                            ("transient_child", 0),
                        ],
                    );
                    assert_latest_hash(
                        &self.runtime,
                        MachineKind::Scope,
                        handle.raw().index,
                        handle.raw().generation,
                        expected_hash,
                    );
                }
                pair => panic!("scope {index} differs: {pair:?}"),
            }
        }

        for (index, binding) in self.tasks.iter().enumerate() {
            match (reference.task_snapshot(index), binding) {
                (None, None) => {}
                (
                    Some((expected_state, owner_index, persistent)),
                    Some(TaskBinding::Live(handle)),
                ) => {
                    let actual = self.runtime.task_snapshot(*handle).unwrap();
                    assert_eq!(actual.state, runtime_task_state(expected_state));
                    assert_eq!(actual.owner, self.scope_handle(owner_index));
                    assert_eq!(actual.persistent, persistent);
                    let expected_hash = machine_invariant_hash(
                        "Task",
                        task_state_name(actual.state),
                        Some(actual.owner.raw()),
                        &[("task_slot", 1)],
                    );
                    assert_latest_hash(
                        &self.runtime,
                        MachineKind::Task,
                        handle.raw().index,
                        handle.raw().generation,
                        expected_hash,
                    );
                }
                (
                    Some((SystemTaskState::Terminal, owner_index, _)),
                    Some(TaskBinding::Terminal(handle, terminal)),
                ) => {
                    assert!(self.runtime.task_snapshot(*handle).is_err());
                    let expected_hash = machine_invariant_hash(
                        "Task",
                        task_state_name(*terminal),
                        Some(self.scope_handle(owner_index).raw()),
                        &[("task_slot", 0)],
                    );
                    assert_latest_hash(
                        &self.runtime,
                        MachineKind::Task,
                        handle.raw().index,
                        handle.raw().generation,
                        expected_hash,
                    );
                }
                pair => panic!("task {index} differs: {pair:?}"),
            }
        }

        let trace_start = self
            .runtime
            .trace()
            .records()
            .first()
            .map_or(0, |record| record.sequence);
        for (offset, record) in self.runtime.trace().records().iter().enumerate() {
            assert_eq!(
                record.sequence,
                trace_start + u64::try_from(offset).unwrap()
            );
        }
    }

    fn scope_handle(&self, index: usize) -> ScopeHandle {
        match self.scopes[index].unwrap() {
            ScopeBinding::Live(handle) | ScopeBinding::Destroyed(handle) => handle,
        }
    }
}

fn reference_error_kind(error: &str) -> ErrorKind {
    if error.contains("occupied") {
        ErrorKind::Occupied
    } else if error.contains("missing") || error.contains("index") {
        ErrorKind::Missing
    } else if error.contains("rejects admission") {
        ErrorKind::Admission
    } else if error.contains("overflow") {
        ErrorKind::Capacity
    } else {
        ErrorKind::State
    }
}

#[allow(clippy::needless_pass_by_value)]
fn runtime_error_kind(error: RuntimeError) -> ErrorKind {
    let message = error.to_string();
    if message.contains("handle") || message.contains("vacant") || message.contains("stale") {
        ErrorKind::Missing
    } else if message.contains("admit") || message.contains("active") {
        ErrorKind::Admission
    } else if message.contains("capacity") || message.contains("limit") {
        ErrorKind::Capacity
    } else {
        ErrorKind::State
    }
}

fn runtime_scope_state(state: SystemScopeState) -> ScopeState {
    match state {
        SystemScopeState::Active => ScopeState::Active,
        SystemScopeState::Cancelling => ScopeState::Cancelling,
        SystemScopeState::Cancelled => ScopeState::Cancelled,
        SystemScopeState::Destroyed => ScopeState::Destroyed,
    }
}

fn runtime_task_state(state: SystemTaskState) -> TaskState {
    match state {
        SystemTaskState::Ready => TaskState::Ready,
        SystemTaskState::Running => TaskState::Running,
        SystemTaskState::FuelYielded => TaskState::FuelYielded,
        SystemTaskState::Waiting => TaskState::Waiting,
        SystemTaskState::Terminal => unreachable!("terminal tasks are released"),
    }
}

fn scope_state_name(state: ScopeState) -> &'static str {
    match state {
        ScopeState::Created => "Created",
        ScopeState::Active => "Active",
        ScopeState::CancelRequested => "CancelRequested",
        ScopeState::Cancelling => "Cancelling",
        ScopeState::Cancelled => "Cancelled",
        ScopeState::Destroyed => "Destroyed",
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Created => "Created",
        TaskState::Ready => "Ready",
        TaskState::Running => "Running",
        TaskState::FuelYielded => "FuelYielded",
        TaskState::ExplicitYielded => "ExplicitYielded",
        TaskState::Waiting => "Waiting",
        TaskState::ReloadPauseRequested => "ReloadPauseRequested",
        TaskState::ReloadPaused => "ReloadPaused",
        TaskState::CancelRequested => "CancelRequested",
        TaskState::Cancelling => "Cancelling",
        TaskState::Cleanup => "Cleanup",
        TaskState::Completed => "Completed",
        TaskState::Cancelled => "Cancelled",
        TaskState::Trapped => "Trapped",
    }
}

fn assert_latest_hash(
    runtime: &TaskRuntime,
    kind: MachineKind,
    index: u32,
    generation: u32,
    expected: u64,
) {
    let machine_id = u64::from(generation) << 32 | u64::from(index);
    let record = runtime
        .trace()
        .records()
        .iter()
        .rev()
        .find(|record| record.machine_kind == kind && record.machine_id == machine_id)
        .unwrap();
    assert_eq!(record.invariant_hash, expected);
}

#[test]
fn every_bounded_task_scope_world_and_rejection_matches_the_runtime() {
    let config = SystemConfig::parse(include_str!(
        "../../../specs/systems/task_scope.system.spec"
    ))
    .unwrap();
    let report = explore_task_scope(config);
    assert!(report.failures.is_empty(), "{:?}", report.failures);

    for path in &report.world_paths {
        for candidate in all_events(config) {
            let mut reference = TaskScopeWorld::new(config);
            let mut actual = RuntimeAdapter::new(config);
            for event in path {
                reference.apply(*event).unwrap();
                actual.apply(*event).unwrap();
                actual.assert_matches(&reference);
            }

            let expected = reference.apply(candidate).map_err(reference_error_kind);
            let observed = actual.apply(candidate);
            assert_eq!(
                observed, expected,
                "differential mismatch after {path:?} + {candidate:?}"
            );
            actual.assert_matches(&reference);
        }
    }
}
