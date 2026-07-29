//! High-level, package-oriented embedding API for trusted Nexa content.

mod builder;
mod capability;
mod change_scan;
mod contract;
mod diagnostic;
mod directory_source;
mod dispatch;
mod entitlement;
mod lifecycle;
mod manifest;
mod memory_source;
mod package;
mod persistence;
mod policy;
mod source;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

pub use builder::NexaEmbedBuilder;
pub use capability::CapabilitySet;
pub use change_scan::ChangeScanConfig;
pub use contract::{ExportRequirement, HostContract};
pub use diagnostic::{DiagnosticStage, PackageDiagnostic};
pub use directory_source::DirectorySource;
pub use entitlement::{EntitlementResolver, NoEntitlements, StaticEntitlements};
pub use lifecycle::{LifecycleError, PackageLifecycle, PackageStatus};
pub use manifest::{
    CapabilityId, EntitlementId, ManifestError, PackageId, PackageManifest, PackagePath,
    PackageVersion, SourceId,
};
pub use memory_source::MemorySource;
pub use package::{EmbedHealth, PackageInfo, PackageOutput};
pub use policy::{
    ActivationPolicy, ActivationSet, PackagePolicy, PackageRuntimeLimits, TrustLevel,
};
pub use source::{PackageCandidate, PackageSource, PackageSourceError};

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

pub struct NexaEmbed {
    contract: HostContract,
    idl: nexa_idl::Idl,
    host_factory: Box<dyn HostRegistryFactory>,
    sources: Vec<Box<dyn PackageSource>>,
    entitlements: Box<dyn EntitlementResolver>,
    storage_dir: Option<PathBuf>,
    runtime_host: nexa_runtime::RuntimeHost,
    packages: Vec<PackageRecord>,
    diagnostics: Vec<PackageDiagnostic>,
    required_exports: Vec<ExportRequirement>,
    persisted: BTreeMap<PackageId, bool>,
    development_mode: bool,
    change_scan: ChangeScanConfig,
    ticks: u64,
    next_realm_id: u32,
    delivered_release_records: u64,
    shutdown: bool,
}

impl NexaEmbed {
    #[must_use]
    pub fn builder(contract: HostContract) -> NexaEmbedBuilder {
        NexaEmbedBuilder::new(contract)
    }

    pub(crate) fn from_builder(builder: NexaEmbedBuilder) -> Result<Self, EmbedError> {
        let idl = nexa_idl::parse(builder.contract.canonical_idl)
            .map_err(|error| EmbedError::Contract(error.to_string()))?;
        if nexa_idl::exact_hash(&idl) != builder.contract.interface_hash {
            return Err(EmbedError::Contract(
                "generated interface hash mismatch".into(),
            ));
        }
        let persisted = builder
            .storage_dir
            .as_deref()
            .map(persistence::load)
            .transpose()
            .map_err(|error| EmbedError::Persistence(error.to_string()))?
            .unwrap_or_default();
        Ok(Self {
            contract: builder.contract,
            idl,
            host_factory: builder.host_factory.ok_or(EmbedError::MissingHostFactory)?,
            sources: builder.sources,
            entitlements: builder.entitlements,
            storage_dir: builder.storage_dir,
            runtime_host: nexa_runtime::RuntimeHost::new(builder.runtime_host_capacity),
            packages: Vec::new(),
            diagnostics: Vec::new(),
            required_exports: builder.required_exports,
            persisted,
            development_mode: builder.development_mode,
            change_scan: builder.change_scan,
            ticks: 0,
            next_realm_id: 1,
            delivered_release_records: 0,
            shutdown: false,
        })
    }

