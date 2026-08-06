use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

pub use nexa_analysis::PackageCandidate;
use nexa_analysis::{
    CompilationLimits, CompilationOptions, DependencyGraphError, IdentityError, LockDrift,
    LockError, LockFile, NormalizedPackagePath, PackageCatalog, PackageLocation, PackageManifest,
    PackageSourceSet, ResolvedBuildInput, ResolvedBuildInputError, SourceSetError,
};

use crate::manifest::{ManifestError, SourceId, apply_package_policy};
use crate::policy::PackagePolicy;

/// Immutable compiler and Host inputs that participate in every package Build Fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateBuildContext {
    pub host_contract_source_identity: nexa::SourceIdentity,
    pub host_contract: Vec<u8>,
    /// Host-selected entrypoints which every enabled application must implement.
    ///
    /// The complete contract remains in `host_contract`; this list only selects the required
    /// subset for analysis and the canonical build fingerprint.
    pub required_entrypoints: Vec<String>,
}

impl CandidateBuildContext {
    #[must_use]
    pub fn new(host_contract: impl Into<Vec<u8>>) -> Self {
        let host_contract = host_contract.into();
        if let Ok(source) = std::str::from_utf8(&host_contract)
            && let Ok(idl) = nexa::parse_contract(source)
        {
            let contract = nexa::HostContractInput::canonical(&idl);
            return Self {
                host_contract_source_identity: contract.source().identity().clone(),
                host_contract: contract.source().text().as_bytes().to_vec(),
                required_entrypoints: Vec::new(),
            };
        }
        Self {
            host_contract_source_identity: nexa::SourceIdentity::standalone("host-contract.nidl"),
            host_contract,
            required_entrypoints: Vec::new(),
        }
    }

    /// Retains the exact standalone Host source identity and raw UTF-8 snapshot.
    #[must_use]
    pub fn with_source(
        host_contract_source_identity: nexa::SourceIdentity,
        host_contract: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            host_contract_source_identity,
            host_contract: host_contract.into(),
            required_entrypoints: Vec::new(),
        }
    }

    /// Selects the Host entrypoints which are required for this build.
    #[must_use]
    pub fn requiring_entrypoints(
        mut self,
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.required_entrypoints = names.into_iter().map(Into::into).collect();
        self
    }
}

pub trait PackageSource {
    fn id(&self) -> &SourceId;
    fn policy(&self) -> &PackagePolicy;
    fn discover(
        &self,
        build: &CandidateBuildContext,
    ) -> Result<Vec<DiscoveredPackage>, PackageSourceError>;
}

/// One application candidate paired with the exact immutable dependency/source snapshot that
/// must be compiled for it.
///
/// Keeping this separate from [`PackageCandidate`] preserves the canonical analysis candidate
/// shape while ensuring discovery, workers, freshness checks, and code generation all refer to
/// the same resolved bytes.
#[derive(Clone, Debug)]
pub struct DiscoveredPackage {
    pub candidate: PackageCandidate,
    pub build_input: Arc<ResolvedBuildInput>,
}

impl std::ops::Deref for DiscoveredPackage {
    type Target = PackageCandidate;

