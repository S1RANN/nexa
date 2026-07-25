use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV3Event {
    CreateScope,
    SpawnTask,
    BeginHostRequest,
    HostSuccess,
    HostError,
    HostCancel,
    HostAbandon,
    BeginReload,
    RollbackPreCommit,
    PublishActivationSuccess,
    PublishActivationFailure,
    LateHostSuccess,
    DrainHostReleases,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV3TaskState {
    #[default]
    Absent,
    Ready,
    Running,
    Waiting,
    ReloadPaused,
    Completed,
    Cancelled,
    Trapped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV3RequestState {
    #[default]
    Absent,
    InFlight,
    Completed,
    Failed,
    Cancelled,
    Abandoned,
    Detached,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV3ModuleState {
    #[default]
    Absent,
    Staging,
    Active,
    ActivationFaulted,
    Retired,
    Drained,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmV3ReloadState {
    #[default]
    Idle,
    PreCommit,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmV3World {
    pub scopes: [bool; 2],
    pub modules: [RealmV3ModuleState; 2],
    pub tasks: [RealmV3TaskState; 2],
    pub task_epochs: [u8; 2],
    pub requests: [RealmV3RequestState; 2],
    pub request_epochs: [u8; 2],
    pub request_reservations: [bool; 2],
    pub resource_owner: Option<u8>,
    pub reload: RealmV3ReloadState,
    pub active_epoch: u8,
    pub completion_reservations: u8,
    pub realm_release_records: u8,
    pub host_release_records: u8,
    pub discarded_late_results: u8,
    tasks_before_reload: [RealmV3TaskState; 2],
}

impl Default for RealmV3World {
    fn default() -> Self {
        Self {
            scopes: [false; 2],
            modules: [RealmV3ModuleState::Active, RealmV3ModuleState::Absent],
            tasks: [RealmV3TaskState::Absent; 2],
            task_epochs: [0; 2],
            requests: [RealmV3RequestState::Absent; 2],
            request_epochs: [0; 2],
            request_reservations: [false; 2],
            resource_owner: None,
            reload: RealmV3ReloadState::Idle,
            active_epoch: 0,
            completion_reservations: 0,
            realm_release_records: 0,
            host_release_records: 0,
            discarded_late_results: 0,
            tasks_before_reload: [RealmV3TaskState::Absent; 2],
        }
    }
}

impl RealmV3World {
    pub fn apply(&mut self, event: RealmV3Event) -> Result<(), &'static str> {
        match event {
            RealmV3Event::CreateScope => {
                let scope = self
                    .scopes
                    .iter()
                    .position(|created| !created)
                    .ok_or("operation rejected")?;
                self.scopes[scope] = true;
            }
            RealmV3Event::SpawnTask => self.spawn_task()?,
            RealmV3Event::BeginHostRequest => self.begin_host_request()?,
            RealmV3Event::HostSuccess => {
                self.finish_host_request(
                    RealmV3RequestState::Completed,
                    RealmV3TaskState::Running,
                )?;
            }
            RealmV3Event::HostError => {
                self.finish_host_request(RealmV3RequestState::Failed, RealmV3TaskState::Trapped)?;
            }
            RealmV3Event::HostCancel => self
                .finish_host_request(RealmV3RequestState::Cancelled, RealmV3TaskState::Cancelled)?,
            RealmV3Event::HostAbandon => {
                self.finish_host_request(
                    RealmV3RequestState::Abandoned,
                    RealmV3TaskState::Trapped,
                )?;
            }
            RealmV3Event::BeginReload
                if self.reload == RealmV3ReloadState::Idle
                    && self.active_epoch == 0
                    && self.tasks.iter().any(|task| is_live_task(*task)) =>
            {
                self.reload = RealmV3ReloadState::PreCommit;
                self.modules[1] = RealmV3ModuleState::Staging;
                self.tasks_before_reload = self.tasks;
                for task in &mut self.tasks {
                    if is_live_task(*task) {
                        *task = RealmV3TaskState::ReloadPaused;
                    }
                }
            }
            RealmV3Event::RollbackPreCommit if self.reload == RealmV3ReloadState::PreCommit => {
                self.reload = RealmV3ReloadState::Idle;
                self.modules[1] = RealmV3ModuleState::Absent;
                self.tasks = self.tasks_before_reload;
                self.tasks_before_reload = [RealmV3TaskState::Absent; 2];
            }
            RealmV3Event::PublishActivationSuccess
                if self.reload == RealmV3ReloadState::PreCommit =>
            {
                self.publish(RealmV3ModuleState::Active);
                self.reload = RealmV3ReloadState::Idle;
            }
            RealmV3Event::PublishActivationFailure
                if self.reload == RealmV3ReloadState::PreCommit =>
            {
                self.publish(RealmV3ModuleState::ActivationFaulted);
                self.reload = RealmV3ReloadState::ActivationFaulted;
            }
            RealmV3Event::LateHostSuccess => {
                let request = self
                    .requests
                    .iter()
                    .enumerate()
                    .position(|(index, state)| {
                        *state == RealmV3RequestState::Detached
                            && self.request_epochs[index] == 0
                            && self.request_reservations[index]
                    })
                    .ok_or("operation rejected")?;
                self.request_reservations[request] = false;
                self.completion_reservations = 0;
                for reservation in self.request_reservations {
                    self.completion_reservations += u8::from(reservation);
                }
                self.discarded_late_results += 1;
                if self.epoch_completion_reservations(0) == 0 {
                    self.modules[0] = RealmV3ModuleState::Drained;
                }
            }
            RealmV3Event::DrainHostReleases if self.host_release_records > 0 => {
                self.host_release_records = 0;
            }
            _ => return Err("operation rejected"),
        }
        self.check_invariants()
    }

    fn spawn_task(&mut self) -> Result<(), &'static str> {
        if !self.scopes.iter().any(|created| *created)
            || self.modules[self.active_epoch as usize] != RealmV3ModuleState::Active
            || self.reload != RealmV3ReloadState::Idle
        {
            return Err("operation rejected");
        }
        let task = self
            .tasks
            .iter()
            .position(|state| *state == RealmV3TaskState::Absent)
            .ok_or("operation rejected")?;
        self.tasks[task] = RealmV3TaskState::Ready;
        self.task_epochs[task] = self.active_epoch;
        if self.resource_owner.is_none() {
            self.resource_owner = Some(u8::try_from(task).expect("Realm v3 task index fits in u8"));
        }
        Ok(())
    }

    fn begin_host_request(&mut self) -> Result<(), &'static str> {
        let task = self
            .tasks
            .iter()
            .enumerate()
            .position(|(index, state)| {
                *state == RealmV3TaskState::Ready
                    && self.requests[index] == RealmV3RequestState::Absent
            })
            .ok_or("operation rejected")?;
        self.tasks[task] = RealmV3TaskState::Waiting;
        self.requests[task] = RealmV3RequestState::InFlight;
        self.request_epochs[task] = self.task_epochs[task];
        self.request_reservations[task] = true;
        self.completion_reservations += 1;
        Ok(())
    }

    fn finish_host_request(
        &mut self,
        request_state: RealmV3RequestState,
        task_state: RealmV3TaskState,
    ) -> Result<(), &'static str> {
        let task = self
            .tasks
            .iter()
            .enumerate()
            .position(|(index, state)| {
                *state == RealmV3TaskState::Waiting
                    && self.requests[index] == RealmV3RequestState::InFlight
            })
            .ok_or("operation rejected")?;
        self.tasks[task] = task_state;
        self.requests[task] = request_state;
        self.request_reservations[task] = false;
        self.completion_reservations -= 1;
        self.host_release_records += 1;
        if matches!(
            task_state,
            RealmV3TaskState::Cancelled | RealmV3TaskState::Trapped
        ) {
            self.release_task_resources(task);
        }
        Ok(())
    }

    fn release_task_resources(&mut self, task: usize) {
        if self.resource_owner == Some(u8::try_from(task).expect("Realm v3 task index fits in u8"))
        {
            self.resource_owner = None;
            self.host_release_records += 2;
        }
    }

    fn publish(&mut self, candidate: RealmV3ModuleState) {
        self.modules[0] = if self.epoch_completion_reservations(0) == 0 {
            RealmV3ModuleState::Drained
        } else {
            RealmV3ModuleState::Retired
        };
        self.modules[1] = candidate;
        self.active_epoch = 1;
        for task in 0..self.tasks.len() {
            if self.task_epochs[task] == 0 && is_live_task(self.tasks_before_reload[task]) {
                self.tasks[task] = RealmV3TaskState::Cancelled;
                self.release_task_resources(task);
            }
            if self.request_epochs[task] == 0
                && self.requests[task] == RealmV3RequestState::InFlight
            {
                self.requests[task] = RealmV3RequestState::Detached;
                self.host_release_records += 1;
            }
        }
        self.tasks_before_reload = [RealmV3TaskState::Absent; 2];
    }

    fn epoch_completion_reservations(&self, epoch: u8) -> u8 {
        u8::try_from(
            self.request_reservations
                .iter()
                .enumerate()
                .filter(|(index, reserved)| **reserved && self.request_epochs[*index] == epoch)
                .count(),
        )
        .expect("Realm v3 request count fits in u8")
    }

    fn check_invariants(&self) -> Result<(), &'static str> {
        for task in 0..self.tasks.len() {
            if self.tasks[task] == RealmV3TaskState::Waiting
                && (self.requests[task] != RealmV3RequestState::InFlight
                    || !self.request_reservations[task])
            {
                return Err("waiting task lacks exactly one request reservation");
            }
            if matches!(
                self.tasks[task],
                RealmV3TaskState::Completed
                    | RealmV3TaskState::Cancelled
                    | RealmV3TaskState::Trapped
            ) && self.requests[task] == RealmV3RequestState::InFlight
                && self.reload != RealmV3ReloadState::ActivationFaulted
            {
                return Err("terminal task owns an in-flight request");
            }
        }
        if self.active_epoch == 1 && self.modules[0] == RealmV3ModuleState::Active {
            return Err("published root restored old module");
        }
        if self.reload == RealmV3ReloadState::ActivationFaulted
            && self.modules[1] != RealmV3ModuleState::ActivationFaulted
        {
            return Err("activation fault lost candidate root");
        }
        if self.modules[0] == RealmV3ModuleState::Drained
            && self.epoch_completion_reservations(0) != 0
        {
            return Err("drained epoch still owns a completion");
        }
        if self.resource_owner.is_some_and(|owner| {
            matches!(
                self.tasks[owner as usize],
                RealmV3TaskState::Completed
                    | RealmV3TaskState::Cancelled
                    | RealmV3TaskState::Trapped
            )
        }) {
            return Err("terminal task still owns token or snapshot");
        }
        Ok(())
    }
}

