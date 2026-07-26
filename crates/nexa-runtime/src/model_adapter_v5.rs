use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::{
    GcRef, HostCompletionTicket, HostRegistry, ModuleHandle, PendingHostRequest, RealmConfig,
    RealmRuntime, ResourceTokenHandle, RuntimeFailureInjector, RuntimeHost, ScopeHandle,
    SnapshotHandle, StableId, TaskHandle,
};

use super::{
    REALM_V5_EPOCH_COUNT, REALM_V5_REQUEST_COUNT, REALM_V5_RETIRED_COUNT, REALM_V5_TASK_COUNT,
    RealmV5RuntimeApplyError, RealmV5RuntimeEvent, RealmV5RuntimeExecution,
    RealmV5RuntimeLedgerSnapshot, RealmV5RuntimeReloadState, RealmV5RuntimeRequestSnapshot,
    RealmV5RuntimeRequestState, RealmV5RuntimeRetiredEpoch, RealmV5RuntimeSnapshot,
    RealmV5RuntimeTaskSnapshot, RealmV5RuntimeTaskState, RoutingRegistry, routing_old_module,
};

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
        let host_hash = StableId::from_name("realm-v5-production-host");
        let schema_hash = StableId::from_name("realm-v5-production-schema-a");
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
            .load_module(
                routing_old_module(host_hash, schema_hash),
                host_hash,
                schema_hash,
            )
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
        match event {
            RealmV5RuntimeEvent::GcCollect => {
                self.realm
                    .collect_garbage()
                    .map_err(|error| RealmV5RuntimeApplyError::Invariant(format!("{error:?}")))?;
                if self
                    .fixtures
                    .gc_object
                    .is_some_and(|object| self.realm.resolve_heap_object(object).is_err())
                {
                    self.fixtures.gc_object = None;
                }
                Ok(())
            }
            _ => Err(RealmV5RuntimeApplyError::Invariant(format!(
                "{event:?} is not mapped until its production fixture is installed"
            ))),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn snapshot(&self) -> Result<RealmV5RuntimeSnapshot, String> {
        let active = self
            .realm
            .active_root()
            .ok_or_else(|| "Realm v5 production active root is missing".to_owned())?;
        let active_epoch = normalize_epoch(self.realm.module_epoch(active).map_err(debug)?)?;
        let mut state_registry_objects = [0; REALM_V5_EPOCH_COUNT];
        for (index, module) in self.modules.epochs.iter().copied().enumerate() {
            if let Some(module) = module {
                state_registry_objects[index] =
                    self.realm.state_handles(module).map_err(debug)?.len();
            }
        }
        let ledger = self.realm.resource_ledger();
        let tasks = std::array::from_fn(|index| self.task_snapshot(index));
        let scheduler = std::array::from_fn(|index| {
            matches!(
                tasks[index].state,
                RealmV5RuntimeTaskState::Ready
                    | RealmV5RuntimeTaskState::FuelYielded
                    | RealmV5RuntimeTaskState::ExplicitYielded
            )
        });
        let requests = std::array::from_fn(|index| RealmV5RuntimeRequestSnapshot {
            state: if self.fixtures.request_handles[index].is_some() {
                RealmV5RuntimeRequestState::Pending
            } else {
                RealmV5RuntimeRequestState::Vacant
            },
            task: self.fixtures.request_handles[index].and_then(|_| u8::try_from(index).ok()),
            epoch: tasks[index].epoch,
        });
        let mut retired_epochs = [RealmV5RuntimeRetiredEpoch::Vacant; REALM_V5_RETIRED_COUNT];
        for (index, retired) in self
            .realm
            .retired_epochs()
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
        Ok(RealmV5RuntimeSnapshot {
            active_epoch,
            candidate_epoch: self.candidate_epoch()?,
            retired_epochs,
            tasks,
            scheduler,
            requests,
            token_live: self.fixtures.tokens.iter().any(Option::is_some),
            token_owner: first_index(&self.fixtures.tokens),
            token_epoch: first_epoch(tasks, &self.fixtures.tokens),
            token_consumed: false,
            snapshot_live: self.fixtures.snapshots.iter().any(Option::is_some),
            snapshot_owner: first_index(&self.fixtures.snapshots),
            snapshot_epoch: first_epoch(tasks, &self.fixtures.snapshots),
            snapshot_consumed: false,
            heap_object: self
                .fixtures
                .gc_object
                .is_some_and(|object| self.realm.resolve_heap_object(object).is_ok()),
            gc_root: false,
            gc_epoch: active_epoch,
            gc_consumed: self.fixtures.gc_object.is_some(),
            reload: self.reload_state()?,
            reload_completion_buffer: self.realm.reload_buffered_completions(),
            release_backlog: [0; REALM_V5_EPOCH_COUNT],
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
                completion_queued: count(self.realm.completion_accounting().queued),
                tokens: count(ledger.tokens),
                snapshots: count(ledger.snapshots),
                release_records: count(
                    ledger
                        .release_reservations
                        .saturating_add(ledger.queued_releases),
                ),
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

    fn task_snapshot(&self, index: usize) -> RealmV5RuntimeTaskSnapshot {
        let Some(task) = self.tasks[index] else {
            return RealmV5RuntimeTaskSnapshot {
                state: RealmV5RuntimeTaskState::Vacant,
                execution: RealmV5RuntimeExecution::None,
                epoch: 0,
            };
        };
        if let Ok(snapshot) = self.realm.task_snapshot(task) {
            let state = runtime_task_state(snapshot.state);
            return RealmV5RuntimeTaskSnapshot {
                state,
                execution: execution_state(state),
                epoch: normalize_epoch(snapshot.module_epoch).unwrap_or(0),
            };
        }
        let state = self
            .realm
            .terminal_record(task)
            .map_or(RealmV5RuntimeTaskState::Vacant, |record| {
                runtime_task_state(record.state)
            });
        RealmV5RuntimeTaskSnapshot {
            state,
            execution: RealmV5RuntimeExecution::None,
            epoch: 0,
        }
    }

    fn candidate_epoch(&self) -> Result<Option<u8>, String> {
        for module in self.modules.epochs.iter().flatten().copied() {
            if matches!(
                self.realm.module_lifecycle(module).map_err(debug)?,
                crate::ModuleLifecycle::Staging | crate::ModuleLifecycle::Activating
            ) {
                return normalize_epoch(self.realm.module_epoch(module).map_err(debug)?).map(Some);
            }
        }
        Ok(None)
    }

    fn reload_state(&self) -> Result<RealmV5RuntimeReloadState, String> {
        for module in self.modules.epochs.iter().flatten().copied() {
            match self.realm.module_lifecycle(module).map_err(debug)? {
                crate::ModuleLifecycle::Staging => {
                    return Ok(RealmV5RuntimeReloadState::Prepared);
                }
                crate::ModuleLifecycle::Activating => {
                    return Ok(RealmV5RuntimeReloadState::Migrated);
                }
                crate::ModuleLifecycle::ActivationFaulted => {
                    return Ok(RealmV5RuntimeReloadState::ActivationFaulted);
                }
                crate::ModuleLifecycle::Active | crate::ModuleLifecycle::Retired => {}
            }
        }
        Ok(RealmV5RuntimeReloadState::Idle)
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

const fn execution_state(state: RealmV5RuntimeTaskState) -> RealmV5RuntimeExecution {
    match state {
        RealmV5RuntimeTaskState::Ready => RealmV5RuntimeExecution::Ready,
        RealmV5RuntimeTaskState::Running => RealmV5RuntimeExecution::Running,
        RealmV5RuntimeTaskState::FuelYielded => RealmV5RuntimeExecution::FuelYielded,
        RealmV5RuntimeTaskState::ExplicitYielded => RealmV5RuntimeExecution::ExplicitYielded,
        RealmV5RuntimeTaskState::Waiting => RealmV5RuntimeExecution::Waiting,
        RealmV5RuntimeTaskState::ReloadPaused => RealmV5RuntimeExecution::ReloadPaused,
        RealmV5RuntimeTaskState::Cancelling => RealmV5RuntimeExecution::Cancelling,
        RealmV5RuntimeTaskState::Cleanup => RealmV5RuntimeExecution::Cleanup,
        RealmV5RuntimeTaskState::Vacant
        | RealmV5RuntimeTaskState::Completed
        | RealmV5RuntimeTaskState::Cancelled
        | RealmV5RuntimeTaskState::Trapped => RealmV5RuntimeExecution::None,
    }
}

fn normalize_epoch(epoch: u64) -> Result<u8, String> {
    let normalized = epoch
        .checked_sub(1)
        .ok_or_else(|| "Realm epoch must be positive".to_owned())?;
    u8::try_from(normalized).map_err(|_| "Realm epoch exceeds v5 fixture capacity".to_owned())
}

fn first_index<T>(values: &[Option<T>; REALM_V5_TASK_COUNT]) -> Option<u8> {
    values
        .iter()
        .position(Option::is_some)
        .and_then(|index| u8::try_from(index).ok())
}

fn first_epoch<T>(
    tasks: [RealmV5RuntimeTaskSnapshot; REALM_V5_TASK_COUNT],
    values: &[Option<T>; REALM_V5_TASK_COUNT],
) -> u8 {
    values
        .iter()
        .position(Option::is_some)
        .map_or(0, |index| tasks[index].epoch)
}

fn count(value: u64) -> usize {
    usize::try_from(value).expect("Realm v5 bounded resource count fits usize")
}

fn debug(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

#[cfg(test)]
mod tests {
    use super::RealmV5RuntimeAdapter;

    #[test]
    fn real_realm_v5_adapter_has_no_shadow_state() {
        let adapter = RealmV5RuntimeAdapter::new();
        assert_eq!(adapter.realm().realm_id(), 83);
        let source = include_str!("model_adapter_v5.rs");
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
}
