use crate::manifest::SourceId;
use crate::policy::PackagePolicy;
use crate::source::{PackageCandidate, PackageSource, PackageSourceError};

pub struct MemorySource {
    id: SourceId,
    policy: PackagePolicy,
    packages: Vec<(String, String)>,
}

impl MemorySource {
    #[must_use]
    pub fn new(id: SourceId, policy: PackagePolicy) -> Self {
        Self {
            id,
            policy,
            packages: Vec::new(),
        }
    }

    #[must_use]
    pub fn package(mut self, manifest: impl Into<String>, source: impl Into<String>) -> Self {
        self.packages.push((manifest.into(), source.into()));
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

    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError> {
        if self.packages.len() > self.policy.max_packages {
            return Err(PackageSourceError::TooManyPackages);
        }
        self.packages
            .iter()
            .map(|(manifest_source, entry_source)| {
                let manifest =
                    crate::manifest::PackageManifest::parse(manifest_source, &self.policy)?;
                Ok(PackageCandidate::new(
                    manifest,
                    manifest_source.clone(),
                    entry_source.clone(),
                ))
            })
            .collect()
    }
}
