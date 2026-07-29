use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilityId, CapabilitySet, EngineDiagnostic,
    EngineDiagnosticStage, ExportRequirement, PackageCandidate, PackageId, PackageManifest,
    PackagePolicy, PackageRuntimeLimits, SourceId, TrustLevel,
};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    pub schema: u32,
    pub contract: PathBuf,
    #[serde(default)]
    pub required_exports: Vec<String>,
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigTrustLevel {
    FirstParty,
    Trusted,
}

impl From<ConfigTrustLevel> for TrustLevel {
    fn from(value: ConfigTrustLevel) -> Self {
        match value {
            ConfigTrustLevel::FirstParty => Self::FirstParty,
            ConfigTrustLevel::Trusted => Self::Trusted,
        }
    }
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

impl From<RuntimeLimitsConfig> for PackageRuntimeLimits {
    fn from(value: RuntimeLimitsConfig) -> Self {
        Self {
            handler_fuel: value.handler_fuel,
            cumulative_budget: value.cumulative_budget,
            heap_objects: value.heap_objects,
            host_resources: value.host_resources,
            tasks: value.tasks,
            release_records: value.release_records,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LoadedSource {
    pub id: SourceId,
    pub root: PathBuf,
    pub policy: PackagePolicy,
}

#[derive(Clone, Debug)]
pub struct DiscoveredPackage {
    pub directory: PathBuf,
    pub source_id: SourceId,
    pub policy: PackagePolicy,
}

#[derive(Clone, Debug)]
pub struct LoadedProject {
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub sources: Vec<LoadedSource>,
    pub contract_source: String,
    pub idl: nexa_idl::Idl,
    pub required_exports: Vec<ExportRequirement>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectCheck {
    pub checked_packages: usize,
    pub diagnostics: Vec<EngineDiagnostic>,
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
    pub fn load(path: &Path) -> Result<Self, String> {
        let config_path = path
            .canonicalize()
            .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
        let root = config_path
            .parent()
            .ok_or("project configuration has no parent directory")?
            .to_path_buf();
        let source = std::fs::read_to_string(&config_path)
            .map_err(|error| format!("could not read {}: {error}", config_path.display()))?;
        let config: ProjectConfig = toml::from_str(&source)
            .map_err(|error| format!("invalid {}: {error}", config_path.display()))?;
        if config.schema != 2 {
            return Err(format!(
                "unsupported nexa.dev.toml schema {}; expected schema 2",
                config.schema
            ));
        }
        if config.sources.is_empty() {
            return Err("nexa.dev.toml must declare at least one source".into());
        }

        let mut source_ids = BTreeSet::new();
        let mut sources = Vec::with_capacity(config.sources.len());
        for configured in &config.sources {
            let id = SourceId::new(configured.id.clone())
                .map_err(|error| format!("invalid source id `{}`: {error}", configured.id))?;
            if !source_ids.insert(id.clone()) {
                return Err(format!("duplicate source id `{id}`"));
            }
            let source_root = resolve_within(&root, &configured.root)?;
            if !source_root.is_dir() {
                return Err(format!(
                    "source root is not a directory: {}",
                    source_root.display()
                ));
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
        let contract_source = std::fs::read_to_string(&contract_path)
            .map_err(|error| format!("could not read {}: {error}", contract_path.display()))?;
        let idl = nexa_idl::parse(&contract_source)
            .map_err(|error| format!("invalid {}: {error}", contract_path.display()))?;
        let required_exports =
            resolve_required_exports(&idl, &config.required_exports, &contract_path)?;
        Ok(Self {
            config_path,
            root,
            config,
            sources,
            contract_source,
            idl,
            required_exports,
        })
    }

    pub fn package_directories(&self) -> Result<Vec<DiscoveredPackage>, String> {
        let mut packages = Vec::new();
        for source in &self.sources {
            let mut source_packages = if source.root.join("package.toml").is_file() {
                vec![source.root.clone()]
            } else {
                let mut children = std::fs::read_dir(&source.root)
                    .map_err(|error| format!("could not read {}: {error}", source.root.display()))?
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path())
                    .filter(|path| path.join("package.toml").is_file())
                    .collect::<Vec<_>>();
                children.sort();
                children
            };
            if source_packages.len() > source.policy.max_packages {
                return Err(format!(
                    "source `{}` contains {} packages, exceeding max_packages {}",
                    source.id,
                    source_packages.len(),
                    source.policy.max_packages
                ));
            }
            packages.extend(
                source_packages
                    .drain(..)
                    .map(|directory| DiscoveredPackage {
                        directory,
                        source_id: source.id.clone(),
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

    pub fn source_for_directory(&self, directory: &Path) -> Result<&LoadedSource, String> {
        let directory = directory
            .canonicalize()
            .map_err(|error| format!("could not resolve {}: {error}", directory.display()))?;
        self.sources
            .iter()
            .find(|source| directory.starts_with(&source.root))
            .ok_or_else(|| {
                format!(
                    "package {} is not inside any configured source root",
                    directory.display()
                )
            })
    }

    #[allow(clippy::result_large_err)]
    pub fn load_candidate(
        &self,
        directory: &Path,
    ) -> Result<(SourceId, PackageCandidate), EngineDiagnostic> {
        let source = self.source_for_directory(directory).map_err(|message| {
            EngineDiagnostic::without_source(
                None,
                None,
                EngineDiagnosticStage::Policy,
                nexa::ErrorCode::NX7003,
                message,
            )
        })?;
        load_package_candidate(directory, &source.id, &source.policy)
    }

    pub fn check(&self) -> Result<ProjectCheck, String> {
        let mut check = ProjectCheck::default();
        let mut package_ids = BTreeMap::<PackageId, (PathBuf, SourceId)>::new();
        for package in self.package_directories()? {
            check.checked_packages += 1;
            match load_package_candidate(&package.directory, &package.source_id, &package.policy) {
                Ok((source_id, candidate)) => {
                    if let Some((first_path, first_source)) = package_ids.insert(
                        candidate.manifest.id.clone(),
                        (package.directory.clone(), source_id.clone()),
                    ) {
                        check.diagnostics.push(EngineDiagnostic::without_source(
                            Some(candidate.manifest.id),
                            Some(source_id),
                            EngineDiagnosticStage::Policy,
                            nexa::ErrorCode::NX7003,
                            format!(
                                "duplicate Package ID; first declared at {} in source `{first_source}`, repeated at {}",
                                first_path.display(),
                                package.directory.display()
                            ),
                        ));
                        continue;
                    }
                    if let Err(diagnostic) = nexa_embed::compile_package(
                        &self.idl,
                        &self.required_exports,
                        &source_id,
                        &candidate,
                    ) {
                        check.diagnostics.push(diagnostic);
                    }
                }
                Err(diagnostic) => check.diagnostics.push(diagnostic),
            }
        }
        Ok(check)
    }
}

#[allow(clippy::result_large_err)]
pub fn check_package(
    directory: &Path,
    idl: &nexa_idl::Idl,
    required_exports: &[ExportRequirement],
    source_id: &SourceId,
    policy: &PackagePolicy,
) -> Result<nexa_embed::CompiledPackageArtifact, EngineDiagnostic> {
    let (_, candidate) = load_package_candidate(directory, source_id, policy)?;
    nexa_embed::compile_package(idl, required_exports, source_id, &candidate)
}

#[allow(clippy::result_large_err)]
pub fn load_package_candidate(
    directory: &Path,
    source_id: &SourceId,
    policy: &PackagePolicy,
) -> Result<(SourceId, PackageCandidate), EngineDiagnostic> {
    let manifest_path = directory.join("package.toml");
    let manifest_source = std::fs::read_to_string(&manifest_path).map_err(|error| {
        EngineDiagnostic::without_source(
            None,
            Some(source_id.clone()),
            EngineDiagnosticStage::SourceDiscovery,
            nexa::ErrorCode::NX7001,
            format!("could not read {}: {error}", manifest_path.display()),
        )
    })?;
    let manifest = PackageManifest::parse(&manifest_source, policy).map_err(|error| {
        let policy_error = matches!(
            error,
            nexa_embed::ManifestError::ActivationNotAllowed
                | nexa_embed::ManifestError::CapabilityCeiling
                | nexa_embed::ManifestError::RuntimeLimit
                | nexa_embed::ManifestError::EntitlementNotAllowed
        );
        EngineDiagnostic::without_source(
            None,
            Some(source_id.clone()),
            if policy_error {
                EngineDiagnosticStage::Policy
            } else {
                EngineDiagnosticStage::Manifest
            },
            if policy_error {
                nexa::ErrorCode::NX7003
            } else {
                nexa::ErrorCode::NX7002
            },
            error.to_string(),
        )
    })?;
    let canonical_root = directory.canonicalize().map_err(|error| {
        EngineDiagnostic::without_source(
            Some(manifest.id.clone()),
            Some(source_id.clone()),
            EngineDiagnosticStage::SourceDiscovery,
            nexa::ErrorCode::NX7001,
            error.to_string(),
        )
    })?;
    let entry_path = canonical_root
        .join(manifest.entry.as_path())
        .canonicalize()
        .map_err(|error| {
            EngineDiagnostic::without_source(
                Some(manifest.id.clone()),
                Some(source_id.clone()),
                EngineDiagnosticStage::SourceDiscovery,
                nexa::ErrorCode::NX7001,
                error.to_string(),
            )
        })?;
    if !entry_path.starts_with(&canonical_root) {
        return Err(EngineDiagnostic::without_source(
            Some(manifest.id.clone()),
            Some(source_id.clone()),
            EngineDiagnosticStage::Policy,
            nexa::ErrorCode::NX7003,
            "package entry escapes the package directory",
        ));
    }
    let entry_source = std::fs::read_to_string(&entry_path).map_err(|error| {
        EngineDiagnostic::without_source(
            Some(manifest.id.clone()),
            Some(source_id.clone()),
            EngineDiagnosticStage::SourceDiscovery,
            nexa::ErrorCode::NX7001,
            error.to_string(),
        )
    })?;
    let candidate = PackageCandidate::new(manifest, manifest_source, entry_source);
    Ok((source_id.clone(), candidate))
}

pub fn load_policy(path: &Path) -> Result<(SourceId, PackagePolicy), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let config: PolicyFile =
        toml::from_str(&source).map_err(|error| format!("invalid {}: {error}", path.display()))?;
    if config.schema != 1 {
        return Err(format!(
            "unsupported package policy schema {}; expected schema 1",
            config.schema
        ));
    }
    let id = SourceId::new(config.id)
        .map_err(|error| format!("invalid package policy source id: {error}"))?;
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

pub fn manifest_validation_policy(source: &str) -> Result<PackagePolicy, String> {
    let value: toml::Value =
        toml::from_str(source).map_err(|error| format!("invalid package manifest: {error}"))?;
    let capabilities = value
        .get("capabilities")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "manifest capability must be a string".to_owned())
                .and_then(|value| CapabilityId::new(value).map_err(|error| error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PackagePolicy {
        trust: TrustLevel::FirstParty,
        capability_ceiling: CapabilitySet::new(capabilities),
        allowed_activation: ActivationSet::new([
            ActivationPolicy::Required,
            ActivationPolicy::DefaultEnabled,
            ActivationPolicy::UserControlled,
            ActivationPolicy::Programmatic,
        ]),
        max_packages: usize::MAX,
        runtime_limits: PackageRuntimeLimits {
            handler_fuel: u64::MAX,
            cumulative_budget: u64::MAX,
            heap_objects: u32::MAX,
            host_resources: u32::MAX,
            tasks: u32::MAX,
            release_records: usize::MAX,
        },
        allow_entitlement: true,
    })
}

fn policy_from_parts(
    trust: ConfigTrustLevel,
    activation: &[ActivationPolicy],
    capabilities: &[String],
    allow_entitlement: bool,
    max_packages: usize,
    limits: RuntimeLimitsConfig,
) -> Result<PackagePolicy, String> {
    if activation.is_empty() {
        return Err("source policy activation set must not be empty".into());
    }
    if max_packages == 0 {
        return Err("source policy max_packages must be greater than zero".into());
    }
    let unique_activation = activation.iter().copied().collect::<BTreeSet<_>>();
    if unique_activation.len() != activation.len() {
        return Err("duplicate activation mode in source policy".into());
    }
    let mut capability_ids = Vec::with_capacity(capabilities.len());
    let mut seen = BTreeSet::new();
    for capability in capabilities {
        let id = CapabilityId::new(capability.clone())
            .map_err(|error| format!("invalid capability `{capability}`: {error}"))?;
        if !seen.insert(id.clone()) {
            return Err(format!(
                "duplicate capability `{capability}` in source policy"
            ));
        }
        capability_ids.push(id);
    }
    let activation_set = ActivationSet::new(unique_activation);
    Ok(PackagePolicy {
        trust: trust.into(),
        capability_ceiling: CapabilitySet::new(capability_ids),
        allowed_activation: activation_set,
        max_packages,
        runtime_limits: limits.into(),
        allow_entitlement,
    })
}

fn resolve_required_exports(
    idl: &nexa_idl::Idl,
    names: &[String],
    contract_path: &Path,
) -> Result<Vec<ExportRequirement>, String> {
    let mut required_exports = Vec::new();
    for name in names {
        let export = idl
            .exports
            .iter()
            .find(|export| export.name == *name)
            .ok_or_else(|| {
                format!(
                    "required export `{name}` is not declared by {}",
                    contract_path.display()
                )
            })?;
        required_exports.push(ExportRequirement {
            name: name.clone(),
            stable_id: nexa_idl::export_stable_id(idl, export),
            signature: nexa_idl::export_signature(idl, export),
        });
    }
    Ok(required_exports)
}

fn reject_overlapping_roots(sources: &[LoadedSource]) -> Result<(), String> {
    for (index, left) in sources.iter().enumerate() {
        for right in &sources[index + 1..] {
            if left.root.starts_with(&right.root) || right.root.starts_with(&left.root) {
                return Err(format!(
                    "source roots overlap: `{}` ({}) and `{}` ({})",
                    left.id,
                    left.root.display(),
                    right.id,
                    right.root.display()
                ));
            }
        }
    }
    Ok(())
}

fn resolve_within(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    if !resolved.starts_with(root) {
        return Err(format!("path escapes project root: {}", path.display()));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexa_embed::{
        ActivationPolicy, ActivationSet, CapabilityId, CapabilitySet, PackagePolicy,
        PackageRuntimeLimits, SourceId, TrustLevel,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    fn temporary_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nexa-m3r1-cli-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn policy(
        capabilities: &[&str],
        activation: &[ActivationPolicy],
        allow_entitlement: bool,
    ) -> PackagePolicy {
        PackagePolicy {
            trust: TrustLevel::Trusted,
            capability_ceiling: CapabilitySet::new(
                capabilities
                    .iter()
                    .map(|value| CapabilityId::new(*value).expect("capability")),
            ),
            allowed_activation: ActivationSet::new(activation.iter().copied()),
            max_packages: 4,
            runtime_limits: PackageRuntimeLimits {
                handler_fuel: 20_000,
                cumulative_budget: 200_000,
                heap_objects: 4_096,
                host_resources: 256,
                tasks: 8,
                release_records: 512,
            },
            allow_entitlement,
        }
    }

    fn manifest(
        id: &str,
        activation: &str,
        capabilities: &str,
        entitlement: &str,
        handler_fuel: u64,
    ) -> String {
        format!(
            "schema = 1\n\
             id = \"{id}\"\n\
             name = \"Policy Test\"\n\
             version = \"1.0.0\"\n\
             entry = \"main.nexa\"\n\
             activation = \"{activation}\"\n\
             handler_fuel = {handler_fuel}\n\
             capabilities = [{capabilities}]\n\
             entitlement = \"{entitlement}\"\n"
        )
    }

    fn write_package(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let package = root.join(name);
        std::fs::create_dir_all(&package).expect("package directory");
        std::fs::write(package.join("package.toml"), manifest).expect("manifest");
        std::fs::write(
            package.join("main.nexa"),
            "module policy.fixture;\nfn Value() -> i32 { return 1; }",
        )
        .expect("entry source");
        package
    }

    fn assert_policy_rejection(manifest: &str, policy: &PackagePolicy) {
        let root = temporary_root("policy-rejection");
        let package = write_package(&root, "package", manifest);
        let diagnostic = super::load_package_candidate(
            &package,
            &SourceId::new("policy-test").expect("source ID"),
            policy,
        )
        .expect_err("Package policy must reject the manifest");
        assert_eq!(diagnostic.diagnostic.code, nexa::ErrorCode::NX7003);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn full_policy_rejects_capability_activation_entitlement_and_limits() {
        let strict = policy(
            &["allowed.read"],
            &[ActivationPolicy::UserControlled],
            false,
        );
        assert_policy_rejection(
            &manifest(
                "policy.capability",
                "user-controlled",
                "\"denied.write\"",
                "",
                20_000,
            ),
            &strict,
        );
        assert_policy_rejection(
            &manifest("policy.activation", "required", "", "", 20_000),
            &strict,
        );
        assert_policy_rejection(
            &manifest(
                "policy.entitlement",
                "user-controlled",
                "",
                "paid.feature",
                20_000,
            ),
            &strict,
        );
        assert_policy_rejection(
            &manifest("policy.limit", "user-controlled", "", "", 20_001),
            &strict,
        );
    }

    #[test]
    fn project_rejects_duplicate_source_ids_overlapping_roots_and_path_escape() {
        let root = temporary_root("project-shape");
        std::fs::create_dir_all(root.join("packages/nested")).expect("source roots");
        std::fs::write(root.join("api.nidl"), "interface TestHost {}").expect("IDL");
        let limits = "handler_fuel = 20000\n\
                      cumulative_budget = 200000\n\
                      heap_objects = 4096\n\
                      host_resources = 256\n\
                      tasks = 8\n\
                      release_records = 512\n";
        let source = |id: &str, source_root: &str| {
            format!(
                "[[sources]]\n\
                 id = \"{id}\"\n\
                 root = \"{source_root}\"\n\
                 trust = \"trusted\"\n\
                 activation = [\"user-controlled\"]\n\
                 capabilities = []\n\
                 allow_entitlement = false\n\
                 max_packages = 4\n\
                 [sources.limits]\n{limits}"
            )
        };
        std::fs::write(
            root.join("duplicate.toml"),
            format!(
                "schema = 2\ncontract = \"api.nidl\"\n{}{}",
                source("same", "packages"),
                source("same", "packages/nested")
            ),
        )
        .expect("duplicate config");
        assert!(
            super::LoadedProject::load(&root.join("duplicate.toml"))
                .expect_err("duplicate source ID")
                .contains("duplicate source id")
        );
        std::fs::write(
            root.join("overlap.toml"),
            format!(
                "schema = 2\ncontract = \"api.nidl\"\n{}{}",
                source("outer", "packages"),
                source("inner", "packages/nested")
            ),
        )
        .expect("overlap config");
        assert!(
            super::LoadedProject::load(&root.join("overlap.toml"))
                .expect_err("overlapping roots")
                .contains("overlap")
        );
        std::fs::write(
            root.join("escape.toml"),
            format!(
                "schema = 2\ncontract = \"../outside.nidl\"\n{}",
                source("safe", "packages")
            ),
        )
        .expect("escape config");
        assert!(super::LoadedProject::load(&root.join("escape.toml")).is_err());
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn project_selects_policy_by_source_and_rejects_wrong_source() {
        let root = temporary_root("source-selection");
        let allowed = root.join("allowed");
        let denied = root.join("denied");
        std::fs::create_dir_all(&allowed).expect("allowed root");
        std::fs::create_dir_all(&denied).expect("denied root");
        write_package(
            &denied,
            "package",
            &manifest(
                "policy.wrong-source",
                "user-controlled",
                "\"allowed.read\"",
                "",
                20_000,
            ),
        );
        std::fs::write(root.join("api.nidl"), "interface TestHost {}").expect("IDL");
        std::fs::write(
            root.join("nexa.dev.toml"),
            "schema = 2\n\
             contract = \"api.nidl\"\n\
             [[sources]]\n\
             id = \"allowed\"\n\
             root = \"allowed\"\n\
             trust = \"trusted\"\n\
             activation = [\"user-controlled\"]\n\
             capabilities = [\"allowed.read\"]\n\
             max_packages = 4\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 200000\n\
             heap_objects = 4096\n\
             host_resources = 256\n\
             tasks = 8\n\
             release_records = 512\n\
             [[sources]]\n\
             id = \"denied\"\n\
             root = \"denied\"\n\
             trust = \"trusted\"\n\
             activation = [\"user-controlled\"]\n\
             capabilities = []\n\
             max_packages = 4\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 200000\n\
             heap_objects = 4096\n\
             host_resources = 256\n\
             tasks = 8\n\
             release_records = 512\n",
        )
        .expect("project config");
        let project =
            super::LoadedProject::load(&root.join("nexa.dev.toml")).expect("loaded project");
        let report = project.check().expect("project check");
        assert_eq!(report.checked_packages, 1);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].diagnostic.code,
            nexa::ErrorCode::NX7003
        );
        let outside = temporary_root("outside-source");
        std::fs::create_dir_all(&outside).expect("outside package");
        assert!(project.source_for_directory(&outside).is_err());
        std::fs::remove_dir_all(outside).expect("outside cleanup");
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn project_reports_duplicate_package_ids_across_sources() {
        let root = temporary_root("duplicate-package");
        let left = root.join("left");
        let right = root.join("right");
        std::fs::create_dir_all(&left).expect("left root");
        std::fs::create_dir_all(&right).expect("right root");
        let duplicate = manifest("policy.duplicate", "user-controlled", "", "", 20_000);
        write_package(&left, "first", &duplicate);
        write_package(&right, "second", &duplicate);
        std::fs::write(root.join("api.nidl"), "interface TestHost {}").expect("IDL");
        let source = |id: &str, path: &str| {
            format!(
                "[[sources]]\n\
                 id = \"{id}\"\n\
                 root = \"{path}\"\n\
                 trust = \"trusted\"\n\
                 activation = [\"user-controlled\"]\n\
                 capabilities = []\n\
                 max_packages = 4\n\
                 [sources.limits]\n\
                 handler_fuel = 20000\n\
                 cumulative_budget = 200000\n\
                 heap_objects = 4096\n\
                 host_resources = 256\n\
                 tasks = 8\n\
                 release_records = 512\n"
            )
        };
        std::fs::write(
            root.join("nexa.dev.toml"),
            format!(
                "schema = 2\ncontract = \"api.nidl\"\n{}{}",
                source("left", "left"),
                source("right", "right")
            ),
        )
        .expect("project config");
        let project =
            super::LoadedProject::load(&root.join("nexa.dev.toml")).expect("loaded project");
        let report = project.check().expect("project check");
        assert_eq!(report.checked_packages, 2);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.diagnostic.code == nexa::ErrorCode::NX7003)
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
