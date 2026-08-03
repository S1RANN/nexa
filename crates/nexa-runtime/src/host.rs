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

    #[cfg(any(test, feature = "model-adapter"))]
    #[must_use]
    pub fn inspection_releases(&self) -> Vec<ReleaseRecord> {
        let state = self
            .releases
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut records =
            Vec::with_capacity(state.host_lists.iter().map(|list| list.len).sum::<usize>());
        for list in &state.host_lists {
            let mut node = list.head;
            while let Some(index) = node {
                let entry = state.pool.nodes[index];
                if let Some(record) = entry.record {
                    records.push(record);
                }
                node = entry.next;
            }
        }
        records
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
    String(String),
    Array(CopyBuffer<HostPayload>),
    Buffer(CopyBuffer<HostPayload>),
    Struct(Vec<HostPayload>),
    Enum {
        type_id: nexa_core::StableId,
        variant: nexa_core::StableId,
        tag: u32,
        payload: Option<Box<HostPayload>>,
    },
    Opaque(u64),
    Token(ResourceTokenHandle),
    Snapshot(SnapshotHandle),
    Unit,
}

impl HostPayload {
    #[must_use]
    pub fn structure<const N: usize>(fields: [Self; N]) -> Self {
        Self::Struct(Vec::from(fields))
    }
}

const MAX_HOST_ARGUMENTS: usize = 8;
pub const MAX_HOST_RETURN_FIELDS: usize = nexa_bytecode::MAX_STRUCT_FIELDS;

/// A UTF-8 string view borrowed directly from the VM heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStr<'a>(&'a str);

impl<'a> HostStr<'a> {
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.0
    }
}

impl std::ops::Deref for HostStr<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// A single runtime value plus the heap storage it may borrow.
#[derive(Clone, Copy, Debug)]
pub struct HostValueRef<'a> {
    value: crate::RuntimeValue,
    heap: Option<&'a crate::Heap>,
    /// WP52: set when this logical value is a flattened struct row inside
    /// an array extent; `struct_ref` serves it as a borrowed view without
    /// any materialization.
    row: Option<HostStructRow<'a>>,
}

/// One flattened struct row borrowed straight from the collection arena.
#[derive(Clone, Copy, Debug)]
struct HostStructRow<'a> {
    type_id: StableId,
    fields: &'a [crate::RuntimeValue],
}

impl<'a> HostValueRef<'a> {
    pub(crate) const fn new(value: crate::RuntimeValue, heap: &'a crate::Heap) -> Self {
        Self {
            value,
            heap: Some(heap),
            row: None,
        }
    }

    #[must_use]
    pub const fn runtime_value(self) -> crate::RuntimeValue {
        self.value
    }

    pub fn i32(self) -> Result<i32, HostTrap> {
        match self.value {
            crate::RuntimeValue::I32(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn i64(self) -> Result<i64, HostTrap> {
        match self.value {
            crate::RuntimeValue::I64(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn f32(self) -> Result<f32, HostTrap> {
        match self.value {
            crate::RuntimeValue::F32(value) => Ok(f32::from_bits(value)),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn f64(self) -> Result<f64, HostTrap> {
        match self.value {
            crate::RuntimeValue::F64(value) => Ok(f64::from_bits(value)),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn bool(self) -> Result<bool, HostTrap> {
        match self.value {
            crate::RuntimeValue::Bool(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn rune(self) -> Result<char, HostTrap> {
        match self.value {
            crate::RuntimeValue::Rune(value) => char::from_u32(value).ok_or(HostTrap::Type),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn str_ref(self) -> Result<HostStr<'a>, HostTrap> {
        let crate::RuntimeValue::String { reference, .. } = self.value else {
            return Err(HostTrap::Type);
        };
        self.heap()?
            .string(reference)
            .map(HostStr)
            .map_err(|_| HostTrap::Type)
    }

    pub fn struct_ref(self, type_id: StableId) -> Result<HostStructRef<'a>, HostTrap> {
        // WP52: flattened array elements are served as borrowed views over
        // their arena row; nothing is materialized at the host boundary.
        if let Some(row) = self.row {
            if row.type_id != type_id {
                return Err(HostTrap::Type);
            }
            return Ok(HostStructRef {
                type_id,
                fields: crate::CollectionView::Values(row.fields),
                heap: self.heap()?,
            });
        }
        let crate::RuntimeValue::Struct {
            type_id: actual, ..
        } = self.value
        else {
            return Err(HostTrap::Type);
        };
        if actual != type_id {
            return Err(HostTrap::Type);
        }
        let heap = self.heap()?;
        let fields = heap.struct_fields(self.value).map_err(|_| HostTrap::Type)?;
        Ok(HostStructRef {
            type_id,
            fields,
            heap,
        })
    }

    pub fn class_ref(self, type_id: StableId) -> Result<HostClassRef<'a>, HostTrap> {
        if !matches!(
            self.value,
            crate::RuntimeValue::NamedRef {
                type_id: actual,
                ..
            } if actual == type_id
        ) {
            return Err(HostTrap::Type);
        }
        let heap = self.heap()?;
        let fields = heap.class_fields(self.value).map_err(|_| HostTrap::Type)?;
        Ok(HostClassRef {
            type_id,
            fields,
            heap,
        })
    }

    pub fn enum_ref(self, type_id: StableId) -> Result<HostEnumRef<'a>, HostTrap> {
        let heap = self.heap()?;
        let (actual, variant, tag, payload) =
            heap.enum_parts(self.value).map_err(|_| HostTrap::Type)?;
        if actual != type_id {
            return Err(HostTrap::Type);
        }
        Ok(HostEnumRef {
            type_id,
            variant,
            tag,
            payload,
            heap,
        })
    }

    pub fn array_ref(self, type_id: StableId) -> Result<HostArrayRef<'a>, HostTrap> {
        if !matches!(
            self.value,
            crate::RuntimeValue::NamedRef {
                type_id: actual,
                ..
            } if actual == type_id
        ) {
            return Err(HostTrap::Type);
        }
        let heap = self.heap()?;
        // WP52: flattened struct-element arrays expose their arena rows as
        // borrowed views; plain arrays keep the one-cell-per-element view.
        if let Some(rows) = heap.array_rows(self.value).map_err(|_| HostTrap::Type)? {
            return Ok(HostArrayRef {
                type_id,
                values: crate::CollectionView::Values(rows.cells),
                rows: Some((rows.stride, rows.struct_type)),
                heap,
            });
        }
        let values = heap.array_values(self.value).map_err(|_| HostTrap::Type)?;
        Ok(HostArrayRef {
            type_id,
            values,
            rows: None,
            heap,
        })
    }

    pub fn buffer_ref(self, type_id: StableId) -> Result<HostBufferRef<'a>, HostTrap> {
        if !matches!(
            self.value,
            crate::RuntimeValue::NamedRef {
                type_id: actual,
                ..
            } if actual == type_id
        ) {
            return Err(HostTrap::Type);
        }
        let heap = self.heap()?;
        let values = heap.buffer_values(self.value).map_err(|_| HostTrap::Type)?;
        Ok(HostBufferRef {
            type_id,
            values,
            rows: None,
            heap,
        })
    }

    pub fn map_ref(self, type_id: StableId) -> Result<HostMapRef<'a>, HostTrap> {
        if !matches!(
            self.value,
            crate::RuntimeValue::NamedRef {
                type_id: actual,
                ..
            } if actual == type_id
        ) {
            return Err(HostTrap::Type);
        }
        let heap = self.heap()?;
        let len = heap.map_len(self.value).map_err(|_| HostTrap::Type)?;
        Ok(HostMapRef {
            type_id,
            value: self.value,
            len,
            heap,
        })
    }

    fn heap(self) -> Result<&'a crate::Heap, HostTrap> {
        self.heap.ok_or(HostTrap::Type)
    }
}

/// A named struct whose fields remain in the VM heap.
#[derive(Clone, Copy, Debug)]
pub struct HostStructRef<'a> {
    type_id: StableId,
    fields: crate::CollectionView<'a>,
    heap: &'a crate::Heap,
}

impl<'a> HostStructRef<'a> {
    #[must_use]
    pub const fn type_id(self) -> StableId {
        self.type_id
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.fields.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(self, index: usize) -> Result<HostValueRef<'a>, HostTrap> {
        self.fields
            .get(index)
            .map(|value| HostValueRef {
                value,
                heap: Some(self.heap),
                row: None,
            })
            .ok_or(HostTrap::Type)
    }
}

/// A named class whose fields remain in the VM heap.
#[derive(Clone, Copy, Debug)]
pub struct HostClassRef<'a> {
    type_id: StableId,
    fields: crate::CollectionView<'a>,
    heap: &'a crate::Heap,
}

impl<'a> HostClassRef<'a> {
    #[must_use]
    pub const fn type_id(self) -> StableId {
        self.type_id
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.fields.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(self, index: usize) -> Result<HostValueRef<'a>, HostTrap> {
        self.fields
            .get(index)
            .map(|value| HostValueRef {
                value,
                heap: Some(self.heap),
                row: None,
            })
            .ok_or(HostTrap::Type)
    }

    #[must_use]
    pub fn iter(self) -> impl ExactSizeIterator<Item = HostValueRef<'a>> + 'a {
        self.fields.iter().map(|value| HostValueRef {
            value,
            heap: Some(self.heap),
            row: None,
        })
    }
}

/// A named enum whose optional payload remains in the VM heap.
#[derive(Clone, Copy, Debug)]
pub struct HostEnumRef<'a> {
    type_id: StableId,
    variant: StableId,
    tag: u32,
    payload: Option<crate::RuntimeValue>,
    heap: &'a crate::Heap,
}

impl<'a> HostEnumRef<'a> {
    #[must_use]
    pub const fn type_id(self) -> StableId {
        self.type_id
    }

    #[must_use]
    pub const fn variant(self) -> StableId {
        self.variant
    }

    #[must_use]
    pub const fn tag(self) -> u32 {
        self.tag
    }

    #[must_use]
    pub fn payload(self) -> Option<HostValueRef<'a>> {
        self.payload.map(|value| HostValueRef {
            value,
            heap: Some(self.heap),
            row: None,
        })
    }
}

macro_rules! host_collection_ref {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug)]
        pub struct $name<'a> {
            type_id: StableId,
            /// Live arena cells: one per element, or `stride` per element
            /// for flattened struct rows (WP52).
            values: crate::CollectionView<'a>,
            /// `Some((stride, struct_type))` when elements are flattened
            /// struct rows served as borrowed views.
            rows: Option<(usize, StableId)>,
            heap: &'a crate::Heap,
        }

