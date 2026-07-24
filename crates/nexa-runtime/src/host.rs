use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use nexa_core::RawHandle;

use crate::{HandleError, SlotAllocError, SlotPool, TaskHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeHostDomain {
    VmThread,
    Render,
    Audio,
    Io,
    Custom(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseReservation(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseKind {
    HostRequest,
    ResourceToken,
    Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseRecord {
    pub realm_id: u32,
    pub kind: ReleaseKind,
    pub object_id: u64,
    pub domain: RuntimeHostDomain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseQueueState {
    Healthy,
    Stalled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseQueueError {
    Capacity,
    NotReserved,
}

#[derive(Debug)]
pub struct ReleaseQueue {
    records: VecDeque<ReleaseRecord>,
    capacity: usize,
    reserved: BTreeSet<u64>,
    next_reservation: u64,
    state: ReleaseQueueState,
}

impl ReleaseQueue {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
            reserved: BTreeSet::new(),
            next_reservation: 0,
            state: ReleaseQueueState::Healthy,
        }
    }

    pub fn reserve(&mut self) -> Result<ReleaseReservation, ReleaseQueueError> {
        if self.records.len() + self.reserved.len() >= self.capacity {
            self.state = ReleaseQueueState::Stalled;
            return Err(ReleaseQueueError::Capacity);
        }
        let reservation = self.next_reservation;
        self.next_reservation = self
            .next_reservation
            .checked_add(1)
            .ok_or(ReleaseQueueError::Capacity)?;
        self.reserved.insert(reservation);
        Ok(ReleaseReservation(reservation))
    }

    pub fn enqueue_reserved(
        &mut self,
        reservation: ReleaseReservation,
        record: ReleaseRecord,
    ) -> Result<(), ReleaseQueueError> {
        if !self.reserved.remove(&reservation.0) {
            return Err(ReleaseQueueError::NotReserved);
        }
        self.records.push_back(record);
        Ok(())
    }

    pub fn cancel_reservation(&mut self, reservation: ReleaseReservation) {
        self.reserved.remove(&reservation.0);
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ReleaseRecord> + '_ {
        self.state = ReleaseQueueState::Healthy;
        self.records.drain(..)
    }

    pub fn reparent_realm(&mut self, old_realm: u32, new_realm: u32) {
        for record in &mut self.records {
            if record.realm_id == old_realm {
                record.realm_id = new_realm;
            }
        }
    }

    #[must_use]
    pub const fn state(&self) -> ReleaseQueueState {
        self.state
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostRequestHandle(RawHandle);

impl HostRequestHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRequestState {
    Pending,
    Completed,
    Cancelled,
    Detached,
}

#[derive(Debug)]
struct HostRequest {
    epoch: u64,
    state: HostRequestState,
    release: Option<ReleaseReservation>,
}

#[derive(Clone, Debug)]
struct Completion {
    request: HostRequestHandle,
    epoch: u64,
    value: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRequestError {
    Handle(HandleError),
    Allocation(SlotAllocError),
    ReleaseQueue(ReleaseQueueError),
    CompletionQueueFull,
    InvalidState,
}

impl fmt::Display for HostRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HostRequestError {}

impl From<HandleError> for HostRequestError {
    fn from(error: HandleError) -> Self {
        Self::Handle(error)
    }
}

impl From<SlotAllocError> for HostRequestError {
    fn from(error: SlotAllocError) -> Self {
        Self::Allocation(error)
    }
}

impl From<ReleaseQueueError> for HostRequestError {
    fn from(error: ReleaseQueueError) -> Self {
        Self::ReleaseQueue(error)
    }
}

#[derive(Debug)]
pub struct HostRequestManager {
    realm_id: u32,
    requests: SlotPool<HostRequest>,
    completions: VecDeque<Completion>,
    completion_capacity: usize,
    discarded_late_results: u64,
}

impl HostRequestManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            requests: SlotPool::with_capacity_limit(realm_id, capacity),
            completions: VecDeque::with_capacity(capacity as usize),
            completion_capacity: capacity as usize,
            discarded_late_results: 0,
        }
    }

    pub fn create(
        &mut self,
        epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Result<HostRequestHandle, HostRequestError> {
        let release = releases.reserve()?;
        match self.requests.try_allocate(HostRequest {
            epoch,
            state: HostRequestState::Pending,
            release: Some(release),
        }) {
            Ok(handle) => Ok(HostRequestHandle(handle)),
            Err(error) => {
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    pub fn complete_from_host(
        &mut self,
        request: HostRequestHandle,
        epoch: u64,
        value: i32,
    ) -> Result<(), HostRequestError> {
        if self.completions.len() >= self.completion_capacity {
            return Err(HostRequestError::CompletionQueueFull);
        }
        self.completions.push_back(Completion {
            request,
            epoch,
            value,
        });
        Ok(())
    }

    pub fn drain_completions(
        &mut self,
        current_epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Vec<(HostRequestHandle, i32)> {
        let mut accepted = Vec::new();
        while let Some(completion) = self.completions.pop_front() {
            let Ok(request) = self.requests.resolve_mut(completion.request.raw()) else {
                self.discarded_late_results += 1;
                continue;
            };
            if request.epoch != current_epoch
                || completion.epoch != current_epoch
                || request.state != HostRequestState::Pending
            {
                self.discarded_late_results += 1;
                continue;
            }
            request.state = HostRequestState::Completed;
            accepted.push((completion.request, completion.value));
            enqueue_request_release(self.realm_id, completion.request, request, releases);
        }
        accepted
    }

    pub fn cancel(
        &mut self,
        request: HostRequestHandle,
        detach: bool,
        releases: &mut ReleaseQueue,
    ) -> Result<(), HostRequestError> {
        let request_state = self.requests.resolve_mut(request.raw())?;
        if request_state.state != HostRequestState::Pending {
            return Err(HostRequestError::InvalidState);
        }
        request_state.state = if detach {
            HostRequestState::Detached
        } else {
            HostRequestState::Cancelled
        };
        enqueue_request_release(self.realm_id, request, request_state, releases);
        Ok(())
    }

    #[must_use]
    pub const fn discarded_late_results(&self) -> u64 {
        self.discarded_late_results
    }
}

fn enqueue_request_release(
    realm_id: u32,
    handle: HostRequestHandle,
    request: &mut HostRequest,
    releases: &mut ReleaseQueue,
) {
    if let Some(reservation) = request.release.take() {
        releases
            .enqueue_reserved(
                reservation,
                ReleaseRecord {
                    realm_id,
                    kind: ReleaseKind::HostRequest,
                    object_id: u64::from(handle.raw().generation) << 32
                        | u64::from(handle.raw().index),
                    domain: RuntimeHostDomain::Io,
                },
            )
            .expect("a request owns its release reservation");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceTokenHandle(RawHandle);

#[derive(Debug)]
struct ResourceToken {
    owner: TaskHandle,
    domain: RuntimeHostDomain,
    released: bool,
    release: Option<ReleaseReservation>,
}

#[derive(Debug)]
pub struct ResourceTokenManager {
    realm_id: u32,
    tokens: SlotPool<ResourceToken>,
}

impl ResourceTokenManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            tokens: SlotPool::with_capacity_limit(realm_id, capacity),
        }
    }

    pub fn create(
        &mut self,
        owner: TaskHandle,
        domain: RuntimeHostDomain,
        releases: &mut ReleaseQueue,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        let release = releases.reserve()?;
        match self.tokens.try_allocate(ResourceToken {
            owner,
            domain,
            released: false,
            release: Some(release),
        }) {
            Ok(handle) => Ok(ResourceTokenHandle(handle)),
            Err(error) => {
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    pub fn release(
        &mut self,
        handle: ResourceTokenHandle,
        releases: &mut ReleaseQueue,
    ) -> Result<bool, HostRequestError> {
        let token = self.tokens.resolve_mut(handle.0)?;
        if token.released {
            return Ok(false);
        }
        token.released = true;
        let reservation = token
            .release
            .take()
            .expect("unreleased token owns reservation");
        releases.enqueue_reserved(
            reservation,
            ReleaseRecord {
                realm_id: self.realm_id,
                kind: ReleaseKind::ResourceToken,
                object_id: u64::from(handle.0.generation) << 32 | u64::from(handle.0.index),
                domain: token.domain,
            },
        )?;
        Ok(true)
    }

    pub fn release_owned_by(
        &mut self,
        owner: TaskHandle,
        releases: &mut ReleaseQueue,
    ) -> Result<usize, HostRequestError> {
        let handles = self
            .tokens
            .occupied_handles()
            .into_iter()
            .filter(|handle| {
                self.tokens
                    .resolve(*handle)
                    .is_ok_and(|token| token.owner == owner && !token.released)
            })
            .map(ResourceTokenHandle)
            .collect::<Vec<_>>();
        for handle in &handles {
            self.release(*handle, releases)?;
        }
        Ok(handles.len())
    }
}

#[derive(Clone, Debug)]
pub struct CopyBuffer<T> {
    data: Vec<T>,
}

impl<T> CopyBuffer<T> {
    #[must_use]
    pub fn new(data: Vec<T>) -> Self {
        Self { data }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.data
    }
}

#[derive(Debug)]
pub struct ImmutableSnapshot<T> {
    data: Arc<[T]>,
    external_bytes: usize,
    release: ReleaseReservation,
    realm_id: u32,
    id: u64,
}

impl<T> ImmutableSnapshot<T> {
    pub fn new(
        realm_id: u32,
        id: u64,
        data: Arc<[T]>,
        external_bytes: usize,
        releases: &mut ReleaseQueue,
    ) -> Result<Self, ReleaseQueueError> {
        Ok(Self {
            data,
            external_bytes,
            release: releases.reserve()?,
            realm_id,
            id,
        })
    }

    #[must_use]
    pub fn data(&self) -> &[T] {
        &self.data
    }

    #[must_use]
    pub const fn external_bytes(&self) -> usize {
        self.external_bytes
    }

    pub fn share(&self, releases: &mut ReleaseQueue) -> Result<Self, ReleaseQueueError> {
        Ok(Self {
            data: Arc::clone(&self.data),
            external_bytes: self.external_bytes,
            release: releases.reserve()?,
            realm_id: self.realm_id,
            id: self.id,
        })
    }

    pub fn release(self, releases: &mut ReleaseQueue) -> Result<(), ReleaseQueueError> {
        releases.enqueue_reserved(
            self.release,
            ReleaseRecord {
                realm_id: self.realm_id,
                kind: ReleaseKind::Snapshot,
                object_id: self.id,
                domain: RuntimeHostDomain::VmThread,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        HostRequestManager, ImmutableSnapshot, ReleaseKind, ReleaseQueue, ReleaseQueueState,
        ResourceTokenManager, RuntimeHostDomain,
    };
    use crate::{RuntimeLimits, TaskRuntime};

    #[test]
    fn late_completion_is_discarded_and_release_is_pre_reserved() {
        let mut releases = ReleaseQueue::new(1);
        let mut requests = HostRequestManager::new(3, 1);
        let request = requests.create(7, &mut releases).unwrap();
        requests.complete_from_host(request, 7, 9).unwrap();
        assert!(requests.drain_completions(8, &mut releases).is_empty());
        assert_eq!(requests.discarded_late_results(), 1);
        requests.cancel(request, true, &mut releases).unwrap();
        let records = releases.drain().collect::<Vec<_>>();
        assert_eq!(records[0].kind, ReleaseKind::HostRequest);
    }

    #[test]
    fn snapshot_shares_read_only_data_and_queue_recovers_after_drain() {
        let mut releases = ReleaseQueue::new(1);
        let snapshot =
            ImmutableSnapshot::new(1, 2, Arc::<[i32]>::from([1, 2, 3]), 12, &mut releases).unwrap();
        assert_eq!(snapshot.data(), &[1, 2, 3]);
        assert!(releases.reserve().is_err());
        assert_eq!(releases.state(), ReleaseQueueState::Stalled);
        snapshot.release(&mut releases).unwrap();
        assert_eq!(releases.drain().count(), 1);
        assert_eq!(releases.state(), ReleaseQueueState::Healthy);
    }

    #[test]
    fn resource_release_is_idempotent_and_realm_records_can_be_reparented() {
        let mut runtime = TaskRuntime::new(9, RuntimeLimits::default());
        let scope = runtime.create_scope(None).unwrap();
        let task = runtime.admit_task(scope, 1, true).unwrap();
        let mut releases = ReleaseQueue::new(2);
        let mut resources = ResourceTokenManager::new(9, 2);
        let token = resources
            .create(task, RuntimeHostDomain::Render, &mut releases)
            .unwrap();
        assert_eq!(resources.release(token, &mut releases), Ok(true));
        assert_eq!(resources.release(token, &mut releases), Ok(false));
        releases.reparent_realm(9, 99);
        let record = releases.drain().next().unwrap();
        assert_eq!(record.realm_id, 99);
        assert_eq!(record.kind, ReleaseKind::ResourceToken);
    }
}
