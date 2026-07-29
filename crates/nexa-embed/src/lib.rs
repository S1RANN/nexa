//! High-level, package-oriented embedding API for trusted Nexa content.

mod artifact;
mod builder;
mod capability;
mod contract;
mod development;
mod diagnostic;
mod directory_source;
mod dispatch;
mod entitlement;
mod inspection;
mod lifecycle;
mod manifest;
mod memory_source;
mod package;
mod persistence;
mod policy;
mod source;
mod source_file;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;

pub use artifact::{
    CompiledPackageArtifact, FunctionDebugInfo, LastKnownGood, ManifestHash, ModuleDebugInfo,
    SourceHash, compile_package,
};
pub use builder::NexaEngineBuilder;
pub use capability::CapabilitySet;
pub use contract::{ExportRequirement, HostContract};
pub use development::{
    DevelopmentCompileRequest, DevelopmentCompileResult, DevelopmentCompiler, DevelopmentConfig,
    DevelopmentEvent, DevelopmentEventData, DevelopmentState,
};
pub use diagnostic::{
    DiagnosticRenderer, EngineDiagnostic, EngineDiagnosticContext, EngineDiagnosticStage,
    EngineDiagnosticSummary, EngineTaskId, RelatedDiagnostic,
};
pub use directory_source::DirectorySource;
pub use entitlement::{EntitlementResolver, NoEntitlements, StaticEntitlements};
pub use inspection::{
    DevelopmentInspection, EngineInspection, EngineTickReport, PackageInspection, PackageMetric,
    ReloadReport, ReloadReportOutcome, ReloadReportSummary,
};
pub use lifecycle::{LifecycleError, PackageLifecycle, PackageStatus};
pub use manifest::{
    CapabilityId, EntitlementId, ManifestError, PackageId, PackageManifest, PackagePath,
    PackageVersion, SourceId,
};
pub use memory_source::MemorySource;
pub use package::{EngineHealth, PackageInfo, PackageOutput};
pub use policy::{
    ActivationPolicy, ActivationSet, PackagePolicy, PackageRuntimeLimits, TrustLevel,
};
pub use source::{PackageCandidate, PackageSource, PackageSourceError};
pub use source_file::{
    SourceFile, SourceFileRegistry, SourceFileRegistryError, SourcePosition, SourceRange,
};

use development::{CompileJob, DevelopmentWorker};
use diagnostic::BoundedDiagnosticLog;
use package::{PackageRecord, PackageRuntime};

#[derive(Clone, Debug)]
pub struct PackageContext {
    pub package_id: PackageId,
    pub source_id: SourceId,
    pub trust: TrustLevel,
    pub capabilities: CapabilitySet,
    pub data_namespace: String,
    pub version: PackageVersion,
}

pub trait HostRegistryFactory {
    fn create(&self, context: &PackageContext) -> Box<dyn nexa_runtime::HostRegistry>;
}

impl<F> HostRegistryFactory for F
where
    F: for<'a> Fn(&'a PackageContext) -> Box<dyn nexa_runtime::HostRegistry>,
{
    fn create(&self, context: &PackageContext) -> Box<dyn nexa_runtime::HostRegistry> {
        self(context)
    }
}

pub struct NexaEngine {
    contract: HostContract,
    idl: nexa_idl::Idl,
    host_factory: Box<dyn HostRegistryFactory>,
    sources: Vec<Box<dyn PackageSource>>,
    entitlements: Box<dyn EntitlementResolver>,
    storage_dir: Option<PathBuf>,
    runtime_host: nexa_runtime::RuntimeHost,
    packages: Vec<PackageRecord>,
    diagnostics: BoundedDiagnosticLog,
    required_exports: Vec<ExportRequirement>,
    persisted: BTreeMap<PackageId, bool>,
    development: DevelopmentConfig,
    development_worker: Option<DevelopmentWorker>,
    development_events: VecDeque<DevelopmentEvent>,
    pending_development_events: Vec<DevelopmentEvent>,
    reload_reports: VecDeque<ReloadReport>,
    pending_reload_reports: Vec<ReloadReport>,
    dropped_events: u64,
    last_tick_diagnostic_sequence: u64,
    ticks: u64,
    next_realm_id: u32,
    delivered_release_records: u64,
    shutdown: bool,
}

impl NexaEngine {
    #[must_use]
    pub fn builder(contract: HostContract) -> NexaEngineBuilder {
        NexaEngineBuilder::new(contract)
    }

    pub(crate) fn from_builder(builder: NexaEngineBuilder) -> Result<Self, EngineError> {
        let idl = nexa_idl::parse(builder.contract.canonical_idl)
            .map_err(|error| EngineError::Contract(error.to_string()))?;
        if nexa_idl::exact_hash(&idl) != builder.contract.interface_hash {
            return Err(EngineError::Contract(
                "generated interface hash mismatch".into(),
            ));
        }
        let persisted = builder
            .storage_dir
            .as_deref()
            .map(persistence::load)
            .transpose()
            .map_err(|error| EngineError::Persistence(error.to_string()))?
            .unwrap_or_default();
        let development_worker = DevelopmentWorker::start(&builder.development);
        Ok(Self {
            contract: builder.contract,
            idl,
            host_factory: builder
                .host_factory
                .ok_or(EngineError::MissingHostFactory)?,
            sources: builder.sources,
            entitlements: builder.entitlements,
            storage_dir: builder.storage_dir,
            runtime_host: nexa_runtime::RuntimeHost::new(builder.runtime_host_capacity),
            packages: Vec::new(),
            diagnostics: BoundedDiagnosticLog::default(),
            required_exports: builder.required_exports,
            persisted,
            development: builder.development,
            development_worker,
            development_events: VecDeque::new(),
            pending_development_events: Vec::new(),
            reload_reports: VecDeque::new(),
            pending_reload_reports: Vec::new(),
            dropped_events: 0,
            last_tick_diagnostic_sequence: 0,
            ticks: 0,
            next_realm_id: 1,
            delivered_release_records: 0,
            shutdown: false,
        })
    }

    pub fn discover(&mut self) -> Result<Vec<PackageInfo>, EngineError> {
        self.require_open()?;
        let mut records = Vec::new();
        for source in &self.sources {
            let source_id = source.id().clone();
            let candidates = match source.discover() {
                Ok(candidates) => candidates,
                Err(error) => {
                    let message = error.to_string();
                    self.diagnostics.push(EngineDiagnostic::without_source(
                        None,
                        Some(source_id.clone()),
                        EngineDiagnosticStage::SourceDiscovery,
                        nexa::ErrorCode::NX7001,
                        &message,
                    ));
                    return Err(EngineError::Source {
                        source: source_id,
                        message,
                    });
                }
            };
            for candidate in candidates {
                let mut lifecycle = PackageLifecycle::discovered();
                let locked = candidate
                    .manifest
                    .entitlement
                    .as_ref()
                    .is_some_and(|id| !self.entitlements.contains(id));
                lifecycle.transition(if locked {
                    PackageStatus::Locked
                } else {
                    PackageStatus::Disabled
                })?;
                records.push(PackageRecord {
                    source_id: source.id().clone(),
                    policy: source.policy().clone(),
                    candidate,
                    lifecycle,
                    runtime: None,
                    last_diagnostic: None,
                    last_known_good: None,
                    development: development::PackageDevelopment::default(),
                    handler_calls_this_tick: 0,
                    fuel_used_this_tick: 0,
                    outputs_this_tick: 0,
                    ready_candidate: None,
                });
            }
        }
        let mut counts = BTreeMap::<PackageId, usize>::new();
        for record in &records {
            *counts
                .entry(record.candidate.manifest.id.clone())
                .or_default() += 1;
        }
        for record in &mut records {
            if counts
                .get(&record.candidate.manifest.id)
                .is_some_and(|count| *count > 1)
            {
                record.lifecycle.transition(PackageStatus::Incompatible)?;
                record.last_diagnostic = Some(
                    EngineDiagnostic::without_source(
                        Some(record.candidate.manifest.id.clone()),
                        Some(record.source_id.clone()),
                        EngineDiagnosticStage::Manifest,
                        nexa::ErrorCode::NX7002,
                        "duplicate package id",
                    )
                    .summary(),
                );
            }
        }
        self.packages = records;
        Ok(self.packages())
    }

