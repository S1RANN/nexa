//! Real-runtime adapter for the current bounded Realm reference model.

use std::sync::{Arc, Mutex, OnceLock};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, ScriptExport, Signature,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

use crate::{
    CancelReason, HostCallOutcome, HostCompletionResult, HostFunctionAuthority, HostPayload,
    HostRegistry, HostRequestHandle, HostRequestState, HostTrap, ModuleHandle, ModuleLifecycle,
    PendingHostRequest, RealmConfig, RealmError, RealmRuntime, ReloadError, ReloadInspectionState,
    ResourceContext, RestartReloadOutcome, RestartReloadPolicy, RuntimeError, RuntimeFailurePoint,
    RuntimeHost, RuntimeHostArgs, RuntimeLimits, RuntimeResourceLedger, ScopeHandle,
    SlotAllocError, StepConfig, TaskError, TaskHandle, TaskLimits, TaskPoll, TaskState, TickBudget,
    ValueType,
};

const MODEL_HOST: crate::StableId = crate::StableId(0x4d31_5245_414c_484f);

fn model_task_export_id() -> crate::StableId {
    crate::StableId::from_name("RealmRuntimeModel::run")
}

fn model_schema() -> nexa_core::StateSchemaFingerprint {
    nexa_bytecode::StateSchema::default().fingerprint()
}

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
    Completed,
    Cancelled,
    Detached,
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
    pub cancelled_requests: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeRequestRejection {
    StaleHandle,
    CrossRealmHandle,
    AlreadyCompleted,
    DetachedByReload,
    InvalidState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeInvocationCounters {
    pub spawn_attempts: u64,
    pub poll_attempts: u64,
    pub cancel_attempts: u64,
    pub completion_attempts: u64,
    pub reload_attempts: u64,
    pub physical_completion_attempts: u64,
}

impl RuntimeInvocationCounters {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.spawn_attempts
            .saturating_add(self.poll_attempts)
            .saturating_add(self.cancel_attempts)
            .saturating_add(self.completion_attempts)
            .saturating_add(self.reload_attempts)
            .saturating_add(self.physical_completion_attempts)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeStateFingerprint {
    pub snapshot: RuntimeRealmSnapshot,
    pub ledger: RuntimeResourceLedger,
    pub pending_host_completions: usize,
    pub pending_host_releases: usize,
}

pub struct RealmRuntimeModelAdapter {
    realm: Option<RealmRuntime>,
    runtime_host: RuntimeHost,
    module: ModuleHandle,
    replacement_module: VerifiedModule,
    scope: ScopeHandle,
    task: Option<TaskHandle>,
    request: Option<HostRequestHandle>,
    physical_request: Option<PendingHostRequest>,
    detached_physical_request: Option<PendingHostRequest>,
    pending_slot: Arc<Mutex<Option<PendingHostRequest>>>,
    probe: ProbeFixture,
    counters: RuntimeInvocationCounters,
    last_request_rejection: Option<RuntimeRequestRejection>,
    drop_result: Option<RuntimeRealmSnapshot>,
    dropped: bool,
}

impl std::fmt::Debug for RealmRuntimeModelAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RealmRuntimeModelAdapter")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl Default for RealmRuntimeModelAdapter {
    fn default() -> Self {
        let runtime_host = RuntimeHost::new(8);
        let pending_slot = Arc::new(Mutex::new(None));
        let mut realm = RealmRuntime::hosted(
            bounded_config(1),
            runtime_host.clone(),
            Box::new(ModelHost {
                pending: Arc::clone(&pending_slot),
            }),
        )
        .expect("model adapter Realm");
        let replacement_module = async_module(false);
        let module = realm
            .load_module(replacement_module.clone(), MODEL_HOST, model_schema())
            .expect("model adapter module");
        let scope = realm.create_scope(None).expect("model adapter scope");
        Self {
            realm: Some(realm),
            runtime_host,
            module,
            replacement_module,
            scope,
            task: None,
            request: None,
            physical_request: None,
            detached_physical_request: None,
            pending_slot,
            probe: ProbeFixture::new(),
            counters: RuntimeInvocationCounters::default(),
            last_request_rejection: None,
            drop_result: None,
            dropped: false,
        }
    }
}

impl RealmRuntimeModelAdapter {
    #[must_use]
    pub fn snapshot(&self) -> RuntimeRealmSnapshot {
        if let Some(snapshot) = self.drop_result {
            return snapshot;
        }
        let realm = self.realm.as_ref().expect("live adapter has a Realm");
        normalize_snapshot(
            realm,
            self.task,
            self.request,
            self.runtime_host.pending_releases(),
            self.runtime_host.pending_completions(),
        )
    }

    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        let snapshot = self.snapshot();
        let task_balanced = matches!(
            snapshot.task,
            RuntimeTaskLifecycle::Ready | RuntimeTaskLifecycle::Waiting
        ) == (snapshot.task_resources == 1);
        let request_balanced = (snapshot.request == RuntimeRequestLifecycle::Pending)
            == (snapshot.request_resources == 1);
        if self.dropped {
            return task_balanced
                && request_balanced
                && snapshot.task_resources == 0
                && snapshot.request_resources == 0
                && self.runtime_host.pending_releases() == 0;
        }
        let Some(realm) = self.realm.as_ref() else {
            return false;
        };
        let ledger = realm.resource_ledger();
        let accounting = realm.completion_accounting();
        task_balanced
            && request_balanced
            && ledger.tasks <= 1
            && ledger.requests <= 1
            && self.runtime_host.pending_releases()
                == self_host_pending_releases(&realm.inspection_snapshot())
            && self.runtime_host.pending_completions()
                == usize::try_from(accounting.pending()).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub const fn invocation_counters(&self) -> RuntimeInvocationCounters {
        self.counters
    }

    #[must_use]
    pub const fn current_task_handle(&self) -> Option<TaskHandle> {
        self.task
    }

    #[must_use]
    pub const fn current_request_handle(&self) -> Option<HostRequestHandle> {
        self.request
    }

    #[must_use]
    pub const fn last_request_rejection(&self) -> Option<RuntimeRequestRejection> {
        self.last_request_rejection
    }

    #[must_use]
    pub fn state_fingerprint(&self) -> RuntimeStateFingerprint {
        if self.dropped {
            return RuntimeStateFingerprint {
                snapshot: self.snapshot(),
                ledger: RuntimeResourceLedger::default(),
                pending_host_completions: self.runtime_host.pending_completions(),
                pending_host_releases: self.runtime_host.pending_releases(),
            };
        }
        RuntimeStateFingerprint {
            snapshot: self.snapshot(),
            ledger: self
                .realm
                .as_ref()
                .expect("live adapter has a Realm")
                .resource_ledger(),
            pending_host_completions: self.runtime_host.pending_completions(),
            pending_host_releases: self.runtime_host.pending_releases(),
        }
    }

    #[must_use]
    pub const fn has_detached_physical_ticket(&self) -> bool {
        self.detached_physical_request.is_some()
    }

    #[must_use]
    pub const fn has_physical_completion_ticket(&self) -> bool {
        self.physical_request.is_some() || self.detached_physical_request.is_some()
    }

    pub fn apply(&mut self, event: RuntimeRealmEvent) -> Result<(), RuntimeRealmRejection> {
        if self.dropped {
            return Err(RuntimeRealmRejection::RealmDropped);
        }
        self.last_request_rejection = None;
        match event {
            RuntimeRealmEvent::Spawn => self.spawn(),
            RuntimeRealmEvent::Poll => self.poll(),
            RuntimeRealmEvent::CompleteRequest => self.complete_request(),
            RuntimeRealmEvent::Cancel => self.cancel(),
            RuntimeRealmEvent::RestartReload => self.restart_reload(),
            RuntimeRealmEvent::MigrationFailure => self.migration_failure(),
            RuntimeRealmEvent::ActivationFailure => self.activation_failure(),
            RuntimeRealmEvent::LateCompletion => self.late_completion(),
            RuntimeRealmEvent::RealmDrop => {
                self.drop_realm();
                Ok(())
            }
        }
    }

    fn spawn(&mut self) -> Result<(), RuntimeRealmRejection> {
        self.counters.spawn_attempts = self.counters.spawn_attempts.saturating_add(1);
        let result = self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .spawn_task(
                self.module,
                model_task_export_id(),
                &[],
                task_config(self.scope),
            );
        match result {
            Ok(task) => {
                if self.request.is_some_and(|request| {
                    self.realm
                        .as_ref()
                        .expect("live adapter has a Realm")
                        .request_terminal_record(request)
                        .is_some_and(|terminal| {
                            matches!(
                                terminal.state,
                                HostRequestState::Completed
                                    | HostRequestState::Failed
                                    | HostRequestState::Cancelled
                                    | HostRequestState::Abandoned
                            )
                        })
                }) {
                    self.request = None;
                    self.physical_request.take();
                }
                self.task = Some(task);
                Ok(())
            }
            Err(error) => Err(map_spawn_error(error)),
        }
    }

    fn poll(&mut self) -> Result<(), RuntimeRealmRejection> {
        let snapshot = self.snapshot();
        let target = self.current_task_or_probe();
        self.counters.poll_attempts = self.counters.poll_attempts.saturating_add(1);
        match self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .poll_task(target, 64)
        {
            Ok(TaskPoll::Waiting(request)) if snapshot.task == RuntimeTaskLifecycle::Ready => {
                self.request = Some(request);
                self.physical_request = self
                    .pending_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                assert!(
                    self.physical_request.is_some(),
                    "real Host call must publish its physical completion ticket"
                );
                Ok(())
            }
            Ok(TaskPoll::Waiting(request)) if snapshot.task == RuntimeTaskLifecycle::Waiting => {
                assert_eq!(
                    Some(request),
                    self.request,
                    "re-polling a Waiting task must return its current request"
                );
                assert!(
                    self.pending_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_none(),
                    "re-polling a Waiting task must not publish another physical ticket"
                );
                Ok(())
            }
            Ok(TaskPoll::Trapped(_))
                if snapshot.task == RuntimeTaskLifecycle::Ready
                    && snapshot.request == RuntimeRequestLifecycle::Detached
                    && snapshot.late_completions_discarded < snapshot.detached_requests =>
            {
                Ok(())
            }
            Ok(unexpected) => panic!("unexpected successful poll for {snapshot:?}: {unexpected:?}"),
            Err(error) => Err(map_task_error(error)),
        }
    }

    fn complete_request(&mut self) -> Result<(), RuntimeRealmRejection> {
        if self.snapshot().request == RuntimeRequestLifecycle::Pending {
            let mut pending = self
                .physical_request
                .take()
                .expect("Pending request must retain its physical ticket");
            self.counters.physical_completion_attempts =
                self.counters.physical_completion_attempts.saturating_add(1);
            pending
                .ticket
                .complete(HostPayload::I32(7))
                .unwrap_or_else(|error| panic!("physical completion failed: {error:?}"));
            self.realm
                .as_mut()
                .expect("live adapter has a Realm")
                .tick(single_task_tick())
                .unwrap_or_else(|error| panic!("completion tick failed: {error:?}"));
            return Ok(());
        }
        let target = self.current_request_or_probe();
        self.counters.completion_attempts = self.counters.completion_attempts.saturating_add(1);
        match self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .complete_request(target, HostCompletionResult::Success(HostPayload::I32(7)))
        {
            Ok(unexpected) => panic!("invalid completion unexpectedly succeeded: {unexpected:?}"),
            Err(error) => Err(self.record_request_error(error)),
        }
    }

    fn cancel(&mut self) -> Result<(), RuntimeRealmRejection> {
        let snapshot = self.snapshot();
        let target = if matches!(
            snapshot.task,
            RuntimeTaskLifecycle::Ready
                | RuntimeTaskLifecycle::Waiting
                | RuntimeTaskLifecycle::Terminal
        ) {
            self.task.expect("non-vacant snapshot has a Task handle")
        } else {
            self.probe.cross_realm_task()
        };
        self.counters.cancel_attempts = self.counters.cancel_attempts.saturating_add(1);
        match self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .cancel_task(target, CancelReason::RuntimeShutdown)
        {
            Ok(_)
                if matches!(
                    snapshot.task,
                    RuntimeTaskLifecycle::Ready | RuntimeTaskLifecycle::Waiting
                ) =>
            {
                Ok(())
            }
            Ok(unexpected) => {
                panic!("invalid cancellation unexpectedly succeeded: {unexpected:?}")
            }
            Err(error) => Err(map_task_error(error)),
        }
    }

    fn restart_reload(&mut self) -> Result<(), RuntimeRealmRejection> {
        let request_was_pending = self.snapshot().request == RuntimeRequestLifecycle::Pending;
        self.counters.reload_attempts = self.counters.reload_attempts.saturating_add(1);
        let outcome = self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .restart_reload(
                self.module,
                self.replacement_module.clone(),
                RestartReloadPolicy::default(),
            )
            .map_err(map_reload_error)?;
        let RestartReloadOutcome::Committed(candidate) = outcome else {
            panic!("successful restart reload returned {outcome:?}");
        };
        self.module = candidate;
        self.capture_detached_ticket(request_was_pending);
        Ok(())
    }

    fn migration_failure(&mut self) -> Result<(), RuntimeRealmRejection> {
        let request_was_pending = self.snapshot().request == RuntimeRequestLifecycle::Pending;
        self.counters.reload_attempts = self.counters.reload_attempts.saturating_add(1);
        let outcome = self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .restart_reload(
                self.module,
                async_module(true),
                RestartReloadPolicy::default(),
            )
            .map_err(map_reload_error)?;
        assert!(
            matches!(outcome, RestartReloadOutcome::RolledBackBeforeCommit { .. }),
            "failing migration returned {outcome:?}"
        );
        self.capture_detached_ticket(request_was_pending);
        Ok(())
    }

    fn activation_failure(&mut self) -> Result<(), RuntimeRealmRejection> {
        let request_was_pending = self.snapshot().request == RuntimeRequestLifecycle::Pending;
        self.counters.reload_attempts = self.counters.reload_attempts.saturating_add(1);
        let realm = self.realm.as_mut().expect("live adapter has a Realm");
        let probe = realm
            .failure_injector()
            .arm_once(RuntimeFailurePoint::ActivationTrap);
        let outcome = realm
            .restart_reload(
                self.module,
                self.replacement_module.clone(),
                RestartReloadPolicy::default(),
            )
            .map_err(map_reload_error)?;
        let RestartReloadOutcome::ActivationFaulted { candidate, .. } = outcome else {
            panic!("activation failure returned {outcome:?}");
        };
        probe
            .require_consumed()
            .unwrap_or_else(|error| panic!("ActivationTrap probe not consumed: {error:?}"));
        self.module = candidate;
        self.capture_detached_ticket(request_was_pending);
        Ok(())
    }

    fn late_completion(&mut self) -> Result<(), RuntimeRealmRejection> {
        if self.snapshot().request == RuntimeRequestLifecycle::Detached {
            let Some(mut pending) = self.detached_physical_request.take() else {
                return Err(RuntimeRealmRejection::InvalidRequestState);
            };
            self.counters.physical_completion_attempts =
                self.counters.physical_completion_attempts.saturating_add(1);
            pending
                .ticket
                .complete(HostPayload::I32(9))
                .unwrap_or_else(|error| panic!("late physical completion failed: {error:?}"));
            self.realm
                .as_mut()
                .expect("live adapter has a Realm")
                .tick(single_task_tick())
                .unwrap_or_else(|error| panic!("late completion tick failed: {error:?}"));
            return Ok(());
        }
        if self.detached_physical_request.is_none() && self.physical_request.is_none() {
            return Err(RuntimeRealmRejection::InvalidRequestState);
        }
        let target = self.probe.cross_realm_request();
        self.counters.completion_attempts = self.counters.completion_attempts.saturating_add(1);
        match self
            .realm
            .as_mut()
            .expect("live adapter has a Realm")
            .complete_request(target, HostCompletionResult::Success(HostPayload::I32(9)))
        {
            Ok(unexpected) => {
                panic!("invalid late completion unexpectedly succeeded: {unexpected:?}")
            }
            Err(error) => Err(self.record_request_error(error)),
        }
    }

    fn capture_detached_ticket(&mut self, request_was_pending: bool) {
        if request_was_pending {
            self.detached_physical_request = self.physical_request.take();
            assert!(
                self.detached_physical_request.is_some(),
                "reload detached request must retain its physical ticket"
            );
        }
    }

    fn current_task_or_probe(&self) -> TaskHandle {
        self.task.unwrap_or_else(|| self.probe.cross_realm_task())
    }

    fn current_request_or_probe(&self) -> HostRequestHandle {
        self.request
            .unwrap_or_else(|| self.probe.cross_realm_request())
    }

    fn record_request_error(&mut self, error: RuntimeError) -> RuntimeRealmRejection {
        let (rejection, detail) = map_request_error(error);
        self.last_request_rejection = Some(detail);
        rejection
    }

    fn drop_realm(&mut self) {
        let before = self.snapshot();
        drop(self.realm.take());
        let releases = self.runtime_host.drain_releases();
        let request_was_live = before.request == RuntimeRequestLifecycle::Pending;
        self.drop_result = Some(RuntimeRealmSnapshot {
            task: if before.task == RuntimeTaskLifecycle::Vacant {
                RuntimeTaskLifecycle::Vacant
            } else {
                RuntimeTaskLifecycle::Terminal
            },
            request: if request_was_live {
                RuntimeRequestLifecycle::Detached
            } else {
                before.request
            },
            task_resources: 0,
            request_resources: 0,
            cancelled_tasks: before.cancelled_tasks
                + u64::from(matches!(
                    before.task,
                    RuntimeTaskLifecycle::Ready | RuntimeTaskLifecycle::Waiting
                )),
            detached_requests: before.detached_requests + u64::from(request_was_live),
            ..before
        });
        debug_assert_eq!(self.runtime_host.pending_releases(), 0);
        debug_assert!(!releases.is_empty() || before.request_resources == 0);
        self.dropped = true;
    }
}

impl Drop for RealmRuntimeModelAdapter {
    fn drop(&mut self) {
        self.physical_request.take();
        self.detached_physical_request.take();
        self.pending_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(self.realm.take());
        for _ in 0..3 {
            let _ = self.runtime_host.drain_releases();
        }
        let _ = self.runtime_host.begin_close();
        let _ = self.runtime_host.try_finish_close();
    }
}

struct ProbeFixture {
    realm: Option<RealmRuntime>,
    runtime_host: RuntimeHost,
    cross_task: TaskHandle,
    cross_request: HostRequestHandle,
    cross_physical_request: Option<PendingHostRequest>,
    terminal_task: TaskHandle,
    completed_request: HostRequestHandle,
    stale_task: TaskHandle,
    stale_request: HostRequestHandle,
}

impl ProbeFixture {
    fn new() -> Self {
        let runtime_host = RuntimeHost::new(8);
        let pending_slot = Arc::new(Mutex::new(None));
        let mut config = bounded_config(2);
        config.tombstone_capacity = 1;
        let mut realm = RealmRuntime::hosted(
            config,
            runtime_host.clone(),
            Box::new(ModelHost {
                pending: Arc::clone(&pending_slot),
            }),
        )
        .expect("Probe Realm");
        let module = realm
            .load_module(async_module(false), MODEL_HOST, model_schema())
            .expect("Probe module");
        let scope = realm.create_scope(None).expect("Probe scope");

        let (stale_task, stale_request, mut stale_physical) =
            spawn_waiting_probe(&mut realm, &pending_slot, module, scope);
        stale_physical
            .ticket
            .complete(HostPayload::I32(1))
            .expect("complete stale Probe request");
        realm
            .tick(single_task_tick())
            .expect("finish stale Probe task");

        let (terminal_task, completed_request, mut completed_physical) =
            spawn_waiting_probe(&mut realm, &pending_slot, module, scope);
        completed_physical
            .ticket
            .complete(HostPayload::I32(2))
            .expect("complete terminal Probe request");
        realm
            .tick(single_task_tick())
            .expect("finish terminal Probe task");

        assert!(matches!(
            realm.poll_task(stale_task, 64),
            Err(RuntimeError::StaleTaskHandle)
        ));
        assert!(matches!(
            realm.complete_request(
                stale_request,
                HostCompletionResult::Success(HostPayload::I32(3))
            ),
            Err(RuntimeError::Realm(error))
                if matches!(*error, RealmError::Host(crate::HostRequestError::StaleHostRequestHandle))
        ));
        assert!(matches!(
            realm.poll_task(terminal_task, 64),
            Err(RuntimeError::TerminalTask)
        ));
        assert!(matches!(
            realm.complete_request(
                completed_request,
                HostCompletionResult::Success(HostPayload::I32(4))
            ),
            Err(RuntimeError::Realm(error))
                if matches!(*error, RealmError::Host(crate::HostRequestError::AlreadyCompleted))
        ));

        let (cross_task, cross_request, cross_physical_request) =
            spawn_waiting_probe(&mut realm, &pending_slot, module, scope);
        Self {
            realm: Some(realm),
            runtime_host,
            cross_task,
            cross_request,
            cross_physical_request: Some(cross_physical_request),
            terminal_task,
            completed_request,
            stale_task,
            stale_request,
        }
    }

    const fn cross_realm_task(&self) -> TaskHandle {
        self.cross_task
    }

    const fn cross_realm_request(&self) -> HostRequestHandle {
        self.cross_request
    }
}

impl Drop for ProbeFixture {
    fn drop(&mut self) {
        self.cross_physical_request.take();
        drop(self.realm.take());
        for _ in 0..3 {
            let _ = self.runtime_host.drain_releases();
        }
        let _ = self.runtime_host.begin_close();
        let _ = self.runtime_host.try_finish_close();
    }
}

impl std::fmt::Debug for ProbeFixture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProbeFixture")
            .field("cross_task", &self.cross_task)
            .field("cross_request", &self.cross_request)
            .field("terminal_task", &self.terminal_task)
            .field("completed_request", &self.completed_request)
            .field("stale_task", &self.stale_task)
            .field("stale_request", &self.stale_request)
            .finish_non_exhaustive()
    }
}

