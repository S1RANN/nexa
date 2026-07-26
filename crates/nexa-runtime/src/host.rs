use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
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
}

pub const RELEASE_DOMAIN_COUNT: usize = 4;

impl RuntimeHostDomain {
    const ALL: [Self; RELEASE_DOMAIN_COUNT] = [Self::VmThread, Self::Render, Self::Audio, Self::Io];

    #[must_use]
    pub const fn bucket(self) -> usize {
        match self {
            Self::VmThread => 0,
            Self::Render => 1,
            Self::Audio => 2,
            Self::Io => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseReservation {
    node: usize,
    module_id: u32,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseKind {
    HostRequest,
    ResourceToken,
    Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseRecord {
    pub realm_id: u32,
    pub module_id: u32,
    pub epoch: u64,
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
    HostClosing,
    HostClosed,
}

#[derive(Clone, Copy, Debug, Default)]
struct IntrusiveReleaseList {
    head: Option<usize>,
    tail: Option<usize>,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseNodeState {
    Free,
    Reserved,
    Queued,
}

#[derive(Clone, Copy, Debug)]
struct ReleaseNode {
    record: Option<ReleaseRecord>,
    next: Option<usize>,
    state: ReleaseNodeState,
}

#[derive(Debug)]
struct ReleaseNodePool {
    nodes: Vec<ReleaseNode>,
    free_head: Option<usize>,
}

impl ReleaseNodePool {
    fn new(capacity: usize) -> Self {
        let mut nodes = Vec::with_capacity(capacity);
        for index in 0..capacity {
            nodes.push(ReleaseNode {
                record: None,
                next: (index + 1 < capacity).then_some(index + 1),
                state: ReleaseNodeState::Free,
            });
        }
        Self {
            nodes,
            free_head: (capacity > 0).then_some(0),
        }
    }

    fn reserve(&mut self) -> Option<usize> {
        let node = self.free_head?;
        self.free_head = self.nodes[node].next.take();
        self.nodes[node].state = ReleaseNodeState::Reserved;
        Some(node)
    }

    fn release(&mut self, node: usize) {
        self.nodes[node].record = None;
        self.nodes[node].state = ReleaseNodeState::Free;
        self.nodes[node].next = self.free_head;
        self.free_head = Some(node);
    }
}

#[derive(Debug)]
struct ReleaseDomainState {
    pool: ReleaseNodePool,
    host_lists: [IntrusiveReleaseList; RELEASE_DOMAIN_COUNT],
}

#[derive(Clone, Copy, Debug, Default)]
struct EpochReleaseCount {
    module_id: u32,
    epoch: u64,
    generation: u64,
    queued: usize,
    reserved: usize,
}

#[derive(Clone, Copy, Debug)]
struct EpochCount {
    module_id: u32,
    epoch: u64,
    count: usize,
}

#[derive(Debug)]
struct EpochCounts {
    slots: Vec<EpochCount>,
    capacity: usize,
}

impl EpochCounts {
    fn new(capacity: usize) -> Self {
        Self {
            slots: Vec::with_capacity(capacity),
            capacity,
        }
    }

    fn increment(&mut self, module_id: u32, epoch: u64) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.module_id == module_id && slot.epoch == epoch)
        {
            slot.count = slot.count.saturating_add(1);
            return;
        }
        assert!(
            self.slots.len() < self.capacity,
            "epoch count slots are preflighted by resource capacity"
        );
        self.slots.push(EpochCount {
            module_id,
            epoch,
            count: 1,
        });
    }

    fn decrement(&mut self, module_id: u32, epoch: u64) {
        let index = self
            .slots
            .iter()
            .position(|slot| slot.module_id == module_id && slot.epoch == epoch)
            .expect("epoch ownership count exists");
        self.slots[index].count = self.slots[index]
            .count
            .checked_sub(1)
            .expect("epoch ownership count is positive");
        if self.slots[index].count == 0 {
            self.slots.swap_remove(index);
        }
    }

    fn get(&self, module_id: u32, epoch: u64) -> usize {
        self.slots
            .iter()
            .find(|slot| slot.module_id == module_id && slot.epoch == epoch)
            .map_or(0, |slot| slot.count)
    }
}

#[derive(Clone, Debug)]
struct ReleaseDomain {
    inner: Arc<Mutex<ReleaseDomainState>>,
}

impl ReleaseDomain {
    fn new(capacity: usize) -> Self {
        let inner = Arc::new(Mutex::new(ReleaseDomainState {
            pool: ReleaseNodePool::new(capacity),
            host_lists: [IntrusiveReleaseList::default(); RELEASE_DOMAIN_COUNT],
        }));
        drop(
            inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Self { inner }
    }
}

#[derive(Debug)]
pub struct ReleaseQueue {
    domain: ReleaseDomain,
    host_admission: Option<HostAdmissionGate>,
    lists: [IntrusiveReleaseList; RELEASE_DOMAIN_COUNT],
    epoch_pending: Vec<EpochReleaseCount>,
    transfer_generation: u64,
    capacity: usize,
    reserved: usize,
    machine_state: release_queue::State,
}

/// Process-level host domain that outlives realms and owns deferred release delivery.
#[derive(Clone, Debug)]
pub struct RuntimeHost {
    releases: ReleaseDomain,
    admission: HostAdmissionGate,
    pending_completions: Arc<AtomicUsize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeHostState {
    #[default]
    Open,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Default)]
struct RuntimeHostLifecycle {
    live_realms: usize,
    state: RuntimeHostState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostAdmissionKind {
    HostedRealm,
    AsyncHostCall,
    HostRequest,
    CompletionReservation,
    ResourceToken,
    Snapshot,
    ReleaseReservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostAdmissionError {
    Closing,
    Closed,
}

/// The only fact source for process-host admission decisions.
#[derive(Clone, Debug)]
struct HostAdmissionGate {
    inner: Arc<Mutex<RuntimeHostLifecycle>>,
}

impl HostAdmissionGate {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RuntimeHostLifecycle::default())),
        }
    }

    fn admit(&self, kind: HostAdmissionKind) -> Result<(), HostAdmissionError> {
        Self::decision(
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state,
            kind,
        )
    }

    const fn decision(
        state: RuntimeHostState,
        _kind: HostAdmissionKind,
    ) -> Result<(), HostAdmissionError> {
        match state {
            RuntimeHostState::Open => Ok(()),
            RuntimeHostState::Closing => Err(HostAdmissionError::Closing),
            RuntimeHostState::Closed => Err(HostAdmissionError::Closed),
        }
    }

    fn register_realm(&self) -> Result<(), HostAdmissionError> {
        let mut lifecycle = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::decision(lifecycle.state, HostAdmissionKind::HostedRealm)?;
        lifecycle.live_realms = lifecycle
            .live_realms
            .checked_add(1)
            .expect("hosted realm count cannot overflow");
        Ok(())
    }

    fn unregister_realm(&self) {
        let mut lifecycle = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.live_realms = lifecycle
            .live_realms
            .checked_sub(1)
            .expect("hosted realm registration is balanced");
    }

    fn begin_close(&self) {
        let mut lifecycle = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.state == RuntimeHostState::Open {
            lifecycle.state = RuntimeHostState::Closing;
        }
    }

    fn snapshot(&self) -> RuntimeHostLifecycle {
        *self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeHostCloseStatus {
    pub state: RuntimeHostState,
    pub live_realms: usize,
    pub pending_completions: usize,
    pub pending_releases: usize,
}

impl RuntimeHostCloseStatus {
    #[must_use]
    pub const fn is_drained(self) -> bool {
        self.live_realms == 0 && self.pending_completions == 0 && self.pending_releases == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeHostCloseError {
    NotClosing,
    LiveRealms,
    PendingCompletions,
    PendingReleases,
}

impl fmt::Display for RuntimeHostCloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for RuntimeHostCloseError {}

impl RuntimeHost {
    #[must_use]
    pub fn new(release_capacity: usize) -> Self {
        Self {
            releases: ReleaseDomain::new(release_capacity),
            admission: HostAdmissionGate::new(),
            pending_completions: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn register_realm(&self) -> Result<(), RuntimeHostState> {
        self.admission
            .register_realm()
            .map_err(|error| match error {
                HostAdmissionError::Closing => RuntimeHostState::Closing,
                HostAdmissionError::Closed => RuntimeHostState::Closed,
            })
    }

    pub(crate) fn unregister_realm(&self) {
        self.admission.unregister_realm();
    }

    /// Starts the idempotent close protocol and rejects all later admissions.
    #[must_use]
    pub fn begin_close(&self) -> RuntimeHostCloseStatus {
        self.admission.begin_close();
        self.close_status()
    }

    /// Finishes close only after every previously admitted resource has drained.
    pub fn try_finish_close(&self) -> Result<RuntimeHostCloseStatus, RuntimeHostCloseError> {
        let mut lifecycle = self
            .admission
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match lifecycle.state {
            RuntimeHostState::Open => return Err(RuntimeHostCloseError::NotClosing),
            RuntimeHostState::Closed => {
                return Ok(RuntimeHostCloseStatus {
                    state: RuntimeHostState::Closed,
                    live_realms: 0,
                    pending_completions: 0,
                    pending_releases: 0,
                });
            }
            RuntimeHostState::Closing => {}
        }
        if lifecycle.live_realms != 0 {
            return Err(RuntimeHostCloseError::LiveRealms);
        }
        let pending_completions = self.pending_completions();
        if pending_completions != 0 {
            return Err(RuntimeHostCloseError::PendingCompletions);
        }
        let pending_releases = self.pending_releases();
        if pending_releases != 0 {
            return Err(RuntimeHostCloseError::PendingReleases);
        }
        lifecycle.state = RuntimeHostState::Closed;
        let status = RuntimeHostCloseStatus {
            state: RuntimeHostState::Closed,
            live_realms: 0,
            pending_completions,
            pending_releases,
        };
        debug_assert!(self.resource_ledger().is_zero());
        Ok(status)
    }

    #[must_use]
    pub fn state(&self) -> RuntimeHostState {
        self.admission.snapshot().state
    }

    #[must_use]
    pub fn close_status(&self) -> RuntimeHostCloseStatus {
        let lifecycle = self.admission.snapshot();
        RuntimeHostCloseStatus {
            state: lifecycle.state,
            live_realms: lifecycle.live_realms,
            pending_completions: self.pending_completions(),
            pending_releases: self.pending_releases(),
        }
    }

    #[must_use]
    pub fn resource_ledger(&self) -> crate::RuntimeResourceLedger {
        crate::RuntimeResourceLedger {
            completion_reservations: crate::ledger::count(self.pending_completions()),
            queued_releases: crate::ledger::count(self.pending_releases()),
            ..crate::RuntimeResourceLedger::default()
        }
    }

    #[must_use]
    pub fn pending_completions(&self) -> usize {
        self.pending_completions.load(Ordering::Acquire)
    }

    pub(crate) fn release_queue(&self, capacity: usize) -> ReleaseQueue {
        ReleaseQueue::with_domain(
            self.releases.clone(),
            capacity,
            Some(self.admission.clone()),
        )
    }

    #[must_use]
    pub fn drain_releases(&self) -> Vec<ReleaseRecord> {
        let capacity = self.pending_releases();
        let mut records = vec![
            ReleaseRecord {
                realm_id: 0,
                module_id: 0,
                epoch: 0,
                kind: ReleaseKind::HostRequest,
                object_id: 0,
                domain: RuntimeHostDomain::VmThread,
            };
            capacity
        ];
        let count = self.drain_into(&mut records);
        records.truncate(count);
        records
    }

    #[must_use]
    pub fn drain(&self, domain: RuntimeHostDomain, max_items: usize) -> Vec<ReleaseRecord> {
        let capacity = max_items.min(self.pending_releases());
        let mut records = vec![
            ReleaseRecord {
                realm_id: 0,
                module_id: 0,
                epoch: 0,
                kind: ReleaseKind::HostRequest,
                object_id: 0,
                domain,
            };
            capacity
        ];
        let count = self.drain_domain_into(domain, &mut records);
        records.truncate(count);
        records
    }

    pub fn drain_into(&self, records: &mut [ReleaseRecord]) -> usize {
        let mut count = 0;
        for domain in RuntimeHostDomain::ALL {
            if count == records.len() {
                break;
            }
            count += self.drain_domain_into(domain, &mut records[count..]);
        }
        count
    }

    pub fn drain_domain_into(
        &self,
        domain: RuntimeHostDomain,
        records: &mut [ReleaseRecord],
    ) -> usize {
        let bucket = domain.bucket();
        let mut state = self
            .releases
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut count = 0;
        let ReleaseDomainState { pool, host_lists } = &mut *state;
        for record in records {
            let Some(node) = pop_release_node(pool, &mut host_lists[bucket]) else {
                break;
            };
            *record = pool.nodes[node]
                .record
                .take()
                .expect("queued release node owns a record");
            pool.release(node);
            count += 1;
        }
        count
    }

    #[must_use]
    pub fn pending_releases(&self) -> usize {
        self.releases
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .host_lists
            .iter()
            .map(|list| list.len)
            .sum()
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        if cfg!(debug_assertions) && Arc::strong_count(&self.admission.inner) == 1 {
            let lifecycle = self.admission.snapshot();
            if lifecycle.state != RuntimeHostState::Closed {
                eprintln!(
                    "RuntimeHost dropped in {:?}: live_realms={}, pending_completions={}, \
                     pending_releases={}",
                    lifecycle.state,
                    lifecycle.live_realms,
                    self.pending_completions(),
                    self.pending_releases()
                );
            }
        }
    }
}

impl ReleaseQueue {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_domain(ReleaseDomain::new(capacity), capacity, None)
    }

    fn with_domain(
        domain: ReleaseDomain,
        capacity: usize,
        host_admission: Option<HostAdmissionGate>,
    ) -> Self {
        Self {
            domain,
            host_admission,
            lists: [IntrusiveReleaseList::default(); RELEASE_DOMAIN_COUNT],
            epoch_pending: Vec::with_capacity(capacity),
            transfer_generation: 0,
            capacity,
            reserved: 0,
            machine_state: release_queue::State::Healthy,
        }
    }

    pub fn reserve(
        &mut self,
        module_id: u32,
        epoch: u64,
    ) -> Result<ReleaseReservation, ReleaseQueueError> {
        if let Some(admission) = &self.host_admission {
            admission
                .admit(HostAdmissionKind::ReleaseReservation)
                .map_err(|error| match error {
                    HostAdmissionError::Closing => ReleaseQueueError::HostClosing,
                    HostAdmissionError::Closed => ReleaseQueueError::HostClosed,
                })?;
        }
        if self.queued_len() + self.reserved >= self.capacity {
            self.transition_to_stalled();
            return Err(ReleaseQueueError::Capacity);
        }
        self.epoch_pending.retain(|count| {
            count.reserved > 0 || (count.generation == self.transfer_generation && count.queued > 0)
        });
        let epoch_index = self
            .epoch_pending
            .iter()
            .position(|count| count.module_id == module_id && count.epoch == epoch);
        if epoch_index.is_none() && self.epoch_pending.len() == self.epoch_pending.capacity() {
            self.transition_to_stalled();
            return Err(ReleaseQueueError::Capacity);
        }
        let Some(reservation) = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pool
            .reserve()
        else {
            self.transition_to_stalled();
            return Err(ReleaseQueueError::Capacity);
        };
        let index = epoch_index.unwrap_or_else(|| {
            self.epoch_pending.push(EpochReleaseCount {
                module_id,
                epoch,
                generation: self.transfer_generation,
                queued: 0,
                reserved: 0,
            });
            self.epoch_pending.len() - 1
        });
        let count = &mut self.epoch_pending[index];
        if count.generation != self.transfer_generation {
            count.generation = self.transfer_generation;
            count.queued = 0;
        }
        count.reserved += 1;
        self.reserved += 1;
        self.refresh_state();
        Ok(ReleaseReservation {
            node: reservation,
            module_id,
            epoch,
        })
    }

    pub fn enqueue_reserved(
        &mut self,
        reservation: ReleaseReservation,
        record: ReleaseRecord,
    ) -> Result<(), ReleaseQueueError> {
        if reservation.module_id != record.module_id || reservation.epoch != record.epoch {
            return Err(ReleaseQueueError::NotReserved);
        }
        let bucket = record.domain.bucket();
        let mut state = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(node) = state.pool.nodes.get_mut(reservation.node) else {
            return Err(ReleaseQueueError::NotReserved);
        };
        if node.state != ReleaseNodeState::Reserved {
            return Err(ReleaseQueueError::NotReserved);
        }
        node.record = Some(record);
        node.state = ReleaseNodeState::Queued;
        append_release_node(&mut state.pool, &mut self.lists[bucket], reservation.node);
        self.reserved -= 1;
        drop(state);
        let count = self
            .epoch_pending
            .iter_mut()
            .find(|count| count.module_id == record.module_id && count.epoch == record.epoch)
            .expect("resource creation prepared the epoch release index");
        if count.generation != self.transfer_generation {
            count.generation = self.transfer_generation;
            count.queued = 0;
        }
        count.reserved = count
            .reserved
            .checked_sub(1)
            .expect("release reservation count is positive");
        count.queued += 1;
        self.refresh_state();
        Ok(())
    }

    pub fn cancel_reservation(&mut self, reservation: ReleaseReservation) {
        let mut state = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .pool
            .nodes
            .get(reservation.node)
            .is_some_and(|node| node.state == ReleaseNodeState::Reserved)
        {
            state.pool.release(reservation.node);
            self.reserved -= 1;
            let index = self
                .epoch_pending
                .iter()
                .position(|count| {
                    count.module_id == reservation.module_id && count.epoch == reservation.epoch
                })
                .expect("release reservation has epoch ownership");
            let count = &mut self.epoch_pending[index];
            count.reserved = count
                .reserved
                .checked_sub(1)
                .expect("release reservation count is positive");
            if count.reserved == 0
                && (count.generation != self.transfer_generation || count.queued == 0)
            {
                self.epoch_pending.swap_remove(index);
            }
        }
        drop(state);
        self.refresh_state();
    }

    pub fn drain(&mut self) -> impl Iterator<Item = ReleaseRecord> {
        let mut records = vec![
            ReleaseRecord {
                realm_id: 0,
                module_id: 0,
                epoch: 0,
                kind: ReleaseKind::HostRequest,
                object_id: 0,
                domain: RuntimeHostDomain::VmThread,
            };
            self.queued_len()
        ];
        let count = self.drain_into(&mut records);
        records.truncate(count);
        records.into_iter()
    }

    pub fn drain_into(&mut self, records: &mut [ReleaseRecord]) -> usize {
        let mut count = 0;
        let mut state = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for list in &mut self.lists {
            while count < records.len()
                && let Some(node) = pop_release_node(&mut state.pool, list)
            {
                records[count] = state.pool.nodes[node]
                    .record
                    .take()
                    .expect("queued release node owns a record");
                state.pool.release(node);
                count += 1;
            }
        }
        drop(state);
        if count != 0 {
            self.advance_transfer_generation();
        }
        self.refresh_state();
        count
    }

    pub fn reparent_realm(&mut self, old_realm: u32, new_realm: u32) {
        let mut state = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for list in self.lists {
            let mut current = list.head;
            while let Some(node) = current {
                let entry = &mut state.pool.nodes[node];
                if let Some(record) = &mut entry.record
                    && record.realm_id == old_realm
                {
                    record.realm_id = new_realm;
                }
                current = entry.next;
            }
        }
    }

    pub fn transfer_to_host(&mut self) -> usize {
        let count = self.queued_len();
        if count == 0 {
            return 0;
        }
        let mut state = self
            .domain
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ReleaseDomainState { pool, host_lists } = &mut *state;
        for (host_list, realm_list) in host_lists.iter_mut().zip(&mut self.lists) {
            append_release_list(pool, host_list, realm_list);
        }
        drop(state);
        self.advance_transfer_generation();
        self.refresh_state();
        count
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
        let used = self.queued_len() + self.reserved;
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

    fn queued_len(&self) -> usize {
        self.lists.iter().map(|list| list.len).sum()
    }

    fn record_count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.epoch_pending
            .iter()
            .find(|count| count.module_id == module_id && count.epoch == epoch)
            .filter(|count| count.generation == self.transfer_generation)
            .map_or(0, |count| count.queued)
    }

    fn advance_transfer_generation(&mut self) {
        self.transfer_generation = self
            .transfer_generation
            .checked_add(1)
            .expect("release transfer generation exhausted");
    }
}

fn append_release_node(pool: &mut ReleaseNodePool, list: &mut IntrusiveReleaseList, node: usize) {
    pool.nodes[node].next = None;
    if let Some(tail) = list.tail {
        pool.nodes[tail].next = Some(node);
    } else {
        list.head = Some(node);
    }
    list.tail = Some(node);
    list.len += 1;
}

fn pop_release_node(pool: &mut ReleaseNodePool, list: &mut IntrusiveReleaseList) -> Option<usize> {
    let node = list.head?;
    list.head = pool.nodes[node].next.take();
    if list.head.is_none() {
        list.tail = None;
    }
    list.len -= 1;
    Some(node)
}

fn append_release_list(
    pool: &mut ReleaseNodePool,
    target: &mut IntrusiveReleaseList,
    source: &mut IntrusiveReleaseList,
) {
    let Some(source_head) = source.head else {
        return;
    };
    if let Some(target_tail) = target.tail {
        pool.nodes[target_tail].next = Some(source_head);
    } else {
        target.head = Some(source_head);
    }
    target.tail = source.tail;
    target.len += source.len;
    *source = IntrusiveReleaseList::default();
}

fn increment_epoch_count(counts: &mut EpochCounts, module_id: u32, epoch: u64) {
    counts.increment(module_id, epoch);
}

fn decrement_epoch_count(counts: &mut EpochCounts, module_id: u32, epoch: u64) {
    counts.decrement(module_id, epoch);
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
    CompletionQueued,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    Detached,
}

#[derive(Debug)]
struct HostRequest {
    module_id: u32,
    epoch: u64,
    state: host_request::State,
    release: Option<ReleaseReservation>,
    completion_reservation: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPayload {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    Bool(bool),
    Rune(u32),
    Opaque(u64),
    Token(ResourceTokenHandle),
    Snapshot(SnapshotHandle),
    Unit,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostValue {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Rune(char),
    String(String),
    Opaque(u64),
    Struct(Vec<HostValue>),
    Request(HostRequestHandle),
    Token(ResourceTokenHandle),
    Snapshot(SnapshotHandle),
    Unit,
}

const MAX_HOST_ARGUMENTS: usize = 8;

#[derive(Clone, Debug)]
pub struct HostArgs<'a> {
    borrowed: Option<&'a [HostValue]>,
    inline: [HostValue; MAX_HOST_ARGUMENTS],
    len: usize,
}

impl<'a> HostArgs<'a> {
    #[must_use]
    pub fn new(values: &'a [HostValue]) -> Self {
        Self {
            borrowed: Some(values),
            inline: std::array::from_fn(|_| HostValue::Unit),
            len: values.len(),
        }
    }

    pub(crate) fn from_runtime(values: &[crate::RuntimeValue]) -> Result<Self, HostTrap> {
        if values.len() > MAX_HOST_ARGUMENTS {
            return Err(HostTrap::Arity);
        }
        let mut inline = std::array::from_fn(|_| HostValue::Unit);
        for (destination, value) in inline.iter_mut().zip(values.iter().copied()) {
            *destination = runtime_argument_to_host_value(value);
        }
        Ok(Self {
            borrowed: None,
            inline,
            len: values.len(),
        })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, index: usize) -> Result<&HostValue, HostTrap> {
        let values = self.borrowed.unwrap_or(&self.inline[..self.len]);
        values.get(index).ok_or(HostTrap::Arity)
    }
}

fn runtime_argument_to_host_value(value: crate::RuntimeValue) -> HostValue {
    match value {
        crate::RuntimeValue::I32(value) => HostValue::I32(value),
        crate::RuntimeValue::I64(value) => HostValue::I64(value),
        crate::RuntimeValue::F32(bits) => HostValue::F32(f32::from_bits(bits)),
        crate::RuntimeValue::F64(bits) => HostValue::F64(f64::from_bits(bits)),
        crate::RuntimeValue::Bool(value) => HostValue::Bool(value),
        crate::RuntimeValue::Rune(value) => {
            HostValue::Rune(char::from_u32(value).expect("verified rune is a Unicode scalar value"))
        }
        crate::RuntimeValue::Ref(reference) | crate::RuntimeValue::NamedRef { reference, .. } => {
            HostValue::Opaque(u64::from(reference.generation) << 32 | u64::from(reference.index))
        }
        crate::RuntimeValue::HostRequest(request) => HostValue::Request(request),
        crate::RuntimeValue::ResourceToken(token) => HostValue::Token(token),
        crate::RuntimeValue::Snapshot(snapshot) => HostValue::Snapshot(snapshot),
        crate::RuntimeValue::Opaque { value, .. } => HostValue::Opaque(value),
        crate::RuntimeValue::StateHandle {
            domain,
            stable_id,
            generation,
            ..
        } => HostValue::Opaque(
            domain ^ stable_id.0.rotate_left(17) ^ u64::from(generation).rotate_left(41),
        ),
        crate::RuntimeValue::Unit => HostValue::Unit,
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
    Host(crate::RuntimeMessage),
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
pub struct HostErrorPayload {
    pub code: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCompletionResult {
    Success(HostPayload),
    Error(HostErrorPayload),
    Cancelled,
    Abandoned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCompletionDelivery {
    pub realm_id: u32,
    pub module_id: u32,
    pub epoch: u64,
    pub request: HostRequestHandle,
    pub result: HostCompletionResult,
    pub terminal_sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HostCompletion {
    realm_id: u32,
    module_id: u32,
    epoch: u64,
    request: HostRequestHandle,
    result: HostCompletionResult,
    reservation: usize,
    terminal_sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletionReservation {
    realm_id: u32,
    module_id: u32,
    epoch: u64,
    request: HostRequestHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionSlot {
    Free,
    Reserved(CompletionReservation),
    Queued(CompletionReservation),
    Consumed(CompletionReservation),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompletionAccounting {
    pub reserved: u64,
    pub queued: u64,
    pub delivered: u64,
    pub cancelled: u64,
    pub abandoned: u64,
    pub reload_discarded: u64,
    pub late_discarded: u64,
}

impl CompletionAccounting {
    #[must_use]
    pub const fn terminal_total(self) -> u64 {
        self.delivered
            .saturating_add(self.cancelled)
            .saturating_add(self.abandoned)
            .saturating_add(self.reload_discarded)
            .saturating_add(self.late_discarded)
    }

    #[must_use]
    pub const fn accounted_total(self) -> u64 {
        self.terminal_total().saturating_add(self.pending())
    }

    #[must_use]
    pub const fn pending(self) -> u64 {
        self.reserved.saturating_sub(self.terminal_total())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionTerminal {
    Delivered,
    Cancelled,
    Abandoned,
    LateDiscarded,
}

#[derive(Debug)]
struct CompletionQueue {
    items: VecDeque<HostCompletion>,
    capacity: usize,
    slots: Vec<CompletionSlot>,
    free: Vec<usize>,
    epoch_counts: EpochCounts,
    closed: bool,
    next_terminal_sequence: u64,
    global_pending: Option<Arc<AtomicUsize>>,
    host_admission: Option<HostAdmissionGate>,
    accounting: CompletionAccounting,
}

#[derive(Clone, Debug)]
pub struct HostCompletionSender {
    queue: Arc<Mutex<CompletionQueue>>,
}

impl HostCompletionSender {
    fn submit(
        &self,
        reservation: usize,
        result: HostCompletionResult,
    ) -> Result<(), HostRequestError> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let metadata = match queue.slots.get(reservation).copied() {
            Some(CompletionSlot::Reserved(metadata)) => metadata,
            Some(
                CompletionSlot::Free | CompletionSlot::Queued(_) | CompletionSlot::Consumed(_),
            )
            | None => {
                return Err(HostRequestError::AlreadyCompleted);
            }
        };
        if queue.closed {
            queue.slots[reservation] = CompletionSlot::Consumed(metadata);
            queue.accounting.late_discarded = queue.accounting.late_discarded.saturating_add(1);
            free_completion_slot(&mut queue, reservation, metadata);
            assert_completion_invariant(&queue);
            return Err(HostRequestError::CompletionQueueClosed);
        }
        debug_assert!(queue.items.len() < queue.capacity);
        let terminal_sequence = queue.next_terminal_sequence;
        let Some(next_terminal_sequence) = terminal_sequence.checked_add(1) else {
            queue.slots[reservation] = CompletionSlot::Consumed(metadata);
            queue.accounting.late_discarded = queue.accounting.late_discarded.saturating_add(1);
            free_completion_slot(&mut queue, reservation, metadata);
            assert_completion_invariant(&queue);
            return Err(HostRequestError::CompletionQueueFull);
        };
        queue.next_terminal_sequence = next_terminal_sequence;
        queue.slots[reservation] = CompletionSlot::Queued(metadata);
        queue.accounting.queued = queue.accounting.queued.saturating_add(1);
        queue.items.push_back(HostCompletion {
            realm_id: metadata.realm_id,
            module_id: metadata.module_id,
            epoch: metadata.epoch,
            request: metadata.request,
            result,
            reservation,
            terminal_sequence,
        });
        assert_completion_invariant(&queue);
        Ok(())
    }
}

fn free_completion_slot(
    queue: &mut CompletionQueue,
    reservation: usize,
    metadata: CompletionReservation,
) {
    queue.slots[reservation] = CompletionSlot::Free;
    queue.free.push(reservation);
    decrement_epoch_count(&mut queue.epoch_counts, metadata.module_id, metadata.epoch);
    if let Some(global_pending) = &queue.global_pending {
        let previous = global_pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "completion reservations are balanced");
    }
}

fn assert_completion_invariant(queue: &CompletionQueue) {
    let (reserved, queued, consumed) =
        queue
            .slots
            .iter()
            .fold((0_u64, 0_u64, 0_u64), |counts, slot| match slot {
                CompletionSlot::Free => counts,
                CompletionSlot::Reserved(_) => (counts.0 + 1, counts.1, counts.2),
                CompletionSlot::Queued(_) => (counts.0, counts.1 + 1, counts.2),
                CompletionSlot::Consumed(_) => (counts.0, counts.1, counts.2 + 1),
            });
    debug_assert_eq!(
        consumed, 0,
        "Consumed is a terminal transition, not storage"
    );
    debug_assert_eq!(queue.accounting.queued, queued);
    debug_assert!(
        queue.accounting.reserved >= queue.accounting.terminal_total(),
        "terminal completions cannot exceed reservations"
    );
    debug_assert_eq!(
        queue.accounting.pending(),
        reserved.saturating_add(queued),
        "pending accounting matches Reserved and Queued slots"
    );
    debug_assert_eq!(
        queue.accounting.reserved,
        queue
            .accounting
            .terminal_total()
            .saturating_add(reserved)
            .saturating_add(queued),
        "every completion reservation has exactly one active or terminal classification"
    );
}

#[derive(Debug)]
pub struct HostCompletionTicket {
    sender: HostCompletionSender,
    reservation: usize,
    consumed: bool,
}

impl HostCompletionTicket {
    pub fn complete(&mut self, payload: HostPayload) -> Result<(), HostRequestError> {
        self.terminate(HostCompletionResult::Success(payload))
    }

    pub fn fail(&mut self, error: HostErrorPayload) -> Result<(), HostRequestError> {
        self.terminate(HostCompletionResult::Error(error))
    }

    pub fn cancelled(&mut self) -> Result<(), HostRequestError> {
        self.terminate(HostCompletionResult::Cancelled)
    }

    pub fn abandon(&mut self) -> Result<(), HostRequestError> {
        self.terminate(HostCompletionResult::Abandoned)
    }

    fn terminate(&mut self, result: HostCompletionResult) -> Result<(), HostRequestError> {
        if self.consumed {
            return Err(HostRequestError::AlreadyCompleted);
        }
        self.consumed = true;
        self.sender.submit(self.reservation, result)
    }
}

impl Drop for HostCompletionTicket {
    fn drop(&mut self) {
        if !self.consumed {
            let _ = self.abandon();
        }
    }
}

#[derive(Debug)]
pub struct PendingHostRequest {
    pub request: HostRequestHandle,
    pub ticket: HostCompletionTicket,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRequestError {
    Handle(HandleError),
    Allocation(SlotAllocError),
    ReleaseQueue(ReleaseQueueError),
    HostClosing,
    HostClosed,
    CompletionQueueFull,
    CompletionQueueClosed,
    UnknownCustomDomain(u32),
    AlreadyCompleted,
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
        match error {
            ReleaseQueueError::HostClosing => Self::HostClosing,
            ReleaseQueueError::HostClosed => Self::HostClosed,
            error => Self::ReleaseQueue(error),
        }
    }
}

#[derive(Debug)]
pub(crate) struct HostRequestManager {
    realm_id: u32,
    requests: SlotPool<HostRequest>,
    epoch_counts: EpochCounts,
    completions: Arc<Mutex<CompletionQueue>>,
    discarded_late_results: u64,
    terminal_records: VecDeque<RequestTerminalRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestTerminalRecord {
    pub request: HostRequestHandle,
    pub state: HostRequestState,
    pub epoch: u64,
    pub terminal_sequence: Option<u64>,
}

impl HostRequestManager {
    #[cfg(test)]
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self::with_completion_counter(realm_id, capacity, None, None)
    }

    fn with_completion_counter(
        realm_id: u32,
        capacity: u32,
        global_pending: Option<Arc<AtomicUsize>>,
        host_admission: Option<HostAdmissionGate>,
    ) -> Self {
        let completions = Arc::new(Mutex::new(CompletionQueue {
            items: VecDeque::with_capacity(capacity as usize),
            capacity: capacity as usize,
            slots: vec![CompletionSlot::Free; capacity as usize],
            free: (0..capacity as usize).rev().collect(),
            epoch_counts: EpochCounts::new(capacity as usize),
            closed: false,
            next_terminal_sequence: 1,
            global_pending,
            host_admission,
            accounting: CompletionAccounting::default(),
        }));
        drop(
            completions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        Self {
            realm_id,
            requests: SlotPool::with_capacity_limit(realm_id, capacity),
            epoch_counts: EpochCounts::new(capacity as usize),
            completions,
            discarded_late_results: 0,
            terminal_records: VecDeque::with_capacity(capacity as usize),
        }
    }

    #[must_use]
    fn completion_sender(&self) -> HostCompletionSender {
        HostCompletionSender {
            queue: Arc::clone(&self.completions),
        }
    }

    #[cfg(test)]
    pub fn create(
        &mut self,
        epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Result<PendingHostRequest, HostRequestError> {
        self.create_for_module(0, epoch, releases)
    }

    pub fn create_for_module(
        &mut self,
        module_id: u32,
        epoch: u64,
        releases: &mut ReleaseQueue,
    ) -> Result<PendingHostRequest, HostRequestError> {
        self.admit(HostAdmissionKind::HostRequest)?;
        let release = releases.reserve(module_id, epoch)?;
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
            completion_reservation: None,
        }) {
            Ok(handle) => {
                let request = HostRequestHandle(handle);
                let completion_reservation =
                    match self.reserve_completion(module_id, epoch, request) {
                        Ok(reservation) => reservation,
                        Err(error) => {
                            self.requests
                                .release(handle)
                                .expect("new request remains allocated");
                            releases.cancel_reservation(release);
                            return Err(error);
                        }
                    };
                self.requests
                    .resolve_mut(handle)
                    .expect("new request resolves")
                    .completion_reservation = Some(completion_reservation);
                increment_epoch_count(&mut self.epoch_counts, module_id, epoch);
                Ok(PendingHostRequest {
                    request,
                    ticket: HostCompletionTicket {
                        sender: self.completion_sender(),
                        reservation: completion_reservation,
                        consumed: false,
                    },
                })
            }
            Err(error) => {
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub fn drain_completions(
        &mut self,
        releases: &mut ReleaseQueue,
    ) -> Vec<HostCompletionDelivery> {
        std::iter::from_fn(|| self.pop_completion(releases)).collect()
    }

    fn pop_completion(&mut self, releases: &mut ReleaseQueue) -> Option<HostCompletionDelivery> {
        loop {
            let completion = self
                .completions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .items
                .pop_front()?;
            let Ok(request) = self.requests.resolve_mut(completion.request.raw()) else {
                self.discarded_late_results += 1;
                self.consume_completion(&completion, CompletionTerminal::LateDiscarded);
                continue;
            };
            if completion.realm_id != self.realm_id
                || request.epoch != completion.epoch
                || completion.module_id != request.module_id
                || request.state != host_request::State::InFlight
            {
                self.discarded_late_results += 1;
                let _ = request;
                self.consume_completion(&completion, CompletionTerminal::LateDiscarded);
                continue;
            }
            let (queue_event, deliver_event, terminal_state, completion_terminal) =
                match &completion.result {
                    HostCompletionResult::Success(_) => (
                        host_request::Event::QueueSuccess,
                        host_request::Event::DeliverSuccess,
                        HostRequestState::Completed,
                        CompletionTerminal::Delivered,
                    ),
                    HostCompletionResult::Error(_) => (
                        host_request::Event::QueueFailure,
                        host_request::Event::DeliverFailure,
                        HostRequestState::Failed,
                        CompletionTerminal::Delivered,
                    ),
                    HostCompletionResult::Cancelled => (
                        host_request::Event::HostCancelled,
                        host_request::Event::DeliverCancelled,
                        HostRequestState::Cancelled,
                        CompletionTerminal::Cancelled,
                    ),
                    HostCompletionResult::Abandoned => (
                        host_request::Event::HostAbandoned,
                        host_request::Event::DeliverAbandoned,
                        HostRequestState::Abandoned,
                        CompletionTerminal::Abandoned,
                    ),
                };
            request.state = host_request::apply(request.state, queue_event, |_| true)
                .expect("generated host request queue transition exists")
                .state;
            request.state = host_request::apply(request.state, deliver_event, |_| true)
                .expect("generated host request delivery transition exists")
                .state;
            request.completion_reservation = None;
            enqueue_request_release(self.realm_id, completion.request, request, releases);
            request.state =
                host_request::apply(request.state, host_request::Event::Release, |_| true)
                    .expect("generated completed request release transition exists")
                    .state;
            let terminal = RequestTerminalRecord {
                request: completion.request,
                state: terminal_state,
                epoch: request.epoch,
                terminal_sequence: Some(completion.terminal_sequence),
            };
            decrement_epoch_count(&mut self.epoch_counts, request.module_id, request.epoch);
            let _ = request;
            self.requests
                .release(completion.request.raw())
                .expect("resolved request remains live");
            self.push_terminal(terminal);
            self.consume_completion(&completion, completion_terminal);
            return Some(HostCompletionDelivery {
                realm_id: completion.realm_id,
                module_id: completion.module_id,
                epoch: completion.epoch,
                request: completion.request,
                result: completion.result,
                terminal_sequence: completion.terminal_sequence,
            });
        }
    }

    fn peek_completion(&self) -> Option<HostCompletionDelivery> {
        let queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.items.iter().find_map(|completion| {
            let request = self.requests.resolve(completion.request.raw()).ok()?;
            (completion.realm_id == self.realm_id
                && request.epoch == completion.epoch
                && completion.module_id == request.module_id
                && request.state == host_request::State::InFlight)
                .then(|| HostCompletionDelivery {
                    realm_id: completion.realm_id,
                    module_id: completion.module_id,
                    epoch: completion.epoch,
                    request: completion.request,
                    result: completion.result.clone(),
                    terminal_sequence: completion.terminal_sequence,
                })
        })
    }

    fn consume_completion(&self, completion: &HostCompletion, terminal: CompletionTerminal) {
        let metadata = CompletionReservation {
            realm_id: completion.realm_id,
            module_id: completion.module_id,
            epoch: completion.epoch,
            request: completion.request,
        };
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert_eq!(
            queue.slots[completion.reservation],
            CompletionSlot::Queued(metadata)
        );
        queue.slots[completion.reservation] = CompletionSlot::Consumed(metadata);
        queue.accounting.queued = queue
            .accounting
            .queued
            .checked_sub(1)
            .expect("consumed completion is queued");
        match terminal {
            CompletionTerminal::Delivered => {
                queue.accounting.delivered = queue.accounting.delivered.saturating_add(1);
            }
            CompletionTerminal::Cancelled => {
                queue.accounting.cancelled = queue.accounting.cancelled.saturating_add(1);
            }
            CompletionTerminal::Abandoned => {
                queue.accounting.abandoned = queue.accounting.abandoned.saturating_add(1);
            }
            CompletionTerminal::LateDiscarded => {
                queue.accounting.late_discarded = queue.accounting.late_discarded.saturating_add(1);
            }
        }
        free_completion_slot(&mut queue, completion.reservation, metadata);
        assert_completion_invariant(&queue);
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
            let reservation = self
                .requests
                .resolve_mut(request.raw())?
                .completion_reservation
                .take();
            if let Some(reservation) = reservation {
                self.cancel_completion_reservation(reservation);
            }
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
            terminal_sequence: None,
        };
        decrement_epoch_count(
            &mut self.epoch_counts,
            request_state.module_id,
            request_state.epoch,
        );
        let _ = request_state;
        self.requests.release(request.raw())?;
        self.push_terminal(terminal);
        Ok(())
    }

    #[must_use]
    pub const fn discarded_late_results(&self) -> u64 {
        self.discarded_late_results
    }

    fn terminal_record(&self, request: HostRequestHandle) -> Option<&RequestTerminalRecord> {
        self.terminal_records
            .iter()
            .find(|record| record.request == request)
    }

    fn completion_counts(&self) -> (usize, usize) {
        let queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue
            .slots
            .iter()
            .fold((0, 0), |(reserved, queued), slot| match slot {
                CompletionSlot::Reserved(_) => (reserved + 1, queued),
                CompletionSlot::Queued(_) => (reserved, queued + 1),
                CompletionSlot::Free | CompletionSlot::Consumed(_) => (reserved, queued),
            })
    }

    fn completion_accounting(&self) -> CompletionAccounting {
        self.completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .accounting
    }

    fn account_reload_discarded(&self, result: &HostCompletionResult) {
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source = match result {
            HostCompletionResult::Success(_) | HostCompletionResult::Error(_) => {
                &mut queue.accounting.delivered
            }
            HostCompletionResult::Cancelled => &mut queue.accounting.cancelled,
            HostCompletionResult::Abandoned => &mut queue.accounting.abandoned,
        };
        *source = source
            .checked_sub(1)
            .expect("reload discard reclassifies one accepted completion");
        queue.accounting.reload_discarded = queue.accounting.reload_discarded.saturating_add(1);
        assert_completion_invariant(&queue);
    }

    fn completion_count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        let queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.epoch_counts.get(module_id, epoch)
    }

    fn request_count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.epoch_counts.get(module_id, epoch)
    }

    fn reserve_completion(
        &self,
        module_id: u32,
        epoch: u64,
        request: HostRequestHandle,
    ) -> Result<usize, HostRequestError> {
        let admission = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .host_admission
            .clone();
        if let Some(admission) = admission {
            admission
                .admit(HostAdmissionKind::CompletionReservation)
                .map_err(host_admission_error)?;
        }
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue.closed {
            return Err(HostRequestError::CompletionQueueClosed);
        }
        let reservation = queue
            .free
            .pop()
            .ok_or(HostRequestError::CompletionQueueFull)?;
        queue.slots[reservation] = CompletionSlot::Reserved(CompletionReservation {
            realm_id: self.realm_id,
            module_id,
            epoch,
            request,
        });
        queue.accounting.reserved = queue.accounting.reserved.saturating_add(1);
        increment_epoch_count(&mut queue.epoch_counts, module_id, epoch);
        if let Some(global_pending) = &queue.global_pending {
            global_pending.fetch_add(1, Ordering::AcqRel);
        }
        assert_completion_invariant(&queue);
        Ok(reservation)
    }

    fn admit(&self, kind: HostAdmissionKind) -> Result<(), HostRequestError> {
        let admission = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .host_admission
            .clone();
        admission.map_or(Ok(()), |admission| {
            admission.admit(kind).map_err(host_admission_error)
        })
    }

    fn cancel_completion_reservation(&self, reservation: usize) {
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(CompletionSlot::Reserved(metadata)) = queue.slots.get(reservation).copied() {
            queue.slots[reservation] = CompletionSlot::Consumed(metadata);
            queue.accounting.cancelled = queue.accounting.cancelled.saturating_add(1);
            free_completion_slot(&mut queue, reservation, metadata);
            assert_completion_invariant(&queue);
        }
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
        let mut queue = self
            .completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.closed = true;
        while let Some(completion) = queue.items.pop_front() {
            let metadata = CompletionReservation {
                realm_id: completion.realm_id,
                module_id: completion.module_id,
                epoch: completion.epoch,
                request: completion.request,
            };
            debug_assert_eq!(
                queue.slots[completion.reservation],
                CompletionSlot::Queued(metadata)
            );
            queue.slots[completion.reservation] = CompletionSlot::Consumed(metadata);
            queue.accounting.queued = queue
                .accounting
                .queued
                .checked_sub(1)
                .expect("queued completion is accounted during Realm drop");
            queue.accounting.late_discarded = queue.accounting.late_discarded.saturating_add(1);
            free_completion_slot(&mut queue, completion.reservation, metadata);
        }
        assert_completion_invariant(&queue);
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
                    module_id: request.module_id,
                    epoch: request.epoch,
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
    module_id: u32,
    epoch: u64,
    domain: RuntimeHostDomain,
    state: resource_token::State,
    release: Option<ReleaseReservation>,
}

#[derive(Debug)]
pub(crate) struct ResourceTokenManager {
    realm_id: u32,
    tokens: SlotPool<ResourceToken>,
    epoch_counts: EpochCounts,
    terminal: VecDeque<RawHandle>,
    terminal_capacity: usize,
}

impl ResourceTokenManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            tokens: SlotPool::with_capacity_limit(realm_id, capacity),
            epoch_counts: EpochCounts::new(capacity as usize),
            terminal: VecDeque::with_capacity(capacity as usize),
            terminal_capacity: capacity as usize,
        }
    }

    pub fn create(
        &mut self,
        _owner: TaskHandle,
        module_id: u32,
        epoch: u64,
        domain: RuntimeHostDomain,
        releases: &mut ReleaseQueue,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        let release = releases.reserve(module_id, epoch)?;
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
            module_id,
            epoch,
            domain,
            state: published.state,
            release: Some(release),
        }) {
            Ok(handle) => {
                increment_epoch_count(&mut self.epoch_counts, module_id, epoch);
                Ok(ResourceTokenHandle(handle))
            }
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
                module_id: token.module_id,
                epoch: token.epoch,
                kind: ReleaseKind::ResourceToken,
                object_id: u64::from(handle.0.generation) << 32 | u64::from(handle.0.index),
                domain,
            },
        )?;
        token.state =
            resource_token::apply(token.state, resource_token::Event::HostRelease, |_| true)
                .expect("generated resource release transition exists")
                .state;
        decrement_epoch_count(&mut self.epoch_counts, token.module_id, token.epoch);
        let _ = token;
        self.tokens.release(handle.0)?;
        if self.terminal.len() == self.terminal_capacity {
            self.terminal.pop_front();
        }
        self.terminal.push_back(handle.0);
        Ok(true)
    }

    fn count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.epoch_counts.get(module_id, epoch)
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
    module_id: u32,
    epoch: u64,
    data: Arc<[i32]>,
    external_bytes: usize,
    release: ReleaseReservation,
}

#[derive(Debug)]
pub(crate) struct SnapshotManager {
    realm_id: u32,
    snapshots: SlotPool<SnapshotEntry>,
    epoch_counts: EpochCounts,
}

impl SnapshotManager {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32) -> Self {
        Self {
            realm_id,
            snapshots: SlotPool::with_capacity_limit(realm_id, capacity),
            epoch_counts: EpochCounts::new(capacity as usize),
        }
    }

    fn create(
        &mut self,
        _owner: TaskHandle,
        module_id: u32,
        epoch: u64,
        data: Arc<[i32]>,
        releases: &mut ReleaseQueue,
    ) -> Result<SnapshotHandle, HostRequestError> {
        let release = releases.reserve(module_id, epoch)?;
        let external_bytes = data.len().saturating_mul(std::mem::size_of::<i32>());
        match self.snapshots.try_allocate(SnapshotEntry {
            module_id,
            epoch,
            data,
            external_bytes,
            release,
        }) {
            Ok(raw) => {
                increment_epoch_count(&mut self.epoch_counts, module_id, epoch);
                Ok(SnapshotHandle(raw))
            }
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
                module_id: snapshot.module_id,
                epoch: snapshot.epoch,
                kind: ReleaseKind::Snapshot,
                object_id: u64::from(handle.raw().generation) << 32 | u64::from(handle.raw().index),
                domain: RuntimeHostDomain::VmThread,
            },
        )?;
        decrement_epoch_count(&mut self.epoch_counts, snapshot.module_id, snapshot.epoch);
        Ok(())
    }

    fn count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.epoch_counts.get(module_id, epoch)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskResourceSet {
    pub requests: BTreeSet<HostRequestHandle>,
    pub tokens: BTreeSet<ResourceTokenHandle>,
    pub snapshots: BTreeSet<SnapshotHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedResource {
    Request {
        task: TaskHandle,
        handle: HostRequestHandle,
    },
    Token {
        task: TaskHandle,
        handle: ResourceTokenHandle,
    },
    Snapshot {
        task: TaskHandle,
        handle: SnapshotHandle,
    },
}

impl OwnedResource {
    const fn task(self) -> TaskHandle {
        match self {
            Self::Request { task, .. } | Self::Token { task, .. } | Self::Snapshot { task, .. } => {
                task
            }
        }
    }
}

#[derive(Debug)]
pub struct RuntimeResources {
    requests: HostRequestManager,
    tokens: ResourceTokenManager,
    snapshots: SnapshotManager,
    releases: ReleaseQueue,
    ownership: Vec<OwnedResource>,
    host_admission: Option<HostAdmissionGate>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeResourceSnapshot {
    pub requests: usize,
    pub tokens: usize,
    pub snapshots: usize,
    pub release_records: usize,
    pub release_reservations: usize,
    pub completion_reservations: usize,
    pub completion_queued: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EpochResourceCounts {
    pub requests: usize,
    pub tokens: usize,
    pub snapshots: usize,
    pub pending_releases: usize,
    pub pending_completions: usize,
}

impl RuntimeResources {
    #[must_use]
    pub fn new(realm_id: u32, capacity: u32, release_capacity: usize) -> Self {
        Self::with_release_queue(
            realm_id,
            capacity,
            ReleaseQueue::new(release_capacity),
            None,
            None,
            None,
        )
    }

    pub(crate) fn with_runtime_host(
        realm_id: u32,
        capacity: u32,
        release_capacity: usize,
        runtime_host: &RuntimeHost,
    ) -> Self {
        Self::with_release_queue(
            realm_id,
            capacity,
            runtime_host.release_queue(release_capacity),
            Some(Arc::clone(&runtime_host.pending_completions)),
            Some(runtime_host.admission.clone()),
            Some(runtime_host.admission.clone()),
        )
    }

    fn with_release_queue(
        realm_id: u32,
        capacity: u32,
        releases: ReleaseQueue,
        global_pending: Option<Arc<AtomicUsize>>,
        host_admission: Option<HostAdmissionGate>,
        resource_admission: Option<HostAdmissionGate>,
    ) -> Self {
        Self {
            requests: HostRequestManager::with_completion_counter(
                realm_id,
                capacity,
                global_pending,
                host_admission,
            ),
            tokens: ResourceTokenManager::new(realm_id, capacity),
            snapshots: SnapshotManager::new(realm_id, capacity),
            releases,
            ownership: Vec::with_capacity(
                (capacity as usize)
                    .checked_mul(3)
                    .expect("host resource ownership capacity overflow"),
            ),
            host_admission: resource_admission,
        }
    }

    pub fn context(&mut self, task: TaskHandle, module_id: u32, epoch: u64) -> ResourceContext<'_> {
        ResourceContext {
            task,
            module_id,
            epoch,
            resources: self,
        }
    }

    pub fn drain_completions(&mut self) -> Vec<HostCompletionDelivery> {
        std::iter::from_fn(|| self.pop_completion()).collect()
    }

    pub(crate) fn pop_completion(&mut self) -> Option<HostCompletionDelivery> {
        let delivery = self.requests.pop_completion(&mut self.releases)?;
        self.ownership.retain(|owned| {
            !matches!(
                owned,
                OwnedResource::Request { handle, .. } if *handle == delivery.request
            )
        });
        Some(delivery)
    }

    pub(crate) fn peek_completion(&self) -> Option<HostCompletionDelivery> {
        self.requests.peek_completion()
    }

    pub fn cleanup_task(
        &mut self,
        task: TaskHandle,
        detach_requests: bool,
    ) -> Result<(), HostRequestError> {
        while let Some(index) = self.ownership.iter().position(|owned| owned.task() == task) {
            match self.ownership.swap_remove(index) {
                OwnedResource::Request { handle, .. } => {
                    match self
                        .requests
                        .cancel(handle, detach_requests, &mut self.releases)
                    {
                        Ok(()) | Err(HostRequestError::Handle(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
                OwnedResource::Token { handle, .. } => {
                    self.tokens.release(handle, &mut self.releases)?;
                }
                OwnedResource::Snapshot { handle, .. } => {
                    self.snapshots.release(handle, &mut self.releases)?;
                }
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "model-adapter"))]
    pub(crate) fn detach_request_for_model(
        &mut self,
        task: TaskHandle,
        handle: HostRequestHandle,
    ) -> Result<(), HostRequestError> {
        let index = self
            .ownership
            .iter()
            .position(|owned| {
                matches!(
                    owned,
                    OwnedResource::Request {
                        task: owner,
                        handle: owned_handle,
                    } if *owner == task && *owned_handle == handle
                )
            })
            .ok_or(HostRequestError::InvalidState)?;
        self.ownership.swap_remove(index);
        self.requests.cancel(handle, true, &mut self.releases)
    }

    #[cfg(any(test, feature = "model-adapter"))]
    pub(crate) fn release_token_for_model(
        &mut self,
        task: TaskHandle,
        handle: ResourceTokenHandle,
    ) -> Result<(), HostRequestError> {
        let index = self
            .ownership
            .iter()
            .position(|owned| {
                matches!(
                    owned,
                    OwnedResource::Token {
                        task: owner,
                        handle: owned_handle,
                    } if *owner == task && *owned_handle == handle
                )
            })
            .ok_or(HostRequestError::InvalidState)?;
        self.ownership.swap_remove(index);
        self.tokens.release(handle, &mut self.releases).map(|_| ())
    }

    #[cfg(any(test, feature = "model-adapter"))]
    pub(crate) fn release_snapshot_for_model(
        &mut self,
        task: TaskHandle,
        handle: SnapshotHandle,
    ) -> Result<(), HostRequestError> {
        let index = self
            .ownership
            .iter()
            .position(|owned| {
                matches!(
                    owned,
                    OwnedResource::Snapshot {
                        task: owner,
                        handle: owned_handle,
                    } if *owner == task && *owned_handle == handle
                )
            })
            .ok_or(HostRequestError::InvalidState)?;
        self.ownership.swap_remove(index);
        self.snapshots.release(handle, &mut self.releases)
    }

    pub fn drain_releases(&mut self) -> Vec<ReleaseRecord> {
        self.releases.drain().collect()
    }

    pub fn transfer_releases_to_host(&mut self) -> usize {
        self.releases.transfer_to_host()
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
    pub fn request_terminal_record(
        &self,
        request: HostRequestHandle,
    ) -> Option<&RequestTerminalRecord> {
        self.requests.terminal_record(request)
    }

    #[must_use]
    pub fn ownership(&self, task: TaskHandle) -> Option<TaskResourceSet> {
        let mut snapshot = TaskResourceSet::default();
        for owned in self
            .ownership
            .iter()
            .copied()
            .filter(|owned| owned.task() == task)
        {
            match owned {
                OwnedResource::Request { handle, .. } => {
                    snapshot.requests.insert(handle);
                }
                OwnedResource::Token { handle, .. } => {
                    snapshot.tokens.insert(handle);
                }
                OwnedResource::Snapshot { handle, .. } => {
                    snapshot.snapshots.insert(handle);
                }
            }
        }
        (!snapshot.requests.is_empty()
            || !snapshot.tokens.is_empty()
            || !snapshot.snapshots.is_empty())
        .then_some(snapshot)
    }

    #[must_use]
    pub fn owns_request(&self, task: TaskHandle, request: HostRequestHandle) -> bool {
        self.ownership.iter().any(|owned| {
            matches!(
                owned,
                OwnedResource::Request {
                    task: owner,
                    handle,
                } if *owner == task && *handle == request
            )
        })
    }

    #[must_use]
    pub(crate) fn request_count_for_task(&self, task: TaskHandle) -> usize {
        self.ownership
            .iter()
            .filter(|owned| {
                matches!(owned, OwnedResource::Request { task: owner, .. } if *owner == task)
            })
            .count()
    }

    #[must_use]
    pub const fn discarded_late_results(&self) -> u64 {
        self.requests.discarded_late_results()
    }

    #[must_use]
    pub fn completion_accounting(&self) -> CompletionAccounting {
        self.requests.completion_accounting()
    }

    pub(crate) fn account_reload_discarded(&self, delivery: &HostCompletionDelivery) {
        self.requests.account_reload_discarded(&delivery.result);
    }

    #[must_use]
    pub(crate) fn reserved_capacities(&self) -> (usize, usize) {
        (
            self.requests.requests.reserved_capacity(),
            self.releases.capacity,
        )
    }

    #[must_use]
    pub fn model_snapshot(&self) -> RuntimeResourceSnapshot {
        let (completion_reservations, completion_queued) = self.requests.completion_counts();
        RuntimeResourceSnapshot {
            requests: self.requests.requests.occupied_len(),
            tokens: self.tokens.tokens.occupied_len(),
            snapshots: self.snapshots.snapshots.occupied_len(),
            release_records: self.releases.queued_len(),
            release_reservations: self.releases.reserved,
            completion_reservations,
            completion_queued,
        }
    }

    #[must_use]
    pub fn resource_ledger(&self) -> crate::RuntimeResourceLedger {
        let snapshot = self.model_snapshot();
        crate::RuntimeResourceLedger {
            requests: crate::ledger::count(snapshot.requests),
            completion_reservations: crate::ledger::count(
                snapshot
                    .completion_reservations
                    .saturating_add(snapshot.completion_queued),
            ),
            tokens: crate::ledger::count(snapshot.tokens),
            snapshots: crate::ledger::count(snapshot.snapshots),
            release_reservations: crate::ledger::count(snapshot.release_reservations),
            queued_releases: crate::ledger::count(snapshot.release_records),
            ..crate::RuntimeResourceLedger::default()
        }
    }

    pub(crate) fn completion_count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.requests.completion_count_for_epoch(module_id, epoch)
    }

    pub(crate) fn epoch_counts(&self, module_id: u32, epoch: u64) -> EpochResourceCounts {
        EpochResourceCounts {
            requests: self.requests.request_count_for_epoch(module_id, epoch),
            tokens: self.tokens.count_for_epoch(module_id, epoch),
            snapshots: self.snapshots.count_for_epoch(module_id, epoch),
            pending_releases: self.releases.record_count_for_epoch(module_id, epoch),
            pending_completions: self.completion_count_for_epoch(module_id, epoch),
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
    pub fn create_request(&mut self) -> Result<PendingHostRequest, HostRequestError> {
        self.admit(HostAdmissionKind::AsyncHostCall)?;
        let pending = self.resources.requests.create_for_module(
            self.module_id,
            self.epoch,
            &mut self.resources.releases,
        )?;
        debug_assert!(self.resources.ownership.len() < self.resources.ownership.capacity());
        self.resources.ownership.push(OwnedResource::Request {
            task: self.task,
            handle: pending.request,
        });
        Ok(pending)
    }

    pub fn create_token(
        &mut self,
        domain: RuntimeHostDomain,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        self.admit(HostAdmissionKind::ResourceToken)?;
        let token = self.resources.tokens.create(
            self.task,
            self.module_id,
            self.epoch,
            domain,
            &mut self.resources.releases,
        )?;
        debug_assert!(self.resources.ownership.len() < self.resources.ownership.capacity());
        self.resources.ownership.push(OwnedResource::Token {
            task: self.task,
            handle: token,
        });
        Ok(token)
    }

    pub fn create_snapshot(
        &mut self,
        data: Arc<[i32]>,
    ) -> Result<SnapshotHandle, HostRequestError> {
        self.admit(HostAdmissionKind::Snapshot)?;
        let snapshot = self.resources.snapshots.create(
            self.task,
            self.module_id,
            self.epoch,
            data,
            &mut self.resources.releases,
        )?;
        debug_assert!(self.resources.ownership.len() < self.resources.ownership.capacity());
        self.resources.ownership.push(OwnedResource::Snapshot {
            task: self.task,
            handle: snapshot,
        });
        Ok(snapshot)
    }

    fn admit(&self, kind: HostAdmissionKind) -> Result<(), HostRequestError> {
        self.resources
            .host_admission
            .as_ref()
            .map_or(Ok(()), |gate| {
                gate.admit(kind).map_err(host_admission_error)
            })
    }
}

fn host_admission_error(error: HostAdmissionError) -> HostRequestError {
    match error {
        HostAdmissionError::Closing => HostRequestError::HostClosing,
        HostAdmissionError::Closed => HostRequestError::HostClosed,
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

#[cfg(feature = "fuzzing")]
pub fn fuzz_completion_ticket_terminal_race(data: &[u8]) {
    if data.len() > 64 {
        return;
    }
    let mut releases = ReleaseQueue::new(1);
    let mut requests = HostRequestManager::with_completion_counter(5, 1, None, None);
    let Ok(pending) = requests.create_for_module(2, 3, &mut releases) else {
        return;
    };
    let request = pending.request;
    let mut ticket = pending.ticket;
    let requests = Arc::new(Mutex::new(requests));
    let releases = Arc::new(Mutex::new(releases));
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let operation = data.first().copied().unwrap_or_default() % 4;

    let ticket_barrier = Arc::clone(&barrier);
    let terminal = std::thread::spawn(move || {
        ticket_barrier.wait();
        match operation {
            0 => ticket.complete(HostPayload::I32(1)),
            1 => ticket.fail(HostErrorPayload { code: 7 }),
            2 => ticket.cancelled(),
            _ => ticket.abandon(),
        }
    });
    let cancel_requests = Arc::clone(&requests);
    let cancel_releases = Arc::clone(&releases);
    let cancel_barrier = Arc::clone(&barrier);
    let cancel = std::thread::spawn(move || {
        cancel_barrier.wait();
        cancel_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel(
                request,
                false,
                &mut cancel_releases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
    });
    barrier.wait();
    let _ = terminal.join();
    let _ = cancel.join();
    let mut requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut releases = releases
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = requests.drain_completions(&mut releases);
    let _ = releases.drain().count();
}

#[cfg(feature = "fuzzing")]
pub fn fuzz_release_intrusive_list(data: &[u8]) {
    if data.len() > 128 {
        return;
    }
    const CAPACITY: usize = 16;
    let host = RuntimeHost::new(CAPACITY);
    let mut queue = host.release_queue(CAPACITY);
    let mut reservations = Vec::with_capacity(CAPACITY);
    for (index, byte) in data.iter().copied().enumerate().take(64) {
        match byte % 5 {
            0 if reservations.len() < CAPACITY => {
                if let Ok(reservation) = queue.reserve(u32::from(byte % 4), u64::from(byte % 8)) {
                    reservations.push(reservation);
                }
            }
            1 => {
                if let Some(reservation) = reservations.pop() {
                    let domain = RuntimeHostDomain::ALL[usize::from(byte) % RELEASE_DOMAIN_COUNT];
                    let _ = queue.enqueue_reserved(
                        reservation,
                        ReleaseRecord {
                            realm_id: 1,
                            module_id: reservation.module_id,
                            epoch: reservation.epoch,
                            kind: ReleaseKind::ResourceToken,
                            object_id: index as u64,
                            domain,
                        },
                    );
                }
            }
            2 => {
                if let Some(reservation) = reservations.pop() {
                    queue.cancel_reservation(reservation);
                }
            }
            3 => {
                let mut records = [ReleaseRecord {
                    realm_id: 0,
                    module_id: 0,
                    epoch: 0,
                    kind: ReleaseKind::HostRequest,
                    object_id: 0,
                    domain: RuntimeHostDomain::VmThread,
                }; 4];
                let _ = queue.drain_into(&mut records);
            }
            _ => {
                let _ = queue.transfer_to_host();
                let _ = host.drain_releases();
            }
        }
    }
    for reservation in reservations {
        queue.cancel_reservation(reservation);
    }
    let _ = queue.drain().count();
    let _ = queue.transfer_to_host();
    let _ = host.drain_releases();
    let _ = host.begin_close();
    let _ = host.try_finish_close();
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use nexa_core::RawHandle;

    use super::{
        HostAdmissionError, HostAdmissionKind, HostErrorPayload, HostPayload, HostRequestError,
        HostRequestHandle, HostRequestManager, ReleaseKind, ReleaseQueue, ReleaseQueueError,
        ReleaseQueueState, ResourceTokenManager, RuntimeHost, RuntimeHostCloseError,
        RuntimeHostDomain, RuntimeHostState, SnapshotManager,
    };
    use crate::{RuntimeLimits, TaskRuntime};

    #[test]
    fn late_completion_is_discarded_and_release_is_pre_reserved() {
        let mut releases = ReleaseQueue::new(1);
        let mut requests = HostRequestManager::new(3, 1);
        let mut pending = requests.create(7, &mut releases).unwrap();
        let request = pending.request;
        requests.cancel(request, true, &mut releases).unwrap();
        pending.ticket.complete(HostPayload::I32(9)).unwrap();
        assert!(requests.drain_completions(&mut releases).is_empty());
        assert_eq!(requests.discarded_late_results(), 1);
        assert_eq!(
            requests.completion_accounting(),
            super::CompletionAccounting {
                reserved: 1,
                late_discarded: 1,
                ..super::CompletionAccounting::default()
            }
        );
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
            .create(task, 0, 1, Arc::<[i32]>::from([1, 2, 3]), &mut releases)
            .unwrap();
        assert_eq!(snapshots.data(snapshot).unwrap(), &[1, 2, 3]);
        assert_eq!(snapshots.external_bytes(snapshot).unwrap(), 12);
        assert!(releases.reserve(0, 1).is_err());
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
            .create(task, 0, 1, RuntimeHostDomain::Render, &mut releases)
            .unwrap();
        assert_eq!(resources.release(token, &mut releases), Ok(true));
        assert_eq!(resources.release(token, &mut releases), Ok(false));
        releases.reparent_realm(9, 99);
        let record = releases.drain().next().unwrap();
        assert_eq!(record.realm_id, 99);
        assert_eq!(record.kind, ReleaseKind::ResourceToken);
    }

    #[test]
    fn completion_tickets_are_single_use_send_and_request_slots_recycle_under_threads() {
        const COUNT: u32 = 32;

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<super::HostCompletionTicket>();

        let mut releases = ReleaseQueue::new(COUNT as usize);
        let mut requests = HostRequestManager::new(7, COUNT);
        let pending = (0..COUNT)
            .map(|_| requests.create_for_module(3, 9, &mut releases).unwrap())
            .collect::<Vec<_>>();
        let handles = pending
            .iter()
            .map(|pending| pending.request)
            .collect::<Vec<_>>();
        let workers = pending
            .into_iter()
            .map(|mut pending| {
                thread::spawn(move || {
                    pending.ticket.complete(HostPayload::I32(1)).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(
            requests.drain_completions(&mut releases).len(),
            COUNT as usize
        );
        assert_eq!(releases.drain().count(), COUNT as usize);
        let mut pending = requests.create_for_module(3, 9, &mut releases).unwrap();
        pending.ticket.fail(HostErrorPayload { code: 7 }).unwrap();
        assert_eq!(
            pending.ticket.cancelled(),
            Err(HostRequestError::AlreadyCompleted)
        );
        assert_eq!(requests.drain_completions(&mut releases).len(), 1);
        let mut pending = requests.create_for_module(3, 9, &mut releases).unwrap();
        drop(requests);
        assert_eq!(
            pending.ticket.complete(HostPayload::Unit),
            Err(HostRequestError::CompletionQueueClosed)
        );
        assert_eq!(handles.len(), COUNT as usize);
    }

    #[test]
    fn completion_success_error_cancel_abandon_and_drop_consume_exactly_one_reservation() {
        let mut releases = ReleaseQueue::new(5);
        let mut requests = HostRequestManager::new(11, 5);
        let mut success = requests.create_for_module(4, 8, &mut releases).unwrap();
        let mut failure = requests.create_for_module(4, 8, &mut releases).unwrap();
        let mut cancelled = requests.create_for_module(4, 8, &mut releases).unwrap();
        let mut abandoned = requests.create_for_module(4, 8, &mut releases).unwrap();
        let dropped = requests.create_for_module(4, 8, &mut releases).unwrap();
        assert_eq!(requests.completion_counts(), (5, 0));
        assert_eq!(requests.completion_accounting().reserved, 5);

        success.ticket.complete(HostPayload::I32(1)).unwrap();
        failure.ticket.fail(HostErrorPayload { code: 42 }).unwrap();
        cancelled.ticket.cancelled().unwrap();
        abandoned.ticket.abandon().unwrap();
        drop(dropped);
        assert_eq!(requests.completion_counts(), (0, 5));
        assert_eq!(requests.completion_accounting().queued, 5);

        let deliveries = requests.drain_completions(&mut releases);
        assert_eq!(deliveries.len(), 5);
        assert!(matches!(
            deliveries[0].result,
            super::HostCompletionResult::Success(HostPayload::I32(1))
        ));
        assert!(matches!(
            deliveries[1].result,
            super::HostCompletionResult::Error(_)
        ));
        assert!(matches!(
            deliveries[2].result,
            super::HostCompletionResult::Cancelled
        ));
        assert!(matches!(
            deliveries[3].result,
            super::HostCompletionResult::Abandoned
        ));
        assert!(matches!(
            deliveries[4].result,
            super::HostCompletionResult::Abandoned
        ));
        assert_eq!(requests.completion_counts(), (0, 0));
        assert_eq!(
            requests.completion_accounting(),
            super::CompletionAccounting {
                reserved: 5,
                delivered: 2,
                cancelled: 1,
                abandoned: 2,
                ..super::CompletionAccounting::default()
            }
        );
        assert_eq!(releases.drain().count(), 5);
    }

    #[test]
    fn completion_and_request_cancel_race_has_one_terminal_classification() {
        let mut releases = ReleaseQueue::new(1);
        let mut requests = HostRequestManager::new(5, 1);
        let pending = requests.create_for_module(2, 3, &mut releases).unwrap();
        let request = pending.request;
        let mut ticket = pending.ticket;
        let requests = Arc::new(Mutex::new(requests));
        let releases = Arc::new(Mutex::new(releases));
        let barrier = Arc::new(Barrier::new(3));

        let complete_barrier = Arc::clone(&barrier);
        let complete = thread::spawn(move || {
            complete_barrier.wait();
            ticket.complete(HostPayload::Unit)
        });
        let cancel_requests = Arc::clone(&requests);
        let cancel_releases = Arc::clone(&releases);
        let cancel_barrier = Arc::clone(&barrier);
        let cancel = thread::spawn(move || {
            cancel_barrier.wait();
            cancel_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel(
                    request,
                    false,
                    &mut cancel_releases
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                )
        });
        barrier.wait();
        let completion_result = complete.join().unwrap();
        cancel.join().unwrap().unwrap();
        assert!(matches!(
            completion_result,
            Ok(()) | Err(HostRequestError::AlreadyCompleted)
        ));

        let mut requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut releases = releases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(requests.drain_completions(&mut releases).is_empty());
        let accounting = requests.completion_accounting();
        assert_eq!(accounting.reserved, 1);
        assert_eq!(accounting.queued, 0);
        assert_eq!(accounting.terminal_total(), 1);
        assert_eq!(accounting.pending(), 0);
        assert_eq!(accounting.cancelled + accounting.late_discarded, 1);
        assert_eq!(accounting.accounted_total(), accounting.reserved);
    }

    #[test]
    fn completion_terminal_operations_are_pairwise_exactly_once() {
        #[derive(Clone, Copy)]
        enum Operation {
            Complete,
            Cancel,
            Abandon,
            Drop,
        }

        fn terminate(mut ticket: super::HostCompletionTicket, operation: Operation) {
            match operation {
                Operation::Complete => {
                    let _ = ticket.complete(HostPayload::Unit);
                }
                Operation::Cancel => {
                    let _ = ticket.cancelled();
                }
                Operation::Abandon => {
                    let _ = ticket.abandon();
                }
                Operation::Drop => drop(ticket),
            }
        }

        for (left, right) in [
            (Operation::Complete, Operation::Drop),
            (Operation::Complete, Operation::Cancel),
            (Operation::Complete, Operation::Abandon),
            (Operation::Cancel, Operation::Abandon),
        ] {
            let mut releases = ReleaseQueue::new(1);
            let mut requests = HostRequestManager::new(6, 1);
            let pending = requests.create_for_module(2, 3, &mut releases).unwrap();
            let duplicate = super::HostCompletionTicket {
                sender: pending.ticket.sender.clone(),
                reservation: pending.ticket.reservation,
                consumed: false,
            };
            let barrier = Arc::new(Barrier::new(3));
            let left_barrier = Arc::clone(&barrier);
            let left_worker = thread::spawn(move || {
                left_barrier.wait();
                terminate(pending.ticket, left);
            });
            let right_barrier = Arc::clone(&barrier);
            let right_worker = thread::spawn(move || {
                right_barrier.wait();
                terminate(duplicate, right);
            });
            barrier.wait();
            left_worker.join().unwrap();
            right_worker.join().unwrap();

            assert_eq!(requests.drain_completions(&mut releases).len(), 1);
            let accounting = requests.completion_accounting();
            assert_eq!(accounting.reserved, 1);
            assert_eq!(accounting.pending(), 0);
            assert_eq!(accounting.terminal_total(), 1);
        }
    }

    #[test]
    fn realm_drop_and_completion_race_consumes_the_reservation() {
        let mut releases = ReleaseQueue::new(1);
        let mut requests = HostRequestManager::new(8, 1);
        let pending = requests.create_for_module(2, 3, &mut releases).unwrap();
        let observer = pending.ticket.sender.clone();
        let mut ticket = pending.ticket;
        let barrier = Arc::new(Barrier::new(3));
        let drop_barrier = Arc::clone(&barrier);
        let drop_realm = thread::spawn(move || {
            drop_barrier.wait();
            drop(requests);
        });
        let complete_barrier = Arc::clone(&barrier);
        let complete = thread::spawn(move || {
            complete_barrier.wait();
            ticket.complete(HostPayload::Unit)
        });
        barrier.wait();
        drop_realm.join().unwrap();
        let result = complete.join().unwrap();
        assert!(matches!(
            result,
            Ok(()) | Err(HostRequestError::CompletionQueueClosed)
        ));
        let queue = observer
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            queue.accounting,
            super::CompletionAccounting {
                reserved: 1,
                late_discarded: 1,
                ..super::CompletionAccounting::default()
            }
        );
        assert!(matches!(queue.slots[0], super::CompletionSlot::Free));
    }

    #[test]
    fn release_node_pool_transfers_intrusive_domain_lists_without_losing_capacity() {
        let host = RuntimeHost::new(3);
        let mut releases = host.release_queue(3);
        for (domain, object_id) in [
            (RuntimeHostDomain::Render, 1),
            (RuntimeHostDomain::Render, 2),
            (RuntimeHostDomain::Io, 3),
        ] {
            let reservation = releases.reserve(3, 5).unwrap();
            releases
                .enqueue_reserved(
                    reservation,
                    super::ReleaseRecord {
                        realm_id: 9,
                        module_id: 3,
                        epoch: 5,
                        kind: ReleaseKind::ResourceToken,
                        object_id,
                        domain,
                    },
                )
                .unwrap();
        }
        assert_eq!(releases.transfer_to_host(), 3);
        assert_eq!(host.pending_releases(), 3);
        assert_eq!(host.drain(RuntimeHostDomain::Render, 1).len(), 1);
        assert_eq!(host.pending_releases(), 2);
        assert_eq!(host.drain_releases().len(), 2);
        assert_eq!(host.pending_releases(), 0);
        assert!(releases.reserve(3, 6).is_ok());
    }

    #[test]
    fn begin_close_and_realm_registration_are_linearized() {
        let host = RuntimeHost::new(1);
        let barrier = Arc::new(Barrier::new(3));
        let register_host = host.clone();
        let register_barrier = Arc::clone(&barrier);
        let register = thread::spawn(move || {
            register_barrier.wait();
            let result = register_host.register_realm();
            if result.is_ok() {
                register_host.unregister_realm();
            }
            result
        });
        let close_host = host.clone();
        let close_barrier = Arc::clone(&barrier);
        let close = thread::spawn(move || {
            close_barrier.wait();
            close_host.begin_close()
        });
        barrier.wait();
        let admission = register.join().unwrap();
        let status = close.join().unwrap();

        assert!(matches!(admission, Ok(()) | Err(RuntimeHostState::Closing)));
        assert_eq!(status.state, RuntimeHostState::Closing);
        assert_eq!(host.close_status().live_realms, 0);
        assert_eq!(
            host.try_finish_close().unwrap().state,
            RuntimeHostState::Closed
        );
    }

    #[test]
    fn begin_close_races_request_and_completion_reservations_without_leaks() {
        let host = RuntimeHost::new(2);
        let barrier = Arc::new(Barrier::new(2));
        let request_host = host.clone();
        let request_barrier = Arc::clone(&barrier);
        let request = thread::spawn(move || {
            let mut requests = HostRequestManager::with_completion_counter(
                1,
                1,
                Some(Arc::clone(&request_host.pending_completions)),
                Some(request_host.admission.clone()),
            );
            let mut releases = request_host.release_queue(1);
            request_barrier.wait();
            let outcome = requests.create_for_module(2, 3, &mut releases);
            let admitted = outcome.is_ok();
            if let Ok(pending) = outcome {
                drop(pending);
                requests.drain_completions(&mut releases);
                releases.transfer_to_host();
            }
            admitted
        });
        barrier.wait();
        let _ = host.begin_close();
        let _admitted_before_close = request.join().unwrap();
        let _ = host.drain_releases();
        assert_eq!(host.pending_completions(), 0);

        let completion_host = RuntimeHost::new(1);
        let requests = HostRequestManager::with_completion_counter(
            1,
            1,
            Some(Arc::clone(&completion_host.pending_completions)),
            Some(completion_host.admission.clone()),
        );
        let _ = completion_host.begin_close();
        assert_eq!(
            requests.reserve_completion(2, 3, HostRequestHandle(RawHandle::new(1, 0, 0))),
            Err(HostRequestError::HostClosing)
        );
        completion_host.try_finish_close().unwrap();

        host.try_finish_close().unwrap();
    }

    #[test]
    fn begin_close_races_release_reservation_and_rejects_all_new_resources() {
        let host = RuntimeHost::new(3);
        let barrier = Arc::new(Barrier::new(2));
        let reserve_host = host.clone();
        let reserve_barrier = Arc::clone(&barrier);
        let reserve = thread::spawn(move || {
            let mut releases = reserve_host.release_queue(1);
            reserve_barrier.wait();
            let result = releases.reserve(4, 5);
            if let Ok(reservation) = result {
                releases.cancel_reservation(reservation);
                Ok(())
            } else {
                result.map(|_| ())
            }
        });
        barrier.wait();
        let _ = host.begin_close();
        assert!(matches!(
            reserve.join().unwrap(),
            Ok(()) | Err(ReleaseQueueError::HostClosing)
        ));

        let mut runtime = TaskRuntime::new(1, RuntimeLimits::default());
        let scope = runtime.create_scope(None).unwrap();
        let task = runtime.admit_task(scope, 1, true).unwrap();
        let mut request_releases = host.release_queue(1);
        let mut requests = HostRequestManager::with_completion_counter(
            1,
            1,
            Some(Arc::clone(&host.pending_completions)),
            Some(host.admission.clone()),
        );
        assert!(matches!(
            requests.create_for_module(2, 3, &mut request_releases),
            Err(HostRequestError::HostClosing)
        ));
        let mut token_releases = host.release_queue(1);
        let mut tokens = ResourceTokenManager::new(1, 1);
        assert_eq!(
            tokens.create(task, 2, 3, RuntimeHostDomain::Render, &mut token_releases),
            Err(HostRequestError::HostClosing)
        );
        let mut snapshot_releases = host.release_queue(1);
        let mut snapshots = SnapshotManager::new(1, 1);
        assert_eq!(
            snapshots.create(task, 2, 3, Arc::from([1]), &mut snapshot_releases),
            Err(HostRequestError::HostClosing)
        );
        host.try_finish_close().unwrap();
        assert_eq!(
            host.release_queue(1).reserve(4, 5),
            Err(ReleaseQueueError::HostClosed)
        );
    }

    #[test]
    fn old_completion_ticket_can_finish_while_host_is_closing() {
        let host = RuntimeHost::new(1);
        let mut requests = HostRequestManager::with_completion_counter(
            1,
            1,
            Some(Arc::clone(&host.pending_completions)),
            Some(host.admission.clone()),
        );
        let mut releases = host.release_queue(1);
        let mut pending = requests.create_for_module(2, 3, &mut releases).unwrap();
        let _ = host.begin_close();
        pending.ticket.complete(HostPayload::Unit).unwrap();
        assert_eq!(
            host.try_finish_close(),
            Err(RuntimeHostCloseError::PendingCompletions)
        );
        assert_eq!(requests.drain_completions(&mut releases).len(), 1);
        assert_eq!(releases.transfer_to_host(), 1);
        assert_eq!(
            host.try_finish_close(),
            Err(RuntimeHostCloseError::PendingReleases)
        );
        assert_eq!(host.drain_releases().len(), 1);
        host.try_finish_close().unwrap();
    }

    #[test]
    fn finish_close_races_realm_drop_and_release_drain() {
        let host = RuntimeHost::new(1);
        host.register_realm().unwrap();
        let _ = host.begin_close();
        let drop_host = host.clone();
        let unregister = thread::spawn(move || drop_host.unregister_realm());
        let first_finish = host.try_finish_close();
        unregister.join().unwrap();
        assert!(matches!(
            first_finish,
            Ok(_) | Err(RuntimeHostCloseError::LiveRealms)
        ));
        if host.state() == RuntimeHostState::Closing {
            host.try_finish_close().unwrap();
        }

        let host = RuntimeHost::new(1);
        let mut releases = host.release_queue(1);
        let reservation = releases.reserve(1, 1).unwrap();
        releases
            .enqueue_reserved(
                reservation,
                super::ReleaseRecord {
                    realm_id: 1,
                    module_id: 1,
                    epoch: 1,
                    kind: ReleaseKind::ResourceToken,
                    object_id: 1,
                    domain: RuntimeHostDomain::Render,
                },
            )
            .unwrap();
        releases.transfer_to_host();
        let _ = host.begin_close();
        let drain_host = host.clone();
        let drain = thread::spawn(move || drain_host.drain_releases());
        let first_finish = host.try_finish_close();
        let drained = drain.join().unwrap();
        assert_eq!(drained.len(), 1);
        assert!(matches!(
            first_finish,
            Ok(_) | Err(RuntimeHostCloseError::PendingReleases)
        ));
        if host.state() == RuntimeHostState::Closing {
            host.try_finish_close().unwrap();
        }
    }

    #[test]
    fn close_protocol_is_idempotent_but_cannot_skip_closing() {
        let host = RuntimeHost::new(1);
        assert_eq!(
            host.try_finish_close(),
            Err(RuntimeHostCloseError::NotClosing)
        );
        let first = host.begin_close();
        let second = host.begin_close();
        assert_eq!(first, second);
        let first = host.try_finish_close().unwrap();
        let second = host.try_finish_close().unwrap();
        assert_eq!(first, second);
        assert_eq!(second.state, RuntimeHostState::Closed);
    }

    #[test]
    fn one_admission_gate_classifies_every_host_resource_in_each_state() {
        const KINDS: [HostAdmissionKind; 7] = [
            HostAdmissionKind::HostedRealm,
            HostAdmissionKind::AsyncHostCall,
            HostAdmissionKind::HostRequest,
            HostAdmissionKind::CompletionReservation,
            HostAdmissionKind::ResourceToken,
            HostAdmissionKind::Snapshot,
            HostAdmissionKind::ReleaseReservation,
        ];
        let host = RuntimeHost::new(1);
        for kind in KINDS {
            assert_eq!(host.admission.admit(kind), Ok(()));
        }
        let _ = host.begin_close();
        for kind in KINDS {
            assert_eq!(host.admission.admit(kind), Err(HostAdmissionError::Closing));
        }
        host.try_finish_close().unwrap();
        for kind in KINDS {
            assert_eq!(host.admission.admit(kind), Err(HostAdmissionError::Closed));
        }
    }
}
