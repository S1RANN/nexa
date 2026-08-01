use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::source::{collect_source_paths, reject_symlink};
use crate::{
    CompilationLimits, LockError, LockFile, ManifestError, ModulePath, NormalizedPackagePath,
    PackageManifest, PackageSourceSet, SourceDiscoveryError, SourceRole, SourceSetBuilder,
    SourceSetError,
};

#[derive(Clone, Debug)]
pub struct LoadedPackageDirectory {
    pub directory: PathBuf,
    pub manifest_source: Arc<str>,
    pub manifest: Arc<PackageManifest>,
    pub production_sources: Arc<PackageSourceSet>,
    pub test_sources: Arc<PackageSourceSet>,
    pub lock: Option<Arc<LockFile>>,
}

/// Loads one schema-2 package directory without following links or accepting ambiguous paths.
///
/// Lock freshness depends on the resolved dependency catalog and is intentionally checked after
/// this filesystem-only step.
pub fn load_package_directory(
    directory: impl AsRef<Path>,
    limits: CompilationLimits,
) -> Result<LoadedPackageDirectory, PackageLoadError> {
    load_package_directory_impl(directory.as_ref(), limits, true)
}

/// Loads sources and the manifest while deliberately ignoring an existing `nexa.lock`.
///
/// This is the input mode for the explicit `nexa lock` writer, allowing it to repair malformed or
/// unsupported lockfiles. Build, check, dev, and Engine paths must use [`load_package_directory`].
pub fn load_package_directory_without_lock(
    directory: impl AsRef<Path>,
    limits: CompilationLimits,
) -> Result<LoadedPackageDirectory, PackageLoadError> {
    load_package_directory_impl(directory.as_ref(), limits, false)
}

fn load_package_directory_impl(
    directory: &Path,
    limits: CompilationLimits,
    parse_lock: bool,
) -> Result<LoadedPackageDirectory, PackageLoadError> {
    reject_symlink(directory).map_err(PackageLoadError::Discovery)?;
    let canonical_directory = fs::canonicalize(directory).map_err(PackageLoadError::Io)?;

    let manifest_path = directory.join("package.toml");
    reject_symlink(&manifest_path).map_err(PackageLoadError::Discovery)?;
    let manifest_source = read_utf8(&manifest_path)?;
    let manifest =
        Arc::new(PackageManifest::parse(&manifest_source).map_err(PackageLoadError::Manifest)?);

    let source_root = directory.join(manifest.source_root.as_path());
    let production = load_tree(
        directory,
        &canonical_directory,
        &source_root,
        manifest.id.clone(),
        SourceRole::Production,
        limits,
    )?;
    if let Some(entry) = manifest.expected_entry_source()
        && !production.units().keys().any(|key| key.path == entry)
    {
        return Err(PackageLoadError::MissingEntry(entry));
    }

    let tests_root = directory.join("tests");
    let tests = if tests_root.exists() {
        load_tree(
            directory,
            &canonical_directory,
            &tests_root,
            manifest.id.clone(),
            SourceRole::Test,
            limits,
        )?
    } else {
        SourceSetBuilder::new(manifest.id.clone(), limits)
            .build()
            .map_err(PackageLoadError::SourceSet)?
    };

    let lock_path = directory.join("nexa.lock");
    let lock = if parse_lock && lock_path.exists() {
        reject_symlink(&lock_path).map_err(PackageLoadError::Discovery)?;
        Some(Arc::new(
            LockFile::parse(&read_utf8(&lock_path)?).map_err(PackageLoadError::Lock)?,
        ))
    } else {
        None
    };

    Ok(LoadedPackageDirectory {
        directory: canonical_directory,
        manifest_source: manifest_source.into(),
        manifest,
        production_sources: Arc::new(production),
        test_sources: Arc::new(tests),
        lock,
    })
}

