use crate::capability::CapabilitySet;
use crate::lifecycle::{PackageLifecycle, PackageStatus};
use crate::manifest::{EntitlementId, PackageId, PackageVersion, SourceId};
use crate::policy::{ActivationPolicy, PackagePolicy, TrustLevel};
use crate::source::PackageCandidate;

pub(crate) struct PackageRuntime {
    pub realm: nexa_runtime::RealmRuntime,
    pub module: nexa_runtime::ModuleHandle,
    pub root_scope: nexa_runtime::ScopeHandle,
}

pub(crate) struct PackageRecord {
    pub source_id: SourceId,
    pub policy: PackagePolicy,
    pub candidate: PackageCandidate,
    pub lifecycle: PackageLifecycle,
    pub runtime: Option<PackageRuntime>,
    pub last_error: Option<String>,
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
            capabilities: self.candidate.manifest.capabilities.clone(),
            activation: self.candidate.manifest.activation,
            entitlement: self.candidate.manifest.entitlement.clone(),
            status: self.lifecycle.status(),
            priority: self.candidate.manifest.priority,
            last_error: self.last_error.clone(),
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
    pub last_error: Option<String>,
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
pub struct EmbedHealth {
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
