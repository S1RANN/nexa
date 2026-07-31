use std::path::PathBuf;

use nexa_analysis::{CompilationLimits, NormalizedPackagePath, load_package_directory};

use crate::manifest::SourceId;
use crate::policy::PackagePolicy;
use crate::source::{
    CandidateBuildContext, DiscoveredPackage, PackageSource, PackageSourceError,
    ResolvedSourcePackage, resolve_application_candidates,
};

pub struct DirectorySource {
    id: SourceId,
    root: PathBuf,
    policy: PackagePolicy,
    compilation_limits: CompilationLimits,
}

impl DirectorySource {
    #[must_use]
    pub fn new(id: SourceId, root: impl Into<PathBuf>, policy: PackagePolicy) -> Self {
        Self {
            id,
            root: root.into(),
            policy,
            compilation_limits: CompilationLimits::default(),
        }
    }

    #[must_use]
    pub const fn with_compilation_limits(mut self, limits: CompilationLimits) -> Self {
        self.compilation_limits = limits;
        self
    }
}

impl PackageSource for DirectorySource {
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
        let root = self.root.canonicalize()?;
        let mut directories = Vec::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(PackageSourceError::EscapedRoot);
            }
            if file_type.is_dir() {
                directories.push(entry.path());
            }
        }
        directories.sort();
        if directories.len() > self.policy.max_packages {
            return Err(PackageSourceError::TooManyPackages);
        }

        let mut packages = Vec::with_capacity(directories.len());
        for directory in directories {
            let package_root = directory.canonicalize()?;
            if !package_root.starts_with(&root) {
                return Err(PackageSourceError::EscapedRoot);
            }
            let relative = package_root
                .strip_prefix(&root)
                .map_err(|_| PackageSourceError::EscapedRoot)?;
            let directory =
                NormalizedPackagePath::from_path(relative).map_err(PackageSourceError::Identity)?;
            let loaded = load_package_directory(&package_root, self.compilation_limits)?;
            packages.push(ResolvedSourcePackage {
                directory,
                manifest: loaded.manifest,
                source_set: loaded.production_sources,
                lock: loaded.lock,
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