    pub fn discover(&mut self) -> Result<Vec<PackageInfo>, EmbedError> {
        self.require_open()?;
        let mut records = Vec::new();
        for source in &self.sources {
            let source_id = source.id().clone();
            let candidates = match source.discover() {
                Ok(candidates) => candidates,
                Err(error) => {
                    let message = error.to_string();
                    self.diagnostics.push(PackageDiagnostic {
                        package_id: None,
                        source_id: source_id.clone(),
                        stage: DiagnosticStage::Source,
                        message: message.clone(),
                        source_start: None,
                        source_end: None,
                    });
                    return Err(EmbedError::Source {
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
                    last_error: None,
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
                record.last_error = Some("duplicate package id".into());
            }
        }
        self.packages = records;
        Ok(self.packages())
    }

    pub fn enable_defaults(&mut self) -> Result<(), EmbedError> {
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

    pub fn enable(&mut self, id: &PackageId) -> Result<(), EmbedError> {
        self.require_open()?;
        let index = self.unique_index(id)?;
        match self.packages[index].lifecycle.status() {
            PackageStatus::Enabled => return Ok(()),
            PackageStatus::Locked => return Err(EmbedError::Locked(id.clone())),
            PackageStatus::Incompatible => return Err(EmbedError::Incompatible(id.clone())),
            PackageStatus::Discovered | PackageStatus::Disabled | PackageStatus::Faulted => {}
            status => return Err(EmbedError::InvalidState(id.clone(), status)),
        }
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Enabling)?;
        match self.fresh_candidate(index) {
            Ok(candidate) => self.packages[index].candidate = candidate,
            Err(error) => {
                self.packages[index].last_error = Some(error.to_string());
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Faulted)?;
                self.record_error(index, &error);
                return Err(error);
            }
        }
        let result = self.build_runtime(index);
        match result {
            Ok(runtime) => {
                self.packages[index].runtime = Some(runtime);
                self.packages[index].last_error = None;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                self.persisted.insert(id.clone(), true);
                self.persist_selections()?;
                Ok(())
            }
            Err(error) => {
                self.packages[index].runtime = None;
                self.packages[index].last_error = Some(error.to_string());
                let next = if matches!(
                    error,
                    EmbedError::MissingExport(_, _) | EmbedError::ExportSignature(_, _)
                ) {
                    PackageStatus::Incompatible
                } else {
                    PackageStatus::Faulted
                };
                self.packages[index].lifecycle.transition(next)?;
                self.record_error(index, &error);
                self.drain_releases();
                Err(error)
            }
        }
    }

    fn fresh_candidate(&self, index: usize) -> Result<PackageCandidate, EmbedError> {
        let record = &self.packages[index];
        let source = self
            .sources
            .iter()
            .find(|source| source.id() == &record.source_id)
            .ok_or_else(|| EmbedError::Source {
                source: record.source_id.clone(),
                message: "package source is no longer registered".into(),
            })?;
        source
            .discover()
            .map_err(|error| EmbedError::Source {
                source: source.id().clone(),
                message: error.to_string(),
            })?
            .into_iter()
            .find(|candidate| candidate.manifest.id == record.candidate.manifest.id)
            .ok_or_else(|| EmbedError::Source {
                source: source.id().clone(),
                message: format!("package {} disappeared", record.candidate.manifest.id),
            })
    }

    fn build_runtime(&mut self, index: usize) -> Result<PackageRuntime, EmbedError> {
        let record = &self.packages[index];
        let manifest = &record.candidate.manifest;
        let schema_hash =
            nexa_core::StableId::from_parts(&[manifest.id.as_str(), "::", &manifest.state_schema]);
        let verified = nexa_compiler::compile_with_interface(
            &record.candidate.entry_source,
            &self.idl,
            schema_hash,
        )
        .map_err(|error| EmbedError::Compile(manifest.id.clone(), error.to_string()))?;
        self.validate_exports(manifest.id.clone(), &verified)?;
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
            .ok_or(EmbedError::RealmIdExhausted)?;
        let mut realm =
            nexa_runtime::RealmRuntime::hosted(config, self.runtime_host.clone(), registry)
                .map_err(|error| EmbedError::Load(manifest.id.clone(), error.to_string()))?;
        let module = realm
            .load_module(verified, self.contract.interface_hash, schema_hash)
            .map_err(|error| EmbedError::Load(manifest.id.clone(), error.to_string()))?;
        let root_scope = realm
            .create_scope(None)
            .map_err(|error| EmbedError::Load(manifest.id.clone(), error.to_string()))?;
        Ok(PackageRuntime {
            realm,
            module,
            root_scope,
        })
    }

    fn validate_exports(
        &self,
        id: PackageId,
        verified: &nexa_verifier::VerifiedModule,
    ) -> Result<(), EmbedError> {
        for requirement in &self.required_exports {
            let Some(found) = verified
                .module()
                .exports
                .iter()
                .find(|export| export.stable_id == requirement.stable_id)
            else {
                return Err(EmbedError::MissingExport(id, requirement.name));
            };
            if found.signature != requirement.signature {
                return Err(EmbedError::ExportSignature(id, requirement.name));
            }
        }
        Ok(())
    }

    pub fn disable(&mut self, id: &PackageId) -> Result<(), EmbedError> {
        let index = self.unique_index(id)?;
        if self.packages[index].candidate.manifest.activation == ActivationPolicy::Required {
            return Err(EmbedError::RequiredPackage(id.clone()));
        }
        self.disable_index(index, true)
    }

    pub fn fault(&mut self, id: &PackageId, message: impl Into<String>) -> Result<(), EmbedError> {
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
            return Err(EmbedError::InvalidState(id.clone(), status));
        }
        self.packages[index].last_error = Some(message);
        self.record_diagnostic(index, DiagnosticStage::HandlerTrap, None);
        self.drain_releases();
        Ok(())
    }

    fn disable_index(&mut self, index: usize, persist: bool) -> Result<(), EmbedError> {
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

    pub fn reload(&mut self, id: &PackageId) -> Result<(), EmbedError> {
        let index = self.unique_index(id)?;
        if self.packages[index].lifecycle.status() != PackageStatus::Enabled {
            return Err(EmbedError::InvalidState(
                id.clone(),
                self.packages[index].lifecycle.status(),
            ));
        }
        let candidate = self.fresh_candidate(index)?;
        self.reload_candidate(index, candidate)
    }

    fn reload_candidate(
        &mut self,
        index: usize,
        candidate: PackageCandidate,
    ) -> Result<(), EmbedError> {
        let id = self.packages[index].candidate.manifest.id.clone();
        self.packages[index]
            .lifecycle
            .transition(PackageStatus::Reloading)?;
        let schema_hash =
            nexa_core::StableId::from_parts(&[id.as_str(), "::", &candidate.manifest.state_schema]);
        let verified = match nexa_compiler::compile_with_interface(
            &candidate.entry_source,
            &self.idl,
            schema_hash,
        ) {
            Ok(verified) => verified,
            Err(error) => {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                return Err(EmbedError::Reload(id, error.to_string()));
            }
        };
        if let Err(error) = self.validate_exports(id.clone(), &verified) {
            self.packages[index]
                .lifecycle
                .transition(PackageStatus::Enabled)?;
            self.packages[index].last_error = Some(error.to_string());
            self.record_error(index, &error);
            return Err(error);
        }
        let runtime = self.packages[index]
            .runtime
            .as_mut()
            .ok_or_else(|| EmbedError::InvalidState(id.clone(), PackageStatus::Reloading))?;
        match runtime.realm.restart_reload(
            runtime.module,
            verified,
            nexa_runtime::RestartReloadPolicy::default(),
        ) {
            Ok(nexa_runtime::RestartReloadOutcome::Committed(module)) => {
                runtime.module = module;
                self.packages[index].candidate = candidate;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                self.packages[index].last_error = None;
                self.drain_releases();
                Ok(())
            }
            Ok(nexa_runtime::RestartReloadOutcome::RolledBackBeforeCommit { reason, .. }) => {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                let error = EmbedError::Reload(id, reason.to_string());
                self.packages[index].last_error = Some(error.to_string());
                self.record_error(index, &error);
                self.drain_releases();
                Err(error)
            }
            Ok(nexa_runtime::RestartReloadOutcome::ActivationFaulted { error, .. }) => {
                self.packages[index].runtime = None;
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Faulted)?;
                let error = EmbedError::Reload(id, error.to_string());
                self.packages[index].last_error = Some(error.to_string());
                self.record_error(index, &error);
                self.drain_releases();
                Err(error)
            }
            Err(error) => {
                self.packages[index]
                    .lifecycle
                    .transition(PackageStatus::Enabled)?;
                let error = EmbedError::Reload(id, error.to_string());
                self.packages[index].last_error = Some(error.to_string());
                self.record_error(index, &error);
                self.drain_releases();
                Err(error)
            }
        }
    }

    pub fn reload_changed(&mut self) -> Result<usize, EmbedError> {
        let mut changed = Vec::new();
        for source in &self.sources {
            for candidate in source.discover().map_err(|error| EmbedError::Source {
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

    pub fn call<E: nexa_runtime::ScriptExport>(
        &mut self,
        id: &PackageId,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EmbedError> {
        let index = self.unique_index(id)?;
        self.call_index::<E>(index, args)
    }

    fn call_index<E: nexa_runtime::ScriptExport>(
        &mut self,
        index: usize,
        args: &E::Args,
    ) -> Result<PackageOutput<E::Output>, EmbedError> {
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EmbedError::InvalidState(
                record.candidate.manifest.id.clone(),
                record.lifecycle.status(),
            ));
        }
        let manifest = &record.candidate.manifest;
        let policy = nexa_runtime::MustCompletePolicy {
            fuel: manifest.handler_fuel,
            cumulative_budget: manifest.cumulative_budget,
        };
        let runtime = record
            .runtime
            .as_mut()
            .ok_or_else(|| EmbedError::InvalidState(manifest.id.clone(), PackageStatus::Enabled))?;
        match runtime
            .realm
            .call_export::<E>(runtime.module, runtime.root_scope, args, policy)
        {
            Ok(value) => Ok(PackageOutput {
                package_id: manifest.id.clone(),
                source_id: record.source_id.clone(),
                trust: record.policy.trust,
                capabilities: manifest.capabilities.clone(),
                value,
            }),
            Err(call_error) => {
                let stage = match call_error {
                    nexa_runtime::ScriptCallError::HandlerDidNotComplete => {
                        DiagnosticStage::HandlerYield
                    }
                    nexa_runtime::ScriptCallError::HostWaitNotAllowed => {
                        DiagnosticStage::HandlerWait
                    }
                    _ => DiagnosticStage::HandlerTrap,
                };
                let error = EmbedError::Handler(manifest.id.clone(), call_error.to_string());
                record.last_error = Some(error.to_string());
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
                self.record_diagnostic(index, stage, Some(error.to_string()));
                self.drain_releases();
                Err(error)
            }
        }
    }

    pub fn dispatch<E: nexa_runtime::ScriptExport>(
        &mut self,
        args: &E::Args,
    ) -> Vec<Result<PackageOutput<E::Output>, EmbedError>> {
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
    ) -> Vec<Result<PackageOutput<E::Output>, EmbedError>> {
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
    ) -> Result<(), EmbedError> {
        let index = self.unique_index(id)?;
        let record = &mut self.packages[index];
        if record.lifecycle.status() != PackageStatus::Enabled {
            return Err(EmbedError::InvalidState(
                id.clone(),
                record.lifecycle.status(),
            ));
        }
        let runtime = record
            .runtime
            .as_mut()
            .ok_or_else(|| EmbedError::InvalidState(id.clone(), PackageStatus::Enabled))?;
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
            .map_err(|error| EmbedError::State(id.clone(), error.to_string()))
    }

    /// Reads one scalar field from a package's typed state domain.
    pub fn state_i32(
        &self,
        id: &PackageId,
        state_key: &str,
        type_name: &str,
        field_name: &str,
    ) -> Result<Option<i32>, EmbedError> {
        let index = self.unique_index(id)?;
        let record = &self.packages[index];
        let Some(runtime) = &record.runtime else {
            return Ok(None);
        };
        let stable_id = nexa_core::StableId::from_name(state_key);
        let Some(handle) = runtime
            .realm
            .state_handles(runtime.module)
            .map_err(|error| EmbedError::State(id.clone(), error.to_string()))?
            .into_iter()
            .find(|handle| handle.stable_id() == stable_id)
        else {
            return Ok(None);
        };
        let value = runtime
            .realm
            .resolve_state(runtime.module, handle)
            .map_err(|error| EmbedError::State(id.clone(), error.to_string()))?;
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

    pub fn tick(&mut self) -> Result<(), EmbedError> {
        self.require_open()?;
        self.ticks = self.ticks.saturating_add(1);
        for record in &mut self.packages {
            if let Some(runtime) = record.runtime.as_mut()
                && let Err(error) = runtime.realm.tick(nexa_runtime::TickBudget {
                    collect_garbage: true,
                    ..nexa_runtime::TickBudget::default()
                })
            {
                record.last_error = Some(error.to_string());
                record.runtime = None;
                record.lifecycle.transition(PackageStatus::Faulted)?;
            }
        }
        self.drain_releases();
        if self.development_mode
            && self.change_scan.interval_ticks != 0
            && self.ticks.is_multiple_of(self.change_scan.interval_ticks)
        {
            let _ = self.reload_changed();
        }
        Ok(())
    }

    pub fn refresh_entitlements(&mut self) -> Result<(), EmbedError> {
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
    pub fn diagnostics(&self) -> &[PackageDiagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn health(&self) -> EmbedHealth {
        let mut health = EmbedHealth {
            delivered_release_records: self.delivered_release_records,
            ..EmbedHealth::default()
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

    pub fn shutdown(&mut self) -> Result<(), EmbedError> {
        if self.shutdown {
            return Ok(());
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
            .map_err(|error| EmbedError::Shutdown(error.to_string()))?;
        self.shutdown = true;
        Ok(())
    }

    fn unique_index(&self, id: &PackageId) -> Result<usize, EmbedError> {
        let indexes = self
            .packages
            .iter()
            .enumerate()
            .filter(|(_, record)| record.candidate.manifest.id == *id)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        match indexes.as_slice() {
            [index] => Ok(*index),
            [] => Err(EmbedError::UnknownPackage(id.clone())),
            _ => Err(EmbedError::Incompatible(id.clone())),
        }
    }

    fn persist_selections(&self) -> Result<(), EmbedError> {
        self.storage_dir.as_deref().map_or(Ok(()), |directory| {
            persistence::save(directory, &self.persisted)
                .map_err(|error| EmbedError::Persistence(error.to_string()))
        })
    }

    fn drain_releases(&mut self) {
        let count = self.runtime_host.drain_releases().len();
        self.delivered_release_records = self
            .delivered_release_records
            .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }

    fn record_error(&mut self, index: usize, error: &EmbedError) {
        let stage = match error {
            EmbedError::Source { .. } => DiagnosticStage::Source,
            EmbedError::Persistence(_) => DiagnosticStage::Persistence,
            EmbedError::Locked(_) => DiagnosticStage::Entitlement,
            EmbedError::Compile(_, _) => DiagnosticStage::Compile,
            EmbedError::Load(_, _) => DiagnosticStage::Load,
            EmbedError::MissingExport(_, _) | EmbedError::ExportSignature(_, _) => {
                DiagnosticStage::Export
            }
            EmbedError::Handler(_, _) => DiagnosticStage::HandlerTrap,
            EmbedError::Reload(_, _) => DiagnosticStage::Reload,
            _ => DiagnosticStage::Policy,
        };
        self.record_diagnostic(index, stage, Some(error.to_string()));
    }

    fn record_diagnostic(&mut self, index: usize, stage: DiagnosticStage, message: Option<String>) {
        let record = &self.packages[index];
        let message = message
            .or_else(|| record.last_error.clone())
            .unwrap_or_else(|| format!("{stage:?}"));
        let (source_start, source_end) = diagnostic_source_range(&message);
        self.diagnostics.push(PackageDiagnostic {
            package_id: Some(record.candidate.manifest.id.clone()),
            source_id: record.source_id.clone(),
            stage,
            message,
            source_start,
            source_end,
        });
    }

    fn require_open(&self) -> Result<(), EmbedError> {
        if self.shutdown {
            Err(EmbedError::Shutdown("embed is already shut down".into()))
        } else {
            Ok(())
        }
    }
}

fn diagnostic_source_range(message: &str) -> (Option<usize>, Option<usize>) {
    let parse_after = |needle: &str| {
        message
            .split_once(needle)
            .and_then(|(_, tail)| {
                tail.split(|character: char| !character.is_ascii_digit())
                    .next()
            })
            .and_then(|digits| digits.parse().ok())
    };
    (parse_after("start: "), parse_after("end: "))
}

impl Drop for NexaEmbed {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug)]
pub enum EmbedError {
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
    Compile(PackageId, String),
    Load(PackageId, String),
    MissingExport(PackageId, &'static str),
    ExportSignature(PackageId, &'static str),
    Handler(PackageId, String),
    State(PackageId, String),
    Reload(PackageId, String),
    RealmIdExhausted,
    Shutdown(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for EmbedError {}

impl From<LifecycleError> for EmbedError {
    fn from(error: LifecycleError) -> Self {
        Self::Lifecycle(error)
    }
}
