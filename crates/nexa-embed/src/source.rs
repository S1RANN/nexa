use std::fmt;

use crate::manifest::{PackageManifest, SourceId};
use crate::policy::PackagePolicy;

#[derive(Clone, Debug)]
pub struct PackageCandidate {
    pub manifest: PackageManifest,
    pub manifest_source: String,
    pub entry_source: String,
    pub manifest_hash: nexa_core::StableId,
    pub entry_hash: nexa_core::StableId,
}

impl PackageCandidate {
    #[must_use]
    pub fn new(manifest: PackageManifest, manifest_source: String, entry_source: String) -> Self {
        Self {
            manifest_hash: nexa_core::StableId::from_name(&manifest_source),
            entry_hash: nexa_core::StableId::from_name(&entry_source),
            manifest,
            manifest_source,
            entry_source,
        }
    }
}

pub trait PackageSource {
    fn id(&self) -> &SourceId;
    fn policy(&self) -> &PackagePolicy;
    fn discover(&self) -> Result<Vec<PackageCandidate>, PackageSourceError>;
}

#[derive(Debug)]
pub enum PackageSourceError {
    Io(std::io::Error),
    Manifest(crate::manifest::ManifestError),
    EscapedRoot,
    TooManyPackages,
}

impl fmt::Display for PackageSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PackageSourceError {}

impl From<std::io::Error> for PackageSourceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<crate::manifest::ManifestError> for PackageSourceError {
    fn from(error: crate::manifest::ManifestError) -> Self {
        Self::Manifest(error)
    }
}
