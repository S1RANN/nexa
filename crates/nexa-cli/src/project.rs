use std::path::{Path, PathBuf};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilityId, CapabilitySet, EngineDiagnostic,
    EngineDiagnosticStage, ExportRequirement, PackageCandidate, PackageManifest, PackagePolicy,
    PackageRuntimeLimits, SourceId, TrustLevel,
};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectConfig {
    pub schema: u32,
    pub contract: PathBuf,
    pub package_roots: Vec<PathBuf>,
    #[serde(default)]
    pub required_exports: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct LoadedProject {
    pub config_path: PathBuf,
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub contract_source: String,
    pub idl: nexa_idl::Idl,
    pub required_exports: Vec<ExportRequirement>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectCheck {
    pub checked_packages: usize,
    pub diagnostics: Vec<EngineDiagnostic>,
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
        if config.schema != 1 {
            return Err(format!(
                "unsupported nexa.dev.toml schema {}",
                config.schema
            ));
        }
        let contract_path = resolve_within(&root, &config.contract)?;
        let contract_source = std::fs::read_to_string(&contract_path)
            .map_err(|error| format!("could not read {}: {error}", contract_path.display()))?;
        let idl = nexa_idl::parse(&contract_source)
            .map_err(|error| format!("invalid {}: {error}", contract_path.display()))?;
        let mut required_exports = Vec::new();
        for name in &config.required_exports {
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
                stable_id: nexa_idl::export_stable_id(&idl, export),
                signature: nexa_idl::export_signature(&idl, export),
            });
        }
        Ok(Self {
            config_path,
            root,
            config,
            contract_source,
            idl,
            required_exports,
        })
    }

    pub fn package_directories(&self) -> Result<Vec<PathBuf>, String> {
        let mut packages = Vec::new();
        for configured_root in &self.config.package_roots {
            let root = resolve_within(&self.root, configured_root)?;
            if root.join("package.toml").is_file() {
                packages.push(root);
                continue;
            }
            let mut children = std::fs::read_dir(&root)
                .map_err(|error| format!("could not read {}: {error}", root.display()))?
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .filter(|path| path.join("package.toml").is_file())
                .collect::<Vec<_>>();
            children.sort();
            packages.extend(children);
        }
        packages.sort();
        packages.dedup();
        Ok(packages)
    }

    pub fn check(&self) -> Result<ProjectCheck, String> {
        let mut check = ProjectCheck::default();
        for package in self.package_directories()? {
            check.checked_packages += 1;
            if let Err(diagnostic) = check_package(&package, &self.idl, &self.required_exports) {
                check.diagnostics.push(diagnostic);
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
) -> Result<nexa_embed::CompiledPackageArtifact, EngineDiagnostic> {
    let (source_id, candidate) = load_package_candidate(directory)?;
    nexa_embed::compile_package(idl, required_exports, &source_id, &candidate)
}

#[allow(clippy::result_large_err)]
pub fn load_package_candidate(
    directory: &Path,
) -> Result<(SourceId, PackageCandidate), EngineDiagnostic> {
    let source_id = SourceId::new("project").expect("static source id is valid");
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
    let policy = development_policy(&manifest_source).map_err(|message| {
        EngineDiagnostic::without_source(
            None,
            Some(source_id.clone()),
            EngineDiagnosticStage::Manifest,
            nexa::ErrorCode::NX7002,
            message,
        )
    })?;
    let manifest = PackageManifest::parse(&manifest_source, &policy).map_err(|error| {
        let message = error.to_string();
        let policy_error = message.contains("Capability")
            || message.contains("Activation")
            || message.contains("Entitlement");
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
            message,
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
            Some(source_id),
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
    Ok((source_id, candidate))
}

fn development_policy(manifest: &str) -> Result<PackagePolicy, String> {
    let value: toml::Value =
        toml::from_str(manifest).map_err(|error| format!("invalid package manifest: {error}"))?;
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
        max_packages: 4_096,
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
