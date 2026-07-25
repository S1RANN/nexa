use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4TaskState {
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
    Trapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4Event {
    Poll,
    FuelExhaust,
    ResumeFuel,
    ExplicitYield,
    ResumeExplicit,
    BeginRequest,
    CompleteRequest,
    BeginReload,
    RollbackReload,
    RequestCancel,
    ReloadCommitCancel,
    ReachSafepoint,
    CleanupSuccess,
    CleanupTrap,
    Complete,
    Trap,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4CancelKind {
    #[default]
    None,
    Ordinary,
    ReloadCommit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV4World {
    pub task: RealmV4TaskState,
    pub scheduler_tokens: u8,
    resources: u8,
    reload_restore: Option<RealmV4TaskState>,
    cancel: RealmV4CancelKind,
}

const CONTINUATION: u8 = 1 << 0;
const REQUEST: u8 = 1 << 1;
const RELOAD: u8 = 1 << 2;
const USER_DEFER: u8 = 1 << 3;

impl Default for RealmV4World {
    fn default() -> Self {
        Self {
            task: RealmV4TaskState::Ready,
            scheduler_tokens: 1,
            resources: CONTINUATION | USER_DEFER,
            reload_restore: None,
            cancel: RealmV4CancelKind::None,
        }
    }
}

impl RealmV4World {
    pub fn apply(&mut self, event: RealmV4Event) -> Result<(), &'static str> {
        match (self.task, event) {
            (RealmV4TaskState::Ready, RealmV4Event::Poll)
            | (RealmV4TaskState::FuelYielded, RealmV4Event::ResumeFuel)
            | (RealmV4TaskState::ExplicitYielded, RealmV4Event::ResumeExplicit) => {
                self.task = RealmV4TaskState::Running;
                self.scheduler_tokens = 0;
            }
            (RealmV4TaskState::Running, RealmV4Event::FuelExhaust) => {
                self.task = RealmV4TaskState::FuelYielded;
                self.scheduler_tokens = 1;
            }
            (RealmV4TaskState::Running, RealmV4Event::ExplicitYield) => {
                self.task = RealmV4TaskState::ExplicitYielded;
                self.scheduler_tokens = 1;
            }
            (RealmV4TaskState::Running, RealmV4Event::BeginRequest) => {
                self.task = RealmV4TaskState::Waiting;
                self.set_resource(REQUEST, true);
            }
            (RealmV4TaskState::Waiting, RealmV4Event::CompleteRequest) => {
                self.task = RealmV4TaskState::Running;
                self.set_resource(REQUEST, false);
            }
            (state, RealmV4Event::BeginReload) if is_reload_pausable(state) => {
                self.set_resource(RELOAD, true);
                self.reload_restore = Some(state);
                self.task = RealmV4TaskState::ReloadPaused;
                self.scheduler_tokens = 0;
            }
            (RealmV4TaskState::ReloadPaused, RealmV4Event::RollbackReload)
                if self.has_resource(RELOAD) =>
            {
                let restored = self.reload_restore.take().ok_or("operation rejected")?;
                self.task = restored;
                self.set_resource(RELOAD, false);
                self.scheduler_tokens = u8::from(matches!(
                    restored,
                    RealmV4TaskState::Ready
                        | RealmV4TaskState::FuelYielded
                        | RealmV4TaskState::ExplicitYielded
                ));
            }
            (state, RealmV4Event::RequestCancel) if is_cancellable(state) => {
                self.begin_cancel(RealmV4CancelKind::Ordinary);
            }
            (RealmV4TaskState::ReloadPaused, RealmV4Event::ReloadCommitCancel)
                if self.has_resource(RELOAD) =>
            {
                self.set_resource(RELOAD, false);
                self.reload_restore = None;
                self.set_resource(USER_DEFER, false);
                self.begin_cancel(RealmV4CancelKind::ReloadCommit);
            }
            (RealmV4TaskState::Cancelling, RealmV4Event::ReachSafepoint) => {
                if self.cancel == RealmV4CancelKind::Ordinary && self.has_resource(USER_DEFER) {
                    self.task = RealmV4TaskState::Cleanup;
                } else {
                    self.finish_terminal(RealmV4TaskState::Cancelled);
                }
            }
            (RealmV4TaskState::Cleanup, RealmV4Event::CleanupSuccess) => {
                self.set_resource(USER_DEFER, false);
                self.finish_terminal(RealmV4TaskState::Cancelled);
            }
            (RealmV4TaskState::Cleanup, RealmV4Event::CleanupTrap) => {
                self.set_resource(USER_DEFER, false);
                self.finish_terminal(RealmV4TaskState::Trapped);
            }
            (RealmV4TaskState::Running, RealmV4Event::Complete) => {
                self.finish_terminal(RealmV4TaskState::Completed);
            }
            (RealmV4TaskState::Running, RealmV4Event::Trap) => {
                self.finish_terminal(RealmV4TaskState::Trapped);
            }
            _ => return Err("operation rejected"),
        }
        self.check_invariants()
    }

    fn begin_cancel(&mut self, kind: RealmV4CancelKind) {
        self.task = RealmV4TaskState::Cancelling;
        self.cancel = kind;
        self.set_resource(REQUEST, false);
        self.scheduler_tokens = 0;
    }

    fn finish_terminal(&mut self, state: RealmV4TaskState) {
        self.task = state;
        self.set_resource(CONTINUATION | REQUEST | RELOAD | USER_DEFER, false);
        self.scheduler_tokens = 0;
        self.reload_restore = None;
        self.cancel = RealmV4CancelKind::None;
    }

    const fn has_resource(self, resource: u8) -> bool {
        self.resources & resource != 0
    }

    fn set_resource(&mut self, resource: u8, present: bool) {
        if present {
            self.resources |= resource;
        } else {
            self.resources &= !resource;
        }
    }

    fn check_invariants(self) -> Result<(), &'static str> {
        if matches!(
            self.task,
            RealmV4TaskState::FuelYielded | RealmV4TaskState::ExplicitYielded
        ) && !self.has_resource(CONTINUATION)
        {
            return Err("yielded task lacks continuation");
        }
        let waiting_request = self.task == RealmV4TaskState::Waiting
            || (self.task == RealmV4TaskState::ReloadPaused
                && self.reload_restore == Some(RealmV4TaskState::Waiting));
        if waiting_request != self.has_resource(REQUEST) {
            return Err("waiting task must own exactly one request");
        }
        if is_terminal(self.task)
            && (self.scheduler_tokens != 0
                || self.has_resource(REQUEST)
                || self.has_resource(CONTINUATION))
        {
            return Err("terminal task retains runtime resources");
        }
        if self.task == RealmV4TaskState::ReloadPaused && !self.has_resource(RELOAD) {
            return Err("reload-paused task lacks active transaction");
        }
        if self.cancel == RealmV4CancelKind::ReloadCommit && self.has_resource(USER_DEFER) {
            return Err("reload-commit cancellation retained user defer");
        }
        if self.task == RealmV4TaskState::Cleanup && self.cancel != RealmV4CancelKind::Ordinary {
            return Err("cleanup must belong to an ordinary cancellation");
        }
        if self.scheduler_tokens > 1 {
            return Err("task owns multiple scheduler tokens");
        }
        Ok(())
    }

    #[must_use]
    pub const fn has_continuation(self) -> bool {
        self.has_resource(CONTINUATION)
    }

    #[must_use]
    pub const fn has_request(self) -> bool {
        self.has_resource(REQUEST)
    }

    #[must_use]
    pub const fn reload_active(self) -> bool {
        self.has_resource(RELOAD)
    }

    #[must_use]
    pub const fn has_user_defer(self) -> bool {
        self.has_resource(USER_DEFER)
    }

    #[must_use]
    pub const fn cancel_kind(self) -> RealmV4CancelKind {
        self.cancel
    }

    #[must_use]
    pub const fn reload_restore(self) -> Option<RealmV4TaskState> {
        self.reload_restore
    }
}

fn is_reload_pausable(state: RealmV4TaskState) -> bool {
    matches!(
        state,
        RealmV4TaskState::Ready
            | RealmV4TaskState::Running
            | RealmV4TaskState::FuelYielded
            | RealmV4TaskState::ExplicitYielded
            | RealmV4TaskState::Waiting
    )
}

fn is_cancellable(state: RealmV4TaskState) -> bool {
    is_reload_pausable(state)
}

fn is_terminal(state: RealmV4TaskState) -> bool {
    matches!(
        state,
        RealmV4TaskState::Completed | RealmV4TaskState::Cancelled | RealmV4TaskState::Trapped
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV4Config {
    pub max_depth: usize,
    pub max_worlds: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RealmV4Report {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub reached_states: BTreeSet<RealmV4TaskState>,
    pub shortest_paths: Vec<Vec<RealmV4Event>>,
    pub failures: Vec<(String, Vec<RealmV4Event>)>,
    pub truncated: bool,
}

#[must_use]
pub fn explore_realm_v4(config: RealmV4Config) -> RealmV4Report {
    const EVENTS: [RealmV4Event; 16] = [
        RealmV4Event::Poll,
        RealmV4Event::FuelExhaust,
        RealmV4Event::ResumeFuel,
        RealmV4Event::ExplicitYield,
        RealmV4Event::ResumeExplicit,
        RealmV4Event::BeginRequest,
        RealmV4Event::CompleteRequest,
        RealmV4Event::BeginReload,
        RealmV4Event::RollbackReload,
        RealmV4Event::RequestCancel,
        RealmV4Event::ReloadCommitCancel,
        RealmV4Event::ReachSafepoint,
        RealmV4Event::CleanupSuccess,
        RealmV4Event::CleanupTrap,
        RealmV4Event::Complete,
        RealmV4Event::Trap,
    ];
    let initial = RealmV4World::default();
    let mut report = RealmV4Report::default();
    report.shortest_paths.push(Vec::new());
    let mut seen = BTreeSet::from([initial]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    while let Some((world, path)) = queue.pop_front() {
        report.reached_states.insert(world.task);
        if path.len() >= config.max_depth {
            if EVENTS.into_iter().any(|event| {
                let mut next = world;
                next.apply(event).is_ok() && !seen.contains(&next)
            }) {
                report.truncated = true;
            }
            continue;
        }
        for event in EVENTS {
            let mut next = world;
            let mut next_path = path.clone();
            next_path.push(event);
            match next.apply(event) {
                Ok(()) if !seen.contains(&next) => {
                    if seen.len() == config.max_worlds {
                        report.truncated = true;
                        break;
                    }
                    seen.insert(next);
                    report.shortest_paths.push(next_path.clone());
                    queue.push_back((next, next_path));
                }
                Ok(()) => {}
                Err("operation rejected") => report.rejected_operations += 1,
                Err(error) => report.failures.push((error.into(), next_path)),
            }
        }
    }
    report.visited_worlds = seen.len();
    for state in [
        RealmV4TaskState::Ready,
        RealmV4TaskState::Running,
        RealmV4TaskState::FuelYielded,
        RealmV4TaskState::ExplicitYielded,
        RealmV4TaskState::Waiting,
        RealmV4TaskState::ReloadPaused,
        RealmV4TaskState::Cancelling,
        RealmV4TaskState::Cleanup,
        RealmV4TaskState::Completed,
        RealmV4TaskState::Cancelled,
        RealmV4TaskState::Trapped,
    ] {
        if !report.reached_states.contains(&state) {
            report
                .failures
                .push((format!("task state {state:?} is unreachable"), Vec::new()));
        }
    }
    report
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4RoutingEvent {
    CompleteA,
    CompleteB,
    RollbackA,
    CommitA,
    ActivationFaultA,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4ReloadState {
    Reloading,
    RolledBack,
    Committed,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV4CompletionState {
    Pending,
    Buffered,
    Delivered,
    Replayed,
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV4RoutingWorld {
    pub reload: RealmV4ReloadState,
    pub completion_a: RealmV4CompletionState,
    pub completion_b: RealmV4CompletionState,
}

impl Default for RealmV4RoutingWorld {
    fn default() -> Self {
        Self {
            reload: RealmV4ReloadState::Reloading,
            completion_a: RealmV4CompletionState::Pending,
            completion_b: RealmV4CompletionState::Pending,
        }
    }
}

impl RealmV4RoutingWorld {
    pub fn apply(&mut self, event: RealmV4RoutingEvent) -> Result<(), &'static str> {
        match event {
            RealmV4RoutingEvent::CompleteA
                if self.completion_a == RealmV4CompletionState::Pending =>
            {
                self.completion_a = match self.reload {
                    RealmV4ReloadState::Reloading => RealmV4CompletionState::Buffered,
                    RealmV4ReloadState::RolledBack => RealmV4CompletionState::Delivered,
                    RealmV4ReloadState::Committed | RealmV4ReloadState::ActivationFaulted => {
                        RealmV4CompletionState::Discarded
                    }
                };
            }
            RealmV4RoutingEvent::CompleteB
                if self.completion_b == RealmV4CompletionState::Pending =>
            {
                self.completion_b = RealmV4CompletionState::Delivered;
            }
            RealmV4RoutingEvent::RollbackA if self.reload == RealmV4ReloadState::Reloading => {
                self.reload = RealmV4ReloadState::RolledBack;
                if self.completion_a == RealmV4CompletionState::Buffered {
                    self.completion_a = RealmV4CompletionState::Replayed;
                }
            }
            RealmV4RoutingEvent::CommitA if self.reload == RealmV4ReloadState::Reloading => {
                self.reload = RealmV4ReloadState::Committed;
                if self.completion_a == RealmV4CompletionState::Buffered {
                    self.completion_a = RealmV4CompletionState::Discarded;
                }
            }
            RealmV4RoutingEvent::ActivationFaultA
                if self.reload == RealmV4ReloadState::Reloading =>
            {
                self.reload = RealmV4ReloadState::ActivationFaulted;
                if self.completion_a == RealmV4CompletionState::Buffered {
                    self.completion_a = RealmV4CompletionState::Discarded;
                }
            }
            _ => return Err("operation rejected"),
        }
        self.check_invariants()
    }

    fn check_invariants(self) -> Result<(), &'static str> {
        if matches!(
            self.completion_b,
            RealmV4CompletionState::Buffered
                | RealmV4CompletionState::Replayed
                | RealmV4CompletionState::Discarded
        ) {
            return Err("module B completion was captured by module A reload");
        }
        if self.completion_a == RealmV4CompletionState::Replayed
            && self.reload != RealmV4ReloadState::RolledBack
        {
            return Err("module A completion replayed without rollback");
        }
        if self.completion_a == RealmV4CompletionState::Discarded
            && !matches!(
                self.reload,
                RealmV4ReloadState::Committed | RealmV4ReloadState::ActivationFaulted
            )
        {
            return Err("module A completion discarded before publication");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RealmV4RoutingReport {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub shortest_paths: Vec<Vec<RealmV4RoutingEvent>>,
    pub failures: Vec<(String, Vec<RealmV4RoutingEvent>)>,
    pub truncated: bool,
}

#[must_use]
pub fn explore_realm_v4_routing(config: RealmV4Config) -> RealmV4RoutingReport {
    const EVENTS: [RealmV4RoutingEvent; 5] = [
        RealmV4RoutingEvent::CompleteA,
        RealmV4RoutingEvent::CompleteB,
        RealmV4RoutingEvent::RollbackA,
        RealmV4RoutingEvent::CommitA,
        RealmV4RoutingEvent::ActivationFaultA,
    ];
    let initial = RealmV4RoutingWorld::default();
    let mut report = RealmV4RoutingReport {
        shortest_paths: vec![Vec::new()],
        ..RealmV4RoutingReport::default()
    };
    let mut seen = BTreeSet::from([initial]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    while let Some((world, path)) = queue.pop_front() {
        if path.len() >= config.max_depth {
            if EVENTS.into_iter().any(|event| {
                let mut next = world;
                next.apply(event).is_ok() && !seen.contains(&next)
            }) {
                report.truncated = true;
            }
            continue;
        }
        for event in EVENTS {
            let mut next = world;
            let mut next_path = path.clone();
            next_path.push(event);
            match next.apply(event) {
                Ok(()) if !seen.contains(&next) => {
                    if seen.len() == config.max_worlds {
                        report.truncated = true;
                        break;
                    }
                    seen.insert(next);
                    report.shortest_paths.push(next_path.clone());
                    queue.push_back((next, next_path));
                }
                Ok(()) => {}
                Err("operation rejected") => report.rejected_operations += 1,
                Err(error) => report.failures.push((error.into(), next_path)),
            }
        }
    }
    report.visited_worlds = seen.len();
    report
}
