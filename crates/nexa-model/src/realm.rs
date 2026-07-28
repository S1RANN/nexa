//! Bounded reference model for the current restart-reload Realm contract.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskLifecycle {
    #[default]
    Vacant,
    Ready,
    Waiting,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RequestLifecycle {
    #[default]
    Vacant,
    Pending,
    Detached,
    Completed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReloadLifecycle {
    #[default]
    Idle,
    Staging,
    Active,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmEvent {
    Spawn,
    Poll,
    CompleteRequest,
    Cancel,
    RestartReload,
    MigrationFailure,
    ActivationFailure,
    LateCompletion,
    RealmDrop,
}

pub const CURRENT_REALM_EVENTS: [RealmEvent; 9] = [
    RealmEvent::Spawn,
    RealmEvent::Poll,
    RealmEvent::CompleteRequest,
    RealmEvent::Cancel,
    RealmEvent::RestartReload,
    RealmEvent::MigrationFailure,
    RealmEvent::ActivationFailure,
    RealmEvent::LateCompletion,
    RealmEvent::RealmDrop,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RealmSnapshot {
    pub task: TaskLifecycle,
    pub request: RequestLifecycle,
    pub reload: ReloadLifecycle,
    pub epoch: u64,
    pub task_resources: u8,
    pub request_resources: u8,
    pub cancelled_tasks: u64,
    pub detached_requests: u64,
    pub late_completions_discarded: u64,
    pub publications: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmRejection {
    InvalidTaskState,
    InvalidRequestState,
    InvalidReloadState,
    RealmDropped,
}

#[derive(Clone, Debug, Default)]
pub struct RealmModel {
    snapshot: RealmSnapshot,
    cancelled_requests: u64,
    dropped: bool,
}

impl RealmModel {
    #[must_use]
    pub const fn snapshot(&self) -> RealmSnapshot {
        self.snapshot
    }

    pub fn apply(&mut self, event: RealmEvent) -> Result<(), RealmRejection> {
        if self.dropped {
            return Err(RealmRejection::RealmDropped);
        }
        match event {
            RealmEvent::Spawn
                if self.snapshot.task == TaskLifecycle::Vacant
                    && self.snapshot.reload != ReloadLifecycle::ActivationFaulted =>
            {
                self.snapshot.task = TaskLifecycle::Ready;
                self.snapshot.task_resources = 1;
            }
            RealmEvent::Poll if self.snapshot.task == TaskLifecycle::Ready => {
                self.snapshot.task = TaskLifecycle::Waiting;
                self.snapshot.request = RequestLifecycle::Pending;
                self.snapshot.request_resources = 1;
            }
            RealmEvent::CompleteRequest
                if self.snapshot.request == RequestLifecycle::Pending
                    && self.snapshot.task == TaskLifecycle::Waiting =>
            {
                self.snapshot.request = RequestLifecycle::Completed;
                self.snapshot.request_resources = 0;
                self.snapshot.task = TaskLifecycle::Terminal;
                self.snapshot.task_resources = 0;
            }
            RealmEvent::Cancel
                if matches!(
                    self.snapshot.task,
                    TaskLifecycle::Ready | TaskLifecycle::Waiting
                ) =>
            {
                if self.snapshot.request == RequestLifecycle::Pending {
                    self.snapshot.request = RequestLifecycle::Completed;
                    self.cancelled_requests += 1;
                    self.snapshot.detached_requests += 1;
                    self.snapshot.request_resources = 0;
                }
                if self.snapshot.task != TaskLifecycle::Vacant {
                    self.snapshot.cancelled_tasks += 1;
                }
                self.snapshot.task = TaskLifecycle::Terminal;
                self.snapshot.task_resources = 0;
            }
            RealmEvent::RestartReload if self.snapshot.reload == ReloadLifecycle::Idle => {
                self.snapshot.detached_requests = self.cancelled_requests;
                self.restart_quiesce();
                self.snapshot.epoch += 1;
                self.snapshot.publications += 1;
                self.snapshot.reload = ReloadLifecycle::Active;
            }
            RealmEvent::MigrationFailure if self.snapshot.reload == ReloadLifecycle::Idle => {
                self.snapshot.detached_requests = self.cancelled_requests;
                self.restart_quiesce();
                self.snapshot.reload = ReloadLifecycle::Idle;
            }
            RealmEvent::ActivationFailure if self.snapshot.reload == ReloadLifecycle::Idle => {
                self.snapshot.detached_requests = self.cancelled_requests;
                self.restart_quiesce();
                self.snapshot.epoch += 1;
                self.snapshot.publications += 1;
                self.snapshot.reload = ReloadLifecycle::ActivationFaulted;
            }
            RealmEvent::LateCompletion if self.snapshot.request == RequestLifecycle::Detached => {
                self.snapshot.late_completions_discarded += 1;
            }
            RealmEvent::RealmDrop => {
                self.restart_quiesce();
                if self.snapshot.task != TaskLifecycle::Vacant {
                    self.snapshot.task = TaskLifecycle::Terminal;
                }
                self.snapshot.task_resources = 0;
                self.snapshot.request_resources = 0;
                self.dropped = true;
            }
            RealmEvent::Poll | RealmEvent::Cancel => {
                return Err(RealmRejection::InvalidTaskState);
            }
            RealmEvent::CompleteRequest | RealmEvent::LateCompletion => {
                return Err(RealmRejection::InvalidRequestState);
            }
            RealmEvent::Spawn
            | RealmEvent::RestartReload
            | RealmEvent::MigrationFailure
            | RealmEvent::ActivationFailure => {
                return Err(RealmRejection::InvalidReloadState);
            }
        }
        debug_assert!(self.invariants_hold());
        Ok(())
    }

    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        let task_balanced = match self.snapshot.task {
            TaskLifecycle::Ready | TaskLifecycle::Waiting => self.snapshot.task_resources == 1,
            TaskLifecycle::Vacant | TaskLifecycle::Terminal => self.snapshot.task_resources == 0,
        };
        let request_balanced = (self.snapshot.request == RequestLifecycle::Pending)
            == (self.snapshot.request_resources == 1);
        task_balanced && request_balanced
    }

    fn restart_quiesce(&mut self) {
        if self.snapshot.request == RequestLifecycle::Pending {
            self.snapshot.request = RequestLifecycle::Detached;
            self.snapshot.detached_requests += 1;
            self.snapshot.request_resources = 0;
        }
        if matches!(
            self.snapshot.task,
            TaskLifecycle::Ready | TaskLifecycle::Waiting
        ) {
            self.snapshot.cancelled_tasks += 1;
            self.snapshot.task = TaskLifecycle::Terminal;
            self.snapshot.task_resources = 0;
        }
    }
}
