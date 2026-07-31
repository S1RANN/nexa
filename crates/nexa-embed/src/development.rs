use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::artifact::{CandidateCompilation, compile_package_candidate};
use crate::contract::ExportRequirement;
use crate::{
    CandidateIdentity, EngineDiagnostic, EngineDiagnosticStage, PackageId, ReloadReportSummary,
    SourceId,
};

pub(crate) type SharedPackageBuildSession = Arc<Mutex<nexa::PackageBuildSession>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevelopmentConfig {
    pub enabled: bool,
    pub scan_interval_ticks: u64,
    pub stable_scan_count: u8,
    pub compile_queue_capacity: usize,
    pub result_queue_capacity: usize,
    pub retain_events: usize,
    pub auto_reload: bool,
}

impl Default for DevelopmentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_ticks: 15,
            stable_scan_count: 2,
            compile_queue_capacity: 16,
            result_queue_capacity: 16,
            retain_events: 128,
            auto_reload: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DevelopmentState {
    #[default]
    Idle,
    ChangeObserved,
    WaitingForStableWrite,
    AwaitingQueue,
    CompileQueued,
    Compiling,
    CandidateReady,
    ReloadPending,
    Reloading,
    Reloaded,
    CompileFailed,
    VerifyFailed,
    MigrationFailed,
    ActivationFaulted,
    HostRebuildRequired,
    SourceMissing,
}

#[derive(Clone, Debug)]
pub struct DevelopmentEventData {
    pub identity: CandidateIdentity,
    pub diagnostic: Option<EngineDiagnostic>,
    pub reload: Option<ReloadReportSummary>,
    pub queue_duration: Option<Duration>,
}

#[derive(Clone, Debug)]
pub enum DevelopmentEvent {
    ChangeDetected(DevelopmentEventData),
    ChangeStabilized(DevelopmentEventData),
    CompileQueued(DevelopmentEventData),
    CompileStarted(DevelopmentEventData),
    CompileSucceeded(DevelopmentEventData),
    CompileFailed(DevelopmentEventData),
    VerifyFailed(DevelopmentEventData),
    CandidateSuperseded(DevelopmentEventData),
    CandidateReady(DevelopmentEventData),
    ReloadStarted(DevelopmentEventData),
    ReloadCommitted(DevelopmentEventData),
    ReloadRolledBack(DevelopmentEventData),
    ActivationFaulted(DevelopmentEventData),
    HostRebuildRequired(DevelopmentEventData),
    SourceMissing(DevelopmentEventData),
    CandidateCancelled(DevelopmentEventData),
}

