use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use nexa_core::{RawHandle, StableId};
use nexa_verifier::VerifiedModule;

use crate::reload::{ReloadCoordinator, ReloadTransaction, StatefulRegistry};
use crate::scheduler::Scheduler;
use crate::task::TaskExecution;
use crate::{
    CheckedInterpreter, CollectionStats, ContinuationReservation, ExecutionCharge, FuelState,
    GcRef, GcRoots, Heap, HeapError, HostCompletionSender, HostRequestError, HostRequestHandle,
    InterpreterError, InterpreterOutcome, Object, OpcodeCostTable, ReloadError,
    ResourceTokenHandle, RuntimeError, RuntimeHostDomain, RuntimeLimits, RuntimeResources,
    RuntimeTrace, RuntimeValue, ScopeHandle, SlotAllocError, SlotPool, SnapshotHandle, StepConfig,
    SuspendReason, TaskHandle, TaskRuntime, TaskState, Trap,
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
        }
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
        if verified.module().host_interface_hash != Some(host_hash) {
            return Err(RealmError::HostHashMismatch);
        }
        if verified.module().schema_hash != Some(schema_hash) {
            return Err(RealmError::SchemaHashMismatch);
        }
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.saturating_add(1);
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
                let execution = self.tasks.take_execution(*task)?;
                let continuation = execution.continuation().clone();
                self.tasks.put_execution(
                    *task,
                    TaskExecution::ReloadPaused(continuation),
                    snapshot.fuel,
                )?;
                self.reload.transaction_mut()?.paused_tasks.push(*task);
            }
        }
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
        match CheckedInterpreter::run(&candidate.verified, migration_function, arguments, 4_096)? {
            InterpreterOutcome::Returned { value, .. } => {
                let old_module = self.reload.transaction()?.old_module;
                let migrated = self
                    .modules
                    .resolve(old_module.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .state
                    .clone_for_module(candidate_module_id);
                self.modules
                    .resolve_mut(candidate_handle.raw())
                    .map_err(RealmError::ModuleHandle)?
                    .state = migrated;
                Ok(value)
            }
            InterpreterOutcome::Suspended { .. } => {
                Err(ReloadError::Migration("migration attempted to suspend".into()).into())
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
        let transaction = self.reload.finish()?;
        self.modules
            .resolve_mut(transaction.old_module.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = false;
        self.modules
            .resolve_mut(transaction.candidate.raw())
            .map_err(RealmError::ModuleHandle)?
            .accepts_calls = true;
        for task in transaction.paused_tasks {
            self.cancel_task(task, CancelReason::ReloadCommit)?;
        }
        if let Err(error) = activate(transaction.candidate) {
            self.modules
                .resolve_mut(transaction.candidate.raw())
                .map_err(RealmError::ModuleHandle)?
                .accepts_calls = false;
            return Err(ReloadError::Activation(error).into());
        }
        Ok(transaction.candidate)
    }

    pub fn rollback_reload(&mut self) -> Result<(), RealmError> {
        let transaction = self.reload.finish()?;
        for task in transaction.paused_tasks {
            let snapshot = self.tasks.task_snapshot(task)?;
            self.tasks.rollback_reload(task)?;
            let execution = self.tasks.take_execution(task)?;
            let continuation = execution.continuation().clone();
            self.tasks.put_execution(
                task,
                TaskExecution::FuelYielded(continuation),
                snapshot.fuel,
            )?;
            self.scheduler.schedule(task, snapshot.priority);
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
            } => {
                self.tasks.put_execution(
                    task,
                    TaskExecution::Waiting {
                        continuation,
                        request,
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
        let module = self.module_for_id(snapshot.module_id)?;
        let fuel = FuelState::new(
            fuel_slice,
            snapshot.fuel.cumulative_used,
            snapshot.fuel.cumulative_limit,
        );
        let trace_start = self.trace_cursor();
        match CheckedInterpreter::poll(&module.verified, continuation, fuel, &self.cost_table)? {
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
        report.releases = self.resources.drain_releases().len();
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
                        continuation,
                        request: waiting_request,
                    } = execution
                    else {
                        return Err(RealmError::TaskWaiting);
                    };
                    if waiting_request != request {
                        return Err(RealmError::TaskWaiting);
                    }
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
            .occupied_handles()
            .into_iter()
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
}

impl Drop for RealmRuntime {
    fn drop(&mut self) {
        for task in self.tasks.task_handles() {
            let _ = self.resources.cleanup_task(task, true);
        }
        let _ = self.resources.drain_releases();
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
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime};
    use crate::{RuntimeValue, StepConfig, TaskLimits};

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
}