fn spawn_waiting_probe(
    realm: &mut RealmRuntime,
    pending_slot: &Arc<Mutex<Option<PendingHostRequest>>>,
    module: ModuleHandle,
    scope: ScopeHandle,
) -> (TaskHandle, HostRequestHandle, PendingHostRequest) {
    let task = realm
        .spawn_task(module, model_task_export_id(), &[], task_config(scope))
        .expect("spawn Probe task");
    let TaskPoll::Waiting(request) = realm.poll_task(task, 64).expect("poll Probe task") else {
        panic!("Probe task must wait on a Host request");
    };
    let physical = pending_slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("Probe physical completion ticket");
    (task, request, physical)
}

fn bounded_config(realm_id: u32) -> RealmConfig {
    RealmConfig {
        realm_id,
        runtime_limits: RuntimeLimits {
            max_tasks: 1,
            max_scopes: 2,
            max_frame_segments: 1,
            max_scheduler_tokens: 1,
            max_trace_records: 32,
            max_transient_children_per_scope: 1,
            max_persistent_children_per_scope: 1,
        },
        max_host_resources: 1,
        release_capacity: 8,
        tombstone_capacity: 16,
        ..RealmConfig::default()
    }
}

fn task_config(scope: ScopeHandle) -> StepConfig {
    StepConfig {
        owner: scope,
        priority: 1,
        fuel_slice: 64,
        cumulative_budget: 1_024,
        limits: TaskLimits::default(),
    }
}

