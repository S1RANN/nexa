use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use nexa_bytecode::{AbandonPolicy, AsyncResultType, CancelPolicy, HostImport, ValueType};
use nexa_core::{RawHandle, StableId};
use nexa_verifier::VerifiedModule;

use crate::machines::retired_epoch;
use crate::reload::{ReloadCompletionBuffer, ReloadCoordinator, ReloadTransaction};
use crate::scheduler::Scheduler;
use crate::stateful::{MigrationLimitError, MigrationLimits, StatefulDomainId, StatefulRegistry};
use crate::task::TaskExecution;
use crate::{
    CheckedInterpreter, CollectionStats, ContinuationReservation, ExecutionCharge, FuelState,
    GcRef, GcRoots, Heap, HeapError, HostArgs, HostCallOutcome, HostCompletionDelivery,
    HostCompletionResult, HostPayload, HostRegistry, HostRequestError, HostRequestHandle, HostTrap,
    HostValue, InterpreterError, InterpreterHost, InterpreterHostOutcome, InterpreterOutcome,
    Object, OpcodeCostTable, PendingHostRequest, ReloadError, ResourceTokenHandle, RuntimeError,
    RuntimeHost, RuntimeHostDomain, RuntimeHostState, RuntimeLimits, RuntimeResources,
    RuntimeTrace, RuntimeValue, ScopeHandle, SlotAllocError, SlotPool, SnapshotHandle, StepConfig,
    SuspendReason, TaskHandle, TaskRuntime, TaskState, Trap, TrapKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleHandle(RawHandle);

impl ModuleHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleLifecycle {
    Active,
    Staging,
    Activating,
    ActivationFaulted,
    Retired,
}

#[derive(Clone, Debug)]
pub struct ModuleEpochRoot {
    pub module_id: u32,
    pub stateful_domain: StatefulDomainId,
    pub epoch: u64,
    pub verified: Arc<VerifiedModule>,
    pub host_hash: StableId,
    pub schema_hash: StableId,
    pub lifecycle: ModuleLifecycle,
    globals: Vec<GcRef>,
    state: StatefulRegistry,
    staging_roots: Vec<GcRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootPublicationRecord {
    pub publication_id: u64,
    pub old_root: ModuleHandle,
    pub candidate_root: ModuleHandle,
    pub candidate_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetiredEpochState {
    Retired,
    Drained,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredEpochSnapshot {
    pub module: ModuleHandle,
    pub epoch: u64,
    pub state: RetiredEpochState,
    pub task_count: usize,
    pub request_count: usize,
    pub token_count: usize,
    pub snapshot_count: usize,
    pub gc_root_count: usize,
    pub pending_releases: usize,
    pub pending_completions: usize,
}

#[derive(Debug)]
struct RetiredEpochRegistry {
    entries: VecDeque<RetiredEpochSnapshot>,
    capacity: usize,
}

impl RetiredEpochRegistry {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn retire(&mut self, entry: RetiredEpochSnapshot) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() == self.capacity {
            let drained = self
                .entries
                .iter()
                .position(|entry| entry.state == RetiredEpochState::Drained)
                .expect("live retired epochs cannot exhaust the module-sized registry");
            self.entries.remove(drained);
        }
        self.entries.push_back(entry);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ActivationEntry<'a> {
    pub function_id: u32,
    pub arguments: &'a [RuntimeValue],
    pub fuel: u64,
}

struct RealmHostBridge<'a> {
    registry: &'a mut dyn HostRegistry,
    resources: &'a mut RuntimeResources,
    task: TaskHandle,
    module_id: u32,
    epoch: u64,
    imports: &'a [HostImport],
}

impl InterpreterHost for RealmHostBridge<'_> {
    fn call(
        &mut self,
        import: u32,
        arguments: &[RuntimeValue],
    ) -> Result<InterpreterHostOutcome, HostTrap> {
        let metadata = self
            .imports
            .get(import as usize)
            .ok_or(HostTrap::UnknownFunction(import))?;
        let values = HostArgs::from_runtime(arguments)?;
        let mut context = self
            .resources
            .context(self.task, self.module_id, self.epoch);
        match self.registry.call(import, &mut context, values)? {
            HostCallOutcome::Immediate(value) => Ok(InterpreterHostOutcome::Immediate(
                host_to_runtime_value(value, metadata.result)?,
            )),
            HostCallOutcome::Pending(request) => {
                if !self.resources.owns_request(self.task, request) {
                    return Err(HostTrap::Host(
                        "pending host request is not owned by the calling task".into(),
                    ));
                }
                Ok(InterpreterHostOutcome::Pending(request))
            }
        }
    }
}

fn host_to_runtime_value(
    value: HostValue,
    expected: Option<ValueType>,
) -> Result<RuntimeValue, HostTrap> {
    match (value, expected) {
        (HostValue::I32(value), Some(ValueType::I32)) => Ok(RuntimeValue::I32(value)),
        (HostValue::Bool(value), Some(ValueType::Bool)) => Ok(RuntimeValue::Bool(value)),
        (HostValue::Request(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::HostRequest(value))
        }
        (HostValue::Token(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::ResourceToken(value))
        }
        (HostValue::Snapshot(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::Snapshot(value))
        }
        (HostValue::Opaque(value), Some(ValueType::Named(type_id))) => {
            Ok(RuntimeValue::Opaque { value, type_id })
        }
        (HostValue::Unit, None) => Ok(RuntimeValue::Unit),
        _ => Err(HostTrap::Type),
    }
}

fn completion_to_runtime(
    payload: HostPayload,
    expected: Option<ValueType>,
) -> Result<RuntimeValue, InterpreterError> {
    if expected.is_none() {
        return Ok(RuntimeValue::Unit);
    }
    match (payload, expected) {
        (HostPayload::I32(value), Some(ValueType::I32)) => Ok(RuntimeValue::I32(value)),
        (HostPayload::Bool(value), Some(ValueType::Bool)) => Ok(RuntimeValue::Bool(value)),
        (HostPayload::Opaque(value), Some(ValueType::Named(type_id))) => {
            Ok(RuntimeValue::Opaque { value, type_id })
        }
        (HostPayload::Token(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::ResourceToken(value))
        }
        (HostPayload::Snapshot(value), Some(ValueType::Named(_))) => {
            Ok(RuntimeValue::Snapshot(value))
        }
        _ => Err(InterpreterError::TypeMismatch),
    }
}

#[derive(Clone, Debug)]
pub struct RealmConfig {
    pub realm_id: u32,
    pub runtime_limits: RuntimeLimits,
    pub max_modules: u32,
    pub max_heap_objects: u32,
    pub max_host_resources: u32,
    pub release_capacity: usize,
    pub tombstone_capacity: usize,
    pub cost_table: OpcodeCostTable,
    pub migration_limits: MigrationLimits,
}

impl Default for RealmConfig {
    fn default() -> Self {
        Self {
            realm_id: 1,
            runtime_limits: RuntimeLimits::default(),
            max_modules: 16,
            max_heap_objects: 4_096,
            max_host_resources: 1_024,
            release_capacity: 2_048,
            tombstone_capacity: 1_024,
            cost_table: OpcodeCostTable::default(),
            migration_limits: MigrationLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingReason {
    Fuel,
    ExplicitYield,
    HostRequest,
    ReloadPause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionRoute {
    Delivered,
    BufferedForReload,
    DiscardedForCommittedEpoch,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadCompletionStats {
    pub buffered: u64,
    pub replayed: u64,
    pub discarded_after_commit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    OwnerDestroyed,
    ScopeCancelled,
    BudgetExceeded,
    RuntimeShutdown,
    ReloadCommit,
    HostCancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollResult<T> {
    Completed(T),
    Pending(PendingReason),
    Cancelled(CancelReason),
    Trapped(Trap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskTerminalReason {
    Completed(Option<RuntimeValue>),
    Cancelled(CancelReason),
    Trapped(Trap),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTerminalRecord {
    pub state: TaskState,
    pub reason: TaskTerminalReason,
    pub module_epoch: u64,
    pub final_charge: ExecutionCharge,
    pub trace_range: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickBudget {
    pub max_tasks: usize,
    pub frame_fuel_budget: u64,
    pub collect_garbage: bool,
}

impl Default for TickBudget {
    fn default() -> Self {
        Self {
            max_tasks: 64,
            frame_fuel_budget: 1_024,
            collect_garbage: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub polled: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub trapped: usize,
    pub releases: usize,
    pub collection: Option<CollectionStats>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeCapacityReport {
    pub module_slots: usize,
    pub task_slots: usize,
    pub scope_slots: usize,
    pub trace_records: usize,
    pub scheduler_ready: usize,
    pub scheduler_waiting: usize,
    pub host_requests: usize,
    pub release_records: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealmError {
    Runtime(RuntimeError),
    Interpreter(InterpreterError),
    Host(HostRequestError),
    Heap(HeapError),
    ModuleAllocation(SlotAllocError),
    ModuleHandle(crate::HandleError),
    MissingModule(u32),
    HostCapabilitiesUnavailable,
    MissingHostInterfaceHash,
    RuntimeHostClosing,
    RuntimeHostClosed,
    HostHashMismatch,
    SchemaHashMismatch,
    EpochExhausted,
    ModuleNotCallable,
    TerminalTask,
    TaskWaiting,
    Reload(ReloadError),
    State(crate::StatefulError),
}

impl fmt::Display for RealmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for RealmError {}

impl From<RuntimeError> for RealmError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<InterpreterError> for RealmError {
    fn from(error: InterpreterError) -> Self {
        Self::Interpreter(error)
    }
}

impl From<HostRequestError> for RealmError {
    fn from(error: HostRequestError) -> Self {
        Self::Host(error)
    }
}

impl From<HeapError> for RealmError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

impl From<ReloadError> for RealmError {
    fn from(error: ReloadError) -> Self {
        Self::Reload(error)
    }
}

impl From<crate::StatefulError> for RealmError {
    fn from(error: crate::StatefulError) -> Self {
        Self::State(error)
    }
}

pub struct RealmRuntime {
    realm_id: u32,
    modules: SlotPool<ModuleEpochRoot>,
    active_root: Option<ModuleHandle>,
    root_publications: VecDeque<RootPublicationRecord>,
    next_publication_id: u64,
    tasks: TaskRuntime,
    resources: RuntimeResources,
    heap: Heap,
    scheduler: Scheduler,
    cost_table: OpcodeCostTable,
    tombstones: VecDeque<(TaskHandle, TaskTerminalRecord)>,
    tombstone_capacity: usize,
    next_epoch: u64,
    next_stateful_domain: u64,
    retired_epochs: RetiredEpochRegistry,
    reload: ReloadCoordinator,
    migration_limits: MigrationLimits,
    reload_completion_capacity: usize,
    reload_completion_stats: ReloadCompletionStats,
    host_registry: Option<Box<dyn HostRegistry>>,
    host_registry_hash: Option<StableId>,
    runtime_host: Option<RuntimeHost>,
}

impl RealmRuntime {
    fn base(config: RealmConfig) -> Self {
        Self {
            realm_id: config.realm_id,
            modules: SlotPool::with_capacity_limit(config.realm_id, config.max_modules),
            active_root: None,
            root_publications: VecDeque::with_capacity(config.max_modules as usize),
            next_publication_id: 1,
            tasks: TaskRuntime::new(config.realm_id, config.runtime_limits),
            resources: RuntimeResources::new(
                config.realm_id,
                config.max_host_resources,
                config.release_capacity,
            ),
            heap: Heap::new(config.max_heap_objects),
            scheduler: Scheduler::with_capacity(
                config.runtime_limits.max_scheduler_tokens as usize,
            ),
            cost_table: config.cost_table,
            tombstones: VecDeque::with_capacity(config.tombstone_capacity),
            tombstone_capacity: config.tombstone_capacity,
            next_epoch: 1,
            next_stateful_domain: 1,
            retired_epochs: RetiredEpochRegistry::new(config.max_modules as usize),
            reload: ReloadCoordinator::default(),
            migration_limits: config.migration_limits,
            reload_completion_capacity: config.max_host_resources as usize,
            reload_completion_stats: ReloadCompletionStats::default(),
            host_registry: None,
            host_registry_hash: None,
            runtime_host: None,
        }
    }

    #[must_use]
    pub fn isolated(config: RealmConfig) -> Self {
        Self::base(config)
    }

    pub fn hosted(
        config: RealmConfig,
        runtime_host: RuntimeHost,
        registry: Box<dyn HostRegistry>,
    ) -> Result<Self, RealmError> {
        let host_registry_hash = registry
            .interface_hash()
            .ok_or(RealmError::MissingHostInterfaceHash)?;
        runtime_host.register_realm().map_err(|state| match state {
            RuntimeHostState::Closing => RealmError::RuntimeHostClosing,
            RuntimeHostState::Closed => RealmError::RuntimeHostClosed,
            RuntimeHostState::Open => unreachable!("open hosts admit realms"),
        })?;
        let resource_config = config.clone();
        let mut realm = Self::base(config);
        realm.host_registry = Some(registry);
        realm.host_registry_hash = Some(host_registry_hash);
        realm.resources = RuntimeResources::with_runtime_host(
            resource_config.realm_id,
            resource_config.max_host_resources,
            resource_config.release_capacity,
            &runtime_host,
        );
        realm.runtime_host = Some(runtime_host);
        Ok(realm)
    }

    pub fn create_scope(&mut self, parent: Option<ScopeHandle>) -> Result<ScopeHandle, RealmError> {
        Ok(self.tasks.create_scope(parent)?)
    }

    pub fn scope_snapshot(&self, scope: ScopeHandle) -> Result<crate::ScopeSnapshot, RealmError> {
        Ok(self.tasks.scope_snapshot(scope)?)
    }

    #[must_use]
    pub const fn realm_id(&self) -> u32 {
        self.realm_id
    }

    #[must_use]
    pub fn reserved_capacities(&self) -> RuntimeCapacityReport {
        let (task_slots, scope_slots, trace_records) = self.tasks.reserved_capacities();
        let (scheduler_ready, scheduler_waiting) = self.scheduler.reserved_capacities();
        let (host_requests, release_records) = self.resources.reserved_capacities();
        RuntimeCapacityReport {
            module_slots: self.modules.reserved_capacity(),
            task_slots,
            scope_slots,
            trace_records,
            scheduler_ready,
            scheduler_waiting,
            host_requests,
            release_records,
        }
    }

    pub fn module_epoch(&self, module: ModuleHandle) -> Result<u64, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .epoch)
    }

    pub fn module_stateful_domain(
        &self,
        module: ModuleHandle,
    ) -> Result<StatefulDomainId, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .stateful_domain)
    }

    #[must_use]
    pub const fn active_root(&self) -> Option<ModuleHandle> {
        self.active_root
    }

    pub fn module_lifecycle(&self, module: ModuleHandle) -> Result<ModuleLifecycle, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle)
    }

    #[must_use]
    pub fn root_publications(&self) -> &VecDeque<RootPublicationRecord> {
        &self.root_publications
    }

    #[must_use]
    pub fn retired_epochs(&self) -> &VecDeque<RetiredEpochSnapshot> {
        &self.retired_epochs.entries
    }

    pub fn insert_state(
        &mut self,
        module: ModuleHandle,
        stable_id: StableId,
        value: crate::StateValue,
    ) -> Result<crate::StateHandle, RealmError> {
        Ok(self
            .modules
            .resolve_mut(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .insert(stable_id, value)?)
    }

    pub fn state_handles(
        &self,
        module: ModuleHandle,
    ) -> Result<Vec<crate::StateHandle>, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .handles())
    }

    pub fn resolve_state(
        &self,
        module: ModuleHandle,
        handle: crate::StateHandle,
    ) -> Result<crate::StateValue, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .resolve(handle)?)
    }

    #[must_use]
    pub fn migration_capacity_report(&self) -> crate::MigrationCapacityReport {
        self.migration_limits.capacity_report()
    }

    pub fn load_module(
        &mut self,
        verified: VerifiedModule,
        host_hash: StableId,
        schema_hash: StableId,
    ) -> Result<ModuleHandle, RealmError> {
        if self.runtime_host.is_none() && module_requires_host_capabilities(verified.module()) {
            return Err(RealmError::HostCapabilitiesUnavailable);
        }
        if self
            .host_registry_hash
            .is_some_and(|registry_hash| registry_hash != host_hash)
        {
            return Err(RealmError::HostHashMismatch);
        }
        if verified.module().host_interface_hash != Some(host_hash) {
            return Err(RealmError::HostHashMismatch);
        }
        if verified.module().schema_hash != Some(schema_hash) {
            return Err(RealmError::SchemaHashMismatch);
        }
        let epoch = self.next_epoch;
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        let stateful_domain = StatefulDomainId::new(self.next_stateful_domain);
        self.next_stateful_domain = self
            .next_stateful_domain
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        let raw = self
            .modules
            .try_allocate(ModuleEpochRoot {
                module_id: 0,
                stateful_domain,
                epoch,
                verified: Arc::new(verified),
                host_hash,
                schema_hash,
                lifecycle: ModuleLifecycle::Active,
                globals: Vec::new(),
                state: StatefulRegistry::new(stateful_domain),
                staging_roots: Vec::new(),
            })
            .map_err(RealmError::ModuleAllocation)?;
        let loaded = self
            .modules
            .resolve_mut(raw)
            .expect("new module handle resolves");
        loaded.module_id = raw.index;
        let handle = ModuleHandle(raw);
        if self.active_root.is_none() {
            self.set_active_root(handle);
        }
        Ok(handle)
    }

    pub fn prepare_reload(
        &mut self,
        old_module: ModuleHandle,
        candidate: VerifiedModule,
        host_hash: StableId,
        schema_hash: StableId,
    ) -> Result<ModuleHandle, RealmError> {
        if self.reload.active() {
            return Err(ReloadError::InvalidState.into());
        }
        let old = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if old.verified.module().host_interface_hash != Some(host_hash) {
            return Err(RealmError::HostHashMismatch);
        }
        if old.verified.module().schema_hash != Some(schema_hash) {
            return Err(RealmError::SchemaHashMismatch);
        }
        let stateful_domain = old.stateful_domain;
        let candidate = self.load_module(candidate, host_hash, schema_hash)?;
        let candidate_root = self
            .modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        candidate_root.lifecycle = ModuleLifecycle::Staging;
        candidate_root.stateful_domain = stateful_domain;
        candidate_root.state = StatefulRegistry::new(stateful_domain);
        self.reload.begin(ReloadTransaction {
            old_module,
            candidate,
            paused_tasks: Vec::new(),
            completions: ReloadCompletionBuffer::new(self.reload_completion_capacity),
        })?;
        Ok(candidate)
    }

    pub fn prepare_reload_migrating(
        &mut self,
        old_module: ModuleHandle,
        candidate: VerifiedModule,
        host_hash: StableId,
    ) -> Result<ModuleHandle, RealmError> {
        if self.reload.active() {
            return Err(ReloadError::InvalidState.into());
        }
        let old = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if old.verified.module().host_interface_hash != Some(host_hash) {
            return Err(RealmError::HostHashMismatch);
        }
        let old_schema = old
            .verified
            .module()
            .schema_hash
            .ok_or(RealmError::SchemaHashMismatch)?;
        let stateful_domain = old.stateful_domain;
        let candidate_schema = candidate
            .module()
            .schema_hash
            .ok_or(RealmError::SchemaHashMismatch)?;
        if candidate_schema != old_schema
            && !candidate
                .module()
                .functions
                .iter()
                .any(|function| function.effect == nexa_bytecode::FunctionEffect::Migration)
        {
            return Err(
                ReloadError::Migration("schema changes require a migration entry".into()).into(),
            );
        }
        let candidate = self.load_module(candidate, host_hash, candidate_schema)?;
        let candidate_root = self
            .modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        candidate_root.lifecycle = ModuleLifecycle::Staging;
        candidate_root.stateful_domain = stateful_domain;
        candidate_root.state = StatefulRegistry::new(stateful_domain);
        self.reload.begin(ReloadTransaction {
            old_module,
            candidate,
            paused_tasks: Vec::new(),
            completions: ReloadCompletionBuffer::new(self.reload_completion_capacity),
        })?;
        Ok(candidate)
    }

    pub fn quiesce_reload(&mut self) -> Result<usize, RealmError> {
        let old_module = self.reload.transaction()?.old_module;
        let old_id = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .module_id;
        let tasks = self
            .tasks
            .task_handles()
            .into_iter()
            .filter(|task| {
                self.tasks
                    .task_snapshot(*task)
                    .is_ok_and(|snapshot| snapshot.module_id == old_id)
            })
            .collect::<Vec<_>>();
        for task in &tasks {
            let snapshot = self.tasks.task_snapshot(*task)?;
            let execution = self.tasks.execution_checkpoint(*task)?;
            let scheduler = self.scheduler.checkpoint(*task);
            match snapshot.state {
                TaskState::Ready => {
                    self.tasks.poll_task(*task)?;
                    self.tasks.pause_task_for_reload(*task)?;
                }
                TaskState::Running => self.tasks.pause_task_for_reload(*task)?,
                TaskState::FuelYielded | TaskState::ExplicitYielded | TaskState::Waiting => {
                    self.tasks.request_reload_pause(*task)?;
                }
                _ => {}
            }
            if self.tasks.task_snapshot(*task)?.state == TaskState::ReloadPaused {
                self.tasks.mark_execution_reload_paused(*task)?;
                self.scheduler.cancel_task(*task);
                self.reload
                    .transaction_mut()?
                    .paused_tasks
                    .push(crate::reload::PausedTask {
                        handle: *task,
                        snapshot,
                        execution,
                        scheduler,
                    });
            }
        }
        self.reload.quiesced()?;
        Ok(tasks.len())
    }

    pub fn stage_reload(
        &mut self,
        migration_function: u32,
        arguments: &[RuntimeValue],
    ) -> Result<Option<RuntimeValue>, RealmError> {
        self.buffer_reload_completions()?;
        let candidate_handle = self.reload.transaction()?.candidate;
        let candidate = self
            .modules
            .resolve(candidate_handle.raw())
            .map_err(RealmError::ModuleHandle)?;
        let function = candidate
            .verified
            .module()
            .functions
            .get(migration_function as usize)
            .ok_or(RealmError::MissingModule(migration_function))?;
        if function.effect != nexa_bytecode::FunctionEffect::Migration {
            return Err(ReloadError::Migration(
                "migration entry does not have Migration effect".into(),
            )
            .into());
        }
        let candidate_domain = candidate.stateful_domain;
        let candidate_schema = candidate.verified.module().state_schema.clone();
        let old_module = self.reload.transaction()?.old_module;
        let old_root = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let schema_unchanged = old_root.verified.module().state_schema == candidate_schema;
        let old_state = old_root.state.clone();
        let mut migration = crate::stateful::MigrationContext::new(
            old_state,
            candidate_domain,
            candidate_schema,
            schema_unchanged,
            self.migration_limits,
        )?;
        let execution = CheckedInterpreter::run_migration(
            &candidate.verified,
            migration_function,
            arguments,
            self.migration_limits.max_fuel,
            crate::FrameLimits {
                max_call_depth: u32::from(self.migration_limits.max_call_depth),
                ..crate::FrameLimits::default()
            },
            &mut migration,
        );
        if let Some(error) = migration.limit_error() {
            return Err(ReloadError::MigrationLimit(error).into());
        }
        let execution = match execution {
            Err(InterpreterError::ContinuationLimit(crate::FrameError::CallDepthLimit)) => {
                return Err(ReloadError::MigrationLimit(MigrationLimitError::CallDepth).into());
            }
            Err(error) => return Err(error.into()),
            Ok(execution) => execution,
        };
        match execution {
            InterpreterOutcome::Returned { value, .. } => {
                let migrated = migration.finish()?;
                self.modules
                    .resolve_mut(candidate_handle.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .state = migrated;
                self.reload.staged()?;
                Ok(value)
            }
            InterpreterOutcome::Suspended { reason, .. } => {
                if reason == SuspendReason::Fuel {
                    Err(ReloadError::MigrationLimit(MigrationLimitError::Fuel).into())
                } else {
                    Err(ReloadError::Migration("migration attempted to suspend".into()).into())
                }
            }
            InterpreterOutcome::HostPending { .. } => {
                Err(ReloadError::Migration("migration attempted a host call".into()).into())
            }
            InterpreterOutcome::Trapped { trap, .. } => {
                Err(ReloadError::Migration(trap.message).into())
            }
        }
    }

    pub fn commit_reload(
        &mut self,
        activation: ActivationEntry<'_>,
    ) -> Result<ModuleHandle, RealmError> {
        self.buffer_reload_completions()?;
        let candidate = self.reload.transaction()?.candidate;
        let verified = self
            .modules
            .resolve(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .verified
            .clone();
        self.publish_reload_root()?;
        let activation_result = (|| {
            let function = verified
                .module()
                .functions
                .get(activation.function_id as usize)
                .ok_or_else(|| "activation function is missing".to_owned())?;
            if function.effect != nexa_bytecode::FunctionEffect::Immediate {
                return Err("activation entry must have Immediate effect".into());
            }
            match CheckedInterpreter::run_with_heap(
                &verified,
                activation.function_id,
                activation.arguments,
                activation.fuel,
                &mut self.heap,
            )
            .map_err(|error| error.to_string())?
            {
                InterpreterOutcome::Returned { .. } => Ok(()),
                InterpreterOutcome::Trapped { trap, .. } => Err(trap.message),
                InterpreterOutcome::Suspended { .. } | InterpreterOutcome::HostPending { .. } => {
                    Err("activation entry attempted to suspend".into())
                }
            }
        })();
        match activation_result {
            Ok(()) => {
                self.reload.activation_succeeded()?;
                self.modules
                    .resolve_mut(candidate.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .lifecycle = ModuleLifecycle::Active;
                self.discard_reload_completions()?;
                let transaction = self.reload.finish()?;
                Ok(transaction.candidate)
            }
            Err(error) => {
                self.reload.activation_failed()?;
                self.modules
                    .resolve_mut(candidate.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .lifecycle = ModuleLifecycle::ActivationFaulted;
                self.discard_reload_completions()?;
                self.reload.finish()?;
                Err(ReloadError::Activation(error).into())
            }
        }
    }

    pub fn rollback_reload(&mut self) -> Result<(), RealmError> {
        self.buffer_reload_completions()?;
        self.reload.rollback()?;
        let mut transaction = self.reload.finish()?;
        for paused in transaction.paused_tasks.drain(..) {
            self.tasks
                .restore_task_checkpoint(paused.handle, paused.snapshot, paused.execution)?;
            self.scheduler.restore(paused.handle, paused.scheduler);
        }
        self.modules
            .release(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        for delivery in transaction.completions.drain_ordered() {
            if !self.deliver_host_completion(delivery)? {
                return Err(ReloadError::InvalidState.into());
            }
            self.reload_completion_stats.replayed =
                self.reload_completion_stats.replayed.saturating_add(1);
        }
        Ok(())
    }

    #[must_use]
    pub fn reload_buffered_completions(&self) -> usize {
        self.reload
            .transaction()
            .map_or(0, |transaction| transaction.completions.len())
    }

    #[must_use]
    pub const fn reload_completion_stats(&self) -> ReloadCompletionStats {
        self.reload_completion_stats
    }

    fn discard_reload_completions(&mut self) -> Result<usize, RealmError> {
        self.buffer_reload_completions()?;
        let old = self.reload.transaction()?.old_module;
        let root = self
            .modules
            .resolve(old.raw())
            .map_err(RealmError::ModuleHandle)?;
        let old_identity = (root.module_id, root.epoch);
        let transaction = self.reload.transaction_mut()?;
        let mut discarded = 0_u64;
        for delivery in transaction.completions.drain_ordered() {
            debug_assert_eq!(
                (delivery.module_id, delivery.epoch),
                old_identity,
                "reload buffer contains an unrelated completion"
            );
            discarded = discarded.saturating_add(1);
        }
        self.reload_completion_stats.discarded_after_commit = self
            .reload_completion_stats
            .discarded_after_commit
            .saturating_add(discarded);
        usize::try_from(discarded).map_err(|_| ReloadError::CompletionBufferCapacity.into())
    }

    fn publish_reload_root(&mut self) -> Result<(), RealmError> {
        let transaction = self.reload.transaction()?;
        let old = transaction.old_module;
        let candidate = transaction.candidate;
        if self.active_root != Some(old) {
            return Err(ReloadError::InvalidState.into());
        }
        let publication_id = self.next_publication_id;
        let next_publication_id = publication_id
            .checked_add(1)
            .ok_or(RealmError::EpochExhausted)?;
        let candidate_epoch = self.module_epoch(candidate)?;
        self.reload.publish()?;
        self.set_active_root(candidate);
        self.next_publication_id = next_publication_id;
        if self.root_publications.len() == self.root_publications.capacity() {
            self.root_publications.pop_front();
        }
        self.root_publications.push_back(RootPublicationRecord {
            publication_id,
            old_root: old,
            candidate_root: candidate,
            candidate_epoch,
        });
        self.reload.begin_activation()?;
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Activating;
        self.modules
            .resolve_mut(old.raw())
            .map_err(RealmError::ModuleHandle)?
            .lifecycle = ModuleLifecycle::Retired;
        let paused_tasks = self
            .reload
            .transaction()?
            .paused_tasks
            .iter()
            .map(|paused| paused.handle)
            .collect::<Vec<_>>();
        for task in paused_tasks {
            self.cancel_task(task, CancelReason::ReloadCommit)?;
        }
        self.register_retired_epoch(old)?;
        Ok(())
    }

    fn set_active_root(&mut self, root: ModuleHandle) {
        self.active_root = Some(root);
    }

    pub fn call(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        if self
            .reload
            .transaction()
            .is_ok_and(|transaction| transaction.old_module == module)
        {
            return Err(ReloadError::InvalidState.into());
        }
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if loaded.lifecycle != ModuleLifecycle::Active {
            return Err(RealmError::ModuleNotCallable);
        }
        let reservation = reservation_for_module(&loaded.verified, config.limits.frames);
        let continuation = CheckedInterpreter::start(
            &loaded.verified,
            function,
            arguments,
            config.limits.frames,
            reservation,
        )?;
        let task = self.tasks.admit_task(config.owner, loaded.epoch, true)?;
        if let Err(error) = self.tasks.attach_continuation(
            task,
            config.priority,
            FuelState::new(config.fuel_slice, 0, config.cumulative_budget),
            continuation,
            loaded.module_id,
            config.limits,
        ) {
            self.tasks.finish_task(task)?;
            return Err(error.into());
        }
        self.scheduler.schedule(task, config.priority);
        Ok(task)
    }

    pub fn spawn(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        self.call(module, function, arguments, config)
    }

    #[allow(clippy::too_many_lines)]
    pub fn poll_task(
        &mut self,
        task: TaskHandle,
        fuel_slice: u64,
    ) -> Result<PollResult<Option<RuntimeValue>>, RealmError> {
        if self
            .tombstones
            .iter()
            .any(|(terminal_task, _)| *terminal_task == task)
        {
            return Err(RealmError::TerminalTask);
        }
        self.scheduler.deschedule(task);
        let snapshot = self.tasks.task_snapshot(task)?;
        crate::allocation::record(
            if snapshot.fuel.cumulative_used == 0 {
                crate::allocation::AllocationPhase::FirstSlice
            } else {
                crate::allocation::AllocationPhase::Resume
            },
            0,
        );
        match snapshot.state {
            TaskState::Ready => self.tasks.poll_task(task)?,
            TaskState::FuelYielded => self.tasks.resume_fuel_task(task)?,
            TaskState::ExplicitYielded => self.tasks.resume_explicit_task(task)?,
            TaskState::Waiting => return Ok(PollResult::Pending(PendingReason::HostRequest)),
            TaskState::ReloadPaused => return Ok(PollResult::Pending(PendingReason::ReloadPause)),
            TaskState::Running => {}
            _ => return Err(RealmError::TerminalTask),
        }
        let execution = self.tasks.take_execution(task)?;
        let continuation = match execution {
            TaskExecution::Ready(continuation)
            | TaskExecution::Running(continuation)
            | TaskExecution::FuelYielded(continuation)
            | TaskExecution::ExplicitYielded(continuation)
            | TaskExecution::Cancelling(continuation)
            | TaskExecution::Cleanup(continuation) => continuation,
            TaskExecution::Waiting {
                continuation,
                request,
                destination,
                expected_type,
                async_result,
            } => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
                        async_result,
                    },
                    snapshot.fuel,
                )?;
                return Err(RealmError::TaskWaiting);
            }
            TaskExecution::ReloadPaused(continuation) => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::ReloadPaused(continuation),
                    snapshot.fuel,
                )?;
                return Ok(PollResult::Pending(PendingReason::ReloadPause));
            }
        };
        let module_raw = self
            .modules
            .occupied_handles_iter()
            .find(|raw| raw.index == snapshot.module_id)
            .ok_or(RealmError::MissingModule(snapshot.module_id))?;
        let module = self
            .modules
            .resolve(module_raw)
            .map_err(RealmError::ModuleHandle)?;
        let fuel = FuelState::new(
            fuel_slice,
            snapshot.fuel.cumulative_used,
            snapshot.fuel.cumulative_limit,
        );
        let trace_start = self.trace_cursor();
        let outcome = if let Some(registry) = self.host_registry.as_deref_mut() {
            let mut bridge = RealmHostBridge {
                registry,
                resources: &mut self.resources,
                task,
                module_id: snapshot.module_id,
                epoch: snapshot.module_epoch,
                imports: &module.verified.module().host_imports,
            };
            CheckedInterpreter::poll_with_host_and_heap(
                &module.verified,
                continuation,
                fuel,
                &self.cost_table,
                &mut bridge,
                &mut self.heap,
            )?
        } else {
            CheckedInterpreter::poll_with_heap(
                &module.verified,
                continuation,
                fuel,
                &self.cost_table,
                &mut self.heap,
            )?
        };
        match outcome {
            InterpreterOutcome::Returned {
                value,
                charge,
                fuel,
            } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                self.finish_task(
                    task,
                    snapshot.module_epoch,
                    trace_start,
                    final_charge,
                    value,
                )?;
                let _ = fuel;
                Ok(PollResult::Completed(value))
            }
            InterpreterOutcome::Suspended {
                continuation,
                reason,
                charge,
                fuel,
            } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                crate::allocation::record(crate::allocation::AllocationPhase::Promotion, 0);
                if continuation.cumulative_exhausted() {
                    if let Some(trap) = self.cancel_task_internal(
                        task,
                        CancelReason::BudgetExceeded,
                        snapshot.module_epoch,
                        trace_start,
                        final_charge,
                    )? {
                        return Ok(PollResult::Trapped(trap));
                    }
                    return Ok(PollResult::Cancelled(CancelReason::BudgetExceeded));
                }
                let pending = match reason {
                    SuspendReason::Fuel => PendingReason::Fuel,
                    SuspendReason::ExplicitYield => PendingReason::ExplicitYield,
                    SuspendReason::HostRequest => PendingReason::HostRequest,
                    SuspendReason::ReloadPause => PendingReason::ReloadPause,
                };
                let execution = match reason {
                    SuspendReason::Fuel => {
                        self.tasks.yield_fuel_task(task)?;
                        TaskExecution::FuelYielded(continuation)
                    }
                    SuspendReason::ExplicitYield => {
                        self.tasks.yield_explicit_task(task)?;
                        TaskExecution::ExplicitYielded(continuation)
                    }
                    SuspendReason::HostRequest | SuspendReason::ReloadPause => {
                        return Err(RealmError::TaskWaiting);
                    }
                };
                self.tasks.put_execution(task, execution, fuel)?;
                self.scheduler.schedule(task, snapshot.priority);
                Ok(PollResult::Pending(pending))
            }
            InterpreterOutcome::HostPending {
                continuation,
                request,
                destination,
                expected_type,
                async_result,
                charge,
                fuel,
            } => {
                self.tasks.record_charge(task, charge)?;
                self.tasks.await_task(task)?;
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
                        async_result,
                    },
                    fuel,
                )?;
                self.scheduler.wait_for(request, task);
                Ok(PollResult::Pending(PendingReason::HostRequest))
            }
            InterpreterOutcome::Trapped { trap, charge, .. } => {
                let final_charge = self.tasks.record_charge(task, charge)?;
                self.trap_task(
                    task,
                    snapshot.module_epoch,
                    trace_start,
                    final_charge,
                    trap.clone(),
                )?;
                Ok(PollResult::Trapped(trap))
            }
        }
    }

    pub fn cancel_scope(&mut self, scope: ScopeHandle) -> Result<usize, RealmError> {
        self.tasks.cancel_scope(scope)?;
        self.tasks.begin_scope_cancellation(scope)?;
        let tasks = self
            .tasks
            .task_handles()
            .into_iter()
            .filter(|task| {
                self.tasks
                    .task_snapshot(*task)
                    .is_ok_and(|snapshot| snapshot.owner == scope)
            })
            .collect::<Vec<_>>();
        for task in &tasks {
            self.cancel_task(*task, CancelReason::ScopeCancelled)?;
        }
        self.tasks.finish_scope_cancellation(scope)?;
        Ok(tasks.len())
    }

    pub fn cancel_task(
        &mut self,
        task: TaskHandle,
        reason: CancelReason,
    ) -> Result<(), RealmError> {
        let snapshot = self.tasks.task_snapshot(task)?;
        if snapshot.state == TaskState::Ready {
            self.tasks.poll_task(task)?;
        }
        let trace_start = self.trace_cursor();
        let _ = self.cancel_task_internal(
            task,
            reason,
            snapshot.module_epoch,
            trace_start,
            snapshot.charge,
        )?;
        Ok(())
    }

    pub fn tick(&mut self, budget: TickBudget) -> Result<TickReport, RealmError> {
        let completions = self.drain_host_completions()?;
        let mut report = TickReport::default();
        for _ in 0..budget.max_tasks {
            let Some(task) = self.scheduler.pop_ready() else {
                break;
            };
            match self.poll_task(task, budget.frame_fuel_budget) {
                Ok(PollResult::Completed(_)) => report.completed += 1,
                Ok(PollResult::Cancelled(_)) => report.cancelled += 1,
                Ok(PollResult::Trapped(_)) => report.trapped += 1,
                Ok(PollResult::Pending(_))
                | Err(RealmError::TerminalTask | RealmError::TaskWaiting) => {}
                Err(error) => return Err(error),
            }
            report.polled += 1;
        }
        report.releases = self.flush_releases();
        self.reap_retired_epochs();
        if budget.collect_garbage {
            report.collection = Some(self.collect_garbage()?);
        }
        let _ = completions;
        Ok(report)
    }

    pub fn collect_garbage(&mut self) -> Result<CollectionStats, RealmError> {
        let mut roots = GcRoots::default();
        for task in self.tasks.task_handles() {
            let snapshot = self.tasks.task_snapshot(task)?;
            let module = self.module_for_id(snapshot.module_id)?;
            let continuation = self.tasks.execution(task)?.continuation();
            roots
                .suspended_tasks
                .extend(continuation.checked_gc_roots(&module.verified)?);
        }
        for raw in self.modules.occupied_handles() {
            let module = self
                .modules
                .resolve(raw)
                .expect("occupied module handle resolves");
            roots.module_globals.extend_from_slice(&module.globals);
            roots.stateful_registry.extend(module.state.gc_roots());
            roots.staging_heap.extend_from_slice(&module.staging_roots);
        }
        Ok(self.heap.collect(&roots)?)
    }

    pub fn allocate(&mut self, object: Object) -> Result<GcRef, RealmError> {
        Ok(self.heap.allocate(object)?)
    }

    pub fn resolve_heap_object(&self, reference: GcRef) -> Result<&Object, RealmError> {
        Ok(self.heap.resolve(reference)?)
    }

    pub fn create_host_request(
        &mut self,
        task: TaskHandle,
    ) -> Result<PendingHostRequest, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_request()?)
    }

    pub fn wait_for_request(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
    ) -> Result<(), RealmError> {
        if !self.resources.owns_request(task, request) {
            return Err(RealmError::Host(HostRequestError::InvalidState));
        }
        let snapshot = self.tasks.task_snapshot(task)?;
        match snapshot.state {
            TaskState::Ready => self.tasks.poll_task(task)?,
            TaskState::FuelYielded => self.tasks.resume_fuel_task(task)?,
            TaskState::ExplicitYielded => self.tasks.resume_explicit_task(task)?,
            TaskState::Running => {}
            _ => return Err(RealmError::TaskWaiting),
        }
        let execution = self.tasks.take_execution(task)?;
        let continuation = match execution {
            TaskExecution::Ready(continuation)
            | TaskExecution::Running(continuation)
            | TaskExecution::FuelYielded(continuation)
            | TaskExecution::ExplicitYielded(continuation) => continuation,
            other => {
                self.tasks.put_execution(task, other, snapshot.fuel)?;
                return Err(RealmError::TaskWaiting);
            }
        };
        self.tasks.await_task(task)?;
        self.tasks.put_execution(
            task,
            TaskExecution::Waiting {
                continuation,
                request,
                destination: 0,
                expected_type: None,
                async_result: None,
            },
            snapshot.fuel,
        )?;
        self.scheduler.deschedule(task);
        self.scheduler.wait_for(request, task);
        Ok(())
    }

    pub fn with_resource_context<T>(
        &mut self,
        task: TaskHandle,
        operation: impl FnOnce(&mut crate::ResourceContext<'_>) -> T,
    ) -> Result<T, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        let mut context = self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch);
        Ok(operation(&mut context))
    }

    pub fn create_resource_token(
        &mut self,
        task: TaskHandle,
        domain: RuntimeHostDomain,
    ) -> Result<ResourceTokenHandle, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_token(domain)?)
    }

    pub fn create_snapshot(
        &mut self,
        task: TaskHandle,
        data: Arc<[i32]>,
    ) -> Result<SnapshotHandle, RealmError> {
        self.require_host_capabilities()?;
        self.require_reload_idle()?;
        let snapshot = self.tasks.task_snapshot(task)?;
        Ok(self
            .resources
            .context(task, snapshot.module_id, snapshot.module_epoch)
            .create_snapshot(data)?)
    }

    pub fn snapshot_data(&self, snapshot: SnapshotHandle) -> Result<&[i32], RealmError> {
        Ok(self.resources.snapshot_data(snapshot)?)
    }

    pub fn snapshot_external_bytes(&self, snapshot: SnapshotHandle) -> Result<usize, RealmError> {
        Ok(self.resources.snapshot_external_bytes(snapshot)?)
    }

    #[must_use]
    pub const fn discarded_late_host_results(&self) -> u64 {
        self.resources.discarded_late_results()
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> crate::RuntimeResourceSnapshot {
        self.resources.model_snapshot()
    }

    pub fn task_snapshot(&self, task: TaskHandle) -> Result<crate::TaskSnapshot, RealmError> {
        Ok(self.tasks.task_snapshot(task)?)
    }

    #[must_use]
    pub fn request_terminal_record(
        &self,
        request: HostRequestHandle,
    ) -> Option<&crate::RequestTerminalRecord> {
        self.resources.request_terminal_record(request)
    }

    #[must_use]
    pub fn terminal_record(&self, task: TaskHandle) -> Option<&TaskTerminalRecord> {
        self.tombstones
            .iter()
            .find_map(|(terminal_task, record)| (*terminal_task == task).then_some(record))
    }

    #[must_use]
    pub fn trace(&self) -> &RuntimeTrace {
        self.tasks.trace()
    }

    pub fn set_trace_enabled(&mut self, enabled: bool) {
        self.tasks.set_trace_enabled(enabled);
    }

    fn drain_host_completions(&mut self) -> Result<usize, RealmError> {
        let mut count = 0;
        for delivery in self.resources.drain_completions() {
            self.route_host_completion(delivery)?;
            count += 1;
        }
        Ok(count)
    }

    fn buffer_reload_completions(&mut self) -> Result<usize, RealmError> {
        let deliveries = self.resources.drain_completions();
        let count = deliveries.len();
        for delivery in deliveries {
            self.route_host_completion(delivery)?;
        }
        Ok(count)
    }

    fn route_host_completion(
        &mut self,
        delivery: HostCompletionDelivery,
    ) -> Result<CompletionRoute, RealmError> {
        if delivery.realm_id != self.realm_id {
            return Err(ReloadError::InvalidState.into());
        }
        if self.reload.active() {
            let old = self.reload.transaction()?.old_module;
            let root = self
                .modules
                .resolve(old.raw())
                .map_err(RealmError::ModuleHandle)?;
            if delivery.module_id == root.module_id && delivery.epoch == root.epoch {
                self.reload.transaction_mut()?.completions.push(delivery)?;
                self.reload_completion_stats.buffered =
                    self.reload_completion_stats.buffered.saturating_add(1);
                return Ok(CompletionRoute::BufferedForReload);
            }
        }
        if self.completion_targets_committed_epoch(&delivery) {
            self.reload_completion_stats.discarded_after_commit = self
                .reload_completion_stats
                .discarded_after_commit
                .saturating_add(1);
            return Ok(CompletionRoute::DiscardedForCommittedEpoch);
        }
        let _ = self.deliver_host_completion(delivery)?;
        Ok(CompletionRoute::Delivered)
    }

    fn completion_targets_committed_epoch(&self, delivery: &HostCompletionDelivery) -> bool {
        self.modules.occupied_handles_iter().any(|raw| {
            self.modules.resolve(raw).is_ok_and(|root| {
                root.module_id == delivery.module_id
                    && root.epoch == delivery.epoch
                    && root.lifecycle == ModuleLifecycle::Retired
            })
        })
    }

    fn deliver_host_completion(
        &mut self,
        delivery: HostCompletionDelivery,
    ) -> Result<bool, RealmError> {
        let request = delivery.request;
        let Some(task) = self.scheduler.wake_request(request) else {
            return Ok(false);
        };
        let snapshot = self.tasks.task_snapshot(task)?;
        if snapshot.state != TaskState::Waiting {
            return Ok(false);
        }
        self.handle_host_completion(task, request, snapshot, delivery.result)?;
        Ok(true)
    }

    fn handle_host_completion(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
        snapshot: crate::TaskSnapshot,
        result: HostCompletionResult,
    ) -> Result<(), RealmError> {
        match result {
            HostCompletionResult::Success(payload) => {
                let execution = self.tasks.take_execution(task)?;
                let TaskExecution::Waiting {
                    mut continuation,
                    request: waiting_request,
                    destination,
                    expected_type,
                    async_result,
                } = execution
                else {
                    return Err(RealmError::TaskWaiting);
                };
                if waiting_request != request {
                    return Err(RealmError::TaskWaiting);
                }
                let value = if let Some(async_result) = async_result {
                    let payload = completion_to_runtime(payload, Some(async_result.success))?;
                    self.allocate_async_result(async_result, true, payload)
                } else {
                    completion_to_runtime(payload, expected_type)
                };
                if let Ok(value) = value {
                    continuation.write_resume_value(destination, expected_type, value)?;
                    self.tasks.resume_waiting_task(task)?;
                    self.tasks.put_execution(
                        task,
                        TaskExecution::Running(continuation),
                        snapshot.fuel,
                    )?;
                    self.scheduler.schedule(task, snapshot.priority);
                } else {
                    self.trap_host_task(
                        task,
                        snapshot,
                        "host completion payload type mismatch".into(),
                    )?;
                }
            }
            HostCompletionResult::Error(error) => {
                self.resume_async_error(task, request, snapshot, error.code)?;
            }
            HostCompletionResult::Cancelled => {
                if let Some(result) = self.waiting_async_result(task)? {
                    if result.cancel_policy == CancelPolicy::ReturnError {
                        self.resume_async_error(
                            task,
                            request,
                            snapshot,
                            result.cancel_error.ok_or(RealmError::TaskWaiting)?,
                        )?;
                    } else {
                        self.cancel_waiting_host_task(task, snapshot)?;
                    }
                } else {
                    self.cancel_waiting_host_task(task, snapshot)?;
                }
            }
            HostCompletionResult::Abandoned => {
                if let Some(result) = self.waiting_async_result(task)? {
                    if result.abandon_policy == AbandonPolicy::ReturnError {
                        self.resume_async_error(
                            task,
                            request,
                            snapshot,
                            result.abandon_error.ok_or(RealmError::TaskWaiting)?,
                        )?;
                    } else {
                        self.trap_host_task(task, snapshot, "host request was abandoned".into())?;
                    }
                } else {
                    self.trap_host_task(task, snapshot, "host request was abandoned".into())?;
                }
            }
        }
        Ok(())
    }

    fn cancel_waiting_host_task(
        &mut self,
        task: TaskHandle,
        snapshot: crate::TaskSnapshot,
    ) -> Result<(), RealmError> {
        let _ = self.cancel_task_internal(
            task,
            CancelReason::HostCancelled,
            snapshot.module_epoch,
            self.trace_cursor(),
            snapshot.charge,
        )?;
        Ok(())
    }

    fn waiting_async_result(
        &self,
        task: TaskHandle,
    ) -> Result<Option<AsyncResultType>, RealmError> {
        match self.tasks.execution(task)? {
            TaskExecution::Waiting { async_result, .. } => Ok(*async_result),
            _ => Err(RealmError::TaskWaiting),
        }
    }

    fn resume_async_error(
        &mut self,
        task: TaskHandle,
        request: HostRequestHandle,
        snapshot: crate::TaskSnapshot,
        code: u32,
    ) -> Result<(), RealmError> {
        let execution = self.tasks.take_execution(task)?;
        let TaskExecution::Waiting {
            mut continuation,
            request: waiting_request,
            destination,
            expected_type,
            async_result,
        } = execution
        else {
            return Err(RealmError::TaskWaiting);
        };
        if waiting_request != request {
            return Err(RealmError::TaskWaiting);
        }
        let Some(async_result) = async_result else {
            self.tasks.put_execution(
                task,
                TaskExecution::Waiting {
                    continuation,
                    request,
                    destination,
                    expected_type,
                    async_result: None,
                },
                snapshot.fuel,
            )?;
            return self.trap_host_task(
                task,
                snapshot,
                format!("host request failed with code {code}"),
            );
        };
        let payload = match async_result.error {
            ValueType::I32 => RuntimeValue::I32(i32::from_ne_bytes(code.to_ne_bytes())),
            ValueType::Named(type_id) => {
                let variant = self
                    .module_for_id(snapshot.module_id)?
                    .verified
                    .module()
                    .enum_types
                    .iter()
                    .find(|enum_type| enum_type.type_id == type_id)
                    .and_then(|enum_type| {
                        enum_type
                            .variants
                            .iter()
                            .find(|variant| variant.tag == code)
                    })
                    .map(|variant| (variant.stable_id, variant.tag));
                if let Some((variant, tag)) = variant {
                    self.heap
                        .allocate_enum(type_id, variant, tag, None)
                        .map_err(RealmError::Heap)?
                } else {
                    self.tasks.put_execution(
                        task,
                        TaskExecution::Waiting {
                            continuation,
                            request,
                            destination,
                            expected_type,
                            async_result: Some(async_result),
                        },
                        snapshot.fuel,
                    )?;
                    return self.trap_host_task(
                        task,
                        snapshot,
                        format!("host error enum has no variant for code {code}"),
                    );
                }
            }
            ValueType::Bool | ValueType::Ref => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
                        async_result: Some(async_result),
                    },
                    snapshot.fuel,
                )?;
                return self.trap_host_task(
                    task,
                    snapshot,
                    "host error payload type mismatch".into(),
                );
            }
        };
        let value = self.allocate_async_result(async_result, false, payload)?;
        continuation.write_resume_value(destination, expected_type, value)?;
        self.tasks.resume_waiting_task(task)?;
        self.tasks
            .put_execution(task, TaskExecution::Running(continuation), snapshot.fuel)?;
        self.scheduler.schedule(task, snapshot.priority);
        Ok(())
    }

    fn allocate_async_result(
        &mut self,
        result: AsyncResultType,
        success: bool,
        payload: RuntimeValue,
    ) -> Result<RuntimeValue, InterpreterError> {
        let (variant, tag) = if success {
            (StableId::from_parts(&["Result", "::Ok"]), 0)
        } else {
            (StableId::from_parts(&["Result", "::Err"]), 1)
        };
        Ok(self
            .heap
            .allocate_enum(result.result_type, variant, tag, Some(payload))?)
    }

    fn trap_host_task(
        &mut self,
        task: TaskHandle,
        snapshot: crate::TaskSnapshot,
        message: String,
    ) -> Result<(), RealmError> {
        self.tasks.request_task_cancel(task)?;
        self.tasks.reach_task_safepoint(task)?;
        self.tasks.mark_execution_cancelling(task)?;
        self.trap_task(
            task,
            snapshot.module_epoch,
            self.trace_cursor(),
            snapshot.charge,
            Trap {
                kind: TrapKind::Host,
                message,
            },
        )
    }

    fn module_for_id(&self, module_id: u32) -> Result<&ModuleEpochRoot, RealmError> {
        self.modules
            .occupied_handles_iter()
            .find(|raw| raw.index == module_id)
            .and_then(|raw| self.modules.resolve(raw).ok())
            .ok_or(RealmError::MissingModule(module_id))
    }

    fn trace_cursor(&self) -> usize {
        self.tasks.trace().records().last().map_or(0, |record| {
            usize::try_from(record.sequence.saturating_add(1)).unwrap_or(usize::MAX)
        })
    }

    fn finish_task(
        &mut self,
        task: TaskHandle,
        epoch: u64,
        trace_start: usize,
        charge: ExecutionCharge,
        value: Option<RuntimeValue>,
    ) -> Result<(), RealmError> {
        crate::allocation::record(crate::allocation::AllocationPhase::TerminalCleanup, 0);
        self.scheduler.cancel_task(task);
        self.resources.cleanup_task(task, false)?;
        self.tasks.finish_task(task)?;
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Completed,
                reason: TaskTerminalReason::Completed(value),
                module_epoch: epoch,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(())
    }

    fn trap_task(
        &mut self,
        task: TaskHandle,
        epoch: u64,
        trace_start: usize,
        charge: ExecutionCharge,
        trap: Trap,
    ) -> Result<(), RealmError> {
        self.scheduler.cancel_task(task);
        self.resources.cleanup_task(task, false)?;
        self.tasks.trap_task(task)?;
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Trapped,
                reason: TaskTerminalReason::Trapped(trap),
                module_epoch: epoch,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(())
    }

    fn cancel_task_internal(
        &mut self,
        task: TaskHandle,
        reason: CancelReason,
        epoch: u64,
        trace_start: usize,
        mut charge: ExecutionCharge,
    ) -> Result<Option<Trap>, RealmError> {
        self.scheduler.cancel_task(task);
        if reason == CancelReason::ReloadCommit {
            self.tasks.begin_reload_commit_cancel(task)?;
            self.tasks.mark_execution_cancelling(task)?;
        } else {
            self.tasks.request_task_cancel(task)?;
            self.tasks.reach_task_safepoint(task)?;
            self.tasks.mark_execution_cancelling(task)?;
        }
        let snapshot = self.tasks.task_snapshot(task)?;
        let has_user_defer = self
            .tasks
            .execution(task)?
            .continuation()
            .arena()
            .defers_rev()
            .next()
            .is_some();
        let run_user_cleanup = reason != CancelReason::ReloadCommit && has_user_defer;
        if run_user_cleanup {
            self.tasks.begin_cleanup(task)?;
            self.tasks.mark_execution_cleanup(task)?;
        }
        let cleanup = if run_user_cleanup {
            let module = self.module_for_id(snapshot.module_id)?;
            CheckedInterpreter::run_cleanup(
                &module.verified,
                self.tasks.execution(task)?.continuation(),
                snapshot.limits.max_cleanup_ops,
                snapshot.limits.max_cleanup_fuel,
                &self.cost_table,
            )?
        } else {
            Ok(ExecutionCharge::default())
        };
        self.resources
            .cleanup_task(task, reason == CancelReason::ReloadCommit)?;
        let cleanup_charge = match cleanup {
            Ok(cleanup_charge) => cleanup_charge,
            Err(trap) => {
                self.tasks.trap_task(task)?;
                self.record_terminal(
                    task,
                    TaskTerminalRecord {
                        state: TaskState::Trapped,
                        reason: TaskTerminalReason::Trapped(trap.clone()),
                        module_epoch: epoch,
                        final_charge: charge,
                        trace_range: trace_start..self.trace_cursor(),
                    },
                );
                return Ok(Some(trap));
            }
        };
        charge.instructions = charge
            .instructions
            .saturating_add(cleanup_charge.instructions);
        charge.fuel_used = charge.fuel_used.saturating_add(cleanup_charge.fuel_used);
        if run_user_cleanup {
            self.tasks.finish_cleanup(task)?;
        } else {
            self.tasks.finish_cancel_without_cleanup(task)?;
        }
        self.record_terminal(
            task,
            TaskTerminalRecord {
                state: TaskState::Cancelled,
                reason: TaskTerminalReason::Cancelled(reason),
                module_epoch: epoch,
                final_charge: charge,
                trace_range: trace_start..self.trace_cursor(),
            },
        );
        Ok(None)
    }

    fn record_terminal(&mut self, task: TaskHandle, record: TaskTerminalRecord) {
        if self.tombstone_capacity == 0 {
            return;
        }
        if self.tombstones.len() == self.tombstone_capacity {
            self.tombstones.pop_front();
        }
        self.tombstones.push_back((task, record));
    }

    fn flush_releases(&mut self) -> usize {
        debug_assert!(
            self.runtime_host.is_some()
                || self.resources.model_snapshot() == crate::RuntimeResourceSnapshot::default(),
            "isolated realms cannot own host release records"
        );
        self.runtime_host
            .as_ref()
            .map_or(0, |_| self.resources.transfer_releases_to_host())
    }

    fn require_host_capabilities(&self) -> Result<(), RealmError> {
        self.runtime_host
            .as_ref()
            .map(|_| ())
            .ok_or(RealmError::HostCapabilitiesUnavailable)
    }

    fn require_reload_idle(&self) -> Result<(), RealmError> {
        if self.reload.active() {
            Err(ReloadError::InvalidState.into())
        } else {
            Ok(())
        }
    }

    fn register_retired_epoch(&mut self, module: ModuleHandle) -> Result<(), RealmError> {
        let root = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        let resources = self.resources.epoch_counts(root.module_id, root.epoch);
        let task_count = self.tasks.count_for_epoch(root.module_id, root.epoch);
        self.retired_epochs.retire(RetiredEpochSnapshot {
            module,
            epoch: root.epoch,
            state: RetiredEpochState::Retired,
            task_count,
            request_count: resources.requests,
            token_count: resources.tokens,
            snapshot_count: resources.snapshots,
            gc_root_count: root.globals.len()
                + root.state.gc_roots().len()
                + root.staging_roots.len(),
            pending_releases: resources.pending_releases,
            pending_completions: resources.pending_completions,
        });
        Ok(())
    }

    fn reap_retired_epochs(&mut self) {
        for index in 0..self.retired_epochs.entries.len() {
            let entry = self.retired_epochs.entries[index];
            if entry.state == RetiredEpochState::Drained {
                continue;
            }
            let Ok(root) = self.modules.resolve(entry.module.raw()) else {
                self.retired_epochs.entries[index].state = RetiredEpochState::Drained;
                continue;
            };
            let module_id = root.module_id;
            let epoch = root.epoch;
            let task_count = self.tasks.count_for_epoch(module_id, epoch);
            let resources = self.resources.epoch_counts(module_id, epoch);
            let root_count =
                root.globals.len() + root.state.gc_roots().len() + root.staging_roots.len();
            let snapshot = &mut self.retired_epochs.entries[index];
            snapshot.task_count = task_count;
            snapshot.request_count = resources.requests;
            snapshot.token_count = resources.tokens;
            snapshot.snapshot_count = resources.snapshots;
            snapshot.gc_root_count = root_count;
            snapshot.pending_releases = resources.pending_releases;
            snapshot.pending_completions = resources.pending_completions;
            if task_count == 0
                && resources.requests == 0
                && resources.tokens == 0
                && resources.snapshots == 0
                && resources.pending_releases == 0
                && resources.pending_completions == 0
            {
                if let Ok(root) = self.modules.resolve_mut(entry.module.raw()) {
                    root.globals.clear();
                    root.state = StatefulRegistry::new(root.stateful_domain);
                    root.staging_roots.clear();
                }
                let _ = self.modules.release(entry.module.raw());
                let draining = retired_epoch::apply(
                    retired_epoch::State::Retired,
                    retired_epoch::Event::BeginDrain,
                    |_| true,
                )
                .expect("retired epoch drain transition exists");
                let drained = retired_epoch::apply(
                    draining.state,
                    retired_epoch::Event::DrainCompleted,
                    |_| true,
                )
                .expect("retired epoch completion transition exists");
                debug_assert_eq!(drained.state, retired_epoch::State::Drained);
                snapshot.gc_root_count = 0;
                snapshot.state = RetiredEpochState::Drained;
            }
        }
    }
}