    fn deref(&self) -> &Self::Target {
        &self.candidate
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedSourcePackage {
    pub directory: NormalizedPackagePath,
    pub manifest: Arc<PackageManifest>,
    pub source_set: Arc<PackageSourceSet>,
    pub lock: Option<Arc<LockFile>>,
}

pub(crate) fn resolve_application_candidates(
    source_id: &SourceId,
    policy: &PackagePolicy,
    packages: Vec<ResolvedSourcePackage>,
    limits: CompilationLimits,
    build: &CandidateBuildContext,
) -> Result<Vec<DiscoveredPackage>, PackageSourceError> {
    if packages.len() > policy.max_packages {
        return Err(PackageSourceError::TooManyPackages);
    }

    let mut catalog = PackageCatalog::new();
    let mut by_directory = BTreeMap::new();
    for package in packages {
        catalog.insert(PackageLocation {
            source_id: source_id.clone(),
            directory: package.directory.clone(),
            manifest: Arc::clone(&package.manifest),
        })?;
        if by_directory
            .insert(package.directory.clone(), package)
            .is_some()
        {
            return Err(PackageSourceError::DuplicateDirectory);
        }
    }

    let mut applications = Vec::new();
    for root in by_directory
        .values()
        .filter(|package| package.manifest.is_application())
    {
        // Validation happens before a Realm is ever considered. Libraries deliberately never
        // receive lifecycle policy and are included only in the static closure below.
        apply_package_policy(&root.manifest, policy).map_err(PackageSourceError::Policy)?;

        let graph = Arc::new(catalog.resolve(source_id, &root.directory, limits)?);
        if graph.edges.is_empty() {
            if let Some(lock) = &root.lock {
                lock.verify(&graph)?;
            }
        } else {
            let lock = root
                .lock
                .as_ref()
                .ok_or_else(|| PackageSourceError::MissingLock(root.manifest.id.clone()))?;
            lock.verify(&graph)?;
        }

        let mut closure_sets = Vec::with_capacity(graph.packages.len());
        let mut dependency_manifests = BTreeMap::new();
        let mut dependency_source_sets = BTreeMap::new();
        for resolved in graph.packages.values() {
            let package = by_directory
                .get(&resolved.directory)
                .ok_or_else(|| PackageSourceError::IncompleteClosure(resolved.id.clone()))?;
            closure_sets.push(package.source_set.as_ref());
            if resolved.id != graph.root {
                dependency_manifests.insert(resolved.id.clone(), Arc::clone(&package.manifest));
                dependency_source_sets.insert(resolved.id.clone(), Arc::clone(&package.source_set));
            }
        }
        PackageSourceSet::validate_dependency_closure(closure_sets, limits)?;

        let host_source = std::str::from_utf8(&build.host_contract)
            .map_err(|error| PackageSourceError::HostContract(error.to_string()))?;
        let host_contract = nexa::parse_contract(host_source)
            .map_err(|error| PackageSourceError::HostContract(error.to_string()))?;
        let host_contract_input = nexa::HostContractInput::with_source(
            &host_contract,
            build.host_contract_source_identity.clone(),
            Arc::<str>::from(host_source),
        )
        .map_err(|error| PackageSourceError::HostContract(error.to_string()))?
        .requiring_entrypoints(&build.required_entrypoints)
        .map_err(|error| PackageSourceError::HostContract(error.to_string()))?;
        let fingerprint_input = nexa::canonical_package_build_fingerprint_input_with_contract(
            &root.manifest,
            &root.source_set,
            &dependency_manifests,
            &dependency_source_sets,
            &host_contract_input,
            root.lock.as_deref(),
        );
        let canonical_host_contract = fingerprint_input.host_contract.clone();
        let host_contract_source_identity = fingerprint_input.host_contract_source.clone();
        let build_input = Arc::new(ResolvedBuildInput::new(
            Arc::clone(&root.manifest),
            Arc::clone(&root.source_set),
            dependency_manifests,
            dependency_source_sets,
            Arc::clone(&graph),
            root.lock.clone(),
            canonical_host_contract,
            host_contract_source_identity,
            fingerprint_input.host_required_entrypoints.clone(),
            CompilationOptions::default(),
            fingerprint_input,
        )?);
        applications.push(DiscoveredPackage {
            candidate: build_input.candidate()?,
            build_input,
        });
    }
    applications
        .sort_by(|left, right| left.candidate.manifest.id.cmp(&right.candidate.manifest.id));
    Ok(applications)
}

#[derive(Debug)]
pub enum PackageSourceError {
    Io(std::io::Error),
    AnalysisManifest(nexa_analysis::ManifestError),
    PackageLoad(nexa_analysis::PackageLoadError),
    Identity(IdentityError),
    SourceSet(SourceSetError),
    DependencyGraph(DependencyGraphError),
    Lock(LockError),
    LockDrift(LockDrift),
    Candidate(nexa_analysis::CandidateError),
    ResolvedBuildInput(ResolvedBuildInputError),
    HostContract(String),
    Policy(ManifestError),
    MissingLock(nexa_analysis::PackageId),
    IncompleteClosure(nexa_analysis::PackageId),
    DuplicateDirectory,
    EscapedRoot,
    TooManyPackages,
}

impl PackageSourceError {
    #[must_use]
    pub const fn is_policy(&self) -> bool {
        matches!(
            self,
            Self::Policy(_) | Self::EscapedRoot | Self::TooManyPackages
        )
    }

    #[must_use]
    pub const fn is_manifest(&self) -> bool {
        matches!(
            self,
            Self::AnalysisManifest(_)
                | Self::PackageLoad(nexa_analysis::PackageLoadError::Manifest(_))
        )
    }
}

impl fmt::Display for PackageSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::AnalysisManifest(error) => error.fmt(formatter),
            Self::PackageLoad(error) => error.fmt(formatter),
            Self::Identity(error) => error.fmt(formatter),
            Self::SourceSet(error) => error.fmt(formatter),
            Self::DependencyGraph(error) => error.fmt(formatter),
            Self::Lock(error) => error.fmt(formatter),
            Self::LockDrift(error) => error.fmt(formatter),
            Self::Candidate(error) => error.fmt(formatter),
            Self::ResolvedBuildInput(error) => error.fmt(formatter),
            Self::HostContract(error) => formatter.write_str(error),
            Self::Policy(error) => error.fmt(formatter),
            Self::MissingLock(package) => {
                write!(
                    formatter,
                    "package {package} has dependencies but no nexa.lock"
                )
            }
            Self::IncompleteClosure(package) => {
                write!(
                    formatter,
                    "resolved dependency closure is missing package {package}"
                )
            }
            Self::DuplicateDirectory => {
                formatter.write_str("package source contains a duplicate directory")
            }
            Self::EscapedRoot => formatter.write_str("package path escapes its source root"),
            Self::TooManyPackages => formatter.write_str("package source package limit exceeded"),
        }
    }
}

impl std::error::Error for PackageSourceError {}

macro_rules! source_error_from {
    ($from:ty, $variant:ident) => {
        impl From<$from> for PackageSourceError {
            fn from(error: $from) -> Self {
                Self::$variant(error)
            }
        }
    };
}

source_error_from!(std::io::Error, Io);
source_error_from!(nexa_analysis::ManifestError, AnalysisManifest);
source_error_from!(nexa_analysis::PackageLoadError, PackageLoad);
source_error_from!(IdentityError, Identity);
source_error_from!(SourceSetError, SourceSet);
source_error_from!(DependencyGraphError, DependencyGraph);
source_error_from!(LockError, Lock);
source_error_from!(LockDrift, LockDrift);
source_error_from!(nexa_analysis::CandidateError, Candidate);
source_error_from!(ResolvedBuildInputError, ResolvedBuildInput);
