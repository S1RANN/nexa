use std::collections::BTreeMap;
use std::fmt;

use nexa_bytecode::{AsyncResultType, ValueType};
use nexa_core::{
    InlineDeltas, MachineKind, RawHandle, ResourceDelta, StableId, TRACE_SCHEMA_VERSION,
    TraceRecord, TransitionDisposition, machine_instance_id, machine_invariant_hash_ids,
};

use crate::machines::task;
pub use crate::machines::task::{
    Event as TaskEvent, Guard as TaskGuard, State as TaskState,
    TransitionError as TaskTransitionError,
};
use crate::scope::ScopeManager;
use crate::{
    FuelState, HandleError, HostRequestHandle, InterpreterContinuation, RuntimeTrace, ScopeError,
    ScopeHandle, SlotAllocError, SlotPool,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskHandle(RawHandle);

impl TaskHandle {
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn from_raw(raw: RawHandle) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildKind {
    Transient,
    Persistent,
}

#[derive(Clone, Debug)]
struct Task {
    state: TaskState,
    owner: ScopeHandle,
    module_epoch: u64,
    module_id: u32,
    module_generation: u32,
    child_kind: ChildKind,
    reserved_slots: i64,
    priority: u32,
    fuel: FuelState,
    execution: Option<TaskExecution>,
    limits: crate::TaskLimits,
    charge: crate::ExecutionCharge,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) enum TaskExecution {
    Ready(InterpreterContinuation),
    Running(InterpreterContinuation),
    FuelYielded(InterpreterContinuation),
    ExplicitYielded(InterpreterContinuation),
    Waiting {
        continuation: InterpreterContinuation,
        request: HostRequestHandle,
        destination: u16,
        expected_type: Option<ValueType>,
        async_result: Option<AsyncResultType>,
    },
    ReloadPaused(InterpreterContinuation),
    Cancelling(InterpreterContinuation),
    Cleanup(InterpreterContinuation),
}

impl TaskExecution {
    pub(crate) const fn continuation(&self) -> &InterpreterContinuation {
        match self {
            Self::Ready(continuation)
            | Self::Running(continuation)
            | Self::FuelYielded(continuation)
            | Self::ExplicitYielded(continuation)
            | Self::Waiting { continuation, .. }
            | Self::ReloadPaused(continuation)
            | Self::Cancelling(continuation)
            | Self::Cleanup(continuation) => continuation,
        }
    }

    pub(crate) fn into_continuation(self) -> InterpreterContinuation {
        match self {
            Self::Ready(continuation)
            | Self::Running(continuation)
            | Self::FuelYielded(continuation)
            | Self::ExplicitYielded(continuation)
            | Self::Waiting { continuation, .. }
            | Self::ReloadPaused(continuation)
            | Self::Cancelling(continuation)
            | Self::Cleanup(continuation) => continuation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub state: TaskState,
    pub owner: ScopeHandle,
    pub module_epoch: u64,
    pub module_id: u32,
    pub module_generation: u32,
    pub persistent: bool,
    pub task_slots: i64,
    pub priority: u32,
    pub fuel: FuelState,
    pub limits: crate::TaskLimits,
    pub charge: crate::ExecutionCharge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    Handle(HandleError),
    Allocation(SlotAllocError),
    Scope(ScopeError),
    Transition(TaskTransitionError),
    Admission(&'static str),
    Invariant(&'static str),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(error) => error.fmt(formatter),
            Self::Allocation(error) => error.fmt(formatter),
            Self::Scope(error) => error.fmt(formatter),
            Self::Transition(error) => error.fmt(formatter),
            Self::Admission(error) | Self::Invariant(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for TaskError {}

impl From<HandleError> for TaskError {
    fn from(error: HandleError) -> Self {
        Self::Handle(error)
    }
}

impl From<SlotAllocError> for TaskError {
    fn from(error: SlotAllocError) -> Self {
        Self::Allocation(error)
    }
}

impl From<ScopeError> for TaskError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

/// Owns first-milestone tasks and applies only generated state-machine transitions.
#[derive(Debug)]
pub(crate) struct TaskManager {
    realm_id: u32,
    tasks: SlotPool<Task>,
    epoch_counts: BTreeMap<(u32, u32, u64), usize>,
}

impl TaskManager {
    #[must_use]
    pub fn with_capacity_limit(realm_id: u32, max_tasks: u32) -> Self {
        Self {
            realm_id,
            tasks: SlotPool::with_capacity_limit(realm_id, max_tasks),
            epoch_counts: BTreeMap::new(),
        }
    }

    pub fn admit(
        &mut self,
        scopes: &mut ScopeManager,
        trace: &mut RuntimeTrace,
        owner: ScopeHandle,
        module_epoch: u64,
        continuation_reserved: bool,
    ) -> Result<TaskHandle, TaskError> {
        let owner_snapshot = scopes.snapshot(owner)?;
        if !owner_snapshot.active {
            return Err(TaskError::Admission("owner scope does not admit tasks"));
        }
        if !continuation_reserved {
            return Err(TaskError::Admission(
                "continuation resources are not reserved",
            ));
        }

        let raw = self.tasks.try_allocate(Task {
            state: TaskState::Created,
            owner,
            module_epoch,
            module_id: 0,
            module_generation: 0,
            child_kind: ChildKind::Transient,
            reserved_slots: 0,
            priority: 0,
            fuel: FuelState::new(0, 0, u64::MAX),
            execution: None,
            limits: crate::TaskLimits::default(),
            charge: crate::ExecutionCharge::default(),
        })?;
        let handle = TaskHandle(raw);
        if let Err(error) = scopes.add_transient_child(trace, owner) {
            let _ = self.tasks.release(raw);
            return Err(error.into());
        }
        if let Err(error) = self.apply(scopes, trace, handle, TaskEvent::Admit) {
            let _ = self.tasks.release(raw);
            let _ = scopes.complete_transient_child(trace, owner);
            return Err(error);
        }
        *self.epoch_counts.entry((0, 0, module_epoch)).or_default() += 1;
        Ok(handle)
    }

    #[allow(clippy::too_many_lines)]
    pub fn apply(
        &mut self,
        scopes: &mut ScopeManager,
        trace: &mut RuntimeTrace,
        handle: TaskHandle,
        event: TaskEvent,
    ) -> Result<(), TaskError> {
        let (old_state, owner, module_epoch, child_kind, task_slots) = {
            let task = self.tasks.resolve(handle.raw())?;
            (
                task.state,
                task.owner,
                task.module_epoch,
                task.child_kind,
                task.reserved_slots,
            )
        };
        scopes.snapshot(owner)?;

        let outcome = task::apply(old_state, event, |guard| {
            matches!(
                guard,
                TaskGuard::OwnerScopeValid | TaskGuard::ContinuationReserved
            )
        });
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let (transition_id, disposition) = match error {
                    TaskTransitionError::GuardRejected { transition_id, .. } => (
                        StableId(transition_id),
                        TransitionDisposition::GuardRejected,
                    ),
                    TaskTransitionError::Undefined { .. } => {
                        (StableId::default(), TransitionDisposition::Undefined)
                    }
                };
                trace.record_with(|| TraceRecord {
                    schema_version: TRACE_SCHEMA_VERSION,
                    sequence: 0,
                    machine_kind: MachineKind::Task,
                    machine_id: machine_instance_id(handle.raw()),
                    transition_id,
                    disposition,
                    old_state: state_id(old_state),
                    event: event_id(event),
                    new_state: state_id(old_state),
                    realm_id: self.realm_id,
                    module_epoch,
                    owner_scope: Some(owner.raw()),
                    resource_deltas: InlineDeltas::new(),
                    error_code: None,
                    invariant_hash: machine_invariant_hash_ids(
                        StableId::from_name("Task"),
                        state_id(old_state),
                        Some(owner.raw()),
                        [(StableId::from_name("task_slot"), task_slots)],
                    ),
                });
                return Err(TaskError::Transition(error));
            }
        };
        let next_task_slots = outcome
            .deltas
            .iter()
            .filter(|delta| delta.resource == "task_slot")
            .try_fold(task_slots, |slots, delta| slots.checked_add(delta.amount))
            .ok_or(TaskError::Invariant("task resource counter overflowed"))?;
        if next_task_slots < 0 {
            return Err(TaskError::Invariant(
                "task resource counter became negative",
            ));
        }
        task::check_invariants(outcome.state, |resource| match resource {
            task::Resource::TaskSlot => next_task_slots,
        })
        .map_err(|_| TaskError::Invariant("task machine invariant failed"))?;

        let promotes = child_kind == ChildKind::Transient
            && old_state == TaskState::Running
            && matches!(
                event,
                TaskEvent::YieldFuel | TaskEvent::YieldExplicit | TaskEvent::AwaitHost
            );
        let terminal = is_terminal(outcome.state);
        if promotes {
            scopes.promote_child(trace, owner)?;
        }
        if terminal {
            match if promotes {
                ChildKind::Persistent
            } else {
                child_kind
            } {
                ChildKind::Transient => scopes.complete_transient_child(trace, owner)?,
                ChildKind::Persistent => scopes.complete_persistent_child(trace, owner)?,
            }
        }

        let next_child_kind = if promotes {
            ChildKind::Persistent
        } else {
            child_kind
        };
        {
            let task = self.tasks.resolve_mut(handle.raw())?;
            task.state = outcome.state;
            task.child_kind = next_child_kind;
            task.reserved_slots = next_task_slots;
        }

        trace.record_with(|| TraceRecord {
            schema_version: TRACE_SCHEMA_VERSION,
            sequence: 0,
            machine_kind: MachineKind::Task,
            machine_id: machine_instance_id(handle.raw()),
            transition_id: StableId(outcome.transition_id),
            disposition: TransitionDisposition::Applied,
            old_state: state_id(old_state),
            event: event_id(event),
            new_state: state_id(outcome.state),
            realm_id: self.realm_id,
            module_epoch,
            owner_scope: Some(owner.raw()),
            resource_deltas: inline_deltas(outcome.deltas),
            error_code: None,
            invariant_hash: machine_invariant_hash_ids(
                StableId::from_name("Task"),
                state_id(outcome.state),
                Some(owner.raw()),
                [(StableId::from_name("task_slot"), next_task_slots)],
            ),
        });

        if terminal {
            let released = self.tasks.release(handle.raw())?;
            debug_assert_eq!(released.reserved_slots, 0);
            let key = (
                released.module_generation,
                released.module_id,
                released.module_epoch,
            );
            let count = self
                .epoch_counts
                .get_mut(&key)
                .expect("terminal task has epoch ownership");
            *count = count
                .checked_sub(1)
                .expect("task epoch ownership count is positive");
            if *count == 0 {
                self.epoch_counts.remove(&key);
            }
        }
        Ok(())
    }

    pub fn snapshot(&self, handle: TaskHandle) -> Result<TaskSnapshot, TaskError> {
        let task = self.tasks.resolve(handle.raw())?;
        Ok(TaskSnapshot {
            state: task.state,
            owner: task.owner,
            module_epoch: task.module_epoch,
            module_id: task.module_id,
            module_generation: task.module_generation,
            persistent: task.child_kind == ChildKind::Persistent,
            task_slots: task.reserved_slots,
            priority: task.priority,
            fuel: task.fuel,
            limits: task.limits,
            charge: task.charge,
        })
    }

    pub(crate) fn attach_execution(
        &mut self,
        handle: TaskHandle,
        priority: u32,
        fuel: FuelState,
        execution: TaskExecution,
        module: RawHandle,
        limits: crate::TaskLimits,
    ) -> Result<(), TaskError> {
        if module.realm_id != self.realm_id {
            return Err(TaskError::Invariant("task module belongs to another realm"));
        }
        let module_generation = module.generation;
        let module_id = module.index;
        let (old_module_generation, old_module_id, module_epoch) = {
            let task = self.tasks.resolve_mut(handle.raw())?;
            if task.execution.is_some() {
                return Err(TaskError::Invariant("task already owns a continuation"));
            }
            let old_module_id = task.module_id;
            let old_module_generation = task.module_generation;
            task.priority = priority;
            task.module_id = module_id;
            task.module_generation = module_generation;
            task.fuel = fuel;
            task.execution = Some(execution);
            task.limits = limits;
            (old_module_generation, old_module_id, task.module_epoch)
        };
        if old_module_generation != module_generation || old_module_id != module_id {
            let old_key = (old_module_generation, old_module_id, module_epoch);
            let old_count = self
                .epoch_counts
                .get_mut(&old_key)
                .expect("admitted task has epoch ownership");
            *old_count = old_count
                .checked_sub(1)
                .expect("task epoch ownership count is positive");
            if *old_count == 0 {
                self.epoch_counts.remove(&old_key);
            }
            *self
                .epoch_counts
                .entry((module_generation, module_id, module_epoch))
                .or_default() += 1;
        }
        Ok(())
    }

    pub(crate) fn take_execution(
        &mut self,
        handle: TaskHandle,
    ) -> Result<TaskExecution, TaskError> {
        self.tasks
            .resolve_mut(handle.raw())?
            .execution
            .take()
            .ok_or(TaskError::Invariant("task has no continuation"))
    }

    pub(crate) fn put_execution(
        &mut self,
        handle: TaskHandle,
        execution: TaskExecution,
        fuel: FuelState,
    ) -> Result<(), TaskError> {
        let task = self.tasks.resolve_mut(handle.raw())?;
        if task.execution.replace(execution).is_some() {
            return Err(TaskError::Invariant("task already owns a continuation"));
        }
        task.fuel = fuel;
        Ok(())
    }

    pub(crate) fn execution(&self, handle: TaskHandle) -> Result<&TaskExecution, TaskError> {
        self.tasks
            .resolve(handle.raw())?
            .execution
            .as_ref()
            .ok_or(TaskError::Invariant("task has no continuation"))
    }

    pub(crate) fn restore_checkpoint(
        &mut self,
        handle: TaskHandle,
        snapshot: TaskSnapshot,
        execution: TaskExecution,
    ) -> Result<(), TaskError> {
        let task = self.tasks.resolve_mut(handle.raw())?;
        task.state = snapshot.state;
        task.owner = snapshot.owner;
        task.module_epoch = snapshot.module_epoch;
        task.module_id = snapshot.module_id;
        task.module_generation = snapshot.module_generation;
        task.child_kind = if snapshot.persistent {
            ChildKind::Persistent
        } else {
            ChildKind::Transient
        };
        task.reserved_slots = snapshot.task_slots;
        task.priority = snapshot.priority;
        task.fuel = snapshot.fuel;
        task.execution = Some(execution);
        task.limits = snapshot.limits;
        task.charge = snapshot.charge;
        Ok(())
    }

    pub(crate) fn handles(&self) -> Vec<TaskHandle> {
        self.tasks
            .occupied_handles()
            .into_iter()
            .map(TaskHandle)
            .collect()
    }

    pub(crate) fn handles_iter(&self) -> impl Iterator<Item = TaskHandle> + '_ {
        self.tasks.occupied_handles_iter().map(TaskHandle)
    }

    pub(crate) fn reserved_capacity(&self) -> usize {
        self.tasks.reserved_capacity()
    }

    pub(crate) fn ledger_counts(&self) -> (usize, usize) {
        let tasks = self.tasks.occupied_len();
        let continuations = self
            .tasks
            .occupied_handles_iter()
            .filter(|handle| {
                self.tasks
                    .resolve(*handle)
                    .is_ok_and(|task| task.execution.is_some())
            })
            .count();
        (tasks, continuations)
    }

    pub(crate) fn count_for_epoch(
        &self,
        module_generation: u32,
        module_id: u32,
        epoch: u64,
    ) -> usize {
        self.epoch_counts
            .get(&(module_generation, module_id, epoch))
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn record_charge(
        &mut self,
        handle: TaskHandle,
        charge: crate::ExecutionCharge,
    ) -> Result<crate::ExecutionCharge, TaskError> {
        let task = self.tasks.resolve_mut(handle.raw())?;
        task.charge.instructions = task.charge.instructions.saturating_add(charge.instructions);
        task.charge.fuel_used = task.charge.fuel_used.saturating_add(charge.fuel_used);
        Ok(task.charge)
    }
}

fn inline_deltas(deltas: &[task::ResourceDelta]) -> InlineDeltas {
    let mut inline = InlineDeltas::new();
    for delta in deltas {
        inline
            .try_push(ResourceDelta {
                resource: StableId::from_name(delta.resource),
                amount: delta.amount,
            })
            .expect("generated machine exceeds inline delta capacity");
    }
    inline
}

fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Cancelled | TaskState::Trapped
    )
}

fn state_id(state: TaskState) -> StableId {
    StableId(task::state_id(state))
}

fn event_id(event: TaskEvent) -> StableId {
    StableId(task::event_id(event))
}