        impl<'a> $name<'a> {
            #[must_use]
            pub const fn type_id(self) -> StableId {
                self.type_id
            }

            #[must_use]
            pub const fn len(self) -> usize {
                match self.rows {
                    Some((stride, _)) => {
                        let divisor = if stride == 0 { 1 } else { stride };
                        self.values.len() / divisor
                    }
                    None => self.values.len(),
                }
            }

            #[must_use]
            pub const fn is_empty(self) -> bool {
                self.values.is_empty()
            }

            pub fn get(self, index: usize) -> Result<HostValueRef<'a>, HostTrap> {
                if let Some((stride, struct_type)) = self.rows {
                    let crate::CollectionView::Values(values) = self.values else {
                        return Err(HostTrap::Type);
                    };
                    let fields = values
                        .get(index * stride..(index + 1) * stride)
                        .ok_or(HostTrap::Type)?;
                    return Ok(HostValueRef {
                        value: crate::RuntimeValue::Unit,
                        heap: Some(self.heap),
                        row: Some(HostStructRow {
                            type_id: struct_type,
                            fields,
                        }),
                    });
                }
                self.values
                    .get(index)
                    .map(|value| HostValueRef {
                        value,
                        heap: Some(self.heap),
                        row: None,
                    })
                    .ok_or(HostTrap::Type)
            }

            pub fn iter(self) -> impl ExactSizeIterator<Item = HostValueRef<'a>> + 'a {
                (0..self.len()).map(move |index| {
                    self.get(index)
                        .expect("iterated index stays within the borrowed view")
                })
            }
        }
    };
}

host_collection_ref!(HostArrayRef);
host_collection_ref!(HostBufferRef);

/// One borrowed key/value pair from a VM map.
#[derive(Clone, Copy, Debug)]
pub struct HostMapEntryRef<'a> {
    key: crate::RuntimeValue,
    value: crate::RuntimeValue,
    heap: &'a crate::Heap,
}

impl<'a> HostMapEntryRef<'a> {
    #[must_use]
    pub const fn key(self) -> HostValueRef<'a> {
        HostValueRef {
            value: self.key,
            heap: Some(self.heap),
            row: None,
        }
    }

    #[must_use]
    pub const fn value(self) -> HostValueRef<'a> {
        HostValueRef {
            value: self.value,
            heap: Some(self.heap),
            row: None,
        }
    }
}

/// A named map whose entries remain in the VM heap.
///
/// Iteration follows the heap's deterministic backing-slot order. It does not
/// allocate, recompute hashes, or expose mutable map storage.
#[derive(Clone, Copy, Debug)]
pub struct HostMapRef<'a> {
    type_id: StableId,
    value: crate::RuntimeValue,
    len: usize,
    heap: &'a crate::Heap,
}

impl<'a> HostMapRef<'a> {
    #[must_use]
    pub const fn type_id(self) -> StableId {
        self.type_id
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn entry(self, index: usize) -> Result<HostMapEntryRef<'a>, HostTrap> {
        self.iter().nth(index).ok_or(HostTrap::Type)
    }

    #[must_use]
    pub fn iter(self) -> impl ExactSizeIterator<Item = HostMapEntryRef<'a>> + 'a {
        let heap = self.heap;
        heap.map_entries(self.value)
            .expect("validated immutable map reference remains valid")
            .map(move |(key, value)| HostMapEntryRef { key, value, heap })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOptionRef<'a, T> {
    None,
    Some(T),
    #[doc(hidden)]
    __Lifetime(std::marker::PhantomData<&'a ()>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostResultRef<'a, T, E> {
    Ok(T),
    Err(E),
    #[doc(hidden)]
    __Lifetime(std::marker::PhantomData<&'a ()>),
}

#[derive(Debug)]
pub struct RuntimeHostArgs<'a> {
    values: &'a [crate::RuntimeValue],
    heap: Option<&'a mut crate::Heap>,
}

impl<'a> RuntimeHostArgs<'a> {
    pub fn new(
        values: &'a [crate::RuntimeValue],
        heap: Option<&'a mut crate::Heap>,
    ) -> Result<Self, HostTrap> {
        if values.len() > MAX_HOST_ARGUMENTS {
            return Err(HostTrap::Arity);
        }
        Ok(Self { values, heap })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn i32(&self, index: usize) -> Result<i32, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::I32(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn i64(&self, index: usize) -> Result<i64, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::I64(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn f32(&self, index: usize) -> Result<f32, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::F32(bits) => Ok(f32::from_bits(bits)),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn f64(&self, index: usize) -> Result<f64, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::F64(bits) => Ok(f64::from_bits(bits)),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn bool(&self, index: usize) -> Result<bool, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::Bool(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn rune(&self, index: usize) -> Result<char, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::Rune(value) => char::from_u32(value).ok_or(HostTrap::Type),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn string(&self, index: usize) -> Result<String, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::String { reference, .. } => self
                .heap
                .as_deref()
                .ok_or(HostTrap::Type)?
                .string(reference)
                .map(str::to_owned)
                .map_err(|_| HostTrap::Type),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn value_ref(&self, index: usize) -> Result<HostValueRef<'_>, HostTrap> {
        Ok(HostValueRef {
            value: self.value(index)?,
            heap: self.heap.as_deref(),
            row: None,
        })
    }

    pub fn str_ref(&self, index: usize) -> Result<HostStr<'_>, HostTrap> {
        self.value_ref(index)?.str_ref()
    }

    pub fn struct_ref(
        &self,
        index: usize,
        type_id: StableId,
    ) -> Result<HostStructRef<'_>, HostTrap> {
        self.value_ref(index)?.struct_ref(type_id)
    }

    pub fn class_ref(&self, index: usize, type_id: StableId) -> Result<HostClassRef<'_>, HostTrap> {
        self.value_ref(index)?.class_ref(type_id)
    }

    pub fn enum_ref(&self, index: usize, type_id: StableId) -> Result<HostEnumRef<'_>, HostTrap> {
        self.value_ref(index)?.enum_ref(type_id)
    }

    pub fn array_ref(&self, index: usize, type_id: StableId) -> Result<HostArrayRef<'_>, HostTrap> {
        self.value_ref(index)?.array_ref(type_id)
    }

    pub fn buffer_ref(
        &self,
        index: usize,
        type_id: StableId,
    ) -> Result<HostBufferRef<'_>, HostTrap> {
        self.value_ref(index)?.buffer_ref(type_id)
    }

