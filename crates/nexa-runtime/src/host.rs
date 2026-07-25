use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use nexa_core::{RawHandle, StableId};

use crate::machines::host_request;
use crate::machines::release_queue;
use crate::machines::resource_token;
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
    Backlog,
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
    machine_state: release_queue::State,
}

/// Process-level host domain that outlives realms and owns deferred release delivery.
#[derive(Clone, Debug)]
pub struct RuntimeHost {
    releases: Arc<Mutex<VecDeque<ReleaseRecord>>>,
}

impl RuntimeHost {
    #[must_use]
    pub fn new(release_capacity: usize) -> Self {
        Self {
            releases: Arc::new(Mutex::new(VecDeque::with_capacity(release_capacity))),
        }
    }

    pub(crate) fn submit_releases(
        &self,
        records: impl IntoIterator<Item = ReleaseRecord>,
    ) -> usize {
        let mut queue = self
            .releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut submitted = 0;
        for record in records {
            queue.push_back(record);
            submitted += 1;
        }
        submitted
    }

    #[must_use]
    pub fn drain_releases(&self) -> Vec<ReleaseRecord> {
        self.releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    #[must_use]
    pub fn pending_releases(&self) -> usize {
        self.releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl ReleaseQueue {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::with_capacity(capacity),
            capacity,
            reserved: BTreeSet::new(),
            next_reservation: 0,
            machine_state: release_queue::State::Healthy,
        }
    }

    pub fn reserve(&mut self) -> Result<ReleaseReservation, ReleaseQueueError> {
        if self.records.len() + self.reserved.len() >= self.capacity {
            self.transition_to_stalled();
            return Err(ReleaseQueueError::Capacity);
        }
        let reservation = self.next_reservation;
        self.next_reservation = self
            .next_reservation
            .checked_add(1)
            .ok_or(ReleaseQueueError::Capacity)?;
        self.reserved.insert(reservation);
        self.refresh_state();
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
        self.refresh_state();
        Ok(())
    }

    pub fn cancel_reservation(&mut self, reservation: ReleaseReservation) {
        self.reserved.remove(&reservation.0);
        self.refresh_state();
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ReleaseRecord> {
        let records = self.records.drain(..).collect::<Vec<_>>();
        self.refresh_state();
        records.into_iter()
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
        match self.machine_state {
            release_queue::State::Healthy => ReleaseQueueState::Healthy,
            release_queue::State::ReleaseBacklog => ReleaseQueueState::Backlog,
            release_queue::State::ResourceStalled => ReleaseQueueState::Stalled,
        }
    }

    fn refresh_state(&mut self) {
        let used = self.records.len() + self.reserved.len();
        if used >= self.capacity {
            self.transition_to_stalled();
        } else if used > 0 {
            if self.machine_state == release_queue::State::ResourceStalled {
                self.apply_release_event(release_queue::Event::Recover);
            }
            if self.machine_state == release_queue::State::Healthy {
                self.apply_release_event(release_queue::Event::SoftLimit);
            }
        } else {
            match self.machine_state {
                release_queue::State::ReleaseBacklog => {
                    self.apply_release_event(release_queue::Event::Drained);
                }
                release_queue::State::ResourceStalled => {
                    self.apply_release_event(release_queue::Event::Recover);
                }
                release_queue::State::Healthy => {}
            }
        }
    }

    fn transition_to_stalled(&mut self) {
        if self.machine_state == release_queue::State::Healthy {
            self.apply_release_event(release_queue::Event::SoftLimit);
        }
        if self.machine_state == release_queue::State::ReleaseBacklog {
            self.apply_release_event(release_queue::Event::HardLimit);
        }
    }

    fn apply_release_event(&mut self, event: release_queue::Event) {
        self.machine_state = release_queue::apply(self.machine_state, event, |_| true)
            .expect("release queue event follows generated state machine")
            .state;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    module_id: u32,
    epoch: u64,
    state: host_request::State,
    release: Option<ReleaseReservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPayload {
    I32(i32),
    Bool(bool),
    Opaque(u64),
    Token(ResourceTokenHandle),
    Snapshot(SnapshotHandle),
    Unit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostValue {
    I32(i32),
    F32(f32),
    Bool(bool),
    String(String),
    Opaque(u64),
    Struct(Vec<HostValue>),
    Request(HostRequestHandle),
    Token(ResourceTokenHandle),
    Snapshot(SnapshotHandle),
    Unit,
}

#[derive(Clone, Copy, Debug)]
pub struct HostArgs<'a> {
    values: &'a [HostValue],
}

impl<'a> HostArgs<'a> {
    #[must_use]
    pub const fn new(values: &'a [HostValue]) -> Self {
        Self { values }
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.values.is_empty()
    }

    pub fn get(self, index: usize) -> Result<&'a HostValue, HostTrap> {
        self.values.get(index).ok_or(HostTrap::Arity)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostCallOutcome {
    Immediate(HostValue),
    Pending(HostRequestHandle),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostTrap {
    UnknownFunction(u32),
    Arity,
    Type,
    Panicked,
    Host(String),
}

pub trait HostRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        None
    }

    fn call(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: HostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap>;
}

pub trait ScriptFunction {
    type Args;
    type Output;
    const FUNCTION_ID: u32;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCompletion {
    pub realm_id: u32,
    pub module_id: u32,
    pub epoch: u64,
    pub request: HostRequestHandle,
    pub payload: HostPayload,
}

#[derive(Debug)]
struct CompletionQueue {
    items: VecDeque<HostCompletion>,
    capacity: usize,
    reserved: usize,
    closed: bool,
}

#[derive(Clone, Debug)]
pub struct HostCompletionSender {
    queue: Arc<Mutex<CompletionQueue>>,
}

impl HostCompletionSender {
    pub fn complete(&self, completion: HostCompletion) -> Result<(), HostRequestError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.closed || queue.reserved == 0 || queue.items.len() >= queue.capacity {
            return Err(HostRequestError::CompletionQueueFull);
        }
        queue.reserved -= 1;
        queue.items.push_back(completion);
        Ok(())
    }
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
pub(crate) struct HostRequestManager {
    realm_id: u32,
    requests: SlotPool<HostRequest>,
    completions: Arc<Mutex<CompletionQueue>>,
    discarded_late_results: u64,
    terminal_records: VecDeque<RequestTerminalRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestTerminalRecord {
    pub request: HostRequestHandle,
    pub state: HostRequestState,
    pub epoch: u64,
}

impl HostRequestManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            requests: SlotPool::with_capacity_limit(realm_id, capacity),
            completions: Arc::new(Mutex::new(CompletionQueue {
                items: VecDeque::with_capacity(capacity as usize),
                capacity: capacity as usize,
                reserved: 0,
                closed: false,
            })),
            discarded_late_results: 0,
            terminal_records: VecDeque::with_capacity(capacity as usize),
        }
    }

    #[must_use]
    pub fn completion_sender(&self) -> HostCompletionSender {
        HostCompletionSender {
            queue: Arc::clone(&self.completions),
        }
    }

    #[cfg(test)]
    pub fn create(
        &mut self,
        epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Result<HostRequestHandle, HostRequestError> {
        self.create_for_module(0, epoch, releases)
    }

    pub fn create_for_module(
        &mut self,
        module_id: u32,
        epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Result<HostRequestHandle, HostRequestError> {
        let release = releases.reserve()?;
        {
            let mut queue = self
                .completions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.items.len() + queue.reserved >= queue.capacity {
                releases.cancel_reservation(release);
                return Err(HostRequestError::CompletionQueueFull);
            }
            queue.reserved += 1;
        }
        let submitted = host_request::apply(
            host_request::State::Created,
            host_request::Event::Submit,
            |_| true,
        )
        .expect("generated host request submit transition exists");
        let in_flight =
            host_request::apply(submitted.state, host_request::Event::Dispatch, |_| true)
                .expect("generated host request dispatch transition exists");
        match self.requests.try_allocate(HostRequest {
            module_id,
            epoch,
            state: in_flight.state,
            release: Some(release),
        }) {
            Ok(handle) => Ok(HostRequestHandle(handle)),
            Err(error) => {
                self.cancel_completion_reservation();
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    #[cfg(test)]
    pub fn complete_from_host(
        &mut self,
        request: HostRequestHandle,
        epoch: u64,
        value: i32,
    ) -> Result<(), HostRequestError> {
        self.completion_sender().complete(HostCompletion {
            realm_id: self.realm_id,
            module_id: 0,
            request,
            epoch,
            payload: HostPayload::I32(value),
        })
    }

    pub fn drain_completions(
        &mut self,
        current_epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Vec<(HostRequestHandle, HostPayload)> {
        let mut accepted = Vec::new();
        let completions = {
            let mut queue = self
                .completions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.items.drain(..).collect::<Vec<_>>()
        };
        for completion in completions {
            let Ok(request) = self.requests.resolve_mut(completion.request.raw()) else {
                self.discarded_late_results += 1;
                continue;
            };
            if completion.realm_id != self.realm_id
                || request.epoch != current_epoch
                || completion.epoch != current_epoch
                || completion.module_id != request.module_id
                || request.state != host_request::State::InFlight
            {
                self.discarded_late_results += 1;
                continue;
            }
            request.state =
                host_request::apply(request.state, host_request::Event::Complete, |_| true)
                    .expect("generated host request complete transition exists")
                    .state;
            accepted.push((completion.request, completion.payload));
            enqueue_request_release(self.realm_id, completion.request, request, releases);
            request.state =
                host_request::apply(request.state, host_request::Event::Release, |_| true)
                    .expect("generated completed request release transition exists")
                    .state;
            let terminal = RequestTerminalRecord {
                request: completion.request,
                state: HostRequestState::Completed,
                epoch: request.epoch,
            };
            let _ = request;
            self.requests
                .release(completion.request.raw())
                .expect("resolved request remains live");
            self.push_terminal(terminal);
        }
        accepted
    }

    pub fn cancel(
        &mut self,
        request: HostRequestHandle,
        detach: bool,
        releases: &mut ReleaseQueue,
    ) -> Result<(), HostRequestError> {
        if self.requests.resolve(request.raw())?.state != host_request::State::InFlight {
            return Err(HostRequestError::InvalidState);
        }
        if !detach {
            self.cancel_completion_reservation();
        }
        let request_state = self.requests.resolve_mut(request.raw())?;
        request_state.state = host_request::apply(
            request_state.state,
            host_request::Event::RequestCancel,
            |_| true,
        )
        .expect("generated host request cancellation transition exists")
        .state;
        request_state.state =
            host_request::apply(request_state.state, host_request::Event::Detach, |_| true)
                .expect("generated host request detach transition exists")
                .state;
        enqueue_request_release(self.realm_id, request, request_state, releases);
        request_state.state =
            host_request::apply(request_state.state, host_request::Event::Release, |_| true)
                .expect("generated detached request release transition exists")
                .state;
        let terminal = RequestTerminalRecord {
            request,
            state: if detach {
                HostRequestState::Detached
            } else {
                HostRequestState::Cancelled
            },
            epoch: request_state.epoch,
        };
        let _ = request_state;
        self.requests.release(request.raw())?;
        self.push_terminal(terminal);
        Ok(())
    }

    #[must_use]
    pub const fn discarded_late_results(&self) -> u64 {
        self.discarded_late_results
    }

    fn cancel_completion_reservation(&self) {
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.reserved = queue.reserved.saturating_sub(1);
    }

    fn push_terminal(&mut self, record: RequestTerminalRecord) {
        if self.terminal_records.len() == self.terminal_records.capacity() {
            self.terminal_records.pop_front();
        }
        self.terminal_records.push_back(record);
    }
}

impl Drop for HostRequestManager {
    fn drop(&mut self) {
        self.completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResourceTokenHandle(RawHandle);

#[derive(Debug)]
struct ResourceToken {
    domain: RuntimeHostDomain,
    state: resource_token::State,
    release: Option<ReleaseReservation>,
}

#[derive(Debug)]
pub(crate) struct ResourceTokenManager {
    realm_id: u32,
    tokens: SlotPool<ResourceToken>,
    terminal: BTreeSet<RawHandle>,
    terminal_capacity: usize,
}

impl ResourceTokenManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            tokens: SlotPool::with_capacity_limit(realm_id, capacity),
            terminal: BTreeSet::new(),
            terminal_capacity: capacity as usize,
        }
    }

    pub fn create(
        &mut self,
        _owner: TaskHandle,
        domain: RuntimeHostDomain,
        releases: &mut ReleaseQueue,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        let release = releases.reserve()?;
        let acquired = resource_token::apply(
            resource_token::State::Reserved,
            resource_token::Event::HostAcquire,
            |_| true,
        )
        .expect("generated resource acquire transition exists");
        let published =
            resource_token::apply(acquired.state, resource_token::Event::Publish, |_| true)
                .expect("generated resource publish transition exists");
        match self.tokens.try_allocate(ResourceToken {
            domain,
            state: published.state,
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
        if self.terminal.contains(&handle.0) {
            return Ok(false);
        }
        let token = self.tokens.resolve_mut(handle.0)?;
        token.state =
            resource_token::apply(token.state, resource_token::Event::EnqueueRelease, |_| true)
                .expect("generated resource enqueue transition exists")
                .state;
        let reservation = token
            .release
            .take()
            .expect("unreleased token owns reservation");
        let domain = token.domain;
        releases.enqueue_reserved(
            reservation,
            ReleaseRecord {
                realm_id: self.realm_id,
                kind: ReleaseKind::ResourceToken,
                object_id: u64::from(handle.0.generation) << 32 | u64::from(handle.0.index),
                domain,
            },
        )?;
        token.state =
            resource_token::apply(token.state, resource_token::Event::HostRelease, |_| true)
                .expect("generated resource release transition exists")
                .state;
        let _ = token;
        self.tokens.release(handle.0)?;
        if self.terminal.len() == self.terminal_capacity {
            let oldest = self.terminal.iter().next().copied();
            if let Some(oldest) = oldest {
                self.terminal.remove(&oldest);
            }
        }
        self.terminal.insert(handle.0);
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotHandle(RawHandle);

impl SnapshotHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Debug)]
struct SnapshotEntry {
    data: Arc<[i32]>,
    external_bytes: usize,
    release: ReleaseReservation,
}

#[derive(Debug)]
pub(crate) struct SnapshotManager {
    realm_id: u32,
    snapshots: SlotPool<SnapshotEntry>,
}

impl SnapshotManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            snapshots: SlotPool::with_capacity_limit(realm_id, capacity),
        }
    }

    fn create(
        &mut self,
        _owner: TaskHandle,
        data: Arc<[i32]>,
        releases: &mut ReleaseQueue,
    ) -> Result<SnapshotHandle, HostRequestError> {
        let release = releases.reserve()?;
        let external_bytes = data.len().saturating_mul(std::mem::size_of::<i32>());
        match self.snapshots.try_allocate(SnapshotEntry {
            data,
            external_bytes,
            release,
        }) {
            Ok(raw) => Ok(SnapshotHandle(raw)),
            Err(error) => {
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    pub fn data(&self, handle: SnapshotHandle) -> Result<&[i32], HostRequestError> {
        Ok(&self.snapshots.resolve(handle.raw())?.data)
    }

    pub fn external_bytes(&self, handle: SnapshotHandle) -> Result<usize, HostRequestError> {
        Ok(self.snapshots.resolve(handle.raw())?.external_bytes)
    }

    fn release(
        &mut self,
        handle: SnapshotHandle,
        releases: &mut ReleaseQueue,
    ) -> Result<(), HostRequestError> {
        let snapshot = self.snapshots.release(handle.raw())?;
        releases.enqueue_reserved(
            snapshot.release,
            ReleaseRecord {
                realm_id: self.realm_id,
                kind: ReleaseKind::Snapshot,
                object_id: u64::from(handle.raw().generation) << 32 | u64::from(handle.raw().index),
                domain: RuntimeHostDomain::VmThread,
            },
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskResourceSet {
    pub requests: BTreeSet<HostRequestHandle>,
    pub tokens: BTreeSet<ResourceTokenHandle>,
    pub snapshots: BTreeSet<SnapshotHandle>,
}

#[derive(Debug)]
pub struct RuntimeResources {
    requests: HostRequestManager,
    tokens: ResourceTokenManager,
    snapshots: SnapshotManager,
    releases: ReleaseQueue,
    ownership: BTreeMap<TaskHandle, TaskResourceSet>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeResourceSnapshot {
    pub requests: usize,
    pub tokens: usize,
    pub snapshots: usize,
    pub release_records: usize,
    pub release_reservations: usize,
}

impl RuntimeResources {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32, release_capacity: usize) -> Self {
        Self {
            requests: HostRequestManager::new(realm_id, capacity),
            tokens: ResourceTokenManager::new(realm_id, capacity),
            snapshots: SnapshotManager::new(realm_id, capacity),
            releases: ReleaseQueue::new(release_capacity),
            ownership: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn completion_sender(&self) -> HostCompletionSender {
        self.requests.completion_sender()
    }

    pub fn context(&mut self, task: TaskHandle, module_id: u32, epoch: u64) -> ResourceContext<'_> {
        ResourceContext {
            task,
            module_id,
            epoch,
            resources: self,
        }
    }

    pub fn drain_completions(&mut self, epoch: u64) -> Vec<(HostRequestHandle, HostPayload)> {
        self.requests
            .drain_completions(epoch, &mut self.releases)
            .into_iter()
            .map(|(request, payload)| {
                for resources in self.ownership.values_mut() {
                    resources.requests.remove(&request);
                }
                (request, payload)
            })
            .collect()
    }

    pub fn cleanup_task(
        &mut self,
        task: TaskHandle,
        detach_requests: bool,
    ) -> Result<(), HostRequestError> {
        let Some(owned) = self.ownership.remove(&task) else {
            return Ok(());
        };
        for request in owned.requests {
            match self
                .requests
                .cancel(request, detach_requests, &mut self.releases)
            {
                Ok(()) | Err(HostRequestError::Handle(_)) => {}
                Err(error) => return Err(error),
            }
        }
        for token in owned.tokens {
            self.tokens.release(token, &mut self.releases)?;
        }
        for snapshot in owned.snapshots {
            self.snapshots.release(snapshot, &mut self.releases)?;
        }
        Ok(())
    }

    pub fn drain_releases(&mut self) -> Vec<ReleaseRecord> {
        self.releases.drain().collect()
    }

    pub fn snapshot_data(&self, snapshot: SnapshotHandle) -> Result<&[i32], HostRequestError> {
        self.snapshots.data(snapshot)
    }

    pub fn snapshot_external_bytes(
        &self,
        snapshot: SnapshotHandle,
    ) -> Result<usize, HostRequestError> {
        self.snapshots.external_bytes(snapshot)
    }

    #[must_use]
    pub fn ownership(&self, task: TaskHandle) -> Option<&TaskResourceSet> {
        self.ownership.get(&task)
    }

    #[must_use]
    pub fn owns_request(&self, task: TaskHandle, request: HostRequestHandle) -> bool {
        self.ownership
            .get(&task)
            .is_some_and(|resources| resources.requests.contains(&request))
    }

    #[must_use]
    pub const fn discarded_late_results(&self) -> u64 {
        self.requests.discarded_late_results()
    }

    #[must_use]
    pub(crate) fn reserved_capacities(&self) -> (usize, usize) {
        (
            self.requests.requests.reserved_capacity(),
            self.releases.records.capacity(),
        )
    }

    #[must_use]
    pub fn model_snapshot(&self) -> RuntimeResourceSnapshot {
        RuntimeResourceSnapshot {
            requests: self.requests.requests.occupied_len(),
            tokens: self.tokens.tokens.occupied_len(),
            snapshots: self.snapshots.snapshots.occupied_len(),
            release_records: self.releases.records.len(),
            release_reservations: self.releases.reserved.len(),
        }
    }
}

pub struct ResourceContext<'a> {
    task: TaskHandle,
    module_id: u32,
    epoch: u64,
    resources: &'a mut RuntimeResources,
}

impl ResourceContext<'_> {
    pub fn create_request(&mut self) -> Result<HostRequestHandle, HostRequestError> {
        let request = self.resources.requests.create_for_module(
            self.module_id,
            self.epoch,
            &mut self.resources.releases,
        )?;
        self.resources
            .ownership
            .entry(self.task)
            .or_default()
            .requests
            .insert(request);
        Ok(request)
    }

    pub fn create_token(
        &mut self,
        domain: RuntimeHostDomain,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        let token =
            self.resources
                .tokens
                .create(self.task, domain, &mut self.resources.releases)?;
        self.resources
            .ownership
            .entry(self.task)
            .or_default()
            .tokens
            .insert(token);
        Ok(token)
    }

    pub fn create_snapshot(
        &mut self,
        data: Arc<[i32]>,
    ) -> Result<SnapshotHandle, HostRequestError> {
        let snapshot =
            self.resources
                .snapshots
                .create(self.task, data, &mut self.resources.releases)?;
        self.resources
            .ownership
            .entry(self.task)
            .or_default()
            .snapshots
            .insert(snapshot);
        Ok(snapshot)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{
        HostCompletion, HostPayload, HostRequestError, HostRequestManager, ReleaseKind,
        ReleaseQueue, ReleaseQueueState, ResourceTokenManager, RuntimeHostDomain, SnapshotManager,
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
        let mut runtime = TaskRuntime::new(1, RuntimeLimits::default());
        let scope = runtime.create_scope(None).unwrap();
        let task = runtime.admit_task(scope, 1, true).unwrap();
        let mut releases = ReleaseQueue::new(1);
        let mut snapshots = SnapshotManager::new(1, 1);
        let snapshot = snapshots
            .create(task, Arc::<[i32]>::from([1, 2, 3]), &mut releases)
            .unwrap();
        assert_eq!(snapshots.data(snapshot).unwrap(), &[1, 2, 3]);
        assert_eq!(snapshots.external_bytes(snapshot).unwrap(), 12);
        assert!(releases.reserve().is_err());
        assert_eq!(releases.state(), ReleaseQueueState::Stalled);
        snapshots.release(snapshot, &mut releases).unwrap();
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

    #[test]
    fn completion_sender_is_send_sync_and_request_slots_recycle_under_threads() {
        const COUNT: u32 = 32;

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::HostCompletionSender>();

        let mut releases = ReleaseQueue::new(COUNT as usize);
        let mut requests = HostRequestManager::new(7, COUNT);
        let handles = (0..COUNT)
            .map(|_| requests.create_for_module(3, 9, &mut releases).unwrap())
            .collect::<Vec<_>>();
        let sender = requests.completion_sender();
        let workers = handles
            .iter()
            .copied()
            .map(|request| {
                let sender = sender.clone();
                thread::spawn(move || {
                    sender
                        .complete(HostCompletion {
                            realm_id: 7,
                            module_id: 3,
                            epoch: 9,
                            request,
                            payload: HostPayload::I32(1),
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            requests.drain_completions(9, &mut releases).len(),
            COUNT as usize
        );
        assert_eq!(releases.drain().count(), COUNT as usize);
        for _ in 0..COUNT {
            requests.create_for_module(3, 9, &mut releases).unwrap();
        }
        drop(requests);
        assert_eq!(
            sender.complete(HostCompletion {
                realm_id: 7,
                module_id: 3,
                epoch: 9,
                request: handles[0],
                payload: HostPayload::Unit,
            }),
            Err(HostRequestError::CompletionQueueFull)
        );
    }
}
