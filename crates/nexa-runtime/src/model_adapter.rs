//! Crate-internal adapter surface for the current bounded Realm model.
//!
//! This module intentionally exposes model snapshots only behind the `model-adapter`
//! feature. It does not reopen the product's raw task API.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeTaskLifecycle {
    #[default]
    Vacant,
    Ready,
    Waiting,
    Terminal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeRequestLifecycle {
    #[default]
    Vacant,
    Pending,
    Detached,
    Completed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeReloadLifecycle {
    #[default]
    Idle,
    Staging,
    Active,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeRealmEvent {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeRealmSnapshot {
    pub task: RuntimeTaskLifecycle,
    pub request: RuntimeRequestLifecycle,
    pub reload: RuntimeReloadLifecycle,
    pub epoch: u64,
    pub task_resources: u8,
    pub request_resources: u8,
    pub cancelled_tasks: u64,
    pub detached_requests: u64,
    pub late_completions_discarded: u64,
    pub publications: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeRealmRejection {
    InvalidTaskState,
    InvalidRequestState,
    InvalidReloadState,
    RealmDropped,
}

#[derive(Clone, Debug, Default)]
pub struct RealmRuntimeModelAdapter {
    snapshot: RuntimeRealmSnapshot,
    dropped: bool,
}

impl RealmRuntimeModelAdapter {
    #[must_use]
    pub const fn snapshot(&self) -> RuntimeRealmSnapshot {
        self.snapshot
    }

    pub fn apply(&mut self, event: RuntimeRealmEvent) -> Result<(), RuntimeRealmRejection> {
        if self.dropped {
            return Err(RuntimeRealmRejection::RealmDropped);
        }
        match event {
            RuntimeRealmEvent::Spawn
                if self.snapshot.task == RuntimeTaskLifecycle::Vacant
                    && self.snapshot.reload == RuntimeReloadLifecycle::Idle =>
            {
                self.snapshot.task = RuntimeTaskLifecycle::Ready;
                self.snapshot.task_resources = 1;
            }
            RuntimeRealmEvent::Poll if self.snapshot.task == RuntimeTaskLifecycle::Ready => {
                self.snapshot.task = RuntimeTaskLifecycle::Waiting;
                self.snapshot.request = RuntimeRequestLifecycle::Pending;
                self.snapshot.request_resources = 1;
            }
            RuntimeRealmEvent::CompleteRequest
                if self.snapshot.request == RuntimeRequestLifecycle::Pending
                    && self.snapshot.task == RuntimeTaskLifecycle::Waiting =>
            {
                self.snapshot.request = RuntimeRequestLifecycle::Completed;
                self.snapshot.request_resources = 0;
                self.snapshot.task = RuntimeTaskLifecycle::Terminal;
                self.snapshot.task_resources = 0;
            }
            RuntimeRealmEvent::Cancel if self.snapshot.task != RuntimeTaskLifecycle::Terminal => {
                self.detach_request();
                if self.snapshot.task != RuntimeTaskLifecycle::Vacant {
                    self.snapshot.cancelled_tasks += 1;
                }
                self.snapshot.task = RuntimeTaskLifecycle::Terminal;
                self.snapshot.task_resources = 0;
            }
            RuntimeRealmEvent::RestartReload
                if self.snapshot.reload == RuntimeReloadLifecycle::Idle =>
            {
                self.restart_quiesce();
                self.snapshot.epoch += 1;
                self.snapshot.publications += 1;
                self.snapshot.reload = RuntimeReloadLifecycle::Active;
            }
            RuntimeRealmEvent::MigrationFailure
                if self.snapshot.reload == RuntimeReloadLifecycle::Idle =>
            {
                self.restart_quiesce();
                self.snapshot.reload = RuntimeReloadLifecycle::Idle;
            }
            RuntimeRealmEvent::ActivationFailure
                if self.snapshot.reload == RuntimeReloadLifecycle::Idle =>
            {
                self.restart_quiesce();
                self.snapshot.epoch += 1;
                self.snapshot.publications += 1;
                self.snapshot.reload = RuntimeReloadLifecycle::ActivationFaulted;
            }
            RuntimeRealmEvent::LateCompletion
                if self.snapshot.request == RuntimeRequestLifecycle::Detached =>
            {
                self.snapshot.late_completions_discarded += 1;
            }
            RuntimeRealmEvent::RealmDrop => {
                self.restart_quiesce();
                self.snapshot.task = RuntimeTaskLifecycle::Terminal;
                self.snapshot.task_resources = 0;
                self.snapshot.request_resources = 0;
                self.dropped = true;
            }
            RuntimeRealmEvent::Poll | RuntimeRealmEvent::Cancel => {
                return Err(RuntimeRealmRejection::InvalidTaskState);
            }
            RuntimeRealmEvent::CompleteRequest | RuntimeRealmEvent::LateCompletion => {
                return Err(RuntimeRealmRejection::InvalidRequestState);
            }
            RuntimeRealmEvent::Spawn
            | RuntimeRealmEvent::RestartReload
            | RuntimeRealmEvent::MigrationFailure
            | RuntimeRealmEvent::ActivationFailure => {
                return Err(RuntimeRealmRejection::InvalidReloadState);
            }
        }
        Ok(())
    }

    fn restart_quiesce(&mut self) {
        self.detach_request();
        if matches!(
            self.snapshot.task,
            RuntimeTaskLifecycle::Ready | RuntimeTaskLifecycle::Waiting
        ) {
            self.snapshot.cancelled_tasks += 1;
            self.snapshot.task = RuntimeTaskLifecycle::Terminal;
            self.snapshot.task_resources = 0;
        }
        self.snapshot.reload = RuntimeReloadLifecycle::Staging;
    }

    fn detach_request(&mut self) {
        if self.snapshot.request == RuntimeRequestLifecycle::Pending {
            self.snapshot.request = RuntimeRequestLifecycle::Detached;
            self.snapshot.detached_requests += 1;
            self.snapshot.request_resources = 0;
        }
    }
}
