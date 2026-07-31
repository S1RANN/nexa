use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexa_analysis::{
    ActivationPolicy, BuildFingerprint, CandidateIdentity, CompilationLimits, CompilationOptions,
    LoadedPackageDirectory, LockFile, NormalizedPackagePath, PackageCandidate, PackageCatalog,
    PackageId, PackageLocation, PackageManifest, ResolvedBuildInput, ResolvedDependencyGraph,
    ResolvedPackage, ResolvedTestInput, SourceId, SourceKey, SourceRole, SourceSetBuilder,
    load_package_directory, load_package_directory_without_lock, validate_module_source_for_role,
};
use serde::Deserialize;

use crate::{CliError, CliResult};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub schema: u32,
    pub contract: PathBuf,
    #[serde(default)]
    pub required_exports: Option<Vec<String>>,
    pub sources: Vec<SourceConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub id: String,
    pub root: PathBuf,
    pub trust: ConfigTrustLevel,
    pub activation: Vec<ActivationPolicy>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub allow_entitlement: bool,
    pub max_packages: usize,
    pub limits: RuntimeLimitsConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigTrustLevel {
    FirstParty,
    Trusted,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLimitsConfig {
    pub handler_fuel: u64,
    pub cumulative_budget: u64,
    pub heap_objects: u32,
    pub host_resources: u32,
    pub tasks: u32,
    pub release_records: usize,
}

#[derive(Clone, Debug)]
pub struct SourcePolicy {
    #[allow(dead_code)]
    pub trust: ConfigTrustLevel,
    pub activation: BTreeSet<ActivationPolicy>,
    pub capabilities: BTreeSet<String>,
    pub allow_entitlement: bool,
    pub max_packages: usize,
    pub limits: RuntimeLimitsConfig,
}

#[derive(Clone, Debug)]
pub struct LoadedSource {
    pub id: SourceId,
    pub root: PathBuf,
    pub policy: SourcePolicy,
}

#[derive(Clone, Debug)]
pub struct DiscoveredPackage {
    pub directory: PathBuf,
    pub source_id: SourceId,
    pub source_root: PathBuf,
    pub policy: SourcePolicy,
}

#[derive(Clone, Debug)]
pub struct LoadedProject {
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub sources: Vec<LoadedSource>,
    pub contract_path: PathBuf,
    pub contract_source: String,
    pub idl: nexa::Idl,
    /// Effective required-export subset. An omitted setting means every NIDL export; an explicit
    /// empty list means no Package export is required.
    pub required_exports: Vec<String>,
}

/// An owned Host contract snapshot which can borrow its parsed IDL only for the duration of one
/// facade call while retaining the exact reader-facing source identity and bytes.
#[derive(Clone, Debug)]
pub struct HostContractSnapshot {
    pub idl: Arc<nexa::Idl>,
    pub identity: nexa::SourceIdentity,
    pub source: Arc<str>,
    pub required_exports: Arc<[String]>,
}

impl HostContractSnapshot {
    pub fn with_source(
        idl: &nexa::Idl,
        identity: nexa::SourceIdentity,
        source: impl Into<Arc<str>>,
    ) -> CliResult<Self> {
        let required_exports = idl
            .exports
            .iter()
            .map(|export| export.name.clone())
            .collect::<Vec<_>>();
        Self::with_required_exports(idl, identity, source, &required_exports)
    }

    pub fn with_required_exports(
        idl: &nexa::Idl,
        identity: nexa::SourceIdentity,
        source: impl Into<Arc<str>>,
        required_exports: &[String],
    ) -> CliResult<Self> {
        let source = source.into();
        nexa::HostContractInput::with_source(idl, identity.clone(), Arc::clone(&source))
            .and_then(|contract| contract.requiring_exports(required_exports))
            .map_err(|error| {
                CliError::internal(format!("invalid owned Host contract snapshot: {error}"))
            })?;
        Ok(Self {
            idl: Arc::new(idl.clone()),
            identity,
            source,
            required_exports: Arc::from(required_exports),
        })
    }

    #[must_use]
    pub fn canonical(idl: &nexa::Idl) -> Self {
        let input = nexa::HostContractInput::canonical(idl);
        Self {
            idl: Arc::new(idl.clone()),
            identity: input.source().identity().clone(),
            source: Arc::clone(input.source().text()),
            required_exports: idl
                .exports
                .iter()
                .map(|export| export.name.clone())
                .collect::<Vec<_>>()
                .into(),
        }
    }

    fn input(&self) -> Result<nexa::HostContractInput<'_>, nexa::HostContractSourceError> {
        nexa::HostContractInput::with_source(
            &self.idl,
            self.identity.clone(),
            Arc::clone(&self.source),
        )
        .and_then(|contract| contract.requiring_exports(&self.required_exports))
    }
}

/// One immutable, fully resolved build input shared by check, build, test, dev, and LSP.
#[derive(Clone, Debug)]
pub struct ResolvedBuild {
    #[allow(dead_code)]
    pub source_id: SourceId,
    #[allow(dead_code)]
    pub source_root: PathBuf,
    #[allow(dead_code)]
    pub root_directory: NormalizedPackagePath,
    pub root: Arc<LoadedPackageDirectory>,
    pub packages: BTreeMap<PackageId, Arc<LoadedPackageDirectory>>,
    pub input: Arc<ResolvedBuildInput>,
    pub host_contract: HostContractSnapshot,
    pub dependency_graph: Arc<ResolvedDependencyGraph>,
    pub canonical_lock: LockFile,
    pub build_fingerprint: BuildFingerprint,
    pub candidate: Arc<PackageCandidate>,
    pub virtual_source_origin: Option<VirtualSourceOrigin>,
}

#[derive(Clone, Debug)]
pub struct VirtualSourceOrigin {
    pub source_key: SourceKey,
    pub display_identity: nexa::SourceIdentity,
    pub original_text: Arc<str>,
    pub source_text_is_original: bool,
}

#[derive(Debug)]
pub struct CompiledBuild {
    pub identity: CandidateIdentity,
    pub artifact: CompiledBuildArtifact,
    pub module_count: usize,
}

#[derive(Debug)]
pub enum CompiledBuildArtifact {
    Checked,
    Product(Box<nexa::CompiledPackageArtifact>),
    Tests(Box<nexa::CompiledPackageTests>),
}

impl CompiledBuild {
    #[must_use]
    pub fn product(&self) -> Option<&nexa::CompiledPackageArtifact> {
        match &self.artifact {
            CompiledBuildArtifact::Product(artifact) => Some(artifact.as_ref()),
            CompiledBuildArtifact::Checked | CompiledBuildArtifact::Tests(_) => None,
        }
    }

    #[must_use]
    pub fn tests(&self) -> Option<&nexa::CompiledPackageTests> {
        match &self.artifact {
            CompiledBuildArtifact::Tests(artifact) => Some(artifact.as_ref()),
            CompiledBuildArtifact::Checked | CompiledBuildArtifact::Product(_) => None,
        }
    }

    #[must_use]
    pub fn function_count(&self) -> Option<usize> {
        self.product()
            .map(|artifact| artifact.module().functions.len())
    }
}

#[derive(Debug)]
pub enum BuildCompileError {
    Cli(CliError),
    Facade(nexa::PackageBuildError),
}