    pub fn enable_defaults(&mut self) -> Result<(), EngineError> {
        let ids = self
            .packages
            .iter()
            .filter(|record| {
                let manifest = &record.candidate.manifest;
                manifest.activation == ActivationPolicy::Required
                    || (manifest.activation == ActivationPolicy::DefaultEnabled
                        && self.persisted.get(&manifest.id).copied().unwrap_or(true))
                    || self.persisted.get(&manifest.id).copied() == Some(true)
            })
            .map(|record| record.candidate.manifest.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if self.status(&id) != Some(PackageStatus::Locked) {
                let _ = self.enable(&id);
            }
        }
        Ok(())
    }

    pub fn enable(&mut self, id: &PackageId) -> Result<(), EngineError> {
        self.require_open()?;
        let index = self.unique_index(id)?;
        match self.packages[index].lifecycle.status() {
            PackageStatus::Enabled => return Ok(()),
            PackageStatus::Locked => return Err(EngineError::Locked(id.clone())),
            PackageStatus::Incompatible => return Err(EngineError::Incompatible(id.clone())),
            PackageStatus::Discovered | PackageStatus::Disabled | PackageStatus::Faulted => {}
            status => return Err(EngineError::InvalidState(id.clone(), status)),
        }
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Enabling)?;
        match self.fresh_candidate(index) {
            Ok(candidate) => self.packages[index].candidate = candidate,
            Err(error) => {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Faulted)?;
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                return Err(error);
            }
        }
        let result = self.build_runtime(index);
        match result {
            Ok(runtime) => {
                let artifact = runtime.artifact.clone();
                let epoch = runtime
                    .realm
                    .active_module_epoch(runtime.module)
                    .unwrap_or_default();
                let manifest = &self.packages[index].candidate.manifest;
                self.packages[index].last_known_good = Some(LastKnownGood {
                    source_hash: artifact.source_hash,
                    state_schema_hash: nexa_core::StableId::from_parts(&[
                        manifest.id.as_str(),
                        "::",
                        &manifest.state_schema,
                    ]),
                    host_interface_hash: self.contract.interface_hash,
                    artifact,
                    epoch,
                    committed_generation: 0,
                });
                self.packages[index].runtime = Some(runtime);
                self.packages[index].last_diagnostic = None;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                self.persisted.insert(id.clone(), true);
                self.persist_selections()?;
                Ok(())
            }
            Err(error) => {
                self.packages[index].runtime = None;
                let next = if matches!(
                    error,
                    EngineError::MissingExport(_, _) | EngineError::ExportSignature(_, _)
                ) {
                    PackageStatus::Incompatible
                } else {
                    PackageStatus::Faulted
                };
                self.packages[index].lifecycle.transition(next)?;
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(error)
            }
        }
    }

    fn fresh_candidate(&self, index: usize) -> Result<PackageCandidate, EngineError> {
        let record = &self.packages[index];
        let source = self
            .sources
            .iter()
            .find(|source| source.id() == &record.source_id)
            .ok_or_else(|| EngineError::Source {
                source: record.source_id.clone(),
                message: "package source is no longer registered".into(),
            })?;
        source
            .discover()
            .map_err(|error| EngineError::Source {
                source: source.id().clone(),
                message: error.to_string(),
            })?
            .into_iter()
            .find(|candidate| candidate.manifest.id == record.candidate.manifest.id)
            .ok_or_else(|| EngineError::Source {
                source: source.id().clone(),
                message: format!("package {} disappeared", record.candidate.manifest.id),
            })
    }

    fn build_runtime(&mut self, index: usize) -> Result<PackageRuntime, EngineError> {
        let artifact =
            self.compile_candidate(&self.packages[index], &self.packages[index].candidate)?;
        self.instantiate_runtime(index, artifact)
    }

    fn instantiate_runtime(
        &mut self,
        index: usize,
        artifact: CompiledPackageArtifact,
    ) -> Result<PackageRuntime, EngineError> {
        let record = &self.packages[index];
        let manifest = &record.candidate.manifest;
        let schema_hash =
            nexa_core::StableId::from_parts(&[manifest.id.as_str(), "::", &manifest.state_schema]);
        self.validate_exports(manifest.id.clone(), &artifact.verified)?;
        let context = PackageContext {
            package_id: manifest.id.clone(),
            source_id: record.source_id.clone(),
            trust: record.policy.trust,
            capabilities: manifest.capabilities.clone(),
            data_namespace: format!("{}.{}", record.source_id, manifest.id),
            version: manifest.version.clone(),
        };
        let registry = self.host_factory.create(&context);
        let config = nexa_runtime::RealmConfig {
            realm_id: self.next_realm_id,
            max_heap_objects: manifest.heap_objects,
            max_host_resources: manifest.host_resources,
            release_capacity: manifest.release_records,
            runtime_limits: nexa_runtime::RuntimeLimits {
                max_tasks: manifest.tasks,
                max_scheduler_tokens: manifest.tasks,
                ..nexa_runtime::RuntimeLimits::default()
            },
            ..nexa_runtime::RealmConfig::default()
        };
        self.next_realm_id = self
            .next_realm_id
            .checked_add(1)
            .ok_or(EngineError::RealmIdExhausted)?;
        let mut realm =
            nexa_runtime::RealmRuntime::hosted(config, self.runtime_host.clone(), registry)
                .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        let module = realm
            .load_module(
                artifact.verified.clone(),
                self.contract.interface_hash,
                schema_hash,
            )
            .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        let root_scope = realm
            .create_scope(None)
            .map_err(|error| EngineError::Load(manifest.id.clone(), error.to_string()))?;
        Ok(PackageRuntime {
            realm,
            module,
            root_scope,
            artifact,
        })
    }

    fn compile_candidate(
        &self,
        record: &PackageRecord,
        candidate: &PackageCandidate,
    ) -> Result<CompiledPackageArtifact, EngineError> {
        artifact::compile_package_candidate(
            &self.idl,
            &self.required_exports,
            &record.source_id,
            candidate,
        )
        .map(|compilation| compilation.artifact)
        .map_err(|diagnostic| EngineError::Diagnostic(Box::new(diagnostic)))
    }

    fn validate_exports(
        &self,
        id: PackageId,
        verified: &nexa_verifier::VerifiedModule,
    ) -> Result<(), EngineError> {
        for requirement in &self.required_exports {
            let Some(found) = verified
                .module()
                .exports
                .iter()
                .find(|export| export.stable_id == requirement.stable_id)
            else {
                return Err(EngineError::MissingExport(id, requirement.name.clone()));
            };
            if found.signature != requirement.signature {
                return Err(EngineError::ExportSignature(id, requirement.name.clone()));
            }
        }
        Ok(())
    }

    pub fn disable(&mut self, id: &PackageId) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        if self.packages[index].candidate.manifest.activation == ActivationPolicy::Required {
            return Err(EngineError::RequiredPackage(id.clone()));
        }
        self.disable_index(index, true)
    }

    pub fn fault(&mut self, id: &PackageId, message: impl Into<String>) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        let message = message.into();
        let status = self.packages[index].lifecycle.status();
        if status == PackageStatus::Enabled {
            if let Some(mut runtime) = self.packages[index].runtime.take() {
                let _ = runtime.realm.cancel_scope(runtime.root_scope);
                drop(runtime);
            }
            self.packages[index]
                .lifecycle
                .transition(PackageStatus::Faulted)?;
        } else if status != PackageStatus::Faulted {
            return Err(EngineError::InvalidState(id.clone(), status));
        }
        let summary = self.record_diagnostic(
            index,
            EngineDiagnosticStage::Handler,
            nexa::ErrorCode::NX7103,
            message,
        );
        self.packages[index].last_diagnostic = Some(summary);
        self.drain_releases();
        Ok(())
    }

    fn disable_index(&mut self, index: usize, persist: bool) -> Result<(), EngineError> {
        if self.packages[index].lifecycle.status() == PackageStatus::Disabled {
            return Ok(());
        }
        if self.packages[index].lifecycle.status() == PackageStatus::Locked {
            return Ok(());
        }
        let id = self.packages[index].candidate.manifest.id.clone();
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Disabling)?;
        if let Some(mut runtime) = self.packages[index].runtime.take() {
            let _ = runtime.realm.cancel_scope(runtime.root_scope);
            let _ = runtime.realm.tick(nexa_runtime::TickBudget {
                max_tasks: usize::MAX,
                frame_fuel_budget: 4_096,
                collect_garbage: true,
            });
            drop(runtime);
        }
        self.drain_releases();
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Disabled)?;
        if persist {
            self.persisted.insert(id, false);
            self.persist_selections()?;
        }
        Ok(())
    }

    pub fn reload(&mut self, id: &PackageId) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        if !matches!(
            self.packages[index].lifecycle.status(),
            PackageStatus::Enabled | PackageStatus::Faulted
        ) {
            return Err(EngineError::InvalidState(
                id.clone(),
                self.packages[index].lifecycle.status(),
            ));
        }
        let candidate = self.fresh_candidate(index)?;
        self.reload_candidate(index, candidate)
    }

    pub fn commit_ready(&mut self, id: &PackageId) -> Result<Option<ReloadReport>, EngineError> {
        let index = self.unique_index(id)?;
        let Some(ready) = self.packages[index].ready_candidate.take() else {
            return Ok(None);
        };
        match self.commit_compiled_candidate(
            index,
            ready.candidate,
            ready.compilation.artifact,
            ready.generation,
            ready.compilation.compile_duration,
            ready.compilation.verify_duration,
        ) {
            Ok(report) => {
                self.publish_reload(report.clone());
                Ok(Some(report))
            }
            Err(failure) => {
                self.publish_reload(failure.report);
                Err(failure.error)
            }
        }
    }

    fn reload_candidate(
        &mut self,
        index: usize,
        candidate: PackageCandidate,
    ) -> Result<(), EngineError> {
        let compilation = match artifact::compile_package_candidate(
            &self.idl,
            &self.required_exports,
            &self.packages[index].source_id,
            &candidate,
        ) {
            Ok(compilation) => compilation,
            Err(diagnostic) => {
                let diagnostic = self.diagnostics.push(diagnostic);
                self.packages[index].last_diagnostic = Some(diagnostic.summary());
                let report = reload_report(
                    self.packages[index].candidate.manifest.id.clone(),
                    0,
                    self.packages[index]
                        .runtime
                        .as_ref()
                        .and_then(|runtime| runtime.realm.active_module_epoch(runtime.module).ok())
                        .unwrap_or_default(),
                    None,
                    candidate_source_hash(&candidate),
                    std::time::Duration::ZERO,
                    std::time::Duration::ZERO,
                    std::time::Duration::ZERO,
                    if diagnostic.stage == EngineDiagnosticStage::Verify {
                        ReloadReportOutcome::VerifyFailed
                    } else {
                        ReloadReportOutcome::CompileFailed
                    },
                    0,
                    0,
                );
                self.publish_reload(report);
                return Err(EngineError::Diagnostic(Box::new(diagnostic)));
            }
        };
        match self.commit_compiled_candidate(
            index,
            candidate,
            compilation.artifact,
            0,
            compilation.compile_duration,
            compilation.verify_duration,
        ) {
            Ok(report) => {
                self.publish_reload(report);
                Ok(())
            }
            Err(failure) => {
                self.publish_reload(failure.report);
                Err(failure.error)
            }
        }
    }

    #[allow(clippy::result_large_err, clippy::too_many_lines)]
    fn commit_compiled_candidate(
        &mut self,
        index: usize,
        candidate: PackageCandidate,
        artifact: CompiledPackageArtifact,
        generation: u64,
        compile_duration: std::time::Duration,
        verify_duration: std::time::Duration,
    ) -> Result<ReloadReport, ReloadFailure> {
        let id = self.packages[index].candidate.manifest.id.clone();
        let source_hash = artifact.source_hash;
        let old_epoch = self.packages[index]
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.realm.active_module_epoch(runtime.module).ok())
            .unwrap_or_default();
        let reload_started = std::time::Instant::now();
        if self.packages[index].lifecycle.status() == PackageStatus::Faulted {
            let old_candidate = std::mem::replace(&mut self.packages[index].candidate, candidate);
            if let Err(error) = self.packages[index]
                .lifecycle
                .transition(PackageStatus::Enabling)
            {
                self.packages[index].candidate = old_candidate;
                let engine_error = EngineError::Lifecycle(error);
                return Err(ReloadFailure::new(
                    reload_report(
                        id,
                        generation,
                        old_epoch,
                        None,
                        source_hash,
                        compile_duration,
                        verify_duration,
                        reload_started.elapsed(),
                        ReloadReportOutcome::ActivationFaulted,
                        0,
                        0,
                    ),
                    engine_error,
                ));
            }
            return match self.instantiate_runtime(index, artifact.clone()) {
                Ok(runtime) => {
                    let new_epoch = runtime.realm.active_module_epoch(runtime.module).ok();
                    self.packages[index].runtime = Some(runtime);
                    let _ = self.packages[index]
                        .lifecycle
                        .transition(PackageStatus::Enabled);
                    self.commit_last_known_good(
                        index,
                        artifact,
                        generation,
                        new_epoch.unwrap_or(0),
                    );
                    self.packages[index].last_diagnostic = None;
                    Ok(reload_report(
                        id,
                        generation,
                        old_epoch,
                        new_epoch,
                        source_hash,
                        compile_duration,
                        verify_duration,
                        reload_started.elapsed(),
                        ReloadReportOutcome::Committed,
                        0,
                        0,
                    ))
                }
                Err(error) => {
                    self.packages[index].candidate = old_candidate;
                    let _ = self.packages[index]
                        .lifecycle
                        .transition(PackageStatus::Faulted);
                    let summary = self.record_error(index, &error);
                    self.packages[index].last_diagnostic = Some(summary);
                    Err(ReloadFailure::new(
                        reload_report(
                            id,
                            generation,
                            old_epoch,
                            None,
                            source_hash,
                            compile_duration,
                            verify_duration,
                            reload_started.elapsed(),
                            ReloadReportOutcome::ActivationFaulted,
                            0,
                            0,
                        ),
                        error,
                    ))
                }
            };
        }
        if let Err(error) = self.packages[index]
            .lifecycle
            .transition(PackageStatus::Reloading)
        {
            return Err(ReloadFailure::new(
                reload_report(
                    id,
                    generation,
                    old_epoch,
                    None,
                    source_hash,
                    compile_duration,
                    verify_duration,
                    reload_started.elapsed(),
                    ReloadReportOutcome::RolledBackBeforeCommit,
                    0,
                    0,
                ),
                EngineError::Lifecycle(error),
            ));
        }
        let outcome = {
            let runtime = self.packages[index].runtime.as_mut().ok_or_else(|| {
                ReloadFailure::new(
                    reload_report(
                        id.clone(),
                        generation,
                        old_epoch,
                        None,
                        source_hash,
                        compile_duration,
                        verify_duration,
                        reload_started.elapsed(),
                        ReloadReportOutcome::RolledBackBeforeCommit,
                        0,
                        0,
                    ),
                    EngineError::InvalidState(id.clone(), PackageStatus::Reloading),
                )
            })?;
            runtime.realm.restart_reload(
                runtime.module,
                artifact.verified.clone(),
                nexa_runtime::RestartReloadPolicy::default(),
            )
        };
        let accounting = self.packages[index]
            .runtime
            .as_ref()
            .map(|runtime| runtime.realm.reload_accounting())
            .unwrap_or_default();
        match outcome {
            Ok(nexa_runtime::RestartReloadOutcome::Committed(module)) => {
                let new_epoch = if let Some(runtime) = self.packages[index].runtime.as_mut() {
                    runtime.module = module;
                    runtime.artifact = artifact.clone();
                    runtime.realm.active_module_epoch(module).ok()
                } else {
                    None
                };
                self.packages[index].candidate = candidate;
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled);
                self.packages[index].last_diagnostic = None;
                self.commit_last_known_good(
                    index,
                    artifact,
                    generation,
                    new_epoch.unwrap_or(old_epoch.saturating_add(1)),
                );
                self.drain_releases();
                Ok(reload_report(
                    id,
                    generation,
                    old_epoch,
                    new_epoch,
                    source_hash,
                    compile_duration,
                    verify_duration,
                    reload_started.elapsed(),
                    ReloadReportOutcome::Committed,
                    accounting.cancelled_tasks,
                    accounting.detached_requests,
                ))
            }
            Ok(nexa_runtime::RestartReloadOutcome::RolledBackBeforeCommit { reason, .. })
            | Err(reason) => {
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled);
                let error = EngineError::Reload(id.clone(), reason.to_string());
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(ReloadFailure::new(
                    reload_report(
                        id,
                        generation,
                        old_epoch,
                        None,
                        source_hash,
                        compile_duration,
                        verify_duration,
                        reload_started.elapsed(),
                        ReloadReportOutcome::RolledBackBeforeCommit,
                        accounting.cancelled_tasks,
                        accounting.detached_requests,
                    ),
                    error,
                ))
            }
            Ok(nexa_runtime::RestartReloadOutcome::ActivationFaulted { error, .. }) => {
                self.packages[index].runtime = None;
                let _ = self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Faulted);
                let error = EngineError::Activation(id.clone(), error.to_string());
                let summary = self.record_error(index, &error);
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(ReloadFailure::new(
                    reload_report(
                        id,
                        generation,
                        old_epoch,
                        None,
                        source_hash,
                        compile_duration,
                        verify_duration,
                        reload_started.elapsed(),
                        ReloadReportOutcome::ActivationFaulted,
                        accounting.cancelled_tasks,
                        accounting.detached_requests,
                    ),
                    error,
                ))
            }
        }
    }

    fn commit_last_known_good(
        &mut self,
        index: usize,
        artifact: CompiledPackageArtifact,
        generation: u64,
        epoch: u64,
    ) {
        let manifest = &self.packages[index].candidate.manifest;
        self.packages[index].last_known_good = Some(LastKnownGood {
            source_hash: artifact.source_hash,
            state_schema_hash: nexa_core::StableId::from_parts(&[
                manifest.id.as_str(),
                "::",
                &manifest.state_schema,
            ]),
            host_interface_hash: self.contract.interface_hash,
            artifact,
            epoch,
            committed_generation: generation,
        });
    }

    pub fn reload_changed(&mut self) -> Result<usize, EngineError> {
        let mut changed = Vec::new();
        for source in &self.sources {
            for candidate in source.discover().map_err(|error| EngineError::Source {
                source: source.id().clone(),
                message: error.to_string(),
            })? {
                if let Some(index) = self.packages.iter().position(|record| {
                    record.source_id == *source.id()
                        && record.candidate.manifest.id == candidate.manifest.id
                        && record.lifecycle.status() == PackageStatus::Enabled
                        && (record.candidate.entry_hash != candidate.entry_hash
                            || record.candidate.manifest_hash != candidate.manifest_hash)
                }) {
                    changed.push((index, candidate));
                }
            }
        }
        let count = changed.len();
        for (index, candidate) in changed {
            self.reload_candidate(index, candidate)?;
        }
        Ok(count)
    }

    #[allow(clippy::too_many_lines)]
    fn scan_development_changes(&mut self) {
        let discoveries = self
            .sources
            .iter()
            .map(|source| {
                let started = std::time::Instant::now();
                (
                    source.id().clone(),
                    source.discover().map_err(|error| error.to_string()),
                    started.elapsed(),
                )
            })
            .collect::<Vec<_>>();
        for (source_id, discovered, discovery_duration) in discoveries {
            let candidates = match discovered {
                Ok(candidates) => candidates,
                Err(message) => {
                    let diagnostic = self.diagnostics.push(EngineDiagnostic::without_source(
                        None,
                        Some(source_id),
                        EngineDiagnosticStage::SourceDiscovery,
                        nexa::ErrorCode::NX7001,
                        message,
                    ));
                    if let Some(package_id) = diagnostic.package_id.clone() {
                        self.publish_event(DevelopmentEvent::CompileFailed(
                            development_event_data(
                                package_id,
                                0,
                                SourceHash::default(),
                                Some(diagnostic),
                            ),
                        ));
                    }
                    continue;
                }
            };
            let indexes = self
                .packages
                .iter()
                .enumerate()
                .filter(|(_, record)| {
                    record.source_id == source_id
                        && matches!(
                            record.lifecycle.status(),
                            PackageStatus::Enabled | PackageStatus::Faulted
                        )
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            for index in indexes {
                self.packages[index].development.last_discovery_duration = discovery_duration;
                let package_id = self.packages[index].candidate.manifest.id.clone();
                let Some(candidate) = candidates
                    .iter()
                    .find(|candidate| candidate.manifest.id == package_id)
                    .cloned()
                else {
                    let generation = self.packages[index]
                        .development
                        .latest_generation
                        .saturating_add(1);
                    self.packages[index].development.state = DevelopmentState::HostRebuildRequired;
                    self.publish_event(DevelopmentEvent::HostRebuildRequired(
                        development_event_data(package_id, generation, SourceHash::default(), None),
                    ));
                    continue;
                };
                let hash_started = std::time::Instant::now();
                let source_hash = candidate_source_hash(&candidate);
                self.packages[index].development.last_source_hash_duration = hash_started.elapsed();
                let active_hash = candidate_source_hash(&self.packages[index].candidate);
                if source_hash == active_hash
                    || self.packages[index].development.last_processed_hash == Some(source_hash)
                {
                    continue;
                }
                if self.packages[index].development.observed_hash == Some(source_hash) {
                    self.packages[index].development.stable_scans = self.packages[index]
                        .development
                        .stable_scans
                        .saturating_add(1);
                } else {
                    self.packages[index].development.observed_hash = Some(source_hash);
                    self.packages[index].development.stable_scans = 1;
                    self.packages[index].development.state = DevelopmentState::ChangeObserved;
                    self.packages[index].development.latest_generation = self.packages[index]
                        .development
                        .latest_generation
                        .saturating_add(1);
                    let generation = self.packages[index].development.latest_generation;
                    self.publish_event(DevelopmentEvent::ChangeDetected(development_event_data(
                        package_id.clone(),
                        generation,
                        source_hash,
                        None,
                    )));
                    if self.development.stable_scan_count > 1 {
                        self.packages[index].development.state =
                            DevelopmentState::WaitingForStableWrite;
                        continue;
                    }
                }
                if self.packages[index].development.stable_scans
                    < self.development.stable_scan_count.max(1)
                {
                    self.packages[index].development.state =
                        DevelopmentState::WaitingForStableWrite;
                    continue;
                }
                let generation = self.packages[index].development.latest_generation;
                self.packages[index].development.last_processed_hash = Some(source_hash);
                self.packages[index].development.state = DevelopmentState::CompileQueued;
                self.publish_event(DevelopmentEvent::ChangeStabilized(development_event_data(
                    package_id.clone(),
                    generation,
                    source_hash,
                    None,
                )));
                let job = CompileJob {
                    package_id: package_id.clone(),
                    source_id: source_id.clone(),
                    generation,
                    source_hash,
                    candidate,
                    idl: self.idl.clone(),
                    required_exports: self.required_exports.clone(),
                    queued_at: std::time::Instant::now(),
                };
                let superseded = self
                    .development_worker
                    .as_ref()
                    .map_or_else(Vec::new, |worker| worker.enqueue(job));
                self.publish_event(DevelopmentEvent::CompileQueued(development_event_data(
                    package_id,
                    generation,
                    source_hash,
                    None,
                )));
                for (package_id, generation, source_hash) in superseded {
                    self.publish_event(DevelopmentEvent::CandidateSuperseded(
                        development_event_data(package_id, generation, source_hash, None),
                    ));
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_compile_results(&mut self) {
        let results = self
            .development_worker
            .as_ref()
            .map_or_else(Vec::new, DevelopmentWorker::drain_results);
        for result in results {
            let Some(index) = self
                .packages
                .iter()
                .position(|record| record.candidate.manifest.id == result.package_id)
            else {
                continue;
            };
            if !matches!(
                self.packages[index].lifecycle.status(),
                PackageStatus::Enabled | PackageStatus::Faulted
            ) {
                self.publish_event(DevelopmentEvent::CandidateSuperseded(
                    development_event_data(
                        result.package_id,
                        result.generation,
                        result.source_hash,
                        None,
                    ),
                ));
                continue;
            }
            let started_data = development_event_data(
                result.package_id.clone(),
                result.generation,
                result.source_hash,
                None,
            );
            self.packages[index].development.state = DevelopmentState::Compiling;
            self.packages[index].development.last_queue_duration = result.queue_duration;
            self.publish_event(DevelopmentEvent::CompileStarted(DevelopmentEventData {
                queue_duration: Some(result.queue_duration),
                ..started_data
            }));
            if result.generation != self.packages[index].development.latest_generation {
                self.publish_event(DevelopmentEvent::CandidateSuperseded(
                    development_event_data(
                        result.package_id.clone(),
                        result.generation,
                        result.source_hash,
                        None,
                    ),
                ));
                self.publish_reload(reload_report(
                    result.package_id,
                    result.generation,
                    0,
                    None,
                    result.source_hash,
                    result.work_duration,
                    std::time::Duration::ZERO,
                    std::time::Duration::ZERO,
                    ReloadReportOutcome::Superseded,
                    0,
                    0,
                ));
                continue;
            }
            match result.result {
                Err(mut diagnostic) => {
                    diagnostic.context.candidate_generation = Some(result.generation);
                    let stage = diagnostic.stage;
                    let diagnostic = self.diagnostics.push(diagnostic);
                    self.packages[index].last_diagnostic = Some(diagnostic.summary());
                    self.packages[index].development.last_compile_duration =
                        Some(result.work_duration);
                    let data = development_event_data(
                        result.package_id.clone(),
                        result.generation,
                        result.source_hash,
                        Some(diagnostic),
                    );
                    let (event, outcome, next_state) = if stage == EngineDiagnosticStage::Verify {
                        (
                            DevelopmentEvent::VerifyFailed(data),
                            ReloadReportOutcome::VerifyFailed,
                            DevelopmentState::VerifyFailed,
                        )
                    } else {
                        (
                            DevelopmentEvent::CompileFailed(data),
                            ReloadReportOutcome::CompileFailed,
                            DevelopmentState::CompileFailed,
                        )
                    };
                    self.packages[index].development.state = next_state;
                    self.publish_event(event);
                    self.publish_reload(reload_report(
                        result.package_id,
                        result.generation,
                        0,
                        None,
                        result.source_hash,
                        result.work_duration,
                        std::time::Duration::ZERO,
                        std::time::Duration::ZERO,
                        outcome,
                        0,
                        0,
                    ));
                }
                Ok(compilation) => {
                    self.packages[index].development.last_compile_duration =
                        Some(compilation.compile_duration);
                    self.packages[index].development.last_verify_duration =
                        compilation.verify_duration;
                    self.publish_event(DevelopmentEvent::CompileSucceeded(development_event_data(
                        result.package_id.clone(),
                        result.generation,
                        result.source_hash,
                        None,
                    )));
                    self.packages[index].development.state = DevelopmentState::CandidateReady;
                    self.publish_event(DevelopmentEvent::CandidateReady(development_event_data(
                        result.package_id.clone(),
                        result.generation,
                        result.source_hash,
                        None,
                    )));
                    if !self.development.auto_reload {
                        self.packages[index].ready_candidate = Some(development::ReadyCandidate {
                            candidate: result.candidate,
                            compilation,
                            generation: result.generation,
                        });
                        continue;
                    }
                    self.packages[index].development.state = DevelopmentState::ReloadPending;
                    self.publish_event(DevelopmentEvent::ReloadStarted(development_event_data(
                        result.package_id.clone(),
                        result.generation,
                        result.source_hash,
                        None,
                    )));
                    self.packages[index].development.state = DevelopmentState::Reloading;
                    let reload_started = std::time::Instant::now();
                    match self.commit_compiled_candidate(
                        index,
                        result.candidate,
                        compilation.artifact,
                        result.generation,
                        compilation.compile_duration,
                        compilation.verify_duration,
                    ) {
                        Ok(report) => {
                            self.packages[index].development.state = DevelopmentState::Reloaded;
                            self.packages[index].development.last_reload_duration =
                                Some(reload_started.elapsed());
                            self.packages[index].development.last_migration_duration =
                                report.migration_duration;
                            self.packages[index].development.last_activation_duration =
                                report.activation_duration;
                            self.publish_event(DevelopmentEvent::ReloadCommitted(
                                DevelopmentEventData {
                                    reload: Some(report.summary()),
                                    ..development_event_data(
                                        result.package_id,
                                        result.generation,
                                        result.source_hash,
                                        None,
                                    )
                                },
                            ));
                            self.publish_reload(report);
                        }
                        Err(failure) => {
                            self.packages[index].development.last_migration_duration =
                                failure.report.migration_duration;
                            self.packages[index].development.last_activation_duration =
                                failure.report.activation_duration;
                            let activation =
                                failure.report.outcome == ReloadReportOutcome::ActivationFaulted;
                            self.packages[index].development.state = if activation {
                                DevelopmentState::ActivationFaulted
                            } else {
                                DevelopmentState::MigrationFailed
                            };
                            let data = DevelopmentEventData {
                                reload: Some(failure.report.summary()),
                                ..development_event_data(
                                    result.package_id,
                                    result.generation,
                                    result.source_hash,
                                    None,
                                )
                            };
                            self.publish_event(if activation {
                                DevelopmentEvent::ActivationFaulted(data)
                            } else {
                                DevelopmentEvent::ReloadRolledBack(data)
                            });
                            self.publish_reload(failure.report);
                        }
                    }
                }
            }
        }
    }

    fn publish_event(&mut self, event: DevelopmentEvent) {
        let retain = self.development.retain_events.max(1);
        while self.development_events.len() >= retain {
            self.development_events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.development_events.push_back(event.clone());
        while self.pending_development_events.len() >= retain {
            self.pending_development_events.remove(0);
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        self.pending_development_events.push(event);
    }

    pub fn call<E: nexa_runtime::ScriptExport>(
        &mut self,
        id: &PackageId,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EngineError> {
        let index = self.unique_index(id)?;
        self.call_index::<E>(index, args)
    }

    fn call_index<E: nexa_runtime::ScriptExport>(
        &mut self,
        index: usize,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EngineError> {
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EngineError::InvalidState(
                record.candidate.manifest.id.clone(),
                record.lifecycle.status(),
            ));
        }
        let manifest = &record.candidate.manifest;
        let policy = nexa_runtime::MustCompletePolicy {
            fuel: manifest.handler_fuel,
            cumulative_budget: manifest.cumulative_budget,
        };
        let runtime = record.runtime.as_mut().ok_or_else(|| {
            EngineError::InvalidState(manifest.id.clone(), PackageStatus::Enabled)
        })?;
        record.handler_calls_this_tick = record.handler_calls_this_tick.saturating_add(1);
        match runtime.realm.call_export_metered::<E>(
            runtime.module,
            runtime.root_scope,
            args,
            policy,
        ) {
            Ok((value, charge)) => {
                record.fuel_used_this_tick =
                    record.fuel_used_this_tick.saturating_add(charge.fuel_used);
                record.outputs_this_tick = record.outputs_this_tick.saturating_add(1);
                Ok(PackageOutput {
                    package_id: manifest.id.clone(),
                    source_id: record.source_id.clone(),
                    trust: record.policy.trust,
                    capabilities: manifest.capabilities.clone(),
                    value,
                })
            }
            Err(call_error) => {
                let trap_diagnostic = match &call_error {
                    nexa_runtime::ScriptCallError::HandlerTrapped(trap) => Some(
                        runtime_trap_diagnostic(record, &self.contract, &self.idl, trap, E::NAME),
                    ),
                    _ => None,
                };
                let code = match &call_error {
                    nexa_runtime::ScriptCallError::HandlerDidNotComplete => nexa::ErrorCode::NX7101,
                    nexa_runtime::ScriptCallError::HostWaitNotAllowed => nexa::ErrorCode::NX7102,
                    _ => nexa::ErrorCode::NX7103,
                };
                let error = EngineError::Handler(manifest.id.clone(), call_error.to_string());
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
                let summary = if let Some(diagnostic) = trap_diagnostic {
                    self.diagnostics.push(diagnostic).summary()
                } else {
                    self.record_diagnostic(
                        index,
                        EngineDiagnosticStage::Handler,
                        code,
                        error.to_string(),
                    )
                };
                self.packages[index].last_diagnostic = Some(summary);
                self.drain_releases();
                Err(error)
            }
        }
    }

    pub fn dispatch<E: nexa_runtime::ScriptExport>(
        &mut self,
        args: &E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EngineError>> {
        let mut indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.lifecycle.status() == PackageStatus::Enabled)
            .map(|(index, record)| {
                (
                    index,
                    record.candidate.manifest.priority,
                    record.candidate.manifest.id.clone(),
                )
            })
            .collect::<Vec<_>>();
        indexes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
        indexes
            .into_iter()
            .map(|(index, _, _)| self.call_index::<E>(index, args))
            .collect()
    }

    /// Deterministically dispatches while allowing package-specific immutable
    /// arguments, such as a projection of that package's typed state.
    pub fn dispatch_with<E: nexa_runtime::ScriptExport>(
        &mut self,
        mut args: impl FnMut(&PackageInfo) -> E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EngineError>> {
        let mut indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.lifecycle.status() == PackageStatus::Enabled)
            .map(|(index, record)| {
                (
                    index,
                    record.candidate.manifest.priority,
                    record.candidate.manifest.id.clone(),
                )
            })
            .collect::<Vec<_>>();
        indexes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
        indexes
            .into_iter()
            .map(|(index, _, _)| {
                let package = self.packages[index].info();
                self.call_index::<E>(index, &args(&package))
            })
            .collect()
    }

    /// Inserts or replaces one scalar field in a package's typed state domain.
    ///
    /// The names use the same stable-ID derivation as `@stateful` classes.
    pub fn set_state_i32(
        &mut self,
        id: &PackageId,
        state_key: &str,
        type_name: &str,
        version: u32,
        field_name: &str,
        value: i32,
    ) -> Result<(), EngineError> {
        let index = self.unique_index(id)?;
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EngineError::InvalidState(
                id.clone(),
                record.lifecycle.status(),
            ));
        }
        let runtime = record
            .runtime
            .as_mut()
            .ok_or_else(|| EngineError::InvalidState(id.clone(), PackageStatus::Enabled))?;
        runtime
            .realm
            .insert_state(
                runtime.module,
                nexa_core::StableId::from_name(state_key),
                nexa_runtime::StateValue::Object(nexa_runtime::StateObject {
                    type_id: nexa_core::StableId::from_name(type_name),
                    version,
                    fields: BTreeMap::from([(
                        nexa_core::StableId::from_parts(&[type_name, "::", field_name]),
                        nexa_runtime::StateValue::I32(value),
                    )]),
                }),
            )
            .map(|_| ())
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))
    }

    /// Reads one scalar field from a package's typed state domain.
    pub fn state_i32(
        &self,
        id: &PackageId,
        state_key: &str,
        type_name: &str,
        field_name: &str,
    ) -> Result<Option<i32>, EngineError> {
        let index = self.unique_index(id)?;
        let record = &self.packages[index];
        let Some(runtime) = &record.runtime else {
            return Ok(None);
        };
        let stable_id = nexa_core::StableId::from_name(state_key);
        let Some(handle) = runtime
            .realm
            .state_handles(runtime.module)
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))?
            .into_iter()
            .find(|handle| handle.stable_id() == stable_id)
        else {
            return Ok(None);
        };
        let value = runtime
            .realm
            .resolve_state(runtime.module, handle)
            .map_err(|error| EngineError::State(id.clone(), error.to_string()))?;
        let nexa_runtime::StateValue::Object(object) = value else {
            return Ok(None);
        };
        if object.type_id != nexa_core::StableId::from_name(type_name) {
            return Ok(None);
        }
        Ok(
            match object.fields.get(&nexa_core::StableId::from_parts(&[
                type_name, "::", field_name,
            ])) {
                Some(nexa_runtime::StateValue::I32(value)) => Some(*value),
                _ => None,
            },
        )
    }

    pub fn tick(&mut self) -> Result<EngineTickReport, EngineError> {
        self.require_open()?;
        self.ticks = self.ticks.saturating_add(1);
        for record in &mut self.packages {
            let ledger = record
                .runtime
                .as_ref()
                .map_or_else(nexa_runtime::RuntimeResourceLedger::default, |runtime| {
                    runtime.realm.resource_ledger()
                });
            while record.development.recent_metrics.len() >= 32 {
                record.development.recent_metrics.pop_front();
            }
            record.development.recent_metrics.push_back(PackageMetric {
                tick: self.ticks,
                discovery_duration: record.development.last_discovery_duration,
                source_hash_duration: record.development.last_source_hash_duration,
                candidate_queue_duration: record.development.last_queue_duration,
                compile_duration: record.development.last_compile_duration.unwrap_or_default(),
                verify_duration: record.development.last_verify_duration,
                reload_duration: record.development.last_reload_duration.unwrap_or_default(),
                migration_duration: record.development.last_migration_duration,
                activation_duration: record.development.last_activation_duration,
                handler_calls: record.handler_calls_this_tick,
                handler_instructions: record.fuel_used_this_tick,
                fuel_used: record.fuel_used_this_tick,
                output_count: record.outputs_this_tick,
                task_peak: ledger.tasks,
                request_peak: ledger.requests,
            });
            record.handler_calls_this_tick = 0;
            record.fuel_used_this_tick = 0;
            record.outputs_this_tick = 0;
        }
        if self.development.enabled
            && self.development.scan_interval_ticks != 0
            && self
                .ticks
                .is_multiple_of(self.development.scan_interval_ticks)
        {
            self.scan_development_changes();
        }
        self.process_compile_results();
        let mut runtime_failures = Vec::new();
        for (index, record) in self.packages.iter_mut().enumerate() {
            if let Some(runtime) = record.runtime.as_mut()
                && let Err(error) = runtime.realm.tick(nexa_runtime::TickBudget {
                    collect_garbage: true,
                    ..nexa_runtime::TickBudget::default()
                })
            {
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
                runtime_failures.push((index, error.to_string()));
            }
        }
        for (index, message) in runtime_failures {
            let summary = self.record_diagnostic(
                index,
                EngineDiagnosticStage::Runtime,
                nexa::ErrorCode::NX7103,
                message,
            );
            self.packages[index].last_diagnostic = Some(summary);
        }
        let released_before = self.delivered_release_records;
        self.drain_releases();
        let diagnostics = self
            .diagnostics
            .entries()
            .into_iter()
            .filter(|diagnostic| diagnostic.sequence > self.last_tick_diagnostic_sequence)
            .collect::<Vec<_>>();
        if let Some(sequence) = diagnostics.last().map(|diagnostic| diagnostic.sequence) {
            self.last_tick_diagnostic_sequence = sequence;
        }
        let faulted_packages = self
            .packages
            .iter()
            .filter(|record| record.lifecycle.status() == PackageStatus::Faulted)
            .map(|record| record.candidate.manifest.id.clone())
            .collect();
        Ok(EngineTickReport {
            development_events: std::mem::take(&mut self.pending_development_events),
            diagnostics,
            reloads: std::mem::take(&mut self.pending_reload_reports),
            faulted_packages,
            released_resources: usize::try_from(
                self.delivered_release_records
                    .saturating_sub(released_before),
            )
            .unwrap_or(usize::MAX),
        })
    }

    pub fn refresh_entitlements(&mut self) -> Result<(), EngineError> {
        for index in 0..self.packages.len() {
            let locked = self.packages[index]
                .candidate
                .manifest
                .entitlement
                .as_ref()
                .is_some_and(|id| !self.entitlements.contains(id));
            let status = self.packages[index].lifecycle.status();
            if locked && status == PackageStatus::Enabled {
                self.disable_index(index, false)?;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Locked)?;
            } else if locked && status == PackageStatus::Disabled {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Locked)?;
            } else if !locked && status == PackageStatus::Locked {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Disabled)?;
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn packages(&self) -> Vec<PackageInfo> {
        self.packages.iter().map(PackageRecord::info).collect()
    }

    #[must_use]
    pub fn status(&self, id: &PackageId) -> Option<PackageStatus> {
        let mut matches = self
            .packages
            .iter()
            .filter(|record| record.candidate.manifest.id == *id);
        let status = matches.next()?.lifecycle.status();
        matches.next().is_none().then_some(status)
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<EngineDiagnostic> {
        self.diagnostics.entries()
    }

    #[must_use]
    pub fn health(&self) -> EngineHealth {
        let mut health = EngineHealth {
            delivered_release_records: self.delivered_release_records,
            ..EngineHealth::default()
        };
        for record in &self.packages {
            let Some(runtime) = &record.runtime else {
                continue;
            };
            health.enabled_packages += 1;
            let ledger = runtime.realm.resource_ledger();
            health.tasks = health.tasks.saturating_add(ledger.tasks);
            health.scopes = health.scopes.saturating_add(ledger.scopes);
            health.continuations = health.continuations.saturating_add(ledger.continuations);
            health.scheduler_tokens = health
                .scheduler_tokens
                .saturating_add(ledger.scheduler_tokens);
            health.requests = health.requests.saturating_add(ledger.requests);
            health.completion_reservations = health
                .completion_reservations
                .saturating_add(ledger.completion_reservations);
            health.tokens = health.tokens.saturating_add(ledger.tokens);
            health.snapshots = health.snapshots.saturating_add(ledger.snapshots);
            health.release_reservations = health
                .release_reservations
                .saturating_add(ledger.release_reservations);
            health.queued_releases = health
                .queued_releases
                .saturating_add(ledger.queued_releases);
            health.heap_objects = health.heap_objects.saturating_add(ledger.heap_objects);
            health.state_objects = health.state_objects.saturating_add(ledger.state_objects);
            health.retired_modules = health
                .retired_modules
                .saturating_add(ledger.retired_modules);
        }
        let host = self.runtime_host.close_status();
        health.host_pending_completions = host.pending_completions;
        health.host_pending_releases = host.pending_releases;
        health
    }

    #[must_use]
    pub fn inspection(&self) -> EngineInspection {
        let packages = self
            .packages
            .iter()
            .map(|record| {
                let (active_epoch, ledger) = record.runtime.as_ref().map_or_else(
                    || (None, nexa_runtime::RuntimeResourceLedger::default()),
                    |runtime| {
                        (
                            runtime.realm.active_module_epoch(runtime.module).ok(),
                            runtime.realm.resource_ledger(),
                        )
                    },
                );
                PackageInspection {
                    package_id: record.candidate.manifest.id.clone(),
                    source_id: record.source_id.clone(),
                    status: record.lifecycle.status(),
                    version: record.candidate.manifest.version.clone(),
                    effective_capabilities: record.candidate.manifest.capabilities.clone(),
                    active_epoch,
                    source_hash: record.last_known_good.as_ref().map_or_else(
                        || candidate_source_hash(&record.candidate),
                        |known_good| known_good.source_hash,
                    ),
                    candidate_generation: record.development.latest_generation,
                    tasks: ledger.tasks,
                    waiting_requests: ledger.requests,
                    host_resources: ledger.tokens.saturating_add(ledger.snapshots),
                    handler_calls_this_tick: record.handler_calls_this_tick,
                    fuel_used_this_tick: record.fuel_used_this_tick,
                    last_compile_duration: record.development.last_compile_duration,
                    last_reload_duration: record.development.last_reload_duration,
                    recent_diagnostic: record.last_diagnostic.clone(),
                    recent_metrics: record.development.recent_metrics.iter().cloned().collect(),
                }
            })
            .collect();
        let mut recent_diagnostics = self
            .diagnostics
            .entries()
            .into_iter()
            .rev()
            .take(128)
            .map(|diagnostic| diagnostic.summary())
            .collect::<Vec<_>>();
        recent_diagnostics.reverse();
        if let Some(summary) = self.diagnostics.dropped_summary() {
            recent_diagnostics.push(summary);
        }
        EngineInspection {
            health: self.health(),
            packages,
            development: DevelopmentInspection {
                enabled: self.development.enabled,
                worker_running: self.development_worker.is_some(),
                queued_candidates: self
                    .development_worker
                    .as_ref()
                    .map_or(0, DevelopmentWorker::queued_len),
                retained_events: self.development_events.len(),
            },
            recent_diagnostics,
            recent_reloads: self
                .reload_reports
                .iter()
                .map(ReloadReport::summary)
                .collect(),
            dropped_diagnostics: self.diagnostics.dropped(),
            dropped_events: self.dropped_events,
        }
    }

    pub fn shutdown(&mut self) -> Result<(), EngineError> {
        if self.shutdown {
            return Ok(());
        }
        if let Some(mut worker) = self.development_worker.take() {
            worker.shutdown();
        }
        for index in 0..self.packages.len() {
            if self.packages[index].lifecycle.status() == PackageStatus::Enabled {
                self.disable_index(index, false)?;
            }
        }
        self.drain_releases();
        let _ = self.runtime_host.begin_close();
        self.runtime_host
            .try_finish_close()
            .map_err(|error| EngineError::Shutdown(error.to_string()))?;
        self.shutdown = true;
        Ok(())
    }

    fn unique_index(&self, id: &PackageId) -> Result<usize, EngineError> {
        let indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.candidate.manifest.id == *id)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match indexes.as_slice() {
            [index] => Ok(*index),
            [] => Err(EngineError::UnknownPackage(id.clone())),
            _ => Err(EngineError::Incompatible(id.clone())),
        }
    }

    fn persist_selections(&self) -> Result<(), EngineError> {
        self.storage_dir.as_deref().map_or(Ok(()), |directory| {
            persistence::save(directory, &self.persisted)
                .map_err(|error| EngineError::Persistence(error.to_string()))
        })
    }

    fn drain_releases(&mut self) -> usize {
        let count = self.runtime_host.drain_releases().len();
        self.delivered_release_records = self
            .delivered_release_records
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        count
    }

    fn publish_reload(&mut self, report: ReloadReport) {
        while self.reload_reports.len() >= 64 {
            self.reload_reports.pop_front();
        }
        self.reload_reports.push_back(report.clone());
        while self.pending_reload_reports.len() >= 64 {
            self.pending_reload_reports.remove(0);
        }
        self.pending_reload_reports.push(report);
    }

    fn record_error(&mut self, index: usize, error: &EngineError) -> EngineDiagnosticSummary {
        let record = &self.packages[index];
        let package_id = Some(record.candidate.manifest.id.clone());
        let source_id = Some(record.source_id.clone());
        let diagnostic = if let EngineError::Diagnostic(diagnostic) = error {
            diagnostic.as_ref().clone()
        } else {
            let (stage, code) = match error {
                EngineError::Source { .. } => (
                    EngineDiagnosticStage::SourceDiscovery,
                    nexa::ErrorCode::NX7001,
                ),
                EngineError::Persistence(_) => {
                    (EngineDiagnosticStage::Persistence, nexa::ErrorCode::NX7302)
                }
                EngineError::Locked(_) => {
                    (EngineDiagnosticStage::Entitlement, nexa::ErrorCode::NX7004)
                }
                EngineError::Load(_, _) => (EngineDiagnosticStage::Load, nexa::ErrorCode::NX4001),
                EngineError::MissingExport(_, _) => {
                    (EngineDiagnosticStage::Export, nexa::ErrorCode::NX7010)
                }
                EngineError::ExportSignature(_, _) => {
                    (EngineDiagnosticStage::Export, nexa::ErrorCode::NX7011)
                }
                EngineError::Handler(_, _) => {
                    (EngineDiagnosticStage::Handler, nexa::ErrorCode::NX7103)
                }
                EngineError::Reload(_, _) => {
                    (EngineDiagnosticStage::Reload, nexa::ErrorCode::NX7201)
                }
                EngineError::Activation(_, _) => {
                    (EngineDiagnosticStage::Activation, nexa::ErrorCode::NX7202)
                }
                EngineError::Shutdown(_) => {
                    (EngineDiagnosticStage::Shutdown, nexa::ErrorCode::NX7303)
                }
                _ => (EngineDiagnosticStage::Policy, nexa::ErrorCode::NX7003),
            };
            EngineDiagnostic::without_source(package_id, source_id, stage, code, error.to_string())
        };
        self.diagnostics.push(diagnostic).summary()
    }

    fn record_diagnostic(
        &mut self,
        index: usize,
        stage: EngineDiagnosticStage,
        code: nexa::ErrorCode,
        message: impl AsRef<str>,
    ) -> EngineDiagnosticSummary {
        let record = &self.packages[index];
        self.diagnostics
            .push(EngineDiagnostic::without_source(
                Some(record.candidate.manifest.id.clone()),
                Some(record.source_id.clone()),
                stage,
                code,
                message,
            ))
            .summary()
    }

    fn require_open(&self) -> Result<(), EngineError> {
        if self.shutdown {
            Err(EngineError::Shutdown("engine is already shut down".into()))
        } else {
            Ok(())
        }
    }
}

impl Drop for NexaEngine {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug)]
pub enum EngineError {
    MissingHostFactory,
    DuplicateSourceId(SourceId),
    Contract(String),
    Source { source: SourceId, message: String },
    Persistence(String),
    Lifecycle(LifecycleError),
    UnknownPackage(PackageId),
    Incompatible(PackageId),
    Locked(PackageId),
    RequiredPackage(PackageId),
    InvalidState(PackageId, PackageStatus),
    Diagnostic(Box<EngineDiagnostic>),
    Load(PackageId, String),
    MissingExport(PackageId, String),
    ExportSignature(PackageId, String),
    Handler(PackageId, String),
    State(PackageId, String),
    Reload(PackageId, String),
    Activation(PackageId, String),
    RealmIdExhausted,
    Shutdown(String),
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for EngineError {}

impl From<LifecycleError> for EngineError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}

