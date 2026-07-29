use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::change_scan::ChangeScanConfig;
use crate::contract::{ExportRequirement, HostContract};
use crate::entitlement::{EntitlementResolver, NoEntitlements};
use crate::manifest::SourceId;
use crate::source::PackageSource;
use crate::{EmbedError, HostRegistryFactory, NexaEmbed};

pub struct NexaEmbedBuilder {
    pub(crate) contract: HostContract,
    pub(crate) host_factory: Option<Box<dyn HostRegistryFactory>>,
    pub(crate) sources: Vec<Box<dyn PackageSource>>,
    pub(crate) entitlements: Box<dyn EntitlementResolver>,
    pub(crate) storage_dir: Option<PathBuf>,
    pub(crate) runtime_host_capacity: usize,
    pub(crate) development_mode: bool,
    pub(crate) change_scan: ChangeScanConfig,
    pub(crate) required_exports: Vec<ExportRequirement>,
}

impl NexaEmbedBuilder {
    pub(crate) fn new(contract: HostContract) -> Self {
        Self {
            contract,
            host_factory: None,
            sources: Vec::new(),
            entitlements: Box::<NoEntitlements>::default(),
            storage_dir: None,
            runtime_host_capacity: 16_384,
            development_mode: false,
            change_scan: ChangeScanConfig::default(),
            required_exports: Vec::new(),
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

    #[must_use]
    pub const fn development_mode(mut self, enabled: bool) -> Self {
        self.development_mode = enabled;
        self
    }

    #[must_use]
    pub const fn change_scan(mut self, config: ChangeScanConfig) -> Self {
        self.change_scan = config;
        self
    }

    #[must_use]
    pub fn require_export<E: nexa_runtime::ScriptExport>(mut self) -> Self {
        self.required_exports.push(ExportRequirement::of::<E>());
        self
    }

    pub fn build(self) -> Result<NexaEmbed, EmbedError> {
        if self.host_factory.is_none() {
            return Err(EmbedError::MissingHostFactory);
        }
        let mut ids = BTreeSet::<SourceId>::new();
        for source in &self.sources {
            if !ids.insert(source.id().clone()) {
                return Err(EmbedError::DuplicateSourceId(source.id().clone()));
            }
        }
        NexaEmbed::from_builder(self)
    }
}
