use std::fmt;

use nexa_core::{
    InlineDeltas, MachineKind, RawHandle, ResourceDelta, StableId, TRACE_SCHEMA_VERSION,
    TraceRecord, TransitionDisposition, machine_instance_id, machine_invariant_hash_ids,
};

use crate::machines::scope::{self, Event};
pub use crate::machines::scope::{
    Guard as ScopeGuard, State as ScopeState, TransitionError as ScopeTransitionError,
};
use crate::{HandleError, RuntimeTrace, SlotAllocError, SlotPool};

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
    state: ScopeState,
    parent: Option<ScopeHandle>,
    transient_children: u32,
    persistent_children: u32,
    active_scopes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeSnapshot {
    pub state: ScopeState,
    pub parent: Option<ScopeHandle>,
    pub transient_children: u32,
    pub persistent_children: u32,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeError {
    Handle(HandleError),
    Allocation(SlotAllocError),
    Transition(ScopeTransitionError),
    Admission(&'static str),
    Invariant(&'static str),
    HasChildren { transient: u32, persistent: u32 },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Handle(error) => error.fmt(formatter),
            Self::Allocation(error) => error.fmt(formatter),
            Self::Transition(error) => error.fmt(formatter),
            Self::Admission(error) | Self::Invariant(error) => formatter.write_str(error),
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

impl From<SlotAllocError> for ScopeError {
    fn from(error: SlotAllocError) -> Self {
        Self::Allocation(error)
    }
}

/// Owns scopes for one realm and emits every transition through the shared trace format.
#[derive(Debug)]
pub(crate) struct ScopeManager {
    realm_id: u32,
    scopes: SlotPool<Scope>,
}

impl ScopeManager {
    #[must_use]
    pub fn with_capacity_limit(realm_id: u32, max_scopes: u32) -> Self {
        Self {
            realm_id,
            scopes: SlotPool::with_capacity_limit(realm_id, max_scopes),
        }
    }

    pub fn create(
        &mut self,
        trace: &mut RuntimeTrace,
        parent: Option<ScopeHandle>,
    ) -> Result<ScopeHandle, ScopeError> {
        if let Some(parent) = parent {
            let parent_scope = self.scopes.resolve(parent.raw())?;
            if parent_scope.state != ScopeState::Active {
                return Err(ScopeError::Admission("parent scope is not active"));
            }
        }
        let raw = self.scopes.try_allocate(Scope {
            state: ScopeState::Created,
            parent,
            transient_children: 0,
            persistent_children: 0,
            active_scopes: 0,
        })?;
        let handle = ScopeHandle(raw);
        self.apply(trace, handle, Event::Activate)?;
        Ok(handle)
    }

    pub fn request_cancel(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::RequestCancel)
    }

    pub fn begin_cancelling(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::ChildrenObserved)
    }

    pub fn finish_cancelling(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::ChildrenFinished)
    }

    pub fn destroy(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::Destroy)?;
        self.scopes.release(handle.raw())?;
        Ok(())
    }

    pub fn add_transient_child(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::AddTransient)
    }

    pub fn promote_child(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::PromoteChild)
    }

    pub fn complete_transient_child(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::CompleteTransient)
    }

    pub fn complete_persistent_child(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
    ) -> Result<(), ScopeError> {
        self.apply(trace, handle, Event::CompletePersistent)
    }

    pub fn snapshot(&self, handle: ScopeHandle) -> Result<ScopeSnapshot, ScopeError> {
        let scope = self.scopes.resolve(handle.raw())?;
        Ok(ScopeSnapshot {
            state: scope.state,
            parent: scope.parent,
            transient_children: scope.transient_children,
            persistent_children: scope.persistent_children,
            active: scope.state == ScopeState::Active,
        })
    }

    pub(crate) fn reserved_capacity(&self) -> usize {
        self.scopes.reserved_capacity()
    }

    fn apply(
        &mut self,
        trace: &mut RuntimeTrace,
        handle: ScopeHandle,
        event: Event,
    ) -> Result<(), ScopeError> {
        let (old, active, transient, persistent) = {
            let current = self.scopes.resolve(handle.raw())?;
            (
                current.state,
                current.active_scopes,
                current.transient_children,
                current.persistent_children,
            )
        };
        let outcome = scope::apply(old, event, |guard| match guard {
            ScopeGuard::ChildrenZero => transient == 0 && persistent == 0,
            ScopeGuard::HasTransient => transient > 0,
            ScopeGuard::HasPersistent => persistent > 0,
        });
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let (transition_id, disposition) = match error {
                    ScopeTransitionError::GuardRejected { transition_id, .. } => (
                        StableId(transition_id),
                        TransitionDisposition::GuardRejected,
                    ),
                    ScopeTransitionError::Undefined { .. } => {
                        (StableId::default(), TransitionDisposition::Undefined)
                    }
                };
                trace.record_with(|| {
                    scope_trace_record(
                        self.realm_id,
                        handle,
                        old,
                        event,
                        old,
                        transition_id,
                        disposition,
                        InlineDeltas::new(),
                        active,
                        transient,
                        persistent,
                    )
                });
                return Err(ScopeError::Transition(error));
            }
        };
        let mut next_active = active;
        let mut next_transient = transient;
        let mut next_persistent = persistent;
        for delta in outcome.deltas {
            let target = match delta.resource {
                "active_scope" => &mut next_active,
                "transient_child" => &mut next_transient,
                "persistent_child" => &mut next_persistent,
                _ => return Err(ScopeError::Invariant("unknown scope resource")),
            };
            *target = apply_resource_delta(*target, delta.amount)
                .ok_or(ScopeError::Invariant("scope resource delta overflow"))?;
        }
        scope::check_invariants(outcome.state, |resource| match resource {
            scope::Resource::ActiveScope => i64::from(next_active),
            scope::Resource::TransientChild => i64::from(next_transient),
            scope::Resource::PersistentChild => i64::from(next_persistent),
        })
        .map_err(|_| ScopeError::Invariant("scope machine invariant failed"))?;
        {
            let current = self.scopes.resolve_mut(handle.raw())?;
            current.state = outcome.state;
            current.active_scopes = next_active;
            current.transient_children = next_transient;
            current.persistent_children = next_persistent;
        }
        trace.record_with(|| {
            scope_trace_record(
                self.realm_id,
                handle,
                old,
                event,
                outcome.state,
                StableId(outcome.transition_id),
                TransitionDisposition::Applied,
                inline_deltas(outcome.deltas),
                next_active,
                next_transient,
                next_persistent,
            )
        });
        Ok(())
    }
}

