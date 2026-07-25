use std::collections::{BTreeSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemConfig {
    pub max_scopes: usize,
    pub max_tasks: usize,
    pub max_depth: usize,
}

impl SystemConfig {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut max_scopes = None;
        let mut max_tasks = None;
        let mut max_depth = None;
        for line in source.lines().map(str::trim) {
            let words = line.split_whitespace().collect::<Vec<_>>();
            match words.as_slice() {
                ["max_scopes", value] => max_scopes = Some(parse_bound("max_scopes", value)?),
                ["max_tasks", value] => max_tasks = Some(parse_bound("max_tasks", value)?),
                ["max_depth", value] => max_depth = Some(parse_bound("max_depth", value)?),
                _ => {}
            }
        }
        Ok(Self {
            max_scopes: max_scopes.ok_or("missing max_scopes")?,
            max_tasks: max_tasks.ok_or("missing max_tasks")?,
            max_depth: max_depth.ok_or("missing max_depth")?,
        })
    }
}

fn parse_bound(name: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {name} bound `{value}`"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemEvent {
    CreateScope(usize),
    AdmitTask { task: usize, owner: usize },
    PollTask(usize),
    YieldFuel(usize),
    AwaitHost(usize),
    ResumeTask(usize),
    FinishTask(usize),
    CancelScope(usize),
    ReachSafepoint(usize),
    DestroyScope(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemScopeState {
    Active,
    Cancelling,
    Cancelled,
    Destroyed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ScopeModel {
    state: SystemScopeState,
    transient: u8,
    persistent: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemTaskState {
    Ready,
    Running,
    FuelYielded,
    Waiting,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TaskModel {
    state: SystemTaskState,
    owner: usize,
    persistent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TaskScopeWorld {
    scopes: Vec<Option<ScopeModel>>,
    tasks: Vec<Option<TaskModel>>,
}

impl TaskScopeWorld {
    #[must_use]
    pub fn new(config: SystemConfig) -> Self {
        Self {
            scopes: vec![None; config.max_scopes],
            tasks: vec![None; config.max_tasks],
        }
    }

    pub fn apply(&mut self, event: SystemEvent) -> Result<(), &'static str> {
        match event {
            SystemEvent::CreateScope(index) => {
                let slot = self.scopes.get_mut(index).ok_or("scope index")?;
                if slot.is_some() {
                    return Err("scope slot occupied");
                }
                *slot = Some(ScopeModel {
                    state: SystemScopeState::Active,
                    transient: 0,
                    persistent: 0,
                });
            }
            SystemEvent::AdmitTask { task, owner } => {
                let scope = self.scope(owner)?;
                if scope.state != SystemScopeState::Active {
                    return Err("scope rejects admission");
                }
                if self.tasks.get(task).ok_or("task index")?.is_some() {
                    return Err("task slot occupied");
                }
                let scope = self.scope_mut(owner)?;
                scope.transient = scope.transient.checked_add(1).ok_or("transient overflow")?;
                self.tasks[task] = Some(TaskModel {
                    state: SystemTaskState::Ready,
                    owner,
                    persistent: false,
                });
            }
            SystemEvent::PollTask(task) => {
                self.task_mut(task, SystemTaskState::Ready)?.state = SystemTaskState::Running;
            }
            SystemEvent::YieldFuel(task) => self.promote(task, SystemTaskState::FuelYielded)?,
            SystemEvent::AwaitHost(task) => self.promote(task, SystemTaskState::Waiting)?,
            SystemEvent::ResumeTask(task) => {
                let task = self
                    .tasks
                    .get_mut(task)
                    .and_then(Option::as_mut)
                    .ok_or("missing task")?;
                if !matches!(
                    task.state,
                    SystemTaskState::FuelYielded | SystemTaskState::Waiting
                ) {
                    return Err("task cannot resume");
                }
                task.state = SystemTaskState::Running;
            }
            SystemEvent::FinishTask(task) => self.finish_task(task)?,
            SystemEvent::CancelScope(scope) => {
                let scope = self.scope_mut(scope)?;
                if scope.state != SystemScopeState::Active {
                    return Err("scope cannot cancel");
                }
                scope.state = if scope.transient == 0 && scope.persistent == 0 {
                    SystemScopeState::Cancelled
                } else {
                    SystemScopeState::Cancelling
                };
            }
            SystemEvent::ReachSafepoint(task) => {
                let owner = self
                    .tasks
                    .get(task)
                    .and_then(Option::as_ref)
                    .ok_or("missing task")?
                    .owner;
                if self.scope(owner)?.state != SystemScopeState::Cancelling {
                    return Err("owner is not cancelling");
                }
                self.cancel_task(task)?;
                let scope = self.scope_mut(owner)?;
                if scope.transient == 0 && scope.persistent == 0 {
                    scope.state = SystemScopeState::Cancelled;
                }
            }
            SystemEvent::DestroyScope(scope) => {
                if self
                    .tasks
                    .iter()
                    .flatten()
                    .any(|task| task.owner == scope && task.state != SystemTaskState::Terminal)
                {
                    return Err("scope still owns task");
                }
                let scope = self.scope_mut(scope)?;
                if scope.state != SystemScopeState::Cancelled {
                    return Err("scope cannot destroy");
                }
                scope.state = SystemScopeState::Destroyed;
            }
        }
        self.check_invariants()
    }

    #[must_use]
    pub fn scope_snapshot(&self, index: usize) -> Option<(SystemScopeState, u8, u8)> {
        self.scopes[index].map(|scope| (scope.state, scope.transient, scope.persistent))
    }

    #[must_use]
    pub fn task_snapshot(&self, index: usize) -> Option<(SystemTaskState, usize, bool)> {
        self.tasks[index].map(|task| (task.state, task.owner, task.persistent))
    }

    fn promote(&mut self, task_index: usize, next: SystemTaskState) -> Result<(), &'static str> {
        let task = self
            .tasks
            .get(task_index)
            .and_then(Option::as_ref)
            .ok_or("missing task")?;
        if task.state != SystemTaskState::Running {
            return Err("task state mismatch");
        }
        let owner = task.owner;
        let promote = !task.persistent;
        if promote {
            let scope = self.scope_mut(owner)?;
            scope.transient = scope
                .transient
                .checked_sub(1)
                .ok_or("transient underflow")?;
            scope.persistent = scope
                .persistent
                .checked_add(1)
                .ok_or("persistent overflow")?;
        }
        let task = self.tasks[task_index].as_mut().expect("preflight task");
        task.persistent |= promote;
        task.state = next;
        Ok(())
    }

    fn finish_task(&mut self, task_index: usize) -> Result<(), &'static str> {
        let task = self
            .tasks
            .get_mut(task_index)
            .and_then(Option::as_mut)
            .ok_or("missing task")?;
        if task.state != SystemTaskState::Running {
            return Err("task state mismatch");
        }
        let owner = task.owner;
        let persistent = task.persistent;
        task.state = SystemTaskState::Terminal;
        let scope = self.scope_mut(owner)?;
        if persistent {
            scope.persistent = scope
                .persistent
                .checked_sub(1)
                .ok_or("persistent underflow")?;
        } else {
            scope.transient = scope
                .transient
                .checked_sub(1)
                .ok_or("transient underflow")?;
        }
        Ok(())
    }

    fn cancel_task(&mut self, task_index: usize) -> Result<(), &'static str> {
        let task = self
            .tasks
            .get_mut(task_index)
            .and_then(Option::as_mut)
            .ok_or("missing task")?;
        if !matches!(
            task.state,
            SystemTaskState::Running | SystemTaskState::FuelYielded | SystemTaskState::Waiting
        ) {
            return Err("task cannot cancel");
        }
        let owner = task.owner;
        let persistent = task.persistent;
        task.state = SystemTaskState::Terminal;
        let scope = self.scope_mut(owner)?;
        if persistent {
            scope.persistent = scope
                .persistent
                .checked_sub(1)
                .ok_or("persistent underflow")?;
        } else {
            scope.transient = scope
                .transient
                .checked_sub(1)
                .ok_or("transient underflow")?;
        }
        Ok(())
    }

    fn scope(&self, index: usize) -> Result<&ScopeModel, &'static str> {
        self.scopes
            .get(index)
            .and_then(Option::as_ref)
            .ok_or("missing scope")
    }

    fn scope_mut(&mut self, index: usize) -> Result<&mut ScopeModel, &'static str> {
        self.scopes
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or("missing scope")
    }

    fn task_mut(
        &mut self,
        index: usize,
        expected: SystemTaskState,
    ) -> Result<&mut TaskModel, &'static str> {
        let task = self
            .tasks
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or("missing task")?;
        if task.state != expected {
            return Err("task state mismatch");
        }
        Ok(task)
    }

    fn check_invariants(&self) -> Result<(), &'static str> {
        for (scope_index, scope) in self.scopes.iter().enumerate() {
            let transient = self
                .tasks
                .iter()
                .flatten()
                .filter(|task| {
                    task.owner == scope_index
                        && !task.persistent
                        && task.state != SystemTaskState::Terminal
                })
                .count();
            let persistent = self
                .tasks
                .iter()
                .flatten()
                .filter(|task| {
                    task.owner == scope_index
                        && task.persistent
                        && task.state != SystemTaskState::Terminal
                })
                .count();
            if let Some(scope) = scope {
                if usize::from(scope.transient) != transient
                    || usize::from(scope.persistent) != persistent
                {
                    return Err("scope membership mismatch");
                }
                if scope.state == SystemScopeState::Destroyed && transient + persistent != 0 {
                    return Err("destroyed scope owns task");
                }
            } else if transient + persistent != 0 {
                return Err("task has invalid owner");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemReport {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub failures: Vec<(String, Vec<SystemEvent>)>,
    pub truncated: bool,
    pub world_paths: Vec<Vec<SystemEvent>>,
}

#[must_use]
pub fn explore_task_scope(config: SystemConfig) -> SystemReport {
    let initial = TaskScopeWorld::new(config);
    let mut seen = BTreeSet::from([initial.clone()]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    let events = all_events(config);
    let mut report = SystemReport::default();
    report.world_paths.push(Vec::new());
    while let Some((world, path)) = queue.pop_front() {
        if path.len() >= config.max_depth {
            report.truncated = true;
            continue;
        }
        for event in &events {
            let mut next = world.clone();
            let mut next_path = path.clone();
            next_path.push(*event);
            match next.apply(*event) {
                Ok(()) if seen.insert(next.clone()) => {
                    report.world_paths.push(next_path.clone());
                    queue.push_back((next, next_path));
                }
                Ok(()) => {}
                Err(_) => report.rejected_operations += 1,
            }
        }
    }
    report.visited_worlds = seen.len();
    report
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmSystemConfig {
    pub max_depth: usize,
    pub max_requests: usize,
    pub max_tokens: usize,
}

impl RealmSystemConfig {
    pub fn parse(source: &str) -> Result<Self, String> {
        let mut max_depth = None;
        let mut max_requests = None;
        let mut max_tokens = None;
        for words in source
            .lines()
            .map(str::trim)
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
        {
            match words.as_slice() {
                ["max_depth", value] => max_depth = Some(parse_bound("max_depth", value)?),
                ["max_requests", value] => {
                    max_requests = Some(parse_bound("max_requests", value)?);
                }
                ["max_tokens", value] => max_tokens = Some(parse_bound("max_tokens", value)?),
                _ => {}
            }
        }
        Ok(Self {
            max_depth: max_depth.ok_or("missing max_depth")?,
            max_requests: max_requests.ok_or("missing max_requests")?,
            max_tokens: max_tokens.ok_or("missing max_tokens")?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RealmSystemEvent {
    SubmitRequest,
    CompleteRequest,
    CancelRequest,
    AcquireToken,
    ReleaseToken,
    BeginReload,
    CommitReload,
    RollbackReload,
    DrainReleases,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct RealmSystemSnapshot {
    pub requests: usize,
    pub tokens: usize,
    pub reservations: usize,
    pub releases: usize,
    pub reload_staging: bool,
}

impl RealmSystemSnapshot {
    pub fn apply(
        &mut self,
        event: RealmSystemEvent,
        config: RealmSystemConfig,
    ) -> Result<(), &'static str> {
        match event {
            RealmSystemEvent::SubmitRequest if self.requests < config.max_requests => {
                self.requests += 1;
                self.reservations += 1;
            }
            RealmSystemEvent::CompleteRequest | RealmSystemEvent::CancelRequest
                if self.requests > 0 =>
            {
                self.requests -= 1;
                self.reservations -= 1;
                self.releases += 1;
            }
            RealmSystemEvent::AcquireToken if self.tokens < config.max_tokens => {
                self.tokens += 1;
                self.reservations += 1;
            }
            RealmSystemEvent::ReleaseToken if self.tokens > 0 => {
                self.tokens -= 1;
                self.reservations -= 1;
                self.releases += 1;
            }
            RealmSystemEvent::BeginReload if !self.reload_staging => {
                self.reload_staging = true;
            }
            RealmSystemEvent::CommitReload | RealmSystemEvent::RollbackReload
                if self.reload_staging =>
            {
                self.reload_staging = false;
            }
            RealmSystemEvent::DrainReleases if self.releases > 0 => self.releases = 0,
            _ => return Err("operation rejected"),
        }
        if self.reservations != self.requests + self.tokens {
            return Err("reservation ownership mismatch");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RealmSystemReport {
    pub visited_worlds: usize,
    pub rejected_operations: usize,
    pub failures: Vec<(String, Vec<RealmSystemEvent>)>,
    pub truncated: bool,
}

#[must_use]
pub fn explore_realm_runtime(config: RealmSystemConfig) -> RealmSystemReport {
    const EVENTS: [RealmSystemEvent; 9] = [
        RealmSystemEvent::SubmitRequest,
        RealmSystemEvent::CompleteRequest,
        RealmSystemEvent::CancelRequest,
        RealmSystemEvent::AcquireToken,
        RealmSystemEvent::ReleaseToken,
        RealmSystemEvent::BeginReload,
        RealmSystemEvent::CommitReload,
        RealmSystemEvent::RollbackReload,
        RealmSystemEvent::DrainReleases,
    ];
    let mut report = RealmSystemReport::default();
    let initial = RealmSystemSnapshot::default();
    let mut seen = BTreeSet::from([initial]);
    let mut queue = VecDeque::from([(initial, Vec::new())]);
    while let Some((world, path)) = queue.pop_front() {
        if path.len() >= config.max_depth {
            report.truncated = true;
            continue;
        }
        for event in EVENTS {
            let mut next = world;
            let mut next_path = path.clone();
            next_path.push(event);
            match next.apply(event, config) {
                Ok(()) if seen.insert(next) => queue.push_back((next, next_path)),
                Ok(()) => {}
                Err("operation rejected") => report.rejected_operations += 1,
                Err(error) => report.failures.push((error.into(), next_path)),
            }
        }
    }
    report.visited_worlds = seen.len();
    report
}

pub fn replay_realm_runtime(
    config: RealmSystemConfig,
    events: impl IntoIterator<Item = RealmSystemEvent>,
) -> Result<RealmSystemSnapshot, &'static str> {
    let mut world = RealmSystemSnapshot::default();
    for event in events {
        world.apply(event, config)?;
    }
    Ok(world)
}

#[must_use]
pub fn all_events(config: SystemConfig) -> Vec<SystemEvent> {
    let mut events = Vec::new();
    for scope in 0..config.max_scopes {
        events.push(SystemEvent::CreateScope(scope));
        events.push(SystemEvent::CancelScope(scope));
        events.push(SystemEvent::DestroyScope(scope));
        for task in 0..config.max_tasks {
            events.push(SystemEvent::AdmitTask { task, owner: scope });
        }
    }
    for task in 0..config.max_tasks {
        events.extend([
            SystemEvent::PollTask(task),
            SystemEvent::YieldFuel(task),
            SystemEvent::AwaitHost(task),
            SystemEvent::ResumeTask(task),
            SystemEvent::FinishTask(task),
            SystemEvent::ReachSafepoint(task),
        ]);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::{RealmSystemConfig, SystemConfig, explore_realm_runtime, explore_task_scope};

    #[test]
    fn task_scope_system_explores_two_by_two_without_invariant_failure() {
        let config = SystemConfig::parse(include_str!(
            "../../../specs/systems/task_scope.system.spec"
        ))
        .unwrap();
        let report = explore_task_scope(config);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(report.visited_worlds > 100);
        assert!(report.rejected_operations > 0);
    }

    #[test]
    fn realm_runtime_composite_model_preserves_cross_machine_reservations() {
        let config = RealmSystemConfig::parse(include_str!(
            "../../../specs/systems/realm_runtime.system.spec"
        ))
        .unwrap();
        let report = explore_realm_runtime(config);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(report.visited_worlds >= 8);
    }
}
