use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use nexa_bytecode::{HostImport, ValueType};
use nexa_core::{RawHandle, StableId};
use nexa_verifier::VerifiedModule;

use crate::reload::{ReloadCoordinator, ReloadTransaction, StatefulRegistry};
use crate::scheduler::Scheduler;
use crate::task::TaskExecution;
use crate::{
    CheckedInterpreter, CollectionStats, ContinuationReservation, ExecutionCharge, FuelState,
    GcRef, GcRoots, Heap, HeapError, HostArgs, HostCallOutcome, HostCompletionSender, HostPayload,
    HostRegistry, HostRequestError, HostRequestHandle, HostTrap, HostValue, InterpreterError,
    InterpreterHost, InterpreterHostOutcome, InterpreterOutcome, Object, OpcodeCostTable,
    ReloadError, ResourceTokenHandle, RuntimeError, RuntimeHost, RuntimeHostDomain, RuntimeLimits,
    RuntimeResources, RuntimeTrace, RuntimeValue, ScopeHandle, SlotAllocError, SlotPool,
    SnapshotHandle, StepConfig, SuspendReason, TaskHandle, TaskRuntime, TaskState, Trap,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleHandle(RawHandle);

impl ModuleHandle {
    #[must_use]
    pub const fn raw(self) -> RawHandle {
        self.0
    }
}

#[derive(Clone, Debug)]
struct LoadedModule {
    module_id: u32,
    epoch: u64,
    verified: VerifiedModule,
    globals: Vec<GcRef>,
    state: StatefulRegistry,
    staging_roots: Vec<GcRef>,
    accepts_calls: bool,
    retired: bool,
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
        let values = arguments
            .iter()
            .copied()
            .map(runtime_to_host_value)
            .collect::<Vec<_>>();
        let mut context = self
            .resources
            .context(self.task, self.module_id, self.epoch);
        match self
            .registry
            .call(import, &mut context, HostArgs::new(&values))?
        {
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

fn runtime_to_host_value(value: RuntimeValue) -> HostValue {
    match value {
        RuntimeValue::I32(value) => HostValue::I32(value),
        RuntimeValue::Bool(value) => HostValue::Bool(value),
        RuntimeValue::Ref(reference) | RuntimeValue::NamedRef { reference, .. } => {
            HostValue::Opaque(u64::from(reference.generation) << 32 | u64::from(reference.index))
        }
        RuntimeValue::HostRequest(request) => HostValue::Request(request),
        RuntimeValue::ResourceToken(token) => HostValue::Token(token),
        RuntimeValue::Snapshot(snapshot) => HostValue::Snapshot(snapshot),
        RuntimeValue::Opaque { value, .. } => HostValue::Opaque(value),
        RuntimeValue::Unit => HostValue::Unit,
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
    modules: SlotPool<LoadedModule>,
    tasks: TaskRuntime,
    resources: RuntimeResources,
    heap: Heap,
    scheduler: Scheduler,
    cost_table: OpcodeCostTable,
    tombstones: VecDeque<(TaskHandle, TaskTerminalRecord)>,
    tombstone_capacity: usize,
    next_epoch: u64,
    reload: ReloadCoordinator,
    host_registry: Option<Box<dyn HostRegistry>>,
    host_registry_hash: Option<StableId>,
    runtime_host: Option<RuntimeHost>,
}

impl RealmRuntime {
    #[must_use]
    pub fn new(config: RealmConfig) -> Self {
        Self {
            realm_id: config.realm_id,
            modules: SlotPool::with_capacity_limit(config.realm_id, config.max_modules),
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
            reload: ReloadCoordinator::default(),
            host_registry: None,
            host_registry_hash: None,
            runtime_host: None,
        }
    }

    #[must_use]
    pub fn with_host_registry(config: RealmConfig, registry: Box<dyn HostRegistry>) -> Self {
        let host_registry_hash = registry.interface_hash();
        let mut realm = Self::new(config);
        realm.host_registry = Some(registry);
        realm.host_registry_hash = host_registry_hash;
        realm
    }

    #[must_use]
    pub fn with_runtime_host(
        config: RealmConfig,
        runtime_host: RuntimeHost,
        registry: Box<dyn HostRegistry>,
    ) -> Self {
        let mut realm = Self::with_host_registry(config, registry);
        realm.runtime_host = Some(runtime_host);
        realm
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
    ) -> Result<&crate::StateValue, RealmError> {
        Ok(self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .resolve(handle)?)
    }

    pub fn load_module(
        &mut self,
        verified: VerifiedModule,
        host_hash: StableId,
        schema_hash: StableId,
    ) -> Result<ModuleHandle, RealmError> {
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
        let raw = self
            .modules
            .try_allocate(LoadedModule {
                module_id: 0,
                epoch,
                verified,
                globals: Vec::new(),
                state: StatefulRegistry::new(0),
                staging_roots: Vec::new(),
                accepts_calls: true,
                retired: false,
            })
            .map_err(RealmError::ModuleAllocation)?;
        let loaded = self
            .modules
            .resolve_mut(raw)
            .expect("new module handle resolves");
        loaded.module_id = raw.index;
        loaded.state = StatefulRegistry::new(raw.index);
        Ok(ModuleHandle(raw))
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
        let candidate = self.load_module(candidate, host_hash, schema_hash)?;
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = false;
        self.reload.begin(ReloadTransaction {
            old_module,
            candidate,
            paused_tasks: Vec::new(),
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
        self.modules
            .resolve_mut(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = false;
        self.reload.begin(ReloadTransaction {
            old_module,
            candidate,
            paused_tasks: Vec::new(),
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
                TaskState::FuelYielded | TaskState::Waiting => {
                    self.tasks.request_reload_pause(*task)?;
                }
                _ => {}
            }
            if self.tasks.task_snapshot(*task)?.state == TaskState::ReloadPaused {
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
        let candidate_module_id = candidate.module_id;
        let candidate_schema = candidate.verified.module().state_schema.clone();
        let old_module = self.reload.transaction()?.old_module;
        let old_state = self
            .modules
            .resolve(old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .state
            .clone();
        let mut migration =
            crate::reload::MigrationContext::new(old_state, candidate_module_id, candidate_schema);
        match CheckedInterpreter::run_migration(
            &candidate.verified,
            migration_function,
            arguments,
            4_096,
            &mut migration,
        )? {
            InterpreterOutcome::Returned { value, .. } => {
                let migrated = migration.finish().map_err(|_| ReloadError::GraphCheck)?;
                self.modules
                    .resolve_mut(candidate_handle.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .state = migrated;
                self.reload.staged()?;
                Ok(value)
            }
            InterpreterOutcome::Suspended { .. } => {
                Err(ReloadError::Migration("migration attempted to suspend".into()).into())
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
        activate: impl FnOnce(ModuleHandle) -> Result<(), String>,
    ) -> Result<ModuleHandle, RealmError> {
        self.reload.publish()?;
        let candidate = self.reload.transaction()?.candidate;
        if let Err(error) = activate(candidate) {
            self.reload.activation_failed()?;
            let transaction = self.reload.finish()?;
            for paused in transaction.paused_tasks {
                self.tasks.restore_task_checkpoint(
                    paused.handle,
                    paused.snapshot,
                    paused.execution,
                )?;
                self.scheduler.restore(paused.handle, paused.scheduler);
            }
            self.modules
                .release(transaction.candidate.raw())
                .map_err(RealmError::ModuleHandle)?;
            return Err(ReloadError::Activation(error).into());
        }
        self.reload.activation_succeeded()?;
        let transaction = self.reload.finish()?;
        self.modules
            .resolve_mut(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = false;
        self.modules
            .resolve_mut(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .retired = true;
        self.modules
            .resolve_mut(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = true;
        for paused in transaction.paused_tasks {
            self.cancel_task(paused.handle, CancelReason::ReloadCommit)?;
        }
        Ok(transaction.candidate)
    }

    pub fn commit_reload_entry(
        &mut self,
        activation_function: u32,
        arguments: &[RuntimeValue],
    ) -> Result<ModuleHandle, RealmError> {
        let candidate = self.reload.transaction()?.candidate;
        let verified = self
            .modules
            .resolve(candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .verified
            .clone();
        let arguments = arguments.to_vec();
        self.commit_reload(move |_| {
            let function = verified
                .module()
                .functions
                .get(activation_function as usize)
                .ok_or_else(|| "activation function is missing".to_owned())?;
            if !matches!(
                function.effect,
                nexa_bytecode::FunctionEffect::Ordinary | nexa_bytecode::FunctionEffect::Immediate
            ) {
                return Err("activation entry must be ordinary or immediate".into());
            }
            match CheckedInterpreter::run(&verified, activation_function, &arguments, 4_096)
                .map_err(|error| error.to_string())?
            {
                InterpreterOutcome::Returned { .. } => Ok(()),
                InterpreterOutcome::Trapped { trap, .. } => Err(trap.message),
                InterpreterOutcome::Suspended { .. } | InterpreterOutcome::HostPending { .. } => {
                    Err("activation entry attempted to suspend".into())
                }
            }
        })
    }

    pub fn rollback_reload(&mut self) -> Result<(), RealmError> {
        self.reload.rollback()?;
        let transaction = self.reload.finish()?;
        for paused in transaction.paused_tasks {
            self.tasks
                .restore_task_checkpoint(paused.handle, paused.snapshot, paused.execution)?;
            self.scheduler.restore(paused.handle, paused.scheduler);
        }
        self.modules
            .release(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?;
        Ok(())
    }

    pub fn call(
        &mut self,
        module: ModuleHandle,
        function: u32,
        arguments: &[RuntimeValue],
        config: StepConfig,
    ) -> Result<TaskHandle, RealmError> {
        let loaded = self
            .modules
            .resolve(module.raw())
            .map_err(RealmError::ModuleHandle)?;
        if !loaded.accepts_calls {
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
            TaskState::FuelYielded => self.tasks.resume_task(task)?,
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
            | TaskExecution::Cancelling(continuation) => continuation,
            TaskExecution::Waiting {
                continuation,
                request,
                destination,
                expected_type,
            } => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
                        destination,
                        expected_type,
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
            CheckedInterpreter::poll_with_host(
                &module.verified,
                continuation,
                fuel,
                &self.cost_table,
                &mut bridge,
            )?
        } else {
            CheckedInterpreter::poll(&module.verified, continuation, fuel, &self.cost_table)?
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
                self.tasks.yield_task(task)?;
                self.tasks
                    .put_execution(task, TaskExecution::FuelYielded(continuation), fuel)?;
                self.scheduler.schedule(task, snapshot.priority);
                Ok(PollResult::Pending(pending))
            }
            InterpreterOutcome::HostPending {
                continuation,
                request,
                destination,
                expected_type,
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
        self.reap_retired_modules();
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

    pub fn create_host_request(
        &mut self,
        task: TaskHandle,
    ) -> Result<HostRequestHandle, RealmError> {
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
            TaskState::FuelYielded => self.tasks.resume_task(task)?,
            TaskState::Running => {}
            _ => return Err(RealmError::TaskWaiting),
        }
        let execution = self.tasks.take_execution(task)?;
        let continuation = match execution {
            TaskExecution::Ready(continuation)
            | TaskExecution::Running(continuation)
            | TaskExecution::FuelYielded(continuation) => continuation,
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
    pub fn completion_sender(&self) -> HostCompletionSender {
        self.resources.completion_sender()
    }

    #[must_use]
    pub fn resource_snapshot(&self) -> crate::RuntimeResourceSnapshot {
        self.resources.model_snapshot()
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
        if self.reload.active() {
            return Ok(0);
        }
        let epochs = self
            .modules
            .occupied_handles()
            .into_iter()
            .filter_map(|raw| self.modules.resolve(raw).ok().map(|module| module.epoch))
            .collect::<Vec<_>>();
        let mut count = 0;
        for epoch in epochs {
            for (request, payload) in self.resources.drain_completions(epoch) {
                let _ = payload;
                if let Some(task) = self.scheduler.wake_request(request) {
                    let snapshot = self.tasks.task_snapshot(task)?;
                    if snapshot.module_epoch != epoch || snapshot.state != TaskState::Waiting {
                        continue;
                    }
                    let execution = self.tasks.take_execution(task)?;
                    let TaskExecution::Waiting {
                        mut continuation,
                        request: waiting_request,
                        destination,
                        expected_type,
                    } = execution
                    else {
                        return Err(RealmError::TaskWaiting);
                    };
                    if waiting_request != request {
                        return Err(RealmError::TaskWaiting);
                    }
                    let value = completion_to_runtime(payload, expected_type)?;
                    continuation.write_resume_value(destination, expected_type, value)?;
                    self.tasks.resume_task(task)?;
                    self.tasks.put_execution(
                        task,
                        TaskExecution::Running(continuation),
                        snapshot.fuel,
                    )?;
                    self.scheduler.schedule(task, snapshot.priority);
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    fn module_for_id(&self, module_id: u32) -> Result<&LoadedModule, RealmError> {
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
        } else {
            self.tasks.request_task_cancel(task)?;
            self.tasks.reach_task_safepoint(task)?;
        }
        let snapshot = self.tasks.task_snapshot(task)?;
        let execution = self.tasks.take_execution(task)?;
        let continuation = execution.continuation();
        let cleanup = if reason == CancelReason::ReloadCommit {
            Ok(ExecutionCharge::default())
        } else {
            let module = self.module_for_id(snapshot.module_id)?;
            CheckedInterpreter::run_cleanup(
                &module.verified,
                continuation,
                snapshot.limits.max_cleanup_ops,
                snapshot.limits.max_cleanup_fuel,
                &self.cost_table,
            )?
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
        self.tasks.clean_task(task)?;
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
        let releases = self.resources.drain_releases();
        let count = releases.len();
        if let Some(host) = &self.runtime_host {
            host.submit_releases(releases);
        }
        count
    }

    fn reap_retired_modules(&mut self) {
        let live_module_ids = self
            .tasks
            .task_handles()
            .into_iter()
            .filter_map(|task| {
                self.tasks
                    .task_snapshot(task)
                    .ok()
                    .map(|task| task.module_id)
            })
            .collect::<std::collections::BTreeSet<_>>();
        let retired = self
            .modules
            .occupied_handles_iter()
            .filter(|raw| {
                self.modules.resolve(*raw).is_ok_and(|module| {
                    module.retired && !live_module_ids.contains(&module.module_id)
                })
            })
            .collect::<Vec<_>>();
        for module in retired {
            let _ = self.modules.release(module);
        }
    }
}

impl Drop for RealmRuntime {
    fn drop(&mut self) {
        for task in self.tasks.task_handles() {
            let _ = self.resources.cleanup_task(task, true);
        }
        self.flush_releases();
    }
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
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder,
        RootMap, Signature, StateField, StateSchema, StateType, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime};
    use crate::{
        HostArgs, HostCallOutcome, HostCompletion, HostPayload, HostRegistry, HostTrap,
        ResourceContext, RuntimeHost, RuntimeValue, StepConfig, TaskLimits, TickBudget,
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

    #[test]
    fn task_handle_is_the_only_resume_credential_and_terminal_record_survives_slot_release() {
        let (module, host, schema) = module(true);
        let mut realm = RealmRuntime::new(RealmConfig::default());
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
            realm.poll_task(task, 10).unwrap(),
            PollResult::Completed(Some(RuntimeValue::I32(7)))
        );
        assert!(realm.terminal_record(task).is_some());
        assert_eq!(realm.poll_task(task, 10), Err(RealmError::TerminalTask));
    }

    struct AsyncRegistry {
        hash: StableId,
        request: Arc<Mutex<Option<crate::HostRequestHandle>>>,
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
            let request = context
                .create_request()
                .map_err(|error| HostTrap::Host(error.to_string()))?;
            *self
                .request
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(request);
            Ok(HostCallOutcome::Pending(request))
        }
    }

    #[test]
    fn host_call_pending_completion_writes_destination_and_runtime_host_keeps_releases() {
        let host_hash = StableId::from_name("integrated-host");
        let schema = StableId::from_name("integrated-schema");
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
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
        let mut builder = ModuleBuilder::new();
        builder.metadata(host_hash, schema);
        builder.host_import(HostImport {
            stable_id: StableId::from_name("Engine::load"),
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Async,
            fuel_cost: 3,
        });
        builder.function(function.finish().unwrap());
        let module = verify(builder.finish(), VerifierLimits::default()).unwrap();

        let request = Arc::new(Mutex::new(None));
        let runtime_host = RuntimeHost::new(8);
        let mut realm = RealmRuntime::with_runtime_host(
            RealmConfig::default(),
            runtime_host.clone(),
            Box::new(AsyncRegistry {
                hash: host_hash,
                request: Arc::clone(&request),
            }),
        );
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
        let request = request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expect("registry created request");
        realm
            .completion_sender()
            .complete(HostCompletion {
                realm_id: realm.realm_id(),
                module_id: module.raw().index,
                epoch: realm.module_epoch(module).unwrap(),
                request,
                payload: HostPayload::I32(42),
            })
            .unwrap();
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
        assert_eq!(runtime_host.pending_releases(), 1);
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
            1,
        );
        task_function
            .effect(FunctionEffect::Task)
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut old = ModuleBuilder::new();
        old.metadata(host_hash, schema);
        old.host_import(HostImport {
            stable_id: StableId::from_name("Host::pending"),
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Async,
            fuel_cost: 1,
        });
        old.function(task_function.finish().unwrap());
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
        let mut realm = RealmRuntime::with_host_registry(
            RealmConfig::default(),
            Box::new(AsyncRegistry {
                hash: host_hash,
                request: Arc::clone(&request),
            }),
        );
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
        let request = request
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap();
        realm
            .prepare_reload(old, candidate, host_hash, schema)
            .unwrap();
        realm.quiesce_reload().unwrap();
        realm
            .completion_sender()
            .complete(HostCompletion {
                realm_id: realm.realm_id(),
                module_id: old.raw().index,
                epoch: realm.module_epoch(old).unwrap(),
                request,
                payload: HostPayload::I32(99),
            })
            .unwrap();
        realm.stage_reload(0, &[RuntimeValue::I32(1)]).unwrap();
        realm.rollback_reload().unwrap();
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
            .emit(Instruction::StateHandleRemap {
                old_id: old_health,
                target: 1,
            })
            .emit(Instruction::Return { source: 0 });
        let mut migration = migration.finish().unwrap();
        migration.root_bitmap = vec![false, true];
        migration.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false, false],
            },
            RootMap {
                pc: 4,
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
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();

        let mut realm = RealmRuntime::new(RealmConfig::default());
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
        realm.commit_reload(|_| Ok(())).unwrap();
        let handle = realm
            .state_handles(candidate)
            .unwrap()
            .into_iter()
            .find(|handle| handle.stable_id == brain)
            .unwrap();
        assert_eq!(
            realm.resolve_state(candidate, handle).unwrap(),
            &crate::StateValue::Object(crate::StateObject {
                type_id: brain_type,
                version: 2,
                fields: BTreeMap::from([(phase, crate::StateValue::I32(37))]),
            })
        );
    }
}