fn state_id(state: ScopeState) -> StableId {
    StableId(scope::state_id(state))
}

fn event_id(event: Event) -> StableId {
    StableId(scope::event_id(event))
}

#[allow(clippy::too_many_arguments)]
fn scope_trace_record(
    realm_id: u32,
    handle: ScopeHandle,
    old: ScopeState,
    event: Event,
    new: ScopeState,
    transition_id: StableId,
    disposition: TransitionDisposition,
    deltas: InlineDeltas,
    active: u32,
    transient: u32,
    persistent: u32,
) -> TraceRecord {
    TraceRecord {
        schema_version: TRACE_SCHEMA_VERSION,
        sequence: 0,
        machine_kind: MachineKind::Scope,
        machine_id: machine_instance_id(handle.raw()),
        transition_id,
        disposition,
        old_state: state_id(old),
        event: event_id(event),
        new_state: state_id(new),
        realm_id,
        module_epoch: 0,
        owner_scope: Some(handle.raw()),
        resource_deltas: deltas,
        error_code: None,
        invariant_hash: machine_invariant_hash_ids(
            StableId::from_name("Scope"),
            state_id(new),
            Some(handle.raw()),
            [
                (StableId::from_name("active_scope"), i64::from(active)),
                (
                    StableId::from_name("persistent_child"),
                    i64::from(persistent),
                ),
                (StableId::from_name("transient_child"), i64::from(transient)),
            ],
        ),
    }
}

fn inline_deltas(deltas: &[scope::ResourceDelta]) -> InlineDeltas {
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

fn apply_resource_delta(value: u32, delta: i64) -> Option<u32> {
    if delta >= 0 {
        value.checked_add(u32::try_from(delta).ok()?)
    } else {
        value.checked_sub(u32::try_from(delta.checked_abs()?).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::MachineKind;

    use crate::RuntimeTrace;

    use super::{ScopeError, ScopeManager};

    #[test]
    fn scope_transitions_emit_monotonic_trace_and_reject_destroy_with_children() {
        let mut manager = ScopeManager::with_capacity_limit(7, u32::MAX);
        let mut trace = RuntimeTrace::new();
        let scope = manager.create(&mut trace, None).unwrap();
        manager.add_transient_child(&mut trace, scope).unwrap();
        manager.request_cancel(&mut trace, scope).unwrap();
        manager.begin_cancelling(&mut trace, scope).unwrap();
        assert!(matches!(
            manager.finish_cancelling(&mut trace, scope),
            Err(ScopeError::Transition(_))
        ));
        manager.complete_transient_child(&mut trace, scope).unwrap();
        manager.finish_cancelling(&mut trace, scope).unwrap();
        manager.destroy(&mut trace, scope).unwrap();

        let records = trace.records();
        assert_eq!(records.len(), 8);
        assert_eq!(trace.count_for(MachineKind::Scope), 8);
        assert!(
            records
                .iter()
                .zip(records.iter().skip(1))
                .all(|(previous, current)| previous.sequence + 1 == current.sequence)
        );
    }

    #[test]
    fn destroyed_scope_handle_cannot_resolve_reused_slot() {
        let mut manager = ScopeManager::with_capacity_limit(7, u32::MAX);
        let mut trace = RuntimeTrace::new();
        let first = manager.create(&mut trace, None).unwrap();
        manager.request_cancel(&mut trace, first).unwrap();
        manager.begin_cancelling(&mut trace, first).unwrap();
        manager.finish_cancelling(&mut trace, first).unwrap();
        manager.destroy(&mut trace, first).unwrap();

        let second = manager.create(&mut trace, None).unwrap();
        assert_eq!(first.raw().index, second.raw().index);
        assert_ne!(first.raw().generation, second.raw().generation);
        assert!(manager.snapshot(first).is_err());
        let scope_ids = trace
            .records()
            .iter()
            .filter(|record| record.machine_kind == MachineKind::Scope)
            .map(|record| record.machine_id)
            .collect::<Vec<_>>();
        assert_ne!(scope_ids[0], *scope_ids.last().unwrap());
    }
}