impl DevelopmentEvent {
    #[must_use]
    pub const fn data(&self) -> &DevelopmentEventData {
        match self {
            Self::ChangeDetected(data)
            | Self::ChangeStabilized(data)
            | Self::CompileQueued(data)
            | Self::CompileStarted(data)
            | Self::CompileSucceeded(data)
            | Self::CompileFailed(data)
            | Self::VerifyFailed(data)
            | Self::CandidateSuperseded(data)
            | Self::CandidateReady(data)
            | Self::ReloadStarted(data)
            | Self::ReloadCommitted(data)
            | Self::ReloadRolledBack(data)
            | Self::ActivationFaulted(data)
            | Self::HostRebuildRequired(data)
            | Self::SourceMissing(data)
            | Self::CandidateCancelled(data) => data,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ChangeDetected(_) => "change-detected",
            Self::ChangeStabilized(_) => "change-stabilized",
            Self::CompileQueued(_) => "compile-queued",
            Self::CompileStarted(_) => "compile-started",
            Self::CompileSucceeded(_) => "compile-succeeded",
            Self::CompileFailed(_) => "compile-failed",
            Self::VerifyFailed(_) => "verify-failed",
            Self::CandidateSuperseded(_) => "candidate-superseded",
            Self::CandidateReady(_) => "candidate-ready",
            Self::ReloadStarted(_) => "reload-started",
            Self::ReloadCommitted(_) => "reload-committed",
            Self::ReloadRolledBack(_) => "reload-rolled-back",
            Self::ActivationFaulted(_) => "activation-faulted",
            Self::HostRebuildRequired(_) => "host-rebuild-required",
            Self::SourceMissing(_) => "source-missing",
            Self::CandidateCancelled(_) => "candidate-cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateTerminalKind {
    Compiled,
    CompileFailed,
    VerifyFailed,
    SupersededBeforeCompile,
    SupersededAfterCompile,
    CancelledByDisable,
    CancelledBySourceRemoval,
    CancelledByShutdown,
    RejectedHostContractChange,
}

#[derive(Clone, Debug)]
pub struct CandidateTerminalData {
    pub source_id: SourceId,
    pub identity: CandidateIdentity,
    pub build_input: Arc<nexa_analysis::ResolvedBuildInput>,
    pub queue_duration: Duration,
    pub work_duration: Duration,
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CandidateTerminal {
    Compiled {
        data: CandidateTerminalData,
        build_input: Arc<nexa_analysis::ResolvedBuildInput>,
        compilation: CandidateCompilation,
    },
    CompileFailed {
        data: CandidateTerminalData,
        diagnostic: EngineDiagnostic,
        additional_diagnostics: Vec<EngineDiagnostic>,
        compile_duration: Duration,
        verify_duration: Duration,
    },
    VerifyFailed {
        data: CandidateTerminalData,
        diagnostic: EngineDiagnostic,
        additional_diagnostics: Vec<EngineDiagnostic>,
        compile_duration: Duration,
        verify_duration: Duration,
    },
    SupersededBeforeCompile(CandidateTerminalData),
    SupersededAfterCompile(CandidateTerminalData),
    CancelledByDisable(CandidateTerminalData),
    CancelledBySourceRemoval(CandidateTerminalData),
    CancelledByShutdown(CandidateTerminalData),
    RejectedHostContractChange(CandidateTerminalData),
}

impl CandidateTerminal {
    #[must_use]
    pub const fn data(&self) -> &CandidateTerminalData {
        match self {
            Self::Compiled { data, .. }
            | Self::CompileFailed { data, .. }
            | Self::VerifyFailed { data, .. }
            | Self::SupersededBeforeCompile(data)
            | Self::SupersededAfterCompile(data)
            | Self::CancelledByDisable(data)
            | Self::CancelledBySourceRemoval(data)
            | Self::CancelledByShutdown(data)
            | Self::RejectedHostContractChange(data) => data,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> CandidateTerminalKind {
        match self {
            Self::Compiled { .. } => CandidateTerminalKind::Compiled,
            Self::CompileFailed { .. } => CandidateTerminalKind::CompileFailed,
            Self::VerifyFailed { .. } => CandidateTerminalKind::VerifyFailed,
            Self::SupersededBeforeCompile(_) => CandidateTerminalKind::SupersededBeforeCompile,
            Self::SupersededAfterCompile(_) => CandidateTerminalKind::SupersededAfterCompile,
            Self::CancelledByDisable(_) => CandidateTerminalKind::CancelledByDisable,
            Self::CancelledBySourceRemoval(_) => CandidateTerminalKind::CancelledBySourceRemoval,
            Self::CancelledByShutdown(_) => CandidateTerminalKind::CancelledByShutdown,
            Self::RejectedHostContractChange(_) => {
                CandidateTerminalKind::RejectedHostContractChange
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum WorkerEvent {
    CompileStarted {
        source_id: SourceId,
        identity: CandidateIdentity,
        queue_duration: Duration,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerInspection {
    pub queued_packages: usize,
    pub in_flight_package: Option<PackageId>,
    pub completed_results: usize,
    pub backpressure_count: u64,
    pub pending_superseded_count: u64,
    pub compiled_superseded_count: u64,
    pub cancelled_count: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PackageDevelopment {
    pub state: DevelopmentState,
    pub desired_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub observed_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub unqueued_generation: Option<CandidateTerminalData>,
    pub stable_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub queued_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub queued_generation: Option<u64>,
    pub in_flight_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub in_flight_generation: Option<u64>,
    pub terminal_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub active_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    pub stable_scans: u8,
    pub latest_generation: u64,
    pub terminal_count: u64,
    pub duplicate_terminal_count: u64,
    pub desired_build_fingerprint_mismatch_rejection_count: u64,
    pub terminal_generations: BTreeMap<u64, CandidateTerminalKind>,
    pub change_observed_at: Option<Instant>,
    pub last_change_to_stable_duration: Duration,
    pub last_ready_to_commit_duration: Duration,
    pub last_quiesce_duration: Duration,
    pub last_commit_duration: Duration,
    pub last_total_change_to_visible_duration: Duration,
    pub last_compile_duration: Option<Duration>,
    pub last_reload_duration: Option<Duration>,
    pub last_discovery_duration: Duration,
    pub last_build_fingerprint_duration: Duration,
    pub last_queue_duration: Duration,
    pub last_verify_duration: Duration,
    pub last_migration_duration: Duration,
    pub last_activation_duration: Duration,
    pub recent_metrics: VecDeque<crate::PackageMetric>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadyCandidate {
    pub build_input: Arc<nexa_analysis::ResolvedBuildInput>,
    pub compilation: CandidateCompilation,
    pub terminal_data: CandidateTerminalData,
}

#[derive(Clone)]
pub struct CompileJob {
    pub source_id: SourceId,
    pub identity: CandidateIdentity,
    pub build_input: Arc<nexa_analysis::ResolvedBuildInput>,
    pub idl: nexa::Idl,
    pub required_exports: Vec<ExportRequirement>,
    host_contract_source_identity: nexa::SourceIdentity,
    host_contract_source: Arc<str>,
    queued_at: Instant,
}

impl CompileJob {
    pub(crate) fn new(
        source_id: SourceId,
        identity: CandidateIdentity,
        build_input: Arc<nexa_analysis::ResolvedBuildInput>,
        idl: nexa::Idl,
        required_exports: Vec<ExportRequirement>,
        host_contract_source_identity: nexa::SourceIdentity,
        host_contract_source: Arc<str>,
    ) -> Self {
        Self {
            source_id,
            identity,
            build_input,
            idl,
            required_exports,
            host_contract_source_identity,
            host_contract_source,
            queued_at: Instant::now(),
        }
    }

    fn from_request(request: DevelopmentCompileRequest) -> Self {
        let (host_contract_source_identity, host_contract_source) = {
            let contract = nexa::HostContractInput::canonical(&request.idl);
            (
                contract.source().identity().clone(),
                Arc::clone(contract.source().text()),
            )
        };
        Self::new(
            request.source_id,
            request.identity,
            request.build_input,
            request.idl,
            request.required_exports,
            host_contract_source_identity,
            host_contract_source,
        )
    }

    fn terminal_data(
        &self,
        queue_duration: Duration,
        work_duration: Duration,
    ) -> CandidateTerminalData {
        CandidateTerminalData {
            source_id: self.source_id.clone(),
            identity: self.identity.clone(),
            build_input: Arc::clone(&self.build_input),
            queue_duration,
            work_duration,
        }
    }

    pub(crate) fn cancel(self, reason: CandidateCancellation) -> CandidateTerminal {
        let queue_duration = self.queued_at.elapsed();
        cancelled_terminal(reason, self.terminal_data(queue_duration, Duration::ZERO))
    }

    pub(crate) fn supersede_before_compile(self) -> CandidateTerminal {
        let queue_duration = self.queued_at.elapsed();
        CandidateTerminal::SupersededBeforeCompile(
            self.terminal_data(queue_duration, Duration::ZERO),
        )
    }
}

#[allow(clippy::large_enum_variant)]
pub enum EnqueueOutcome {
    Accepted,
    ReplacedPending {
        superseded_generation: u64,
        terminal: CandidateTerminal,
    },
    Backpressured {
        job: CompileJob,
    },
    Stopping {
        job: CompileJob,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateCancellation {
    Disable,
    SourceRemoval,
    Shutdown,
}

#[derive(Clone)]
struct InFlightJob {
    job: CompileJob,
    queue_duration: Duration,
    disposition: InFlightDisposition,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum InFlightDisposition {
    #[default]
    Active,
    Superseded,
    Cancelled(CandidateCancellation),
    Terminalized,
}

impl InFlightDisposition {
    fn supersede(&mut self) {
        if matches!(self, Self::Active) {
            *self = Self::Superseded;
        }
    }

    fn cancel(&mut self, reason: CandidateCancellation) {
        if !matches!(self, Self::Terminalized) {
            *self = Self::Cancelled(reason);
        }
    }

    fn terminalize(&mut self) -> bool {
        if matches!(self, Self::Terminalized) {
            false
        } else {
            *self = Self::Terminalized;
            true
        }
    }
}

struct WorkerState {
    pending_order: VecDeque<PackageId>,
    pending_by_package: BTreeMap<PackageId, CompileJob>,
    in_flight: Option<InFlightJob>,
    results: VecDeque<CandidateTerminal>,
    result_count: usize,
    events: VecDeque<WorkerEvent>,
    shutdown_terminals: Vec<CandidateTerminal>,
    stopping: bool,
    backpressure_count: u64,
    pending_superseded_count: u64,
    compiled_superseded_count: u64,
    cancelled_count: u64,
}

struct WorkerShared {
    state: Mutex<WorkerState>,
    build_session: SharedPackageBuildSession,
    job_available: Condvar,
    result_space_available: Condvar,
    queue_capacity: usize,
    result_capacity: usize,
    #[cfg(test)]
    test_control: WorkerTestControl,
}

pub(crate) struct DevelopmentWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(test)]
const WORKER_TEST_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WorkerTestControl {
    shared: Arc<WorkerTestControlShared>,
}

#[cfg(test)]
struct WorkerTestControlShared {
    state: Mutex<WorkerTestControlState>,
    changed: Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct WorkerTestControlState {
    targets: VecDeque<(PackageId, u64)>,
    blocked: Option<(PackageId, u64)>,
    released: bool,
}

#[cfg(test)]
#[allow(dead_code)]
impl WorkerTestControl {
    fn new() -> Self {
        Self {
            shared: Arc::new(WorkerTestControlShared {
                state: Mutex::new(WorkerTestControlState::default()),
                changed: Condvar::new(),
            }),
        }
    }

    pub(crate) fn block_before_compile(&self, package_id: PackageId, generation: u64) {
        self.block_before_compile_sequence([(package_id, generation)]);
    }

    pub(crate) fn block_before_compile_sequence(
        &self,
        targets: impl IntoIterator<Item = (PackageId, u64)>,
    ) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.targets = targets.into_iter().collect();
        state.blocked = None;
        state.released = false;
        self.shared.changed.notify_all();
    }

    pub(crate) fn wait_until_blocked(&self, timeout: Duration) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, timeout, |state| state.blocked.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.blocked.is_some()
    }

    pub(crate) fn wait_until_blocked_for(
        &self,
        package_id: &PackageId,
        generation: u64,
        timeout: Duration,
    ) -> bool {
        let target = (package_id.clone(), generation);
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.blocked.as_ref() != Some(&target)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.blocked.as_ref() == Some(&target)
    }

    pub(crate) fn release(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.released = true;
        self.shared.changed.notify_all();
    }

    fn release_for_shutdown(&self) {
        self.release();
    }

    fn before_compile(&self, package_id: &PackageId, generation: u64) {
        let target = (package_id.clone(), generation);
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.targets.front() != Some(&target) {
            return;
        }
        state.blocked = Some(target.clone());
        self.shared.changed.notify_all();
        let (mut state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, WORKER_TEST_CONTROL_TIMEOUT, |state| !state.released)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.targets.front() == Some(&target) {
            state.targets.pop_front();
        }
        state.blocked = None;
        state.released = false;
        self.shared.changed.notify_all();
    }
}

pub struct DevelopmentCompiler {
    worker: DevelopmentWorker,
}

#[derive(Clone)]
pub struct DevelopmentCompileRequest {
    pub source_id: SourceId,
    pub identity: CandidateIdentity,
    pub build_input: Arc<nexa_analysis::ResolvedBuildInput>,
    pub idl: nexa::Idl,
    pub required_exports: Vec<ExportRequirement>,
}

pub(crate) struct WorkerDrain {
    pub events: Vec<WorkerEvent>,
    pub terminals: Vec<CandidateTerminal>,
}

impl DevelopmentCompiler {
    pub fn start(config: &DevelopmentConfig) -> Result<Self, &'static str> {
        DevelopmentWorker::start(config)
            .map(|worker| Self { worker })
            .ok_or("development compiler is disabled")
    }

    #[must_use]
    pub fn submit(&self, request: DevelopmentCompileRequest) -> EnqueueOutcome {
        self.worker.enqueue(CompileJob::from_request(request))
    }

    #[must_use]
    pub fn retry(&self, job: CompileJob) -> EnqueueOutcome {
        self.worker.enqueue(job)
    }

    #[must_use]
    pub fn cancel(
        &self,
        package_id: &PackageId,
        reason: CandidateCancellation,
    ) -> Vec<CandidateTerminal> {
        self.worker.cancel_package(package_id, reason)
    }

    #[must_use]
    pub fn poll(&self) -> Vec<CandidateTerminal> {
        self.worker.drain_terminals()
    }

    #[must_use]
    pub fn poll_events(&self) -> Vec<WorkerEvent> {
        self.worker.drain_events()
    }

    #[must_use]
    pub fn inspection(&self) -> WorkerInspection {
        self.worker.inspection()
    }

    pub fn shutdown(&mut self) -> Vec<CandidateTerminal> {
        self.worker.shutdown().terminals
    }
}

impl DevelopmentWorker {
    #[must_use]
    pub fn start(config: &DevelopmentConfig) -> Option<Self> {
        Self::start_with_session(
            config,
            Arc::new(Mutex::new(nexa::PackageBuildSession::new())),
        )
    }

    #[must_use]
    pub(crate) fn start_with_session(
        config: &DevelopmentConfig,
        build_session: SharedPackageBuildSession,
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState {
                pending_order: VecDeque::new(),
                pending_by_package: BTreeMap::new(),
                in_flight: None,
                results: VecDeque::new(),
                result_count: 0,
                events: VecDeque::new(),
                shutdown_terminals: Vec::new(),
                stopping: false,
                backpressure_count: 0,
                pending_superseded_count: 0,
                compiled_superseded_count: 0,
                cancelled_count: 0,
            }),
            build_session,
            job_available: Condvar::new(),
            result_space_available: Condvar::new(),
            queue_capacity: config.compile_queue_capacity.max(1),
            result_capacity: config.result_queue_capacity.max(1),
            #[cfg(test)]
            test_control: WorkerTestControl::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("nexa-package-compiler".into())
            .spawn(move || worker_loop(&worker_shared))
            .expect("the Nexa development worker thread must be creatable");
        Some(Self {
            shared,
            thread: Some(thread),
        })
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn test_control(&self) -> WorkerTestControl {
        self.shared.test_control.clone()
    }

    pub fn enqueue(&self, job: CompileJob) -> EnqueueOutcome {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping {
            return EnqueueOutcome::Stopping { job };
        }
        let package_id = job.identity.package_id.clone();
        let mut completed_superseded = 0_u64;
        for terminal in &mut state.results {
            if terminal.data().identity.package_id == package_id
                && terminal.data().identity.generation < job.identity.generation
                && matches!(
                    terminal,
                    CandidateTerminal::Compiled { .. }
                        | CandidateTerminal::CompileFailed { .. }
                        | CandidateTerminal::VerifyFailed { .. }
                )
            {
                let data = terminal.data().clone();
                *terminal = CandidateTerminal::SupersededAfterCompile(data);
                completed_superseded = completed_superseded.saturating_add(1);
            }
        }
        state.compiled_superseded_count = state
            .compiled_superseded_count
            .saturating_add(completed_superseded);
        if let Some(old) = state.pending_by_package.get_mut(&package_id) {
            let old = std::mem::replace(old, job);
            state.pending_superseded_count = state.pending_superseded_count.saturating_add(1);
            let superseded_generation = old.identity.generation;
            let terminal = CandidateTerminal::SupersededBeforeCompile(
                old.terminal_data(old.queued_at.elapsed(), Duration::ZERO),
            );
            self.shared.job_available.notify_one();
            return EnqueueOutcome::ReplacedPending {
                superseded_generation,
                terminal,
            };
        }
        if state.pending_by_package.len() >= self.shared.queue_capacity {
            state.backpressure_count = state.backpressure_count.saturating_add(1);
            return EnqueueOutcome::Backpressured { job };
        }
        state.pending_by_package.insert(package_id.clone(), job);
        state.pending_order.push_back(package_id);
        self.shared.job_available.notify_one();
        EnqueueOutcome::Accepted
    }

    pub fn cancel_package(
        &self,
        package_id: &PackageId,
        reason: CandidateCancellation,
    ) -> Vec<CandidateTerminal> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut terminals = Vec::new();
        if let Some(job) = state.pending_by_package.remove(package_id) {
            state.pending_order.retain(|queued| queued != package_id);
            state.cancelled_count = state.cancelled_count.saturating_add(1);
            terminals.push(cancelled_terminal(
                reason,
                job.terminal_data(job.queued_at.elapsed(), Duration::ZERO),
            ));
        }
        let in_flight_terminal = state.in_flight.as_mut().and_then(|in_flight| {
            (in_flight.job.identity.package_id == *package_id
                && in_flight.disposition.terminalize())
            .then(|| {
                cancelled_terminal(
                    reason,
                    in_flight
                        .job
                        .terminal_data(in_flight.queue_duration, Duration::ZERO),
                )
            })
        });
        if let Some(terminal) = in_flight_terminal {
            state.cancelled_count = state.cancelled_count.saturating_add(1);
            terminals.push(terminal);
        }
        let mut converted = 0_u64;
        let completed = std::mem::take(&mut state.results);
        state.result_count = 0;
        for terminal in completed {
            if terminal.data().identity.package_id == *package_id {
                let data = terminal.data().clone();
                terminals.push(cancelled_terminal(reason, data));
                converted = converted.saturating_add(1);
            } else {
                state.results.push_back(terminal);
                state.result_count = state.result_count.saturating_add(1);
            }
        }
        state.cancelled_count = state.cancelled_count.saturating_add(converted);
        if !terminals.is_empty() {
            self.shared.result_space_available.notify_all();
        }
        terminals
    }

    pub fn supersede_package_except(
        &self,
        package_id: &PackageId,
        desired_build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    ) -> Vec<CandidateTerminal> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut terminals = Vec::new();
        if state
            .pending_by_package
            .get(package_id)
            .is_some_and(|job| Some(job.identity.build_fingerprint) != desired_build_fingerprint)
        {
            let job = state
                .pending_by_package
                .remove(package_id)
                .expect("the stale pending Job was observed while holding the Worker lock");
            state.pending_order.retain(|queued| queued != package_id);
            state.pending_superseded_count = state.pending_superseded_count.saturating_add(1);
            terminals.push(job.supersede_before_compile());
        }
        if let Some(in_flight) = state.in_flight.as_mut()
            && in_flight.job.identity.package_id == *package_id
            && Some(in_flight.job.identity.build_fingerprint) != desired_build_fingerprint
        {
            in_flight.disposition.supersede();
        }
        let mut converted = 0_u64;
        for terminal in &mut state.results {
            if terminal.data().identity.package_id == *package_id
                && Some(terminal.data().identity.build_fingerprint) != desired_build_fingerprint
                && matches!(
                    terminal,
                    CandidateTerminal::Compiled { .. }
                        | CandidateTerminal::CompileFailed { .. }
                        | CandidateTerminal::VerifyFailed { .. }
                )
            {
                let data = terminal.data().clone();
                *terminal = CandidateTerminal::SupersededAfterCompile(data);
                converted = converted.saturating_add(1);
            }
        }
        state.compiled_superseded_count = state.compiled_superseded_count.saturating_add(converted);
        terminals
    }

    pub fn drain(&self) -> WorkerDrain {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let terminals = state.results.drain(..).collect::<Vec<_>>();
        state.result_count = 0;
        let events = state.events.drain(..).collect();
        self.shared.result_space_available.notify_all();
        WorkerDrain { events, terminals }
    }

    fn drain_terminals(&self) -> Vec<CandidateTerminal> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let terminals = state.results.drain(..).collect();
        state.result_count = 0;
        self.shared.result_space_available.notify_all();
        terminals
    }

    fn drain_events(&self) -> Vec<WorkerEvent> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .drain(..)
            .collect()
    }

    #[must_use]
    pub fn inspection(&self) -> WorkerInspection {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        WorkerInspection {
            queued_packages: state.pending_by_package.len(),
            in_flight_package: state
                .in_flight
                .as_ref()
                .map(|in_flight| in_flight.job.identity.package_id.clone()),
            completed_results: state.result_count,
            backpressure_count: state.backpressure_count,
            pending_superseded_count: state.pending_superseded_count,
            compiled_superseded_count: state.compiled_superseded_count,
            cancelled_count: state.cancelled_count,
        }
    }

    pub fn shutdown(&mut self) -> WorkerDrain {
        let mut immediate = Vec::new();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopping = true;
            let pending = std::mem::take(&mut state.pending_by_package);
            state.pending_order.clear();
            for (_, job) in pending {
                state.cancelled_count = state.cancelled_count.saturating_add(1);
                immediate.push(CandidateTerminal::CancelledByShutdown(
                    job.terminal_data(job.queued_at.elapsed(), Duration::ZERO),
                ));
            }
            if let Some(in_flight) = state.in_flight.as_mut() {
                in_flight
                    .disposition
                    .cancel(CandidateCancellation::Shutdown);
            }
            immediate.extend(state.results.drain(..));
            state.result_count = 0;
            self.shared.job_available.notify_all();
            self.shared.result_space_available.notify_all();
        }
        #[cfg(test)]
        self.shared.test_control.release_for_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        immediate.extend(state.results.drain(..));
        immediate.append(&mut state.shutdown_terminals);
        state.result_count = 0;
        let events = state.events.drain(..).collect();
        WorkerDrain {
            events,
            terminals: immediate,
        }
    }
}

impl Drop for DevelopmentWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[allow(clippy::too_many_lines)]
fn worker_loop(shared: &WorkerShared) {
    loop {
        let job = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.pending_order.is_empty() && !state.stopping {
                state = shared
                    .job_available
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.stopping {
                return;
            }
            let package_id = state
                .pending_order
                .pop_front()
                .expect("pending order is non-empty");
            let job = state
                .pending_by_package
                .remove(&package_id)
                .expect("pending order and map remain consistent");
            let queue_duration = job.queued_at.elapsed();
            state.events.push_back(WorkerEvent::CompileStarted {
                source_id: job.source_id.clone(),
                identity: job.identity.clone(),
                queue_duration,
            });
            state.in_flight = Some(InFlightJob {
                job: job.clone(),
                queue_duration,
                disposition: InFlightDisposition::Active,
            });
            job
        };

        #[cfg(test)]
        shared
            .test_control
            .before_compile(&job.identity.package_id, job.identity.generation);

        let work_started = Instant::now();
        let host_contract = nexa::HostContractInput::with_source(
            &job.idl,
            job.host_contract_source_identity.clone(),
            Arc::clone(&job.host_contract_source),
        )
        .expect("compile jobs retain a validated immutable Host source");
        let result = {
            let mut build_session = shared
                .build_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            compile_package_candidate(
                &mut build_session,
                &host_contract,
                &job.required_exports,
                &job.source_id,
                job.identity.clone(),
                &job.build_input,
            )
        };
        let work_duration = work_started.elapsed();

        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.result_count >= shared.result_capacity
            && state
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| in_flight.disposition != InFlightDisposition::Terminalized)
            && !state.stopping
        {
            state = shared
                .result_space_available
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let in_flight = state
            .in_flight
            .take()
            .expect("the Worker owns exactly one in-flight Job");
        let data = job.terminal_data(in_flight.queue_duration, work_duration);
        if state.stopping {
            if in_flight.disposition != InFlightDisposition::Terminalized {
                state.cancelled_count = state.cancelled_count.saturating_add(1);
                state
                    .shutdown_terminals
                    .push(CandidateTerminal::CancelledByShutdown(data));
            }
            return;
        }
        let terminal = match in_flight.disposition {
            InFlightDisposition::Terminalized => continue,
            InFlightDisposition::Cancelled(reason) => {
                state.cancelled_count = state.cancelled_count.saturating_add(1);
                cancelled_terminal(reason, data)
            }
            InFlightDisposition::Superseded => {
                state.compiled_superseded_count = state.compiled_superseded_count.saturating_add(1);
                CandidateTerminal::SupersededAfterCompile(data)
            }
            InFlightDisposition::Active
                if state
                    .pending_by_package
                    .get(&job.identity.package_id)
                    .is_some_and(|pending| {
                        pending.identity.generation > job.identity.generation
                    }) =>
            {
                state.compiled_superseded_count = state.compiled_superseded_count.saturating_add(1);
                CandidateTerminal::SupersededAfterCompile(data)
            }
            InFlightDisposition::Active => match result {
                Ok(compilation) => CandidateTerminal::Compiled {
                    data,
                    build_input: job.build_input,
                    compilation,
                },
                Err(failure) if failure.diagnostic.stage == EngineDiagnosticStage::Verify => {
                    CandidateTerminal::VerifyFailed {
                        data,
                        diagnostic: failure.diagnostic,
                        additional_diagnostics: failure.additional_diagnostics,
                        compile_duration: failure.compile_duration,
                        verify_duration: failure.verify_duration,
                    }
                }
                Err(failure) => CandidateTerminal::CompileFailed {
                    data,
                    diagnostic: failure.diagnostic,
                    additional_diagnostics: failure.additional_diagnostics,
                    compile_duration: failure.compile_duration,
                    verify_duration: failure.verify_duration,
                },
            },
        };
        state.results.push_back(terminal);
        state.result_count = state.result_count.saturating_add(1);
    }
}

fn cancelled_terminal(
    reason: CandidateCancellation,
    data: CandidateTerminalData,
) -> CandidateTerminal {
    match reason {
        CandidateCancellation::Disable => CandidateTerminal::CancelledByDisable(data),
        CandidateCancellation::SourceRemoval => CandidateTerminal::CancelledBySourceRemoval(data),
        CandidateCancellation::Shutdown => CandidateTerminal::CancelledByShutdown(data),
    }
}
