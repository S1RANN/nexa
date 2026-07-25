use nexa_bytecode::{
    FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::reload::ReloadCompletionBuffer;
use crate::scheduler::{Scheduler, SchedulerCheckpoint};
use crate::stateful::StatefulRegistry;
use crate::task::TaskExecution;
use crate::{
    CheckedInterpreter, FuelState, HostArgs, HostCallOutcome, HostCompletionResult, HostPayload,
    HostRegistry, HostTrap, InterpreterOutcome, OpcodeCostTable, PendingHostRequest, RealmConfig,
    RealmRuntime, ResourceContext, RuntimeHost, RuntimeHostDomain, RuntimeHostState, RuntimeLimits,
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
                nexa_core::RawHandle::new(71, 1, 0),
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
                self.publish(true)?;
                self.reload = RealmV4RoutingRuntimeReloadState::Committed;
                if self.realm.reload_completion_stats().discarded_after_commit > before {
                    self.completion_a = RealmV4RoutingRuntimeCompletionState::Discarded;
                }
            }
            RealmV4RoutingRuntimeEvent::ActivationFaultA => {
                let before = self.realm.reload_completion_stats().discarded_after_commit;
                if self.publish(false).is_ok() {
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

    fn publish(&mut self, activation_succeeds: bool) -> Result<(), String> {
        self.realm
            .stage_reload(&[RuntimeValue::I32(1)])
            .map_err(debug)?;
        let result = self
            .realm
            .commit_reload(&[RuntimeValue::Bool(activation_succeeds)], 32);
        if activation_succeeds {
            if result.map_err(debug)? != self.candidate {
                return Err("wrong candidate published".into());
            }
            Ok(())
        } else {
            result.map(|_| ()).map_err(debug)
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

const REALM_V5_TASK_COUNT: usize = 2;
const REALM_V5_REQUEST_COUNT: usize = 2;
const REALM_V5_RETIRED_COUNT: usize = 3;
const REALM_V5_EPOCH_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeEvent {
    TaskAdmission,
    PollTask,
    FuelYield,
    ExplicitYield,
    ResumeTask,
    TaskComplete,
    HostWait,
    HostComplete,
    Cancel,
    Cleanup,
    BeginReload,
    Quiesce,
    Migration,
    Rollback,
    Commit,
    ActivationFault,
    LateCompletion,
    TokenAcquire,
    TokenRelease,
    SnapshotAcquire,
    SnapshotRelease,
    ReleaseDrain,
    GcRootAttach,
    GcRootDrop,
    GcCollect,
    RetiredEpochReap(u8),
    RuntimeHostBeginClose,
    RuntimeHostFinishClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeRejection {
    Capacity,
    HostNotOpen,
    HostResourcesLive,
    InvalidTaskState,
    InvalidRequestState,
    InvalidReloadState,
    InvalidRetiredEpoch,
    ResourceUnavailable,
    RootUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeApplyError {
    Rejected(RealmV5RuntimeRejection),
    Invariant(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeTaskState {
    Vacant,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeExecution {
    None,
    Ready,
    Running,
    FuelYielded,
    ExplicitYielded,
    Waiting,
    ReloadPaused,
    Cancelling,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealmV5RuntimeRequestState {
    Vacant,
    Pending,
    Buffered,
    Completed,
    Late,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RealmV5RuntimeReloadState {
    #[default]
    Idle,
    Prepared,
    Quiesced,
    Migrated,
    ActivationFaulted,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RealmV5RuntimeRetiredEpoch {
    #[default]
    Vacant,
    Retired(u8),
    Drained(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV5RuntimeTaskSnapshot {
    pub state: RealmV5RuntimeTaskState,
    pub execution: RealmV5RuntimeExecution,
    pub epoch: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealmV5RuntimeRequestSnapshot {
    pub state: RealmV5RuntimeRequestState,
    pub task: Option<u8>,
    pub epoch: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RealmV5RuntimeLedgerSnapshot {
    pub task_slots: usize,
    pub continuations: usize,
    pub scheduler_tokens: usize,
    pub requests: usize,
    pub completion_reservations: usize,
    pub completion_queued: usize,
    pub tokens: usize,
    pub snapshots: usize,
    pub release_records: usize,
    pub heap_objects: usize,
    pub state_objects: usize,
    pub retired_epochs: usize,
    pub terminal_records: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RealmV5RuntimeSnapshot {
    pub active_epoch: u8,
    pub candidate_epoch: Option<u8>,
    pub retired_epochs: [RealmV5RuntimeRetiredEpoch; REALM_V5_RETIRED_COUNT],
    pub tasks: [RealmV5RuntimeTaskSnapshot; REALM_V5_TASK_COUNT],
    pub scheduler: [bool; REALM_V5_TASK_COUNT],
    pub requests: [RealmV5RuntimeRequestSnapshot; REALM_V5_REQUEST_COUNT],
    pub token_live: bool,
    pub token_owner: Option<u8>,
    pub token_epoch: u8,
    pub token_consumed: bool,
    pub snapshot_live: bool,
    pub snapshot_owner: Option<u8>,
    pub snapshot_epoch: u8,
    pub snapshot_consumed: bool,
    pub heap_object: bool,
    pub gc_root: bool,
    pub gc_epoch: u8,
    pub gc_consumed: bool,
    pub reload: RealmV5RuntimeReloadState,
    pub reload_completion_buffer: usize,
    pub release_backlog: [usize; REALM_V5_EPOCH_COUNT],
    pub state_registry_objects: [usize; REALM_V5_EPOCH_COUNT],
    pub runtime_host: RuntimeHostState,
    pub terminal_records: usize,
    pub ledger: RealmV5RuntimeLedgerSnapshot,
}

struct RealmV5RequestSlot {
    state: RealmV5RuntimeRequestState,
    task: Option<u8>,
    epoch: u8,
    pending: Option<PendingHostRequest>,
    handle: Option<crate::HostRequestHandle>,
}

impl Default for RealmV5RequestSlot {
    fn default() -> Self {
        Self {
            state: RealmV5RuntimeRequestState::Vacant,
            task: None,
            epoch: 0,
            pending: None,
            handle: None,
        }
    }
}

#[derive(Clone, Copy)]
struct RealmV5OwnedResource<T> {
    handle: T,
    owner: u8,
    epoch: u8,
}

#[allow(clippy::struct_excessive_bools)]
pub struct RealmV5RuntimeAdapter {
    runtime: TaskRuntime,
    scheduler: Scheduler,
    resources: RuntimeResources,
    heap: crate::Heap,
    host: RuntimeHost,
    scope: crate::ScopeHandle,
    tasks: [Option<TaskHandle>; REALM_V5_TASK_COUNT],
    task_epochs: [u8; REALM_V5_TASK_COUNT],
    terminal: [Option<RealmV5RuntimeTaskState>; REALM_V5_TASK_COUNT],
    terminal_records: Vec<RealmV5RuntimeTaskState>,
    requests: [RealmV5RequestSlot; REALM_V5_REQUEST_COUNT],
    token: Option<RealmV5OwnedResource<crate::ResourceTokenHandle>>,
    token_consumed: bool,
    snapshot: Option<RealmV5OwnedResource<crate::SnapshotHandle>>,
    snapshot_consumed: bool,
    heap_object: Option<crate::GcRef>,
    gc_root: bool,
    gc_epoch: u8,
    gc_consumed: bool,
    reload: RealmV5RuntimeReloadState,
    reload_checkpoints: [Option<ReloadCheckpoint>; REALM_V5_TASK_COUNT],
    reload_completions: ReloadCompletionBuffer,
    active_epoch: u8,
    candidate_epoch: Option<u8>,
    retired_epochs: [RealmV5RuntimeRetiredEpoch; REALM_V5_RETIRED_COUNT],
    registries: [Option<StatefulRegistry>; REALM_V5_EPOCH_COUNT],
}

impl RealmV5RuntimeAdapter {
    #[must_use]
    pub fn new() -> Self {
        let host = RuntimeHost::new(16);
        let mut runtime = TaskRuntime::new(
            83,
            RuntimeLimits {
                max_tasks: 2,
                max_scopes: 1,
                max_frame_segments: 8,
                max_scheduler_tokens: 2,
                max_trace_records: 256,
                max_transient_children_per_scope: 2,
                max_persistent_children_per_scope: 2,
            },
        );
        let scope = runtime
            .create_scope(None)
            .expect("Realm v5 model scope is admitted");
        let mut registries: [Option<StatefulRegistry>; REALM_V5_EPOCH_COUNT] =
            std::array::from_fn(|_| None);
        registries[0] = Some(v5_registry_with_object());
        Self {
            runtime,
            scheduler: Scheduler::with_capacity(2),
            resources: RuntimeResources::with_runtime_host(83, 8, 16, &host),
            heap: crate::Heap::new(1),
            host,
            scope,
            tasks: [None; REALM_V5_TASK_COUNT],
            task_epochs: [0; REALM_V5_TASK_COUNT],
            terminal: [None; REALM_V5_TASK_COUNT],
            terminal_records: Vec::with_capacity(REALM_V5_TASK_COUNT),
            requests: std::array::from_fn(|_| RealmV5RequestSlot::default()),
            token: None,
            token_consumed: false,
            snapshot: None,
            snapshot_consumed: false,
            heap_object: None,
            gc_root: false,
            gc_epoch: 0,
            gc_consumed: false,
            reload: RealmV5RuntimeReloadState::Idle,
            reload_checkpoints: std::array::from_fn(|_| None),
            reload_completions: ReloadCompletionBuffer::new(REALM_V5_REQUEST_COUNT),
            active_epoch: 0,
            candidate_epoch: None,
            retired_epochs: [RealmV5RuntimeRetiredEpoch::Vacant; REALM_V5_RETIRED_COUNT],
            registries,
        }
    }

    pub fn apply(&mut self, event: RealmV5RuntimeEvent) -> Result<(), RealmV5RuntimeApplyError> {
        self.preflight(event)
            .map_err(RealmV5RuntimeApplyError::Rejected)?;
        self.apply_checked(event)
            .map_err(RealmV5RuntimeApplyError::Invariant)?;
        self.validate_runtime_storage()
            .map_err(RealmV5RuntimeApplyError::Invariant)
    }

    #[allow(clippy::too_many_lines)]
    fn preflight(&self, event: RealmV5RuntimeEvent) -> Result<(), RealmV5RuntimeRejection> {
        let all_tasks =
            |state| (0..REALM_V5_TASK_COUNT).all(|index| self.task_state(index) == state);
        match event {
            RealmV5RuntimeEvent::TaskAdmission => {
                if self.host.state() != RuntimeHostState::Open {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
                if !matches!(
                    self.reload,
                    RealmV5RuntimeReloadState::Idle | RealmV5RuntimeReloadState::ActivationFaulted
                ) {
                    return Err(RealmV5RuntimeRejection::InvalidReloadState);
                }
                if self.active_epoch != 0 || !all_tasks(RealmV5RuntimeTaskState::Vacant) {
                    return Err(RealmV5RuntimeRejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::PollTask => {
                if !all_tasks(RealmV5RuntimeTaskState::Ready) {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::FuelYield
            | RealmV5RuntimeEvent::ExplicitYield
            | RealmV5RuntimeEvent::TaskComplete => {
                if !all_tasks(RealmV5RuntimeTaskState::Running) {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::ResumeTask => {
                let state = self.task_state(0);
                if !matches!(
                    state,
                    RealmV5RuntimeTaskState::FuelYielded | RealmV5RuntimeTaskState::ExplicitYielded
                ) || !all_tasks(state)
                {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::HostWait => {
                if self.host.state() != RuntimeHostState::Open {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
                if !all_tasks(RealmV5RuntimeTaskState::Running) {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
                if self
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Vacant)
                {
                    return Err(RealmV5RuntimeRejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::HostComplete => {
                if self
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Pending)
                {
                    return Err(RealmV5RuntimeRejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::Cancel => {
                let state = self.task_state(0);
                if !matches!(
                    state,
                    RealmV5RuntimeTaskState::Ready
                        | RealmV5RuntimeTaskState::Running
                        | RealmV5RuntimeTaskState::FuelYielded
                        | RealmV5RuntimeTaskState::ExplicitYielded
                        | RealmV5RuntimeTaskState::Waiting
                ) || !all_tasks(state)
                {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::Cleanup => {
                let state = self.task_state(0);
                if !matches!(
                    state,
                    RealmV5RuntimeTaskState::Cancelling | RealmV5RuntimeTaskState::Cleanup
                ) || !all_tasks(state)
                {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::BeginReload => {
                if self.host.state() == RuntimeHostState::Closed {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
                if !matches!(
                    self.reload,
                    RealmV5RuntimeReloadState::Idle | RealmV5RuntimeReloadState::ActivationFaulted
                ) {
                    return Err(RealmV5RuntimeRejection::InvalidReloadState);
                }
                if usize::from(self.active_epoch) + 1 >= REALM_V5_EPOCH_COUNT {
                    return Err(RealmV5RuntimeRejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::Quiesce => {
                if self.reload != RealmV5RuntimeReloadState::Prepared || self.active_epoch >= 3 {
                    return Err(RealmV5RuntimeRejection::InvalidReloadState);
                }
                if (0..REALM_V5_TASK_COUNT).any(|index| {
                    self.task_is_live(index)
                        && !matches!(
                            self.task_state(index),
                            RealmV5RuntimeTaskState::Ready
                                | RealmV5RuntimeTaskState::Running
                                | RealmV5RuntimeTaskState::FuelYielded
                                | RealmV5RuntimeTaskState::ExplicitYielded
                                | RealmV5RuntimeTaskState::Waiting
                        )
                }) {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
            }
            RealmV5RuntimeEvent::Migration
                if self.reload != RealmV5RuntimeReloadState::Quiesced =>
            {
                return Err(RealmV5RuntimeRejection::InvalidReloadState);
            }
            RealmV5RuntimeEvent::Rollback
                if !matches!(
                    self.reload,
                    RealmV5RuntimeReloadState::Prepared
                        | RealmV5RuntimeReloadState::Quiesced
                        | RealmV5RuntimeReloadState::Migrated
                ) =>
            {
                return Err(RealmV5RuntimeRejection::InvalidReloadState);
            }
            RealmV5RuntimeEvent::Commit | RealmV5RuntimeEvent::ActivationFault => {
                if self.reload != RealmV5RuntimeReloadState::Migrated {
                    return Err(RealmV5RuntimeRejection::InvalidReloadState);
                }
                if !self
                    .retired_epochs
                    .iter()
                    .any(|epoch| !matches!(epoch, RealmV5RuntimeRetiredEpoch::Retired(_)))
                {
                    return Err(RealmV5RuntimeRejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::LateCompletion => {
                if self
                    .requests
                    .iter()
                    .any(|request| request.state != RealmV5RuntimeRequestState::Late)
                {
                    return Err(RealmV5RuntimeRejection::InvalidRequestState);
                }
            }
            RealmV5RuntimeEvent::TokenAcquire | RealmV5RuntimeEvent::SnapshotAcquire => {
                if self.host.state() != RuntimeHostState::Open {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
                if self.release_backlog().iter().any(|count| *count != 0) {
                    return Err(RealmV5RuntimeRejection::ResourceUnavailable);
                }
                if !all_tasks(RealmV5RuntimeTaskState::Running) {
                    return Err(RealmV5RuntimeRejection::InvalidTaskState);
                }
                let (live, consumed) = match event {
                    RealmV5RuntimeEvent::TokenAcquire => {
                        (self.token.is_some(), self.token_consumed)
                    }
                    RealmV5RuntimeEvent::SnapshotAcquire => {
                        (self.snapshot.is_some(), self.snapshot_consumed)
                    }
                    _ => unreachable!(),
                };
                if consumed {
                    return Err(RealmV5RuntimeRejection::ResourceUnavailable);
                }
                if live {
                    return Err(RealmV5RuntimeRejection::Capacity);
                }
            }
            RealmV5RuntimeEvent::TokenRelease if self.token.is_none() => {
                return Err(RealmV5RuntimeRejection::ResourceUnavailable);
            }
            RealmV5RuntimeEvent::SnapshotRelease if self.snapshot.is_none() => {
                return Err(RealmV5RuntimeRejection::ResourceUnavailable);
            }
            RealmV5RuntimeEvent::ReleaseDrain => {
                if self.release_backlog().iter().all(|count| *count == 0) {
                    return Err(RealmV5RuntimeRejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::GcRootAttach => {
                if self.gc_root || self.gc_consumed || self.active_epoch != 0 {
                    return Err(RealmV5RuntimeRejection::RootUnavailable);
                }
            }
            RealmV5RuntimeEvent::GcRootDrop if !self.gc_root => {
                return Err(RealmV5RuntimeRejection::RootUnavailable);
            }
            RealmV5RuntimeEvent::GcCollect if self.heap_object.is_none() || self.gc_root => {
                return Err(RealmV5RuntimeRejection::RootUnavailable);
            }
            RealmV5RuntimeEvent::RetiredEpochReap(index) => {
                let epoch = match self.retired_epochs.get(usize::from(index)) {
                    Some(RealmV5RuntimeRetiredEpoch::Retired(epoch)) => *epoch,
                    Some(
                        RealmV5RuntimeRetiredEpoch::Vacant | RealmV5RuntimeRetiredEpoch::Drained(_),
                    )
                    | None => return Err(RealmV5RuntimeRejection::InvalidRetiredEpoch),
                };
                if self.epoch_is_blocked(epoch) {
                    return Err(RealmV5RuntimeRejection::ResourceUnavailable);
                }
            }
            RealmV5RuntimeEvent::RuntimeHostBeginClose => {
                if self.host.state() != RuntimeHostState::Open
                    || self.active_epoch < 3
                    || self.candidate_epoch.is_some()
                {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
            }
            RealmV5RuntimeEvent::RuntimeHostFinishClose => {
                if self.host.state() != RuntimeHostState::Closing {
                    return Err(RealmV5RuntimeRejection::HostNotOpen);
                }
                if self.host_resources_live() {
                    return Err(RealmV5RuntimeRejection::HostResourcesLive);
                }
            }
            RealmV5RuntimeEvent::Migration
            | RealmV5RuntimeEvent::Rollback
            | RealmV5RuntimeEvent::TokenRelease
            | RealmV5RuntimeEvent::SnapshotRelease
            | RealmV5RuntimeEvent::GcRootDrop
            | RealmV5RuntimeEvent::GcCollect => {}
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn apply_checked(&mut self, event: RealmV5RuntimeEvent) -> Result<(), String> {
        match event {
            RealmV5RuntimeEvent::TaskAdmission => self.admit_tasks()?,
            RealmV5RuntimeEvent::PollTask => {
                for index in 0..REALM_V5_TASK_COUNT {
                    let task = self.task(index)?;
                    self.scheduler.deschedule(task);
                    self.runtime.poll_task(task).map_err(debug)?;
                    self.replace_execution(index, RealmV5RuntimeExecution::Running)?;
                }
            }
            RealmV5RuntimeEvent::FuelYield => {
                for index in 0..REALM_V5_TASK_COUNT {
                    let task = self.task(index)?;
                    self.runtime.yield_fuel_task(task).map_err(debug)?;
                    self.replace_execution(index, RealmV5RuntimeExecution::FuelYielded)?;
                    self.schedule(index)?;
                }
            }
            RealmV5RuntimeEvent::ExplicitYield => {
                for index in 0..REALM_V5_TASK_COUNT {
                    let task = self.task(index)?;
                    self.runtime.yield_explicit_task(task).map_err(debug)?;
                    self.replace_execution(index, RealmV5RuntimeExecution::ExplicitYielded)?;
                    self.schedule(index)?;
                }
            }
            RealmV5RuntimeEvent::ResumeTask => {
                let explicit = self.task_state(0) == RealmV5RuntimeTaskState::ExplicitYielded;
                for index in 0..REALM_V5_TASK_COUNT {
                    let task = self.task(index)?;
                    self.scheduler.deschedule(task);
                    if explicit {
                        self.runtime.resume_explicit_task(task).map_err(debug)?;
                    } else {
                        self.runtime.resume_fuel_task(task).map_err(debug)?;
                    }
                    self.replace_execution(index, RealmV5RuntimeExecution::Running)?;
                }
            }
            RealmV5RuntimeEvent::TaskComplete => {
                for index in 0..REALM_V5_TASK_COUNT {
                    self.finish_task(index, RealmV5RuntimeTaskState::Completed)?;
                }
            }
            RealmV5RuntimeEvent::HostWait => self.begin_host_wait()?,
            RealmV5RuntimeEvent::HostComplete => self.complete_host_requests(false)?,
            RealmV5RuntimeEvent::Cancel => self.cancel_tasks()?,
            RealmV5RuntimeEvent::Cleanup => self.cleanup_tasks()?,
            RealmV5RuntimeEvent::BeginReload => {
                let candidate = self
                    .active_epoch
                    .checked_add(1)
                    .ok_or("candidate epoch overflow")?;
                self.candidate_epoch = Some(candidate);
                self.registries[usize::from(candidate)] = None;
                self.reload = RealmV5RuntimeReloadState::Prepared;
            }
            RealmV5RuntimeEvent::Quiesce => self.quiesce_tasks()?,
            RealmV5RuntimeEvent::Migration => {
                let candidate = usize::from(self.candidate_epoch.ok_or("missing candidate")?);
                self.registries[candidate] = Some(v5_registry_with_object());
                self.reload = RealmV5RuntimeReloadState::Migrated;
            }
            RealmV5RuntimeEvent::Rollback => self.rollback_reload()?,
            RealmV5RuntimeEvent::Commit => self.publish_reload(false)?,
            RealmV5RuntimeEvent::ActivationFault => self.publish_reload(true)?,
            RealmV5RuntimeEvent::LateCompletion => self.complete_host_requests(true)?,
            RealmV5RuntimeEvent::TokenAcquire => {
                let owner = self.task(0)?;
                let handle = self
                    .resources
                    .context(
                        owner,
                        u32::from(self.active_epoch),
                        u64::from(self.active_epoch),
                    )
                    .create_token(RuntimeHostDomain::Render)
                    .map_err(debug)?;
                self.token = Some(RealmV5OwnedResource {
                    handle,
                    owner: 0,
                    epoch: self.active_epoch,
                });
            }
            RealmV5RuntimeEvent::TokenRelease => {
                let token = self.token.take().ok_or("missing token")?;
                self.resources
                    .release_token_for_model(self.task(usize::from(token.owner))?, token.handle)
                    .map_err(debug)?;
                self.token_consumed = true;
            }
            RealmV5RuntimeEvent::SnapshotAcquire => {
                let owner = self.task(0)?;
                let handle = self
                    .resources
                    .context(
                        owner,
                        u32::from(self.active_epoch),
                        u64::from(self.active_epoch),
                    )
                    .create_snapshot(Arc::from([1_i32, 2_i32]))
                    .map_err(debug)?;
                self.snapshot = Some(RealmV5OwnedResource {
                    handle,
                    owner: 0,
                    epoch: self.active_epoch,
                });
            }
            RealmV5RuntimeEvent::SnapshotRelease => {
                let snapshot = self.snapshot.take().ok_or("missing snapshot")?;
                self.resources
                    .release_snapshot_for_model(
                        self.task(usize::from(snapshot.owner))?,
                        snapshot.handle,
                    )
                    .map_err(debug)?;
                self.snapshot_consumed = true;
            }
            RealmV5RuntimeEvent::ReleaseDrain => {
                let _ = self.resources.drain_releases();
            }
            RealmV5RuntimeEvent::GcRootAttach => {
                self.heap_object = Some(
                    self.heap
                        .allocate(crate::Object::I32Array(vec![1]))
                        .map_err(debug)?,
                );
                self.gc_root = true;
                self.gc_epoch = self.active_epoch;
                self.gc_consumed = true;
            }
            RealmV5RuntimeEvent::GcRootDrop => self.gc_root = false,
            RealmV5RuntimeEvent::GcCollect => {
                self.heap
                    .collect(&crate::GcRoots::default())
                    .map_err(debug)?;
                self.heap_object = None;
            }
            RealmV5RuntimeEvent::RetiredEpochReap(index) => {
                let slot = usize::from(index);
                let RealmV5RuntimeRetiredEpoch::Retired(epoch) = self.retired_epochs[slot] else {
                    return Err("preflight lost retired epoch".into());
                };
                self.retired_epochs[slot] = RealmV5RuntimeRetiredEpoch::Drained(epoch);
                self.registries[usize::from(epoch)] = None;
            }
            RealmV5RuntimeEvent::RuntimeHostBeginClose => {
                let _ = self.host.begin_close();
            }
            RealmV5RuntimeEvent::RuntimeHostFinishClose => {
                self.host.try_finish_close().map_err(debug)?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<RealmV5RuntimeSnapshot, String> {
        let tasks = std::array::from_fn(|index| RealmV5RuntimeTaskSnapshot {
            state: self.task_state(index),
            execution: self.execution_state(index),
            epoch: self.task_epochs[index],
        });
        let scheduler = std::array::from_fn(|index| {
            self.tasks[index].is_some_and(|task| {
                matches!(
                    self.scheduler.checkpoint(task),
                    SchedulerCheckpoint::Ready { .. }
                )
            })
        });
        let requests = std::array::from_fn(|index| RealmV5RuntimeRequestSnapshot {
            state: self.requests[index].state,
            task: self.requests[index].task,
            epoch: self.requests[index].epoch,
        });
        let release_backlog = self.release_backlog();
        let state_registry_objects = std::array::from_fn(|index| {
            self.registries[index]
                .as_ref()
                .map_or(0, StatefulRegistry::object_count)
        });
        let resource_snapshot = self.resources.model_snapshot();
        let (task_slots, _, continuations) = self.runtime.ledger_counts();
        let scheduler_tokens = scheduler.iter().filter(|scheduled| **scheduled).count();
        let heap_objects = usize::from(
            self.heap_object
                .is_some_and(|reference| self.heap.resolve(reference).is_ok()),
        );
        let state_objects = state_registry_objects.iter().sum();
        let retired_epochs = self
            .retired_epochs
            .iter()
            .filter(|epoch| matches!(epoch, RealmV5RuntimeRetiredEpoch::Retired(_)))
            .count();
        Ok(RealmV5RuntimeSnapshot {
            active_epoch: self.active_epoch,
            candidate_epoch: self.candidate_epoch,
            retired_epochs: self.retired_epochs,
            tasks,
            scheduler,
            requests,
            token_live: self.token.is_some(),
            token_owner: self.token.map(|token| token.owner),
            token_epoch: self.token.map_or(0, |token| token.epoch),
            token_consumed: self.token_consumed,
            snapshot_live: self.snapshot.is_some(),
            snapshot_owner: self.snapshot.map(|snapshot| snapshot.owner),
            snapshot_epoch: self.snapshot.map_or(0, |snapshot| snapshot.epoch),
            snapshot_consumed: self.snapshot_consumed,
            heap_object: heap_objects != 0,
            gc_root: self.gc_root,
            gc_epoch: self.gc_epoch,
            gc_consumed: self.gc_consumed,
            reload: self.reload,
            reload_completion_buffer: self.reload_completions.len(),
            release_backlog,
            state_registry_objects,
            runtime_host: self.host.state(),
            terminal_records: self.terminal_records.len(),
            ledger: RealmV5RuntimeLedgerSnapshot {
                task_slots,
                continuations,
                scheduler_tokens,
                requests: resource_snapshot.requests,
                completion_reservations: resource_snapshot.completion_reservations,
                completion_queued: resource_snapshot.completion_queued,
                tokens: resource_snapshot.tokens,
                snapshots: resource_snapshot.snapshots,
                release_records: resource_snapshot.release_records,
                heap_objects,
                state_objects,
                retired_epochs,
                terminal_records: self.terminal_records.len(),
            },
        })
    }

    fn admit_tasks(&mut self) -> Result<(), String> {
        let module = v5_task_module();
        for index in 0..REALM_V5_TASK_COUNT {
            let task = self
                .runtime
                .admit_task(self.scope, u64::from(self.active_epoch), true)
                .map_err(debug)?;
            let continuation = continuation_with_user_defer(module);
            self.runtime
                .attach_continuation(
                    task,
                    1,
                    FuelState::new(64, 0, 1_024),
                    continuation,
                    nexa_core::RawHandle::new(83, u32::from(self.active_epoch), 0),
                    TaskLimits::default(),
                )
                .map_err(debug)?;
            self.scheduler.schedule(task, 1);
            self.tasks[index] = Some(task);
            self.task_epochs[index] = self.active_epoch;
        }
        Ok(())
    }

    fn begin_host_wait(&mut self) -> Result<(), String> {
        for index in 0..REALM_V5_TASK_COUNT {
            let task = self.task(index)?;
            let pending = self
                .resources
                .context(
                    task,
                    u32::from(self.active_epoch),
                    u64::from(self.active_epoch),
                )
                .create_request()
                .map_err(debug)?;
            let request = pending.request;
            self.runtime.await_task(task).map_err(debug)?;
            let snapshot = self.runtime.task_snapshot(task).map_err(debug)?;
            let continuation = self
                .runtime
                .take_execution(task)
                .map_err(debug)?
                .into_continuation();
            self.runtime
                .put_execution(
                    task,
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
            self.scheduler.deschedule(task);
            self.scheduler.wait_for(request, task);
            self.requests[index] = RealmV5RequestSlot {
                state: RealmV5RuntimeRequestState::Pending,
                task: Some(index_u8(index)),
                epoch: self.active_epoch,
                pending: Some(pending),
                handle: Some(request),
            };
        }
        Ok(())
    }

    fn complete_host_requests(&mut self, late: bool) -> Result<(), String> {
        for request in &mut self.requests {
            request
                .pending
                .as_mut()
                .ok_or("missing pending completion")?
                .ticket
                .complete(HostPayload::I32(9))
                .map_err(debug)?;
            request.pending = None;
        }
        let deliveries = self.resources.drain_completions();
        if deliveries.len() != REALM_V5_REQUEST_COUNT {
            return Err("completion queue did not yield both requests".into());
        }
        if late {
            for request in &mut self.requests {
                request.state = RealmV5RuntimeRequestState::Completed;
            }
            return Ok(());
        }
        let buffer = matches!(
            self.reload,
            RealmV5RuntimeReloadState::Quiesced | RealmV5RuntimeReloadState::Migrated
        );
        if buffer {
            for delivery in deliveries {
                self.reload_completions.push(delivery).map_err(debug)?;
            }
            for request in &mut self.requests {
                request.state = RealmV5RuntimeRequestState::Buffered;
            }
        } else {
            for delivery in deliveries {
                let index = self
                    .requests
                    .iter()
                    .position(|request| request.handle == Some(delivery.request))
                    .ok_or("completion request identity was lost")?;
                self.deliver_ready(index)?;
                self.requests[index].state = RealmV5RuntimeRequestState::Completed;
            }
        }
        Ok(())
    }

    fn deliver_ready(&mut self, index: usize) -> Result<(), String> {
        let task = self.task(index)?;
        let request = self.requests[index]
            .handle
            .ok_or("missing request handle")?;
        if self.scheduler.wake_request(request) != Some(task) {
            return Err("waiting scheduler mapping was lost".into());
        }
        let snapshot = self.runtime.task_snapshot(task).map_err(debug)?;
        let execution = self.runtime.take_execution(task).map_err(debug)?;
        let continuation = execution.into_continuation();
        self.runtime.resume_waiting_task(task).map_err(debug)?;
        let mut ready = snapshot;
        ready.state = TaskState::Ready;
        self.runtime
            .restore_task_checkpoint(task, ready, TaskExecution::Ready(continuation))
            .map_err(debug)?;
        self.scheduler.schedule(task, ready.priority);
        Ok(())
    }

    fn cancel_tasks(&mut self) -> Result<(), String> {
        for index in 0..REALM_V5_TASK_COUNT {
            let task = self.task(index)?;
            if self.task_state(index) == RealmV5RuntimeTaskState::Ready {
                self.scheduler.deschedule(task);
                self.runtime.poll_task(task).map_err(debug)?;
                self.replace_execution(index, RealmV5RuntimeExecution::Running)?;
            }
            if self.task_state(index) == RealmV5RuntimeTaskState::Waiting {
                let request = self.requests[index]
                    .handle
                    .ok_or("waiting task lacks request")?;
                self.resources
                    .detach_request_for_model(task, request)
                    .map_err(debug)?;
                self.requests[index].state = RealmV5RuntimeRequestState::Late;
            }
            self.runtime.request_task_cancel(task).map_err(debug)?;
            self.runtime.reach_task_safepoint(task).map_err(debug)?;
            self.runtime
                .mark_execution_cancelling(task)
                .map_err(debug)?;
            self.scheduler.cancel_task(task);
        }
        Ok(())
    }

    fn cleanup_tasks(&mut self) -> Result<(), String> {
        let entering = self.task_state(0) == RealmV5RuntimeTaskState::Cancelling;
        for index in 0..REALM_V5_TASK_COUNT {
            let task = self.task(index)?;
            if entering {
                self.runtime.begin_cleanup(task).map_err(debug)?;
                self.runtime.mark_execution_cleanup(task).map_err(debug)?;
            } else {
                self.release_task_resources(index)?;
                self.runtime.finish_cleanup(task).map_err(debug)?;
                self.terminal[index] = Some(RealmV5RuntimeTaskState::Cancelled);
                self.terminal_records
                    .push(RealmV5RuntimeTaskState::Cancelled);
            }
        }
        Ok(())
    }

    fn quiesce_tasks(&mut self) -> Result<(), String> {
        for index in 0..REALM_V5_TASK_COUNT {
            if !self.task_is_live(index) {
                continue;
            }
            let task = self.task(index)?;
            let snapshot = self.runtime.task_snapshot(task).map_err(debug)?;
            let execution = self.runtime.execution_checkpoint(task).map_err(debug)?;
            let scheduler = self.scheduler.checkpoint(task);
            match snapshot.state {
                TaskState::Ready => {
                    self.runtime.poll_task(task).map_err(debug)?;
                    self.runtime.pause_task_for_reload(task).map_err(debug)?;
                }
                TaskState::Running => self.runtime.pause_task_for_reload(task).map_err(debug)?,
                TaskState::FuelYielded | TaskState::ExplicitYielded | TaskState::Waiting => {
                    self.runtime.request_reload_pause(task).map_err(debug)?;
                }
                _ => return Err("task cannot quiesce".into()),
            }
            self.runtime
                .mark_execution_reload_paused(task)
                .map_err(debug)?;
            self.scheduler.cancel_task(task);
            self.reload_checkpoints[index] = Some(ReloadCheckpoint {
                snapshot,
                execution,
                scheduler,
            });
        }
        self.reload = RealmV5RuntimeReloadState::Quiesced;
        Ok(())
    }

    fn rollback_reload(&mut self) -> Result<(), String> {
        for index in 0..REALM_V5_TASK_COUNT {
            if let Some(checkpoint) = self.reload_checkpoints[index].take() {
                let task = self.task(index)?;
                self.runtime
                    .restore_task_checkpoint(task, checkpoint.snapshot, checkpoint.execution)
                    .map_err(debug)?;
                self.scheduler.restore(task, checkpoint.scheduler);
            }
        }
        let buffered = self.reload_completions.drain_ordered().collect::<Vec<_>>();
        for delivery in buffered {
            let index = self
                .requests
                .iter()
                .position(|request| request.handle == Some(delivery.request))
                .ok_or("buffered request identity was lost")?;
            self.deliver_ready(index)?;
            self.requests[index].state = RealmV5RuntimeRequestState::Completed;
        }
        if let Some(candidate) = self.candidate_epoch {
            self.registries[usize::from(candidate)] = None;
        }
        self.candidate_epoch = None;
        self.reload = RealmV5RuntimeReloadState::Idle;
        Ok(())
    }

    fn publish_reload(&mut self, activation_fault: bool) -> Result<(), String> {
        let retired_slot = self
            .retired_epochs
            .iter()
            .position(|epoch| !matches!(epoch, RealmV5RuntimeRetiredEpoch::Retired(_)))
            .ok_or("retired registry is full")?;
        let old_epoch = self.active_epoch;
        let candidate = self.candidate_epoch.ok_or("missing candidate")?;
        self.retired_epochs[retired_slot] = RealmV5RuntimeRetiredEpoch::Retired(old_epoch);
        self.active_epoch = candidate;
        self.candidate_epoch = None;
        for index in 0..REALM_V5_TASK_COUNT {
            if self.task_state(index) == RealmV5RuntimeTaskState::ReloadPaused {
                let task = self.task(index)?;
                if self.requests[index].state == RealmV5RuntimeRequestState::Pending {
                    let request = self.requests[index]
                        .handle
                        .ok_or("pending task lacks request")?;
                    self.resources
                        .detach_request_for_model(task, request)
                        .map_err(debug)?;
                    self.requests[index].state = RealmV5RuntimeRequestState::Late;
                }
                self.release_task_resources(index)?;
                self.runtime
                    .begin_reload_commit_cancel(task)
                    .map_err(debug)?;
                self.runtime
                    .mark_execution_cancelling(task)
                    .map_err(debug)?;
                self.runtime
                    .finish_cancel_without_cleanup(task)
                    .map_err(debug)?;
                self.terminal[index] = Some(RealmV5RuntimeTaskState::Cancelled);
                self.terminal_records
                    .push(RealmV5RuntimeTaskState::Cancelled);
                self.reload_checkpoints[index] = None;
            }
        }
        for request in &mut self.requests {
            if request.state == RealmV5RuntimeRequestState::Buffered {
                request.state = RealmV5RuntimeRequestState::Completed;
            }
        }
        for _ in self.reload_completions.drain_ordered() {}
        self.reload = if activation_fault {
            RealmV5RuntimeReloadState::ActivationFaulted
        } else {
            RealmV5RuntimeReloadState::Idle
        };
        Ok(())
    }

    fn finish_task(
        &mut self,
        index: usize,
        terminal: RealmV5RuntimeTaskState,
    ) -> Result<(), String> {
        let task = self.task(index)?;
        self.scheduler.cancel_task(task);
        self.release_task_resources(index)?;
        self.runtime.finish_task(task).map_err(debug)?;
        self.terminal[index] = Some(terminal);
        self.terminal_records.push(terminal);
        Ok(())
    }

    fn release_task_resources(&mut self, index: usize) -> Result<(), String> {
        let task = self.task(index)?;
        self.resources.cleanup_task(task, false).map_err(debug)?;
        if self
            .token
            .is_some_and(|token| usize::from(token.owner) == index)
        {
            self.token = None;
            self.token_consumed = true;
        }
        if self
            .snapshot
            .is_some_and(|snapshot| usize::from(snapshot.owner) == index)
        {
            self.snapshot = None;
            self.snapshot_consumed = true;
        }
        Ok(())
    }

    fn replace_execution(
        &mut self,
        index: usize,
        kind: RealmV5RuntimeExecution,
    ) -> Result<(), String> {
        let task = self.task(index)?;
        let snapshot = self.runtime.task_snapshot(task).map_err(debug)?;
        let continuation = self
            .runtime
            .take_execution(task)
            .map_err(debug)?
            .into_continuation();
        let execution = match kind {
            RealmV5RuntimeExecution::Ready => TaskExecution::Ready(continuation),
            RealmV5RuntimeExecution::Running => TaskExecution::Running(continuation),
            RealmV5RuntimeExecution::FuelYielded => TaskExecution::FuelYielded(continuation),
            RealmV5RuntimeExecution::ExplicitYielded => {
                TaskExecution::ExplicitYielded(continuation)
            }
            RealmV5RuntimeExecution::ReloadPaused => TaskExecution::ReloadPaused(continuation),
            RealmV5RuntimeExecution::Cancelling => TaskExecution::Cancelling(continuation),
            RealmV5RuntimeExecution::Cleanup => TaskExecution::Cleanup(continuation),
            RealmV5RuntimeExecution::None | RealmV5RuntimeExecution::Waiting => {
                return Err("execution kind requires dedicated metadata".into());
            }
        };
        self.runtime
            .put_execution(task, execution, snapshot.fuel)
            .map_err(debug)
    }

    fn schedule(&mut self, index: usize) -> Result<(), String> {
        let task = self.task(index)?;
        let priority = self.runtime.task_snapshot(task).map_err(debug)?.priority;
        self.scheduler.schedule(task, priority);
        Ok(())
    }

    fn task(&self, index: usize) -> Result<TaskHandle, String> {
        self.tasks[index].ok_or_else(|| "missing task".into())
    }

    fn task_state(&self, index: usize) -> RealmV5RuntimeTaskState {
        if let Some(terminal) = self.terminal[index] {
            return terminal;
        }
        let Some(task) = self.tasks[index] else {
            return RealmV5RuntimeTaskState::Vacant;
        };
        match self
            .runtime
            .task_snapshot(task)
            .map(|snapshot| snapshot.state)
        {
            Ok(TaskState::Ready) => RealmV5RuntimeTaskState::Ready,
            Ok(TaskState::Running) => RealmV5RuntimeTaskState::Running,
            Ok(TaskState::FuelYielded) => RealmV5RuntimeTaskState::FuelYielded,
            Ok(TaskState::ExplicitYielded) => RealmV5RuntimeTaskState::ExplicitYielded,
            Ok(TaskState::Waiting) => RealmV5RuntimeTaskState::Waiting,
            Ok(TaskState::ReloadPaused | TaskState::ReloadPauseRequested) => {
                RealmV5RuntimeTaskState::ReloadPaused
            }
            Ok(TaskState::Cancelling | TaskState::CancelRequested) => {
                RealmV5RuntimeTaskState::Cancelling
            }
            Ok(TaskState::Cleanup) => RealmV5RuntimeTaskState::Cleanup,
            Ok(TaskState::Completed) => RealmV5RuntimeTaskState::Completed,
            Ok(TaskState::Cancelled) => RealmV5RuntimeTaskState::Cancelled,
            Ok(TaskState::Created | TaskState::Trapped) | Err(_) => RealmV5RuntimeTaskState::Vacant,
        }
    }

    fn execution_state(&self, index: usize) -> RealmV5RuntimeExecution {
        let Some(task) = self.tasks[index] else {
            return RealmV5RuntimeExecution::None;
        };
        match self.runtime.execution(task) {
            Ok(TaskExecution::Ready(_)) => RealmV5RuntimeExecution::Ready,
            Ok(TaskExecution::Running(_)) => RealmV5RuntimeExecution::Running,
            Ok(TaskExecution::FuelYielded(_)) => RealmV5RuntimeExecution::FuelYielded,
            Ok(TaskExecution::ExplicitYielded(_)) => RealmV5RuntimeExecution::ExplicitYielded,
            Ok(TaskExecution::Waiting { .. }) => RealmV5RuntimeExecution::Waiting,
            Ok(TaskExecution::ReloadPaused(_)) => RealmV5RuntimeExecution::ReloadPaused,
            Ok(TaskExecution::Cancelling(_)) => RealmV5RuntimeExecution::Cancelling,
            Ok(TaskExecution::Cleanup(_)) => RealmV5RuntimeExecution::Cleanup,
            Err(_) => RealmV5RuntimeExecution::None,
        }
    }

    fn task_is_live(&self, index: usize) -> bool {
        !matches!(
            self.task_state(index),
            RealmV5RuntimeTaskState::Vacant
                | RealmV5RuntimeTaskState::Completed
                | RealmV5RuntimeTaskState::Cancelled
        )
    }

    fn release_backlog(&self) -> [usize; REALM_V5_EPOCH_COUNT] {
        std::array::from_fn(|epoch| {
            self.resources
                .epoch_counts(index_u32(epoch), epoch as u64)
                .pending_releases
        })
    }

    fn epoch_is_blocked(&self, epoch: u8) -> bool {
        self.task_epochs
            .iter()
            .enumerate()
            .any(|(index, task_epoch)| *task_epoch == epoch && self.task_is_live(index))
            || self.requests.iter().any(|request| {
                request.epoch == epoch
                    && matches!(
                        request.state,
                        RealmV5RuntimeRequestState::Pending
                            | RealmV5RuntimeRequestState::Buffered
                            | RealmV5RuntimeRequestState::Late
                    )
            })
            || self.token.is_some_and(|token| token.epoch == epoch)
            || self
                .snapshot
                .is_some_and(|snapshot| snapshot.epoch == epoch)
            || (self.gc_root && self.gc_epoch == epoch)
            || self.release_backlog()[usize::from(epoch)] != 0
    }

    fn host_resources_live(&self) -> bool {
        self.requests.iter().any(|request| {
            matches!(
                request.state,
                RealmV5RuntimeRequestState::Pending
                    | RealmV5RuntimeRequestState::Buffered
                    | RealmV5RuntimeRequestState::Late
            )
        }) || self.token.is_some()
            || self.snapshot.is_some()
            || self.release_backlog().iter().any(|count| *count != 0)
    }

    fn validate_runtime_storage(&self) -> Result<(), String> {
        let snapshot = self.snapshot()?;
        if snapshot.ledger.task_slots
            != snapshot
                .tasks
                .iter()
                .filter(|task| {
                    !matches!(
                        task.state,
                        RealmV5RuntimeTaskState::Vacant
                            | RealmV5RuntimeTaskState::Completed
                            | RealmV5RuntimeTaskState::Cancelled
                    )
                })
                .count()
        {
            return Err("TaskRuntime slot ledger diverged".into());
        }
        if snapshot.ledger.scheduler_tokens
            != snapshot
                .scheduler
                .iter()
                .filter(|scheduled| **scheduled)
                .count()
        {
            return Err("Scheduler ledger diverged".into());
        }
        if snapshot.ledger.tokens != usize::from(snapshot.token_live)
            || snapshot.ledger.snapshots != usize::from(snapshot.snapshot_live)
        {
            return Err("RuntimeResources token/snapshot ledger diverged".into());
        }
        if snapshot.heap_object != (snapshot.ledger.heap_objects != 0) {
            return Err("Heap ledger diverged".into());
        }
        Ok(())
    }
}

impl Default for RealmV5RuntimeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for RealmV5RuntimeAdapter {
    fn drop(&mut self) {
        for request in &mut self.requests {
            request.pending = None;
        }
        let _ = self.resources.drain_completions();
        for task in self.tasks.iter().flatten().copied() {
            let _ = self.resources.cleanup_task(task, false);
        }
        let _ = self.resources.drain_completions();
        let _ = self.resources.drain_releases();
        let _ = self.host.begin_close();
        let _ = self.host.try_finish_close();
    }
}

fn v5_task_module() -> &'static VerifiedModule {
    static MODULE: std::sync::OnceLock<VerifiedModule> = std::sync::OnceLock::new();
    MODULE.get_or_init(|| cleanup_module(false))
}

fn v5_registry_with_object() -> StatefulRegistry {
    let mut registry = StatefulRegistry::try_new(
        crate::StatefulDomainId::new(1),
        crate::MigrationLimits {
            max_objects: 1,
            max_fields: 0,
            max_forwarding_entries: 1,
            max_state_bytes: 16,
            max_gc_roots: 0,
            max_fuel: 16,
            max_call_depth: 1,
        },
    )
    .expect("Realm v5 state registry fits");
    registry
        .insert(
            crate::StableId::from_name("realm-v5-state-object"),
            crate::StateValue::I32(1),
        )
        .expect("Realm v5 state object fits");
    registry
}

fn index_u8(index: usize) -> u8 {
    u8::try_from(index).expect("Realm v5 fixed index fits u8")
}

fn index_u32(index: usize) -> u32 {
    u32::try_from(index).expect("Realm v5 fixed index fits u32")
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
            .map_err(|_| HostTrap::Host("host request admission failed".into()))?;
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
            parameters: vec![ValueType::Bool],
            result: None,
        },
        1,
    );
    activation
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::JumpIfFalse {
            condition: 0,
            target: 2,
        })
        .emit(Instruction::ReturnVoid)
        .emit(Instruction::Trap);
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    module.function(migration.finish().expect("migration is valid"));
    module.function(activation.finish().expect("activation is valid"));
    verify(module.finish(), VerifierLimits::default()).expect("routing candidate verifies")
}