impl ResolvedBuild {
    pub fn identity(&self, generation: u64) -> CliResult<CandidateIdentity> {
        self.candidate
            .identity(generation)
            .map_err(|error| CliError::internal(format!("invalid Candidate identity: {error}")))
    }

    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.root.manifest.id
    }

    #[allow(clippy::result_large_err)]
    pub fn compile(
        &self,
        generation: u64,
        host_idl: Option<&nexa::Idl>,
        required_exports: &[String],
        include_tests: bool,
    ) -> Result<CompiledBuild, BuildCompileError> {
        self.compile_with_limits(
            generation,
            host_idl,
            required_exports,
            include_tests,
            nexa::VerifierLimits::default(),
        )
    }

    #[allow(clippy::result_large_err)]
    pub fn compile_with_limits(
        &self,
        generation: u64,
        host_idl: Option<&nexa::Idl>,
        required_exports: &[String],
        include_tests: bool,
        verifier_limits: nexa::VerifierLimits,
    ) -> Result<CompiledBuild, BuildCompileError> {
        let mut session = nexa::PackageBuildSession::new();
        self.compile_with_session_and_limits(
            &mut session,
            generation,
            host_idl,
            required_exports,
            include_tests,
            verifier_limits,
        )
    }

    #[allow(clippy::result_large_err)]
    pub fn compile_with_session(
        &self,
        session: &mut nexa::PackageBuildSession,
        generation: u64,
        host_idl: Option<&nexa::Idl>,
        required_exports: &[String],
        include_tests: bool,
    ) -> Result<CompiledBuild, BuildCompileError> {
        self.compile_with_session_and_limits(
            session,
            generation,
            host_idl,
            required_exports,
            include_tests,
            nexa::VerifierLimits::default(),
        )
    }

    #[allow(clippy::result_large_err, clippy::too_many_arguments)]
    pub fn compile_with_session_and_limits(
        &self,
        session: &mut nexa::PackageBuildSession,
        generation: u64,
        host_idl: Option<&nexa::Idl>,
        required_exports: &[String],
        include_tests: bool,
        verifier_limits: nexa::VerifierLimits,
    ) -> Result<CompiledBuild, BuildCompileError> {
        let canonical_contract;
        let retained_contract;
        let contract = if let Some(host_idl) = host_idl {
            canonical_contract = nexa::HostContractInput::canonical(host_idl);
            &canonical_contract
        } else {
            retained_contract = self.host_contract.input().map_err(|error| {
                BuildCompileError::Cli(CliError::internal(format!(
                    "retained Host contract is invalid: {error}"
                )))
            })?;
            &retained_contract
        };
        self.compile_with_contract_session_and_limits(
            session,
            generation,
            contract,
            required_exports,
            include_tests,
            verifier_limits,
        )
    }

    #[allow(clippy::result_large_err)]
    pub fn compile_with_contract_session(
        &self,
        session: &mut nexa::PackageBuildSession,
        generation: u64,
        contract: &nexa::HostContractInput<'_>,
        required_exports: &[String],
        include_tests: bool,
    ) -> Result<CompiledBuild, BuildCompileError> {
        self.compile_with_contract_session_and_limits(
            session,
            generation,
            contract,
            required_exports,
            include_tests,
            nexa::VerifierLimits::default(),
        )
    }

    #[allow(clippy::result_large_err, clippy::too_many_arguments)]
    pub fn compile_with_contract_session_and_limits(
        &self,
        session: &mut nexa::PackageBuildSession,
        generation: u64,
        contract: &nexa::HostContractInput<'_>,
        required_exports: &[String],
        include_tests: bool,
        verifier_limits: nexa::VerifierLimits,
    ) -> Result<CompiledBuild, BuildCompileError> {
        let contract = contract
            .requiring_exports(required_exports)
            .map_err(|error| {
                BuildCompileError::Cli(CliError::diagnostic(format!(
                    "invalid required-export view: {error}"
                )))
            })?;
        let identity = self.identity(generation).map_err(BuildCompileError::Cli)?;
        let (artifact, module_count) = if include_tests {
            let input = self.input_with_tests().map_err(BuildCompileError::Cli)?;
            let module_count = input.artifact_files.files().len();
            let artifact = session
                .compile_package_tests_with_contract_and_limits(
                    &input,
                    &contract,
                    identity.clone(),
                    verifier_limits,
                )
                .map(Box::new)
                .map(CompiledBuildArtifact::Tests)
                .map_err(BuildCompileError::Facade)?;
            (artifact, module_count)
        } else if self.root.manifest.is_application() {
            let artifact = session
                .compile_package_with_contract_and_limits(
                    &self.input,
                    &contract,
                    identity.clone(),
                    verifier_limits,
                )
                .map_err(BuildCompileError::Facade)?;
            let module_count = artifact.debug_info.modules.len();
            (
                CompiledBuildArtifact::Product(Box::new(artifact)),
                module_count,
            )
        } else {
            let report = session
                .check_package_with_contract(&self.input, &contract)
                .map_err(BuildCompileError::Facade)?;
            (CompiledBuildArtifact::Checked, report.modules)
        };
        Ok(CompiledBuild {
            identity,
            artifact,
            module_count,
        })
    }

    /// Atomically rebuilds every source-derived identity after an editor overlay changes one or
    /// more package snapshots. Manifest/dependency topology remains fixed; all source tables,
    /// fingerprints, and the candidate identity are reconstructed together.
    pub fn rebuild_with_contract(
        &self,
        packages: BTreeMap<PackageId, Arc<LoadedPackageDirectory>>,
        host_contract: &nexa::HostContractInput<'_>,
    ) -> CliResult<Self> {
        let expected = self
            .dependency_graph
            .packages
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let actual = packages.keys().cloned().collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(CliError::internal(
                "editor overlay changed the resolved Package dependency closure",
            ));
        }
        let root = packages
            .get(self.package_id())
            .cloned()
            .ok_or_else(|| CliError::internal("editor overlay omitted the root Package"))?;
        nexa_analysis::PackageSourceSet::validate_dependency_closure(
            packages
                .values()
                .map(|package| package.production_sources.as_ref()),
            CompilationLimits::default(),
        )
        .map_err(|error| CliError::diagnostic(error.to_string()))?;
        let selected_contract = host_contract
            .requiring_exports(&self.host_contract.required_exports)
            .map_err(|error| {
                CliError::diagnostic(format!("invalid required-export view: {error}"))
            })?;
        let input = resolved_build_input(
            &root,
            Arc::clone(&root.production_sources),
            &packages,
            Arc::clone(&self.dependency_graph),
            self.input.lock.clone(),
            &selected_contract,
        )?;
        let build_fingerprint = input.build_fingerprint;
        let candidate =
            Arc::new(input.candidate().map_err(|error| {
                CliError::internal(format!("invalid overlay Candidate: {error}"))
            })?);
        Ok(Self {
            source_id: self.source_id.clone(),
            source_root: self.source_root.clone(),
            root_directory: self.root_directory.clone(),
            root,
            packages,
            input,
            host_contract: HostContractSnapshot::with_required_exports(
                host_contract.idl(),
                host_contract.source().identity().clone(),
                Arc::clone(host_contract.source().text()),
                &self.host_contract.required_exports,
            )?,
            dependency_graph: Arc::clone(&self.dependency_graph),
            canonical_lock: self.canonical_lock.clone(),
            build_fingerprint,
            candidate,
            virtual_source_origin: self.virtual_source_origin.clone(),
        })
    }

    fn input_with_tests(&self) -> CliResult<Arc<ResolvedTestInput>> {
        ResolvedTestInput::new(Arc::clone(&self.input), Arc::clone(&self.root.test_sources))
            .map(Arc::new)
            .map_err(|error| {
                CliError::internal(format!("resolved Package test input is invalid: {error}"))
            })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    schema: u32,
    id: String,
    trust: ConfigTrustLevel,
    activation: Vec<ActivationPolicy>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    allow_entitlement: bool,
    max_packages: usize,
    limits: RuntimeLimitsConfig,
}

impl LoadedProject {
    pub fn load(path: &Path) -> CliResult<Self> {
        Self::load_with_overlays(path, |_| None)
    }

    /// Loads project metadata while allowing an editor snapshot to replace the exact text of
    /// open build inputs. The callback is consulted for the project manifest and Host Contract;
    /// every absent override is read from disk.
    pub fn load_with_overlays(
        path: &Path,
        overlay_for_path: impl FnMut(&Path) -> Option<String>,
    ) -> CliResult<Self> {
        Self::load_snapshot(path, overlay_for_path, true)
    }

    /// Loads an editor snapshot without rejecting a syntactically valid NIDL that temporarily
    /// omits a configured export. The LSP reports that semantic error against the NIDL URI after
    /// project discovery, while normal CLI and development loads remain strict.
    pub fn load_editor_snapshot(
        path: &Path,
        overlay_for_path: impl FnMut(&Path) -> Option<String>,
    ) -> CliResult<Self> {
        Self::load_snapshot(path, overlay_for_path, false)
    }

