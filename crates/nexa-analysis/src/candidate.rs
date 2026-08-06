use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    ArtifactFileTable, BuildFingerprint, BuildFingerprintInput, CompilationLimits,
    DependencyGraphError, LockFile, PackageId, PackageKind, PackageManifest, PackageSourceSet,
    ResolvedDependencyGraph, SourceSetError, source_set_fingerprint,
};

/// Shared freshness identity carried by every worker, artifact, runtime, and reload stage.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CandidateIdentity {
    pub package_id: PackageId,
    pub generation: u64,
    pub build_fingerprint: BuildFingerprint,
}

impl CandidateIdentity {
    pub fn new(
        package_id: PackageId,
        generation: u64,
        build_fingerprint: BuildFingerprint,
    ) -> Result<Self, CandidateError> {
        if generation == 0 {
            return Err(CandidateError::ZeroGeneration);
        }
        Ok(Self {
            package_id,
            generation,
            build_fingerprint,
        })
    }
}

/// Immutable, completely resolved input to package analysis and compilation.
#[derive(Clone, Debug)]
pub struct PackageCandidate {
    pub manifest: Arc<PackageManifest>,
    pub source_set: Arc<PackageSourceSet>,
    pub dependency_graph: Arc<ResolvedDependencyGraph>,
    pub build_fingerprint: BuildFingerprint,
}

/// One immutable, fully resolved source snapshot shared by analysis, compilation, tooling, and
/// runtime candidate construction.
///
/// The constructor deliberately accepts semantic objects rather than paths. Discovery, dependency
/// resolution, and lock validation must already have captured a stable snapshot before this type
/// can be created. The exact candidate-local file table is then derived from that snapshot, so no
/// consumer can assign conflicting numeric file identifiers.
#[derive(Clone, Debug)]
pub struct ResolvedBuildInput {
    pub root_manifest: Arc<PackageManifest>,
    pub root_source_set: Arc<PackageSourceSet>,
    pub dependency_manifests: Arc<BTreeMap<PackageId, Arc<PackageManifest>>>,
    pub dependency_source_sets: Arc<BTreeMap<PackageId, Arc<PackageSourceSet>>>,
    pub dependency_graph: Arc<ResolvedDependencyGraph>,
    pub artifact_files: Arc<ArtifactFileTable>,
    pub lock: Option<Arc<LockFile>>,
    pub canonical_lock_graph: Arc<[u8]>,
    pub canonical_host_contract: Arc<[u8]>,
    pub host_contract_source_identity: Arc<[u8]>,
    pub host_required_entrypoints_identity: Arc<[u8]>,
    /// Exact analysis/codegen options bound into `build_fingerprint`.
    pub compilation_options: crate::CompilationOptions,
    pub fingerprint_input: Arc<BuildFingerprintInput>,
    pub build_fingerprint: BuildFingerprint,
}

#[derive(Clone, Debug)]
pub struct ResolvedTestInput {
    pub product: Arc<ResolvedBuildInput>,
    pub test_source_set: Arc<PackageSourceSet>,
    pub artifact_files: Arc<ArtifactFileTable>,
}

impl ResolvedTestInput {
    pub fn new(
        product: Arc<ResolvedBuildInput>,
        test_source_set: Arc<PackageSourceSet>,
    ) -> Result<Self, ResolvedBuildInputError> {
        if test_source_set.package_id() != product.root_package() {
            return Err(ResolvedBuildInputError::TestSourcePackageMismatch {
                root: product.root_package().clone(),
                tests: test_source_set.package_id().clone(),
            });
        }
        if test_source_set.production_units().next().is_some() {
            return Err(ResolvedBuildInputError::ProductionSourceInTestSet);
        }
        test_source_set
            .validate_limits(product.compilation_options.limits)
            .map_err(ResolvedBuildInputError::SourceSet)?;
        let artifact_files =
            ArtifactFileTable::for_test_closure(product.all_source_sets(), &test_source_set)
                .map_err(ResolvedBuildInputError::SourceSet)?;
        Ok(Self {
            product,
            test_source_set,
            artifact_files: Arc::new(artifact_files),
        })
    }

