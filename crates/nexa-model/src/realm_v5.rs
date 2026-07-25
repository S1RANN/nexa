use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub const REALM_V5_TASKS: usize = 2;
pub const REALM_V5_REQUESTS: usize = 2;
pub const REALM_V5_RETIRED_EPOCHS: usize = 3;
const REALM_V5_EPOCH_SLOTS: usize = 5;
const REALM_V5_REQUESTS_U8: u8 = 2;
const REALM_V5_RETIRED_EPOCHS_U8: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5TaskState {
    Vacant,
    Ready,
    Running,
    FuelYielded,
    ExplicitYielded,
    Waiting,
    ReloadPaused,
    Cancelling,
    Cleanup,
    Completed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5RequestState {
    Vacant,
    Pending,
    Buffered,
    Completed,
    Late,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5ReloadState {
    #[default]
    Idle,
    Prepared,
    Quiesced,
    Migrated,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5RuntimeHostState {
    #[default]
    Open,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5RetiredEpoch {
    #[default]
    Vacant,
    Retired(u8),
    Drained(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5Event {
    TaskAdmission,
    PollTask,
    FuelYield,
    ExplicitYield,
    ResumeTask,
    TaskComplete,
    HostWait,
    HostComplete,
    Cancel,
    Cleanup,
    BeginReload,
    Quiesce,
    Migration,
    Rollback,
    Commit,
    ActivationFault,
    LateCompletion,
    TokenAcquire,
    TokenRelease,
    SnapshotAcquire,
    SnapshotRelease,
    ReleaseDrain,
    GcRootAttach,
    GcRootDrop,
    GcCollect,
    RetiredEpochReap(u8),
    RuntimeHostBeginClose,
    RuntimeHostFinishClose,
}

pub const REALM_V5_EVENTS: [RealmV5Event; 30] = [
    RealmV5Event::TaskAdmission,
    RealmV5Event::PollTask,
    RealmV5Event::FuelYield,
    RealmV5Event::ExplicitYield,
    RealmV5Event::ResumeTask,
    RealmV5Event::TaskComplete,
    RealmV5Event::HostWait,
    RealmV5Event::HostComplete,
    RealmV5Event::Cancel,
    RealmV5Event::Cleanup,
    RealmV5Event::BeginReload,
    RealmV5Event::Quiesce,
    RealmV5Event::Migration,
    RealmV5Event::Rollback,
    RealmV5Event::Commit,
    RealmV5Event::ActivationFault,
    RealmV5Event::LateCompletion,
    RealmV5Event::TokenAcquire,
    RealmV5Event::TokenRelease,
    RealmV5Event::SnapshotAcquire,
    RealmV5Event::SnapshotRelease,
    RealmV5Event::ReleaseDrain,
    RealmV5Event::GcRootAttach,
    RealmV5Event::GcRootDrop,
    RealmV5Event::GcCollect,
    RealmV5Event::RetiredEpochReap(0),
    RealmV5Event::RetiredEpochReap(1),
    RealmV5Event::RetiredEpochReap(2),
    RealmV5Event::RuntimeHostBeginClose,
    RealmV5Event::RuntimeHostFinishClose,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV5Rejection {
    Capacity,
    HostNotOpen,
    HostResourcesLive,
    InvalidTaskState,
    InvalidRequestState,
    InvalidReloadState,
    InvalidRetiredEpoch,
    ResourceUnavailable,
    RootUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5ApplyError {
    Rejected(RealmV5Rejection),
    Invariant(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV5Task {
    pub state: RealmV5TaskState,
    pub epoch: u8,
    pub reload_restore: Option<RealmV5TaskState>,
}

impl Default for RealmV5Task {
    fn default() -> Self {
        Self {
            state: RealmV5TaskState::Vacant,
            epoch: 0,
            reload_restore: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV5Request {
    pub state: RealmV5RequestState,
    pub task: Option<u8>,
    pub epoch: u8,
}

impl Default for RealmV5Request {
    fn default() -> Self {
        Self {
            state: RealmV5RequestState::Vacant,
            task: None,
            epoch: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV5Resource {
    pub live: bool,
    pub owner: Option<u8>,
    pub epoch: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::struct_excessive_bools)]
pub struct RealmV5World {
    pub active_epoch: u8,
    pub candidate_epoch: Option<u8>,
    pub retired_epochs: [RealmV5RetiredEpoch; REALM_V5_RETIRED_EPOCHS],
    pub tasks: [RealmV5Task; REALM_V5_TASKS],
    pub scheduler: [bool; REALM_V5_TASKS],
    pub requests: [RealmV5Request; REALM_V5_REQUESTS],
    pub token: RealmV5Resource,
    pub token_consumed: bool,
    pub snapshot: RealmV5Resource,
    pub snapshot_consumed: bool,
    pub heap_object: bool,
    pub gc_root: bool,
    pub gc_epoch: u8,
    pub gc_consumed: bool,
    pub reload: RealmV5ReloadState,
    pub reload_completion_buffer: u8,
    pub release_backlog: [u8; REALM_V5_EPOCH_SLOTS],
    pub state_registry_objects: [u8; REALM_V5_EPOCH_SLOTS],
    pub runtime_host: RealmV5RuntimeHostState,
    pub terminal_records: u8,
}

impl Default for RealmV5World {
    fn default() -> Self {
        let mut state_registry_objects = [0; REALM_V5_EPOCH_SLOTS];
        state_registry_objects[0] = 1;
        Self {
            active_epoch: 0,
            candidate_epoch: None,
            retired_epochs: [RealmV5RetiredEpoch::Vacant; REALM_V5_RETIRED_EPOCHS],
            tasks: [RealmV5Task::default(); REALM_V5_TASKS],
            scheduler: [false; REALM_V5_TASKS],
            requests: [RealmV5Request::default(); REALM_V5_REQUESTS],
            token: RealmV5Resource::default(),
            token_consumed: false,
            snapshot: RealmV5Resource::default(),
            snapshot_consumed: false,
            heap_object: false,
            gc_root: false,
            gc_epoch: 0,
            gc_consumed: false,
            reload: RealmV5ReloadState::Idle,
            reload_completion_buffer: 0,
            release_backlog: [0; REALM_V5_EPOCH_SLOTS],
            state_registry_objects,
            runtime_host: RealmV5RuntimeHostState::Open,
            terminal_records: 0,
        }
    }
}

impl RealmV5World {
    pub fn apply(&mut self, event: RealmV5Event) -> Result<(), RealmV5ApplyError> {
        let mut next = *self;
        next.apply_inner(event)
            .map_err(RealmV5ApplyError::Rejected)?;
        next.check_invariants()
            .map_err(RealmV5ApplyError::Invariant)?;
        *self = next;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn apply_inner(&mut self, event: RealmV5Event) -> Result<(), RealmV5Rejection> {
        match event {
            RealmV5Event::TaskAdmission => self.admit_task(),
            RealmV5Event::PollTask => {
                self.require_all_tasks(RealmV5TaskState::Ready)?;
                for (task, scheduled) in self.tasks.iter_mut().zip(&mut self.scheduler) {
                    task.state = RealmV5TaskState::Running;
                    *scheduled = false;
                }
                Ok(())
            }
            RealmV5Event::FuelYield => self.yield_task(RealmV5TaskState::FuelYielded),
            RealmV5Event::ExplicitYield => self.yield_task(RealmV5TaskState::ExplicitYielded),
            RealmV5Event::ResumeTask => {
                let yielded = self.tasks[0].state;
                if !matches!(
                    yielded,
                    RealmV5TaskState::FuelYielded | RealmV5TaskState::ExplicitYielded
                ) || self.tasks.iter().any(|task| task.state != yielded)
                {
                    return Err(RealmV5Rejection::InvalidTaskState);
                }
                for (task, scheduled) in self.tasks.iter_mut().zip(&mut self.scheduler) {
                    task.state = RealmV5TaskState::Running;
                    *scheduled = false;
                }
                Ok(())
            }
            RealmV5Event::TaskComplete => {
                self.require_all_tasks(RealmV5TaskState::Running)?;
                for task in 0..self.tasks.len() {
                    self.finish_task(task, RealmV5TaskState::Completed);
                }
                Ok(())
            }
            RealmV5Event::HostWait => self.begin_host_wait(),
            RealmV5Event::HostComplete => self.complete_host_request(),
            RealmV5Event::Cancel => {
                let state = self.tasks[0].state;
                if !matches!(
                    state,
                    RealmV5TaskState::Ready
                        | RealmV5TaskState::Running
                        | RealmV5TaskState::FuelYielded
                        | RealmV5TaskState::ExplicitYielded
                        | RealmV5TaskState::Waiting
                ) || self.tasks.iter().any(|task| task.state != state)
                {
                    return Err(RealmV5Rejection::InvalidTaskState);
                }
                for index in 0..self.requests.len() {
                    if self.requests[index].state == RealmV5RequestState::Pending {
                        let epoch = usize::from(self.requests[index].epoch);
                        self.release_backlog[epoch] = self.release_backlog[epoch]
                            .checked_add(1)
                            .ok_or(RealmV5Rejection::Capacity)?;
                        self.requests[index].state = RealmV5RequestState::Late;
                    }
                }
                for (task, scheduled) in self.tasks.iter_mut().zip(&mut self.scheduler) {
                    task.state = RealmV5TaskState::Cancelling;
                    *scheduled = false;
                }
                Ok(())
            }
            RealmV5Event::Cleanup => self.cleanup_task(),
            RealmV5Event::BeginReload => self.begin_reload(),
            RealmV5Event::Quiesce => self.quiesce(),
            RealmV5Event::Migration => {
                if self.reload != RealmV5ReloadState::Quiesced {
                    return Err(RealmV5Rejection::InvalidReloadState);
                }
                let candidate = usize::from(
                    self.candidate_epoch
                        .ok_or(RealmV5Rejection::InvalidReloadState)?,
                );
                self.state_registry_objects[candidate] =
                    self.state_registry_objects[usize::from(self.active_epoch)];
                self.reload = RealmV5ReloadState::Migrated;
                Ok(())
            }
            RealmV5Event::Rollback => self.rollback_reload(),
            RealmV5Event::Commit => self.publish_reload(false),
            RealmV5Event::ActivationFault => self.publish_reload(true),
            RealmV5Event::LateCompletion => {
                if self
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RequestState::Late)
                {
                    return Err(RealmV5Rejection::InvalidRequestState);
                }
                for request in &mut self.requests {
                    request.state = RealmV5RequestState::Completed;
                }
                Ok(())
            }
            RealmV5Event::TokenAcquire => self.acquire_resource(true),
            RealmV5Event::TokenRelease => self.release_resource(true),
            RealmV5Event::SnapshotAcquire => self.acquire_resource(false),
            RealmV5Event::SnapshotRelease => self.release_resource(false),
            RealmV5Event::ReleaseDrain => {
                if self.release_backlog.iter().all(|count| *count == 0) {
                    return Err(RealmV5Rejection::ResourceUnavailable);
                }
                self.release_backlog.fill(0);
                Ok(())
            }
            RealmV5Event::GcRootAttach => {
                if self.gc_root || self.gc_consumed || self.active_epoch != 0 {
                    return Err(RealmV5Rejection::RootUnavailable);
                }
                self.heap_object = true;
                self.gc_root = true;
                self.gc_epoch = self.active_epoch;
                self.gc_consumed = true;
                Ok(())
            }
            RealmV5Event::GcRootDrop => {
                if !self.gc_root {
                    return Err(RealmV5Rejection::RootUnavailable);
                }
                self.gc_root = false;
                Ok(())
            }
            RealmV5Event::GcCollect => {
                if !self.heap_object || self.gc_root {
                    return Err(RealmV5Rejection::RootUnavailable);
                }
                self.heap_object = false;
                Ok(())
            }
            RealmV5Event::RetiredEpochReap(index) => self.reap_retired(index),
            RealmV5Event::RuntimeHostBeginClose => {
                if self.runtime_host != RealmV5RuntimeHostState::Open
                    || self.active_epoch < REALM_V5_RETIRED_EPOCHS_U8
                    || self.candidate_epoch.is_some()
                {
                    return Err(RealmV5Rejection::HostNotOpen);
                }
                self.runtime_host = RealmV5RuntimeHostState::Closing;
                Ok(())
            }
            RealmV5Event::RuntimeHostFinishClose => {
                if self.runtime_host != RealmV5RuntimeHostState::Closing {
                    return Err(RealmV5Rejection::HostNotOpen);
                }
                if self.host_resources_live() {
                    return Err(RealmV5Rejection::HostResourcesLive);
                }
                self.runtime_host = RealmV5RuntimeHostState::Closed;
                Ok(())
            }
        }
    }

    fn admit_task(&mut self) -> Result<(), RealmV5Rejection> {
        if self.runtime_host != RealmV5RuntimeHostState::Open {
            return Err(RealmV5Rejection::HostNotOpen);
        }
        if !matches!(
            self.reload,
            RealmV5ReloadState::Idle | RealmV5ReloadState::ActivationFaulted
        ) {
            return Err(RealmV5Rejection::InvalidReloadState);
        }
        if self.active_epoch != 0 {
            return Err(RealmV5Rejection::Capacity);
        }
        if self
            .tasks
            .iter()
            .any(|task| task.state != RealmV5TaskState::Vacant)
        {
            return Err(RealmV5Rejection::Capacity);
        }
        for (task, scheduled) in self.tasks.iter_mut().zip(&mut self.scheduler) {
            *task = RealmV5Task {
                state: RealmV5TaskState::Ready,
                epoch: self.active_epoch,
                reload_restore: None,
            };
            *scheduled = true;
        }
        Ok(())
    }

    fn yield_task(&mut self, state: RealmV5TaskState) -> Result<(), RealmV5Rejection> {
        self.require_all_tasks(RealmV5TaskState::Running)?;
        for (task, scheduled) in self.tasks.iter_mut().zip(&mut self.scheduler) {
            task.state = state;
            *scheduled = true;
        }
        Ok(())
    }

    fn begin_host_wait(&mut self) -> Result<(), RealmV5Rejection> {
        if self.runtime_host != RealmV5RuntimeHostState::Open {
            return Err(RealmV5Rejection::HostNotOpen);
        }
        self.require_all_tasks(RealmV5TaskState::Running)?;
        if self
            .requests
            .iter()
            .any(|request| request.state != RealmV5RequestState::Vacant)
        {
            return Err(RealmV5Rejection::InvalidRequestState);
        }
        for index in 0..self.requests.len() {
            self.requests[index] = RealmV5Request {
                state: RealmV5RequestState::Pending,
                task: Some(index_u8(index)),
                epoch: self.tasks[index].epoch,
            };
            self.tasks[index].state = RealmV5TaskState::Waiting;
            self.scheduler[index] = false;
        }
        Ok(())
    }

    fn complete_host_request(&mut self) -> Result<(), RealmV5Rejection> {
        if self
            .requests
            .iter()
            .any(|request| request.state != RealmV5RequestState::Pending)
        {
            return Err(RealmV5Rejection::InvalidRequestState);
        }
        if matches!(
            self.reload,
            RealmV5ReloadState::Quiesced | RealmV5ReloadState::Migrated
        ) && self
            .requests
            .iter()
            .all(|request| request.epoch == self.active_epoch)
        {
            for request in &mut self.requests {
                request.state = RealmV5RequestState::Buffered;
            }
            self.reload_completion_buffer = REALM_V5_REQUESTS_U8;
        } else {
            for index in 0..self.requests.len() {
                self.requests[index].state = RealmV5RequestState::Completed;
                self.tasks[index].state = RealmV5TaskState::Ready;
                self.tasks[index].reload_restore = None;
                self.scheduler[index] = true;
            }
        }
        self.record_request_releases()?;
        Ok(())
    }

    fn record_request_releases(&mut self) -> Result<(), RealmV5Rejection> {
        for request in self.requests {
            let epoch = usize::from(request.epoch);
            self.release_backlog[epoch] = self.release_backlog[epoch]
                .checked_add(1)
                .ok_or(RealmV5Rejection::Capacity)?;
        }
        Ok(())
    }

    fn cleanup_task(&mut self) -> Result<(), RealmV5Rejection> {
        if self
            .tasks
            .iter()
            .all(|task| task.state == RealmV5TaskState::Cancelling)
        {
            for task in &mut self.tasks {
                task.state = RealmV5TaskState::Cleanup;
            }
            return Ok(());
        }
        self.require_all_tasks(RealmV5TaskState::Cleanup)?;
        for task in 0..self.tasks.len() {
            self.finish_task(task, RealmV5TaskState::Cancelled);
        }
        Ok(())
    }

    fn begin_reload(&mut self) -> Result<(), RealmV5Rejection> {
        if self.runtime_host == RealmV5RuntimeHostState::Closed {
            return Err(RealmV5Rejection::HostNotOpen);
        }
        if !matches!(
            self.reload,
            RealmV5ReloadState::Idle | RealmV5ReloadState::ActivationFaulted
        ) {
            return Err(RealmV5Rejection::InvalidReloadState);
        }
        let candidate = self
            .active_epoch
            .checked_add(1)
            .filter(|epoch| usize::from(*epoch) < REALM_V5_EPOCH_SLOTS)
            .ok_or(RealmV5Rejection::Capacity)?;
        self.candidate_epoch = Some(candidate);
        self.state_registry_objects[usize::from(candidate)] = 0;
        self.reload = RealmV5ReloadState::Prepared;
        Ok(())
    }

    fn quiesce(&mut self) -> Result<(), RealmV5Rejection> {
        if self.reload != RealmV5ReloadState::Prepared
            || self.active_epoch >= REALM_V5_RETIRED_EPOCHS_U8
        {
            return Err(RealmV5Rejection::InvalidReloadState);
        }
        if self.tasks.iter().any(|task| {
            task_is_live(task.state)
                && !matches!(
                    task.state,
                    RealmV5TaskState::Ready
                        | RealmV5TaskState::Running
                        | RealmV5TaskState::FuelYielded
                        | RealmV5TaskState::ExplicitYielded
                        | RealmV5TaskState::Waiting
                )
        }) {
            return Err(RealmV5Rejection::InvalidTaskState);
        }
        for (index, task) in self.tasks.iter_mut().enumerate() {
            if task.epoch == self.active_epoch && task_is_live(task.state) {
                task.reload_restore = Some(task.state);
                task.state = RealmV5TaskState::ReloadPaused;
                self.scheduler[index] = false;
            }
        }
        self.reload = RealmV5ReloadState::Quiesced;
        Ok(())
    }

    fn rollback_reload(&mut self) -> Result<(), RealmV5Rejection> {
        if !matches!(
            self.reload,
            RealmV5ReloadState::Prepared
                | RealmV5ReloadState::Quiesced
                | RealmV5ReloadState::Migrated
        ) {
            return Err(RealmV5Rejection::InvalidReloadState);
        }
        for (index, task) in self.tasks.iter_mut().enumerate() {
            if task.state == RealmV5TaskState::ReloadPaused {
                let restore = task
                    .reload_restore
                    .take()
                    .ok_or(RealmV5Rejection::InvalidTaskState)?;
                task.state = restore;
                self.scheduler[index] = task_is_scheduled(restore);
            }
        }
        for request in &mut self.requests {
            if request.state == RealmV5RequestState::Buffered {
                let owner = usize::from(request.task.ok_or(RealmV5Rejection::InvalidRequestState)?);
                request.state = RealmV5RequestState::Completed;
                self.tasks[owner].state = RealmV5TaskState::Ready;
                self.tasks[owner].reload_restore = None;
                self.scheduler[owner] = true;
            }
        }
        if let Some(candidate) = self.candidate_epoch {
            self.state_registry_objects[usize::from(candidate)] = 0;
        }
        self.candidate_epoch = None;
        self.reload_completion_buffer = 0;
        self.reload = RealmV5ReloadState::Idle;
        Ok(())
    }

    fn publish_reload(&mut self, activation_fault: bool) -> Result<(), RealmV5Rejection> {
        if self.reload != RealmV5ReloadState::Migrated {
            return Err(RealmV5Rejection::InvalidReloadState);
        }
        let retired_slot = self
            .retired_epochs
            .iter()
            .position(|epoch| !matches!(epoch, RealmV5RetiredEpoch::Retired(_)))
            .ok_or(RealmV5Rejection::Capacity)?;
        let old_epoch = self.active_epoch;
        let candidate = self
            .candidate_epoch
            .ok_or(RealmV5Rejection::InvalidReloadState)?;
        self.retired_epochs[retired_slot] = RealmV5RetiredEpoch::Retired(old_epoch);
        self.active_epoch = candidate;
        self.candidate_epoch = None;

        for task in 0..self.tasks.len() {
            if self.tasks[task].state == RealmV5TaskState::ReloadPaused {
                self.tasks[task].reload_restore = None;
                self.finish_task(task, RealmV5TaskState::Cancelled);
            }
        }
        for index in 0..self.requests.len() {
            match self.requests[index].state {
                RealmV5RequestState::Pending if self.requests[index].epoch == old_epoch => {
                    let epoch = usize::from(self.requests[index].epoch);
                    self.release_backlog[epoch] = self.release_backlog[epoch]
                        .checked_add(1)
                        .ok_or(RealmV5Rejection::Capacity)?;
                    self.requests[index].state = RealmV5RequestState::Late;
                }
                RealmV5RequestState::Buffered => {
                    self.requests[index].state = RealmV5RequestState::Completed;
                }
                _ => {}
            }
        }
        self.reload_completion_buffer = 0;
        self.reload = if activation_fault {
            RealmV5ReloadState::ActivationFaulted
        } else {
            RealmV5ReloadState::Idle
        };
        Ok(())
    }

    fn acquire_resource(&mut self, token: bool) -> Result<(), RealmV5Rejection> {
        if self.runtime_host != RealmV5RuntimeHostState::Open {
            return Err(RealmV5Rejection::HostNotOpen);
        }
        if self.release_backlog.iter().any(|count| *count != 0) {
            return Err(RealmV5Rejection::ResourceUnavailable);
        }
        self.require_all_tasks(RealmV5TaskState::Running)?;
        let task = 0;
        if (token && self.token_consumed) || (!token && self.snapshot_consumed) {
            return Err(RealmV5Rejection::ResourceUnavailable);
        }
        let resource = if token {
            &mut self.token
        } else {
            &mut self.snapshot
        };
        if resource.live {
            return Err(RealmV5Rejection::Capacity);
        }
        *resource = RealmV5Resource {
            live: true,
            owner: Some(0),
            epoch: self.tasks[task].epoch,
        };
        Ok(())
    }

    fn release_resource(&mut self, token: bool) -> Result<(), RealmV5Rejection> {
        let resource = if token {
            &mut self.token
        } else {
            &mut self.snapshot
        };
        if !resource.live {
            return Err(RealmV5Rejection::ResourceUnavailable);
        }
        let epoch = usize::from(resource.epoch);
        self.release_backlog[epoch] = self.release_backlog[epoch]
            .checked_add(1)
            .ok_or(RealmV5Rejection::Capacity)?;
        *resource = RealmV5Resource::default();
        if token {
            self.token_consumed = true;
        } else {
            self.snapshot_consumed = true;
        }
        Ok(())
    }

    fn finish_task(&mut self, task: usize, state: RealmV5TaskState) {
        if self.token.owner == Some(index_u8(task)) && self.token.live {
            let epoch = usize::from(self.token.epoch);
            self.release_backlog[epoch] = self.release_backlog[epoch].saturating_add(1);
            self.token = RealmV5Resource::default();
            self.token_consumed = true;
        }
        if self.snapshot.owner == Some(index_u8(task)) && self.snapshot.live {
            let epoch = usize::from(self.snapshot.epoch);
            self.release_backlog[epoch] = self.release_backlog[epoch].saturating_add(1);
            self.snapshot = RealmV5Resource::default();
            self.snapshot_consumed = true;
        }
        self.tasks[task].state = state;
        self.tasks[task].reload_restore = None;
        self.scheduler[task] = false;
        self.terminal_records = self.terminal_records.saturating_add(1);
    }

    fn reap_retired(&mut self, index: u8) -> Result<(), RealmV5Rejection> {
        let slot = self
            .retired_epochs
            .get(usize::from(index))
            .ok_or(RealmV5Rejection::InvalidRetiredEpoch)?;
        let epoch = match *slot {
            RealmV5RetiredEpoch::Retired(epoch) => epoch,
            RealmV5RetiredEpoch::Vacant | RealmV5RetiredEpoch::Drained(_) => {
                return Err(RealmV5Rejection::InvalidRetiredEpoch);
            }
        };
        if self.epoch_is_blocked(epoch) {
            return Err(RealmV5Rejection::ResourceUnavailable);
        }
        self.retired_epochs[usize::from(index)] = RealmV5RetiredEpoch::Drained(epoch);
        self.state_registry_objects[usize::from(epoch)] = 0;
        Ok(())
    }

    fn epoch_is_blocked(&self, epoch: u8) -> bool {
        self.tasks
            .iter()
            .any(|task| task.epoch == epoch && task_is_live(task.state))
            || self.requests.iter().any(|request| {
                request.epoch == epoch
                    && matches!(
                        request.state,
                        RealmV5RequestState::Pending
                            | RealmV5RequestState::Buffered
                            | RealmV5RequestState::Late
                    )
            })
            || (self.token.live && self.token.epoch == epoch)
            || (self.snapshot.live && self.snapshot.epoch == epoch)
            || (self.gc_root && self.gc_epoch == epoch)
            || self.release_backlog[usize::from(epoch)] != 0
    }

    fn require_all_tasks(&self, state: RealmV5TaskState) -> Result<(), RealmV5Rejection> {
        if self.tasks.iter().all(|task| task.state == state) {
            Ok(())
        } else {
            Err(RealmV5Rejection::InvalidTaskState)
        }
    }

    fn host_resources_live(&self) -> bool {
        self.requests
            .iter()
            .any(|request| request_is_live(request.state))
            || self.token.live
            || self.snapshot.live
            || self.release_backlog.iter().any(|count| *count != 0)
    }

    fn check_invariants(&self) -> Result<(), &'static str> {
        let candidate_expected = matches!(
            self.reload,
            RealmV5ReloadState::Prepared
                | RealmV5ReloadState::Quiesced
                | RealmV5ReloadState::Migrated
        );
        if candidate_expected != self.candidate_epoch.is_some() {
            return Err("reload and candidate epoch disagree");
        }
        if self
            .candidate_epoch
            .is_some_and(|candidate| candidate != self.active_epoch + 1)
        {
            return Err("candidate is not the active successor");
        }
        if self.reload_completion_buffer as usize
            != self
                .requests
                .iter()
                .filter(|request| request.state == RealmV5RequestState::Buffered)
                .count()
        {
            return Err("reload completion buffer accounting mismatch");
        }
        for (index, task) in self.tasks.iter().enumerate() {
            if self.scheduler[index] != task_is_scheduled(task.state) {
                return Err("scheduler token does not match task state");
            }
            if task.state == RealmV5TaskState::ReloadPaused
                && (!matches!(
                    self.reload,
                    RealmV5ReloadState::Quiesced | RealmV5ReloadState::Migrated
                ) || task.reload_restore.is_none())
            {
                return Err("reload-paused task lacks a restore checkpoint");
            }
            let waiting = task.state == RealmV5TaskState::Waiting
                || (task.state == RealmV5TaskState::ReloadPaused
                    && task.reload_restore == Some(RealmV5TaskState::Waiting));
            let request = self.requests.iter().any(|request| {
                request.task == Some(index_u8(index))
                    && matches!(
                        request.state,
                        RealmV5RequestState::Pending | RealmV5RequestState::Buffered
                    )
            });
            if waiting != request {
                return Err("waiting task request ownership mismatch");
            }
        }
        if self.gc_root && !self.heap_object {
            return Err("GC root points at no heap object");
        }
        for resource in [self.token, self.snapshot] {
            if resource.live {
                let owner = usize::from(resource.owner.ok_or("live resource lacks owner")?);
                if owner >= self.tasks.len()
                    || self.tasks[owner].epoch != resource.epoch
                    || !task_is_live(self.tasks[owner].state)
                {
                    return Err("resource owner is not a live task in its epoch");
                }
            } else if resource.owner.is_some() {
                return Err("dead resource retains owner");
            }
        }
        let mut retired = BTreeSet::new();
        for epoch in self.retired_epochs {
            if let RealmV5RetiredEpoch::Retired(epoch) | RealmV5RetiredEpoch::Drained(epoch) = epoch
                && (epoch >= self.active_epoch || !retired.insert(epoch))
            {
                return Err("retired epoch identity is invalid");
            }
        }
        if self.runtime_host == RealmV5RuntimeHostState::Closed && self.host_resources_live() {
            return Err("closed RuntimeHost retains resources");
        }
        Ok(())
    }
}

const fn task_is_live(state: RealmV5TaskState) -> bool {
    !matches!(
        state,
        RealmV5TaskState::Vacant | RealmV5TaskState::Completed | RealmV5TaskState::Cancelled
    )
}

const fn task_is_scheduled(state: RealmV5TaskState) -> bool {
    matches!(
        state,
        RealmV5TaskState::Ready | RealmV5TaskState::FuelYielded | RealmV5TaskState::ExplicitYielded
    )
}

const fn request_is_live(state: RealmV5RequestState) -> bool {
    matches!(
        state,
        RealmV5RequestState::Pending | RealmV5RequestState::Buffered | RealmV5RequestState::Late
    )
}

fn index_u8(index: usize) -> u8 {
    u8::try_from(index).expect("Realm v5 fixed indexes fit in u8")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV5Config {
    pub max_depth: usize,
    pub max_worlds: usize,
}

impl Default for RealmV5Config {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_worlds: 32_768,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RealmV5Report {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub rejection_reasons: BTreeMap<RealmV5Rejection, usize>,
    pub accepted_events: BTreeSet<RealmV5Event>,
    pub reached_task_states: BTreeSet<RealmV5TaskState>,
    pub reached_request_states: BTreeSet<RealmV5RequestState>,
    pub reached_reload_states: BTreeSet<RealmV5ReloadState>,
    pub reached_runtime_host_states: BTreeSet<RealmV5RuntimeHostState>,
    pub reached_retired_states: BTreeSet<RealmV5RetiredEpoch>,
    pub reached_two_live_tasks: bool,
    pub reached_two_live_requests: bool,
    pub reached_three_retired_with_candidate: bool,
    pub reached_token: bool,
    pub reached_snapshot: bool,
    pub reached_gc_object: bool,
    pub reached_reload_completion_buffer: bool,
    pub reached_release_backlog: bool,
    pub shortest_paths: Vec<Vec<RealmV5Event>>,
    pub failures: Vec<(String, Vec<RealmV5Event>)>,
    pub max_shortest_path_depth: usize,
    pub truncated: bool,
}

#[must_use]
pub fn explore_realm_v5(config: RealmV5Config) -> RealmV5Report {
    let initial = RealmV5World::default();
    let mut report = RealmV5Report::default();
    report.shortest_paths.push(Vec::new());
    let mut seen = BTreeSet::from([initial]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    while let Some((world, path)) = queue.pop_front() {
        record_reached_states(world, &mut report);
        if path.len() >= config.max_depth {
            if REALM_V5_EVENTS.into_iter().any(|event| {
                let mut next = world;
                next.apply(event).is_ok() && !seen.contains(&next)
            }) {
                report.truncated = true;
            }
            continue;
        }
        for event in REALM_V5_EVENTS {
            let mut next = world;
            let mut next_path = path.clone();
            next_path.push(event);
            match next.apply(event) {
                Ok(()) if !seen.contains(&next) => {
                    report.accepted_events.insert(event);
                    if seen.len() == config.max_worlds {
                        report.truncated = true;
                        break;
                    }
                    seen.insert(next);
                    report.max_shortest_path_depth =
                        report.max_shortest_path_depth.max(next_path.len());
                    report.shortest_paths.push(next_path.clone());
                    queue.push_back((next, next_path));
                }
                Ok(()) => {
                    report.accepted_events.insert(event);
                }
                Err(RealmV5ApplyError::Rejected(reason)) => {
                    report.rejected_operations += 1;
                    *report.rejection_reasons.entry(reason).or_default() += 1;
                }
                Err(RealmV5ApplyError::Invariant(error)) => {
                    report.failures.push((error.into(), next_path));
                }
            }
        }
    }
    report.visited_worlds = seen.len();
    require_all_states(&mut report);
    report
}

fn record_reached_states(world: RealmV5World, report: &mut RealmV5Report) {
    report.reached_reload_states.insert(world.reload);
    report
        .reached_runtime_host_states
        .insert(world.runtime_host);
    for task in world.tasks {
        report.reached_task_states.insert(task.state);
    }
    for request in world.requests {
        report.reached_request_states.insert(request.state);
    }
    for epoch in world.retired_epochs {
        report.reached_retired_states.insert(epoch);
    }
    report.reached_two_live_tasks |= world.tasks.iter().all(|task| task_is_live(task.state));
    report.reached_two_live_requests |= world
        .requests
        .iter()
        .all(|request| request_is_live(request.state));
    report.reached_three_retired_with_candidate |= world.active_epoch == 3
        && world.candidate_epoch == Some(4)
        && world
            .retired_epochs
            .iter()
            .all(|epoch| matches!(epoch, RealmV5RetiredEpoch::Retired(_)));
    report.reached_token |= world.token.live;
    report.reached_snapshot |= world.snapshot.live;
    report.reached_gc_object |= world.heap_object;
    report.reached_reload_completion_buffer |= world.reload_completion_buffer != 0;
    report.reached_release_backlog |= world.release_backlog.iter().any(|count| *count != 0);
}

#[allow(clippy::too_many_lines)]
fn require_all_states(report: &mut RealmV5Report) {
    for event in REALM_V5_EVENTS {
        if !report.accepted_events.contains(&event) {
            report
                .failures
                .push((format!("event {event:?} is unreachable"), Vec::new()));
        }
    }
    for state in [
        RealmV5TaskState::Vacant,
        RealmV5TaskState::Ready,
        RealmV5TaskState::Running,
        RealmV5TaskState::FuelYielded,
        RealmV5TaskState::ExplicitYielded,
        RealmV5TaskState::Waiting,
        RealmV5TaskState::ReloadPaused,
        RealmV5TaskState::Cancelling,
        RealmV5TaskState::Cleanup,
        RealmV5TaskState::Completed,
        RealmV5TaskState::Cancelled,
    ] {
        if !report.reached_task_states.contains(&state) {
            report
                .failures
                .push((format!("task state {state:?} is unreachable"), Vec::new()));
        }
    }
    for state in [
        RealmV5RequestState::Vacant,
        RealmV5RequestState::Pending,
        RealmV5RequestState::Buffered,
        RealmV5RequestState::Completed,
        RealmV5RequestState::Late,
    ] {
        if !report.reached_request_states.contains(&state) {
            report.failures.push((
                format!("request state {state:?} is unreachable"),
                Vec::new(),
            ));
        }
    }
    for state in [
        RealmV5ReloadState::Idle,
        RealmV5ReloadState::Prepared,
        RealmV5ReloadState::Quiesced,
        RealmV5ReloadState::Migrated,
        RealmV5ReloadState::ActivationFaulted,
    ] {
        if !report.reached_reload_states.contains(&state) {
            report
                .failures
                .push((format!("reload state {state:?} is unreachable"), Vec::new()));
        }
    }
    for state in [
        RealmV5RuntimeHostState::Open,
        RealmV5RuntimeHostState::Closing,
        RealmV5RuntimeHostState::Closed,
    ] {
        if !report.reached_runtime_host_states.contains(&state) {
            report.failures.push((
                format!("RuntimeHost state {state:?} is unreachable"),
                Vec::new(),
            ));
        }
    }
    for state in [
        RealmV5RetiredEpoch::Vacant,
        RealmV5RetiredEpoch::Retired(0),
        RealmV5RetiredEpoch::Drained(0),
    ] {
        let reached = match state {
            RealmV5RetiredEpoch::Vacant => report
                .reached_retired_states
                .contains(&RealmV5RetiredEpoch::Vacant),
            RealmV5RetiredEpoch::Retired(_) => report
                .reached_retired_states
                .iter()
                .any(|state| matches!(state, RealmV5RetiredEpoch::Retired(_))),
            RealmV5RetiredEpoch::Drained(_) => report
                .reached_retired_states
                .iter()
                .any(|state| matches!(state, RealmV5RetiredEpoch::Drained(_))),
        };
        if !reached {
            report.failures.push((
                format!("retired epoch state {state:?} is unreachable"),
                Vec::new(),
            ));
        }
    }
    for (reached, component) in [
        (report.reached_two_live_tasks, "two live tasks"),
        (report.reached_two_live_requests, "two live requests"),
        (
            report.reached_three_retired_with_candidate,
            "Retired A/B/C, Active D, Candidate E",
        ),
        (report.reached_token, "resource token"),
        (report.reached_snapshot, "snapshot"),
        (report.reached_gc_object, "GC object"),
        (
            report.reached_reload_completion_buffer,
            "reload completion buffer",
        ),
        (report.reached_release_backlog, "release backlog"),
    ] {
        if !reached {
            report.failures.push((
                format!("component state `{component}` is unreachable"),
                Vec::new(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RealmV5Config, explore_realm_v5};

    #[test]
    fn realm_v5_explores_full_bounded_world_without_truncation() {
        let config = RealmV5Config {
            max_depth: 32,
            max_worlds: 32_768,
        };
        let report = explore_realm_v5(config);
        assert!(
            report.failures.is_empty(),
            "failures={:?}, visited={}, depth={}, truncated={}",
            report.failures,
            report.visited_worlds,
            report.max_shortest_path_depth,
            report.truncated
        );
        assert!(!report.truncated, "{report:#?}");
        assert_eq!(report.shortest_paths.len(), report.visited_worlds);
        assert!(report.rejected_operations > 0);
        assert!(report.max_shortest_path_depth <= config.max_depth);
    }
}