struct ReloadFailure {
    report: ReloadReport,
    error: EngineError,
}

impl ReloadFailure {
    const fn new(report: ReloadReport, error: EngineError) -> Self {
        Self { report, error }
    }
}

#[allow(clippy::too_many_arguments)]
fn reload_report(
    package_id: PackageId,
    candidate_generation: u64,
    old_epoch: u64,
    new_epoch: Option<u64>,
    source_hash: SourceHash,
    compile_duration: std::time::Duration,
    verify_duration: std::time::Duration,
    reload_duration: std::time::Duration,
    outcome: ReloadReportOutcome,
    cancelled_tasks: usize,
    detached_requests: usize,
) -> ReloadReport {
    ReloadReport {
        package_id,
        candidate_generation,
        old_epoch,
        new_epoch,
        source_hash,
        compile_duration,
        verify_duration,
        migration_duration: reload_duration,
        activation_duration: std::time::Duration::ZERO,
        cancelled_tasks,
        detached_requests,
        outcome,
    }
}

fn candidate_source_hash(candidate: &PackageCandidate) -> SourceHash {
    SourceHash(nexa_core::StableId::from_parts(&[
        &candidate.manifest_source,
        "\0",
        &candidate.entry_source,
    ]))
}

fn development_event_data(
    package_id: PackageId,
    candidate_generation: u64,
    source_hash: SourceHash,
    diagnostic: Option<EngineDiagnostic>,
) -> DevelopmentEventData {
    DevelopmentEventData {
        package_id,
        candidate_generation,
        source_hash,
        diagnostic,
        reload: None,
        queue_duration: None,
    }
}