impl Drop for RealmRuntime {
    fn drop(&mut self) {
        for task in self.tasks.task_handles_iter() {
            let _ = self.resources.cleanup_task(task, true);
        }
        self.flush_releases();
        if let Some(runtime_host) = &self.runtime_host {
            runtime_host.unregister_realm();
        }
    }
}

fn module_requires_host_capabilities(module: &nexa_bytecode::Module) -> bool {
    if !module.host_imports.is_empty() {
        return true;
    }
    let mut visited = BTreeSet::new();
    let mut requires = |ty| requires_host_capabilities(module, ty, &mut visited);
    module.functions.iter().any(|function| {
        function
            .signature
            .parameters
            .iter()
            .copied()
            .any(&mut requires)
            || function.signature.result.is_some_and(&mut requires)
    }) || module.exports.iter().any(|export| {
        export
            .signature
            .parameters
            .iter()
            .copied()
            .any(&mut requires)
            || export.signature.result.is_some_and(&mut requires)
    }) || module.enum_types.iter().any(|enum_type| {
        enum_type
            .variants
            .iter()
            .filter_map(|variant| variant.payload_type)
            .any(&mut requires)
    }) || module
        .state_schema
        .types
        .iter()
        .any(|state_type| state_type.fields.iter().any(|field| requires(field.ty)))
}