fn is_live_task(state: RealmV3TaskState) -> bool {
    matches!(
        state,
        RealmV3TaskState::Ready
            | RealmV3TaskState::Running
            | RealmV3TaskState::Waiting
            | RealmV3TaskState::ReloadPaused
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV3Config {
    pub max_depth: usize,
    pub max_worlds: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RealmV3Report {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub shortest_paths: Vec<Vec<RealmV3Event>>,
    pub failures: Vec<(String, Vec<RealmV3Event>)>,
    pub truncated: bool,
}

#[must_use]
pub fn explore_realm_v3(config: RealmV3Config) -> RealmV3Report {
    const EVENTS: [RealmV3Event; 13] = [
        RealmV3Event::CreateScope,
        RealmV3Event::SpawnTask,
        RealmV3Event::BeginHostRequest,
        RealmV3Event::HostSuccess,
        RealmV3Event::HostError,
        RealmV3Event::HostCancel,
        RealmV3Event::HostAbandon,
        RealmV3Event::BeginReload,
        RealmV3Event::RollbackPreCommit,
        RealmV3Event::PublishActivationSuccess,
        RealmV3Event::PublishActivationFailure,
        RealmV3Event::LateHostSuccess,
        RealmV3Event::DrainHostReleases,
    ];
    let initial = RealmV3World::default();
    let mut report = RealmV3Report {
        shortest_paths: vec![Vec::new()],
        ..RealmV3Report::default()
    };
    let mut seen = BTreeSet::from([initial]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    let mut depth_boundary = Vec::new();
    while let Some((world, path)) = queue.pop_front() {
        if path.len() >= config.max_depth {
            depth_boundary.push(world);
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
    report.truncated |= depth_boundary.into_iter().any(|world| {
        EVENTS.into_iter().any(|event| {
            let mut next = world;
            next.apply(event).is_ok() && !seen.contains(&next)
        })
    });
    report.visited_worlds = seen.len();
    report
}

pub fn replay_realm_v3(
    events: impl IntoIterator<Item = RealmV3Event>,
) -> Result<RealmV3World, &'static str> {
    let mut world = RealmV3World::default();
    for event in events {
        world.apply(event)?;
    }
    Ok(world)
}