fn runtime_trap_diagnostic(
    record: &PackageRecord,
    contract: &HostContract,
    idl: &nexa_idl::Idl,
    trap: &nexa_runtime::Trap,
    export: &str,
) -> EngineDiagnostic {
    let Some(runtime) = &record.runtime else {
        return EngineDiagnostic::without_source(
            Some(record.candidate.manifest.id.clone()),
            Some(record.source_id.clone()),
            EngineDiagnosticStage::Runtime,
            nexa::ErrorCode::NX5001,
            trap.message.to_string(),
        );
    };
    let leaf = trap.source_span.map_or_else(
        || {
            nexa::Diagnostic::without_source(
                nexa::ErrorCode::new(trap.diagnostic_code()),
                nexa::Severity::Error,
                nexa_runtime::RuntimeMessage::inline(&trap.message.to_string()),
            )
        },
        |span| {
            nexa::Diagnostic::from_parts(
                nexa::ErrorCode::new(trap.diagnostic_code()),
                nexa::Severity::Error,
                nexa_runtime::RuntimeMessage::inline(&trap.message.to_string()),
                nexa::Label {
                    span,
                    message: nexa_runtime::RuntimeMessage::Static("runtime trap"),
                },
            )
        },
    );
    let mut diagnostic = EngineDiagnostic::from_leaf(
        Some(record.candidate.manifest.id.clone()),
        Some(record.source_id.clone()),
        EngineDiagnosticStage::Runtime,
        leaf,
        Some(&runtime.artifact.source_files),
    );
    diagnostic.context.export = Some(export.to_owned());
    diagnostic.context.module_epoch = trap.epoch;
    diagnostic.context.task = trap.task.map(|task| EngineTaskId(u64::from(task.index)));
    for frame in trap.script_call_stack.as_slice() {
        let name = runtime
            .artifact
            .debug_info
            .functions
            .iter()
            .find(|function| function.function_index == frame.function)
            .map_or_else(
                || format!("<function #{}>", frame.function),
                |function| function.name.clone(),
            );
        diagnostic.related.push(RelatedDiagnostic {
            message: format!("at {name}"),
            file: frame
                .source_span
                .and_then(|span| runtime.artifact.source_files.file(span.file))
                .cloned(),
            span: frame.source_span,
        });
    }
    if let Some(boundary) = trap.host_call_boundary {
        let function = idl.functions.get(boundary.import as usize);
        let function_name = function.map_or_else(
            || format!("<host import #{}>", boundary.import),
            |function| format!("{}::{}", contract.interface_name, function.name),
        );
        diagnostic.related.push(RelatedDiagnostic {
            message: format!("while calling Host function {function_name}"),
            file: boundary
                .source_span
                .and_then(|span| runtime.artifact.source_files.file(span.file))
                .cloned(),
            span: boundary.source_span,
        });
        if let Some(function) = function
            && let Ok(registry) = SourceFileRegistry::from_files([(
                format!("{}.nidl", contract.interface_name.to_ascii_lowercase()),
                contract.canonical_idl.to_owned(),
            )])
            && let Some(file) = registry.files().next().cloned()
        {
            let start = contract
                .canonical_idl
                .find(&function.name)
                .unwrap_or_default();
            let end = start.saturating_add(function.name.len());
            diagnostic.related.push(RelatedDiagnostic {
                message: format!("Host function {function_name} is declared here"),
                span: Some(nexa_core::SourceSpan::new(
                    file.id,
                    u32::try_from(start).unwrap_or(u32::MAX),
                    u32::try_from(end).unwrap_or(u32::MAX),
                )),
                file: Some(file),
            });
        }
    }
    diagnostic
}

pub mod prelude {
    pub use crate::{
        DirectorySource, EngineError, EngineHealth, MemorySource, NexaEngine, NexaEngineBuilder,
        PackageId, PackageOutput, PackagePolicy, PackageStatus, TrustLevel,
    };
}