fn requires_host_capabilities(
    module: &nexa_bytecode::Module,
    ty: ValueType,
    visited: &mut BTreeSet<StableId>,
) -> bool {
    let ValueType::Named(type_id) = ty else {
        return false;
    };
    if [
        StableId::from_name("HostRequest"),
        StableId::from_name("ResourceToken"),
        StableId::from_name("Snapshot"),
        StableId::from_name("Buffer"),
    ]
    .contains(&type_id)
    {
        return true;
    }
    if !visited.insert(type_id) {
        return false;
    }
    if let Some(enum_type) = module
        .enum_types
        .iter()
        .find(|enum_type| enum_type.type_id == type_id)
        && enum_type
            .variants
            .iter()
            .filter_map(|variant| variant.payload_type)
            .any(|payload| requires_host_capabilities(module, payload, visited))
    {
        return true;
    }
    module
        .state_schema
        .types
        .iter()
        .find(|state_type| state_type.stable_id == type_id)
        .is_some_and(|state_type| {
            state_type
                .fields
                .iter()
                .any(|field| requires_host_capabilities(module, field.ty, visited))
        })
}

fn reservation_for_module(
    module: &VerifiedModule,
    limits: crate::FrameLimits,
) -> ContinuationReservation {
    let max_registers = module
        .module()
        .functions
        .iter()
        .map(|function| u32::from(function.registers))
        .max()
        .unwrap_or(0);
    let max_depth = module
        .module()
        .functions
        .iter()
        .map(|function| u32::from(function.max_static_call_depth))
        .max()
        .unwrap_or(1)
        .max(1);
    ContinuationReservation {
        frame_capacity: limits.max_call_depth.min(max_depth),
        register_capacity: u32::try_from(
            limits.max_frame_bytes / std::mem::size_of::<RuntimeValue>(),
        )
        .unwrap_or(u32::MAX)
        .min(max_registers.saturating_mul(max_depth)),
        defer_capacity: limits.max_defer_records,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder,
        RootMap, Signature, StateField, StateSchema, StateType, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{
        CancelReason, PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime,
        TaskTerminalReason,
    };
    use crate::task::TaskExecution;
    use crate::{
        HostArgs, HostCallOutcome, HostErrorPayload, HostPayload, HostRegistry, HostTrap, Object,
        ReloadError, ResourceContext, RuntimeHost, RuntimeHostDomain, RuntimeValue, StepConfig,
        TaskLimits, TickBudget,
    };

    fn module(yields: bool) -> (nexa_verifier::VerifiedModule, StableId, StableId) {
        let host = StableId::from_name("host");
        let schema = StableId::from_name("schema");
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        function.effect(FunctionEffect::Task);
        if yields {
            function.emit(Instruction::Yield);
        }
        function.emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module
            .metadata(host, schema)
            .function(function.finish().unwrap());
        (
            verify(module.finish(), VerifierLimits::default()).unwrap(),
            host,
            schema,
        )
    }

    fn test_host_error_enum() -> nexa_bytecode::EnumType {
        let type_id = StableId::from_name("TestHostError");
        nexa_bytecode::EnumType {
            type_id,
            variants: vec![
                nexa_bytecode::EnumVariant {
                    stable_id: StableId::from_parts(&["TestHostError", "::Cancelled"]),
                    tag: 6,
                    payload_type: None,
                },
                nexa_bytecode::EnumVariant {
                    stable_id: StableId::from_parts(&["TestHostError", "::Failed"]),
                    tag: 7,
                    payload_type: None,
                },
            ],
        }
    }

    fn reloadable_async_module(host: StableId, schema: StableId) -> nexa_verifier::VerifiedModule {
        let error_enum = test_host_error_enum();
        let error_type = error_enum.type_id;
        let async_enum = nexa_bytecode::result_type(ValueType::I32, ValueType::Named(error_type));
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Return { source: 0 });

        let mut activation = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        activation
            .effect(FunctionEffect::Immediate)
            .emit(Instruction::ReturnVoid);

        let mut async_task = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Named(async_enum.type_id)),
            },
            1,
        );
        async_task
            .effect(FunctionEffect::Task)
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });

        let mut yielding_task = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        yielding_task
            .effect(FunctionEffect::Task)
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });

        let mut module = ModuleBuilder::new();
        module.metadata(host, schema);
        let async_result = nexa_bytecode::AsyncResultType {
            result_type: async_enum.type_id,
            success: ValueType::I32,
            error: ValueType::Named(error_type),
            cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
            cancel_error: Some(6),
            abandon_error: None,
        };
        module.enum_type(error_enum);
        module.enum_type(async_enum);
        module.host_import(HostImport {
            stable_id: StableId::from_name("Host::pending"),
            parameters: Vec::new(),
            result: Some(ValueType::Named(async_result.result_type)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(async_result),
        });
        module.function(migration.finish().unwrap());
        module.function(activation.finish().unwrap());
        let mut async_task = async_task.finish().unwrap();
        async_task.root_bitmap[0] = true;
        async_task.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true],
            },
        ];
        module.function(async_task);
        module.function(yielding_task.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    #[test]
    fn task_handle_is_the_only_resume_credential_and_terminal_record_survives_slot_release() {
        let (module, host, schema) = module(true);
        let mut realm = RealmRuntime::isolated(RealmConfig::default());
        let module = realm.load_module(module, host, schema).unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 10,
                    cumulative_budget: 100,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 10).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        );
        assert_eq!(
            realm.tasks.task_snapshot(task).unwrap().state,
            crate::TaskState::ExplicitYielded
        );
        assert!(matches!(
            realm.tasks.execution(task).unwrap(),
            TaskExecution::ExplicitYielded(_)
        ));
        assert_eq!(
            realm.poll_task(task, 10).unwrap(),
            PollResult::Completed(Some(RuntimeValue::I32(7)))
        );
        assert!(realm.terminal_record(task).is_some());
        assert_eq!(realm.poll_task(task, 10), Err(RealmError::TerminalTask));
    }

    #[test]
    fn fuel_and_explicit_yield_have_distinct_runtime_execution_states() {
        let (fuel_module, host, schema) = module(false);
        let mut realm = RealmRuntime::isolated(RealmConfig::default());
        let fuel_module = realm.load_module(fuel_module, host, schema).unwrap();
        let scope = realm.create_scope(None).unwrap();
        let fuel_task = realm
            .call(fuel_module, 0, &[RuntimeValue::I32(7)], task_config(scope))
            .unwrap();
        assert_eq!(
            realm.poll_task(fuel_task, 0).unwrap(),
            PollResult::Pending(PendingReason::Fuel)
        );
        assert_eq!(
            realm.tasks.task_snapshot(fuel_task).unwrap().state,
            crate::TaskState::FuelYielded
        );
        assert!(matches!(
            realm.tasks.execution(fuel_task).unwrap(),
            TaskExecution::FuelYielded(_)
        ));
        assert_eq!(
            realm.poll_task(fuel_task, 32).unwrap(),
            PollResult::Completed(Some(RuntimeValue::I32(7)))
        );

        let (explicit_candidate, _, _) = module(true);
        let (explicit_module, _, _) = module(true);
        let explicit_module = realm.load_module(explicit_module, host, schema).unwrap();
        let explicit_task = realm
            .call(
                explicit_module,
                0,
                &[RuntimeValue::I32(9)],
                task_config(scope),
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(explicit_task, 32).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        );
        realm
            .prepare_reload(explicit_module, explicit_candidate, host, schema)
            .unwrap();
        realm.quiesce_reload().unwrap();
        assert_eq!(
            realm.tasks.task_snapshot(explicit_task).unwrap().state,
            crate::TaskState::ReloadPaused
        );
        assert!(matches!(
            realm.tasks.execution(explicit_task).unwrap(),
            TaskExecution::ReloadPaused(_)
        ));
        realm.rollback_reload().unwrap();
        assert_eq!(
            realm.tasks.task_snapshot(explicit_task).unwrap().state,
            crate::TaskState::ExplicitYielded
        );
        assert!(matches!(
            realm.tasks.execution(explicit_task).unwrap(),
            TaskExecution::ExplicitYielded(_)
        ));
        assert_eq!(
            realm.poll_task(explicit_task, 32).unwrap(),
            PollResult::Completed(Some(RuntimeValue::I32(9)))
        );
    }

    #[test]
    fn ordinary_cancel_enters_cleanup_and_cleanup_trap_is_terminal() {
        for cleanup_traps in [false, true] {
            let host = StableId::from_name("cleanup-host");
            let schema = StableId::from_name("cleanup-schema");
            let mut realm = RealmRuntime::isolated(RealmConfig::default());
            let module = realm
                .load_module(
                    cancellation_module(host, schema, cleanup_traps),
                    host,
                    schema,
                )
                .unwrap();
            let scope = realm.create_scope(None).unwrap();
            let task = realm
                .call(module, 1, &[RuntimeValue::I32(3)], task_config(scope))
                .unwrap();
            assert_eq!(
                realm.poll_task(task, 32).unwrap(),
                PollResult::Pending(PendingReason::ExplicitYield)
            );
            realm
                .cancel_task(task, CancelReason::ScopeCancelled)
                .unwrap();
            let terminal = realm.terminal_record(task).unwrap();
            if cleanup_traps {
                assert!(matches!(terminal.reason, TaskTerminalReason::Trapped(_)));
                assert_eq!(terminal.state, crate::TaskState::Trapped);
            } else {
                assert_eq!(
                    terminal.reason,
                    TaskTerminalReason::Cancelled(CancelReason::ScopeCancelled)
                );
                assert_eq!(terminal.state, crate::TaskState::Cancelled);
            }
            assert!(realm.trace().records().iter().any(|record| {
                record.machine_kind == nexa_core::MachineKind::Task
                    && record.new_state
                        == StableId(crate::machines::task::state_id(crate::TaskState::Cleanup))
            }));
            assert!(realm.trace().records().iter().any(|record| {
                record.machine_kind == nexa_core::MachineKind::Task
                    && record.old_state
                        == StableId(crate::machines::task::state_id(
                            crate::TaskState::ExplicitYielded,
                        ))
                    && record.new_state
                        == StableId(crate::machines::task::state_id(
                            crate::TaskState::CancelRequested,
                        ))
            }));
            assert_eq!(
                realm.scheduler.checkpoint(task),
                crate::scheduler::SchedulerCheckpoint::Detached
            );
            assert!(realm.tasks.execution(task).is_err());
        }
    }

    #[test]
    fn reload_commit_cancel_skips_trapping_user_cleanup() {
        let host = StableId::from_name("reload-cleanup-host");
        let schema = StableId::from_name("reload-cleanup-schema");
        let mut realm = RealmRuntime::isolated(RealmConfig::default());
        let old = realm
            .load_module(cancellation_module(host, schema, true), host, schema)
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(old, 1, &[RuntimeValue::I32(3)], task_config(scope))
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 32).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        );
        realm
            .prepare_reload(
                old,
                reload_candidate_module(host, schema, false),
                host,
                schema,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        realm
            .commit_reload(super::ActivationEntry {
                function_id: 1,
                arguments: &[],
                fuel: 32,
            })
            .unwrap();
        assert_eq!(
            realm.terminal_record(task).map(|record| &record.reason),
            Some(&TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
        );
        assert!(!realm.trace().records().iter().any(|record| {
            record.machine_kind == nexa_core::MachineKind::Task
                && record.new_state
                    == StableId(crate::machines::task::state_id(crate::TaskState::Cleanup))
        }));
        assert_eq!(
            realm.scheduler.checkpoint(task),
            crate::scheduler::SchedulerCheckpoint::Detached
        );
        assert!(realm.tasks.execution(task).is_err());
    }

    struct AsyncRegistry {
        hash: StableId,
        request: Arc<Mutex<Option<crate::PendingHostRequest>>>,
    }

    impl HostRegistry for AsyncRegistry {
        fn interface_hash(&self) -> Option<StableId> {
            Some(self.hash)
        }

        fn call(
            &mut self,
            id: u32,
            context: &mut ResourceContext<'_>,
            args: HostArgs<'_>,
        ) -> Result<HostCallOutcome, HostTrap> {
            if id != 0 || !args.is_empty() {
                return Err(HostTrap::Arity);
            }
            let pending = context
                .create_request()
                .map_err(|error| HostTrap::Host(error.to_string()))?;
            let request = pending.request;
            *self
                .request
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
            Ok(HostCallOutcome::Pending(request))
        }
    }

    struct QueueAsyncRegistry {
        hash: StableId,
        requests: Arc<Mutex<VecDeque<crate::PendingHostRequest>>>,
    }

    impl HostRegistry for QueueAsyncRegistry {
        fn interface_hash(&self) -> Option<StableId> {
            Some(self.hash)
        }

        fn call(
            &mut self,
            id: u32,
            context: &mut ResourceContext<'_>,
            args: HostArgs<'_>,
        ) -> Result<HostCallOutcome, HostTrap> {
            if id != 0 || !args.is_empty() {
                return Err(HostTrap::Arity);
            }
            let pending = context
                .create_request()
                .map_err(|error| HostTrap::Host(error.to_string()))?;
            let request = pending.request;
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(pending);
            Ok(HostCallOutcome::Pending(request))
        }
    }

    #[derive(Clone, Copy)]
    enum TestCompletion {
        Success,
        Error,
        Cancelled,
        Abandoned,
    }

    fn submit_test_completion(pending: &mut crate::PendingHostRequest, completion: TestCompletion) {
        match completion {
            TestCompletion::Success => pending.ticket.complete(HostPayload::I32(41)).unwrap(),
            TestCompletion::Error => pending.ticket.fail(HostErrorPayload { code: 7 }).unwrap(),
            TestCompletion::Cancelled => pending.ticket.cancelled().unwrap(),
            TestCompletion::Abandoned => pending.ticket.abandon().unwrap(),
        }
    }

    fn task_config(scope: crate::ScopeHandle) -> StepConfig {
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 128,
            limits: TaskLimits::default(),
        }
    }

    fn reload_candidate_module(
        host: StableId,
        schema: StableId,
        activation_fault: bool,
    ) -> nexa_verifier::VerifiedModule {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Return { source: 0 });
        let mut activation = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        activation.effect(FunctionEffect::Immediate);
        if activation_fault {
            activation.emit(Instruction::Trap);
        } else {
            activation.emit(Instruction::ReturnVoid);
        }
        let mut module = ModuleBuilder::new();
        module.metadata(host, schema);
        module.function(migration.finish().unwrap());
        module.function(activation.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn cancellation_module(
        host: StableId,
        schema: StableId,
        cleanup_traps: bool,
    ) -> nexa_verifier::VerifiedModule {
        let mut cleanup = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        cleanup.effect(FunctionEffect::Cleanup);
        if cleanup_traps {
            cleanup.emit(Instruction::Trap);
        } else {
            cleanup.emit(Instruction::CleanupReturn);
        }
        let mut task = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        task.effect(FunctionEffect::Task)
            .emit(Instruction::DeferPush {
                function: 0,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::Yield)
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.metadata(host, schema);
        module.function(cleanup.finish().unwrap());
        module.function(task.finish().unwrap());
        verify(module.finish(), VerifierLimits::default()).unwrap()
    }

    fn unrelated_module_completion_during_reload(completion: TestCompletion) {
        let host_hash = StableId::from_name("routing-host");
        let schema = StableId::from_name("routing-schema");
        let requests = Arc::new(Mutex::new(VecDeque::new()));
        let runtime_host = RuntimeHost::new(8);
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            runtime_host.clone(),
            Box::new(QueueAsyncRegistry {
                hash: host_hash,
                requests: Arc::clone(&requests),
            }),
        )
        .unwrap();
        let module_a = realm
            .load_module(
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        let module_b = realm
            .load_module(
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task_a = realm.call(module_a, 2, &[], task_config(scope)).unwrap();
        let task_b = realm.call(module_b, 2, &[], task_config(scope)).unwrap();
        assert_eq!(
            realm.poll_task(task_a, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        assert_eq!(
            realm.poll_task(task_b, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        let (mut pending_a, mut pending_b) = {
            let mut requests = requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (requests.pop_front().unwrap(), requests.pop_front().unwrap())
        };
        let request_b = pending_b.request;
        realm
            .prepare_reload(
                module_a,
                reload_candidate_module(host_hash, schema, false),
                host_hash,
                schema,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        submit_test_completion(&mut pending_b, completion);
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        assert_eq!(realm.reload_buffered_completions(), 0);
        assert_eq!(realm.reload_completion_stats().buffered, 0);
        realm
            .tick(TickBudget {
                max_tasks: 1,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .unwrap();
        let terminal = realm
            .terminal_record(task_b)
            .expect("unrelated module task must leave Waiting");
        if matches!(completion, TestCompletion::Abandoned) {
            assert!(matches!(terminal.reason, TaskTerminalReason::Trapped(_)));
        } else {
            assert!(matches!(terminal.reason, TaskTerminalReason::Completed(_)));
        }
        assert!(realm.request_terminal_record(request_b).is_some());

        realm.rollback_reload().unwrap();
        pending_a.ticket.complete(HostPayload::I32(42)).unwrap();
        realm
            .tick(TickBudget {
                max_tasks: 1,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .unwrap();
        assert!(realm.terminal_record(task_a).is_some());
        assert_eq!(runtime_host.pending_completions(), 0);
        assert_eq!(runtime_host.pending_releases(), 2);
        assert_eq!(runtime_host.drain_releases().len(), 2);
    }

    #[test]
    fn reload_routes_other_module_success_completion() {
        unrelated_module_completion_during_reload(TestCompletion::Success);
    }

    #[test]
    fn reload_routes_other_module_error_completion() {
        unrelated_module_completion_during_reload(TestCompletion::Error);
    }

    #[test]
    fn reload_routes_other_module_cancelled_completion() {
        unrelated_module_completion_during_reload(TestCompletion::Cancelled);
    }

    #[test]
    fn reload_routes_other_module_abandoned_completion() {
        unrelated_module_completion_during_reload(TestCompletion::Abandoned);
    }

    fn old_completion_during_reload_publication(activation_fault: bool) {
        let host_hash = StableId::from_name("commit-routing-host");
        let schema = StableId::from_name("commit-routing-schema");
        let requests = Arc::new(Mutex::new(VecDeque::new()));
        let runtime_host = RuntimeHost::new(4);
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            runtime_host.clone(),
            Box::new(QueueAsyncRegistry {
                hash: host_hash,
                requests: Arc::clone(&requests),
            }),
        )
        .unwrap();
        let old = realm
            .load_module(
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm.call(old, 2, &[], task_config(scope)).unwrap();
        assert_eq!(
            realm.poll_task(task, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        let mut pending = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap();
        let request = pending.request;
        let candidate = realm
            .prepare_reload(
                old,
                reload_candidate_module(host_hash, schema, activation_fault),
                host_hash,
                schema,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        pending.ticket.complete(HostPayload::I32(77)).unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        assert_eq!(realm.reload_buffered_completions(), 1);

        let result = realm.commit_reload(super::ActivationEntry {
            function_id: 1,
            arguments: &[],
            fuel: 32,
        });
        if activation_fault {
            assert!(matches!(
                result,
                Err(RealmError::Reload(ReloadError::Activation(_)))
            ));
            assert_eq!(
                realm.modules.resolve(candidate.raw()).unwrap().lifecycle,
                super::ModuleLifecycle::ActivationFaulted
            );
        } else {
            assert_eq!(result.unwrap(), candidate);
        }
        assert_eq!(realm.active_root(), Some(candidate));
        assert_eq!(
            realm.terminal_record(task).map(|record| &record.reason),
            Some(&TaskTerminalReason::Cancelled(CancelReason::ReloadCommit))
        );
        assert!(realm.request_terminal_record(request).is_some());
        assert_eq!(
            realm.reload_completion_stats(),
            super::ReloadCompletionStats {
                buffered: 1,
                replayed: 0,
                discarded_after_commit: 1,
            }
        );
        realm
            .tick(TickBudget {
                max_tasks: 0,
                frame_fuel_budget: 0,
                collect_garbage: false,
            })
            .unwrap();
        assert_eq!(runtime_host.pending_completions(), 0);
        assert_eq!(runtime_host.pending_releases(), 1);
        assert_eq!(runtime_host.drain_releases().len(), 1);
    }

    #[test]
    fn reload_commit_explicitly_discards_buffered_old_completion() {
        old_completion_during_reload_publication(false);
    }

    #[test]
    fn activation_fault_explicitly_discards_buffered_old_completion() {
        old_completion_during_reload_publication(true);
    }

    #[test]
    fn same_tick_completions_route_by_identity_and_preserve_terminal_sequence() {
        for complete_b_first in [false, true] {
            let host_hash = StableId::from_name("same-tick-routing-host");
            let schema = StableId::from_name("same-tick-routing-schema");
            let requests = Arc::new(Mutex::new(VecDeque::new()));
            let runtime_host = RuntimeHost::new(4);
            let mut realm = RealmRuntime::hosted(
                RealmConfig {
                    max_host_resources: 2,
                    ..RealmConfig::default()
                },
                runtime_host.clone(),
                Box::new(QueueAsyncRegistry {
                    hash: host_hash,
                    requests: Arc::clone(&requests),
                }),
            )
            .unwrap();
            let module_a = realm
                .load_module(
                    reloadable_async_module(host_hash, schema),
                    host_hash,
                    schema,
                )
                .unwrap();
            let module_b = realm
                .load_module(
                    reloadable_async_module(host_hash, schema),
                    host_hash,
                    schema,
                )
                .unwrap();
            let scope = realm.create_scope(None).unwrap();
            let task_a = realm.call(module_a, 2, &[], task_config(scope)).unwrap();
            let task_b = realm.call(module_b, 2, &[], task_config(scope)).unwrap();
            assert!(matches!(
                realm.poll_task(task_a, 32).unwrap(),
                PollResult::Pending(PendingReason::HostRequest)
            ));
            assert!(matches!(
                realm.poll_task(task_b, 32).unwrap(),
                PollResult::Pending(PendingReason::HostRequest)
            ));
            let (mut pending_a, mut pending_b) = {
                let mut requests = requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (requests.pop_front().unwrap(), requests.pop_front().unwrap())
            };
            let request_a = pending_a.request;
            let request_b = pending_b.request;
            realm
                .prepare_reload(
                    module_a,
                    reload_candidate_module(host_hash, schema, false),
                    host_hash,
                    schema,
                )
                .unwrap();
            realm.quiesce_reload().unwrap();
            if complete_b_first {
                pending_b.ticket.complete(HostPayload::I32(2)).unwrap();
                pending_a.ticket.complete(HostPayload::I32(1)).unwrap();
            } else {
                pending_a.ticket.complete(HostPayload::I32(1)).unwrap();
                pending_b.ticket.complete(HostPayload::I32(2)).unwrap();
            }
            realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
            assert_eq!(realm.reload_buffered_completions(), 1);
            assert_eq!(realm.reload_completion_stats().buffered, 1);
            realm.rollback_reload().unwrap();
            assert_eq!(realm.reload_completion_stats().replayed, 1);
            realm
                .tick(TickBudget {
                    max_tasks: 2,
                    frame_fuel_budget: 64,
                    collect_garbage: false,
                })
                .unwrap();
            assert!(realm.terminal_record(task_a).is_some());
            assert!(realm.terminal_record(task_b).is_some());
            let sequence_a = realm
                .request_terminal_record(request_a)
                .and_then(|record| record.terminal_sequence)
                .unwrap();
            let sequence_b = realm
                .request_terminal_record(request_b)
                .and_then(|record| record.terminal_sequence)
                .unwrap();
            assert_eq!(sequence_b < sequence_a, complete_b_first);
            assert_eq!(runtime_host.pending_completions(), 0);
            assert_eq!(runtime_host.pending_releases(), 2);
            assert_eq!(runtime_host.drain_releases().len(), 2);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn host_call_pending_completion_writes_destination_and_runtime_host_keeps_releases() {
        let host_hash = StableId::from_name("integrated-host");
        let schema = StableId::from_name("integrated-schema");
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            2,
        );
        function
            .effect(FunctionEffect::Task)
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::EnumPayload {
                source: 0,
                variant: StableId::from_parts(&["Result", "::Ok"]),
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });
        let mut builder = ModuleBuilder::new();
        builder.metadata(host_hash, schema);
        let async_enum = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
        let async_result = nexa_bytecode::AsyncResultType {
            result_type: async_enum.type_id,
            success: ValueType::I32,
            error: ValueType::I32,
            cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
            cancel_error: Some(u32::MAX - 1),
            abandon_error: None,
        };
        builder.enum_type(async_enum);
        builder.host_import(HostImport {
            stable_id: StableId::from_name("Engine::load"),
            parameters: Vec::new(),
            result: Some(ValueType::Named(async_result.result_type)),
            mode: HostCallMode::Async,
            fuel_cost: 3,
            async_result: Some(async_result),
        });
        let mut function = function.finish().unwrap();
        function.root_bitmap[0] = true;
        function.safepoints = vec![0, 1, 2];
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 2,
                bitmap: vec![true, false],
            },
        ];
        builder.function(function);
        let module = verify(builder.finish(), VerifierLimits::default()).unwrap();

        let request = Arc::new(Mutex::new(None));
        let runtime_host = RuntimeHost::new(8);
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            runtime_host.clone(),
            Box::new(AsyncRegistry {
                hash: host_hash,
                request: Arc::clone(&request),
            }),
        )
        .unwrap();
        let module = realm.load_module(module, host_hash, schema).unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                0,
                &[],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 32,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        assert_eq!(runtime_host.pending_completions(), 1);
        let mut pending = request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("registry created request");
        pending.ticket.complete(HostPayload::I32(42)).unwrap();
        assert_eq!(runtime_host.pending_completions(), 1);
        realm
            .tick(TickBudget {
                max_tasks: 1,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .unwrap();
        assert!(matches!(
            realm.terminal_record(task).map(|record| &record.reason),
            Some(super::TaskTerminalReason::Completed(Some(
                RuntimeValue::I32(42)
            )))
        ));
        assert_eq!(runtime_host.pending_completions(), 0);
        assert_eq!(runtime_host.pending_releases(), 1);
        assert_eq!(
            runtime_host.begin_close().state,
            crate::RuntimeHostState::Closing
        );
        assert_eq!(
            runtime_host.try_finish_close(),
            Err(crate::RuntimeHostCloseError::LiveRealms)
        );
        drop(realm);
        assert_eq!(
            runtime_host.try_finish_close(),
            Err(crate::RuntimeHostCloseError::PendingReleases)
        );
        assert_eq!(runtime_host.drain_releases().len(), 1);
        runtime_host.try_finish_close().unwrap();
        assert_eq!(runtime_host.state(), crate::RuntimeHostState::Closed);
        assert!(matches!(
            RealmRuntime::hosted(
                RealmConfig::default(),
                runtime_host.clone(),
                Box::new(AsyncRegistry {
                    hash: host_hash,
                    request,
                }),
            ),
            Err(RealmError::RuntimeHostClosed)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn three_epochs_retain_late_completion_and_release_backlog_until_drain() {
        let host_hash = StableId::from_name("three-epoch-host");
        let schema = StableId::from_name("three-epoch-schema");
        let request = Arc::new(Mutex::new(None));
        let runtime_host = RuntimeHost::new(8);
        let mut realm = RealmRuntime::hosted(
            RealmConfig {
                max_modules: 3,
                max_host_resources: 8,
                release_capacity: 8,
                ..RealmConfig::default()
            },
            runtime_host.clone(),
            Box::new(AsyncRegistry {
                hash: host_hash,
                request: Arc::clone(&request),
            }),
        )
        .unwrap();
        let epoch_one = realm
            .load_module(
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let waiting = realm
            .call(
                epoch_one,
                2,
                &[],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 32,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert!(matches!(
            realm.poll_task(waiting, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        ));
        let mut late = request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap();

        let epoch_two = realm
            .prepare_reload(
                epoch_one,
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        realm
            .commit_reload(super::ActivationEntry {
                function_id: 1,
                arguments: &[],
                fuel: 32,
            })
            .unwrap();

        let token_owner = realm
            .call(
                epoch_two,
                3,
                &[RuntimeValue::I32(7)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 32,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert!(matches!(
            realm.poll_task(token_owner, 32).unwrap(),
            PollResult::Pending(PendingReason::ExplicitYield)
        ));
        realm
            .create_resource_token(token_owner, crate::RuntimeHostDomain::Render)
            .unwrap();

        let epoch_three = realm
            .prepare_reload(
                epoch_two,
                reloadable_async_module(host_hash, schema),
                host_hash,
                schema,
            )
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(2)]).unwrap();
        realm
            .commit_reload(super::ActivationEntry {
                function_id: 1,
                arguments: &[],
                fuel: 32,
            })
            .unwrap();

        assert_eq!(realm.active_root(), Some(epoch_three));
        assert_eq!(realm.retired_epochs().len(), 2);
        assert_eq!(realm.retired_epochs()[0].pending_completions, 1);
        assert_eq!(realm.retired_epochs()[1].pending_releases, 1);

        late.ticket.complete(HostPayload::I32(99)).unwrap();
        realm
            .tick(TickBudget {
                max_tasks: 0,
                frame_fuel_budget: 0,
                collect_garbage: false,
            })
            .unwrap();
        assert!(realm.retired_epochs().iter().all(|entry| {
            entry.state == super::RetiredEpochState::Drained
                && entry.pending_completions == 0
                && entry.pending_releases == 0
        }));
        assert_eq!(runtime_host.pending_releases(), 2);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reload_rollback_restores_waiting_destination_and_applies_buffered_completion() {
        let host_hash = StableId::from_name("rollback-host");
        let schema = StableId::from_name("rollback-schema");
        let mut task_function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            2,
        );
        task_function
            .effect(FunctionEffect::Task)
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::EnumPayload {
                source: 0,
                variant: StableId::from_parts(&["Result", "::Ok"]),
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });
        let mut old = ModuleBuilder::new();
        old.metadata(host_hash, schema);
        let async_enum = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
        let async_result = nexa_bytecode::AsyncResultType {
            result_type: async_enum.type_id,
            success: ValueType::I32,
            error: ValueType::I32,
            cancel_policy: nexa_bytecode::CancelPolicy::ReturnError,
            abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
            cancel_error: Some(u32::MAX - 1),
            abandon_error: None,
        };
        old.enum_type(async_enum);
        old.host_import(HostImport {
            stable_id: StableId::from_name("Host::pending"),
            parameters: Vec::new(),
            result: Some(ValueType::Named(async_result.result_type)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(async_result),
        });
        let mut task_function = task_function.finish().unwrap();
        task_function.root_bitmap[0] = true;
        task_function.safepoints = vec![0, 1, 2];
        task_function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 2,
                bitmap: vec![true, false],
            },
        ];
        old.function(task_function);
        let old = verify(old.finish(), VerifierLimits::default()).unwrap();

        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Return { source: 0 });
        let mut candidate = ModuleBuilder::new();
        candidate
            .metadata(host_hash, schema)
            .function(migration.finish().unwrap());
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();

        let request = Arc::new(Mutex::new(None));
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            RuntimeHost::new(8),
            Box::new(AsyncRegistry {
                hash: host_hash,
                request: Arc::clone(&request),
            }),
        )
        .unwrap();
        let old = realm.load_module(old, host_hash, schema).unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                old,
                0,
                &[],
                StepConfig {
                    owner: scope,
                    priority: 7,
                    fuel_slice: 32,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert!(matches!(
            realm.poll_task(task, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        ));
        let mut pending = request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap();
        realm
            .prepare_reload(old, candidate, host_hash, schema)
            .unwrap();
        realm.quiesce_reload().unwrap();
        pending.ticket.complete(HostPayload::I32(99)).unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        assert_eq!(realm.reload_buffered_completions(), 1);
        realm.rollback_reload().unwrap();
        assert_eq!(
            realm.reload_completion_stats(),
            super::ReloadCompletionStats {
                buffered: 1,
                replayed: 1,
                discarded_after_commit: 0,
            }
        );
        realm
            .tick(TickBudget {
                max_tasks: 1,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .unwrap();
        assert!(matches!(
            realm.terminal_record(task).map(|record| &record.reason),
            Some(super::TaskTerminalReason::Completed(Some(
                RuntimeValue::I32(99)
            )))
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn restricted_migration_vm_builds_and_validates_candidate_state_graph() {
        let host = StableId::from_name("migration-host");
        let old_schema_hash = StableId::from_name("schema-v1");
        let new_schema_hash = StableId::from_name("schema-v2");
        let old_health = StableId::from_name("old-health");
        let brain = StableId::from_name("EnemyBrain::boss");
        let brain_type = StableId::from_name("EnemyBrain");
        let phase = StableId::from_name("EnemyBrain::phase");

        let mut old_function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::I32),
            },
            1,
        );
        old_function
            .effect(FunctionEffect::Task)
            .emit(Instruction::Return { source: 0 });
        let mut old_module = ModuleBuilder::new();
        old_module
            .metadata(host, old_schema_hash)
            .function(old_function.finish().unwrap());
        let old_module = verify(old_module.finish(), VerifierLimits::default()).unwrap();

        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            2,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::StateOldGet {
                stable_id: old_health,
                ty: ValueType::I32,
                dst: 0,
            })
            .emit(Instruction::StateNewCreate {
                stable_id: brain,
                type_id: brain_type,
                dst: 1,
            })
            .emit(Instruction::StateNewSet {
                object: 1,
                field_id: phase,
                source: 0,
            })
            .emit(Instruction::StateReplace {
                old_id: old_health,
                target: 1,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::Return { source: 0 });
        let mut migration = migration.finish().unwrap();
        migration.root_bitmap = vec![false, true];
        migration.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 5,
                bitmap: vec![false, true],
            },
        ];
        let schema = StateSchema {
            types: vec![StateType {
                stable_id: brain_type,
                version: 2,
                fields: vec![StateField {
                    stable_id: phase,
                    ty: ValueType::I32,
                }],
            }],
        };
        let mut candidate = ModuleBuilder::new();
        candidate
            .metadata(host, new_schema_hash)
            .state_schema(schema)
            .function(migration);
        let mut activation = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        activation
            .effect(FunctionEffect::Immediate)
            .emit(Instruction::ReturnVoid);
        candidate.function(activation.finish().unwrap());
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();
        let failure_old_module = old_module.clone();
        let failure_candidate = candidate.clone();

        let mut realm = RealmRuntime::isolated(RealmConfig::default());
        let old = realm
            .load_module(old_module, host, old_schema_hash)
            .unwrap();
        realm
            .insert_state(old, old_health, crate::StateValue::I32(37))
            .unwrap();
        let candidate = realm
            .prepare_reload_migrating(old, candidate, host)
            .unwrap();
        realm.quiesce_reload().unwrap();
        assert_eq!(
            realm.stage_reload(0, &[]).unwrap(),
            Some(RuntimeValue::I32(37))
        );
        realm
            .commit_reload(super::ActivationEntry {
                function_id: 1,
                arguments: &[],
                fuel: 64,
            })
            .unwrap();
        let handle = realm
            .state_handles(candidate)
            .unwrap()
            .into_iter()
            .find(|handle| handle.stable_id == brain)
            .unwrap();
        assert_eq!(
            realm.resolve_state(candidate, handle).unwrap(),
            crate::StateValue::Object(crate::StateObject {
                type_id: brain_type,
                version: 2,
                fields: BTreeMap::from([(phase, crate::StateValue::I32(37))]),
            })
        );

        let mut failure_config = RealmConfig::default();
        failure_config.migration_limits.max_objects = 0;
        let mut failure_realm = RealmRuntime::isolated(failure_config);
        let failure_old = failure_realm
            .load_module(failure_old_module, host, old_schema_hash)
            .unwrap();
        let failure_handle = failure_realm
            .insert_state(failure_old, old_health, crate::StateValue::I32(37))
            .unwrap();
        let failure_candidate = failure_realm
            .prepare_reload_migrating(failure_old, failure_candidate, host)
            .unwrap();
        failure_realm.quiesce_reload().unwrap();
        assert_eq!(
            failure_realm.stage_reload(0, &[]),
            Err(RealmError::Reload(ReloadError::MigrationLimit(
                crate::MigrationLimitError::Objects
            )))
        );
        assert_eq!(failure_realm.active_root(), Some(failure_old));
        assert_eq!(
            failure_realm.module_lifecycle(failure_old).unwrap(),
            super::ModuleLifecycle::Active
        );
        assert_eq!(
            failure_realm.module_lifecycle(failure_candidate).unwrap(),
            super::ModuleLifecycle::Staging
        );
        assert!(failure_realm.root_publications().is_empty());
        assert_eq!(
            failure_realm
                .resolve_state(failure_old, failure_handle)
                .unwrap(),
            crate::StateValue::I32(37)
        );
    }

    #[test]
    fn isolated_realm_rejects_host_modules_and_resource_apis() {
        let host = StableId::from_name("isolated-host");
        let schema = StableId::from_name("isolated-schema");
        let host_module = reloadable_async_module(host, schema);
        let mut isolated = RealmRuntime::isolated(RealmConfig::default());
        assert_eq!(
            isolated.load_module(host_module, host, schema),
            Err(RealmError::HostCapabilitiesUnavailable)
        );

        let option =
            nexa_bytecode::option_type(ValueType::Named(StableId::from_name("HostRequest")));
        let mut wrapped = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(option.type_id)],
                result: Some(ValueType::Named(option.type_id)),
            },
            1,
        );
        wrapped.set_root(0).unwrap();
        wrapped.emit(Instruction::Return { source: 0 });
        let mut indirect = ModuleBuilder::new();
        indirect
            .metadata(host, schema)
            .enum_type(option)
            .function(wrapped.finish().unwrap());
        let indirect = verify(indirect.finish(), VerifierLimits::default()).unwrap();
        assert_eq!(
            isolated.load_module(indirect, host, schema),
            Err(RealmError::HostCapabilitiesUnavailable)
        );

        let (pure, host, schema) = module(false);
        let module = isolated.load_module(pure, host, schema).unwrap();
        let scope = isolated.create_scope(None).unwrap();
        let task = isolated
            .call(
                module,
                0,
                &[RuntimeValue::I32(1)],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 8,
                    cumulative_budget: 32,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            isolated.create_resource_token(task, RuntimeHostDomain::Render),
            Err(RealmError::HostCapabilitiesUnavailable)
        );
    }

    #[test]
    fn typed_async_error_resumes_as_result_err() {
        let host = StableId::from_name("typed-error-host");
        let schema = StableId::from_name("typed-error-schema");
        let request = Arc::new(Mutex::new(None));
        let mut realm = RealmRuntime::hosted(
            RealmConfig::default(),
            RuntimeHost::new(8),
            Box::new(AsyncRegistry {
                hash: host,
                request: Arc::clone(&request),
            }),
        )
        .unwrap();
        let module = realm
            .load_module(reloadable_async_module(host, schema), host, schema)
            .unwrap();
        let scope = realm.create_scope(None).unwrap();
        let task = realm
            .call(
                module,
                2,
                &[],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 32,
                    cumulative_budget: 128,
                    limits: TaskLimits::default(),
                },
            )
            .unwrap();
        assert_eq!(
            realm.poll_task(task, 32).unwrap(),
            PollResult::Pending(PendingReason::HostRequest)
        );
        request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap()
            .ticket
            .fail(HostErrorPayload { code: 7 })
            .unwrap();
        realm
            .tick(TickBudget {
                max_tasks: 1,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .unwrap();
        let Some(super::TaskTerminalReason::Completed(Some(RuntimeValue::NamedRef {
            reference,
            ..
        }))) = realm.terminal_record(task).map(|record| &record.reason)
        else {
            panic!("typed host error did not complete with Result::Err");
        };
        let Object::Enum {
            tag: 1,
            payload:
                Some(RuntimeValue::NamedRef {
                    reference: error_reference,
                    ..
                }),
            ..
        } = realm.resolve_heap_object(*reference).unwrap()
        else {
            panic!("Result::Err did not contain the typed host error enum");
        };
        let error_reference = *error_reference;
        assert!(matches!(
            realm.resolve_heap_object(error_reference).unwrap(),
            Object::Enum { tag: 7, .. }
        ));
    }

    #[test]
    fn schema_change_requires_explicit_finished_migration_output() {
        let host = StableId::from_name("strict-migration-host");
        let old_schema = StableId::from_name("strict-schema-v1");
        let new_schema = StableId::from_name("strict-schema-v2");
        let type_id = StableId::from_name("StrictState");
        let field_id = StableId::from_name("StrictState::value");

        let mut old_entry = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        old_entry.emit(Instruction::ReturnVoid);
        let mut old = ModuleBuilder::new();
        old.metadata(host, old_schema)
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: type_id,
                    version: 1,
                    fields: Vec::new(),
                }],
            })
            .function(old_entry.finish().unwrap());
        let old = verify(old.finish(), VerifierLimits::default()).unwrap();

        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::ReturnVoid);
        let mut candidate = ModuleBuilder::new();
        candidate
            .metadata(host, new_schema)
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: type_id,
                    version: 2,
                    fields: vec![StateField {
                        stable_id: field_id,
                        ty: ValueType::I32,
                    }],
                }],
            })
            .function(migration.finish().unwrap());
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();

        let mut realm = RealmRuntime::isolated(RealmConfig::default());
        let old = realm.load_module(old, host, old_schema).unwrap();
        let domain = realm.module_stateful_domain(old).unwrap();
        let candidate = realm
            .prepare_reload_migrating(old, candidate, host)
            .unwrap();
        assert_eq!(realm.module_stateful_domain(candidate).unwrap(), domain);
        realm.quiesce_reload().unwrap();
        assert_eq!(
            realm.stage_reload(0, &[]),
            Err(RealmError::Reload(ReloadError::MigrationNoOutput))
        );
    }
}
