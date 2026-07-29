use std::path::PathBuf;

use crate::manifest::SourceId;
use crate::policy::PackagePolicy;
use crate::source::{PackageCandidate, PackageSource, PackageSourceError};

pub struct DirectorySource {
    id: SourceId,
    root: PathBuf,
    policy: PackagePolicy,
}

impl DirectorySource {
    #[must_use]
    pub fn new(id: SourceId, root: impl Into<PathBuf>, policy: PackagePolicy) -> Self {
        Self {
            id,
            root: root.into(),
            policy,
        }
    }
}

impl PackageSource for DirectorySource {
    fn id(&self) -> &SourceId {
        &self.id
    }

    fn policy(&self) -> &PackagePolicy {
        &self.policy
    }

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        let root = self.root.canonicalize()?;
        let mut directories = std::fs::read_dir(&root)?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        directories.sort();
        if directories.len() > self.policy.max_packages {
            return Err(PackageSourceError::TooManyPackages);
        }
        let mut candidates = Vec::with_capacity(directories.len());
        for directory in directories {
            let package_root = directory.canonicalize()?;
            if !package_root.starts_with(&root) {
                return Err(PackageSourceError::EscapedRoot);
            }
            let manifest_path = package_root.join("package.toml");
            let manifest_source = std::fs::read_to_string(&manifest_path)?;
            let manifest = crate::manifest::PackageManifest::parse(&manifest_source, &self.policy)?;
            let entry = package_root.join(manifest.entry.as_path()).canonicalize()?;
            if !entry.starts_with(&package_root) {
                return Err(PackageSourceError::EscapedRoot);
            }
            let entry_source = std::fs::read_to_string(entry)?;
            candidates.push(PackageCandidate::new(
                manifest,
                manifest_source,
                entry_source,
            ));
        }
        Ok(candidates)
    }
}