    #[allow(clippy::too_many_lines)]
    fn load_snapshot(
        path: &Path,
        mut overlay_for_path: impl FnMut(&Path) -> Option<String>,
        validate_exports: bool,
    ) -> CliResult<Self> {
        let snapshot_config_path = snapshot_path(path);
        let config_overlay = overlay_for_path(path).or_else(|| {
            (snapshot_config_path != path)
                .then(|| overlay_for_path(&snapshot_config_path))
                .flatten()
        });
        let config_path = if config_overlay.is_some() {
            if path.exists() {
                reject_symlink_path(path)?;
            }
            snapshot_config_path
        } else {
            reject_symlink_path(path)?;
            path.canonicalize().map_err(|error| {
                CliError::environment(format!(
                    "could not resolve project configuration {}: {error}",
                    path.display()
                ))
            })?
        };
        let root = config_path
            .parent()
            .ok_or_else(|| CliError::environment("project configuration has no parent directory"))?
            .to_path_buf();
        let source = config_overlay
            .or_else(|| overlay_for_path(&config_path))
            .map_or_else(
                || {
                    fs::read_to_string(&config_path).map_err(|error| {
                        CliError::internal(format!(
                            "could not read {}: {error}",
                            config_path.display()
                        ))
                    })
                },
                Ok,
            )?;
        let config: ProjectConfig = toml::from_str(&source).map_err(|error| {
            CliError::environment(format!("invalid {}: {error}", config_path.display()))
        })?;
        if config.schema != 2 {
            return Err(CliError::environment(format!(
                "unsupported nexa.dev.toml schema {}; expected schema 2",
                config.schema
            )));
        }
        if config.sources.is_empty() {
            return Err(CliError::environment(
                "nexa.dev.toml must declare at least one [[sources]] entry",
            ));
        }
        if let Some(required_exports) = &config.required_exports {
            reject_duplicate_required_exports(required_exports)?;
        }

        let mut source_ids = BTreeSet::new();
        let mut sources = Vec::with_capacity(config.sources.len());
        for configured in &config.sources {
            let id = SourceId::new(configured.id.clone()).map_err(|error| {
                CliError::environment(format!("invalid source id `{}`: {error}", configured.id))
            })?;
            if !source_ids.insert(id.clone()) {
                return Err(CliError::environment(format!("duplicate source id `{id}`")));
            }
            let source_root = resolve_within(&root, &configured.root)?;
            if !source_root.is_dir() {
                return Err(CliError::environment(format!(
                    "source root is not a directory: {}",
                    source_root.display()
                )));
            }
            let policy = policy_from_parts(
                configured.trust,
                &configured.activation,
                &configured.capabilities,
                configured.allow_entitlement,
                configured.max_packages,
                configured.limits,
            )?;
            sources.push(LoadedSource {
                id,
                root: source_root,
                policy,
            });
        }
        reject_overlapping_roots(&sources)?;

        let contract_path = resolve_within(&root, &config.contract)?;
        let contract_source = overlay_for_path(&contract_path).map_or_else(
            || {
                fs::read_to_string(&contract_path).map_err(|error| {
                    CliError::internal(format!(
                        "could not read {}: {error}",
                        contract_path.display()
                    ))
                })
            },
            Ok,
        )?;
        let idl = nexa::parse_idl(&contract_source).map_err(|error| {
            CliError::diagnostic(format!("invalid {}: {error}", contract_path.display()))
        })?;
        let required_exports = config.required_exports.clone().unwrap_or_else(|| {
            idl.exports
                .iter()
                .map(|export| export.name.clone())
                .collect()
        });
        if validate_exports {
            validate_required_exports(&idl, &required_exports, &contract_path)?;
        }

        Ok(Self {
            config_path,
            root,
            sources,
            contract_path,
            contract_source,
            idl,
            required_exports,
        })
    }

    pub fn package_directories(&self) -> CliResult<Vec<DiscoveredPackage>> {
        self.package_directories_snapshot(None)
    }

    /// Discovers packages from one editor snapshot. An open `package.toml` is authoritative even
    /// before the file exists on disk, so adding or moving a package manifest immediately changes
    /// the package scope and the resulting [`ResolvedBuildInput`].
    pub fn package_directories_with_overlays(
        &self,
        overlays: &BTreeMap<PathBuf, String>,
    ) -> CliResult<Vec<DiscoveredPackage>> {
        self.package_directories_snapshot(Some(overlays))
    }

    fn package_directories_snapshot(
        &self,
        overlays: Option<&BTreeMap<PathBuf, String>>,
    ) -> CliResult<Vec<DiscoveredPackage>> {
        let mut packages = Vec::new();
        for source in &self.sources {
            let mut source_packages = overlays.map_or_else(
                || discover_package_roots(&source.root),
                |overlays| discover_package_roots_with_overlays(&source.root, overlays),
            )?;
            if source_packages.len() > source.policy.max_packages {
                return Err(CliError::environment(format!(
                    "source `{}` contains {} packages, exceeding max_packages {}",
                    source.id,
                    source_packages.len(),
                    source.policy.max_packages
                )));
            }
            packages.extend(
                source_packages
                    .drain(..)
                    .map(|directory| DiscoveredPackage {
                        directory,
                        source_id: source.id.clone(),
                        source_root: source.root.clone(),
                        policy: source.policy.clone(),
                    }),
            );
        }
        packages.sort_by(|left, right| {
            left.source_id
                .cmp(&right.source_id)
                .then_with(|| left.directory.cmp(&right.directory))
        });
        Ok(packages)
    }

    pub fn host_contract_snapshot(&self) -> CliResult<HostContractSnapshot> {
        HostContractSnapshot::with_required_exports(
            &self.idl,
            nexa::SourceIdentity::standalone(self.contract_path.to_string_lossy().into_owned()),
            Arc::<str>::from(self.contract_source.as_str()),
            &self.required_exports,
        )
    }

    pub fn resolve_package(
        &self,
        package: &DiscoveredPackage,
        require_current_lock: bool,
    ) -> CliResult<ResolvedBuild> {
        self.resolve_package_snapshot(package, require_current_lock, None)
    }

    /// Resolves the complete package/dependency closure from one editor snapshot.
    ///
    /// Open `package.toml`, `nexa.lock`, production, and test documents are authoritative even
    /// before they reach disk. Dependency retargeting therefore rebuilds the graph and every
    /// source-derived identity atomically instead of patching a previously resolved graph.
    pub fn resolve_package_with_overlays(
        &self,
        package: &DiscoveredPackage,
        require_current_lock: bool,
        overlays: &BTreeMap<PathBuf, String>,
    ) -> CliResult<ResolvedBuild> {
        self.resolve_package_snapshot(
            package,
            require_current_lock,
            (!overlays.is_empty()).then_some(overlays),
        )
    }

    fn resolve_package_snapshot(
        &self,
        package: &DiscoveredPackage,
        require_current_lock: bool,
        overlays: Option<&BTreeMap<PathBuf, String>>,
    ) -> CliResult<ResolvedBuild> {
        validate_package_belongs_to_source(package)?;
        let (resolver_root, allowed_root) = if package.directory == package.source_root {
            (
                package.source_root.parent().ok_or_else(|| {
                    CliError::environment("package source root has no parent directory")
                })?,
                package.source_root.as_path(),
            )
        } else {
            (package.source_root.as_path(), package.source_root.as_path())
        };
        resolve_package_build(
            &package.directory,
            resolver_root,
            allowed_root,
            package.source_id.clone(),
            Some(&package.policy),
            &self.host_contract_snapshot()?,
            require_current_lock,
            false,
            overlays,
        )
    }

    pub fn resolve_package_for_lock(
        &self,
        package: &DiscoveredPackage,
    ) -> CliResult<ResolvedBuild> {
        validate_package_belongs_to_source(package)?;
        let (resolver_root, allowed_root) = if package.directory == package.source_root {
            (
                package.source_root.parent().ok_or_else(|| {
                    CliError::environment("package source root has no parent directory")
                })?,
                package.source_root.as_path(),
            )
        } else {
            (package.source_root.as_path(), package.source_root.as_path())
        };
        resolve_package_build(
            &package.directory,
            resolver_root,
            allowed_root,
            package.source_id.clone(),
            Some(&package.policy),
            &self.host_contract_snapshot()?,
            false,
            true,
            None,
        )
    }

