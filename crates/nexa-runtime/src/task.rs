use std::fmt;

use nexa_core::{
    MachineKind, RawHandle, StableId, TRACE_SCHEMA_VERSION, TraceRecord, machine_event_id,
    machine_invariant_hash, machine_state_id,
};

use crate::machines::task;
pub use crate::machines::task::{
    Event as TaskEvent, State as TaskState, TransitionError as TaskTransitionError,
};
use crate::{HandleError, ScopeError, ScopeHandle, ScopeManager, SlotPool, TraceRecorder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskHandle(RawHandle);

impl TaskHandle {
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
    child_kind: ChildKind,
    reserved_slots: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub state: TaskState,
    pub owner: ScopeHandle,
    pub module_epoch: u64,
    pub persistent: bool,
    pub task_slots: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskError {
    Handle(HandleError),
    Scope(ScopeError),
    Transition(TaskTransitionError),
    Admission(&'static str),
    Invariant(&'static str),
}

impl fmt::Display for TaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(error) => error.fmt(formatter),
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

impl From<ScopeError> for TaskError {
    fn from(error: ScopeError) -> Self {
        Self::Scope(error)
    }
}

/// Owns first-milestone tasks and applies only generated state-machine transitions.
#[derive(Debug)]
pub struct TaskManager {
    realm_id: u32,
    tasks: SlotPool<Task>,
    trace: TraceRecorder,
}

impl TaskManager {
    #[must_use]
    pub fn new(realm_id: u32) -> Self {
        Self {
            realm_id,
            tasks: SlotPool::new(realm_id),
            trace: TraceRecorder::new(),
        }
    }

    pub fn admit(
        &mut self,
        scopes: &mut ScopeManager,
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

        scopes.add_transient_child(owner)?;
        let raw = self.tasks.allocate(Task {
            state: TaskState::Created,
            owner,
            module_epoch,
            child_kind: ChildKind::Transient,
            reserved_slots: 0,
        });
        let handle = TaskHandle(raw);
        if let Err(error) = self.apply(scopes, handle, TaskEvent::Admit) {
            let _ = self.tasks.release(raw);
            let _ = scopes.complete_transient_child(owner);
            return Err(error);
        }
        Ok(handle)
    }

    pub fn apply(
        &mut self,
        scopes: &mut ScopeManager,
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
            matches!(guard, "owner_scope_valid" | "continuation_reserved")
        })
        .map_err(TaskError::Transition)?;
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

        let promotes = child_kind == ChildKind::Transient
            && old_state == TaskState::Running
            && matches!(event, TaskEvent::YieldFuel | TaskEvent::AwaitHost);
        let terminal = is_terminal(outcome.state);
        if promotes {
            scopes.promote_child(owner)?;
        }
        if terminal {
            match if promotes {
                ChildKind::Persistent
            } else {
                child_kind
            } {
                ChildKind::Transient => scopes.complete_transient_child(owner)?,
                ChildKind::Persistent => scopes.complete_persistent_child(owner)?,
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

        self.trace.record(TraceRecord {
            schema_version: TRACE_SCHEMA_VERSION,
            sequence: 0,
            machine_kind: MachineKind::Task,
            machine_id: u64::from(handle.raw().index),
            transition_id: StableId(outcome.transition_id),
            old_state: state_id(old_state),
            event: event_id(event),
            new_state: state_id(outcome.state),
            realm_id: self.realm_id,
            module_epoch,
            owner_scope: Some(owner.raw()),
            resource_deltas: outcome
                .deltas
                .iter()
                .map(|delta| nexa_core::ResourceDelta {
                    resource: delta.resource.to_owned(),
                    amount: delta.amount,
                })
                .collect(),
            error_code: None,
            invariant_hash: machine_invariant_hash(
                "Task",
                &format!("{:?}", outcome.state),
                Some(owner.raw()),
                &[("task_slot", next_task_slots)],
            ),
        });

        if terminal {
            let released = self.tasks.release(handle.raw())?;
            debug_assert_eq!(released.reserved_slots, 0);
        }
        Ok(())
    }

    pub fn snapshot(&self, handle: TaskHandle) -> Result<TaskSnapshot, TaskError> {
        let task = self.tasks.resolve(handle.raw())?;
        Ok(TaskSnapshot {
            state: task.state,
            owner: task.owner,
            module_epoch: task.module_epoch,
            persistent: task.child_kind == ChildKind::Persistent,
            task_slots: task.reserved_slots,
        })
    }

    #[must_use]
    pub fn active_len(&self) -> usize {
        self.tasks.occupied_len()
    }

    #[must_use]
    pub fn trace(&self) -> &TraceRecorder {
        &self.trace
    }
}

fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Completed | TaskState::Cancelled | TaskState::Trapped
    )
}

fn state_id(state: TaskState) -> StableId {
    machine_state_id("Task", &format!("{state:?}"))
}

fn event_id(event: TaskEvent) -> StableId {
    machine_event_id("Task", &format!("{event:?}"))
}