    pub fn map_ref(&self, index: usize, type_id: StableId) -> Result<HostMapRef<'_>, HostTrap> {
        self.value_ref(index)?.map_ref(type_id)
    }

    pub fn request(&self, index: usize) -> Result<HostRequestHandle, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::HostRequest(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn token(&self, index: usize) -> Result<ResourceTokenHandle, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::ResourceToken(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn typed_token(
        &self,
        index: usize,
        content_type: StableId,
    ) -> Result<ResourceTokenHandle, HostTrap> {
        let token = self.token(index)?;
        if token.content_type() != content_type {
            return Err(HostTrap::Type);
        }
        Ok(token)
    }

    pub fn snapshot(&self, index: usize) -> Result<SnapshotHandle, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::Snapshot(value) => Ok(value),
            _ => Err(HostTrap::Type),
        }
    }

    pub fn opaque(&self, index: usize) -> Result<u64, HostTrap> {
        match self.value(index)? {
            crate::RuntimeValue::Opaque { value, .. } => Ok(value),
            crate::RuntimeValue::Ref(reference)
            | crate::RuntimeValue::NamedRef { reference, .. } => {
                Ok(u64::from(reference.generation) << 32 | u64::from(reference.index))
            }
            _ => Err(HostTrap::Type),
        }
    }

    pub fn return_transaction(
        self,
        requirements: HostReturnRequirements,
    ) -> Result<HostReturnTransaction<'a>, HostTrap> {
        HostReturnTransaction::new(self.heap.ok_or(HostTrap::Type)?, requirements)
    }

    fn value(&self, index: usize) -> Result<crate::RuntimeValue, HostTrap> {
        self.values.get(index).copied().ok_or(HostTrap::Arity)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostReturnRequirements {
    pub object_slots: usize,
    pub collection_elements: usize,
    pub string_bytes: usize,
    pub struct_fields: usize,
}

impl HostReturnRequirements {
    pub const ZERO: Self = Self {
        object_slots: 0,
        collection_elements: 0,
        string_bytes: 0,
        struct_fields: 0,
    };

    pub fn checked_add(self, other: Self) -> Result<Self, HostTrap> {
        Ok(Self {
            object_slots: self
                .object_slots
                .checked_add(other.object_slots)
                .ok_or(HostTrap::Type)?,
            collection_elements: self
                .collection_elements
                .checked_add(other.collection_elements)
                .ok_or(HostTrap::Type)?,
            string_bytes: self
                .string_bytes
                .checked_add(other.string_bytes)
                .ok_or(HostTrap::Type)?,
            struct_fields: self
                .struct_fields
                .checked_add(other.struct_fields)
                .ok_or(HostTrap::Type)?,
        })
    }

    pub fn with_object(self) -> Result<Self, HostTrap> {
        self.checked_add(Self {
            object_slots: 1,
            ..Self::ZERO
        })
    }

    pub fn with_collection(self, elements: usize) -> Result<Self, HostTrap> {
        self.checked_add(Self {
            collection_elements: elements,
            ..Self::ZERO
        })
    }

    pub fn with_struct_fields(self, fields: usize) -> Result<Self, HostTrap> {
        self.checked_add(Self {
            struct_fields: fields,
            ..Self::ZERO
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct HostCollectionBuilder {
    range: crate::heap::CollectionRange,
    written: usize,
    type_id: StableId,
    element_type: nexa_bytecode::ValueType,
    storage: crate::CollectionStorage,
    buffer: bool,
    pending: usize,
}

#[derive(Clone, Copy, Debug)]
struct PendingHostCollection {
    range: crate::heap::CollectionRange,
    storage: crate::CollectionStorage,
}

/// An all-or-nothing encoder over pre-reserved object and collection storage.
pub struct HostReturnTransaction<'a> {
    heap: &'a mut crate::Heap,
    heap_reservation: crate::heap::HeapReservation,
    collection_quota: crate::heap::CollectionQuotaReservation,
    pending_collections: [Option<PendingHostCollection>; MAX_HOST_RETURN_FIELDS],
    remaining_string_bytes: usize,
    remaining_struct_fields: usize,
    committed: bool,
}

impl<'a> HostReturnTransaction<'a> {
    pub(crate) fn new(
        heap: &'a mut crate::Heap,
        requirements: HostReturnRequirements,
    ) -> Result<Self, HostTrap> {
        heap.preflight_host_string_bytes(requirements.string_bytes)
            .map_err(|_| HostTrap::Type)?;
        if heap.failure_trigger(crate::RuntimeFailurePoint::HostReturnObjectReservation) {
            return Err(HostTrap::Type);
        }
        let heap_reservation = heap
            .preflight(requirements.object_slots)
            .map_err(|_| HostTrap::Type)?;
        if heap.failure_trigger(crate::RuntimeFailurePoint::HostReturnCollectionReservation) {
            return Err(HostTrap::Type);
        }
        let mut collection_quota = heap
            .preflight_collection_quota(requirements.collection_elements)
            .map_err(|_| HostTrap::Type)?;
        if heap.begin_host_transaction().is_err() {
            heap.release_collection_quota_reservation(&mut collection_quota);
            return Err(HostTrap::Type);
        }
        Ok(Self {
            heap,
            heap_reservation,
            collection_quota,
            pending_collections: [None; MAX_HOST_RETURN_FIELDS],
            remaining_string_bytes: requirements.string_bytes,
            remaining_struct_fields: requirements.struct_fields,
            committed: false,
        })
    }

    pub fn write_string(&mut self, value: String) -> Result<crate::RuntimeValue, HostTrap> {
        self.remaining_string_bytes = self
            .remaining_string_bytes
            .checked_sub(value.len())
            .ok_or(HostTrap::Type)?;
        self.heap
            .commit_owned_string(&mut self.heap_reservation, value)
            .map_err(|_| HostTrap::Type)
    }

    pub fn write_struct(
        &mut self,
        type_id: StableId,
        fields: &[crate::RuntimeValue],
    ) -> Result<crate::RuntimeValue, HostTrap> {
        if self
            .heap
            .failure_trigger(crate::RuntimeFailurePoint::HostReturnStructWrite)
        {
            return Err(HostTrap::Type);
        }
        self.remaining_struct_fields = self
            .remaining_struct_fields
            .checked_sub(fields.len())
            .ok_or(HostTrap::Type)?;
        let value = self
            .heap
            .commit_struct(&mut self.heap_reservation, type_id, fields)
            .map_err(|_| HostTrap::Type)?;
        self.heap.record_host_codec_field_copy(fields);
        Ok(value)
    }

    pub fn write_enum(
        &mut self,
        type_id: StableId,
        variant: StableId,
        tag: u32,
        payload: Option<crate::RuntimeValue>,
    ) -> Result<crate::RuntimeValue, HostTrap> {
        let value = self.heap.allocate_enum_reserved(
            &mut self.heap_reservation,
            type_id,
            variant,
            tag,
            payload,
        );
        if payload.is_some() {
            self.heap
                .record_host_codec_copy(std::mem::size_of::<crate::RuntimeValue>() as u64);
        }
        Ok(value)
    }

    pub fn begin_array(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        length: usize,
    ) -> Result<HostCollectionBuilder, HostTrap> {
        self.heap
            .validate_collection_length(length)
            .map_err(|_| HostTrap::Type)?;
        let pending = self
            .pending_collections
            .iter()
            .position(Option::is_none)
            .ok_or(HostTrap::Type)?;
        // A bare Named type is ambiguous at this API boundary: structs and
        // opaque handles require wide value cells. Generated bindings use
        // the explicit compact-reference entrypoint for enum/collection
        // references.
        let storage = match element_type {
            nexa_bytecode::ValueType::Named(_) => crate::CollectionStorage::Values,
            other => crate::CollectionStorage::for_type(other),
        };
        let range = self
            .heap
            .claim_reserved_typed_collection(&mut self.collection_quota, storage, length)
            .map_err(|_| HostTrap::Type)?;
        self.pending_collections[pending] = Some(PendingHostCollection { range, storage });
        Ok(HostCollectionBuilder {
            range,
            written: 0,
            type_id,
            element_type,
            storage,
            buffer: false,
            pending,
        })
    }

    pub fn push_array_value(
        &mut self,
        builder: &mut HostCollectionBuilder,
        value: crate::RuntimeValue,
    ) -> Result<(), HostTrap> {
        self.push_collection_value(builder, value)
    }

    /// Starts an Array whose named elements are represented by one compact
    /// GC reference (enum/class/collection values), not a wide value cell.
    pub fn begin_reference_array(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        length: usize,
    ) -> Result<HostCollectionBuilder, HostTrap> {
        if !matches!(element_type, nexa_bytecode::ValueType::Named(_)) {
            return Err(HostTrap::Type);
        }
        let mut builder = self.begin_array(type_id, element_type, length)?;
        let storage = crate::CollectionStorage::NamedRef;
        self.heap
            .claim_physical_collection(storage, builder.range)
            .map_err(|_| HostTrap::Type)?;
        builder.storage = storage;
        self.pending_collections[builder.pending] = Some(PendingHostCollection {
            range: builder.range,
            storage,
        });
        Ok(builder)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn finish_array(
        &mut self,
        builder: HostCollectionBuilder,
    ) -> Result<crate::RuntimeValue, HostTrap> {
        if builder.buffer || builder.written != builder.range.length {
            return Err(HostTrap::Type);
        }
        let value = self
            .heap
            .commit_array_reserved(
                &mut self.heap_reservation,
                builder.type_id,
                builder.element_type,
                builder.storage,
                builder.range,
            )
            .map_err(|_| HostTrap::Type)?;
        self.pending_collections[builder.pending] = None;
        Ok(value)
    }

    pub fn begin_buffer(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        length: usize,
    ) -> Result<HostCollectionBuilder, HostTrap> {
        let mut builder = self.begin_array(type_id, element_type, length)?;
        builder.buffer = true;
        Ok(builder)
    }

    pub fn begin_reference_buffer(
        &mut self,
        type_id: StableId,
        element_type: nexa_bytecode::ValueType,
        length: usize,
    ) -> Result<HostCollectionBuilder, HostTrap> {
        let mut builder = self.begin_reference_array(type_id, element_type, length)?;
        builder.buffer = true;
        Ok(builder)
    }

    pub fn push_buffer_value(
        &mut self,
        builder: &mut HostCollectionBuilder,
        value: crate::RuntimeValue,
    ) -> Result<(), HostTrap> {
        self.push_collection_value(builder, value)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn finish_buffer(
        &mut self,
        builder: HostCollectionBuilder,
    ) -> Result<crate::RuntimeValue, HostTrap> {
        if !builder.buffer || builder.written != builder.range.length {
            return Err(HostTrap::Type);
        }
        let value = self
            .heap
            .commit_buffer_reserved(
                &mut self.heap_reservation,
                builder.type_id,
                builder.element_type,
                builder.storage,
                builder.range,
            )
            .map_err(|_| HostTrap::Type)?;
        self.pending_collections[builder.pending] = None;
        Ok(value)
    }

    fn push_collection_value(
        &mut self,
        builder: &mut HostCollectionBuilder,
        value: crate::RuntimeValue,
    ) -> Result<(), HostTrap> {
        if self
            .heap
            .failure_trigger(crate::RuntimeFailurePoint::HostReturnCollectionWrite)
        {
            return Err(HostTrap::Type);
        }
        self.heap
            .typed_collection_set(
                builder.storage,
                builder.element_type,
                builder.range,
                builder.written,
                value,
            )
            .map_err(|_| HostTrap::Type)?;
        self.heap.record_host_codec_storage_copy(builder.storage, 1);
        builder.written += 1;
        Ok(())
    }

    pub fn commit(mut self, value: crate::RuntimeValue) -> Result<crate::RuntimeValue, HostTrap> {
        self.finish()?;
        Ok(value)
    }

    pub fn commit_arguments(
        mut self,
        values: Vec<crate::RuntimeValue>,
    ) -> Result<Vec<crate::RuntimeValue>, HostTrap> {
        self.finish()?;
        Ok(values)
    }

    fn finish(&mut self) -> Result<(), HostTrap> {
        if self
            .heap
            .failure_trigger(crate::RuntimeFailurePoint::HostReturnCommit)
            || !crate::Heap::reservation_complete(&self.heap_reservation)
            || self.remaining_string_bytes != 0
            || self.remaining_struct_fields != 0
            || !crate::Heap::collection_quota_complete(&self.collection_quota)
            || self.pending_collections.iter().any(Option::is_some)
        {
            return Err(HostTrap::Type);
        }
        self.heap.commit_host_transaction();
        self.committed = true;
        Ok(())
    }
}

impl Drop for HostReturnTransaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.heap.rollback_host_transaction();
            for pending in &mut self.pending_collections {
                if let Some(pending) = pending.take() {
                    self.heap
                        .release_typed_collection(pending.storage, pending.range);
                }
            }
            self.heap
                .release_collection_quota_reservation(&mut self.collection_quota);
        }
    }
}

pub trait EncodeHostReturn {
    fn requirements(&self) -> Result<HostReturnRequirements, HostTrap>;

    fn encode_into(
        self,
        transaction: &mut HostReturnTransaction<'_>,
    ) -> Result<crate::RuntimeValue, HostTrap>;
}

macro_rules! scalar_host_return {
    ($ty:ty, $variant:ident, $encode:expr) => {
        impl EncodeHostReturn for $ty {
            fn requirements(&self) -> Result<HostReturnRequirements, HostTrap> {
                Ok(HostReturnRequirements::ZERO)
            }

            fn encode_into(
                self,
                _: &mut HostReturnTransaction<'_>,
            ) -> Result<crate::RuntimeValue, HostTrap> {
                Ok(crate::RuntimeValue::$variant(($encode)(self)))
            }
        }
    };
}

scalar_host_return!(i32, I32, |value| value);
scalar_host_return!(i64, I64, |value| value);
scalar_host_return!(f32, F32, f32::to_bits);
scalar_host_return!(f64, F64, f64::to_bits);
scalar_host_return!(bool, Bool, |value| value);
scalar_host_return!(char, Rune, |value| value as u32);

impl EncodeHostReturn for String {
    fn requirements(&self) -> Result<HostReturnRequirements, HostTrap> {
        Ok(HostReturnRequirements {
            object_slots: 1,
            string_bytes: self.len(),
            ..HostReturnRequirements::ZERO
        })
    }

    fn encode_into(
        self,
        transaction: &mut HostReturnTransaction<'_>,
    ) -> Result<crate::RuntimeValue, HostTrap> {
        transaction.write_string(self)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostCallOutcome {
    RuntimeImmediate(crate::RuntimeValue),
    Pending(HostRequestHandle),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostTrap {
    UnknownFunction(StableId),
    InvalidFunctionSlot(HostFunctionSlot),
    Arity,
    Type,
    ResourceCapacity,
    Panicked,
    Host(crate::RuntimeMessage),
}

/// Registry-local dense identity for one Host function.
///
/// Stable IDs remain the portable linking identity. A Realm resolves them to
/// these slots exactly once during module admission, then the interpreter
/// carries only the dense slot across the Host boundary.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostFunctionSlot(u32);

impl HostFunctionSlot {
    #[must_use]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Immutable runtime authority for one Host function in a Contract.
///
/// Generated registries expose these records by [`StableId`]. A Realm accepts
/// a module Host import only when every executable field is identical to this
/// Contract metadata, preventing bytecode from weakening fuel or async
/// completion policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFunctionAuthority {
    stable_id: StableId,
    declaration_fingerprint: [u8; 32],
    parameters: Vec<nexa_bytecode::ValueType>,
    result: Option<nexa_bytecode::ValueType>,
    mode: nexa_bytecode::HostCallMode,
    fuel_cost: u32,
    async_result: Option<nexa_bytecode::AsyncResultType>,
    capabilities: Vec<String>,
}

impl HostFunctionAuthority {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stable_id: StableId,
        declaration_fingerprint: [u8; 32],
        parameters: &[nexa_bytecode::ValueType],
        result: Option<nexa_bytecode::ValueType>,
        mode: nexa_bytecode::HostCallMode,
        fuel_cost: u32,
        async_result: Option<nexa_bytecode::AsyncResultType>,
        capabilities: &[&str],
    ) -> Self {
        Self::new_owned(
            stable_id,
            declaration_fingerprint,
            parameters.to_vec(),
            result,
            mode,
            fuel_cost,
            async_result,
            capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
        )
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new_owned(
        stable_id: StableId,
        declaration_fingerprint: [u8; 32],
        parameters: Vec<nexa_bytecode::ValueType>,
        result: Option<nexa_bytecode::ValueType>,
        mode: nexa_bytecode::HostCallMode,
        fuel_cost: u32,
        async_result: Option<nexa_bytecode::AsyncResultType>,
        capabilities: Vec<String>,
    ) -> Self {
        Self {
            stable_id,
            declaration_fingerprint,
            parameters,
            result,
            mode,
            fuel_cost,
            async_result,
            capabilities,
        }
    }

    #[must_use]
    pub fn from_import(import: &nexa_bytecode::HostImport) -> Self {
        Self::new_owned(
            import.stable_id,
            import.declaration_fingerprint,
            import.parameters.clone(),
            import.result,
            import.mode,
            import.fuel_cost,
            import.async_result,
            import.capabilities.clone(),
        )
    }

    #[must_use]
    pub const fn stable_id(&self) -> StableId {
        self.stable_id
    }

    #[must_use]
    pub const fn declaration_fingerprint(&self) -> [u8; 32] {
        self.declaration_fingerprint
    }

    #[must_use]
    pub fn parameters(&self) -> &[nexa_bytecode::ValueType] {
        &self.parameters
    }

    #[must_use]
    pub const fn result(&self) -> Option<nexa_bytecode::ValueType> {
        self.result
    }

    #[must_use]
    pub const fn mode(&self) -> nexa_bytecode::HostCallMode {
        self.mode
    }

    #[must_use]
    pub const fn fuel_cost(&self) -> u32 {
        self.fuel_cost
    }

    #[must_use]
    pub const fn async_result(&self) -> Option<nexa_bytecode::AsyncResultType> {
        self.async_result
    }

    #[must_use]
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }
}

/// Load-time binding between portable Host authority and registry-local slot.
#[derive(Clone, Copy, Debug)]
pub struct ResolvedHostFunction<'a> {
    slot: HostFunctionSlot,
    authority: &'a HostFunctionAuthority,
}

impl<'a> ResolvedHostFunction<'a> {
    #[must_use]
    pub const fn new(slot: HostFunctionSlot, authority: &'a HostFunctionAuthority) -> Self {
        Self { slot, authority }
    }

    #[must_use]
    pub const fn slot(self) -> HostFunctionSlot {
        self.slot
    }

    #[must_use]
    pub const fn authority(self) -> &'a HostFunctionAuthority {
        self.authority
    }
}

pub trait HostRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        None
    }

    /// Resolve a portable Stable ID during module admission.
    ///
    /// The returned authority and slot are one indivisible binding: admission
    /// validates the authority once and records the slot for every hot call.
    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>>;

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap>;
}

pub const HOST_CONTRACT_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostContract {
    contract_name: &'static str,
    source: &'static str,
    canonical_descriptor: &'static [u8],
    contract_fingerprint: [u8; 32],
    contract_runtime_id: StableId,
    generator_schema_version: u32,
}

impl HostContract {
    #[must_use]
    pub const fn new(
        contract_name: &'static str,
        source: &'static str,
        canonical_descriptor: &'static [u8],
        contract_fingerprint: [u8; 32],
        contract_runtime_id: StableId,
        generator_schema_version: u32,
    ) -> Self {
        let expected = contract_runtime_id_from_fingerprint(contract_fingerprint);
        assert!(
            contract_runtime_id.0 == expected.0,
            "Contract runtime ID must be the fingerprint's first eight little-endian bytes"
        );
        assert!(
            generator_schema_version == HOST_CONTRACT_SCHEMA_VERSION,
            "Host Contract generator schema version must match this Runtime"
        );
        Self {
            contract_name,
            source,
            canonical_descriptor,
            contract_fingerprint,
            contract_runtime_id,
            generator_schema_version,
        }
    }

    #[must_use]
    pub const fn contract_name(self) -> &'static str {
        self.contract_name
    }

    #[must_use]
    pub const fn source(self) -> &'static str {
        self.source
    }

    #[must_use]
    pub const fn canonical_descriptor(self) -> &'static [u8] {
        self.canonical_descriptor
    }

    #[must_use]
    pub const fn contract_fingerprint(self) -> [u8; 32] {
        self.contract_fingerprint
    }

    #[must_use]
    pub const fn contract_runtime_id(self) -> StableId {
        self.contract_runtime_id
    }

    #[must_use]
    pub const fn generator_schema_version(self) -> u32 {
        self.generator_schema_version
    }
}