    pub fn resolved_builds(&self, require_current_lock: bool) -> CliResult<Vec<ResolvedBuild>> {
        let packages = self.package_directories()?;
        let mut builds = Vec::with_capacity(packages.len());
        let mut package_ids = BTreeMap::<PackageId, PathBuf>::new();
        for package in packages {
            let build = self.resolve_package(&package, require_current_lock)?;
            if let Some(first) = package_ids.get(build.package_id()) {
                return Err(CliError::diagnostic(format!(
                    "duplicate Package ID `{}` at {} and {}",
                    build.package_id(),
                    first.display(),
                    package.directory.display()
                )));
            }
            package_ids.insert(build.package_id().clone(), package.directory.clone());
            builds.push(build);
        }
        builds.sort_by(|left, right| left.package_id().cmp(right.package_id()));
        Ok(builds)
    }
}

/// Resolves a direct package command in a controlled sibling-only source domain.
pub fn resolve_direct_package(
    directory: &Path,
    source_id: SourceId,
    policy: Option<&SourcePolicy>,
    host_contract: &HostContractSnapshot,
    require_current_lock: bool,
) -> CliResult<ResolvedBuild> {
    reject_symlink_path(directory)?;
    let directory = directory.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve {}: {error}",
            directory.display()
        ))
    })?;
    if !directory.is_dir() {
        return Err(CliError::environment(format!(
            "package path is not a directory: {}",
            directory.display()
        )));
    }
    let source_root = directory
        .parent()
        .ok_or_else(|| CliError::environment("package directory has no parent"))?
        .to_path_buf();
    resolve_package_build(
        &directory,
        &source_root,
        &source_root,
        source_id,
        policy,
        host_contract,
        require_current_lock,
        false,
        None,
    )
}

pub fn resolve_direct_package_for_lock(
    directory: &Path,
    source_id: SourceId,
) -> CliResult<ResolvedBuild> {
    reject_symlink_path(directory)?;
    let directory = directory.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve {}: {error}",
            directory.display()
        ))
    })?;
    let source_root = directory
        .parent()
        .ok_or_else(|| CliError::environment("package directory has no parent"))?
        .to_path_buf();
    let host_idl = empty_host_idl()?;
    let host_contract = HostContractSnapshot::canonical(&host_idl);
    resolve_package_build(
        &directory,
        &source_root,
        &source_root,
        source_id,
        None,
        &host_contract,
        false,
        true,
        None,
    )
}

fn empty_host_idl() -> CliResult<nexa::Idl> {
    nexa::parse_idl("interface NexaCliEmptyHost {}\n").map_err(|error| {
        CliError::internal(format!("invalid built-in empty Host contract: {error}"))
    })
}

/// Adapts the single-file CLI surface to the same package identity and fingerprint model.
#[allow(clippy::too_many_lines)]
pub fn virtual_snippet(source: &str, display_path: &Path) -> CliResult<ResolvedBuild> {
    let host_idl = empty_host_idl()?;
    let host_contract = HostContractSnapshot::canonical(&host_idl);
    let package_id = PackageId::new("nexa.snippet")
        .map_err(|error| CliError::internal(format!("invalid snippet Package ID: {error}")))?;
    let manifest_source = "schema = 2\n\
                           kind = \"application\"\n\
                           id = \"nexa.snippet\"\n\
                           name = \"Nexa Snippet\"\n\
                           version = \"0.0.0\"\n\
                           source_root = \"src\"\n\
                           entry = \"main\"\n\
                           activation = \"programmatic\"\n";
    let manifest = Arc::new(
        PackageManifest::parse(manifest_source)
            .map_err(|error| CliError::internal(format!("invalid virtual manifest: {error}")))?,
    );
    let original_text = Arc::<str>::from(source);
    let mut source_builder =
        SourceSetBuilder::new(package_id.clone(), CompilationLimits::default());
    source_builder
        .add_virtual_snippet(
            NormalizedPackagePath::new("src/main.nexa")
                .map_err(|error| CliError::internal(error.to_string()))?,
            Arc::clone(&original_text),
            nexa_analysis::ModulePath::new("main")
                .map_err(|error| CliError::internal(error.to_string()))?,
        )
        .map_err(|error| CliError::diagnostic(error.to_string()))?;
    let production_sources = Arc::new(
        source_builder
            .build()
            .map_err(|error| CliError::diagnostic(error.to_string()))?,
    );
    let source_key = production_sources
        .production_units()
        .next()
        .expect("virtual snippet has exactly one source")
        .key
        .clone();
    let test_sources = Arc::new(
        SourceSetBuilder::new(package_id.clone(), CompilationLimits::default())
            .build()
            .map_err(|error| CliError::internal(error.to_string()))?,
    );
    let source_id = SourceId::new("cli")
        .map_err(|error| CliError::internal(format!("invalid CLI Source ID: {error}")))?;
    let root_directory = NormalizedPackagePath::new("snippet")
        .map_err(|error| CliError::internal(error.to_string()))?;
    let graph = Arc::new(ResolvedDependencyGraph {
        root: package_id.clone(),
        packages: BTreeMap::from([(
            package_id.clone(),
            ResolvedPackage {
                id: package_id.clone(),
                version: manifest.version.clone(),
                source_id: source_id.clone(),
                directory: root_directory.clone(),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let root = Arc::new(LoadedPackageDirectory {
        directory: PathBuf::from("nexa.snippet"),
        manifest_source: Arc::from(manifest_source),
        manifest: Arc::clone(&manifest),
        production_sources: Arc::clone(&production_sources),
        test_sources,
        lock: None,
    });
    let packages = BTreeMap::from([(package_id, Arc::clone(&root))]);
    let canonical_lock = LockFile::from_graph(&graph);
    let contract = host_contract.input().map_err(|error| {
        CliError::internal(format!("invalid virtual Host contract snapshot: {error}"))
    })?;
    let input = resolved_build_input(
        &root,
        Arc::clone(&root.production_sources),
        &packages,
        Arc::clone(&graph),
        None,
        &contract,
    )?;
    let build_fingerprint = input.build_fingerprint;
    let candidate = Arc::new(
        input
            .candidate()
            .map_err(|error| CliError::internal(format!("invalid snippet Candidate: {error}")))?,
    );
    Ok(ResolvedBuild {
        source_id,
        source_root: PathBuf::from("nexa.snippet"),
        root_directory,
        root,
        packages,
        input,
        host_contract,
        dependency_graph: graph,
        canonical_lock,
        build_fingerprint,
        candidate,
        virtual_source_origin: Some(VirtualSourceOrigin {
            source_key,
            display_identity: nexa::SourceIdentity::standalone(
                display_path.to_string_lossy().into_owned(),
            ),
            original_text,
            source_text_is_original: true,
        }),
    })
}

fn load_package_directory_with_overlays(
    directory: &Path,
    limits: CompilationLimits,
    parse_lock: bool,
    overlays: &BTreeMap<PathBuf, String>,
) -> CliResult<LoadedPackageDirectory> {
    reject_symlink_path(directory)?;
    let canonical_directory = directory.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve package directory {}: {error}",
            directory.display()
        ))
    })?;

    let manifest_path = canonical_directory.join("package.toml");
    if manifest_path.exists() {
        reject_symlink_path(&manifest_path)?;
    }
    let manifest_source = snapshot_text(overlays, &manifest_path)
        .map(str::to_owned)
        .map_or_else(
            || {
                fs::read_to_string(&manifest_path).map_err(|error| {
                    CliError::internal(format!(
                        "could not read {}: {error}",
                        manifest_path.display()
                    ))
                })
            },
            Ok,
        )?;
    let manifest = Arc::new(PackageManifest::parse(&manifest_source).map_err(|error| {
        CliError::diagnostic(format!("invalid {}: {error}", manifest_path.display()))
    })?);

    let production_sources = Arc::new(load_source_tree_with_overlays(
        &canonical_directory,
        &canonical_directory.join(manifest.source_root.as_path()),
        manifest.id.clone(),
        SourceRole::Production,
        limits,
        overlays,
    )?);
    if let Some(entry) = manifest.expected_entry_source()
        && !production_sources
            .units()
            .keys()
            .any(|key| key.path == entry)
    {
        return Err(CliError::diagnostic(format!(
            "Package entry source is missing: {entry}"
        )));
    }

    let test_sources = Arc::new(load_source_tree_with_overlays(
        &canonical_directory,
        &canonical_directory.join("tests"),
        manifest.id.clone(),
        SourceRole::Test,
        limits,
        overlays,
    )?);

    let lock_path = canonical_directory.join("nexa.lock");
    let lock_source = snapshot_text(overlays, &lock_path).map(str::to_owned);
    let lock = if parse_lock && (lock_source.is_some() || lock_path.exists()) {
        if lock_path.exists() {
            reject_symlink_path(&lock_path)?;
        }
        let source = lock_source.map_or_else(
            || {
                fs::read_to_string(&lock_path).map_err(|error| {
                    CliError::internal(format!("could not read {}: {error}", lock_path.display()))
                })
            },
            Ok,
        )?;
        Some(Arc::new(LockFile::parse(&source).map_err(|error| {
            CliError::environment(format!("invalid {}: {error}", lock_path.display()))
        })?))
    } else {
        None
    };

    Ok(LoadedPackageDirectory {
        directory: canonical_directory,
        manifest_source: manifest_source.into(),
        manifest,
        production_sources,
        test_sources,
        lock,
    })
}

fn load_source_tree_with_overlays(
    package_directory: &Path,
    tree_root: &Path,
    package_id: PackageId,
    role: SourceRole,
    limits: CompilationLimits,
    overlays: &BTreeMap<PathBuf, String>,
) -> CliResult<nexa_analysis::PackageSourceSet> {
    let normalized_package = snapshot_path(package_directory);
    let normalized_root = snapshot_path(tree_root);
    if !normalized_root.starts_with(&normalized_package) {
        return Err(CliError::environment(format!(
            "package source root escapes {}: {}",
            package_directory.display(),
            tree_root.display()
        )));
    }

    let mut paths = BTreeSet::new();
    let has_overlay_source = overlays.keys().any(|path| {
        let normalized = snapshot_path(path);
        normalized.starts_with(&normalized_root)
            && normalized.extension().and_then(|value| value.to_str()) == Some("nexa")
    });
    if role == SourceRole::Production && !tree_root.exists() && !has_overlay_source {
        return Err(CliError::environment(format!(
            "package source root does not exist: {}",
            tree_root.display()
        )));
    }
    if tree_root.exists() {
        collect_nexa_source_paths(tree_root, &mut paths)?;
    }
    for path in overlays.keys() {
        let normalized = snapshot_path(path);
        if normalized.starts_with(&normalized_root)
            && normalized.extension().and_then(|value| value.to_str()) == Some("nexa")
        {
            paths.insert(normalized);
        }
    }

    let mut builder = SourceSetBuilder::new(package_id, limits);
    for path in paths {
        let normalized = snapshot_path(&path);
        if !normalized.starts_with(&normalized_root) || !normalized.starts_with(&normalized_package)
        {
            return Err(CliError::environment(format!(
                "package source path escapes {}: {}",
                package_directory.display(),
                path.display()
            )));
        }
        let relative = normalized
            .strip_prefix(&normalized_package)
            .map_err(|_| {
                CliError::environment(format!(
                    "package source path escapes {}: {}",
                    package_directory.display(),
                    path.display()
                ))
            })?
            .to_path_buf();
        let normalized_relative = NormalizedPackagePath::from_path(&relative)
            .map_err(|error| CliError::diagnostic(error.to_string()))?;
        let source = snapshot_text(overlays, &normalized)
            .map(str::to_owned)
            .map_or_else(
                || {
                    fs::read_to_string(&normalized).map_err(|error| {
                        CliError::internal(format!(
                            "could not read {}: {error}",
                            normalized.display()
                        ))
                    })
                },
                Ok,
            )?;
        if role == SourceRole::Production {
            validate_module_source_for_role(&normalized_relative, &source, role)
                .map_err(|error| package_load_error(package_directory, error))?;
        }
        builder
            .add(normalized_relative, source, role)
            .map_err(|error| CliError::diagnostic(error.to_string()))?;
    }
    builder
        .build()
        .map_err(|error| CliError::diagnostic(error.to_string()))
}

fn collect_nexa_source_paths(directory: &Path, output: &mut BTreeSet<PathBuf>) -> CliResult<()> {
    reject_symlink_path(directory)?;
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", directory.display()))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            CliError::internal(format!("could not read {}: {error}", directory.display()))
        })?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            CliError::internal(format!("could not inspect {}: {error}", path.display()))
        })?;
        if file_type.is_symlink() {
            return Err(CliError::environment(format!(
                "symlink package paths are not allowed: {}",
                path.display()
            )));
        }
        if file_type.is_dir() {
            collect_nexa_source_paths(&path, output)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("nexa")
        {
            output.insert(snapshot_path(&path));
        }
    }
    Ok(())
}

