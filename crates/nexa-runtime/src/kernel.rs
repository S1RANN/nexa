use crate::FrameLimits;
use crate::RuntimeTrace;
use crate::scope::{ScopeError, ScopeHandle, ScopeManager, ScopeSnapshot};
use crate::task::TaskExecution;
use crate::task::{TaskError, TaskEvent, TaskHandle, TaskManager, TaskSnapshot};
use crate::{FuelState, InterpreterContinuation};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub max_tasks: u32,
    pub max_scopes: u32,
    pub max_frame_segments: u32,
    pub max_scheduler_tokens: u32,
    pub max_trace_records: u32,
    pub max_transient_children_per_scope: u32,
    pub max_persistent_children_per_scope: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskLimits {
    pub frames: FrameLimits,
    pub max_cleanup_ops: u32,
    pub max_cleanup_fuel: u64,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            frames: FrameLimits::default(),
            max_cleanup_ops: 256,
            max_cleanup_fuel: 4_096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepConfig {
    pub owner: ScopeHandle,
    pub priority: u32,
    pub fuel_slice: u64,
    pub cumulative_budget: u64,
    pub limits: TaskLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepResult<T> {
    Completed(T),
    Promoted(TaskHandle),
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            max_tasks: 1_024,
            max_scopes: 256,
            max_frame_segments: 1_024,
            max_scheduler_tokens: 1_024,
            max_trace_records: 1_024,
            max_transient_children_per_scope: 256,
            max_persistent_children_per_scope: 256,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    Scope(ScopeError),
    Task(TaskError),
    ResourceLimit(&'static str),
    InjectedFailure(FailurePoint),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scope(error) => error.fmt(formatter),
            Self::Task(error) => error.fmt(formatter),
            Self::ResourceLimit(limit) => write!(formatter, "runtime resource limit: {limit}"),
            Self::InjectedFailure(point) => write!(formatter, "injected failure at {point:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailurePoint {
    TaskSlotReserve,
    FrameSegmentReserve,
    SchedulerTokenReserve,
    TraceCapacityReserve,
    ScopeMembership,
    ContinuationReserve,
}

impl std::error::Error for RuntimeError {}

impl From<ScopeError> for RuntimeError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

impl From<TaskError> for RuntimeError {
    fn from(error: TaskError) -> Self {
        Self::Task(error)
    }
}

/// The single domain kernel for all Task/Scope operations in one realm.
#[derive(Debug)]
pub struct TaskRuntime {
    scopes: ScopeManager,
    tasks: TaskManager,
    trace: RuntimeTrace,
    limits: RuntimeLimits,
    available_frame_segments: u32,
    available_scheduler_tokens: u32,
    available_trace_records: u32,
    injected_failure: Option<FailurePoint>,
}

impl TaskRuntime {
    #[must_use]
    pub fn new(realm_id: u32, limits: RuntimeLimits) -> Self {
        Self {
            scopes: ScopeManager::with_capacity_limit(realm_id, limits.max_scopes),
            tasks: TaskManager::with_capacity_limit(realm_id, limits.max_tasks),
            trace: RuntimeTrace::with_capacity(limits.max_trace_records as usize),
            limits,
            available_frame_segments: limits.max_frame_segments,
            available_scheduler_tokens: limits.max_scheduler_tokens,
            available_trace_records: limits.max_trace_records,
            injected_failure: None,
        }
    }

    pub fn create_scope(
        &mut self,
        parent: Option<ScopeHandle>,
    ) -> Result<ScopeHandle, RuntimeError> {
        Ok(self.scopes.create(&mut self.trace, parent)?)
    }

    pub fn admit_task(
        &mut self,
        owner: ScopeHandle,
        module_epoch: u64,
        continuation_reserved: bool,
    ) -> Result<TaskHandle, RuntimeError> {
        self.fail_if_injected(FailurePoint::TaskSlotReserve)?;
        self.require_admission_pool(
            FailurePoint::FrameSegmentReserve,
            self.available_frame_segments,
            "frame segment pool",
        )?;
        self.require_admission_pool(
            FailurePoint::SchedulerTokenReserve,
            self.available_scheduler_tokens,
            "scheduler token pool",
        )?;
        self.require_admission_pool(
            FailurePoint::TraceCapacityReserve,
            self.available_trace_records,
            "trace capacity pool",
        )?;
        let scope = self.scopes.snapshot(owner)?;
        if scope.transient_children >= self.limits.max_transient_children_per_scope {
            return Err(RuntimeError::ResourceLimit("transient child limit"));
        }
        self.fail_if_injected(FailurePoint::ScopeMembership)?;
        self.fail_if_injected(FailurePoint::ContinuationReserve)?;
        self.available_frame_segments -= 1;
        self.available_scheduler_tokens -= 1;
        self.available_trace_records -= 1;
        match self.tasks.admit(
            &mut self.scopes,
            &mut self.trace,
            owner,
            module_epoch,
            continuation_reserved,
        ) {
            Ok(task) => Ok(task),
            Err(error) => {
                self.release_admission_reservation();
                Err(error.into())
            }
        }
    }

    pub fn poll_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::Poll)
    }

    pub fn yield_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        let owner = self.tasks.snapshot(task)?.owner;
        let scope = self.scopes.snapshot(owner)?;
        if !self.tasks.snapshot(task)?.persistent
            && scope.persistent_children >= self.limits.max_persistent_children_per_scope
        {
            return Err(RuntimeError::ResourceLimit("persistent child limit"));
        }
        self.apply_task(task, TaskEvent::YieldFuel)
    }

    pub fn await_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        let owner = self.tasks.snapshot(task)?.owner;
        let scope = self.scopes.snapshot(owner)?;
        if !self.tasks.snapshot(task)?.persistent
            && scope.persistent_children >= self.limits.max_persistent_children_per_scope
        {
            return Err(RuntimeError::ResourceLimit("persistent child limit"));
        }
        self.apply_task(task, TaskEvent::AwaitHost)
    }

    pub fn resume_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::Resume)
    }

    pub fn finish_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_terminal_task(task, TaskEvent::Finish)
    }

    pub fn request_task_cancel(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::RequestCancel)
    }

    pub fn reach_task_safepoint(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::ReachSafepoint)
    }

    pub fn clean_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_terminal_task(task, TaskEvent::Clean)
    }

    pub fn request_reload_pause(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::RequestReloadPause)
    }

    pub fn pause_task_for_reload(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.request_reload_pause(task)?;
        if self.tasks.snapshot(task)?.state == crate::TaskState::ReloadPauseRequested {
            self.reach_task_safepoint(task)?;
        }
        Ok(())
    }

    pub fn rollback_reload(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::RollbackReload)
    }

    pub fn commit_reload_cancel(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.begin_reload_commit_cancel(task)?;
        self.clean_task(task)
    }

    pub(crate) fn begin_reload_commit_cancel(
        &mut self,
        task: TaskHandle,
    ) -> Result<(), RuntimeError> {
        self.apply_task(task, TaskEvent::CommitReload)
    }

    pub fn trap_task(&mut self, task: TaskHandle) -> Result<(), RuntimeError> {
        self.apply_terminal_task(task, TaskEvent::Trap)
    }

    pub fn cancel_scope(&mut self, scope: ScopeHandle) -> Result<(), RuntimeError> {
        Ok(self.scopes.request_cancel(&mut self.trace, scope)?)
    }

    pub fn begin_scope_cancellation(&mut self, scope: ScopeHandle) -> Result<(), RuntimeError> {
        Ok(self.scopes.begin_cancelling(&mut self.trace, scope)?)
    }

    pub fn finish_scope_cancellation(&mut self, scope: ScopeHandle) -> Result<(), RuntimeError> {
        Ok(self.scopes.finish_cancelling(&mut self.trace, scope)?)
    }

    pub fn destroy_scope(&mut self, scope: ScopeHandle) -> Result<(), RuntimeError> {
        Ok(self.scopes.destroy(&mut self.trace, scope)?)
    }

    pub fn task_snapshot(&self, task: TaskHandle) -> Result<TaskSnapshot, RuntimeError> {
        Ok(self.tasks.snapshot(task)?)
    }

    pub fn scope_snapshot(&self, scope: ScopeHandle) -> Result<ScopeSnapshot, RuntimeError> {
        Ok(self.scopes.snapshot(scope)?)
    }

    #[must_use]
    pub fn trace(&self) -> &RuntimeTrace {
        &self.trace
    }

    pub fn set_trace_enabled(&mut self, enabled: bool) {
        self.trace.set_enabled(enabled);
    }

    pub fn inject_failure_once(&mut self, point: FailurePoint) {
        self.injected_failure = Some(point);
    }

    pub(crate) fn attach_continuation(
        &mut self,
        task: TaskHandle,
        priority: u32,
        fuel: FuelState,
        continuation: InterpreterContinuation,
        module_id: u32,
        limits: TaskLimits,
    ) -> Result<(), RuntimeError> {
        Ok(self.tasks.attach_execution(
            task,
            priority,
            fuel,
            TaskExecution::Ready(continuation),
            module_id,
            limits,
        )?)
    }

    pub(crate) fn take_execution(
        &mut self,
        task: TaskHandle,
    ) -> Result<TaskExecution, RuntimeError> {
        Ok(self.tasks.take_execution(task)?)
    }

    pub(crate) fn put_execution(
        &mut self,
        task: TaskHandle,
        execution: TaskExecution,
        fuel: FuelState,
    ) -> Result<(), RuntimeError> {
        Ok(self.tasks.put_execution(task, execution, fuel)?)
    }

    pub(crate) fn execution(&self, task: TaskHandle) -> Result<&TaskExecution, RuntimeError> {
        Ok(self.tasks.execution(task)?)
    }

    pub(crate) fn task_handles(&self) -> Vec<TaskHandle> {
        self.tasks.handles()
    }

    fn apply_task(&mut self, task: TaskHandle, event: TaskEvent) -> Result<(), RuntimeError> {
        Ok(self
            .tasks
            .apply(&mut self.scopes, &mut self.trace, task, event)?)
    }

    fn apply_terminal_task(
        &mut self,
        task: TaskHandle,
        event: TaskEvent,
    ) -> Result<(), RuntimeError> {
        self.apply_task(task, event)?;
        self.release_admission_reservation();
        Ok(())
    }

    fn require_admission_pool(
        &mut self,
        point: FailurePoint,
        available: u32,
        name: &'static str,
    ) -> Result<(), RuntimeError> {
        self.fail_if_injected(point)?;
        if available == 0 {
            Err(RuntimeError::ResourceLimit(name))
        } else {
            Ok(())
        }
    }

    fn release_admission_reservation(&mut self) {
        self.available_frame_segments = self.available_frame_segments.saturating_add(1);
        self.available_scheduler_tokens = self.available_scheduler_tokens.saturating_add(1);
        self.available_trace_records = self.available_trace_records.saturating_add(1);
    }

    fn fail_if_injected(&mut self, point: FailurePoint) -> Result<(), RuntimeError> {
        if self.injected_failure == Some(point) {
            self.injected_failure = None;
            Err(RuntimeError::InjectedFailure(point))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{FailurePoint, RuntimeError, RuntimeLimits, TaskRuntime};

    #[test]
    fn full_task_pool_and_injected_admission_leave_scope_unchanged() {
        let limits = RuntimeLimits {
            max_tasks: 0,
            ..RuntimeLimits::default()
        };
        let mut runtime = TaskRuntime::new(1, limits);
        let scope = runtime.create_scope(None).unwrap();
        assert!(matches!(
            runtime.admit_task(scope, 1, true),
            Err(RuntimeError::Task(_))
        ));
        let snapshot = runtime.scope_snapshot(scope).unwrap();
        assert_eq!(snapshot.transient_children, 0);
        assert_eq!(snapshot.persistent_children, 0);

        let mut runtime = TaskRuntime::new(2, RuntimeLimits::default());
        let scope = runtime.create_scope(None).unwrap();
        for point in [
            FailurePoint::TaskSlotReserve,
            FailurePoint::FrameSegmentReserve,
            FailurePoint::SchedulerTokenReserve,
            FailurePoint::TraceCapacityReserve,
            FailurePoint::ScopeMembership,
            FailurePoint::ContinuationReserve,
        ] {
            runtime.inject_failure_once(point);
            assert_eq!(
                runtime.admit_task(scope, 1, true),
                Err(RuntimeError::InjectedFailure(point))
            );
            assert_eq!(runtime.scope_snapshot(scope).unwrap().transient_children, 0);
        }
    }
}