#[must_use]
pub const fn contract_runtime_id_from_fingerprint(fingerprint: [u8; 32]) -> StableId {
    StableId(u64::from_le_bytes([
        fingerprint[0],
        fingerprint[1],
        fingerprint[2],
        fingerprint[3],
        fingerprint[4],
        fingerprint[5],
        fingerprint[6],
        fingerprint[7],
    ]))
}

pub type ScriptArgumentRequirements = HostReturnRequirements;
pub type ScriptCallWriter<'a> = HostReturnTransaction<'a>;

#[derive(Clone, Copy, Debug)]
pub struct ScriptOutputReader<'a> {
    heap: &'a crate::Heap,
}

impl<'a> ScriptOutputReader<'a> {
    pub(crate) const fn new(heap: &'a crate::Heap) -> Self {
        Self { heap }
    }

    #[must_use]
    pub const fn value(self, value: crate::RuntimeValue) -> HostValueRef<'a> {
        HostValueRef::new(value, self.heap)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptCallError {
    MissingExport {
        name: &'static str,
        stable_id: StableId,
    },
    SignatureMismatch {
        name: &'static str,
    },
    EffectMismatch {
        name: &'static str,
    },
    EffectNotCallable {
        name: &'static str,
    },
    ArgumentEncoding,
    OutputDecoding,
    Runtime(String),
    HandlerDidNotComplete,
    HostWaitNotAllowed,
    HandlerTrapped(Box<crate::Trap>),
}

impl fmt::Display for ScriptCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ScriptCallError {}

impl From<HostTrap> for ScriptCallError {
    fn from(_: HostTrap) -> Self {
        Self::ArgumentEncoding
    }
}

pub trait ScriptExport {
    type Args;
    type Output;