const fn single_task_tick() -> TickBudget {
    TickBudget {
        max_tasks: 1,
        frame_fuel_budget: 64,
        collect_garbage: false,
    }
}

fn normalize_snapshot(
    realm: &RealmRuntime,
    task: Option<TaskHandle>,
    request: Option<HostRequestHandle>,
    pending_releases: usize,
    pending_completions: usize,
) -> RuntimeRealmSnapshot {
    let inspection = realm.inspection_snapshot();
    let ledger = realm.resource_ledger();
    let accounting = realm.completion_accounting();
    let task_lifecycle = task.map_or(RuntimeTaskLifecycle::Vacant, |handle| {
        inspection
            .tasks
            .iter()
            .find(|item| item.handle == handle)
            .map_or_else(
                || RuntimeTaskLifecycle::Terminal,
                |item| match item.state {
                    TaskState::Ready
                    | TaskState::Running
                    | TaskState::FuelYielded
                    | TaskState::ExplicitYielded => RuntimeTaskLifecycle::Ready,
                    TaskState::Waiting => RuntimeTaskLifecycle::Waiting,
                    _ => RuntimeTaskLifecycle::Terminal,
                },
            )
    });
    let publications = u64::try_from(inspection.reload.root_publications.len()).unwrap_or(u64::MAX);
    let request_lifecycle = request.map_or(RuntimeRequestLifecycle::Vacant, |handle| {
        if ledger.requests != 0 {
            RuntimeRequestLifecycle::Pending
        } else {
            match realm
                .request_terminal_record(handle)
                .map(|record| record.state)
            {
                Some(HostRequestState::Completed | HostRequestState::Failed) => {
                    RuntimeRequestLifecycle::Completed
                }
                Some(HostRequestState::Cancelled | HostRequestState::Abandoned) => {
                    RuntimeRequestLifecycle::Cancelled
                }
                Some(HostRequestState::Detached) => RuntimeRequestLifecycle::Detached,
                Some(HostRequestState::Pending | HostRequestState::CompletionQueued) => {
                    panic!("terminal request record retained a live state")
                }
                None => panic!("current request has neither a live slot nor terminal evidence"),
            }
        }
    });
    let activation_faulted = inspection
        .active_root
        .as_ref()
        .is_some_and(|module| module.lifecycle == ModuleLifecycle::ActivationFaulted);
    let reload = if activation_faulted {
        RuntimeReloadLifecycle::ActivationFaulted
    } else {
        match inspection.reload.state {
            ReloadInspectionState::ActivationFaulted => RuntimeReloadLifecycle::ActivationFaulted,
            ReloadInspectionState::Preparing
            | ReloadInspectionState::Quiescing
            | ReloadInspectionState::Staging
            | ReloadInspectionState::Committing
            | ReloadInspectionState::Published
            | ReloadInspectionState::Activating => RuntimeReloadLifecycle::Staging,
            ReloadInspectionState::Idle
            | ReloadInspectionState::Completed
            | ReloadInspectionState::RolledBack => {
                if publications == 0 {
                    RuntimeReloadLifecycle::Idle
                } else {
                    RuntimeReloadLifecycle::Active
                }
            }
        }
    };
    let terminal_cancellations = inspection
        .terminal_tasks
        .iter()
        .filter(|terminal| terminal.state == TaskState::Cancelled)
        .count();
    debug_assert_eq!(pending_releases, self_host_pending_releases(&inspection));
    debug_assert_eq!(
        pending_completions,
        usize::try_from(accounting.pending()).unwrap_or(usize::MAX)
    );
    RuntimeRealmSnapshot {
        task: task_lifecycle,
        request: request_lifecycle,
        reload,
        epoch: publications,
        task_resources: u8::from(ledger.tasks != 0),
        request_resources: u8::from(ledger.requests != 0),
        cancelled_tasks: u64::try_from(terminal_cancellations).unwrap_or(u64::MAX),
        cancelled_requests: accounting.cancelled,
        detached_requests: inspection.reload.total_detached_requests,
        late_completions_discarded: accounting.late_discarded,
        publications,
    }
}

