use std::time::Duration;

use crate::{
    CapabilitySet, EngineDiagnostic, EngineDiagnosticSummary, EngineHealth, PackageId,
    PackageStatus, PackageVersion, SourceHash, SourceId,
};

#[derive(Clone, Debug, Default)]
pub struct EngineTickReport {
    pub development_events: Vec<crate::DevelopmentEvent>,
    pub diagnostics: Vec<EngineDiagnostic>,
    pub reloads: Vec<ReloadReport>,
    pub faulted_packages: Vec<PackageId>,
    pub released_resources: usize,
}

#[derive(Clone, Debug)]
pub struct EngineInspection {
    pub health: EngineHealth,
    pub packages: Vec<PackageInspection>,
    pub development: DevelopmentInspection,
    pub recent_diagnostics: Vec<EngineDiagnosticSummary>,
    pub recent_reloads: Vec<ReloadReportSummary>,
    pub dropped_diagnostics: u64,
    pub dropped_events: u64,
}

#[derive(Clone, Debug, Default)]
pub struct DevelopmentInspection {
    pub enabled: bool,
    pub worker_running: bool,
    pub queued_candidates: usize,
    pub retained_events: usize,
    pub generations_without_terminal: u64,
    pub worker: crate::WorkerInspection,
}

#[derive(Clone, Debug)]
pub struct PackageInspection {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub status: PackageStatus,
    pub version: PackageVersion,
    pub effective_capabilities: CapabilitySet,
    pub active_epoch: Option<u64>,
    pub source_hash: SourceHash,
    pub candidate_generation: u64,
    pub tasks: u64,
    pub waiting_requests: u64,
    pub host_resources: u64,
    pub handler_calls_this_tick: u64,
    pub handler_instructions_this_tick: u64,
    pub fuel_used_this_tick: u64,
    pub last_compile_duration: Option<Duration>,
    pub last_reload_duration: Option<Duration>,
    pub recent_diagnostic: Option<EngineDiagnosticSummary>,
    pub recent_metrics: Vec<PackageMetric>,
}

#[derive(Clone, Debug, Default)]
pub struct PackageMetric {
    pub tick: u64,
    pub discovery_duration: Duration,
    pub source_hash_duration: Duration,
    pub change_to_stable_duration: Duration,
    pub candidate_queue_duration: Duration,
    pub compile_duration: Duration,
    pub verify_duration: Duration,
    pub ready_to_commit_duration: Duration,
    pub quiesce_duration: Duration,
    pub reload_duration: Duration,
    pub migration_duration: Duration,
    pub commit_duration: Duration,
    pub activation_duration: Duration,
    pub total_change_to_visible_duration: Duration,
    pub handler_calls: u64,
    pub handler_instructions: u64,
    pub fuel_used: u64,
    pub output_count: u64,
    pub task_peak: u64,
    pub request_peak: u64,
}

#[derive(Clone, Debug)]
pub struct ReloadReport {
    pub package_id: PackageId,
    pub candidate_generation: u64,
    pub old_epoch: u64,
    pub new_epoch: Option<u64>,
    pub source_hash: SourceHash,
    pub change_to_stable_duration: Duration,
    pub queue_duration: Duration,
    pub compile_duration: Duration,
    pub verify_duration: Duration,
    pub ready_to_commit_duration: Duration,
    pub quiesce_duration: Duration,
    pub commit_duration: Duration,
    pub reload_duration: Duration,
    pub migration_duration: Duration,
    pub activation_duration: Duration,
    pub total_change_to_visible_duration: Duration,
    pub cancelled_tasks: usize,
    pub detached_requests: usize,
    pub outcome: ReloadReportOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadReportOutcome {
    Committed,
    CompileFailed,
    VerifyFailed,
    RolledBackBeforeCommit,
    ActivationFaulted,
    Superseded,
    HostRebuildRequired,
}

#[derive(Clone, Debug)]
pub struct ReloadReportSummary {
    pub package_id: PackageId,
    pub candidate_generation: u64,
    pub old_epoch: u64,
    pub new_epoch: Option<u64>,
    pub outcome: ReloadReportOutcome,
    pub total_duration: Duration,
}

impl ReloadReport {
    #[must_use]
    pub fn summary(&self) -> ReloadReportSummary {
        ReloadReportSummary {
            package_id: self.package_id.clone(),
            candidate_generation: self.candidate_generation,
            old_epoch: self.old_epoch,
            new_epoch: self.new_epoch,
            outcome: self.outcome,
            total_duration: self.total_change_to_visible_duration,
        }
    }

    #[must_use]
    pub const fn committed(&self) -> bool {
        matches!(self.outcome, ReloadReportOutcome::Committed)
    }
}