    const STABLE_ID: StableId;
    const NAME: &'static str;

    fn signature() -> nexa_bytecode::Signature;

    fn effect() -> nexa_bytecode::FunctionEffect;

    fn argument_requirements(
        args: &Self::Args,
    ) -> Result<ScriptArgumentRequirements, ScriptCallError>;

    fn encode_args(
        writer: &mut ScriptCallWriter<'_>,
        args: &Self::Args,
    ) -> Result<Vec<crate::RuntimeValue>, ScriptCallError>;

    fn decode_output(
        reader: &ScriptOutputReader<'_>,
        value: crate::RuntimeValue,
    ) -> Result<Self::Output, ScriptCallError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MustCompletePolicy {
    pub fuel: u64,
    pub cumulative_budget: u64,
}

impl Default for MustCompletePolicy {
    fn default() -> Self {
        Self {
            fuel: 20_000,
            cumulative_budget: 20_000,
        }
    }
}

/// Typed error value delivered by an asynchronous Host request.
///
/// `Code` remains available to low-level registries that deliberately use the compact numeric
/// completion protocol. Generated NIDL bindings use `Value`, preserving the declared
/// `Result<Success, Error>` error payload without narrowing nominal or aggregate values to a
/// `u32`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostErrorPayload {
    Code(u32),
    Value(HostPayload),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCompletionResult {
    Success(HostPayload),
    Error(HostErrorPayload),
    Cancelled,
    Abandoned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCompletionProtocolError {
    Abandoned,
    UnknownErrorCode(u32),
}

impl fmt::Display for HostCompletionProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HostCompletionProtocolError {}

pub fn validate_host_completion(
    completion: &HostCompletionResult,
    known_error_codes: &[u32],
) -> Result<(), HostCompletionProtocolError> {
    match completion {
        HostCompletionResult::Error(HostErrorPayload::Code(code))
            if !known_error_codes.contains(code) =>
        {
            Err(HostCompletionProtocolError::UnknownErrorCode(*code))
        }
        HostCompletionResult::Abandoned => Err(HostCompletionProtocolError::Abandoned),
        HostCompletionResult::Success(_)
        | HostCompletionResult::Error(_)
        | HostCompletionResult::Cancelled => Ok(()),
    }
}

pub fn invoke_host_boundary<T>(call: impl FnOnce() -> Result<T, HostTrap>) -> Result<T, HostTrap> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)).map_err(|_| HostTrap::Panicked)?
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
    pub late_discarded: u64,
}

