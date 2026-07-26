use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    GcRef, HostCompletionTicket, HostPayload, HostRegistry, ModuleHandle, Object,
    PendingHostRequest, RealmConfig, RealmRuntime, ReleaseKind, ReleaseRecord, ResourceTokenHandle,
    RuntimeFailureInjector, RuntimeHost, RuntimeHostDomain, RuntimeValue, ScopeHandle,
    SnapshotHandle, StableId, StepConfig, TaskHandle, TaskLimits, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

use super::{
    REALM_V5_EPOCH_COUNT, REALM_V5_REQUEST_COUNT, REALM_V5_RETIRED_COUNT, REALM_V5_TASK_COUNT,
    RealmV5RuntimeApplyError, RealmV5RuntimeEvent, RealmV5RuntimeExecution,
    RealmV5RuntimeLedgerSnapshot, RealmV5RuntimeReloadState, RealmV5RuntimeRequestSnapshot,
    RealmV5RuntimeRequestState, RealmV5RuntimeRetiredEpoch, RealmV5RuntimeSnapshot,
    RealmV5RuntimeTaskSnapshot, RealmV5RuntimeTaskState, RoutingRegistry,
};

const REALM_V5_MODULE_COUNT: usize = 4;

#[derive(Clone, Copy)]
struct RealmV5ModuleHandles {
    epochs: [Option<ModuleHandle>; REALM_V5_EPOCH_COUNT],
}

impl RealmV5ModuleHandles {
    fn new(active: ModuleHandle) -> Self {
        let mut epochs = [None; REALM_V5_EPOCH_COUNT];
        epochs[0] = Some(active);
        Self { epochs }
    }
}

struct RealmV5Fixtures {
    scope: ScopeHandle,
    request_queue: Arc<Mutex<VecDeque<PendingHostRequest>>>,
    request_handles: [Option<crate::HostRequestHandle>; REALM_V5_REQUEST_COUNT],
    tokens: [Option<ResourceTokenHandle>; REALM_V5_TASK_COUNT],
    snapshots: [Option<SnapshotHandle>; REALM_V5_TASK_COUNT],
    gc_object: Option<GcRef>,
}

/// Realm v5's test adapter owns production runtime objects and stable test-input mappings only.
///
/// Reload, module, epoch, registry, completion-buffer, root, task, and resource lifecycle state
/// always comes from `RealmRuntime`, `RuntimeHost`, or the handles they issued.
pub struct RealmV5RuntimeAdapter {
    realm: RealmRuntime,
    host: RuntimeHost,
    modules: RealmV5ModuleHandles,
    tasks: [Option<TaskHandle>; REALM_V5_TASK_COUNT],
    requests: [Option<HostCompletionTicket>; REALM_V5_REQUEST_COUNT],
    fixtures: RealmV5Fixtures,
    failure_injector: RuntimeFailureInjector,
}

impl RealmV5RuntimeAdapter {
    #[must_use]
    pub fn new() -> Self {
        let compiled = realm_v5_modules();
        let host_hash = compiled.host_hash;
        let schema_hash = compiled.schema_hashes[0];
        let request_queue = Arc::new(Mutex::new(VecDeque::new()));
        let host = RuntimeHost::new(32);
        let mut realm = RealmRuntime::hosted(
            RealmConfig {
                realm_id: 83,
                max_modules: u32::try_from(REALM_V5_EPOCH_COUNT)
                    .expect("Realm v5 epoch count fits u32"),
                max_heap_objects: 64,
                max_host_resources: 32,
                release_capacity: 64,
                tombstone_capacity: 16,
                ..RealmConfig::default()
            },
            host.clone(),
            Box::new(RoutingRegistry {
                hash: host_hash,
                requests: Arc::clone(&request_queue),
            }) as Box<dyn HostRegistry>,
        )
        .expect("Realm v5 production RealmRuntime starts");
        let active = realm
            .load_module(compiled.modules[0].clone(), host_hash, schema_hash)
            .expect("Realm v5 production module A loads");
        let scope = realm
            .create_scope(None)
            .expect("Realm v5 production scope starts");
        Self {
            realm,
            host,
            modules: RealmV5ModuleHandles::new(active),
            tasks: [None; REALM_V5_TASK_COUNT],
            requests: std::array::from_fn(|_| None),
            fixtures: RealmV5Fixtures {
                scope,
                request_queue,
                request_handles: [None; REALM_V5_REQUEST_COUNT],
                tokens: [None; REALM_V5_TASK_COUNT],
                snapshots: [None; REALM_V5_TASK_COUNT],
                gc_object: None,
            },
            failure_injector: RuntimeFailureInjector::default(),
        }
    }

    pub fn failure_injector(&mut self) -> &mut RuntimeFailureInjector {
        &mut self.failure_injector
    }

    pub fn apply(&mut self, event: RealmV5RuntimeEvent) -> Result<(), RealmV5RuntimeApplyError> {
        let before = self
            .snapshot()
            .map_err(RealmV5RuntimeApplyError::Invariant)?;
        self.preflight(event, &before)
            .map_err(RealmV5RuntimeApplyError::Rejected)?;
        self.apply_production(event)
            .map_err(RealmV5RuntimeApplyError::Invariant)
    }