    /// Rechecks every authority before a test snapshot crosses the compiler façade.
    pub fn validate_integrity(&self) -> Result<(), ResolvedBuildInputError> {
        self.product.validate_integrity()?;
        let rebuilt = Self::new(Arc::clone(&self.product), Arc::clone(&self.test_source_set))?;
        if self.artifact_files.as_ref() != rebuilt.artifact_files.as_ref() {
            return Err(ResolvedBuildInputError::ArtifactFileTableMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub fn source_snapshots(&self) -> Arc<nexa_diagnostics::SourceSnapshotRegistry> {
        let mut builder = nexa_diagnostics::SourceSnapshotRegistry::builder();
        for source_set in self
            .product
            .all_source_sets()
            .chain(std::iter::once(self.test_source_set.as_ref()))
        {
            for unit in source_set.units().values() {
                builder
                    .insert(
                        nexa_diagnostics::SourceIdentity::package(
                            unit.key.package_id.as_str(),
                            unit.key.path.as_str(),
                        ),
                        Arc::clone(&unit.text),
                    )
                    .expect("resolved source keys are unique");
            }
        }
        builder.build()
    }
}

impl ResolvedBuildInput {
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn new(
        root_manifest: Arc<PackageManifest>,
        root_source_set: Arc<PackageSourceSet>,
        dependency_manifests: BTreeMap<PackageId, Arc<PackageManifest>>,
        dependency_source_sets: BTreeMap<PackageId, Arc<PackageSourceSet>>,
        dependency_graph: Arc<ResolvedDependencyGraph>,
        lock: Option<Arc<LockFile>>,
        canonical_host_contract: impl Into<Arc<[u8]>>,
        host_contract_source_identity: impl Into<Arc<[u8]>>,
        host_required_entrypoints_identity: impl Into<Arc<[u8]>>,
        compilation_options: crate::CompilationOptions,
        mut fingerprint_input: BuildFingerprintInput,
    ) -> Result<Self, ResolvedBuildInputError> {
        if root_source_set.package_id() != &root_manifest.id {
            return Err(ResolvedBuildInputError::RootSourcePackageMismatch {
                manifest: root_manifest.id.clone(),
                source: root_source_set.package_id().clone(),
            });
        }
        if root_source_set.test_units().next().is_some() {
            return Err(ResolvedBuildInputError::ProductSourceContainsTest(
                root_manifest.id.clone(),
            ));
        }
        if dependency_graph.root != root_manifest.id {
            return Err(ResolvedBuildInputError::DependencyRootMismatch {
                manifest: root_manifest.id.clone(),
                graph: dependency_graph.root.clone(),
            });
        }
        let Some(resolved_root) = dependency_graph.package(&root_manifest.id) else {
            return Err(ResolvedBuildInputError::MissingResolvedRoot(
                root_manifest.id.clone(),
            ));
        };
        if resolved_root.version != root_manifest.version
            || resolved_root.kind != root_manifest.kind
        {
            return Err(ResolvedBuildInputError::RootResolutionMismatch);
        }
        validate_dependency_graph_structure(&dependency_graph, compilation_options.limits)?;

        let expected_dependencies = dependency_graph
            .packages
            .keys()
            .filter(|package| *package != &root_manifest.id)
            .cloned()
            .collect::<BTreeSet<_>>();
        let manifest_dependencies = dependency_manifests
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let source_dependencies = dependency_source_sets
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if manifest_dependencies != expected_dependencies {
            return Err(ResolvedBuildInputError::DependencyManifestSetMismatch {
                expected: expected_dependencies.clone(),
                actual: manifest_dependencies,
            });
        }
        if source_dependencies != expected_dependencies {
            return Err(ResolvedBuildInputError::DependencySourceSetMismatch {
                expected: expected_dependencies,
                actual: source_dependencies,
            });
        }

        for (package_id, manifest) in &dependency_manifests {
            if &manifest.id != package_id {
                return Err(
                    ResolvedBuildInputError::DependencyManifestIdentityMismatch {
                        key: package_id.clone(),
                        manifest: manifest.id.clone(),
                    },
                );
            }
            if manifest.kind != PackageKind::Library {
                return Err(ResolvedBuildInputError::DependencyIsNotLibrary(
                    package_id.clone(),
                ));
            }
            let Some(resolved) = dependency_graph.package(package_id) else {
                return Err(ResolvedBuildInputError::UnknownDependency(
                    package_id.clone(),
                ));
            };
            if resolved.version != manifest.version || resolved.kind != manifest.kind {
                return Err(ResolvedBuildInputError::DependencyResolutionMismatch(
                    package_id.clone(),
                ));
            }
        }
        for (package_id, source_set) in &dependency_source_sets {
            if source_set.package_id() != package_id {
                return Err(ResolvedBuildInputError::DependencySourceIdentityMismatch {
                    key: package_id.clone(),
                    source: source_set.package_id().clone(),
                });
            }
            if source_set.test_units().next().is_some() {
                return Err(ResolvedBuildInputError::ProductSourceContainsTest(
                    package_id.clone(),
                ));
            }
        }
        root_source_set
            .validate_limits(compilation_options.limits)
            .map_err(ResolvedBuildInputError::SourceSet)?;
        for source_set in dependency_source_sets.values() {
            source_set
                .validate_limits(compilation_options.limits)
                .map_err(ResolvedBuildInputError::SourceSet)?;
        }
        validate_dependency_edges(&root_manifest, &dependency_manifests, &dependency_graph)?;
        PackageSourceSet::validate_dependency_closure(
            std::iter::once(root_source_set.as_ref())
                .chain(dependency_source_sets.values().map(AsRef::as_ref)),
            compilation_options.limits,
        )
        .map_err(ResolvedBuildInputError::SourceSet)?;

        let has_dependencies = !dependency_graph.edges.is_empty();
        let canonical_lock_graph: Arc<[u8]> = match (has_dependencies, lock.as_deref()) {
            (true, None) => return Err(ResolvedBuildInputError::MissingLock),
            (_, Some(lock)) => {
                lock.verify(&dependency_graph)
                    .map_err(|_| ResolvedBuildInputError::LockDrift)?;
                Arc::from(lock.canonical_bytes())
            }
            (false, None) => Arc::from([]),
        };

        let artifact_files = ArtifactFileTable::for_closure(
            std::iter::once(root_source_set.as_ref())
                .chain(dependency_source_sets.values().map(AsRef::as_ref)),
        )
        .map_err(ResolvedBuildInputError::SourceSet)?;
        let canonical_host_contract = canonical_host_contract.into();
        let host_contract_source_identity = host_contract_source_identity.into();
        let host_required_entrypoints_identity = host_required_entrypoints_identity.into();
        let expected_root_manifest = root_manifest.canonical_bytes();
        let expected_dependency_manifests = dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect::<BTreeMap<_, _>>();
        let expected_dependency_source_sets = dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect::<BTreeMap<_, _>>();
        if fingerprint_input.root_package != root_manifest.id {
            return Err(ResolvedBuildInputError::FingerprintRootMismatch);
        }
        if fingerprint_input.root_manifest != expected_root_manifest {
            return Err(ResolvedBuildInputError::FingerprintRootManifestMismatch);
        }
        if fingerprint_input.root_source_set != source_set_fingerprint(&root_source_set) {
            return Err(ResolvedBuildInputError::FingerprintRootSourceMismatch);
        }
        if fingerprint_input.dependency_manifests != expected_dependency_manifests {
            return Err(ResolvedBuildInputError::FingerprintDependencyManifestMismatch);
        }
        if fingerprint_input.dependency_source_sets != expected_dependency_source_sets {
            return Err(ResolvedBuildInputError::FingerprintDependencySourceMismatch);
        }
        if fingerprint_input.host_contract.as_slice() != canonical_host_contract.as_ref() {
            return Err(ResolvedBuildInputError::FingerprintHostContractMismatch);
        }
        if fingerprint_input.host_contract_source.as_slice()
            != host_contract_source_identity.as_ref()
        {
            return Err(ResolvedBuildInputError::FingerprintHostContractSourceMismatch);
        }
        if fingerprint_input.host_required_entrypoints.as_slice()
            != host_required_entrypoints_identity.as_ref()
        {
            return Err(ResolvedBuildInputError::FingerprintHostRequiredEntrypointsMismatch);
        }
        if fingerprint_input.canonical_lock_graph.as_slice() != canonical_lock_graph.as_ref() {
            return Err(ResolvedBuildInputError::FingerprintLockMismatch);
        }
        if fingerprint_input.language_version != crate::NEXA_LANGUAGE_VERSION {
            return Err(ResolvedBuildInputError::FingerprintLanguageVersionMismatch);
        }
        let standard_library = nexa_stdlib::standard_library();
        if fingerprint_input.standard_library_version != standard_library.version.to_string() {
            return Err(ResolvedBuildInputError::FingerprintStandardLibraryVersionMismatch);
        }
        if fingerprint_input.standard_library_descriptor
            != nexa_stdlib::canonical_descriptor_identity()
        {
            return Err(ResolvedBuildInputError::FingerprintStandardLibraryDescriptorMismatch);
        }
        if fingerprint_input.compiler_version != nexa_core::NEXA_COMPILER_VERSION {
            return Err(ResolvedBuildInputError::FingerprintCompilerVersionMismatch);
        }
        if fingerprint_input.bytecode_version != u32::from(nexa_core::BYTECODE_VERSION) {
            return Err(ResolvedBuildInputError::FingerprintBytecodeVersionMismatch);
        }
        if fingerprint_input.runtime_semantics_version
            != u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION)
        {
            return Err(ResolvedBuildInputError::FingerprintRuntimeSemanticsVersionMismatch);
        }
        if fingerprint_input.opcode_cost_table_version != nexa_core::OPCODE_COST_TABLE_VERSION {
            return Err(ResolvedBuildInputError::FingerprintOpcodeCostTableVersionMismatch);
        }
        if fingerprint_input.deterministic_math_backend != nexa_core::RUNTIME_MATH_BACKEND_ID {
            return Err(ResolvedBuildInputError::FingerprintMathBackendMismatch);
        }
        if fingerprint_input.compiler_options
            != crate::canonical_compilation_options(&compilation_options)
        {
            return Err(ResolvedBuildInputError::FingerprintCompilationOptionsMismatch);
        }
        // Replace validated semantic byte fields with their canonical forms before hashing. This
        // prevents caller-controlled non-canonical encodings from creating a second identity.
        fingerprint_input.root_manifest = expected_root_manifest;
        fingerprint_input.dependency_manifests = expected_dependency_manifests;
        fingerprint_input.dependency_source_sets = expected_dependency_source_sets;
        fingerprint_input.host_contract = canonical_host_contract.to_vec();
        fingerprint_input.host_contract_source = host_contract_source_identity.to_vec();
        fingerprint_input.host_required_entrypoints = host_required_entrypoints_identity.to_vec();
        fingerprint_input.canonical_lock_graph = canonical_lock_graph.to_vec();
        let build_fingerprint = fingerprint_input.fingerprint();
        let fingerprint_input = Arc::new(fingerprint_input);

        Ok(Self {
            root_manifest,
            root_source_set,
            dependency_manifests: Arc::new(dependency_manifests),
            dependency_source_sets: Arc::new(dependency_source_sets),
            dependency_graph,
            artifact_files: Arc::new(artifact_files),
            lock,
            canonical_lock_graph,
            canonical_host_contract,
            host_contract_source_identity,
            host_required_entrypoints_identity,
            compilation_options,
            fingerprint_input,
            build_fingerprint,
        })
    }

    /// Revalidates the complete authority closure currently stored in this value.
    ///
    /// Engine and tooling inspect these fields directly, so the façade invokes this method at
    /// every compile/check boundary. Reconstructing through `new` detects post-construction
    /// replacement of manifests, source sets, dependency/lock/Host/options authorities, or their
    /// fingerprint; the candidate-local file table and cached identities are compared below.
    pub fn validate_integrity(&self) -> Result<(), ResolvedBuildInputError> {
        let rebuilt = Self::new(
            Arc::clone(&self.root_manifest),
            Arc::clone(&self.root_source_set),
            self.dependency_manifests.as_ref().clone(),
            self.dependency_source_sets.as_ref().clone(),
            Arc::clone(&self.dependency_graph),
            self.lock.clone(),
            Arc::clone(&self.canonical_host_contract),
            Arc::clone(&self.host_contract_source_identity),
            Arc::clone(&self.host_required_entrypoints_identity),
            self.compilation_options,
            self.fingerprint_input.as_ref().clone(),
        )?;
        if self.artifact_files.as_ref() != rebuilt.artifact_files.as_ref() {
            return Err(ResolvedBuildInputError::ArtifactFileTableMismatch);
        }
        if self.canonical_lock_graph != rebuilt.canonical_lock_graph {
            return Err(ResolvedBuildInputError::StoredCanonicalLockMismatch);
        }
        if self.build_fingerprint != rebuilt.build_fingerprint {
            return Err(ResolvedBuildInputError::StoredBuildFingerprintMismatch);
        }
        Ok(())
    }

    #[must_use]
    pub fn root_package(&self) -> &PackageId {
        &self.root_manifest.id
    }

    pub fn all_source_sets(&self) -> impl Iterator<Item = &PackageSourceSet> {
        std::iter::once(self.root_source_set.as_ref())
            .chain(self.dependency_source_sets.values().map(AsRef::as_ref))
    }

    #[must_use]
    pub fn source_snapshots(&self) -> Arc<nexa_diagnostics::SourceSnapshotRegistry> {
        let mut builder = nexa_diagnostics::SourceSnapshotRegistry::builder();
        for source_set in self.all_source_sets() {
            for unit in source_set.units().values() {
                // Construction validates package identities and source keys, so duplicates across
                // the closure are impossible. Keep the conversion infallible for downstream
                // diagnostic production.
                builder
                    .insert(
                        nexa_diagnostics::SourceIdentity::package(
                            unit.key.package_id.as_str(),
                            unit.key.path.as_str(),
                        ),
                        Arc::clone(&unit.text),
                    )
                    .expect("resolved source keys are unique");
            }
        }
        builder.build()
    }

    pub fn candidate(&self) -> Result<PackageCandidate, CandidateError> {
        PackageCandidate::new(
            Arc::clone(&self.root_manifest),
            Arc::clone(&self.root_source_set),
            Arc::clone(&self.dependency_graph),
            self.build_fingerprint,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreshnessOutcome {
    Fresh,
    Superseded(FreshnessMismatch),
    HostRebuildRequired {
        candidate: BuildFingerprint,
        current: BuildFingerprint,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreshnessMismatch {
    PackageIdentity,
    Generation {
        candidate: u64,
        desired: u64,
    },
    OriginalInput,
    BuildFingerprint {
        candidate: BuildFingerprint,
        current: BuildFingerprint,
    },
}

impl CandidateIdentity {
    /// Compares a completed candidate with a freshly re-read immutable build input at the commit
    /// safe point. Callers must reconstruct `current` through [`ResolvedBuildInput::new`], which
    /// revalidates the entire root/lock/dependency/Host/compiler/stdlib closure.
    #[must_use]
    pub fn compare_freshness(
        &self,
        desired_generation: u64,
        original: &ResolvedBuildInput,
        current: &ResolvedBuildInput,
    ) -> FreshnessOutcome {
        if self.package_id != original.root_manifest.id
            || self.package_id != current.root_manifest.id
        {
            return FreshnessOutcome::Superseded(FreshnessMismatch::PackageIdentity);
        }
        if self.generation != desired_generation {
            return FreshnessOutcome::Superseded(FreshnessMismatch::Generation {
                candidate: self.generation,
                desired: desired_generation,
            });
        }
        if self.build_fingerprint != original.build_fingerprint {
            return FreshnessOutcome::Superseded(FreshnessMismatch::OriginalInput);
        }
        if self.build_fingerprint == current.build_fingerprint {
            return FreshnessOutcome::Fresh;
        }
        if differs_only_by_host_contract(original, current) {
            return FreshnessOutcome::HostRebuildRequired {
                candidate: self.build_fingerprint,
                current: current.build_fingerprint,
            };
        }
        FreshnessOutcome::Superseded(FreshnessMismatch::BuildFingerprint {
            candidate: self.build_fingerprint,
            current: current.build_fingerprint,
        })
    }
}

fn differs_only_by_host_contract(
    original: &ResolvedBuildInput,
    current: &ResolvedBuildInput,
) -> bool {
    if original.canonical_host_contract == current.canonical_host_contract {
        return false;
    }
    let mut original_input = original.fingerprint_input.as_ref().clone();
    let mut current_input = current.fingerprint_input.as_ref().clone();
    original_input.host_contract.clear();
    original_input.host_contract_source.clear();
    original_input.host_required_entrypoints.clear();
    current_input.host_contract.clear();
    current_input.host_contract_source.clear();
    current_input.host_required_entrypoints.clear();
    original_input == current_input
}

fn validate_dependency_graph_structure(
    graph: &ResolvedDependencyGraph,
    limits: CompilationLimits,
) -> Result<(), ResolvedBuildInputError> {
    for (key, package) in &graph.packages {
        if key != &package.id {
            return Err(
                ResolvedBuildInputError::DependencyGraphPackageIdentityMismatch {
                    key: key.clone(),
                    package: package.id.clone(),
                },
            );
        }
    }

    let mut aliases = BTreeSet::new();
    let mut adjacency = graph
        .packages
        .keys()
        .cloned()
        .map(|package| (package, BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for edge in &graph.edges {
        for endpoint in [&edge.from, &edge.to] {
            if !graph.packages.contains_key(endpoint) {
                return Err(ResolvedBuildInputError::DependencyGraphEdgeEndpointMissing(
                    endpoint.clone(),
                ));
            }
        }
        if !aliases.insert((edge.from.clone(), edge.alias.clone())) {
            return Err(ResolvedBuildInputError::DependencyAliasTargetConflict {
                from: edge.from.clone(),
                alias: edge.alias.clone(),
            });
        }
        adjacency
            .get_mut(&edge.from)
            .expect("dependency edge endpoint was checked")
            .insert(edge.to.clone());
    }

    graph
        .validate_acyclic()
        .map_err(ResolvedBuildInputError::DependencyGraph)?;

    let mut reachable = BTreeSet::from([graph.root.clone()]);
    let mut pending = vec![graph.root.clone()];
    while let Some(package) = pending.pop() {
        if let Some(targets) = adjacency.get(&package) {
            for target in targets {
                if reachable.insert(target.clone()) {
                    pending.push(target.clone());
                }
            }
        }
    }
    if let Some(package) = graph
        .packages
        .keys()
        .find(|package| !reachable.contains(*package))
    {
        return Err(ResolvedBuildInputError::UnreachableDependency(
            package.clone(),
        ));
    }

    let dependency_count = graph.packages.len().saturating_sub(1);
    if dependency_count > limits.dependency_packages {
        return Err(ResolvedBuildInputError::DependencyPackageLimitExceeded {
            count: dependency_count,
            limit: limits.dependency_packages,
        });
    }
    Ok(())
}

fn validate_dependency_edges(
    root: &PackageManifest,
    dependencies: &BTreeMap<PackageId, Arc<PackageManifest>>,
    graph: &ResolvedDependencyGraph,
) -> Result<(), ResolvedBuildInputError> {
    for manifest in std::iter::once(root).chain(dependencies.values().map(AsRef::as_ref)) {
        let Some(resolved_from) = graph.package(&manifest.id) else {
            return Err(ResolvedBuildInputError::UnknownDependency(
                manifest.id.clone(),
            ));
        };
        let mut resolved_edges = BTreeMap::new();
        for edge in graph.dependencies_of(&manifest.id) {
            match resolved_edges.entry(edge.alias.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(edge.to.clone());
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    return Err(ResolvedBuildInputError::DependencyAliasTargetConflict {
                        from: edge.from.clone(),
                        alias: edge.alias.clone(),
                    });
                }
            }
        }
        let declared_aliases = manifest
            .dependencies
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let resolved_aliases = resolved_edges.keys().cloned().collect::<BTreeSet<_>>();
        if declared_aliases != resolved_aliases {
            return Err(ResolvedBuildInputError::DependencyAliasSetMismatch(
                manifest.id.clone(),
            ));
        }
        for (alias, dependency) in &manifest.dependencies {
            let target = resolved_edges
                .get(alias)
                .expect("declared and resolved alias sets are equal");
            if !dependencies.contains_key(target) {
                return Err(ResolvedBuildInputError::UnknownDependency(target.clone()));
            }
            let Some(resolved_target) = graph.package(target) else {
                return Err(ResolvedBuildInputError::UnknownDependency(target.clone()));
            };
            let expected_directory = dependency
                .path
                .resolve_from(&resolved_from.directory)
                .map_err(|_| ResolvedBuildInputError::DependencyPathMismatch {
                    from: manifest.id.clone(),
                    alias: alias.clone(),
                    expected: resolved_target.directory.clone(),
                    actual: resolved_target.directory.clone(),
                })?;
            if expected_directory != resolved_target.directory
                || resolved_from.source_id != resolved_target.source_id
            {
                return Err(ResolvedBuildInputError::DependencyPathMismatch {
                    from: manifest.id.clone(),
                    alias: alias.clone(),
                    expected: expected_directory,
                    actual: resolved_target.directory.clone(),
                });
            }
        }
    }
    Ok(())
}

impl PackageCandidate {
    pub fn new(
        manifest: Arc<PackageManifest>,
        source_set: Arc<PackageSourceSet>,
        dependency_graph: Arc<ResolvedDependencyGraph>,
        build_fingerprint: BuildFingerprint,
    ) -> Result<Self, CandidateError> {
        if source_set.package_id() != &manifest.id {
            return Err(CandidateError::SourcePackageMismatch {
                manifest: manifest.id.clone(),
                source: source_set.package_id().clone(),
            });
        }
        if dependency_graph.root != manifest.id {
            return Err(CandidateError::DependencyRootMismatch {
                manifest: manifest.id.clone(),
                graph: dependency_graph.root.clone(),
            });
        }
        if source_set.test_units().next().is_some() {
            return Err(CandidateError::TestSourceInProduct(manifest.id.clone()));
        }
        if let Some(entry) = manifest.entry() {
            let found = source_set
                .production_units()
                .filter_map(|unit| unit.expected_module_path().ok())
                .any(|module| &module == entry);
            if !found {
                return Err(CandidateError::MissingEntry(entry.clone()));
            }
        }
        Ok(Self {
            manifest,
            source_set,
            dependency_graph,
            build_fingerprint,
        })
    }

    pub fn identity(&self, generation: u64) -> Result<CandidateIdentity, CandidateError> {
        CandidateIdentity::new(self.manifest.id.clone(), generation, self.build_fingerprint)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CandidateError {
    ZeroGeneration,
    SourcePackageMismatch {
        manifest: PackageId,
        source: PackageId,
    },
    DependencyRootMismatch {
        manifest: PackageId,
        graph: PackageId,
    },
    MissingEntry(crate::ModulePath),
    TestSourceInProduct(PackageId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedBuildInputError {
    RootSourcePackageMismatch {
        manifest: PackageId,
        source: PackageId,
    },
    DependencyRootMismatch {
        manifest: PackageId,
        graph: PackageId,
    },
    MissingResolvedRoot(PackageId),
    RootResolutionMismatch,
    DependencyManifestSetMismatch {
        expected: BTreeSet<PackageId>,
        actual: BTreeSet<PackageId>,
    },
    DependencySourceSetMismatch {
        expected: BTreeSet<PackageId>,
        actual: BTreeSet<PackageId>,
    },
    DependencyManifestIdentityMismatch {
        key: PackageId,
        manifest: PackageId,
    },
    DependencySourceIdentityMismatch {
        key: PackageId,
        source: PackageId,
    },
    DependencyIsNotLibrary(PackageId),
    DependencyResolutionMismatch(PackageId),
    DependencyGraphPackageIdentityMismatch {
        key: PackageId,
        package: PackageId,
    },
    DependencyGraphEdgeEndpointMissing(PackageId),
    DependencyAliasTargetConflict {
        from: PackageId,
        alias: crate::DependencyAlias,
    },
    UnreachableDependency(PackageId),
    DependencyGraph(DependencyGraphError),
    DependencyPackageLimitExceeded {
        count: usize,
        limit: usize,
    },
    DependencyAliasSetMismatch(PackageId),
    DependencyPathMismatch {
        from: PackageId,
        alias: crate::DependencyAlias,
        expected: crate::NormalizedPackagePath,
        actual: crate::NormalizedPackagePath,
    },
    UnknownDependency(PackageId),
    MissingLock,
    LockDrift,
    SourceSet(SourceSetError),
    FingerprintRootMismatch,
    FingerprintRootManifestMismatch,
    FingerprintRootSourceMismatch,
    FingerprintDependencyManifestMismatch,
    FingerprintDependencySourceMismatch,
    FingerprintHostContractMismatch,
    FingerprintHostContractSourceMismatch,
    FingerprintHostRequiredEntrypointsMismatch,
    FingerprintLockMismatch,
    FingerprintLanguageVersionMismatch,
    FingerprintStandardLibraryVersionMismatch,
    FingerprintStandardLibraryDescriptorMismatch,
    FingerprintCompilerVersionMismatch,
    FingerprintBytecodeVersionMismatch,
    FingerprintRuntimeSemanticsVersionMismatch,
    FingerprintOpcodeCostTableVersionMismatch,
    FingerprintMathBackendMismatch,
    FingerprintCompilationOptionsMismatch,
    TestSourcePackageMismatch {
        root: PackageId,
        tests: PackageId,
    },
    ProductionSourceInTestSet,
    ProductSourceContainsTest(PackageId),
    ArtifactFileTableMismatch,
    StoredCanonicalLockMismatch,
    StoredBuildFingerprintMismatch,
}

impl fmt::Display for ResolvedBuildInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ResolvedBuildInputError {}

impl fmt::Display for CandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CandidateError {}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::{
        CompilationLimits, DependencyAlias, DependencyEdge, NormalizedPackagePath, PackageId,
        PackageKind, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder,
    };

    use super::*;

    fn resolved_package(id: &str, path: &str, kind: PackageKind) -> ResolvedPackage {
        ResolvedPackage {
            id: PackageId::new(id).unwrap(),
            version: semver::Version::new(1, 0, 0),
            source_id: SourceId::new("graph-test").unwrap(),
            directory: NormalizedPackagePath::new(path).unwrap(),
            kind,
        }
    }

    fn standalone_limit_input(
        sources: Arc<PackageSourceSet>,
        options: crate::CompilationOptions,
    ) -> Result<ResolvedBuildInput, ResolvedBuildInputError> {
        let manifest = Arc::new(
            PackageManifest::parse(&format!(
                r#"
schema = 2
kind = "application"
id = "{}"
name = "Limits"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
                sources.package_id()
            ))
            .unwrap(),
        );
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                resolved_package(
                    manifest.id.as_str(),
                    "packages/limits",
                    PackageKind::Application,
                ),
            )]),
            edges: BTreeSet::new(),
        });
        let fingerprint = BuildFingerprintInput {
            root_package: manifest.id.clone(),
            root_manifest: manifest.canonical_bytes(),
            root_source_set: source_set_fingerprint(&sources),
            dependency_manifests: BTreeMap::new(),
            dependency_source_sets: BTreeMap::new(),
            host_contract: Vec::new(),
            contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
            host_contract_source: Vec::new(),
            host_required_entrypoints: Vec::new(),
            repl_session_context: Vec::new(),
            language_version: crate::NEXA_LANGUAGE_VERSION,
            standard_library_version: nexa_stdlib::standard_library().version.to_string(),
            standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
            compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
            bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
            runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
            opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
            deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
            compiler_options: crate::canonical_compilation_options(&options),
            canonical_lock_graph: Vec::new(),
        };
        ResolvedBuildInput::new(
            manifest,
            sources,
            BTreeMap::new(),
            BTreeMap::new(),
            graph,
            None,
            Vec::<u8>::new(),
            Vec::<u8>::new(),
            fingerprint.host_required_entrypoints.clone(),
            options,
            fingerprint,
        )
    }

    #[test]
    fn resolved_inputs_revalidate_source_sets_against_effective_options() {
        let package = PackageId::new("example.limits").unwrap();
        let main_source = "fn main() -> i32 { return 0; }";
        let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                main_source,
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("src/extra.nexa").unwrap(),
                "fn value() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap();
        let sources = Arc::new(builder.build().unwrap());

        let module_limited = crate::CompilationOptions {
            limits: CompilationLimits {
                modules_per_package: 1,
                ..CompilationLimits::default()
            },
            ..crate::CompilationOptions::default()
        };
        assert!(matches!(
            standalone_limit_input(Arc::clone(&sources), module_limited),
            Err(ResolvedBuildInputError::SourceSet(
                SourceSetError::TooManyModules(2)
            ))
        ));

        let file_limited = crate::CompilationOptions {
            limits: CompilationLimits {
                source_file_bytes: main_source.len() - 1,
                ..CompilationLimits::default()
            },
            ..crate::CompilationOptions::default()
        };
        assert!(matches!(
            standalone_limit_input(Arc::clone(&sources), file_limited),
            Err(ResolvedBuildInputError::SourceSet(
                SourceSetError::SourceFileTooLarge(_)
            ))
        ));

        let package_limited = crate::CompilationOptions {
            limits: CompilationLimits {
                source_bytes_per_package: sources.production_bytes() - 1,
                ..CompilationLimits::default()
            },
            ..crate::CompilationOptions::default()
        };
        assert!(matches!(
            standalone_limit_input(sources, package_limited),
            Err(ResolvedBuildInputError::SourceSet(
                SourceSetError::PackageSourceTooLarge
            ))
        ));
    }

    #[test]
    fn resolved_test_input_revalidates_test_limits() {
        let package = PackageId::new("example.test-limits").unwrap();
        let options = crate::CompilationOptions {
            limits: CompilationLimits {
                modules_per_package: 1,
                ..CompilationLimits::default()
            },
            ..crate::CompilationOptions::default()
        };
        let mut production = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
        production
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "fn main() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap();
        let product = Arc::new(
            standalone_limit_input(Arc::new(production.build().unwrap()), options).unwrap(),
        );
        let mut tests = SourceSetBuilder::new(package, CompilationLimits::default());
        tests
            .add(
                NormalizedPackagePath::new("tests/one.nexa").unwrap(),
                "@test\nfn one() {}",
                SourceRole::Test,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("tests/two.nexa").unwrap(),
                "@test\nfn two() {}",
                SourceRole::Test,
            )
            .unwrap();

        assert!(matches!(
            ResolvedTestInput::new(product, Arc::new(tests.build().unwrap())),
            Err(ResolvedBuildInputError::SourceSet(
                SourceSetError::TooManyTestModules(2)
            ))
        ));
    }

    #[test]
    fn resolved_input_integrity_rejects_post_construction_authority_replacement() {
        let package = PackageId::new("example.integrity").unwrap();
        let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "fn main() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap();
        let baseline = standalone_limit_input(
            Arc::new(builder.build().unwrap()),
            crate::CompilationOptions::default(),
        )
        .unwrap();
        baseline.validate_integrity().unwrap();

        let mut changed_fingerprint = baseline.clone();
        changed_fingerprint.build_fingerprint = BuildFingerprint::default();
        assert!(matches!(
            changed_fingerprint.validate_integrity(),
            Err(ResolvedBuildInputError::StoredBuildFingerprintMismatch)
        ));

        let mut changed_lock_cache = baseline.clone();
        changed_lock_cache.canonical_lock_graph = Arc::from([1_u8]);
        assert!(matches!(
            changed_lock_cache.validate_integrity(),
            Err(ResolvedBuildInputError::StoredCanonicalLockMismatch)
        ));

        let mut changed_sources_builder =
            SourceSetBuilder::new(package.clone(), CompilationLimits::default());
        changed_sources_builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "fn main() -> i32 { return 0; }\npub const CHANGED: i32 = 1;",
                SourceRole::Production,
            )
            .unwrap();
        let mut changed_sources = baseline.clone();
        changed_sources.root_source_set = Arc::new(changed_sources_builder.build().unwrap());
        assert!(matches!(
            changed_sources.validate_integrity(),
            Err(ResolvedBuildInputError::FingerprintRootSourceMismatch)
        ));

        let mut extra_builder = SourceSetBuilder::new(package, CompilationLimits::default());
        extra_builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "fn main() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("src/extra.nexa").unwrap(),
                "fn value() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap();
        let extra = extra_builder.build().unwrap();
        let mut changed_file_table = baseline;
        changed_file_table.artifact_files =
            Arc::new(ArtifactFileTable::for_closure(std::iter::once(&extra)).unwrap());
        assert!(matches!(
            changed_file_table.validate_integrity(),
            Err(ResolvedBuildInputError::ArtifactFileTableMismatch)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolved_input_rejects_every_forged_dependency_graph_shape() {
        let root = PackageId::new("example.root").unwrap();
        let first = PackageId::new("example.first").unwrap();
        let second = PackageId::new("example.second").unwrap();
        let alias = || DependencyAlias::new("shared").unwrap();
        let package = |id: &PackageId, path: &str, kind| {
            let mut resolved = resolved_package(id.as_str(), path, kind);
            resolved.id = id.clone();
            resolved
        };
        let valid_packages = BTreeMap::from([
            (
                root.clone(),
                package(&root, "packages/root", PackageKind::Application),
            ),
            (
                first.clone(),
                package(&first, "packages/first", PackageKind::Library),
            ),
        ]);

        let mut wrong_identity = ResolvedDependencyGraph {
            root: root.clone(),
            packages: valid_packages.clone(),
            edges: BTreeSet::new(),
        };
        wrong_identity.packages.get_mut(&first).unwrap().id = second.clone();
        assert!(matches!(
            validate_dependency_graph_structure(
                &wrong_identity,
                CompilationLimits::default()
            ),
            Err(
                ResolvedBuildInputError::DependencyGraphPackageIdentityMismatch {
                    key,
                    package,
                }
            ) if key == first && package == second
        ));

        let missing = PackageId::new("example.missing").unwrap();
        let missing_endpoint = ResolvedDependencyGraph {
            root: root.clone(),
            packages: BTreeMap::from([(
                root.clone(),
                package(&root, "packages/root", PackageKind::Application),
            )]),
            edges: BTreeSet::from([DependencyEdge {
                from: root.clone(),
                alias: alias(),
                to: missing.clone(),
            }]),
        };
        assert!(matches!(
            validate_dependency_graph_structure(
                &missing_endpoint,
                CompilationLimits::default()
            ),
            Err(ResolvedBuildInputError::DependencyGraphEdgeEndpointMissing(
                package
            )) if package == missing
        ));

        let duplicate_alias = ResolvedDependencyGraph {
            root: root.clone(),
            packages: BTreeMap::from([
                (
                    root.clone(),
                    package(&root, "packages/root", PackageKind::Application),
                ),
                (
                    first.clone(),
                    package(&first, "packages/first", PackageKind::Library),
                ),
                (
                    second.clone(),
                    package(&second, "packages/second", PackageKind::Library),
                ),
            ]),
            edges: BTreeSet::from([
                DependencyEdge {
                    from: root.clone(),
                    alias: alias(),
                    to: first.clone(),
                },
                DependencyEdge {
                    from: root.clone(),
                    alias: alias(),
                    to: second.clone(),
                },
            ]),
        };
        assert!(matches!(
            validate_dependency_graph_structure(
                &duplicate_alias,
                CompilationLimits::default()
            ),
            Err(ResolvedBuildInputError::DependencyAliasTargetConflict {
                from,
                alias: observed,
            }) if from == root && observed == alias()
        ));

        let unreachable = ResolvedDependencyGraph {
            root: root.clone(),
            packages: valid_packages.clone(),
            edges: BTreeSet::new(),
        };
        assert!(matches!(
            validate_dependency_graph_structure(&unreachable, CompilationLimits::default()),
            Err(ResolvedBuildInputError::UnreachableDependency(package)) if package == first
        ));

        let over_limit = ResolvedDependencyGraph {
            root: root.clone(),
            packages: valid_packages,
            edges: BTreeSet::from([DependencyEdge {
                from: root.clone(),
                alias: alias(),
                to: first.clone(),
            }]),
        };
        let zero_dependencies = CompilationLimits {
            dependency_packages: 0,
            ..CompilationLimits::default()
        };
        assert!(matches!(
            validate_dependency_graph_structure(&over_limit, zero_dependencies),
            Err(ResolvedBuildInputError::DependencyPackageLimitExceeded { count: 1, limit: 0 })
        ));

        let cycle = ResolvedDependencyGraph {
            root: root.clone(),
            packages: BTreeMap::from([
                (
                    root.clone(),
                    package(&root, "packages/root", PackageKind::Application),
                ),
                (
                    first.clone(),
                    package(&first, "packages/first", PackageKind::Library),
                ),
                (
                    second.clone(),
                    package(&second, "packages/second", PackageKind::Library),
                ),
            ]),
            edges: BTreeSet::from([
                DependencyEdge {
                    from: root.clone(),
                    alias: DependencyAlias::new("first").unwrap(),
                    to: first.clone(),
                },
                DependencyEdge {
                    from: first.clone(),
                    alias: DependencyAlias::new("second").unwrap(),
                    to: second.clone(),
                },
                DependencyEdge {
                    from: second,
                    alias: DependencyAlias::new("first").unwrap(),
                    to: first,
                },
            ]),
        };
        assert!(matches!(
            validate_dependency_graph_structure(&cycle, CompilationLimits::default()),
            Err(ResolvedBuildInputError::DependencyGraph(
                DependencyGraphError::Cycle(_)
            ))
        ));
    }

    #[test]
    fn candidate_rejects_missing_entry_and_zero_generation() {
        let manifest = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "application"
id = "example.app"
name = "App"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "default-enabled"
"#,
            )
            .unwrap(),
        );
        let mut source = SourceSetBuilder::new(
            PackageId::new("example.app").unwrap(),
            CompilationLimits::default(),
        );
        source
            .add(
                NormalizedPackagePath::new("src/other.nexa").unwrap(),
                "fn helper() {}",
                SourceRole::Production,
            )
            .unwrap();
        let graph = ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id: SourceId::new("local").unwrap(),
                    directory: NormalizedPackagePath::new("app").unwrap(),
                    kind: manifest.kind,
                },
            )]),
            edges: BTreeSet::new(),
        };
        assert!(matches!(
            PackageCandidate::new(
                manifest,
                Arc::new(source.build().unwrap()),
                Arc::new(graph),
                BuildFingerprint::default()
            ),
            Err(CandidateError::MissingEntry(_))
        ));
        assert!(matches!(
            CandidateIdentity::new(
                PackageId::new("example.app").unwrap(),
                0,
                BuildFingerprint::default()
            ),
            Err(CandidateError::ZeroGeneration)
        ));
    }

    #[test]
    fn candidate_matches_application_entry_by_virtual_module_identity() {
        let manifest = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "application"
id = "nexa.snippet"
name = "Snippet"
version = "0.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
            )
            .unwrap(),
        );
        let mut source = SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
        source
            .add_virtual_snippet(
                NormalizedPackagePath::new("src/generated/snippet_019.nexa").unwrap(),
                "fn main() -> i32 {\r\n    return 0;\r\n}\r\n",
                crate::ModulePath::new("main").unwrap(),
            )
            .unwrap();
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id: SourceId::new("cli").unwrap(),
                    directory: NormalizedPackagePath::new("snippet").unwrap(),
                    kind: manifest.kind,
                },
            )]),
            edges: BTreeSet::new(),
        });

        let candidate = PackageCandidate::new(
            manifest,
            Arc::new(source.build().unwrap()),
            graph,
            BuildFingerprint::default(),
        )
        .unwrap();

        let unit = candidate.source_set.production_units().next().unwrap();
        assert_eq!(
            crate::ModulePath::from_source_path(&unit.key.path).unwrap(),
            crate::ModulePath::new("generated.snippet_019").unwrap()
        );
        assert_eq!(
            unit.expected_module_path().unwrap(),
            crate::ModulePath::new("main").unwrap()
        );
        assert_ne!(
            crate::ModulePath::from_source_path(&unit.key.path).unwrap(),
            unit.expected_module_path().unwrap()
        );
        assert_eq!(
            unit.text.as_ref(),
            "fn main() -> i32 {\r\n    return 0;\r\n}\r\n"
        );
    }

    #[test]
    fn resolved_product_rejects_test_units() {
        let manifest = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "application"
id = "example.app"
name = "App"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
            )
            .unwrap(),
        );
        let mut sources = SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
        sources
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "fn main() -> i32 { return 0; }",
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("tests/main.nexa").unwrap(),
                "@test\nfn main_test() {}",
                SourceRole::Test,
            )
            .unwrap();
        let sources = Arc::new(sources.build().unwrap());
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id: SourceId::new("local").unwrap(),
                    directory: NormalizedPackagePath::new("app").unwrap(),
                    kind: manifest.kind,
                },
            )]),
            edges: BTreeSet::new(),
        });
        let compilation_options = crate::CompilationOptions::default();
        let fingerprint = BuildFingerprintInput {
            root_package: manifest.id.clone(),
            root_manifest: manifest.canonical_bytes(),
            root_source_set: source_set_fingerprint(&sources),
            dependency_manifests: BTreeMap::new(),
            dependency_source_sets: BTreeMap::new(),
            host_contract: Vec::new(),
            contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
            host_contract_source: Vec::new(),
            host_required_entrypoints: Vec::new(),
            repl_session_context: Vec::new(),
            language_version: crate::NEXA_LANGUAGE_VERSION,
            standard_library_version: nexa_stdlib::standard_library().version.to_string(),
            standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
            compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
            bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
            runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
            opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
            deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
            compiler_options: crate::canonical_compilation_options(&compilation_options),
            canonical_lock_graph: Vec::new(),
        };
        assert!(matches!(
            ResolvedBuildInput::new(
                manifest,
                sources,
                BTreeMap::new(),
                BTreeMap::new(),
                graph,
                None,
                Vec::<u8>::new(),
                Vec::<u8>::new(),
                fingerprint.host_required_entrypoints.clone(),
                compilation_options,
                fingerprint,
            ),
            Err(ResolvedBuildInputError::ProductSourceContainsTest(_))
        ));
    }

    #[test]
    fn resolved_input_enforces_dependency_closure_byte_limit() {
        let manifest = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "application"
id = "example.closure"
name = "Closure"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
            )
            .unwrap(),
        );
        let mut source_builder =
            SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
        source_builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "pub fn main() {}\n",
                SourceRole::Production,
            )
            .unwrap();
        let sources = Arc::new(source_builder.build().unwrap());
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                resolved_package(
                    manifest.id.as_str(),
                    "packages/closure",
                    PackageKind::Application,
                ),
            )]),
            edges: BTreeSet::new(),
        });
        let options = crate::CompilationOptions {
            limits: CompilationLimits {
                dependency_closure_bytes: sources.production_bytes().saturating_sub(1),
                ..CompilationLimits::default()
            },
            ..crate::CompilationOptions::default()
        };
        let fingerprint = BuildFingerprintInput {
            root_package: manifest.id.clone(),
            root_manifest: manifest.canonical_bytes(),
            root_source_set: source_set_fingerprint(&sources),
            dependency_manifests: BTreeMap::new(),
            dependency_source_sets: BTreeMap::new(),
            host_contract: Vec::new(),
            contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
            host_contract_source: Vec::new(),
            host_required_entrypoints: Vec::new(),
            repl_session_context: Vec::new(),
            language_version: crate::NEXA_LANGUAGE_VERSION,
            standard_library_version: nexa_stdlib::standard_library().version.to_string(),
            standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
            compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
            bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
            runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
            opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
            deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
            compiler_options: crate::canonical_compilation_options(&options),
            canonical_lock_graph: Vec::new(),
        };
        assert!(matches!(
            ResolvedBuildInput::new(
                manifest,
                sources,
                BTreeMap::new(),
                BTreeMap::new(),
                graph,
                None,
                Vec::<u8>::new(),
                Vec::<u8>::new(),
                fingerprint.host_required_entrypoints.clone(),
                options,
                fingerprint,
            ),
            Err(ResolvedBuildInputError::SourceSet(
                SourceSetError::DependencyClosureTooLarge
            ))
        ));
    }

    #[test]
    fn resolved_input_rejects_forged_dependency_path_edge() {
        let root = PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.app"
name = "App"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"

[dependencies]
shared = { path = "../lib" }
"#,
        )
        .unwrap();
        let library = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "library"