fn load_tree(
    package_directory: &Path,
    canonical_package_directory: &Path,
    tree_root: &Path,
    package_id: crate::PackageId,
    role: SourceRole,
    limits: CompilationLimits,
) -> Result<PackageSourceSet, PackageLoadError> {
    reject_symlink(tree_root).map_err(PackageLoadError::Discovery)?;
    let canonical_tree = fs::canonicalize(tree_root).map_err(PackageLoadError::Io)?;
    if !canonical_tree.starts_with(canonical_package_directory) {
        return Err(PackageLoadError::RootEscape(canonical_tree));
    }
    let mut paths = Vec::new();
    collect_source_paths(tree_root, &mut paths).map_err(PackageLoadError::Discovery)?;
    paths.sort();
    let mut builder = SourceSetBuilder::new(package_id, limits);
    for path in paths {
        let canonical_path = fs::canonicalize(&path).map_err(PackageLoadError::Io)?;
        if !canonical_path.starts_with(canonical_package_directory) {
            return Err(PackageLoadError::RootEscape(canonical_path));
        }
        let relative = path
            .strip_prefix(package_directory)
            .map_err(|_| PackageLoadError::RootEscape(path.clone()))?;
        let normalized =
            NormalizedPackagePath::from_path(relative).map_err(PackageLoadError::Identity)?;
        let text = read_utf8(&path)?;
        if role == SourceRole::Production {
            let expected_module =
                ModulePath::from_source_path(&normalized).map_err(PackageLoadError::Identity)?;
            let declared = validate_module_source_for_role(&normalized, &text, role)?;
            debug_assert_eq!(declared, expected_module);
        }
        builder
            .add(
                NormalizedPackagePath::from_path(relative).map_err(PackageLoadError::Identity)?,
                text,
                role,
            )
            .map_err(PackageLoadError::SourceSet)?;
    }
    builder.build().map_err(PackageLoadError::SourceSet)
}

fn read_utf8(path: &Path) -> Result<String, PackageLoadError> {
    let bytes = fs::read(path).map_err(PackageLoadError::Io)?;
    String::from_utf8(bytes).map_err(|_| PackageLoadError::NonUtf8File(path.to_path_buf()))
}

/// Derives one in-memory production source's module identity using the same path rules as
/// directory discovery.
///
/// Source syntax is deliberately left to package analysis so filesystem and in-memory sources
/// receive the same structured diagnostics from the selected Product or Test target.
pub fn validate_module_source(
    path: &NormalizedPackagePath,
    source: &str,
) -> Result<ModulePath, PackageLoadError> {
    validate_module_source_for_role(path, source, SourceRole::Production)
}

/// Derives a source's path-owned module identity without parsing its text.
///
/// In particular, an invalid Test source must remain loadable as an immutable snapshot so a later
/// Test analysis can diagnose it. Product discovery likewise does not become a second parser.
pub fn validate_module_source_for_role(
    path: &NormalizedPackagePath,
    _source: &str,
    role: SourceRole,
) -> Result<ModulePath, PackageLoadError> {
    match role {
        SourceRole::Production => ModulePath::from_source_path(path),
        SourceRole::Test => path
            .as_str()
            .strip_prefix("tests/")
            .and_then(|value| value.strip_suffix(".nexa"))
            .ok_or_else(|| crate::IdentityError::Invalid {
                kind: "test source path",
                value: path.to_string(),
            })
            .and_then(|relative| ModulePath::new(format!("test.{}", relative.replace('/', ".")))),
    }
    .map_err(PackageLoadError::Identity)
}

#[derive(Debug)]
pub enum PackageLoadError {
    Io(std::io::Error),
    Discovery(SourceDiscoveryError),
    Manifest(ManifestError),
    Lock(LockError),
    Identity(crate::IdentityError),
    SourceSet(SourceSetError),
    RootEscape(PathBuf),
    NonUtf8File(PathBuf),
    MissingEntry(NormalizedPackagePath),
}

impl fmt::Display for PackageLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PackageLoadError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_identity_is_derived_only_from_the_source_path() {
        let path = NormalizedPackagePath::new("src/food/effects.nexa").unwrap();
        assert_eq!(
            validate_module_source(&path, "pub fn score() -> i32 { return 1; }")
                .unwrap()
                .as_str(),
            "food.effects"
        );
    }

    #[test]
    fn test_module_identity_is_derived_from_the_test_path() {
        let path = NormalizedPackagePath::new("tests/food/effects.nexa").unwrap();
        assert_eq!(
            validate_module_source_for_role(&path, "", SourceRole::Test)
                .unwrap()
                .as_str(),
            "test.food.effects"
        );
    }
}
