use std::fmt;

use nexa_core::{
    MachineKind, RawHandle, StableId, TRACE_SCHEMA_VERSION, TraceRecord, machine_event_id,
    machine_invariant_hash, machine_state_id,
};

pub use crate::machines::scope::TransitionError as ScopeTransitionError;
use crate::machines::scope::{self, Event, State};
use crate::{HandleError, SlotPool, TraceRecorder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScopeHandle(RawHandle);

impl ScopeHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Debug)]
struct Scope {
    state: State,
    parent: Option<ScopeHandle>,
    transient_children: u32,
    persistent_children: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeSnapshot {
    pub parent: Option<ScopeHandle>,
    pub transient_children: u32,
    pub persistent_children: u32,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    Handle(HandleError),
    Transition(ScopeTransitionError),
    Admission(&'static str),
    HasChildren { transient: u32, persistent: u32 },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(error) => error.fmt(formatter),
            Self::Transition(error) => error.fmt(formatter),
            Self::Admission(error) => formatter.write_str(error),
            Self::HasChildren {
                transient,
                persistent,
            } => write!(
                formatter,
                "scope still has {transient} transient and {persistent} persistent children"
            ),
        }
    }
}

impl std::error::Error for ScopeError {}

impl From<HandleError> for ScopeError {
    fn from(error: HandleError) -> Self {
        Self::Handle(error)
    }
}

/// Owns scopes for one realm and emits every transition through the shared trace format.
#[derive(Debug)]
pub struct ScopeManager {
    realm_id: u32,
    scopes: SlotPool<Scope>,
    trace: TraceRecorder,
}

impl ScopeManager {
    #[must_use]
    pub fn new(realm_id: u32) -> Self {
        Self {
            realm_id,
            scopes: SlotPool::new(realm_id),
            trace: TraceRecorder::new(),
        }
    }

    pub fn create(&mut self, parent: Option<ScopeHandle>) -> Result<ScopeHandle, ScopeError> {
        if let Some(parent) = parent {
            let parent_scope = self.scopes.resolve(parent.raw())?;
            if parent_scope.state != State::Active {
                return Err(ScopeError::Admission("parent scope is not active"));
            }
        }
        let raw = self.scopes.allocate(Scope {
            state: State::Created,
            parent,
            transient_children: 0,
            persistent_children: 0,
        });
        let handle = ScopeHandle(raw);
        self.apply(handle, Event::Activate)?;
        Ok(handle)
    }

    pub fn request_cancel(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        self.apply(handle, Event::RequestCancel)
    }

    pub fn begin_cancelling(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        self.apply(handle, Event::ChildrenObserved)
    }

    pub fn finish_cancelling(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        let scope = self.scopes.resolve(handle.raw())?;
        if scope.transient_children != 0 || scope.persistent_children != 0 {
            return Err(ScopeError::HasChildren {
                transient: scope.transient_children,
                persistent: scope.persistent_children,
            });
        }
        self.apply(handle, Event::ChildrenFinished)
    }

    pub fn destroy(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        self.apply(handle, Event::Destroy)?;
        self.scopes.release(handle.raw())?;
        Ok(())
    }

    pub fn add_transient_child(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        let scope = self.scopes.resolve_mut(handle.raw())?;
        if scope.state != State::Active {
            return Err(ScopeError::Admission("scope does not admit new children"));
        }
        scope.transient_children = scope
            .transient_children
            .checked_add(1)
            .expect("scope transient child counter exhausted u32");
        Ok(())
    }

    pub fn promote_child(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        let scope = self.scopes.resolve_mut(handle.raw())?;
        scope.transient_children = scope
            .transient_children
            .checked_sub(1)
            .ok_or(ScopeError::Admission("scope has no transient child"))?;
        scope.persistent_children = scope
            .persistent_children
            .checked_add(1)
            .expect("scope persistent child counter exhausted u32");
        Ok(())
    }

    pub fn complete_transient_child(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        let scope = self.scopes.resolve_mut(handle.raw())?;
        scope.transient_children = scope
            .transient_children
            .checked_sub(1)
            .ok_or(ScopeError::Admission("scope has no transient child"))?;
        Ok(())
    }

