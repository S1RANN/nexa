use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::contract::{ExportRequirement, HostContract};
use crate::development::DevelopmentConfig;
use crate::entitlement::{EntitlementResolver, NoEntitlements};
use crate::manifest::SourceId;
use crate::source::PackageSource;
use crate::{EngineError, HostRegistryFactory, NexaEngine};

pub struct NexaEngineBuilder {
    pub(crate) contract: HostContract,
    pub(crate) host_factory: Option<Box<dyn HostRegistryFactory>>,
    pub(crate) sources: Vec<Box<dyn PackageSource>>,
    pub(crate) entitlements: Box<dyn EntitlementResolver>,
    pub(crate) storage_dir: Option<PathBuf>,
    pub(crate) runtime_host_capacity: usize,
    pub(crate) runtime_host: Option<nexa::prelude::RuntimeHost>,
    pub(crate) development: DevelopmentConfig,
    pub(crate) required_exports: Vec<ExportRequirement>,
    pub(crate) host_contract_source: Option<(nexa::SourceIdentity, Arc<str>)>,
}

impl NexaEngineBuilder {
    pub(crate) fn new(contract: HostContract) -> Self {
        Self {
            contract,
            host_factory: None,
            sources: Vec::new(),
            entitlements: Box::<NoEntitlements>::default(),
            storage_dir: None,
            runtime_host_capacity: 16_384,
            runtime_host: None,
            development: DevelopmentConfig {
                enabled: false,
                ..DevelopmentConfig::default()
            },
            required_exports: Vec::new(),
            host_contract_source: None,
        }
    }

    #[must_use]
    pub fn host_factory(mut self, factory: impl HostRegistryFactory + 'static) -> Self {
        self.host_factory = Some(Box::new(factory));
        self
    }

    #[must_use]
    pub fn package_source(mut self, source: impl PackageSource + 'static) -> Self {
        self.sources.push(Box::new(source));
        self
    }

    #[must_use]
    pub fn entitlements(mut self, resolver: impl EntitlementResolver + 'static) -> Self {
        self.entitlements = Box::new(resolver);
        self
    }

    #[must_use]
    pub fn storage_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.storage_dir = Some(path.into());
        self
    }

    #[must_use]
    pub const fn runtime_host_capacity(mut self, capacity: usize) -> Self {
        self.runtime_host_capacity = capacity;
        self
    }

    pub(crate) fn runtime_host_for_evidence(
        mut self,
        runtime_host: nexa::prelude::RuntimeHost,
    ) -> Self {
        self.runtime_host = Some(runtime_host);
        self
    }

    #[must_use]
    pub fn development(mut self, config: DevelopmentConfig) -> Self {
        self.development = config;
        self
    }

    #[must_use]
    pub fn require_export<E: nexa::ScriptExport>(mut self) -> Self {
        self.required_exports.push(ExportRequirement::of::<E>());
        self
    }

    /// Uses the exact standalone `.nidl` source snapshot for fingerprints and compilation.
    ///
    /// [`Self::build`] rejects a source which is not standalone or does not parse to the
    /// generated [`HostContract`] supplied to [`crate::NexaEngine::builder`].
    #[must_use]
    pub fn host_contract_source(
        mut self,
        identity: nexa::SourceIdentity,
        text: impl Into<Arc<str>>,
    ) -> Self {
        self.host_contract_source = Some((identity, text.into()));
        self
    }

    pub fn build(self) -> Result<NexaEngine, EngineError> {
        if self.host_factory.is_none() {
            return Err(EngineError::MissingHostFactory);
        }
        let mut ids = BTreeSet::<SourceId>::new();
        for source in &self.sources {
            if !ids.insert(source.id().clone()) {
                return Err(EngineError::DuplicateSourceId(source.id().clone()));
            }
        }
        NexaEngine::from_builder(self)
    }
}