id = "example.lib"
name = "Library"
version = "1.0.0"
source_root = "src"
"#,
            )
            .unwrap(),
        );
        let source_id = SourceId::new("local").unwrap();
        let graph = ResolvedDependencyGraph {
            root: root.id.clone(),
            packages: BTreeMap::from([
                (
                    root.id.clone(),
                    ResolvedPackage {
                        id: root.id.clone(),
                        version: root.version.clone(),
                        source_id: source_id.clone(),
                        directory: NormalizedPackagePath::new("packages/app").unwrap(),
                        kind: root.kind,
                    },
                ),
                (
                    library.id.clone(),
                    ResolvedPackage {
                        id: library.id.clone(),
                        version: library.version.clone(),
                        source_id,
                        directory: NormalizedPackagePath::new("packages/forged").unwrap(),
                        kind: library.kind,
                    },
                ),
            ]),
            edges: BTreeSet::from([DependencyEdge {
                from: root.id.clone(),
                alias: DependencyAlias::new("shared").unwrap(),
                to: library.id.clone(),
            }]),
        };
        assert!(matches!(
            validate_dependency_edges(
                &root,
                &BTreeMap::from([(library.id.clone(), library)]),
                &graph,
            ),
            Err(ResolvedBuildInputError::DependencyPathMismatch { .. })
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn resolved_input_rejects_every_forged_global_build_authority() {
        let manifest = Arc::new(
            PackageManifest::parse(
                r#"
schema = 2
kind = "application"
id = "example.authority"
name = "Authority"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
            )
            .unwrap(),
        );
        let options = crate::CompilationOptions::default();
        let mut source_builder = SourceSetBuilder::new(manifest.id.clone(), options.limits);
        source_builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "pub fn main() {}\n",
                SourceRole::Production,
            )
            .unwrap();
        let sources = Arc::new(source_builder.build().unwrap());
        let graph = Arc::new(ResolvedDependencyGraph {
            root: manifest.id.clone(),
            packages: BTreeMap::from([(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id: SourceId::new("authority-test").unwrap(),
                    directory: NormalizedPackagePath::new("packages/authority").unwrap(),
                    kind: manifest.kind,
                },
            )]),
            edges: BTreeSet::new(),
        });
        let baseline = BuildFingerprintInput {
            root_package: manifest.id.clone(),
            root_manifest: manifest.canonical_bytes(),
            root_source_set: source_set_fingerprint(&sources),
            dependency_manifests: BTreeMap::new(),
            dependency_source_sets: BTreeMap::new(),
            host_contract: Vec::new(),
            contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
            host_contract_source: Vec::new(),
            host_required_entrypoints: Vec::new(),
            repl_session_context: Vec::new(),
            language_version: crate::NEXA_LANGUAGE_VERSION,
            standard_library_version: nexa_stdlib::standard_library().version.to_string(),
            standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
            compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
            bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
            runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
            opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
            deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
            compiler_options: crate::canonical_compilation_options(&options),
            canonical_lock_graph: Vec::new(),
        };
        let mutations: [fn(&mut BuildFingerprintInput); 9] = [
            |input| input.language_version = input.language_version.saturating_add(1),
            |input| input.standard_library_version.push_str("-forged"),
            |input| input.standard_library_descriptor.push(0),
            |input| input.compiler_version.push_str("-forged"),
            |input| input.bytecode_version = input.bytecode_version.saturating_add(1),
            |input| {
                input.runtime_semantics_version = input.runtime_semantics_version.saturating_add(1);
            },
            |input| {
                input.opcode_cost_table_version = input.opcode_cost_table_version.saturating_add(1);
            },
            |input| input.deterministic_math_backend.push_str("-forged"),
            |input| input.compiler_options.push(0),
        ];
        let expected = [
            ResolvedBuildInputError::FingerprintLanguageVersionMismatch,
            ResolvedBuildInputError::FingerprintStandardLibraryVersionMismatch,
            ResolvedBuildInputError::FingerprintStandardLibraryDescriptorMismatch,
            ResolvedBuildInputError::FingerprintCompilerVersionMismatch,
            ResolvedBuildInputError::FingerprintBytecodeVersionMismatch,
            ResolvedBuildInputError::FingerprintRuntimeSemanticsVersionMismatch,
            ResolvedBuildInputError::FingerprintOpcodeCostTableVersionMismatch,
            ResolvedBuildInputError::FingerprintMathBackendMismatch,
            ResolvedBuildInputError::FingerprintCompilationOptionsMismatch,
        ];
        for (mutate, expected) in mutations.into_iter().zip(expected) {
            let mut fingerprint = baseline.clone();
            mutate(&mut fingerprint);
            let error = ResolvedBuildInput::new(
                Arc::clone(&manifest),
                Arc::clone(&sources),
                BTreeMap::new(),
                BTreeMap::new(),
                Arc::clone(&graph),
                None,
                Vec::<u8>::new(),
                Vec::<u8>::new(),
                Vec::<u8>::new(),
                options,
                fingerprint,
            )
            .unwrap_err();
            assert_eq!(error, expected);
        }
    }
}
