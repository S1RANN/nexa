use crate::capability::CapabilitySet;
use crate::lifecycle::{PackageLifecycle, PackageStatus};
use crate::manifest::{
    EffectiveApplicationSettings, EntitlementId, PackageId, PackageVersion, SourceId,
};
use crate::policy::{ActivationPolicy, PackagePolicy, TrustLevel};
use crate::source::PackageCandidate;
use crate::{EngineDiagnosticSummary, LastKnownGood};
use std::sync::Arc;

pub(crate) struct PackageRuntime {
    pub realm: nexa::prelude::RealmRuntime,
    pub module: nexa::prelude::ModuleHandle,
    pub root_scope: nexa::prelude::ScopeHandle,
    pub artifact: crate::CompiledPackageArtifact,
    /// Contract-slot-indexed call plans, fully resolved and verified once
    /// when the package is admitted.
    pub entrypoints: Vec<Option<nexa::PreparedScriptExport>>,
}

pub(crate) struct PackageRecord {
    pub source_id: SourceId,
    pub policy: PackagePolicy,
    pub effective: EffectiveApplicationSettings,
    pub candidate: PackageCandidate,
    pub build_input: Arc<nexa_analysis::ResolvedBuildInput>,
    pub lifecycle: PackageLifecycle,
    pub runtime: Option<PackageRuntime>,
    pub last_diagnostic: Option<EngineDiagnosticSummary>,
    pub last_known_good: Option<LastKnownGood>,
    pub development: crate::development::PackageDevelopment,
    pub awaiting_job: Option<crate::CompileJob>,
    pub handler_calls_this_tick: u64,
    pub handler_instructions_this_tick: u64,
    pub fuel_used_this_tick: u64,
    pub outputs_this_tick: u64,
    pub ready_candidate: Option<crate::development::ReadyCandidate>,
    pub ready_commit_requested: bool,
}

impl PackageRecord {
    #[must_use]
    pub fn info(&self) -> PackageInfo {
        PackageInfo {
            id: self.candidate.manifest.id.clone(),
            source_id: self.source_id.clone(),
            name: self.candidate.manifest.name.clone(),
            version: self.candidate.manifest.version.clone(),
            trust: self.policy.trust,
            capabilities: self.effective.capabilities.clone(),
            activation: self.effective.activation,
            entitlement: self.effective.entitlement.clone(),
            status: self.lifecycle.status(),
            priority: self.effective.priority,
            last_diagnostic: self.last_diagnostic.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageInfo {
    pub id: PackageId,
    pub source_id: SourceId,
    pub name: String,
    pub version: PackageVersion,
    pub trust: TrustLevel,
    pub capabilities: CapabilitySet,
    pub activation: ActivationPolicy,
    pub entitlement: Option<EntitlementId>,
    pub status: PackageStatus,
    pub priority: i32,
    pub last_diagnostic: Option<EngineDiagnosticSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PackageOutput<T> {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub trust: TrustLevel,
    pub capabilities: CapabilitySet,
    pub value: T,
}

/// Aggregate runtime ownership visible to embedding health and stress checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EngineHealth {
    pub enabled_packages: usize,
    pub tasks: u64,
    pub scopes: u64,
    pub continuations: u64,
    pub scheduler_tokens: u64,
    pub requests: u64,
    pub completion_reservations: u64,
    pub tokens: u64,
    pub snapshots: u64,
    pub release_reservations: u64,
    pub queued_releases: u64,
    pub heap_objects: u64,
    pub state_objects: u64,
    pub retired_modules: u64,
    pub host_pending_completions: usize,
    pub host_pending_releases: usize,
    pub delivered_release_records: u64,
}