fn self_host_pending_releases(inspection: &crate::RealmInspectionSnapshot) -> usize {
    inspection.runtime_host_releases.len()
}

fn map_spawn_error(error: RuntimeError) -> RuntimeRealmRejection {
    match error {
        RuntimeError::Task(TaskError::Allocation(
            SlotAllocError::CapacityExhausted | SlotAllocError::NoFreeSlot,
        ))
        | RuntimeError::ResourceLimit(
            "scheduler token pool"
            | "frame segment pool"
            | "trace capacity pool"
            | "transient child limit",
        ) => RuntimeRealmRejection::InvalidTaskState,
        RuntimeError::Realm(error) if matches!(*error, RealmError::ModuleNotCallable) => {
            RuntimeRealmRejection::InvalidReloadState
        }
        unexpected => panic!("unknown spawn Runtime error: {unexpected:?}"),
    }
}

fn map_task_error(error: RuntimeError) -> RuntimeRealmRejection {
    match error {
        RuntimeError::TerminalTask
        | RuntimeError::StaleTaskHandle
        | RuntimeError::CrossRealmTaskHandle => RuntimeRealmRejection::InvalidTaskState,
        unexpected => panic!("unknown Task Runtime error: {unexpected:?}"),
    }
}

fn map_request_error(error: RuntimeError) -> (RuntimeRealmRejection, RuntimeRequestRejection) {
    match error {
        RuntimeError::Realm(error) => match *error {
            RealmError::Host(crate::HostRequestError::StaleHostRequestHandle) => (
                RuntimeRealmRejection::InvalidRequestState,
                RuntimeRequestRejection::StaleHandle,
            ),
            RealmError::Host(crate::HostRequestError::CrossRealmHostRequestHandle) => (
                RuntimeRealmRejection::InvalidRequestState,
                RuntimeRequestRejection::CrossRealmHandle,
            ),
            RealmError::Host(crate::HostRequestError::AlreadyCompleted) => (
                RuntimeRealmRejection::InvalidRequestState,
                RuntimeRequestRejection::AlreadyCompleted,
            ),
            RealmError::Host(crate::HostRequestError::DetachedByReload) => (
                RuntimeRealmRejection::InvalidRequestState,
                RuntimeRequestRejection::DetachedByReload,
            ),
            RealmError::Host(crate::HostRequestError::InvalidState) => (
                RuntimeRealmRejection::InvalidRequestState,
                RuntimeRequestRejection::InvalidState,
            ),
            unexpected => panic!("unknown Realm request error: {unexpected:?}"),
        },
        unexpected => panic!("unknown request Runtime error: {unexpected:?}"),
    }
}

