use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::artifact::{CandidateCompilation, compile_package_candidate};
use crate::contract::ExportRequirement;
use crate::{
    CompiledPackageArtifact, EngineDiagnostic, PackageCandidate, PackageId, ReloadReportSummary,
    SourceHash, SourceId,
};

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
}

#[derive(Clone, Debug)]
pub struct DevelopmentEventData {
    pub package_id: PackageId,
    pub candidate_generation: u64,
    pub source_hash: SourceHash,
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
            | Self::HostRebuildRequired(data) => data,
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
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PackageDevelopment {
    pub state: DevelopmentState,
    pub observed_hash: Option<SourceHash>,
    pub last_processed_hash: Option<SourceHash>,
    pub stable_scans: u8,
    pub latest_generation: u64,
    pub last_compile_duration: Option<Duration>,
    pub last_reload_duration: Option<Duration>,
    pub last_discovery_duration: Duration,
    pub last_source_hash_duration: Duration,
    pub last_queue_duration: Duration,
    pub last_verify_duration: Duration,
    pub last_migration_duration: Duration,
    pub last_activation_duration: Duration,
    pub recent_metrics: VecDeque<crate::PackageMetric>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReadyCandidate {
    pub candidate: PackageCandidate,
    pub compilation: CandidateCompilation,
    pub generation: u64,
}

#[derive(Clone)]
pub(crate) struct CompileJob {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub generation: u64,
    pub source_hash: SourceHash,
    pub candidate: PackageCandidate,
    pub idl: nexa_idl::Idl,
    pub required_exports: Vec<ExportRequirement>,
    pub queued_at: Instant,
}

pub(crate) struct CompileResult {
    pub package_id: PackageId,
    pub generation: u64,
    pub source_hash: SourceHash,
    pub candidate: PackageCandidate,
    pub queue_duration: Duration,
    pub work_duration: Duration,
    pub result: Result<CandidateCompilation, EngineDiagnostic>,
}

struct WorkerState {
    pending: VecDeque<CompileJob>,
    results: VecDeque<CompileResult>,
    stopping: bool,
}

struct WorkerShared {
    state: Mutex<WorkerState>,
    wake: Condvar,
    queue_capacity: usize,
    result_capacity: usize,
}

pub(crate) struct DevelopmentWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

pub struct DevelopmentCompiler {
    worker: DevelopmentWorker,
}

#[derive(Clone)]
pub struct DevelopmentCompileRequest {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub generation: u64,
    pub candidate: PackageCandidate,
    pub idl: nexa_idl::Idl,
    pub required_exports: Vec<ExportRequirement>,
}

pub struct DevelopmentCompileResult {
    pub package_id: PackageId,
    pub generation: u64,
    pub source_hash: SourceHash,
    pub candidate: PackageCandidate,
    pub queue_duration: Duration,
    pub work_duration: Duration,
    pub result: Result<CompiledPackageArtifact, EngineDiagnostic>,
}

impl DevelopmentCompiler {
    pub fn start(config: &DevelopmentConfig) -> Result<Self, &'static str> {
        DevelopmentWorker::start(config)
            .map(|worker| Self { worker })
            .ok_or("development compiler is disabled")
    }

    #[must_use]
    pub fn submit(&self, request: DevelopmentCompileRequest) -> Vec<(PackageId, u64, SourceHash)> {
        let source_hash = SourceHash(nexa_core::StableId::from_parts(&[
            &request.candidate.manifest_source,
            "\0",
            &request.candidate.entry_source,
        ]));
        self.worker.enqueue(CompileJob {
            package_id: request.package_id,
            source_id: request.source_id,
            generation: request.generation,
            source_hash,
            candidate: request.candidate,
            idl: request.idl,
            required_exports: request.required_exports,
            queued_at: Instant::now(),
        })
    }

    #[must_use]
    pub fn poll(&self) -> Vec<DevelopmentCompileResult> {
        self.worker
            .drain_results()
            .into_iter()
            .map(|result| DevelopmentCompileResult {
                package_id: result.package_id,
                generation: result.generation,
                source_hash: result.source_hash,
                candidate: result.candidate,
                queue_duration: result.queue_duration,
                work_duration: result.work_duration,
                result: result.result.map(|compilation| compilation.artifact),
            })
            .collect()
    }

    pub fn shutdown(&mut self) {
        self.worker.shutdown();
    }
}

impl DevelopmentWorker {
    #[must_use]
    pub fn start(config: &DevelopmentConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let shared = Arc::new(WorkerShared {
            state: Mutex::new(WorkerState {
                pending: VecDeque::new(),
                results: VecDeque::new(),
                stopping: false,
            }),
            wake: Condvar::new(),
            queue_capacity: config.compile_queue_capacity.max(1),
            result_capacity: config.result_queue_capacity.max(1),
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

    pub fn enqueue(&self, job: CompileJob) -> Vec<(PackageId, u64, SourceHash)> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping {
            return vec![(job.package_id, job.generation, job.source_hash)];
        }
        let mut superseded = Vec::new();
        let mut index = 0;
        while index < state.pending.len() {
            if state.pending[index].package_id == job.package_id {
                let old = state.pending.remove(index).expect("index is in bounds");
                superseded.push((old.package_id, old.generation, old.source_hash));
            } else {
                index += 1;
            }
        }
        while state.pending.len() >= self.shared.queue_capacity {
            if let Some(old) = state.pending.pop_front() {
                superseded.push((old.package_id, old.generation, old.source_hash));
            }
        }
        state.pending.push_back(job);
        self.shared.wake.notify_one();
        superseded
    }

    pub fn drain_results(&self) -> Vec<CompileResult> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.results.drain(..).collect()
    }

    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .len()
    }

    pub fn shutdown(&mut self) {
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopping = true;
            state.pending.clear();
            self.shared.wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DevelopmentWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(shared: &WorkerShared) {
    loop {
        let job = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.pending.is_empty() && !state.stopping {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if state.stopping {
                return;
            }
            state.pending.pop_front().expect("queue is non-empty")
        };
        let queue_duration = job.queued_at.elapsed();
        let work_started = Instant::now();
        let result = compile_package_candidate(
            &job.idl,
            &job.required_exports,
            &job.source_id,
            &job.candidate,
        );
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping {
            return;
        }
        while state.results.len() >= shared.result_capacity {
            state.results.pop_front();
        }
        state.results.push_back(CompileResult {
            package_id: job.package_id,
            generation: job.generation,
            source_hash: job.source_hash,
            candidate: job.candidate,
            queue_duration,
            work_duration: work_started.elapsed(),
            result,
        });
    }
}