fn snapshot_text<'a>(overlays: &'a BTreeMap<PathBuf, String>, path: &Path) -> Option<&'a str> {
    let normalized = snapshot_path(path);
    overlays
        .iter()
        .find(|(candidate, _)| snapshot_path(candidate) == normalized)
        .map(|(_, text)| text.as_str())
}

fn snapshot_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while let Some(name) = cursor.file_name() {
        missing.push(name.to_owned());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
        if let Ok(mut canonical) = cursor.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    path.to_path_buf()
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn resolve_package_build(
    root_directory: &Path,
    resolver_root: &Path,
    allowed_root: &Path,
    source_id: SourceId,
    policy: Option<&SourcePolicy>,
    host_contract: &HostContractSnapshot,
    require_current_lock: bool,
    ignore_existing_lock: bool,
    overlays: Option<&BTreeMap<PathBuf, String>>,
) -> CliResult<ResolvedBuild> {
    let limits = CompilationLimits::default();
    let resolver_root = resolver_root.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve source root {}: {error}",
            resolver_root.display()
        ))
    })?;
    let allowed_root = allowed_root.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve allowed source root {}: {error}",
            allowed_root.display()
        ))
    })?;
    let root_directory = root_directory.canonicalize().map_err(|error| {
        CliError::environment(format!(
            "could not resolve package directory {}: {error}",
            root_directory.display()
        ))
    })?;
    if !root_directory.starts_with(&allowed_root) {
        return Err(CliError::environment(format!(
            "package {} escapes controlled source root {}",
            root_directory.display(),
            allowed_root.display()
        )));
    }
    let root_relative = relative_package_path(&resolver_root, &root_directory)?;

    let mut pending = vec![root_relative.clone()];
    let mut by_path = BTreeMap::<NormalizedPackagePath, Arc<LoadedPackageDirectory>>::new();
    while let Some(relative) = pending.pop() {
        if by_path.contains_key(&relative) {
            continue;
        }
        let requested = resolver_root.join(relative.as_path());
        reject_symlink_path(&requested)?;
        let canonical = requested.canonicalize().map_err(|error| {
            CliError::environment(format!(
                "could not resolve dependency package {}: {error}",
                requested.display()
            ))
        })?;
        if !canonical.starts_with(&allowed_root) {
            return Err(CliError::environment(format!(
                "dependency path escapes controlled source root: {}",
                requested.display()
            )));
        }
        let canonical_relative = relative_package_path(&resolver_root, &canonical)?;
        if canonical_relative != relative {
            return Err(CliError::environment(format!(
                "dependency path is not canonical: requested `{relative}`, resolved `{canonical_relative}`"
            )));
        }
        let loaded = Arc::new(if let Some(overlays) = overlays {
            load_package_directory_with_overlays(
                &canonical,
                limits,
                !ignore_existing_lock,
                overlays,
            )?
        } else {
            (if ignore_existing_lock {
                load_package_directory_without_lock(&canonical, limits)
            } else {
                load_package_directory(&canonical, limits)
            })
            .map_err(|error| package_load_error(&canonical, error))?
        });
        if by_path.is_empty()
            && let Some(policy) = policy
        {
            validate_manifest_policy(&loaded, policy)?;
        }
        for dependency in loaded.manifest.dependencies.values() {
            pending.push(
                dependency
                    .path
                    .resolve_from(&relative)
                    .map_err(|error| CliError::diagnostic(error.to_string()))?,
            );
        }
        by_path.insert(relative, loaded);
    }

    let mut catalog = PackageCatalog::new();
    for (directory, package) in &by_path {
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: directory.clone(),
                manifest: Arc::clone(&package.manifest),
            })
            .map_err(|error| CliError::diagnostic(error.to_string()))?;
    }
    let graph = Arc::new(
        catalog
            .resolve(&source_id, &root_relative, limits)
            .map_err(|error| CliError::diagnostic(error.to_string()))?,
    );

    let packages = graph
        .packages
        .values()
        .map(|resolved| {
            let package = by_path.get(&resolved.directory).ok_or_else(|| {
                CliError::internal(format!(
                    "resolved package {} has no loaded source snapshot",
                    resolved.id
                ))
            })?;
            Ok((resolved.id.clone(), Arc::clone(package)))
        })
        .collect::<CliResult<BTreeMap<_, _>>>()?;
    nexa_analysis::PackageSourceSet::validate_dependency_closure(
        packages
            .values()
            .map(|package| package.production_sources.as_ref()),
        limits,
    )
    .map_err(|error| CliError::diagnostic(error.to_string()))?;

    let root = by_path
        .get(&root_relative)
        .cloned()
        .ok_or_else(|| CliError::internal("root package disappeared during resolution"))?;
    let canonical_lock = LockFile::from_graph(&graph);
    verify_lock(&root, &graph, require_current_lock)?;
    let resolved_lock = if ignore_existing_lock {
        Some(Arc::new(canonical_lock.clone()))
    } else {
        root.lock.clone()
    };
    let contract = host_contract.input().map_err(|error| {
        CliError::internal(format!("invalid resolved Host contract snapshot: {error}"))
    })?;
    let input = resolved_build_input(
        &root,
        Arc::clone(&root.production_sources),
        &packages,
        Arc::clone(&graph),
        resolved_lock,
        &contract,
    )?;
    let build_fingerprint = input.build_fingerprint;
    let candidate = Arc::new(
        input
            .candidate()
            .map_err(|error| CliError::internal(format!("invalid Package Candidate: {error}")))?,
    );

    Ok(ResolvedBuild {
        source_id,
        source_root: resolver_root,
        root_directory: root_relative,
        root,
        packages,
        input,
        host_contract: host_contract.clone(),
        dependency_graph: graph,
        canonical_lock,
        build_fingerprint,
        candidate,
        virtual_source_origin: None,
    })
}