fn map_reload_error(error: ReloadError) -> RuntimeRealmRejection {
    match error {
        ReloadError::InvalidState => RuntimeRealmRejection::InvalidReloadState,
        unexpected => panic!("unknown Reload error: {unexpected:?}"),
    }
}

fn async_module(failing_migration: bool) -> VerifiedModule {
    let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let mut builder = ModuleBuilder::new();
    builder
        .metadata(MODEL_HOST, model_schema())
        .enum_type(result);
    if failing_migration {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Trap);
        builder.function(migration.finish().expect("failing migration"));
    }
    builder.host_import(HostImport {
        stable_id: crate::StableId::from_name("ModelHost::request"),
        declaration_fingerprint: [0; 32],
        capabilities: Vec::new(),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let task_signature = Signature {
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
    };
    let mut function = FunctionBuilder::new(task_signature.clone(), 1);
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut function = function.finish().expect("model async function");
    function.root_bitmap[0] = true;
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![true],
        },
    ];
    let function = builder.function(function);
    builder.script_export(ScriptExport {
        stable_id: model_task_export_id(),
        function,
        signature: task_signature,
        effect: FunctionEffect::Task,
    });
    verify(builder.finish(), VerifierLimits::default()).expect("verified model fixture")
}

struct ModelHost {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for ModelHost {
    fn contract_runtime_id(&self) -> Option<crate::StableId> {
        Some(MODEL_HOST)
    }

