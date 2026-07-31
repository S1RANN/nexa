use std::sync::Arc;

use nexa_analysis::{
    CompilationLimits, LockFile, NormalizedPackagePath, PackageManifest, SourceRole,
    SourceSetBuilder, validate_module_source,
};

use crate::manifest::SourceId;
use crate::policy::PackagePolicy;
use crate::source::{
    CandidateBuildContext, DiscoveredPackage, PackageSource, PackageSourceError,
    ResolvedSourcePackage, resolve_application_candidates,
};

/// One complete in-memory schema-2 package.
///
/// `directory` is the package-source-relative path used by local path dependencies. Source paths
/// are package-relative (`src/foo/bar.nexa`) and may be added in any order.
#[derive(Clone, Debug)]
pub struct MemoryPackage {
    directory: String,
    manifest: String,
    sources: Vec<(String, String)>,
    lock: Option<String>,
}

impl MemoryPackage {
    #[must_use]
    pub fn new(directory: impl Into<String>, manifest: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            manifest: manifest.into(),
            sources: Vec::new(),
            lock: None,
        }
    }

    #[must_use]
    pub fn source(mut self, path: impl Into<String>, text: impl Into<String>) -> Self {
        self.sources.push((path.into(), text.into()));
        self
    }

    #[must_use]
    pub fn lock(mut self, lock: impl Into<String>) -> Self {
        self.lock = Some(lock.into());
        self
    }
}

pub struct MemorySource {
    id: SourceId,
    policy: PackagePolicy,
    packages: Vec<MemoryPackage>,
    compilation_limits: CompilationLimits,
}

impl MemorySource {
    #[must_use]
    pub fn new(id: SourceId, policy: PackagePolicy) -> Self {
        Self {
            id,
            policy,
            packages: Vec::new(),
            compilation_limits: CompilationLimits::default(),
        }
    }

    #[must_use]
    pub fn package(mut self, package: MemoryPackage) -> Self {
        self.packages.push(package);
        self
    }

    #[must_use]
    pub const fn with_compilation_limits(mut self, limits: CompilationLimits) -> Self {
        self.compilation_limits = limits;
        self
    }
}

impl PackageSource for MemorySource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.policy
    }

    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
        if self.packages.len() > self.policy.max_packages {
            return Err(PackageSourceError::TooManyPackages);
        }
        let mut packages = Vec::with_capacity(self.packages.len());
        for package in &self.packages {
            let directory = NormalizedPackagePath::new(&package.directory)?;
            let manifest = Arc::new(PackageManifest::parse(&package.manifest)?);
            let mut sources = SourceSetBuilder::new(manifest.id.clone(), self.compilation_limits);
            for (path, text) in &package.sources {
                let path = NormalizedPackagePath::new(path)?;
                validate_module_source(&path, text).map_err(PackageSourceError::PackageLoad)?;
                sources.add(path, text.clone(), SourceRole::Production)?;
            }
            let source_set = Arc::new(sources.build()?);
            let lock = package
                .lock
                .as_deref()
                .map(LockFile::parse)
                .transpose()?
                .map(Arc::new);
            packages.push(ResolvedSourcePackage {
                directory,
                manifest,
                source_set,
                lock,
            });
        }
        resolve_application_candidates(
            &self.id,
            &self.policy,
            packages,
            self.compilation_limits,
            build,
        )
    }
}