fn verify_lock(
    root: &LoadedPackageDirectory,
    graph: &ResolvedDependencyGraph,
    require_current_lock: bool,
) -> CliResult<()> {
    let has_dependencies = !root.manifest.dependencies.is_empty();
    match root.lock.as_deref() {
        Some(lock) => lock
            .verify(graph)
            .map_err(|error| CliError::environment(error.to_string())),
        None if require_current_lock && has_dependencies => Err(CliError::environment(
            "nexa.lock is missing or stale; run `nexa lock`",
        )),
        None => Ok(()),
    }
}

fn resolved_build_input(
    root: &Arc<LoadedPackageDirectory>,
    root_source_set: Arc<nexa_analysis::PackageSourceSet>,
    packages: &BTreeMap<PackageId, Arc<LoadedPackageDirectory>>,
    graph: Arc<ResolvedDependencyGraph>,
    lock: Option<Arc<LockFile>>,
    host_contract: &nexa::HostContractInput<'_>,
) -> CliResult<Arc<ResolvedBuildInput>> {
    let dependency_manifests = packages
        .iter()
        .filter(|(package, _)| *package != &root.manifest.id)
        .map(|(package, loaded)| (package.clone(), Arc::clone(&loaded.manifest)))
        .collect();
    let dependency_source_sets = packages
        .iter()
        .filter(|(package, _)| *package != &root.manifest.id)
        .map(|(package, loaded)| (package.clone(), Arc::clone(&loaded.production_sources)))
        .collect::<BTreeMap<_, _>>();
    let fingerprint_input = nexa::canonical_package_build_fingerprint_input_with_contract(
        &root.manifest,
        &root_source_set,
        &dependency_manifests,
        &dependency_source_sets,
        host_contract,
        lock.as_deref(),
    );
    let canonical_host_contract = fingerprint_input.host_contract.clone();
    let host_contract_source_identity = fingerprint_input.host_contract_source.clone();
    let host_required_exports_identity = fingerprint_input.host_required_exports.clone();
    ResolvedBuildInput::new(
        Arc::clone(&root.manifest),
        root_source_set,
        dependency_manifests,
        dependency_source_sets,
        graph,
        lock,
        Arc::<[u8]>::from(canonical_host_contract),
        Arc::<[u8]>::from(host_contract_source_identity),
        Arc::<[u8]>::from(host_required_exports_identity),
        CompilationOptions::default(),
        fingerprint_input,
    )
    .map(Arc::new)
    .map_err(|error| {
        CliError::internal(format!(
            "resolved Package input failed canonical validation: {error}"
        ))
    })
}

fn validate_manifest_policy(
    package: &LoadedPackageDirectory,
    policy: &SourcePolicy,
) -> CliResult<()> {
    let Some(application) = &package.manifest.application else {
        return Ok(());
    };
    if !policy.activation.contains(&application.activation) {
        return Err(policy_error(
            package,
            format!(
                "activation {:?} is outside the configured source policy",
                application.activation
            ),
        ));
    }
    if !application
        .capabilities
        .iter()
        .all(|capability| policy.capabilities.contains(capability))
    {
        return Err(policy_error(
            package,
            "Package capabilities exceed the configured source policy",
        ));
    }
    if application.entitlement.is_some() && !policy.allow_entitlement {
        return Err(policy_error(
            package,
            "Package entitlement is outside the configured source policy",
        ));
    }
    check_optional_limit(
        package,
        "handler_fuel",
        application.handler_fuel,
        policy.limits.handler_fuel,
    )?;
    check_optional_limit(
        package,
        "cumulative_budget",
        application.cumulative_budget,
        policy.limits.cumulative_budget,
    )?;
    check_optional_limit(
        package,
        "heap_objects",
        application.heap_objects,
        policy.limits.heap_objects,
    )?;
    check_optional_limit(
        package,
        "host_resources",
        application.host_resources,
        policy.limits.host_resources,
    )?;
    check_optional_limit(package, "tasks", application.tasks, policy.limits.tasks)?;
    check_optional_limit(
        package,
        "release_records",
        application.release_records,
        policy.limits.release_records,
    )?;
    Ok(())
}

fn check_optional_limit<T>(
    package: &LoadedPackageDirectory,
    name: &str,
    requested: Option<T>,
    maximum: T,
) -> CliResult<()>
where
    T: Copy + PartialOrd + std::fmt::Display,
{
    if requested.is_some_and(|requested| requested > maximum) {
        Err(policy_error(
            package,
            format!("runtime limit `{name}` exceeds configured maximum {maximum}"),
        ))
    } else {
        Ok(())
    }
}

fn policy_error(package: &LoadedPackageDirectory, message: impl Into<String>) -> CliError {
    CliError::diagnostic(format!(
        "Package policy rejected `{}`: {}",
        package.manifest.id,
        message.into()
    ))
}

pub fn load_policy(path: &Path) -> CliResult<(SourceId, SourcePolicy)> {
    let source = fs::read_to_string(path).map_err(|error| {
        CliError::internal(format!("could not read {}: {error}", path.display()))
    })?;
    let config: PolicyFile = toml::from_str(&source)
        .map_err(|error| CliError::environment(format!("invalid {}: {error}", path.display())))?;
    if config.schema != 1 {
        return Err(CliError::environment(format!(
            "unsupported package policy schema {}; expected schema 1",
            config.schema
        )));
    }
    let id = SourceId::new(config.id).map_err(|error| {
        CliError::environment(format!("invalid package policy source id: {error}"))
    })?;
    let policy = policy_from_parts(
        config.trust,
        &config.activation,
        &config.capabilities,
        config.allow_entitlement,
        config.max_packages,
        config.limits,
    )?;
    Ok((id, policy))
}