impl CompletionAccounting {
    #[must_use]
    pub const fn terminal_total(self) -> u64 {
        self.delivered
            .saturating_add(self.cancelled)
            .saturating_add(self.abandoned)
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
    StaleHostRequestHandle,
    CrossRealmHostRequestHandle,
    AlreadyCompleted,
    DetachedByReload,
    InvalidState,
    InjectedFailure(crate::RuntimeFailurePoint),
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

    fn submit_result(
        &self,
        request: HostRequestHandle,
        result: HostCompletionResult,
    ) -> Result<(), HostRequestError> {
        if request.raw().realm_id != self.realm_id {
            return Err(HostRequestError::CrossRealmHostRequestHandle);
        }
        let reservation = match self.requests.resolve(request.raw()) {
            Ok(request) => request
                .completion_reservation
                .ok_or(HostRequestError::AlreadyCompleted)?,
            Err(_) => {
                return Err(match self.terminal_record(request) {
                    Some(terminal) if terminal.state == HostRequestState::Detached => {
                        HostRequestError::DetachedByReload
                    }
                    Some(_) => HostRequestError::AlreadyCompleted,
                    None => HostRequestError::StaleHostRequestHandle,
                });
            }
        };
        self.completion_sender().submit(reservation, result)
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
pub struct ResourceTokenHandle {
    raw: RawHandle,
    content_type: StableId,
}

impl ResourceTokenHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.raw
    }

    #[must_use]
    pub const fn content_type(self) -> StableId {
        self.content_type
    }

    #[must_use]
    pub fn token_type(self) -> StableId {
        nexa_bytecode::resource_token_type(self.content_type)
    }
}

#[derive(Debug)]
struct ResourceToken {
    module_id: u32,
    epoch: u64,
    domain: RuntimeHostDomain,
    content_type: StableId,
    state: resource_token::State,
    release: Option<ReleaseReservation>,
}

#[derive(Debug)]
pub(crate) struct ResourceTokenManager {
    realm_id: u32,
    tokens: SlotPool<ResourceToken>,
    epoch_counts: EpochCounts,
    terminal: VecDeque<ResourceTokenHandle>,
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
        content_type: StableId,
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
            content_type,
            state: published.state,
            release: Some(release),
        }) {
            Ok(handle) => {
                increment_epoch_count(&mut self.epoch_counts, module_id, epoch);
                Ok(ResourceTokenHandle {
                    raw: handle,
                    content_type,
                })
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
        if self.terminal.contains(&handle) {
            return Ok(false);
        }
        if self
            .terminal
            .iter()
            .any(|terminal| terminal.raw == handle.raw)
        {
            return Err(HostRequestError::InvalidState);
        }
        let token = self.tokens.resolve_mut(handle.raw)?;
        if token.content_type != handle.content_type {
            return Err(HostRequestError::InvalidState);
        }
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
                object_id: u64::from(handle.raw.generation) << 32 | u64::from(handle.raw.index),
                domain,
            },
        )?;
        token.state =
            resource_token::apply(token.state, resource_token::Event::HostRelease, |_| true)
                .expect("generated resource release transition exists")
                .state;
        decrement_epoch_count(&mut self.epoch_counts, token.module_id, token.epoch);
        let _ = token;
        self.tokens.release(handle.raw)?;
        if self.terminal.len() == self.terminal_capacity {
            self.terminal.pop_front();
        }
        self.terminal.push_back(handle);
        Ok(true)
    }

    fn count_for_epoch(&self, module_id: u32, epoch: u64) -> usize {
        self.epoch_counts.get(module_id, epoch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotHandle {
    raw: RawHandle,
    type_id: StableId,
}

impl SnapshotHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.raw
    }

    #[must_use]
    pub const fn type_id(self) -> StableId {
        self.type_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotLayout {
    pub size: u32,
    pub alignment: u16,
    pub schema_hash: StableId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedSnapshot {
    pub type_id: StableId,
    pub content_type: StableId,
    pub payload: Arc<[u8]>,
    pub layout: SnapshotLayout,
}

impl EncodedSnapshot {
    pub fn new(
        content_type: StableId,
        schema_hash: StableId,
        alignment: u16,
        payload: Arc<[u8]>,
    ) -> Result<Self, HostTrap> {
        let size = u32::try_from(payload.len()).map_err(|_| HostTrap::Type)?;
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(HostTrap::Type);
        }
        Ok(Self {
            type_id: nexa_bytecode::snapshot_type(content_type),
            content_type,
            payload,
            layout: SnapshotLayout {
                size,
                alignment,
                schema_hash,
            },
        })
    }

    pub fn copy_i32_slice(
        content_type: StableId,
        schema_hash: StableId,
        values: &[i32],
    ) -> Result<Self, HostTrap> {
        let mut bytes = Vec::with_capacity(values.len().saturating_mul(std::mem::size_of::<i32>()));
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Self::new(content_type, schema_hash, 4, Arc::from(bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypedSnapshotRef<'a> {
    pub(crate) payload: &'a [u8],
    pub(crate) layout: SnapshotLayout,
}

impl<'a> TypedSnapshotRef<'a> {
    #[must_use]
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }

    #[must_use]
    pub const fn layout(self) -> SnapshotLayout {
        self.layout
    }
}

pub trait DecodeTypedSnapshot<'a>: Sized {
    const TYPE_ID: StableId;
    const CONTENT_TYPE: StableId;
    const SCHEMA_HASH: StableId;
    const ALIGNMENT: u16;

    fn decode(view: TypedSnapshotRef<'a>) -> Result<Self, HostTrap>;
}

#[derive(Debug)]
struct SnapshotEntry {
    module_id: u32,
    epoch: u64,
    type_id: StableId,
    content_type: StableId,
    payload: Arc<[u8]>,
    layout: SnapshotLayout,
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
        encoded: EncodedSnapshot,
        releases: &mut ReleaseQueue,
    ) -> Result<SnapshotHandle, HostRequestError> {
        let release = releases.reserve(module_id, epoch)?;
        if encoded.type_id != nexa_bytecode::snapshot_type(encoded.content_type)
            || encoded.layout.size as usize != encoded.payload.len()
            || encoded.layout.alignment == 0
            || !encoded.layout.alignment.is_power_of_two()
        {
            releases.cancel_reservation(release);
            return Err(HostRequestError::InvalidState);
        }
        let external_bytes = encoded.payload.len();
        let type_id = encoded.type_id;
        match self.snapshots.try_allocate(SnapshotEntry {
            module_id,
            epoch,
            type_id,
            content_type: encoded.content_type,
            payload: encoded.payload,
            layout: encoded.layout,
            external_bytes,
            release,
        }) {
            Ok(raw) => {
                increment_epoch_count(&mut self.epoch_counts, module_id, epoch);
                Ok(SnapshotHandle { raw, type_id })
            }
            Err(error) => {
                releases.cancel_reservation(release);
                Err(error.into())
            }
        }
    }

    pub fn payload(&self, handle: SnapshotHandle) -> Result<&[u8], HostRequestError> {
        Ok(&self.resolve(handle)?.payload)
    }

    pub fn layout(&self, handle: SnapshotHandle) -> Result<SnapshotLayout, HostRequestError> {
        Ok(self.resolve(handle)?.layout)
    }

    pub fn external_bytes(&self, handle: SnapshotHandle) -> Result<usize, HostRequestError> {
        Ok(self.resolve(handle)?.external_bytes)
    }

    pub fn content_type(&self, handle: SnapshotHandle) -> Result<StableId, HostRequestError> {
        Ok(self.resolve(handle)?.content_type)
    }

    fn release(
        &mut self,
        handle: SnapshotHandle,
        releases: &mut ReleaseQueue,
    ) -> Result<(), HostRequestError> {
        self.resolve(handle)?;
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

    fn resolve(&self, handle: SnapshotHandle) -> Result<&SnapshotEntry, HostRequestError> {
        let snapshot = self.snapshots.resolve(handle.raw())?;
        if handle.type_id != snapshot.type_id {
            return Err(HostRequestError::InvalidState);
        }
        Ok(snapshot)
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
    failure_injector: crate::RuntimeFailureInjector,
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
            failure_injector: crate::RuntimeFailureInjector::default(),
        }
    }

    pub(crate) fn set_failure_injector(&mut self, injector: crate::RuntimeFailureInjector) {
        self.failure_injector = injector;
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

    pub(crate) fn complete_request(
        &self,
        request: HostRequestHandle,
        result: HostCompletionResult,
    ) -> Result<(), HostRequestError> {
        self.requests.submit_result(request, result)
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

    pub(crate) fn release_token(
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

    pub(crate) fn release_snapshot(
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

    pub fn snapshot_payload(&self, snapshot: SnapshotHandle) -> Result<&[u8], HostRequestError> {
        self.snapshots.payload(snapshot)
    }

    pub fn snapshot_layout(
        &self,
        snapshot: SnapshotHandle,
    ) -> Result<SnapshotLayout, HostRequestError> {
        self.snapshots.layout(snapshot)
    }

    pub fn snapshot_external_bytes(
        &self,
        snapshot: SnapshotHandle,
    ) -> Result<usize, HostRequestError> {
        self.snapshots.external_bytes(snapshot)
    }

    pub fn snapshot_content_type(
        &self,
        snapshot: SnapshotHandle,
    ) -> Result<StableId, HostRequestError> {
        self.snapshots.content_type(snapshot)
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
        self.fail_if_injected(crate::RuntimeFailurePoint::RequestSlot)?;
        self.fail_if_injected(crate::RuntimeFailurePoint::CompletionSlot)?;
        self.fail_if_injected(crate::RuntimeFailurePoint::ReleaseSlot)?;
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
        content_type: StableId,
        domain: RuntimeHostDomain,
    ) -> Result<ResourceTokenHandle, HostRequestError> {
        self.admit(HostAdmissionKind::ResourceToken)?;
        self.fail_if_injected(crate::RuntimeFailurePoint::ReleaseSlot)?;
        let token = self.resources.tokens.create(
            self.task,
            self.module_id,
            self.epoch,
            content_type,
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

    pub fn create_typed_snapshot(
        &mut self,
        encoded: EncodedSnapshot,
    ) -> Result<SnapshotHandle, HostRequestError> {
        self.admit(HostAdmissionKind::Snapshot)?;
        self.fail_if_injected(crate::RuntimeFailurePoint::SnapshotSlot)?;
        self.fail_if_injected(crate::RuntimeFailurePoint::ReleaseSlot)?;
        let snapshot = self.resources.snapshots.create(
            self.task,
            self.module_id,
            self.epoch,
            encoded,
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

    fn fail_if_injected(&self, point: crate::RuntimeFailurePoint) -> Result<(), HostRequestError> {
        if self
            .resources
            .failure_injector
            .trigger_with_context(point, Some(self.task), None)
        {
            Err(HostRequestError::InjectedFailure(point))
        } else {
            Ok(())
        }
    }
}

fn host_admission_error(error: HostAdmissionError) -> HostRequestError {
    match error {
        HostAdmissionError::Closing => HostRequestError::HostClosing,
        HostAdmissionError::Closed => HostRequestError::HostClosed,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.data
    }
}

impl<T> IntoIterator for CopyBuffer<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
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
            1 => ticket.fail(HostErrorPayload::Code(7)),
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
    const CAPACITY: usize = 16;

    if data.len() > 128 {
        return;
    }
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
        EncodedSnapshot, HOST_CONTRACT_SCHEMA_VERSION, HostAdmissionError, HostAdmissionKind,
        HostContract, HostErrorPayload, HostPayload, HostRequestError, HostRequestHandle,
        HostRequestManager, HostReturnRequirements, ReleaseKind, ReleaseQueue, ReleaseQueueError,
        ReleaseQueueState, ResourceTokenManager, RuntimeHost, RuntimeHostArgs,
        RuntimeHostCloseError, RuntimeHostDomain, RuntimeHostState, ScriptOutputReader,
        SnapshotLayout, SnapshotManager, contract_runtime_id_from_fingerprint,
    };
    use crate::{
        GcRoots, Heap, RuntimeFailurePoint, RuntimeLimits, RuntimeValue, StableId, TaskRuntime,
        ValueType,
    };

    fn encode_three_i32(heap: &mut Heap) -> Result<RuntimeValue, super::HostTrap> {
        let requirements = HostReturnRequirements {
            object_slots: 1,
            collection_elements: 3,
            ..HostReturnRequirements::ZERO
        };
        let mut transaction =
            RuntimeHostArgs::new(&[], Some(heap))?.return_transaction(requirements)?;
        let type_id = nexa_bytecode::array_type(ValueType::I32);
        let mut array = transaction.begin_array(type_id, ValueType::I32, 3)?;
        for value in [1, 2, 3] {
            transaction.push_array_value(&mut array, RuntimeValue::I32(value))?;
        }
        let value = transaction.finish_array(array)?;
        transaction.commit(value)
    }

    #[test]
    fn host_contract_v2_preserves_source_descriptor_and_full_fingerprint() {
        let fingerprint = [
            0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17,
            18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        ];
        let runtime_id = contract_runtime_id_from_fingerprint(fingerprint);
        let contract = HostContract::new(
            "game.host",
            "contract GameHost {}",
            b"descriptor-v2",
            fingerprint,
            runtime_id,
            HOST_CONTRACT_SCHEMA_VERSION,
        );
        assert_eq!(contract.contract_name(), "game.host");
        assert_eq!(contract.source(), "contract GameHost {}");
        assert_eq!(contract.canonical_descriptor(), b"descriptor-v2");
        assert_eq!(contract.contract_fingerprint(), fingerprint);
        assert_eq!(contract.contract_runtime_id(), runtime_id);
        assert_eq!(
            contract.generator_schema_version(),
            HOST_CONTRACT_SCHEMA_VERSION
        );
        assert_eq!(runtime_id, StableId(0xfedc_ba98_7654_3210));
    }

    #[test]
    #[should_panic(expected = "generator schema version")]
    fn host_contract_rejects_non_v2_generator_schema() {
        let fingerprint = [0_u8; 32];
        let _ = HostContract::new(
            "game.host",
            "",
            b"",
            fingerprint,
            contract_runtime_id_from_fingerprint(fingerprint),
            HOST_CONTRACT_SCHEMA_VERSION - 1,
        );
    }

    #[test]
    fn host_return_transaction_is_atomic_and_reuses_collection_arena() {
        let mut heap = Heap::new_with_arena_limits(4, 64, 8, 8, 5);
        let initial = heap.collection_inspection();
        let _collection_probe = heap
            .failure_injector()
            .arm_once(RuntimeFailurePoint::HostReturnCollectionWrite);
        assert_eq!(encode_three_i32(&mut heap), Err(super::HostTrap::Type));
        assert_eq!(heap.live_len(), 0);
        assert_eq!(heap.collection_inspection(), initial);
        assert_eq!(heap.vm_allocation_counters().host_codec_copy_bytes, 0);

        let _commit_probe = heap
            .failure_injector()
            .arm_once(RuntimeFailurePoint::HostReturnCommit);
        assert_eq!(encode_three_i32(&mut heap), Err(super::HostTrap::Type));
        assert_eq!(heap.live_len(), 0);
        assert_eq!(heap.collection_inspection(), initial);
        assert_eq!(heap.vm_allocation_counters().host_codec_copy_bytes, 12);

        let array = encode_three_i32(&mut heap).unwrap();
        assert_eq!(
            heap.array_values(array).unwrap().iter().collect::<Vec<_>>(),
            vec![
                RuntimeValue::I32(1),
                RuntimeValue::I32(2),
                RuntimeValue::I32(3)
            ]
        );
        assert_eq!(heap.vm_allocation_counters().host_codec_copy_bytes, 24);
        let RuntimeValue::NamedRef { reference, .. } = array else {
            unreachable!()
        };
        let stats = heap.collect(&GcRoots::default()).unwrap();
        assert_eq!(stats.reclaimed, 1);
        assert_eq!(heap.collection_inspection(), initial);
        assert!(heap.resolve(reference).is_err());
        assert!(encode_three_i32(&mut heap).is_ok());
    }

    #[test]
    fn host_return_requirements_use_checked_arithmetic() {
        let string = HostReturnRequirements {
            object_slots: 1,
            string_bytes: 5,
            ..HostReturnRequirements::ZERO
        };
        let array = HostReturnRequirements {
            object_slots: 1,
            collection_elements: 3,
            ..HostReturnRequirements::ZERO
        };
        assert_eq!(
            string.checked_add(array).unwrap(),
            HostReturnRequirements {
                object_slots: 2,
                collection_elements: 3,
                string_bytes: 5,
                struct_fields: 0,
            }
        );
        assert_eq!(
            HostReturnRequirements {
                object_slots: usize::MAX,
                ..HostReturnRequirements::ZERO
            }
            .with_object(),
            Err(super::HostTrap::Type)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn complex_host_views_borrow_runtime_storage() {
        let mut heap = Heap::new(16);
        let string_reference = heap.allocate_string("Nexa界").unwrap();
        let string = RuntimeValue::String {
            reference: string_reference,
            hash: heap.string_hash(string_reference).unwrap(),
        };
        let struct_type = StableId::from_name("HostViewStruct");
        let structure = heap
            .allocate_struct(struct_type, &[RuntimeValue::I32(7), string])
            .unwrap();
        let enum_type = StableId::from_name("HostViewEnum");
        let variant = StableId::from_name("HostViewEnum::Some");
        let enumeration = heap
            .allocate_enum(enum_type, variant, 1, Some(structure))
            .unwrap();
        let array_type = nexa_bytecode::array_type(nexa_bytecode::ValueType::I32);
        let array = heap
            .allocate_array(array_type, nexa_bytecode::ValueType::I32)
            .unwrap();
        heap.array_push(array, RuntimeValue::I32(3)).unwrap();
        heap.array_push(array, RuntimeValue::I32(5)).unwrap();
        let buffer_type = nexa_bytecode::buffer_type(nexa_bytecode::ValueType::I32);
        let buffer = heap
            .allocate_buffer(
                buffer_type,
                nexa_bytecode::ValueType::I32,
                &[RuntimeValue::I32(11), RuntimeValue::I32(13)],
            )
            .unwrap();
        let class_type = StableId::from_name("HostViewClass");
        let class = heap
            .allocate_class(class_type, &[RuntimeValue::I32(17), string])
            .unwrap();
        let map_type =
            nexa_bytecode::map_type(nexa_bytecode::ValueType::I32, nexa_bytecode::ValueType::I32);
        let map = heap
            .allocate_map(
                map_type,
                nexa_bytecode::ValueType::I32,
                nexa_bytecode::ValueType::I32,
            )
            .unwrap();
        assert_eq!(
            heap.map_set(map, RuntimeValue::I32(19), RuntimeValue::I32(23)),
            Ok(crate::MapSetOutcome::Complete)
        );
        let values = [string, structure, enumeration, array, buffer, class, map];
        {
            let args = RuntimeHostArgs::new(&values, Some(&mut heap)).unwrap();

            assert_eq!(args.str_ref(0).unwrap().as_str(), "Nexa界");
            let structure = args.struct_ref(1, struct_type).unwrap();
            assert_eq!(structure.field(0).unwrap().i32().unwrap(), 7);
            assert_eq!(
                structure.field(1).unwrap().str_ref().unwrap().as_str(),
                "Nexa界"
            );
            let enumeration = args.enum_ref(2, enum_type).unwrap();
            assert_eq!(enumeration.variant(), variant);
            assert_eq!(enumeration.tag(), 1);
            assert_eq!(
                enumeration
                    .payload()
                    .unwrap()
                    .struct_ref(struct_type)
                    .unwrap()
                    .field(0)
                    .unwrap()
                    .i32()
                    .unwrap(),
                7
            );
            assert_eq!(
                args.array_ref(3, array_type).unwrap().get(1).unwrap().i32(),
                Ok(5)
            );
            assert_eq!(
                args.buffer_ref(4, buffer_type)
                    .unwrap()
                    .get(0)
                    .unwrap()
                    .i32(),
                Ok(11)
            );
            assert_eq!(
                args.class_ref(5, class_type)
                    .unwrap()
                    .field(1)
                    .unwrap()
                    .str_ref()
                    .unwrap()
                    .as_str(),
                "Nexa界"
            );
            let entry = args.map_ref(6, map_type).unwrap().entry(0).unwrap();
            assert_eq!(entry.key().i32(), Ok(19));
            assert_eq!(entry.value().i32(), Ok(23));
        }

        let reader = ScriptOutputReader::new(&heap);
        assert_eq!(
            reader
                .value(class)
                .class_ref(class_type)
                .unwrap()
                .field(0)
                .unwrap()
                .i32(),
            Ok(17)
        );
        let map = reader.value(map).map_ref(map_type).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.iter().len(), 1);
    }

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
        let content_type = nexa_core::StableId::from_name("EnemyView");
        let schema_hash = nexa_core::StableId::from_name("EnemyView::snapshot-schema");
        let snapshot = snapshots
            .create(
                task,
                0,
                1,
                EncodedSnapshot::copy_i32_slice(content_type, schema_hash, &[1, 2, 3]).unwrap(),
                &mut releases,
            )
            .unwrap();
        assert_eq!(
            snapshot.type_id(),
            nexa_bytecode::snapshot_type(content_type)
        );
        assert_eq!(snapshots.content_type(snapshot), Ok(content_type));
        assert_eq!(
            snapshots.payload(snapshot).unwrap(),
            &[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]
        );
        assert_eq!(
            snapshots.layout(snapshot).unwrap(),
            SnapshotLayout {
                size: 12,
                alignment: 4,
                schema_hash,
            }
        );
        assert_eq!(snapshots.external_bytes(snapshot).unwrap(), 12);
        let wrong_content = nexa_core::StableId::from_name("OtherView");
        let forged = super::SnapshotHandle {
            raw: snapshot.raw(),
            type_id: nexa_bytecode::snapshot_type(wrong_content),
        };
        assert_eq!(
            snapshots.payload(forged),
            Err(HostRequestError::InvalidState)
        );
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
        let content_type = StableId::from_name("RenderLease");
        let token = resources
            .create(
                task,
                0,
                1,
                content_type,
                RuntimeHostDomain::Render,
                &mut releases,
            )
            .unwrap();
        assert_eq!(token.content_type(), content_type);
        assert_eq!(
            token.token_type(),
            nexa_bytecode::resource_token_type(content_type)
        );
        let forged = super::ResourceTokenHandle {
            raw: token.raw(),
            content_type: StableId::from_name("DifferentLease"),
        };
        assert_eq!(
            resources.release(forged, &mut releases),
            Err(HostRequestError::InvalidState)
        );
        assert_eq!(resources.release(token, &mut releases), Ok(true));
        assert_eq!(
            resources.release(forged, &mut releases),
            Err(HostRequestError::InvalidState)
        );
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
        pending.ticket.fail(HostErrorPayload::Code(7)).unwrap();
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
        failure.ticket.fail(HostErrorPayload::Code(42)).unwrap();
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
            tokens.create(
                task,
                2,
                3,
                StableId::from_name("ClosingToken"),
                RuntimeHostDomain::Render,
                &mut token_releases,
            ),
            Err(HostRequestError::HostClosing)
        );
        let mut snapshot_releases = host.release_queue(1);
        let mut snapshots = SnapshotManager::new(1, 1);
        assert_eq!(
            snapshots.create(
                task,
                2,
                3,
                EncodedSnapshot::copy_i32_slice(
                    nexa_core::StableId::from_name("EnemyView"),
                    nexa_core::StableId::from_name("EnemyView::snapshot-schema"),
                    &[1],
                )
                .unwrap(),
                &mut snapshot_releases,
            ),
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
