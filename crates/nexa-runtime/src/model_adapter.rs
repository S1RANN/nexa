use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::scheduler::{Scheduler, SchedulerCheckpoint};
use crate::task::TaskExecution;
use crate::{
    ActivationEntry, CheckedInterpreter, FuelState, HostArgs, HostCallOutcome,
    HostCompletionResult, HostPayload, HostRegistry, HostTrap, InterpreterOutcome, OpcodeCostTable,
    PendingHostRequest, RealmConfig, RealmRuntime, ResourceContext, RuntimeHost, RuntimeLimits,
    RuntimeResources, RuntimeValue, StepConfig, TaskHandle, TaskLimits, TaskRuntime, TaskSnapshot,
    TaskState, TickBudget,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4RuntimeEvent {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4RuntimeTaskState {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4ExecutionKind {
    Ready,
    Running,
    FuelYielded,
    ExplicitYielded,
    Waiting,
    ReloadPaused,
    Cancelling,
    Cleanup,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RealmV4RuntimeCancelKind {
    #[default]
    None,
    Ordinary,
    ReloadCommit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RealmV4RuntimeTerminalReason {
    #[default]
    None,
    Completed,
    Cancelled,
    Trapped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RealmV4VmResourceCounts {
    pub task_slots: usize,
    pub requests: usize,
    pub release_records: usize,
    pub completion_reservations: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RealmV4RuntimeSnapshot {
    pub task_state: RealmV4RuntimeTaskState,
    pub execution: RealmV4ExecutionKind,
    pub scheduler_tokens: usize,
    pub request_owned: bool,
    pub continuation_owned: bool,
    pub reload_checkpoint: bool,
    pub cancel_kind: RealmV4RuntimeCancelKind,
    pub user_defer: bool,
    pub terminal_reason: RealmV4RuntimeTerminalReason,
    pub vm_resources: RealmV4VmResourceCounts,
}

struct ReloadCheckpoint {
    snapshot: TaskSnapshot,
    execution: TaskExecution,
    scheduler: SchedulerCheckpoint,
}

pub struct RealmV4RuntimeAdapter {
    runtime: TaskRuntime,
    scheduler: Scheduler,
    resources: RuntimeResources,
    module: VerifiedModule,
    trap_cleanup_module: VerifiedModule,
    task: TaskHandle,
    pending: Option<PendingHostRequest>,
    request: Option<crate::HostRequestHandle>,
    reload: Option<ReloadCheckpoint>,
    cancel_kind: RealmV4RuntimeCancelKind,
    terminal_reason: RealmV4RuntimeTerminalReason,
}

impl RealmV4RuntimeAdapter {
    #[must_use]
    pub fn new() -> Self {
        let module = cleanup_module(false);
        let trap_cleanup_module = cleanup_module(true);
        let continuation = continuation_with_user_defer(&module);
        let mut runtime = TaskRuntime::new(71, RuntimeLimits::default());
        let scope = runtime
            .create_scope(None)
            .expect("model scope admission succeeds");
        let task = runtime
            .admit_task(scope, 1, true)
            .expect("model task admission succeeds");
        runtime
            .attach_continuation(
                task,
                1,
                FuelState::new(64, 0, 1_024),
                continuation,
                1,
                crate::TaskLimits::default(),
            )
            .expect("model continuation attachment succeeds");
        let mut scheduler = Scheduler::with_capacity(4);
        scheduler.schedule(task, 1);
        Self {
            runtime,
            scheduler,
            resources: RuntimeResources::new(71, 4, 8),
            module,
            trap_cleanup_module,
            task,
            pending: None,
            request: None,
            reload: None,
            cancel_kind: RealmV4RuntimeCancelKind::None,
            terminal_reason: RealmV4RuntimeTerminalReason::None,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, event: RealmV4RuntimeEvent) -> Result<(), String> {
        match event {
            RealmV4RuntimeEvent::Poll => {
                self.scheduler.deschedule(self.task);
                self.runtime.poll_task(self.task).map_err(debug)?;
                self.replace_execution(RealmV4ExecutionKind::Running)?;
            }
            RealmV4RuntimeEvent::FuelExhaust => {
                self.runtime.yield_fuel_task(self.task).map_err(debug)?;
                self.replace_execution(RealmV4ExecutionKind::FuelYielded)?;
                self.schedule();
            }
            RealmV4RuntimeEvent::ResumeFuel => {
                self.scheduler.deschedule(self.task);
                self.runtime.resume_fuel_task(self.task).map_err(debug)?;
                self.replace_execution(RealmV4ExecutionKind::Running)?;
            }
            RealmV4RuntimeEvent::ExplicitYield => {
                self.runtime.yield_explicit_task(self.task).map_err(debug)?;
                self.replace_execution(RealmV4ExecutionKind::ExplicitYielded)?;
                self.schedule();
            }
            RealmV4RuntimeEvent::ResumeExplicit => {
                self.scheduler.deschedule(self.task);
                self.runtime
                    .resume_explicit_task(self.task)
                    .map_err(debug)?;
                self.replace_execution(RealmV4ExecutionKind::Running)?;
            }
            RealmV4RuntimeEvent::BeginRequest => self.begin_request()?,
            RealmV4RuntimeEvent::CompleteRequest => self.complete_request()?,
            RealmV4RuntimeEvent::BeginReload => self.begin_reload()?,
            RealmV4RuntimeEvent::RollbackReload => self.rollback_reload()?,
            RealmV4RuntimeEvent::RequestCancel => self.request_cancel()?,
            RealmV4RuntimeEvent::ReloadCommitCancel => {
                self.runtime
                    .begin_reload_commit_cancel(self.task)
                    .map_err(debug)?;
                self.runtime
                    .mark_execution_cancelling(self.task)
                    .map_err(debug)?;
                self.scheduler.cancel_task(self.task);
                self.detach_request(true)?;
                self.reload = None;
                self.cancel_kind = RealmV4RuntimeCancelKind::ReloadCommit;
            }
            RealmV4RuntimeEvent::ReachSafepoint => {
                if self.cancel_kind == RealmV4RuntimeCancelKind::Ordinary && self.user_defer() {
                    self.runtime.begin_cleanup(self.task).map_err(debug)?;
                    self.runtime
                        .mark_execution_cleanup(self.task)
                        .map_err(debug)?;
                } else {
                    self.detach_request(
                        self.cancel_kind == RealmV4RuntimeCancelKind::ReloadCommit,
                    )?;
                    self.runtime
                        .finish_cancel_without_cleanup(self.task)
                        .map_err(debug)?;
                    self.terminal_reason = RealmV4RuntimeTerminalReason::Cancelled;
                    self.cancel_kind = RealmV4RuntimeCancelKind::None;
                }
            }
            RealmV4RuntimeEvent::CleanupSuccess => {
                let cleanup = CheckedInterpreter::run_cleanup(
                    &self.module,
                    self.runtime
                        .take_execution(self.task)
                        .map_err(debug)?
                        .into_continuation(),
                    16,
                    128,
                    &OpcodeCostTable::default(),
                )
                .map_err(debug)?;
                if cleanup.is_err() {
                    return Err("success cleanup trapped".into());
                }
                self.detach_request(false)?;
                self.runtime.finish_cleanup(self.task).map_err(debug)?;
                self.terminal_reason = RealmV4RuntimeTerminalReason::Cancelled;
                self.cancel_kind = RealmV4RuntimeCancelKind::None;
            }
            RealmV4RuntimeEvent::CleanupTrap => {
                let cleanup = CheckedInterpreter::run_cleanup(
                    &self.trap_cleanup_module,
                    self.runtime
                        .take_execution(self.task)
                        .map_err(debug)?
                        .into_continuation(),
                    16,
                    128,
                    &OpcodeCostTable::default(),
                )
                .map_err(debug)?;
                if cleanup.is_ok() {
                    return Err("trap cleanup completed".into());
                }
                self.detach_request(false)?;
                self.runtime.trap_task(self.task).map_err(debug)?;
                self.terminal_reason = RealmV4RuntimeTerminalReason::Trapped;
                self.cancel_kind = RealmV4RuntimeCancelKind::None;
            }
            RealmV4RuntimeEvent::Complete => {
                self.scheduler.cancel_task(self.task);
                self.detach_request(false)?;
                self.runtime.finish_task(self.task).map_err(debug)?;
                self.terminal_reason = RealmV4RuntimeTerminalReason::Completed;
            }
            RealmV4RuntimeEvent::Trap => {
                self.scheduler.cancel_task(self.task);
                self.detach_request(false)?;
                self.runtime.trap_task(self.task).map_err(debug)?;
                self.terminal_reason = RealmV4RuntimeTerminalReason::Trapped;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<RealmV4RuntimeSnapshot, String> {
        let resources = self.resources.model_snapshot();
        let task_live = self.runtime.task_snapshot(self.task).is_ok();
        Ok(RealmV4RuntimeSnapshot {
            task_state: self.normalized_state()?,
            execution: self.execution_kind(),
            scheduler_tokens: usize::from(matches!(
                self.scheduler.checkpoint(self.task),
                SchedulerCheckpoint::Ready { .. }
            )),
            request_owned: self
                .request
                .is_some_and(|request| self.resources.owns_request(self.task, request)),
            continuation_owned: task_live && self.runtime.execution(self.task).is_ok(),
            reload_checkpoint: self.reload.is_some(),
            cancel_kind: self.cancel_kind,
            user_defer: self.user_defer(),
            terminal_reason: self.terminal_reason,
            vm_resources: RealmV4VmResourceCounts {
                task_slots: usize::from(task_live),
                requests: resources.requests,
                release_records: resources.release_records,
                completion_reservations: resources.completion_reservations,
            },
        })
    }

    fn begin_request(&mut self) -> Result<(), String> {
        let pending = self
            .resources
            .context(self.task, 1, 1)
            .create_request()
            .map_err(debug)?;
        let request = pending.request;
        self.runtime.await_task(self.task).map_err(debug)?;
        let snapshot = self.runtime.task_snapshot(self.task).map_err(debug)?;
        let continuation = self
            .runtime
            .take_execution(self.task)
            .map_err(debug)?
            .into_continuation();
        self.runtime
            .put_execution(
                self.task,
                TaskExecution::Waiting {
                    continuation,
                    request,
                    destination: 0,
                    expected_type: None,
                    async_result: None,
                },
                snapshot.fuel,
            )
            .map_err(debug)?;
        self.scheduler.deschedule(self.task);
        self.scheduler.wait_for(request, self.task);
        self.pending = Some(pending);
        self.request = Some(request);
        Ok(())
    }

    fn complete_request(&mut self) -> Result<(), String> {
        self.pending
            .as_mut()
            .ok_or("missing completion ticket")?
            .ticket
            .complete(HostPayload::I32(9))
            .map_err(debug)?;
        let delivery = self
            .resources
            .drain_completions()
            .into_iter()
            .next()
            .ok_or("missing completion delivery")?;
        if !matches!(delivery.result, HostCompletionResult::Success(_)) {
            return Err("unexpected completion result".into());
        }
        if self.scheduler.wake_request(delivery.request) != Some(self.task) {
            return Err("request scheduler mapping was lost".into());
        }
        let snapshot = self.runtime.task_snapshot(self.task).map_err(debug)?;
        let continuation = self
            .runtime
            .take_execution(self.task)
            .map_err(debug)?
            .into_continuation();
        self.runtime.resume_waiting_task(self.task).map_err(debug)?;
        self.runtime
            .put_execution(
                self.task,
                TaskExecution::Running(continuation),
                snapshot.fuel,
            )
            .map_err(debug)?;
        self.pending = None;
        self.request = None;
        Ok(())
    }

    fn begin_reload(&mut self) -> Result<(), String> {
        let snapshot = self.runtime.task_snapshot(self.task).map_err(debug)?;
        let execution = self
            .runtime
            .execution_checkpoint(self.task)
            .map_err(debug)?;
        let scheduler = self.scheduler.checkpoint(self.task);
        match snapshot.state {
            TaskState::Ready => {
                self.runtime.poll_task(self.task).map_err(debug)?;
                self.runtime
                    .pause_task_for_reload(self.task)
                    .map_err(debug)?;
            }
            TaskState::Running => self
                .runtime
                .pause_task_for_reload(self.task)
                .map_err(debug)?,
            TaskState::FuelYielded | TaskState::ExplicitYielded | TaskState::Waiting => self
                .runtime
                .request_reload_pause(self.task)
                .map_err(debug)?,
            _ => return Err("task cannot enter reload pause".into()),
        }
        self.runtime
            .mark_execution_reload_paused(self.task)
            .map_err(debug)?;
        self.scheduler.cancel_task(self.task);
        self.reload = Some(ReloadCheckpoint {
            snapshot,
            execution,
            scheduler,
        });
        Ok(())
    }

    fn rollback_reload(&mut self) -> Result<(), String> {
        let checkpoint = self.reload.take().ok_or("missing reload checkpoint")?;
        self.runtime
            .restore_task_checkpoint(self.task, checkpoint.snapshot, checkpoint.execution)
            .map_err(debug)?;
        self.scheduler.restore(self.task, checkpoint.scheduler);
        Ok(())
    }

    fn request_cancel(&mut self) -> Result<(), String> {
        if self.runtime.task_snapshot(self.task).map_err(debug)?.state == TaskState::Ready {
            self.scheduler.deschedule(self.task);
            self.runtime.poll_task(self.task).map_err(debug)?;
            self.replace_execution(RealmV4ExecutionKind::Running)?;
        }
        self.runtime.request_task_cancel(self.task).map_err(debug)?;
        self.runtime
            .reach_task_safepoint(self.task)
            .map_err(debug)?;
        self.runtime
            .mark_execution_cancelling(self.task)
            .map_err(debug)?;
        self.scheduler.cancel_task(self.task);
        self.detach_request(false)?;
        self.cancel_kind = RealmV4RuntimeCancelKind::Ordinary;
        Ok(())
    }

    fn detach_request(&mut self, detach: bool) -> Result<(), String> {
        if self.request.is_some() {
            self.resources
                .cleanup_task(self.task, detach)
                .map_err(debug)?;
            self.request = None;
            self.pending = None;
        }
        Ok(())
    }

    fn replace_execution(&mut self, kind: RealmV4ExecutionKind) -> Result<(), String> {
        let snapshot = self.runtime.task_snapshot(self.task).map_err(debug)?;
        let continuation = self
            .runtime
            .take_execution(self.task)
            .map_err(debug)?
            .into_continuation();
        let execution = match kind {
            RealmV4ExecutionKind::Ready => TaskExecution::Ready(continuation),
            RealmV4ExecutionKind::Running => TaskExecution::Running(continuation),
            RealmV4ExecutionKind::FuelYielded => TaskExecution::FuelYielded(continuation),
            RealmV4ExecutionKind::ExplicitYielded => TaskExecution::ExplicitYielded(continuation),
            RealmV4ExecutionKind::ReloadPaused => TaskExecution::ReloadPaused(continuation),
            RealmV4ExecutionKind::Cancelling => TaskExecution::Cancelling(continuation),
            RealmV4ExecutionKind::Cleanup => TaskExecution::Cleanup(continuation),
            RealmV4ExecutionKind::Waiting | RealmV4ExecutionKind::None => {
                return Err("execution variant requires dedicated metadata".into());
            }
        };
        self.runtime
            .put_execution(self.task, execution, snapshot.fuel)
            .map_err(debug)
    }

    fn schedule(&mut self) {
        let priority = self
            .runtime
            .task_snapshot(self.task)
            .expect("scheduled model task is live")
            .priority;
        self.scheduler.schedule(self.task, priority);
    }

    fn normalized_state(&self) -> Result<RealmV4RuntimeTaskState, String> {
        if self.terminal_reason != RealmV4RuntimeTerminalReason::None {
            return Ok(match self.terminal_reason {
                RealmV4RuntimeTerminalReason::Completed => RealmV4RuntimeTaskState::Completed,
                RealmV4RuntimeTerminalReason::Cancelled => RealmV4RuntimeTaskState::Cancelled,
                RealmV4RuntimeTerminalReason::Trapped => RealmV4RuntimeTaskState::Trapped,
                RealmV4RuntimeTerminalReason::None => unreachable!(),
            });
        }
        Ok(
            match self.runtime.task_snapshot(self.task).map_err(debug)?.state {
                TaskState::Ready => RealmV4RuntimeTaskState::Ready,
                TaskState::Running => RealmV4RuntimeTaskState::Running,
                TaskState::FuelYielded => RealmV4RuntimeTaskState::FuelYielded,
                TaskState::ExplicitYielded => RealmV4RuntimeTaskState::ExplicitYielded,
                TaskState::Waiting => RealmV4RuntimeTaskState::Waiting,
                TaskState::ReloadPaused => RealmV4RuntimeTaskState::ReloadPaused,
                TaskState::Cancelling => RealmV4RuntimeTaskState::Cancelling,
                TaskState::Cleanup => RealmV4RuntimeTaskState::Cleanup,
                TaskState::Created
                | TaskState::ReloadPauseRequested
                | TaskState::CancelRequested
                | TaskState::Completed
                | TaskState::Cancelled
                | TaskState::Trapped => return Err("adapter observed an intermediate state".into()),
            },
        )
    }

    fn execution_kind(&self) -> RealmV4ExecutionKind {
        match self.runtime.execution(self.task) {
            Ok(TaskExecution::Ready(_)) => RealmV4ExecutionKind::Ready,
            Ok(TaskExecution::Running(_)) => RealmV4ExecutionKind::Running,
            Ok(TaskExecution::FuelYielded(_)) => RealmV4ExecutionKind::FuelYielded,
            Ok(TaskExecution::ExplicitYielded(_)) => RealmV4ExecutionKind::ExplicitYielded,
            Ok(TaskExecution::Waiting { .. }) => RealmV4ExecutionKind::Waiting,
            Ok(TaskExecution::ReloadPaused(_)) => RealmV4ExecutionKind::ReloadPaused,
            Ok(TaskExecution::Cancelling(_)) => RealmV4ExecutionKind::Cancelling,
            Ok(TaskExecution::Cleanup(_)) => RealmV4ExecutionKind::Cleanup,
            Err(_) => RealmV4ExecutionKind::None,
        }
    }

    fn user_defer(&self) -> bool {
        if self.cancel_kind == RealmV4RuntimeCancelKind::ReloadCommit {
            return false;
        }
        self.runtime.execution(self.task).is_ok_and(|execution| {
            execution
                .continuation()
                .arena()
                .defers_rev()
                .next()
                .is_some()
        })
    }
}

impl Default for RealmV4RuntimeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

fn cleanup_module(trap: bool) -> VerifiedModule {
    let mut cleanup = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    cleanup.effect(FunctionEffect::Cleanup);
    if trap {
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
    module.function(cleanup.finish().expect("cleanup function is valid"));
    module.function(task.finish().expect("task function is valid"));
    verify(module.finish(), VerifierLimits::default()).expect("model adapter module verifies")
}

fn continuation_with_user_defer(module: &VerifiedModule) -> crate::InterpreterContinuation {
    let continuation = CheckedInterpreter::start(
        module,
        1,
        &[RuntimeValue::I32(7)],
        crate::FrameLimits::default(),
        crate::ContinuationReservation::for_limits(crate::FrameLimits::default()),
    )
    .expect("model continuation starts");
    match CheckedInterpreter::poll(
        module,
        continuation,
        FuelState::new(64, 0, 1_024),
        &OpcodeCostTable::default(),
    )
    .expect("model continuation reaches explicit yield")
    {
        InterpreterOutcome::Suspended { continuation, .. } => continuation,
        outcome => panic!("expected explicit-yield continuation, got {outcome:?}"),
    }
}

fn debug(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4RoutingRuntimeEvent {
    CompleteA,
    CompleteB,
    RollbackA,
    CommitA,
    ActivationFaultA,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4RoutingRuntimeReloadState {
    Reloading,
    RolledBack,
    Committed,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV4RoutingRuntimeCompletionState {
    Pending,
    Buffered,
    Delivered,
    Replayed,
    Discarded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV4RoutingRuntimeSnapshot {
    pub reload: RealmV4RoutingRuntimeReloadState,
    pub completion_a: RealmV4RoutingRuntimeCompletionState,
    pub completion_b: RealmV4RoutingRuntimeCompletionState,
    pub buffered: u64,
    pub replayed: u64,
    pub discarded_after_commit: u64,
    pub pending_completions: usize,
    pub request_reservations: usize,
}

pub struct RealmV4RoutingRuntimeAdapter {
    realm: RealmRuntime,
    host: RuntimeHost,
    candidate: crate::ModuleHandle,
    task_a: TaskHandle,
    task_b: TaskHandle,
    pending_a: Option<PendingHostRequest>,
    pending_b: Option<PendingHostRequest>,
    reload: RealmV4RoutingRuntimeReloadState,
    completion_a: RealmV4RoutingRuntimeCompletionState,
    completion_b: RealmV4RoutingRuntimeCompletionState,
}

impl RealmV4RoutingRuntimeAdapter {
    #[must_use]
    pub fn new() -> Self {
        let host_hash = crate::StableId::from_name("realm-v4-routing-host");
        let schema_hash = crate::StableId::from_name("realm-v4-routing-schema");
        let requests = Arc::new(Mutex::new(VecDeque::new()));
        let host = RuntimeHost::new(8);
        let mut realm = RealmRuntime::hosted(
            RealmConfig {
                max_host_resources: 4,
                ..RealmConfig::default()
            },
            host.clone(),
            Box::new(RoutingRegistry {
                hash: host_hash,
                requests: Arc::clone(&requests),
            }),
        )
        .expect("routing realm starts");
        let module_a = realm
            .load_module(
                routing_old_module(host_hash, schema_hash),
                host_hash,
                schema_hash,
            )
            .expect("module A loads");
        let module_b = realm
            .load_module(
                routing_old_module(host_hash, schema_hash),
                host_hash,
                schema_hash,
            )
            .expect("module B loads");
        let scope = realm.create_scope(None).expect("routing scope starts");
        let config = StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 32,
            cumulative_budget: 128,
            limits: TaskLimits::default(),
        };
        let task_a = realm
            .call(module_a, 0, &[], config)
            .expect("module A task starts");
        let task_b = realm
            .call(module_b, 0, &[], config)
            .expect("module B task starts");
        realm
            .poll_task(task_a, 32)
            .expect("module A reaches request");
        realm
            .poll_task(task_b, 32)
            .expect("module B reaches request");
        let (pending_a, pending_b) = {
            let mut requests = requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                requests.pop_front().expect("module A ticket exists"),
                requests.pop_front().expect("module B ticket exists"),
            )
        };
        let candidate = realm
            .prepare_reload(
                module_a,
                routing_candidate_module(host_hash, schema_hash),
                host_hash,
                schema_hash,
            )
            .expect("module A reload prepares");
        realm.quiesce_reload().expect("module A quiesces");
        Self {
            realm,
            host,
            candidate,
            task_a,
            task_b,
            pending_a: Some(pending_a),
            pending_b: Some(pending_b),
            reload: RealmV4RoutingRuntimeReloadState::Reloading,
            completion_a: RealmV4RoutingRuntimeCompletionState::Pending,
            completion_b: RealmV4RoutingRuntimeCompletionState::Pending,
        }
    }

    pub fn apply(&mut self, event: RealmV4RoutingRuntimeEvent) -> Result<(), String> {
        match event {
            RealmV4RoutingRuntimeEvent::CompleteA => self.complete_a()?,
            RealmV4RoutingRuntimeEvent::CompleteB => self.complete_b()?,
            RealmV4RoutingRuntimeEvent::RollbackA => {
                let before = self.realm.reload_completion_stats().replayed;
                self.realm.rollback_reload().map_err(debug)?;
                self.reload = RealmV4RoutingRuntimeReloadState::RolledBack;
                if self.realm.reload_completion_stats().replayed > before {
                    self.completion_a = RealmV4RoutingRuntimeCompletionState::Replayed;
                    self.tick(1)?;
                }
            }
            RealmV4RoutingRuntimeEvent::CommitA => {
                let before = self.realm.reload_completion_stats().discarded_after_commit;
                self.publish(1)?;
                self.reload = RealmV4RoutingRuntimeReloadState::Committed;
                if self.realm.reload_completion_stats().discarded_after_commit > before {
                    self.completion_a = RealmV4RoutingRuntimeCompletionState::Discarded;
                }
            }
            RealmV4RoutingRuntimeEvent::ActivationFaultA => {
                let before = self.realm.reload_completion_stats().discarded_after_commit;
                if self.publish(2).is_ok() {
                    return Err("faulting activation succeeded".into());
                }
                self.reload = RealmV4RoutingRuntimeReloadState::ActivationFaulted;
                if self.realm.reload_completion_stats().discarded_after_commit > before {
                    self.completion_a = RealmV4RoutingRuntimeCompletionState::Discarded;
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn snapshot(&self) -> RealmV4RoutingRuntimeSnapshot {
        let stats = self.realm.reload_completion_stats();
        let resources = self.realm.resource_snapshot();
        RealmV4RoutingRuntimeSnapshot {
            reload: self.reload,
            completion_a: self.completion_a,
            completion_b: self.completion_b,
            buffered: stats.buffered,
            replayed: stats.replayed,
            discarded_after_commit: stats.discarded_after_commit,
            pending_completions: self.host.pending_completions(),
            request_reservations: resources.completion_reservations,
        }
    }

    fn complete_a(&mut self) -> Result<(), String> {
        let late_before = self.realm.discarded_late_host_results();
        self.pending_a
            .as_mut()
            .ok_or("module A completion already consumed")?
            .ticket
            .complete(HostPayload::I32(1))
            .map_err(debug)?;
        self.tick(1)?;
        self.pending_a = None;
        self.completion_a = match self.reload {
            RealmV4RoutingRuntimeReloadState::Reloading => {
                if self.realm.reload_buffered_completions() != 1 {
                    return Err("module A completion was not buffered".into());
                }
                RealmV4RoutingRuntimeCompletionState::Buffered
            }
            RealmV4RoutingRuntimeReloadState::RolledBack => {
                if self.realm.terminal_record(self.task_a).is_none() {
                    return Err("module A completion was not delivered after rollback".into());
                }
                RealmV4RoutingRuntimeCompletionState::Delivered
            }
            RealmV4RoutingRuntimeReloadState::Committed
            | RealmV4RoutingRuntimeReloadState::ActivationFaulted => {
                if self.realm.discarded_late_host_results() <= late_before {
                    return Err("late module A completion was not discarded".into());
                }
                RealmV4RoutingRuntimeCompletionState::Discarded
            }
        };
        Ok(())
    }

    fn complete_b(&mut self) -> Result<(), String> {
        self.pending_b
            .as_mut()
            .ok_or("module B completion already consumed")?
            .ticket
            .complete(HostPayload::I32(2))
            .map_err(debug)?;
        self.tick(1)?;
        self.pending_b = None;
        if self.realm.terminal_record(self.task_b).is_none() {
            return Err("module B completion was not delivered".into());
        }
        self.completion_b = RealmV4RoutingRuntimeCompletionState::Delivered;
        Ok(())
    }

    fn publish(&mut self, activation_function: u32) -> Result<(), String> {
        self.realm
            .stage_reload(0, &[RuntimeValue::I32(1)])
            .map_err(debug)?;
        let result = self.realm.commit_reload(ActivationEntry {
            function_id: activation_function,
            arguments: &[],
            fuel: 32,
        });
        if activation_function == 2 {
            result.map(|_| ()).map_err(debug)
        } else {
            if result.map_err(debug)? != self.candidate {
                return Err("wrong candidate published".into());
            }
            Ok(())
        }
    }

    fn tick(&mut self, max_tasks: usize) -> Result<(), String> {
        self.realm
            .tick(TickBudget {
                max_tasks,
                frame_fuel_budget: 32,
                collect_garbage: false,
            })
            .map_err(debug)?;
        Ok(())
    }
}

impl Default for RealmV4RoutingRuntimeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

struct RoutingRegistry {
    hash: crate::StableId,
    requests: Arc<Mutex<VecDeque<PendingHostRequest>>>,
}

impl HostRegistry for RoutingRegistry {
    fn interface_hash(&self) -> Option<crate::StableId> {
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

fn routing_old_module(host: crate::StableId, schema: crate::StableId) -> VerifiedModule {
    let async_result_enum = nexa_bytecode::result_type(ValueType::I32, ValueType::I32);
    let async_result = nexa_bytecode::AsyncResultType {
        result_type: async_result_enum.type_id,
        success: ValueType::I32,
        error: ValueType::I32,
        cancel_policy: nexa_bytecode::CancelPolicy::CancelTask,
        abandon_policy: nexa_bytecode::AbandonPolicy::Trap,
        cancel_error: None,
        abandon_error: None,
    };
    let mut task = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(async_result.result_type)),
        },
        1,
    );
    task.effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    module.enum_type(async_result_enum);
    module.host_import(nexa_bytecode::HostImport {
        stable_id: crate::StableId::from_name("Routing::pending"),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: nexa_bytecode::HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut task = task.finish().expect("routing task is valid");
    task.root_bitmap[0] = true;
    task.root_maps = vec![
        nexa_bytecode::RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        nexa_bytecode::RootMap {
            pc: 1,
            bitmap: vec![true],
        },
    ];
    module.function(task);
    verify(module.finish(), VerifierLimits::default()).expect("routing old module verifies")
}

fn routing_candidate_module(host: crate::StableId, schema: crate::StableId) -> VerifiedModule {
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
    let mut fault = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    fault
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::Trap);
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    module.function(migration.finish().expect("migration is valid"));
    module.function(activation.finish().expect("activation is valid"));
    module.function(fault.finish().expect("fault activation is valid"));
    verify(module.finish(), VerifierLimits::default()).expect("routing candidate verifies")
}