fn policy_from_parts(
    trust: ConfigTrustLevel,
    activation: &[ActivationPolicy],
    capabilities: &[String],
    allow_entitlement: bool,
    max_packages: usize,
    limits: RuntimeLimitsConfig,
) -> CliResult<SourcePolicy> {
    if activation.is_empty() {
        return Err(CliError::environment(
            "source policy activation set must not be empty",
        ));
    }
    if max_packages == 0 {
        return Err(CliError::environment(
            "source policy max_packages must be greater than zero",
        ));
    }
    let activation_set = activation.iter().copied().collect::<BTreeSet<_>>();
    if activation_set.len() != activation.len() {
        return Err(CliError::environment(
            "duplicate activation mode in source policy",
        ));
    }
    let mut capability_set = BTreeSet::new();
    for capability in capabilities {
        if !valid_capability(capability) {
            return Err(CliError::environment(format!(
                "invalid capability `{capability}`"
            )));
        }
        if !capability_set.insert(capability.clone()) {
            return Err(CliError::environment(format!(
                "duplicate capability `{capability}` in source policy"
            )));
        }
    }
    Ok(SourcePolicy {
        trust,
        activation: activation_set,
        capabilities: capability_set,
        allow_entitlement,
        max_packages,
        limits,
    })
}

fn valid_capability(value: &str) -> bool {
    !value.is_empty()
        && !value.split('.').any(str::is_empty)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_required_exports(
    idl: &nexa::Idl,
    names: &[String],
    contract_path: &Path,
) -> CliResult<()> {
    for name in names {
        if !idl.exports.iter().any(|export| export.name == *name) {
            return Err(CliError::environment(format!(
                "required export `{name}` is not declared by {}",
                contract_path.display()
            )));
        }
    }
    Ok(())
}

fn reject_duplicate_required_exports(exports: &[String]) -> CliResult<()> {
    let mut seen = BTreeSet::new();
    for export in exports {
        if !seen.insert(export) {
            return Err(CliError::environment(format!(
                "duplicate required export `{export}`"
            )));
        }
    }
    Ok(())
}

fn discover_package_roots(root: &Path) -> CliResult<Vec<PathBuf>> {
    fn visit(directory: &Path, output: &mut Vec<PathBuf>) -> CliResult<()> {
        reject_symlink_path(directory)?;
        if directory.join("package.toml").is_file() {
            output.push(directory.canonicalize().map_err(|error| {
                CliError::environment(format!(
                    "could not resolve package directory {}: {error}",
                    directory.display()
                ))
            })?);
            return Ok(());
        }
        let mut entries = fs::read_dir(directory)
            .map_err(|error| {
                CliError::internal(format!("could not read {}: {error}", directory.display()))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                CliError::internal(format!("could not read {}: {error}", directory.display()))
            })?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| {
                CliError::internal(format!(
                    "could not inspect {}: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_symlink() {
                return Err(CliError::environment(format!(
                    "symlink is not allowed in a package source root: {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                visit(&entry.path(), output)?;
            }
        }
        Ok(())
    }

    let mut packages = Vec::new();
    visit(root, &mut packages)?;
    packages.sort();
    Ok(packages)
}

fn discover_package_roots_with_overlays(
    root: &Path,
    overlays: &BTreeMap<PathBuf, String>,
) -> CliResult<Vec<PathBuf>> {
    fn visit(
        directory: &Path,
        overlays: &BTreeMap<PathBuf, String>,
        output: &mut Vec<PathBuf>,
    ) -> CliResult<()> {
        reject_symlink_path(directory)?;
        let manifest = directory.join("package.toml");
        if snapshot_text(overlays, &manifest).is_some() || manifest.is_file() {
            output.push(snapshot_path(directory));
            return Ok(());
        }
        let mut entries = fs::read_dir(directory)
            .map_err(|error| {
                CliError::internal(format!("could not read {}: {error}", directory.display()))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                CliError::internal(format!("could not read {}: {error}", directory.display()))
            })?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let file_type = entry.file_type().map_err(|error| {
                CliError::internal(format!(
                    "could not inspect {}: {error}",
                    entry.path().display()
                ))
            })?;
            if file_type.is_symlink() {
                return Err(CliError::environment(format!(
                    "symlink is not allowed in a package source root: {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                visit(&entry.path(), overlays, output)?;
            }
        }
        Ok(())
    }

    let mut packages = Vec::new();
    visit(root, overlays, &mut packages)?;
    packages.sort();
    packages.dedup();
    Ok(packages)
}

fn validate_package_belongs_to_source(package: &DiscoveredPackage) -> CliResult<()> {
    if package.directory.starts_with(&package.source_root) {
        Ok(())
    } else {
        Err(CliError::internal(format!(
            "discovered package {} escaped source root {}",
            package.directory.display(),
            package.source_root.display()
        )))
    }
}

fn relative_package_path(root: &Path, path: &Path) -> CliResult<NormalizedPackagePath> {
    let relative = path.strip_prefix(root).map_err(|_| {
        CliError::environment(format!(
            "package {} escapes source root {}",
            path.display(),
            root.display()
        ))
    })?;
    NormalizedPackagePath::from_path(relative)
        .map_err(|error| CliError::environment(error.to_string()))
}

fn reject_symlink_path(path: &Path) -> CliResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CliError::environment(format!("could not inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() {
        Err(CliError::environment(format!(
            "symlink package paths are not allowed: {}",
            path.display()
        )))
    } else {
        Ok(())
    }
}

fn reject_symlink_below(root: &Path, relative: &Path) -> CliResult<()> {
    if relative.is_absolute() {
        return Err(CliError::environment(format!(
            "path must be relative to the project root: {}",
            relative.display()
        )));
    }
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(component) => current.push(component),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => {
                return Err(CliError::environment(format!(
                    "path must be canonical and cannot contain `..`: {}",
                    relative.display()
                )));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(CliError::environment(format!(
                    "path must be relative to the project root: {}",
                    relative.display()
                )));
            }
        }
        reject_symlink_path(&current)?;
    }
    Ok(())
}

fn package_load_error(path: &Path, error: nexa_analysis::PackageLoadError) -> CliError {
    match error {
        nexa_analysis::PackageLoadError::Io(error) => {
            CliError::internal(format!("could not load {}: {error}", path.display()))
        }
        nexa_analysis::PackageLoadError::Lock(error) => {
            CliError::environment(format!("invalid nexa.lock in {}: {error}", path.display()))
        }
        nexa_analysis::PackageLoadError::RootEscape(escaped) => CliError::environment(format!(
            "package source path escapes {}: {}",
            path.display(),
            escaped.display()
        )),
        nexa_analysis::PackageLoadError::MissingEntry(entry) => {
            CliError::diagnostic(format!("Package entry source is missing: {entry}"))
        }
        other => CliError::diagnostic(format!("invalid package {}: {other}", path.display())),
    }
}

fn reject_overlapping_roots(sources: &[LoadedSource]) -> CliResult<()> {
    for (index, left) in sources.iter().enumerate() {
        for right in &sources[index + 1..] {
            if left.root.starts_with(&right.root) || right.root.starts_with(&left.root) {
                return Err(CliError::environment(format!(
                    "source roots overlap: `{}` ({}) and `{}` ({})",
                    left.id,
                    left.root.display(),
                    right.id,
                    right.root.display()
                )));
            }
        }
    }
    Ok(())
}

fn resolve_within(root: &Path, path: &Path) -> CliResult<PathBuf> {
    reject_symlink_below(root, path)?;
    let resolved = root.join(path).canonicalize().map_err(|error| {
        CliError::environment(format!("could not resolve {}: {error}", path.display()))
    })?;
    if !resolved.starts_with(root) {
        return Err(CliError::environment(format!(
            "path escapes project root: {}",
            path.display()
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexa_embed::{
        ActivationSet, CandidateBuildContext, CapabilitySet, MemoryPackage, MemorySource,
        PackagePolicy, PackageRuntimeLimits, PackageSource, TrustLevel,
    };

    use super::*;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nexa-cli-lockless-fingerprint-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lockless_package_has_identical_cli_and_memory_source_build_identity() {
        const CONTRACT: &str = "interface LocklessHost {}\n";
        const MANIFEST: &str = "schema = 2\n\
kind = \"application\"\n\
id = \"example.lockless\"\n\
name = \"Lockless\"\n\
version = \"1.0.0\"\n\
source_root = \"src\"\n\
entry = \"example.lockless\"\n\
activation = \"default-enabled\"\n\
handler_fuel = 20000\n\
capabilities = []\n";
        const SOURCE: &str = "module example.lockless;\n\
pub fn value() -> i32 {\n\
    return 1;\n\
}\n";

        let directory = TestDirectory::new();
        let package = directory.0.join("packages/app");
        fs::create_dir_all(package.join("src/example")).expect("create package source directory");
        fs::write(directory.0.join("lockless.nidl"), CONTRACT).expect("write Host contract");
        fs::write(package.join("package.toml"), MANIFEST).expect("write Package Manifest");
        fs::write(package.join("src/example/lockless.nexa"), SOURCE).expect("write Package source");
        fs::write(
            directory.0.join("nexa.dev.toml"),
            "schema = 2\n\
contract = \"lockless.nidl\"\n\
\n\
[[sources]]\n\
id = \"lockless\"\n\
root = \"packages\"\n\
trust = \"first-party\"\n\
activation = [\"default-enabled\"]\n\
capabilities = []\n\
allow_entitlement = false\n\
max_packages = 1\n\
\n\
[sources.limits]\n\
handler_fuel = 20000\n\
cumulative_budget = 20000\n\
heap_objects = 4096\n\
host_resources = 1024\n\
tasks = 128\n\
release_records = 2048\n",
        )
        .expect("write Project Manifest");

        assert!(
            !package.join("nexa.lock").exists(),
            "the regression must exercise an absent Lockfile"
        );
        let project =
            LoadedProject::load(&directory.0.join("nexa.dev.toml")).expect("load CLI project");
        let exact_host_source = project
            .host_contract_snapshot()
            .expect("retain exact CLI Host source");
        let cli_build = project
            .resolved_builds(true)
            .expect("resolve CLI project")
            .pop()
            .expect("one CLI build");

        let policy = PackagePolicy {
            trust: TrustLevel::FirstParty,
            capability_ceiling: CapabilitySet::default(),
            allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
            max_packages: 1,
            runtime_limits: PackageRuntimeLimits::default(),
            allow_entitlement: false,
        };
        let memory_build = MemorySource::new(SourceId::new("lockless").expect("Source ID"), policy)
            .package(
                MemoryPackage::new("app", MANIFEST).source("src/example/lockless.nexa", SOURCE),
            )
            .discover(&CandidateBuildContext::with_source(
                exact_host_source.identity,
                exact_host_source.source.as_bytes().to_vec(),
            ))
            .expect("resolve MemorySource for Engine")
            .pop()
            .expect("one Engine candidate");

        assert!(cli_build.input.lock.is_none());
        assert!(memory_build.build_input.lock.is_none());
        assert!(cli_build.input.canonical_lock_graph.is_empty());
        assert!(memory_build.build_input.canonical_lock_graph.is_empty());
        assert_eq!(
            cli_build.input.fingerprint_input.as_ref(),
            memory_build.build_input.fingerprint_input.as_ref()
        );
        assert_eq!(
            cli_build.build_fingerprint,
            memory_build.candidate.build_fingerprint
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn host_contract_uri_changes_build_identity_and_artifact_source_registry() {
        const CONTRACT: &str = "interface HostIdentity {}\n";
        const MANIFEST: &str = "schema = 2\n\
kind = \"application\"\n\
id = \"example.hostidentity\"\n\
name = \"Host Identity\"\n\
version = \"1.0.0\"\n\
source_root = \"src\"\n\
entry = \"example.hostidentity\"\n\
activation = \"default-enabled\"\n\
handler_fuel = 20000\n\
capabilities = []\n";
        const SOURCE: &str = "module example.hostidentity;\n\
pub fn value() -> i32 {\n\
    return 1;\n\
}\n";
        const CONFIG: &str = "schema = 2\n\
contract = \"contract-a.nidl\"\n\
\n\
[[sources]]\n\
id = \"host-identity\"\n\
root = \"packages\"\n\
trust = \"first-party\"\n\
activation = [\"default-enabled\"]\n\
capabilities = []\n\
allow_entitlement = false\n\
max_packages = 1\n\
\n\
[sources.limits]\n\
handler_fuel = 20000\n\
cumulative_budget = 20000\n\
heap_objects = 4096\n\
host_resources = 1024\n\
tasks = 128\n\
release_records = 2048\n";

        let directory = TestDirectory::new();
        let package = directory.0.join("packages/app");
        let config_path = directory.0.join("nexa.dev.toml");
        let contract_a_path = directory.0.join("contract-a.nidl");
        let contract_b_path = directory.0.join("contract-b.nidl");
        fs::create_dir_all(package.join("src/example")).expect("create package source directory");
        fs::write(&contract_a_path, CONTRACT).expect("write first Host contract");
        fs::write(&contract_b_path, CONTRACT).expect("write moved Host contract");
        fs::write(package.join("package.toml"), MANIFEST).expect("write Package Manifest");
        fs::write(package.join("src/example/hostidentity.nexa"), SOURCE)
            .expect("write Package source");
        fs::write(&config_path, CONFIG).expect("write first Project Manifest");

        let project_a = LoadedProject::load(&config_path).expect("load first CLI project");
        let build_a = project_a
            .resolved_builds(true)
            .expect("resolve first CLI project")
            .pop()
            .expect("one first CLI build");
        let compiled_a = build_a
            .compile(1, None, &project_a.required_exports, false)
            .expect("compile first exact Host source");
        let artifact_a = compiled_a.product().expect("first product artifact");
        let canonical_contract_a = contract_a_path
            .canonicalize()
            .expect("canonical first Host contract path");
        let identity_a =
            nexa::SourceIdentity::standalone(canonical_contract_a.to_string_lossy().into_owned());
        let snapshot_a = artifact_a
            .source_files
            .diagnostic_sources()
            .get(&identity_a)
            .expect("first artifact retains the first Host URI");
        assert_eq!(snapshot_a.text(), CONTRACT);

        fs::write(
            &config_path,
            CONFIG.replace("contract-a.nidl", "contract-b.nidl"),
        )
        .expect("retarget Project Manifest to the moved Host contract");
        let project_b = LoadedProject::load(&config_path).expect("load retargeted CLI project");
        let build_b = project_b
            .resolved_builds(true)
            .expect("resolve retargeted CLI project")
            .pop()
            .expect("one retargeted CLI build");
        assert_ne!(
            build_a.build_fingerprint, build_b.build_fingerprint,
            "moving an exact external Host source must invalidate linked freshness"
        );

        let compiled_b = build_b
            .compile(1, None, &project_b.required_exports, false)
            .expect("compile retargeted exact Host source");
        let artifact_b = compiled_b.product().expect("retargeted product artifact");
        assert_ne!(
            artifact_a.linked_state_fingerprint, artifact_b.linked_state_fingerprint,
            "the linked-state identity must include the exact Host source URI"
        );
        let canonical_contract_b = contract_b_path
            .canonicalize()
            .expect("canonical moved Host contract path");
        let identity_b =
            nexa::SourceIdentity::standalone(canonical_contract_b.to_string_lossy().into_owned());
        let snapshot_b = artifact_b
            .source_files
            .diagnostic_sources()
            .get(&identity_b)
            .expect("retargeted artifact retains the active Host URI");
        assert_eq!(snapshot_b.text(), CONTRACT);
        assert!(
            artifact_b
                .source_files
                .diagnostic_sources()
                .get(&identity_a)
                .is_none(),
            "the active debug registry must not retain the stale Host URI"
        );
    }
}