    pub fn complete_persistent_child(&mut self, handle: ScopeHandle) -> Result<(), ScopeError> {
        let scope = self.scopes.resolve_mut(handle.raw())?;
        scope.persistent_children = scope
            .persistent_children
            .checked_sub(1)
            .ok_or(ScopeError::Admission("scope has no persistent child"))?;
        Ok(())
    }

    pub fn snapshot(&self, handle: ScopeHandle) -> Result<ScopeSnapshot, ScopeError> {
        let scope = self.scopes.resolve(handle.raw())?;
        Ok(ScopeSnapshot {
            parent: scope.parent,
            transient_children: scope.transient_children,
            persistent_children: scope.persistent_children,
            active: scope.state == State::Active,
        })
    }

    #[must_use]
    pub fn trace(&self) -> &TraceRecorder {
        &self.trace
    }

    fn apply(&mut self, handle: ScopeHandle, event: Event) -> Result<(), ScopeError> {
        let (old, outcome, invariant_hash) = {
            let scope = self.scopes.resolve_mut(handle.raw())?;
            let old = scope.state;
            let outcome = scope::apply(old, event, |_| true).map_err(ScopeError::Transition)?;
            scope.state = outcome.state;
            let invariant_hash = machine_invariant_hash(
                "Scope",
                &format!("{:?}", scope.state),
                Some(handle.raw()),
                &[
                    ("persistent_children", i64::from(scope.persistent_children)),
                    ("transient_children", i64::from(scope.transient_children)),
                ],
            );
            (old, outcome, invariant_hash)
        };
        self.trace.record(TraceRecord {
            schema_version: TRACE_SCHEMA_VERSION,
            sequence: 0,
            machine_kind: MachineKind::Scope,
            machine_id: u64::from(handle.raw().index),
            transition_id: StableId(outcome.transition_id),
            old_state: state_id(old),
            event: event_id(event),
            new_state: state_id(outcome.state),
            realm_id: self.realm_id,
            module_epoch: 0,
            owner_scope: Some(handle.raw()),
            resource_deltas: outcome
                .deltas
                .iter()
                .map(|delta| nexa_core::ResourceDelta {
                    resource: delta.resource.to_owned(),
                    amount: delta.amount,
                })
                .collect(),
            error_code: None,
            invariant_hash,
        });
        Ok(())
    }
}

fn state_id(state: State) -> StableId {
    machine_state_id("Scope", &format!("{state:?}"))
}

fn event_id(event: Event) -> StableId {
    machine_event_id("Scope", &format!("{event:?}"))
}

#[cfg(test)]
mod tests {
    use nexa_core::MachineKind;

    use super::{ScopeError, ScopeManager};

    #[test]
    fn scope_transitions_emit_monotonic_trace_and_reject_destroy_with_children() {
        let mut manager = ScopeManager::new(7);
        let scope = manager.create(None).unwrap();
        manager.add_transient_child(scope).unwrap();
        manager.request_cancel(scope).unwrap();
        manager.begin_cancelling(scope).unwrap();
        assert!(matches!(
            manager.finish_cancelling(scope),
            Err(ScopeError::HasChildren { .. })
        ));
        manager.complete_transient_child(scope).unwrap();
        manager.finish_cancelling(scope).unwrap();
        manager.destroy(scope).unwrap();

        let records = manager.trace().records();
        assert_eq!(records.len(), 5);
        assert_eq!(manager.trace().count_for(MachineKind::Scope), 5);
        assert!(
            records
                .windows(2)
                .all(|window| window[0].sequence + 1 == window[1].sequence)
        );
    }

    #[test]
    fn destroyed_scope_handle_cannot_resolve_reused_slot() {
        let mut manager = ScopeManager::new(7);
        let first = manager.create(None).unwrap();
        manager.request_cancel(first).unwrap();
        manager.begin_cancelling(first).unwrap();
        manager.finish_cancelling(first).unwrap();
        manager.destroy(first).unwrap();

        let second = manager.create(None).unwrap();
        assert_eq!(first.raw().index, second.raw().index);
        assert_ne!(first.raw().generation, second.raw().generation);
        assert!(manager.snapshot(first).is_err());
    }
}