    fn function_authority(&self, id: crate::StableId) -> Option<&HostFunctionAuthority> {
        static AUTHORITY: OnceLock<HostFunctionAuthority> = OnceLock::new();
        let authority = AUTHORITY.get_or_init(|| {
            let stable_id = crate::StableId::from_name("ModelHost::request");
            let result = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
            HostFunctionAuthority::new(
                stable_id,
                [0; 32],
                &[],
                Some(ValueType::Named(result.type_id)),
                HostCallMode::Async,
                1,
                Some(AsyncResultType {
                    result_type: result.type_id,
                    success: ValueType::I32,
                    error: ValueType::I32,
                    cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
                    abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
                    cancel_error: Some(1),
                    abandon_error: None,
                }),
                &[],
            )
        });
        (id == authority.stable_id()).then_some(authority)
    }

    fn call_runtime(
        &mut self,
        id: crate::StableId,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != crate::StableId::from_name("ModelHost::request") {
            return Err(HostTrap::UnknownFunction(id));
        }
        if !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        let pending = context
            .create_request()
            .map_err(|_| HostTrap::ResourceCapacity)?;
        let request = pending.request;
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
        Ok(HostCallOutcome::Pending(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_snapshot_is_derived_from_real_realm() {
        let mut adapter = RealmRuntimeModelAdapter::default();
        assert!(adapter.realm.is_some());
        adapter.apply(RuntimeRealmEvent::Spawn).expect("spawn");
        adapter.apply(RuntimeRealmEvent::Poll).expect("poll");
        assert_eq!(
            adapter.realm.as_ref().unwrap().resource_ledger().requests,
            1
        );
        assert_eq!(adapter.snapshot().request, RuntimeRequestLifecycle::Pending);
    }
}
