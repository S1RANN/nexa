//! Real-runtime adapter for the current bounded Realm reference model.

use std::sync::{Arc, Mutex};

use nexa_bytecode::{
    AsyncResultType, FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
    ModuleBuilder, RootMap, Signature,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

use crate::{
    CancelReason, HostCallOutcome, HostCompletionResult, HostPayload, HostRegistry,
    HostRequestHandle, HostTrap, ModuleHandle, ModuleLifecycle, PendingHostRequest, RealmConfig,
    RealmRuntime, ReloadInspectionState, ResourceContext, RestartReloadOutcome,
    RestartReloadPolicy, RuntimeFailurePoint, RuntimeHost, RuntimeHostArgs, ScopeHandle,
    StepConfig, TaskHandle, TaskLimits, TaskPoll, TaskState, TickBudget, ValueType,
};

const MODEL_HOST: crate::StableId = crate::StableId(0x4d31_5245_414c_484f);
const MODEL_SCHEMA: crate::StableId = crate::StableId(0x4d31_5245_414c_5354);

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

pub struct RealmRuntimeModelAdapter {
    realm: Option<RealmRuntime>,
    runtime_host: RuntimeHost,
    module: ModuleHandle,
    replacement_module: VerifiedModule,
    scope: ScopeHandle,
    task: Option<TaskHandle>,
    request: Option<HostRequestHandle>,
    physical_request: Option<PendingHostRequest>,
    pending_slot: Arc<Mutex<Option<PendingHostRequest>>>,
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
        let runtime_host = RuntimeHost::new(64);
        let pending_slot = Arc::new(Mutex::new(None));
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            runtime_host.clone(),
            Box::new(ModelHost {
                pending: Arc::clone(&pending_slot),
            }),
        )
        .expect("model adapter Realm");
        let replacement_module = async_module(false);
        let module = realm
            .load_module(replacement_module.clone(), MODEL_HOST, MODEL_SCHEMA)
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
            pending_slot,
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

    pub fn apply(&mut self, event: RuntimeRealmEvent) -> Result<(), RuntimeRealmRejection> {
        if self.dropped {
            return Err(RuntimeRealmRejection::RealmDropped);
        }
        let before = self.snapshot();
        self.validate(event, before)?;
        let result = match event {
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
        };
        result.map_err(|()| rejection_for(event))
    }

    fn validate(
        &self,
        event: RuntimeRealmEvent,
        snapshot: RuntimeRealmSnapshot,
    ) -> Result<(), RuntimeRealmRejection> {
        match event {
            RuntimeRealmEvent::Spawn
                if snapshot.task == RuntimeTaskLifecycle::Vacant
                    && snapshot.reload != RuntimeReloadLifecycle::ActivationFaulted =>
            {
                Ok(())
            }
            RuntimeRealmEvent::Poll if snapshot.task == RuntimeTaskLifecycle::Ready => Ok(()),
            RuntimeRealmEvent::CompleteRequest
                if snapshot.task == RuntimeTaskLifecycle::Waiting
                    && snapshot.request == RuntimeRequestLifecycle::Pending =>
            {
                Ok(())
            }
            RuntimeRealmEvent::Cancel
                if matches!(
                    snapshot.task,
                    RuntimeTaskLifecycle::Ready | RuntimeTaskLifecycle::Waiting
                ) =>
            {
                Ok(())
            }
            RuntimeRealmEvent::RestartReload
            | RuntimeRealmEvent::MigrationFailure
            | RuntimeRealmEvent::ActivationFailure
                if snapshot.reload == RuntimeReloadLifecycle::Idle =>
            {
                Ok(())
            }
            RuntimeRealmEvent::LateCompletion
                if snapshot.request == RuntimeRequestLifecycle::Detached
                    && self.physical_request.is_some() =>
            {
                Ok(())
            }
            RuntimeRealmEvent::RealmDrop => Ok(()),
            RuntimeRealmEvent::Poll | RuntimeRealmEvent::Cancel => {
                Err(RuntimeRealmRejection::InvalidTaskState)
            }
            RuntimeRealmEvent::CompleteRequest | RuntimeRealmEvent::LateCompletion => {
                Err(RuntimeRealmRejection::InvalidRequestState)
            }
            RuntimeRealmEvent::Spawn
            | RuntimeRealmEvent::RestartReload
            | RuntimeRealmEvent::MigrationFailure
            | RuntimeRealmEvent::ActivationFailure => {
                Err(RuntimeRealmRejection::InvalidReloadState)
            }
        }
    }

    fn spawn(&mut self) -> Result<(), ()> {
        let realm = self.realm.as_mut().ok_or(())?;
        self.task = Some(
            realm
                .spawn_task(
                    self.module,
                    0,
                    &[],
                    StepConfig {
                        owner: self.scope,
                        priority: 1,
                        fuel_slice: 64,
                        cumulative_budget: 1_024,
                        limits: TaskLimits::default(),
                    },
                )
                .map_err(|_| ())?,
        );
        Ok(())
    }

    fn poll(&mut self) -> Result<(), ()> {
        let realm = self.realm.as_mut().ok_or(())?;
        let TaskPoll::Waiting(request) =
            realm.poll_task(self.task.ok_or(())?, 64).map_err(|_| ())?
        else {
            return Err(());
        };
        self.request = Some(request);
        self.physical_request = self
            .pending_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.physical_request.as_ref().ok_or(())?;
        Ok(())
    }

    fn complete_request(&mut self) -> Result<(), ()> {
        let realm = self.realm.as_mut().ok_or(())?;
        realm
            .complete_request(
                self.request.ok_or(())?,
                HostCompletionResult::Success(HostPayload::I32(7)),
            )
            .map_err(|_| ())?;
        let poll = realm.poll_task(self.task.ok_or(())?, 64).map_err(|_| ())?;
        if !matches!(poll, TaskPoll::Completed(_)) {
            return Err(());
        }
        self.physical_request.take();
        Ok(())
    }

    fn cancel(&mut self) -> Result<(), ()> {
        self.realm
            .as_mut()
            .ok_or(())?
            .cancel_task(self.task.ok_or(())?, CancelReason::RuntimeShutdown)
            .map_err(|_| ())?;
        Ok(())
    }

    fn restart_reload(&mut self) -> Result<(), ()> {
        let outcome = self
            .realm
            .as_mut()
            .ok_or(())?
            .restart_reload(
                self.module,
                self.replacement_module.clone(),
                RestartReloadPolicy::default(),
            )
            .map_err(|_| ())?;
        let RestartReloadOutcome::Committed(candidate) = outcome else {
            return Err(());
        };
        self.module = candidate;
        Ok(())
    }

    fn migration_failure(&mut self) -> Result<(), ()> {
        let outcome = self
            .realm
            .as_mut()
            .ok_or(())?
            .restart_reload(
                self.module,
                async_module(true),
                RestartReloadPolicy::default(),
            )
            .map_err(|_| ())?;
        if !matches!(outcome, RestartReloadOutcome::RolledBackBeforeCommit { .. }) {
            return Err(());
        }
        Ok(())
    }

    fn activation_failure(&mut self) -> Result<(), ()> {
        let realm = self.realm.as_mut().ok_or(())?;
        let probe = realm
            .failure_injector()
            .arm_once(RuntimeFailurePoint::ActivationTrap);
        let outcome = realm
            .restart_reload(
                self.module,
                self.replacement_module.clone(),
                RestartReloadPolicy::default(),
            )
            .map_err(|_| ())?;
        probe.require_consumed().map_err(|_| ())?;
        let RestartReloadOutcome::ActivationFaulted { candidate, .. } = outcome else {
            return Err(());
        };
        self.module = candidate;
        Ok(())
    }

    fn late_completion(&mut self) -> Result<(), ()> {
        self.physical_request
            .take()
            .ok_or(())?
            .ticket
            .complete(HostPayload::I32(9))
            .map_err(|_| ())?;
        self.realm
            .as_mut()
            .ok_or(())?
            .tick(TickBudget::default())
            .map_err(|_| ())?;
        Ok(())
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
    let reload_detached_requests =
        u64::try_from(inspection.reload.detached_requests).unwrap_or(u64::MAX);
    let request_lifecycle = request.map_or(RuntimeRequestLifecycle::Vacant, |_| {
        if ledger.requests != 0 {
            RuntimeRequestLifecycle::Pending
        } else if accounting.delivered != 0
            || (accounting.cancelled != 0 && reload_detached_requests == 0)
        {
            RuntimeRequestLifecycle::Completed
        } else {
            RuntimeRequestLifecycle::Detached
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
            ReloadInspectionState::Idle | ReloadInspectionState::Completed if publications != 0 => {
                RuntimeReloadLifecycle::Active
            }
            ReloadInspectionState::Idle
            | ReloadInspectionState::Completed
            | ReloadInspectionState::RolledBack => RuntimeReloadLifecycle::Idle,
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
        detached_requests: accounting.cancelled.max(reload_detached_requests),
        late_completions_discarded: accounting.late_discarded,
        publications,
    }
}

fn self_host_pending_releases(inspection: &crate::RealmInspectionSnapshot) -> usize {
    inspection.runtime_host_releases.len()
}

const fn rejection_for(event: RuntimeRealmEvent) -> RuntimeRealmRejection {
    match event {
        RuntimeRealmEvent::Poll | RuntimeRealmEvent::Cancel => {
            RuntimeRealmRejection::InvalidTaskState
        }
        RuntimeRealmEvent::CompleteRequest | RuntimeRealmEvent::LateCompletion => {
            RuntimeRealmRejection::InvalidRequestState
        }
        RuntimeRealmEvent::Spawn
        | RuntimeRealmEvent::RestartReload
        | RuntimeRealmEvent::MigrationFailure
        | RuntimeRealmEvent::ActivationFailure => RuntimeRealmRejection::InvalidReloadState,
        RuntimeRealmEvent::RealmDrop => RuntimeRealmRejection::RealmDropped,
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
    builder.metadata(MODEL_HOST, MODEL_SCHEMA).enum_type(result);
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
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(async_result.result_type)),
        },
        1,
    );
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
    builder.function(function);
    verify(builder.finish(), VerifierLimits::default()).expect("verified model fixture")
}

struct ModelHost {
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl HostRegistry for ModelHost {
    fn interface_hash(&self) -> Option<crate::StableId> {
        Some(MODEL_HOST)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        arguments: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 || !arguments.is_empty() {
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