    #[allow(clippy::too_many_lines)]
    fn preflight(
        &self,
        event: RealmV5RuntimeEvent,
        snapshot: &RealmV5RuntimeSnapshot,
    ) -> Result<(), super::RealmV5RuntimeRejection> {
        use super::RealmV5RuntimeRejection as Rejection;
        let all_tasks = |state| snapshot.tasks.iter().all(|task| task.state == state);
        let live_task = |state| {
            !matches!(
                state,
                RealmV5RuntimeTaskState::Vacant
                    | RealmV5RuntimeTaskState::Completed
                    | RealmV5RuntimeTaskState::Cancelled
                    | RealmV5RuntimeTaskState::Trapped
            )
        };
        let reload_idle = matches!(
            snapshot.reload,
            RealmV5RuntimeReloadState::Idle | RealmV5RuntimeReloadState::ActivationFaulted
        );
        match event {
            RealmV5RuntimeEvent::TaskAdmission => {
                if snapshot.runtime_host != crate::RuntimeHostState::Open {
                    return Err(Rejection::HostNotOpen);
                }
                if !reload_idle {
                    return Err(Rejection::InvalidReloadState);
                }
                if snapshot.active_epoch != 0 || !all_tasks(RealmV5RuntimeTaskState::Vacant) {
                    return Err(Rejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::PollTask | RealmV5RuntimeEvent::FuelYield => {
                if !all_tasks(RealmV5RuntimeTaskState::Ready) {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::ExplicitYield => {
                if !all_tasks(RealmV5RuntimeTaskState::FuelYielded) {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::ResumeTask => {
                if !all_tasks(RealmV5RuntimeTaskState::FuelYielded)
                    && !all_tasks(RealmV5RuntimeTaskState::Running)
                {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::TaskComplete => {
                if !all_tasks(RealmV5RuntimeTaskState::Running) {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::HostWait => {
                if snapshot.runtime_host != crate::RuntimeHostState::Open {
                    return Err(Rejection::HostNotOpen);
                }
                if !all_tasks(RealmV5RuntimeTaskState::ExplicitYielded) {
                    return Err(Rejection::InvalidTaskState);
                }
                if snapshot
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Vacant)
                {
                    return Err(Rejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::HostComplete => {
                if snapshot
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Pending)
                {
                    return Err(Rejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::Cancel => {
                if !reload_idle {
                    return Err(Rejection::InvalidReloadState);
                }
                let state = snapshot.tasks[0].state;
                if !live_task(state) || !all_tasks(state) {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::Cleanup => {
                if !all_tasks(RealmV5RuntimeTaskState::Cancelled) {
                    return Err(Rejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::BeginReload => {
                if snapshot.runtime_host == crate::RuntimeHostState::Closed {
                    return Err(Rejection::HostNotOpen);
                }
                if !reload_idle {
                    return Err(Rejection::InvalidReloadState);
                }
                if snapshot.token_live
                    || snapshot.snapshot_live
                    || snapshot.release_backlog.iter().any(|count| *count != 0)
                    || snapshot.heap_object
                {
                    return Err(Rejection::ResourceUnavailable);
                }
                if usize::from(snapshot.active_epoch) + 1 >= REALM_V5_EPOCH_COUNT {
                    return Err(Rejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::Quiesce => {
                if snapshot.reload != RealmV5RuntimeReloadState::Prepared
                    || usize::from(snapshot.active_epoch) >= REALM_V5_RETIRED_COUNT
                {
                    return Err(Rejection::InvalidReloadState);
                }
            }
            RealmV5RuntimeEvent::Migration => {
                if snapshot.reload != RealmV5RuntimeReloadState::Quiesced {
                    return Err(Rejection::InvalidReloadState);
                }
            }
            RealmV5RuntimeEvent::Rollback => {
                if !matches!(
                    snapshot.reload,
                    RealmV5RuntimeReloadState::Prepared
                        | RealmV5RuntimeReloadState::Quiesced
                        | RealmV5RuntimeReloadState::Migrated
                ) {
                    return Err(Rejection::InvalidReloadState);
                }
            }
            RealmV5RuntimeEvent::Commit | RealmV5RuntimeEvent::ActivationFault => {
                if snapshot.reload != RealmV5RuntimeReloadState::Migrated {
                    return Err(Rejection::InvalidReloadState);
                }
            }
            RealmV5RuntimeEvent::LateCompletion => {
                if snapshot
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Late)
                {
                    return Err(Rejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::TokenAcquire | RealmV5RuntimeEvent::SnapshotAcquire => {
                if snapshot.runtime_host != crate::RuntimeHostState::Open {
                    return Err(Rejection::HostNotOpen);
                }
                if snapshot.release_backlog.iter().any(|count| *count != 0) {
                    return Err(Rejection::ResourceUnavailable);
                }
                if snapshot.reload != RealmV5RuntimeReloadState::Idle
                    || snapshot.tasks[0].state != RealmV5RuntimeTaskState::Ready
                {
                    return Err(Rejection::InvalidTaskState);
                }
                let already_live = if event == RealmV5RuntimeEvent::TokenAcquire {
                    snapshot.token_live
                } else {
                    snapshot.snapshot_live
                };
                if already_live {
                    return Err(Rejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::TokenRelease => {
                if !snapshot.token_live {
                    return Err(Rejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::SnapshotRelease => {
                if !snapshot.snapshot_live {
                    return Err(Rejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::ReleaseDrain => {
                if snapshot.release_backlog.iter().all(|count| *count == 0) {
                    return Err(Rejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::GcRootAttach => {
                if snapshot.gc_root
                    || snapshot.heap_object
                    || snapshot.tasks.iter().any(|task| live_task(task.state))
                    || snapshot.reload != RealmV5RuntimeReloadState::Idle
                {
                    return Err(Rejection::RootUnavailable);
                }
            }
            RealmV5RuntimeEvent::GcRootDrop => {
                if !snapshot.gc_root {
                    return Err(Rejection::RootUnavailable);
                }
            }
            RealmV5RuntimeEvent::GcCollect => {
                if !snapshot.heap_object
                    || snapshot.gc_root
                    || snapshot.tasks.iter().any(|task| live_task(task.state))
                    || snapshot.reload != RealmV5RuntimeReloadState::Idle
                {
                    return Err(Rejection::RootUnavailable);
                }
            }
            RealmV5RuntimeEvent::RetiredEpochReap(index) => {
                let Some(epoch) = snapshot.retired_epochs.get(usize::from(index)) else {
                    return Err(Rejection::InvalidRetiredEpoch);
                };
                if !matches!(epoch, RealmV5RuntimeRetiredEpoch::Retired(_)) {
                    return Err(Rejection::InvalidRetiredEpoch);
                }
                let inspection = self.realm.inspection_snapshot();
                let Some(retired) = inspection.retired_epochs.get(usize::from(index)) else {
                    return Err(Rejection::InvalidRetiredEpoch);
                };
                if retired.task_count != 0
                    || retired.request_count != 0
                    || retired.token_count != 0
                    || retired.snapshot_count != 0
                    || retired.gc_root_count != 0
                    || retired.pending_completions != 0
                {
                    return Err(Rejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::RuntimeHostBeginClose => {
                if snapshot.runtime_host != crate::RuntimeHostState::Open
                    || usize::from(snapshot.active_epoch) != REALM_V5_RETIRED_COUNT
                    || snapshot.candidate_epoch.is_some()
                    || snapshot.requests.iter().any(|request| {
                        matches!(
                            request.state,
                            RealmV5RuntimeRequestState::Pending
                                | RealmV5RuntimeRequestState::Buffered
                                | RealmV5RuntimeRequestState::Late
                        )
                    })
                    || snapshot.token_live
                    || snapshot.snapshot_live
                    || snapshot.release_backlog.iter().any(|count| *count != 0)
                {
                    return Err(Rejection::HostNotOpen);
                }
            }
            RealmV5RuntimeEvent::RuntimeHostFinishClose => {
                return Err(
                    if snapshot.runtime_host == crate::RuntimeHostState::Closing {
                        Rejection::HostResourcesLive
                    } else {
                        Rejection::HostNotOpen
                    },
                );
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn apply_production(&mut self, event: RealmV5RuntimeEvent) -> Result<(), String> {
        match event {
            RealmV5RuntimeEvent::TaskAdmission => self.admit_driver_tasks(),
            RealmV5RuntimeEvent::PollTask | RealmV5RuntimeEvent::FuelYield => self.poll_tasks(1),
            RealmV5RuntimeEvent::ExplicitYield
            | RealmV5RuntimeEvent::ResumeTask
            | RealmV5RuntimeEvent::HostWait
            | RealmV5RuntimeEvent::TaskComplete => self.poll_tasks(1_024),
            RealmV5RuntimeEvent::HostComplete => self.complete_requests(false),
            RealmV5RuntimeEvent::LateCompletion => self.complete_requests(true),
            RealmV5RuntimeEvent::Cancel | RealmV5RuntimeEvent::Cleanup => {
                for task in self.tasks.iter().flatten().copied() {
                    if self.realm.task_snapshot(task).is_ok() {
                        self.realm
                            .cancel_task(task, crate::CancelReason::OwnerDestroyed)
                            .map_err(debug)?;
                    }
                }
                Ok(())
            }
            RealmV5RuntimeEvent::BeginReload => {
                let active = self
                    .realm
                    .active_root()
                    .ok_or_else(|| "active root is missing".to_owned())?;
                let index = usize::from(normalize_epoch(
                    self.realm.module_epoch(active).map_err(debug)?,
                )?)
                .checked_add(1)
                .ok_or_else(|| "candidate index overflow".to_owned())?;
                let compiled = realm_v5_modules();
                let candidate_module = compiled
                    .modules
                    .get(index)
                    .ok_or_else(|| "no candidate module remains".to_owned())?
                    .clone();
                let candidate = self
                    .realm
                    .prepare_reload(active, candidate_module, compiled.host_hash)
                    .map_err(debug)?;
                self.modules.epochs[index] = Some(candidate);
                Ok(())
            }
            RealmV5RuntimeEvent::Quiesce => {
                self.realm.quiesce_reload().map_err(debug)?;
                Ok(())
            }
            RealmV5RuntimeEvent::Migration => {
                let candidate = self
                    .candidate_epoch()?
                    .ok_or_else(|| "candidate is missing".to_owned())?;
                let arguments = if candidate == 1 {
                    &[RuntimeValue::Bool(false)][..]
                } else {
                    &[]
                };
                self.realm.stage_reload(arguments).map_err(debug)?;
                Ok(())
            }
            RealmV5RuntimeEvent::Rollback => {
                let candidate = self.candidate_epoch()?;
                self.realm.rollback_reload().map_err(debug)?;
                if let Some(candidate) = candidate {
                    self.modules.epochs[usize::from(candidate)] = None;
                }
                Ok(())
            }
            RealmV5RuntimeEvent::Commit | RealmV5RuntimeEvent::ActivationFault => {
                let should_trap = event == RealmV5RuntimeEvent::ActivationFault;
                match self
                    .realm
                    .commit_reload(&[RuntimeValue::Bool(should_trap)], 1_024)
                {
                    Ok(_) => Ok(()),
                    Err(crate::RealmError::Reload(crate::ReloadError::Activation(_)))
                        if should_trap =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(debug(error)),
                }
            }
            RealmV5RuntimeEvent::TokenAcquire => {
                let task = self.live_task(0)?;
                self.fixtures.tokens[0] = Some(
                    self.realm
                        .create_resource_token(task, RuntimeHostDomain::VmThread)
                        .map_err(debug)?,
                );
                Ok(())
            }
            RealmV5RuntimeEvent::TokenRelease => {
                let task = self.task(0)?;
                let token = self.fixtures.tokens[0]
                    .take()
                    .ok_or_else(|| "token is missing".to_owned())?;
                self.realm
                    .release_resource_token(task, token)
                    .map_err(debug)
            }
            RealmV5RuntimeEvent::SnapshotAcquire => {
                let task = self.live_task(0)?;
                self.fixtures.snapshots[0] = Some(
                    self.realm
                        .create_snapshot(
                            task,
                            StableId::from_name("RealmV5Snapshot"),
                            Arc::from([1, 2, 3]),
                        )
                        .map_err(debug)?,
                );
                Ok(())
            }
            RealmV5RuntimeEvent::SnapshotRelease => {
                let task = self.task(0)?;
                let snapshot = self.fixtures.snapshots[0]
                    .take()
                    .ok_or_else(|| "snapshot is missing".to_owned())?;
                self.realm.release_snapshot(task, snapshot).map_err(debug)
            }
            RealmV5RuntimeEvent::ReleaseDrain => {
                self.realm
                    .tick(TickBudget {
                        max_tasks: 0,
                        frame_fuel_budget: 0,
                        collect_garbage: false,
                    })
                    .map_err(debug)?;
                let mut records = [empty_release_record(); 64];
                let drained = self.host.drain_into(&mut records);
                if drained == 0 {
                    return Err("no release record was available".into());
                }
                Ok(())
            }
            RealmV5RuntimeEvent::GcRootAttach => {
                let active = self
                    .realm
                    .active_root()
                    .ok_or_else(|| "active root is missing".to_owned())?;
                let object = self
                    .realm
                    .allocate(Object::String("realm-v5-root".into()))
                    .map_err(debug)?;
                self.realm
                    .attach_module_root(active, object)
                    .map_err(debug)?;
                self.fixtures.gc_object = Some(object);
                Ok(())
            }
            RealmV5RuntimeEvent::GcRootDrop => {
                let active = self
                    .realm
                    .active_root()
                    .ok_or_else(|| "active root is missing".to_owned())?;
                let object = self
                    .fixtures
                    .gc_object
                    .ok_or_else(|| "GC object is missing".to_owned())?;
                self.realm.drop_module_root(active, object).map_err(debug)
            }
            RealmV5RuntimeEvent::GcCollect => {
                self.realm.collect_garbage().map_err(debug)?;
                if self
                    .fixtures
                    .gc_object
                    .is_some_and(|object| self.realm.resolve_heap_object(object).is_err())
                {
                    self.fixtures.gc_object = None;
                }
                Ok(())
            }
            RealmV5RuntimeEvent::RetiredEpochReap(_) => {
                self.realm
                    .tick(TickBudget {
                        max_tasks: 0,
                        frame_fuel_budget: 0,
                        collect_garbage: false,
                    })
                    .map_err(debug)?;
                Ok(())
            }
            RealmV5RuntimeEvent::RuntimeHostBeginClose => {
                let _ = self.host.begin_close();
                Ok(())
            }
            RealmV5RuntimeEvent::RuntimeHostFinishClose => {
                self.host.try_finish_close().map_err(debug)?;
                Ok(())
            }
        }
    }

    fn admit_driver_tasks(&mut self) -> Result<(), String> {
        let active = self
            .realm
            .active_root()
            .ok_or_else(|| "active root is missing".to_owned())?;
        for index in 0..REALM_V5_TASK_COUNT {
            let task = self
                .realm
                .call(
                    active,
                    4,
                    &[RuntimeValue::I32(
                        i32::try_from(index).expect("task index fits i32"),
                    )],
                    StepConfig {
                        owner: self.fixtures.scope,
                        priority: u32::try_from(index).expect("task index fits u32"),
                        fuel_slice: 1,
                        cumulative_budget: 10_000,
                        limits: TaskLimits::default(),
                    },
                )
                .map_err(debug)?;
            self.tasks[index] = Some(task);
        }
        Ok(())
    }

    fn poll_tasks(&mut self, fuel: u64) -> Result<(), String> {
        for task in self.tasks.iter().flatten().copied() {
            if self.realm.task_snapshot(task).is_ok() {
                self.realm.poll_task(task, fuel).map_err(debug)?;
            }
        }
        self.capture_requests();
        Ok(())
    }

    fn capture_requests(&mut self) {
        let mut queue = self
            .fixtures
            .request_queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(pending) = queue.pop_front() {
            if let Some(index) = self.requests.iter().position(Option::is_none) {
                self.fixtures.request_handles[index] = Some(pending.request);
                self.requests[index] = Some(pending.ticket);
            }
        }
    }

    fn complete_requests(&mut self, late: bool) -> Result<(), String> {
        self.capture_requests();
        for ticket in &mut self.requests {
            if let Some(mut ticket) = ticket.take() {
                match ticket.complete(HostPayload::I32(7)) {
                    Ok(()) => {}
                    Err(crate::HostRequestError::AlreadyCompleted) if late => {}
                    Err(error) => return Err(debug(error)),
                }
            }
        }
        self.realm
            .tick(TickBudget {
                max_tasks: 0,
                frame_fuel_budget: 0,
                collect_garbage: false,
            })
            .map_err(debug)?;
        Ok(())
    }

    fn task(&self, index: usize) -> Result<TaskHandle, String> {
        self.tasks[index].ok_or_else(|| "task is missing".to_owned())
    }

    fn live_task(&self, index: usize) -> Result<TaskHandle, String> {
        let task = self.task(index)?;
        self.realm.task_snapshot(task).map_err(debug)?;
        Ok(task)
    }

    #[allow(clippy::too_many_lines)]
    pub fn snapshot(&self) -> Result<RealmV5RuntimeSnapshot, String> {
        let inspection = self.realm.inspection_snapshot();
        let active = inspection
            .active_root
            .as_ref()
            .ok_or_else(|| "Realm v5 production active root is missing".to_owned())?;
        let active_epoch = normalize_epoch(active.epoch)?;
        let mut state_registry_objects = [0; REALM_V5_EPOCH_COUNT];
        for module in &inspection.modules {
            let epoch = usize::from(normalize_epoch(module.epoch)?);
            state_registry_objects[epoch] = module.state_objects;
        }
        let ledger = inspection.resources;
        let tasks = std::array::from_fn(|index| self.task_snapshot(index, &inspection));
        let scheduler = std::array::from_fn(|index| {
            self.tasks[index].is_some_and(|handle| {
                inspection.tasks.iter().any(|task| {
                    task.handle == handle && task.scheduler != crate::SchedulerInspection::Detached
                })
            })
        });
        let requests = std::array::from_fn(|index| RealmV5RuntimeRequestSnapshot {
            state: self.fixtures.request_handles[index].map_or(
                RealmV5RuntimeRequestState::Vacant,
                |request| {
                    if inspection
                        .tasks
                        .iter()
                        .any(|task| task.ownership.requests.contains(&request))
                    {
                        return RealmV5RuntimeRequestState::Pending;
                    }
                    if inspection.reload.completion_buffer != 0 && self.requests[index].is_none() {
                        return RealmV5RuntimeRequestState::Buffered;
                    }
                    self.realm.request_terminal_record(request).map_or(
                        RealmV5RuntimeRequestState::Vacant,
                        |terminal| {
                            if matches!(
                                terminal.state,
                                crate::HostRequestState::Cancelled
                                    | crate::HostRequestState::Detached
                            ) && self.requests[index].is_some()
                            {
                                RealmV5RuntimeRequestState::Late
                            } else {
                                RealmV5RuntimeRequestState::Completed
                            }
                        },
                    )
                },
            ),
            task: self.fixtures.request_handles[index].and_then(|_| u8::try_from(index).ok()),
            epoch: tasks[index].epoch,
        });
        let mut retired_epochs = [RealmV5RuntimeRetiredEpoch::Vacant; REALM_V5_RETIRED_COUNT];
        for (index, retired) in self
            .realm
            .inspection_snapshot()
            .retired_epochs
            .iter()
            .take(REALM_V5_RETIRED_COUNT)
            .enumerate()
        {
            let epoch = normalize_epoch(retired.epoch)?;
            retired_epochs[index] = match retired.state {
                crate::RetiredEpochState::Retired => RealmV5RuntimeRetiredEpoch::Retired(epoch),
                crate::RetiredEpochState::Drained => RealmV5RuntimeRetiredEpoch::Drained(epoch),
            };
        }
        let mut release_backlog = [0; REALM_V5_EPOCH_COUNT];
        for retired in &inspection.retired_epochs {
            let epoch = usize::from(normalize_epoch(retired.epoch)?);
            release_backlog[epoch] = retired.pending_releases;
        }
        for release in &inspection.runtime_host_releases {
            let epoch = usize::from(normalize_epoch(release.epoch)?);
            release_backlog[epoch] = release_backlog[epoch].saturating_add(1);
        }
        let realm_accounted_releases = inspection
            .retired_epochs
            .iter()
            .map(|retired| retired.pending_releases)
            .sum::<usize>();
        let total_releases =
            count(ledger.queued_releases).saturating_add(inspection.runtime_host_releases.len());
        release_backlog[usize::from(active_epoch)] = release_backlog[usize::from(active_epoch)]
            .saturating_add(count(ledger.queued_releases).saturating_sub(realm_accounted_releases));
        let token_owner = first_owned_resource(
            &inspection,
            &self.tasks,
            &self.fixtures.tokens,
            |ownership, handle| ownership.tokens.contains(&handle),
        );
        let snapshot_owner = first_owned_resource(
            &inspection,
            &self.tasks,
            &self.fixtures.snapshots,
            |ownership, handle| ownership.snapshots.contains(&handle),
        );
        Ok(RealmV5RuntimeSnapshot {
            active_epoch,
            candidate_epoch: inspection
                .candidate_root
                .as_ref()
                .map(|candidate| normalize_epoch(candidate.epoch))
                .transpose()?,
            retired_epochs,
            tasks,
            scheduler,
            requests,
            token_live: token_owner.is_some(),
            token_owner,
            token_epoch: owned_resource_epoch(
                &inspection,
                &self.tasks,
                &self.fixtures.tokens,
                |ownership, handle| ownership.tokens.contains(&handle),
            ),
            token_consumed: false,
            snapshot_live: snapshot_owner.is_some(),
            snapshot_owner,
            snapshot_epoch: owned_resource_epoch(
                &inspection,
                &self.tasks,
                &self.fixtures.snapshots,
                |ownership, handle| ownership.snapshots.contains(&handle),
            ),
            snapshot_consumed: false,
            heap_object: inspection.heap.live_objects != 0,
            heap_objects: inspection.heap.live_objects,
            gc_root: inspection.roots.module_globals != 0,
            gc_epoch: if inspection.heap.live_objects == 0 {
                0
            } else {
                active_epoch
            },
            gc_consumed: false,
            reload: inspection_reload_state(inspection.reload.state),
            reload_completion_buffer: inspection.reload.completion_buffer,
            release_backlog,
            state_registry_objects,
            runtime_host: self.host.state(),
            terminal_records: self
                .tasks
                .iter()
                .flatten()
                .filter(|task| self.realm.terminal_record(**task).is_some())
                .count(),
            ledger: RealmV5RuntimeLedgerSnapshot {
                task_slots: count(ledger.tasks),
                continuations: count(ledger.continuations),
                scheduler_tokens: count(ledger.scheduler_tokens),
                requests: count(ledger.requests),
                completion_reservations: count(ledger.completion_reservations),
                completion_queued: count(inspection.completion_accounting.queued),
                tokens: count(ledger.tokens),
                snapshots: count(ledger.snapshots),
                release_records: total_releases,
                heap_objects: count(ledger.heap_objects),
                state_objects: count(ledger.state_objects),
                retired_epochs: count(ledger.retired_epochs),
                terminal_records: self
                    .tasks
                    .iter()
                    .flatten()
                    .filter(|task| self.realm.terminal_record(**task).is_some())
                    .count(),
            },
        })
    }

    fn task_snapshot(
        &self,
        index: usize,
        inspection: &crate::RealmInspectionSnapshot,
    ) -> RealmV5RuntimeTaskSnapshot {
        let Some(task) = self.tasks[index] else {
            return RealmV5RuntimeTaskSnapshot {
                state: RealmV5RuntimeTaskState::Vacant,
                execution: RealmV5RuntimeExecution::None,
                epoch: 0,
            };
        };
        if let Some(snapshot) = inspection
            .tasks
            .iter()
            .find(|snapshot| snapshot.handle == task)
        {
            let state = runtime_task_state(snapshot.state);
            return RealmV5RuntimeTaskSnapshot {
                state,
                execution: inspection_execution(snapshot.execution),
                epoch: normalize_epoch(snapshot.epoch).unwrap_or(0),
            };
        }
        let terminal = inspection
            .terminal_records
            .iter()
            .find(|(terminal, _)| *terminal == task);
        let state = terminal.map_or(RealmV5RuntimeTaskState::Vacant, |(_, record)| {
            runtime_task_state(record.state)
        });
        RealmV5RuntimeTaskSnapshot {
            state,
            execution: RealmV5RuntimeExecution::None,
            epoch: terminal
                .and_then(|(_, record)| normalize_epoch(record.module_epoch).ok())
                .unwrap_or(0),
        }
    }

    fn candidate_epoch(&self) -> Result<Option<u8>, String> {
        for module in self.modules.epochs.iter().flatten().copied() {
            let Ok(lifecycle) = self.realm.module_lifecycle(module) else {
                continue;
            };
            if matches!(
                lifecycle,
                crate::ModuleLifecycle::Staging | crate::ModuleLifecycle::Activating
            ) {
                return normalize_epoch(self.realm.module_epoch(module).map_err(debug)?).map(Some);
            }
        }
        Ok(None)
    }

    #[must_use]
    pub fn realm(&self) -> &RealmRuntime {
        &self.realm
    }

    pub fn realm_mut(&mut self) -> &mut RealmRuntime {
        &mut self.realm
    }

    #[must_use]
    pub fn host(&self) -> &RuntimeHost {
        &self.host
    }

    #[must_use]
    pub const fn scope(&self) -> ScopeHandle {
        self.fixtures.scope
    }

    #[must_use]
    pub fn request_queue(&self) -> &Arc<Mutex<VecDeque<PendingHostRequest>>> {
        debug_assert_eq!(
            self.requests
                .iter()
                .filter(|request| request.is_some())
                .count(),
            self.fixtures
                .request_handles
                .iter()
                .filter(|request| request.is_some())
                .count()
        );
        &self.fixtures.request_queue
    }
}

impl Default for RealmV5RuntimeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn runtime_task_state(state: crate::TaskState) -> RealmV5RuntimeTaskState {
    match state {
        crate::TaskState::Created => RealmV5RuntimeTaskState::Vacant,
        crate::TaskState::Ready => RealmV5RuntimeTaskState::Ready,
        crate::TaskState::Running => RealmV5RuntimeTaskState::Running,
        crate::TaskState::FuelYielded => RealmV5RuntimeTaskState::FuelYielded,
        crate::TaskState::ExplicitYielded => RealmV5RuntimeTaskState::ExplicitYielded,
        crate::TaskState::Waiting => RealmV5RuntimeTaskState::Waiting,
        crate::TaskState::ReloadPauseRequested | crate::TaskState::ReloadPaused => {
            RealmV5RuntimeTaskState::ReloadPaused
        }
        crate::TaskState::CancelRequested | crate::TaskState::Cancelling => {
            RealmV5RuntimeTaskState::Cancelling
        }
        crate::TaskState::Cleanup => RealmV5RuntimeTaskState::Cleanup,
        crate::TaskState::Completed => RealmV5RuntimeTaskState::Completed,
        crate::TaskState::Cancelled => RealmV5RuntimeTaskState::Cancelled,
        crate::TaskState::Trapped => RealmV5RuntimeTaskState::Trapped,
    }
}

fn normalize_epoch(epoch: u64) -> Result<u8, String> {
    let normalized = epoch
        .checked_sub(1)
        .ok_or_else(|| "Realm epoch must be positive".to_owned())?;
    u8::try_from(normalized).map_err(|_| "Realm epoch exceeds v5 fixture capacity".to_owned())
}

fn first_owned_resource<T: Copy>(
    inspection: &crate::RealmInspectionSnapshot,
    tasks: &[Option<TaskHandle>; REALM_V5_TASK_COUNT],
    resources: &[Option<T>; REALM_V5_TASK_COUNT],
    owns: impl Fn(&crate::TaskResourceSet, T) -> bool,
) -> Option<u8> {
    tasks
        .iter()
        .copied()
        .zip(resources.iter().copied())
        .enumerate()
        .find_map(|(index, (task, resource))| {
            let (Some(task), Some(resource)) = (task, resource) else {
                return None;
            };
            inspection
                .tasks
                .iter()
                .find(|snapshot| snapshot.handle == task)
                .filter(|snapshot| owns(&snapshot.ownership, resource))
                .and_then(|_| u8::try_from(index).ok())
        })
}

fn owned_resource_epoch<T: Copy>(
    inspection: &crate::RealmInspectionSnapshot,
    tasks: &[Option<TaskHandle>; REALM_V5_TASK_COUNT],
    resources: &[Option<T>; REALM_V5_TASK_COUNT],
    owns: impl Fn(&crate::TaskResourceSet, T) -> bool,
) -> u8 {
    tasks
        .iter()
        .copied()
        .zip(resources.iter().copied())
        .find_map(|(task, resource)| {
            let (Some(task), Some(resource)) = (task, resource) else {
                return None;
            };
            inspection
                .tasks
                .iter()
                .find(|snapshot| snapshot.handle == task)
                .filter(|snapshot| owns(&snapshot.ownership, resource))
                .and_then(|snapshot| normalize_epoch(snapshot.epoch).ok())
        })
        .unwrap_or(0)
}

const fn inspection_execution(
    execution: crate::TaskExecutionInspection,
) -> RealmV5RuntimeExecution {
    match execution {
        crate::TaskExecutionInspection::Ready => RealmV5RuntimeExecution::Ready,
        crate::TaskExecutionInspection::Running => RealmV5RuntimeExecution::Running,
        crate::TaskExecutionInspection::FuelYielded => RealmV5RuntimeExecution::FuelYielded,
        crate::TaskExecutionInspection::ExplicitYielded => RealmV5RuntimeExecution::ExplicitYielded,
        crate::TaskExecutionInspection::Waiting { .. } => RealmV5RuntimeExecution::Waiting,
        crate::TaskExecutionInspection::ReloadPaused => RealmV5RuntimeExecution::ReloadPaused,
        crate::TaskExecutionInspection::Cancelling => RealmV5RuntimeExecution::Cancelling,
        crate::TaskExecutionInspection::Cleanup => RealmV5RuntimeExecution::Cleanup,
    }
}

const fn inspection_reload_state(state: crate::ReloadInspectionState) -> RealmV5RuntimeReloadState {
    match state {
        crate::ReloadInspectionState::Idle | crate::ReloadInspectionState::Completed => {
            RealmV5RuntimeReloadState::Idle
        }
        crate::ReloadInspectionState::Preparing | crate::ReloadInspectionState::Quiescing => {
            RealmV5RuntimeReloadState::Prepared
        }
        crate::ReloadInspectionState::Staging => RealmV5RuntimeReloadState::Quiesced,
        crate::ReloadInspectionState::Committing
        | crate::ReloadInspectionState::Published
        | crate::ReloadInspectionState::Activating => RealmV5RuntimeReloadState::Migrated,
        crate::ReloadInspectionState::RolledBack => RealmV5RuntimeReloadState::Idle,
        crate::ReloadInspectionState::ActivationFaulted => {
            RealmV5RuntimeReloadState::ActivationFaulted
        }
    }
}

fn count(value: u64) -> usize {
    usize::try_from(value).expect("Realm v5 bounded resource count fits usize")
}

fn debug(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

const fn empty_release_record() -> ReleaseRecord {
    ReleaseRecord {
        realm_id: 0,
        module_id: 0,
        epoch: 0,
        kind: ReleaseKind::HostRequest,
        object_id: 0,
        domain: RuntimeHostDomain::VmThread,
    }
}

struct RealmV5CompiledModules {
    host_hash: StableId,
    schema_hashes: [StableId; REALM_V5_MODULE_COUNT],
    modules: [VerifiedModule; REALM_V5_MODULE_COUNT],
}

fn realm_v5_modules() -> &'static RealmV5CompiledModules {
    static MODULES: OnceLock<RealmV5CompiledModules> = OnceLock::new();
    MODULES.get_or_init(|| {
        let idl = nexa_idl::parse(include_str!("../fixtures/realm_v5/host.idl"))
            .expect("Realm v5 fixture IDL parses");
        let host_hash = nexa_idl::exact_hash(&idl);
        let schema_hashes = [
            StableId::from_name("realm-v5-schema-a"),
            StableId::from_name("realm-v5-schema-b"),
            StableId::from_name("realm-v5-schema-c"),
            StableId::from_name("realm-v5-schema-d"),
        ];
        let sources = [
            include_str!("../fixtures/realm_v5/a.nexa"),
            include_str!("../fixtures/realm_v5/b.nexa"),
            include_str!("../fixtures/realm_v5/c.nexa"),
            include_str!("../fixtures/realm_v5/d.nexa"),
        ];
        let modules = std::array::from_fn(|index| {
            let compiled =
                nexa_compiler::compile_with_interface(sources[index], &idl, schema_hashes[index])
                    .unwrap_or_else(|error| {
                        panic!("Realm v5 module {index} compiles from source: {error:?}")
                    });
            let encoded = compiled.module().encode();
            let decoded = nexa_bytecode::Module::decode(&encoded)
                .unwrap_or_else(|error| panic!("Realm v5 module {index} decodes: {error:?}"));
            verify(decoded, VerifierLimits::default())
                .unwrap_or_else(|error| panic!("Realm v5 module {index} verifies: {error:?}"))
        });
        RealmV5CompiledModules {
            host_hash,
            schema_hashes,
            modules,
        }
    })
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{FunctionEffect, Instruction};

    use crate::StableId;

    use super::{
        RealmV5RuntimeAdapter, RealmV5RuntimeEvent, RealmV5RuntimeTaskState, realm_v5_modules,
    };

    #[test]
    fn real_realm_v5_adapter_has_no_shadow_state() {
        let adapter = RealmV5RuntimeAdapter::new();
        assert_eq!(adapter.realm().realm_id(), 83);
        let source = include_str!("model_adapter_v5.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production adapter source precedes tests");
        let declaration = source
            .split("pub struct RealmV5RuntimeAdapter")
            .nth(1)
            .and_then(|source| source.split('}').next())
            .expect("adapter declaration exists");
        for forbidden in [
            "RealmV5RuntimeReloadState",
            "retired_epochs:",
            "registries:",
            "reload_completions:",
            "active_epoch:",
            "candidate_epoch:",
            "state_registry",
            "completion_buffer",
        ] {
            assert!(
                !declaration.contains(forbidden),
                "adapter declaration contains shadow state field {forbidden}"
            );
        }
        for required in [
            "realm: RealmRuntime",
            "host: RuntimeHost",
            "tasks:",
            "requests:",
        ] {
            assert!(
                declaration.contains(required),
                "adapter declaration is missing {required}"
            );
        }
    }

    #[test]
    fn real_realm_v5_modules_compile_round_trip_and_verify() {
        let fixtures = realm_v5_modules();
        assert_eq!(fixtures.modules.len(), 4);
        for module in &fixtures.modules {
            let module = module.module();
            assert!(module.reload_metadata.migration_entry.is_some());
            assert!(module.reload_metadata.activation_entry.is_some());
            assert!(!module.state_schema.types.is_empty());
            assert!(module.functions.iter().any(|function| {
                function.effect == FunctionEffect::Task
                    && function
                        .code
                        .iter()
                        .any(|instruction| matches!(instruction, Instruction::Yield))
            }));
            assert!(module.functions.iter().any(|function| {
                function.effect == FunctionEffect::Task
                    && function
                        .code
                        .iter()
                        .any(|instruction| matches!(instruction, Instruction::HostCall { .. }))
            }));
            assert!(
                module
                    .functions
                    .iter()
                    .any(|function| function.effect == FunctionEffect::Cleanup)
            );
            assert!(
                module
                    .functions
                    .iter()
                    .any(|function| function.effect == FunctionEffect::Migration)
            );
            assert!(
                module
                    .functions
                    .iter()
                    .any(|function| function.effect == FunctionEffect::Immediate)
            );
        }
        let migration = fixtures.modules[1]
            .module()
            .reload_metadata
            .migration_entry
            .expect("module B has migration");
        let code = &fixtures.modules[1].module().functions[migration as usize].code;
        for required in [
            Instruction::StatePreserve {
                stable_id: StableId::from_name("preserved"),
            },
            Instruction::StateDelete {
                stable_id: StableId::from_name("deleted"),
            },
            Instruction::StateFinish,
        ] {
            assert!(
                code.iter()
                    .any(|instruction| std::mem::discriminant(instruction)
                        == std::mem::discriminant(&required)),
                "module B migration is missing {required:?}"
            );
        }
        assert!(
            code.iter()
                .any(|instruction| matches!(instruction, Instruction::StateReplace { .. }))
        );
    }

    #[test]
    fn real_realm_v5_events_use_production_apis() {
        let mut adapter = RealmV5RuntimeAdapter::new();
        adapter.apply(RealmV5RuntimeEvent::TaskAdmission).unwrap();
        assert!(
            adapter
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::Ready)
        );
        adapter.apply(RealmV5RuntimeEvent::FuelYield).unwrap();
        assert!(
            adapter
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::FuelYielded)
        );
        adapter.apply(RealmV5RuntimeEvent::ExplicitYield).unwrap();
        assert!(
            adapter
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::ExplicitYielded)
        );
        adapter.apply(RealmV5RuntimeEvent::HostWait).unwrap();
        assert!(
            adapter
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::Waiting)
        );
        adapter.apply(RealmV5RuntimeEvent::HostComplete).unwrap();
        let completed_snapshot = adapter.snapshot().unwrap();
        assert!(
            completed_snapshot
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::Running),
            "{completed_snapshot:?}"
        );
        adapter.apply(RealmV5RuntimeEvent::TaskComplete).unwrap();
        assert!(
            adapter
                .snapshot()
                .unwrap()
                .tasks
                .iter()
                .all(|task| task.state == RealmV5RuntimeTaskState::Completed)
        );

        adapter.apply(RealmV5RuntimeEvent::BeginReload).unwrap();
        adapter.apply(RealmV5RuntimeEvent::Quiesce).unwrap();
        adapter.apply(RealmV5RuntimeEvent::Migration).unwrap();
        adapter.apply(RealmV5RuntimeEvent::Commit).unwrap();
        assert_eq!(adapter.snapshot().unwrap().active_epoch, 1);

        let mut resources = RealmV5RuntimeAdapter::new();
        resources.apply(RealmV5RuntimeEvent::TaskAdmission).unwrap();
        resources.apply(RealmV5RuntimeEvent::TokenAcquire).unwrap();
        resources.apply(RealmV5RuntimeEvent::TokenRelease).unwrap();
        resources
            .apply(RealmV5RuntimeEvent::SnapshotAcquire)
            .unwrap();
        resources
            .apply(RealmV5RuntimeEvent::SnapshotRelease)
            .unwrap();
        resources.apply(RealmV5RuntimeEvent::GcRootAttach).unwrap();
        resources.apply(RealmV5RuntimeEvent::GcRootDrop).unwrap();
        resources.apply(RealmV5RuntimeEvent::GcCollect).unwrap();
        resources
            .apply(RealmV5RuntimeEvent::RuntimeHostBeginClose)
            .unwrap();

        let source = include_str!("model_adapter_v5.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production adapter source precedes tests");
        for forbidden in [
            "self.reload =",
            "self.active_epoch =",
            "self.retired_epochs.push",
            "self.state_registry",
            "self.reload_completion_buffer",
        ] {
            assert!(
                !source.contains(forbidden),
                "event adapter directly mutates shadow state: {forbidden}"
            );
        }
    }
}
