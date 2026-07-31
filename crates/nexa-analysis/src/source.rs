use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{
    ArtifactFileId, IdentityError, ModulePath, NormalizedPackagePath, PackageId, SourceKey,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceRole {
    Production,
    Test,
}

#[derive(Clone, Debug)]
pub struct SourceUnit {
    pub key: SourceKey,
    pub role: SourceRole,
    pub text: Arc<str>,
    pub line_starts: Arc<[u32]>,
    virtual_module: Option<ModulePath>,
}

impl SourceUnit {
    fn new(
        key: SourceKey,
        role: SourceRole,
        text: Arc<str>,
        virtual_module: Option<ModulePath>,
    ) -> Result<Self, SourceSetError> {
        let mut line_starts = vec![0];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                let start = u32::try_from(offset + 1)
                    .map_err(|_| SourceSetError::SourceFileTooLarge(key.clone()))?;
                line_starts.push(start);
            }
        }
        Ok(Self {
            key,
            role,
            text,
            line_starts: line_starts.into(),
            virtual_module,
        })
    }

    pub fn expected_module_path(&self) -> Result<ModulePath, IdentityError> {
        if let Some(module) = &self.virtual_module {
            return Ok(module.clone());
        }
        match self.role {
            SourceRole::Production => ModulePath::from_source_path(&self.key.path),
            SourceRole::Test => {
                let relative = self
                    .key
                    .path
                    .as_str()
                    .strip_prefix("tests/")
                    .and_then(|value| value.strip_suffix(".nexa"))
                    .ok_or_else(|| IdentityError::Invalid {
                        kind: "test source path",
                        value: self.key.path.to_string(),
                    })?;
                ModulePath::new(format!("test.{}", relative.replace('/', ".")))
            }
        }
    }

    /// Returns the compiler-provided module identity for an in-memory snippet.
    ///
    /// A virtual module changes only semantic identity. The source text and every byte offset
    /// remain exactly as supplied by the caller, so diagnostics keep their original CRLF and
    /// Unicode coordinates.
    #[must_use]
    pub fn virtual_module_path(&self) -> Option<&ModulePath> {
        self.virtual_module.as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilationLimits {
    pub modules_per_package: usize,
    pub source_file_bytes: usize,
    pub source_bytes_per_package: usize,
    pub dependency_closure_bytes: usize,
    pub imports_per_module: usize,
    pub module_edges: usize,
    pub dependency_packages: usize,
    pub diagnostics_per_revision: usize,
    pub max_loop_iterations: u32,
}

impl Default for CompilationLimits {
    fn default() -> Self {
        Self {
            modules_per_package: 256,
            source_file_bytes: 512 * 1024,
            source_bytes_per_package: 4 * 1024 * 1024,
            dependency_closure_bytes: 32 * 1024 * 1024,
            imports_per_module: 128,
            module_edges: 4_096,
            dependency_packages: 64,
            diagnostics_per_revision: 256,
            max_loop_iterations: 1_000_000,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PackageSourceSet {
    package_id: PackageId,
    units: Arc<BTreeMap<SourceKey, SourceUnit>>,
    artifact_files: Arc<ArtifactFileTable>,
    production_bytes: usize,
    test_bytes: usize,
}

impl PackageSourceSet {
    #[must_use]
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    #[must_use]
    pub fn units(&self) -> &BTreeMap<SourceKey, SourceUnit> {
        &self.units
    }

    #[must_use]
    pub fn get(&self, key: &SourceKey) -> Option<&SourceUnit> {
        self.units.get(key)
    }

    pub fn production_units(&self) -> impl Iterator<Item = &SourceUnit> {
        self.units
            .values()
            .filter(|unit| unit.role == SourceRole::Production)
    }

    pub fn test_units(&self) -> impl Iterator<Item = &SourceUnit> {
        self.units
            .values()
            .filter(|unit| unit.role == SourceRole::Test)
    }

    #[must_use]
    pub fn artifact_files(&self) -> &ArtifactFileTable {
        &self.artifact_files
    }

    #[must_use]
    pub const fn production_bytes(&self) -> usize {
        self.production_bytes
    }

    #[must_use]
    pub const fn test_bytes(&self) -> usize {
        self.test_bytes
    }

    /// Revalidates an already-captured source set against the options of the build which will
    /// consume it.
    ///
    /// Source sets are immutable snapshots, but they can have been constructed under different
    /// limits. The resolved-build boundary must therefore enforce its own effective limits rather
    /// than trusting the builder which happened to create the snapshot.
    pub fn validate_limits(&self, limits: CompilationLimits) -> Result<(), SourceSetError> {
        for unit in self.units.values() {
            if unit.text.len() > limits.source_file_bytes {
                return Err(SourceSetError::SourceFileTooLarge(unit.key.clone()));
            }
        }
        if self.production_bytes > limits.source_bytes_per_package {
            return Err(SourceSetError::PackageSourceTooLarge);
        }
        if self.test_bytes > limits.source_bytes_per_package {
            return Err(SourceSetError::TestSourceTooLarge);
        }
        let production_count = self.production_units().count();
        if production_count > limits.modules_per_package {
            return Err(SourceSetError::TooManyModules(production_count));
        }
        let test_count = self.test_units().count();
        if test_count > limits.modules_per_package {
            return Err(SourceSetError::TooManyTestModules(test_count));
        }
        Ok(())
    }

    pub fn validate_dependency_closure<'a>(
        sets: impl IntoIterator<Item = &'a Self>,
        limits: CompilationLimits,
    ) -> Result<usize, SourceSetError> {
        let total = sets
            .into_iter()
            .try_fold(0_usize, |total, set| {
                total.checked_add(set.production_bytes)
            })
            .ok_or(SourceSetError::DependencyClosureTooLarge)?;
        if total > limits.dependency_closure_bytes {
            return Err(SourceSetError::DependencyClosureTooLarge);
        }
        Ok(total)
    }

    pub fn discover_production(
        package_id: PackageId,
        package_directory: &Path,
        limits: CompilationLimits,
    ) -> Result<Self, SourceDiscoveryError> {
        reject_symlink(package_directory)?;
        let source_root = package_directory.join("src");
        reject_symlink(&source_root)?;
        let mut paths = Vec::new();
        collect_source_paths(&source_root, &mut paths)?;
        paths.sort();

        let mut builder = SourceSetBuilder::new(package_id, limits);
        for path in paths {
            let relative = path
                .strip_prefix(package_directory)
                .map_err(|_| SourceDiscoveryError::EscapedRoot(path.clone()))?;
            let normalized = NormalizedPackagePath::from_path(relative)
                .map_err(SourceDiscoveryError::Identity)?;
            // This validates the path-to-module mapping before any parsing occurs.
            ModulePath::from_source_path(&normalized).map_err(SourceDiscoveryError::Identity)?;
            let bytes = fs::read(&path).map_err(SourceDiscoveryError::Io)?;
            let text =
                String::from_utf8(bytes).map_err(|_| SourceDiscoveryError::NonUtf8Source(path))?;
            builder
                .add(normalized, text, SourceRole::Production)
                .map_err(SourceDiscoveryError::SourceSet)?;
        }
        builder.build().map_err(SourceDiscoveryError::SourceSet)
    }
}

pub(crate) fn reject_symlink(path: &Path) -> Result<(), SourceDiscoveryError> {
    let metadata = fs::symlink_metadata(path).map_err(SourceDiscoveryError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(SourceDiscoveryError::Symlink(path.to_path_buf()));
    }
    Ok(())
}

pub(crate) fn collect_source_paths(
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), SourceDiscoveryError> {
    reject_symlink(directory)?;
    let mut entries = fs::read_dir(directory)
        .map_err(SourceDiscoveryError::Io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SourceDiscoveryError::Io)?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(SourceDiscoveryError::Io)?;
        if file_type.is_symlink() {
            return Err(SourceDiscoveryError::Symlink(path));
        }
        if file_type.is_dir() {
            if path.join("package.toml").exists() {
                return Err(SourceDiscoveryError::NestedSourceRoot(path));
            }
            collect_source_paths(&path, output)?;
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "nexa") {
            output.push(path);
        }
    }
    Ok(())
}

pub struct SourceSetBuilder {
    package_id: PackageId,
    limits: CompilationLimits,
    units: BTreeMap<SourceKey, SourceUnit>,
    folded_paths: BTreeSet<String>,
    production_bytes: usize,
    test_bytes: usize,
}

impl SourceSetBuilder {
    #[must_use]
    pub fn new(package_id: PackageId, limits: CompilationLimits) -> Self {
        Self {
            package_id,
            limits,
            units: BTreeMap::new(),
            folded_paths: BTreeSet::new(),
            production_bytes: 0,
            test_bytes: 0,
        }
    }

    pub fn add(
        &mut self,
        path: NormalizedPackagePath,
        text: impl Into<Arc<str>>,
        role: SourceRole,
    ) -> Result<&mut Self, SourceSetError> {
        self.add_inner(path, text.into(), role, None)
    }

    /// Adds one production-only in-memory snippet under an explicit semantic module identity.
    ///
    /// This is the single-file tooling adapter. It does not prepend a synthetic `module`
    /// declaration or otherwise rewrite `text`. If the source contains an explicit module
    /// declaration, semantic analysis must require it to match `module`; ordinary package sources
    /// should continue to use [`Self::add`] and their path-derived module identity.
    pub fn add_virtual_snippet(
        &mut self,
        path: NormalizedPackagePath,
        text: impl Into<Arc<str>>,
        module: ModulePath,
    ) -> Result<&mut Self, SourceSetError> {
        self.add_inner(path, text.into(), SourceRole::Production, Some(module))
    }

    fn add_inner(
        &mut self,
        path: NormalizedPackagePath,
        text: Arc<str>,
        role: SourceRole,
        virtual_module: Option<ModulePath>,
    ) -> Result<&mut Self, SourceSetError> {
        if text.len() > self.limits.source_file_bytes {
            return Err(SourceSetError::SourceFileTooLarge(SourceKey::new(
                self.package_id.clone(),
                path,
            )));
        }
        let key = SourceKey::new(self.package_id.clone(), path);
        if self.units.contains_key(&key) {
            return Err(SourceSetError::DuplicateSource(key));
        }
        let folded = key.path.as_str().to_ascii_lowercase();
        if !self.folded_paths.insert(folded) {
            return Err(SourceSetError::CaseFoldConflict(key.path.clone()));
        }

        match role {
            SourceRole::Production => {
                let next = self
                    .production_bytes
                    .checked_add(text.len())
                    .ok_or(SourceSetError::PackageSourceTooLarge)?;
                if next > self.limits.source_bytes_per_package {
                    return Err(SourceSetError::PackageSourceTooLarge);
                }
                self.production_bytes = next;
            }
            SourceRole::Test => {
                let next = self
                    .test_bytes
                    .checked_add(text.len())
                    .ok_or(SourceSetError::TestSourceTooLarge)?;
                if next > self.limits.source_bytes_per_package {
                    return Err(SourceSetError::TestSourceTooLarge);
                }
                self.test_bytes = next;
            }
        }
        let unit = SourceUnit::new(key.clone(), role, text, virtual_module)?;
        // Validate every source/module mapping up front.
        unit.expected_module_path()
            .map_err(SourceSetError::Identity)?;
        self.units.insert(key, unit);
        Ok(self)
    }

    pub fn build(self) -> Result<PackageSourceSet, SourceSetError> {
        let artifact_files = ArtifactFileTable::allocate(self.units.keys().cloned())?;
        let source_set = PackageSourceSet {
            package_id: self.package_id,
            units: Arc::new(self.units),
            artifact_files: Arc::new(artifact_files),
            production_bytes: self.production_bytes,
            test_bytes: self.test_bytes,
        };
        source_set.validate_limits(self.limits)?;
        Ok(source_set)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactFile {
    pub id: ArtifactFileId,
    pub key: SourceKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactFileTable {
    files: Arc<[ArtifactFile]>,
    by_key: BTreeMap<SourceKey, ArtifactFileId>,
}

impl ArtifactFileTable {
    /// Allocates one dense, deterministic `FileId` space for a root and its complete static
    /// dependency closure. Test sources are deliberately excluded from the product artifact.
    pub fn for_closure<'a>(
        source_sets: impl IntoIterator<Item = &'a PackageSourceSet>,
    ) -> Result<Self, SourceSetError> {
        Self::allocate(
            source_sets
                .into_iter()
                .flat_map(PackageSourceSet::production_units)
                .map(|unit| unit.key.clone()),
        )
    }

    /// Allocates the test-artifact file space: the complete production dependency closure plus
    /// the root package's test modules. Test files remain absent from product artifacts.
    pub fn for_test_closure<'a>(
        production_sets: impl IntoIterator<Item = &'a PackageSourceSet>,
        root_tests: &'a PackageSourceSet,
    ) -> Result<Self, SourceSetError> {
        Self::allocate(
            production_sets
                .into_iter()
                .flat_map(PackageSourceSet::production_units)
                .chain(root_tests.test_units())
                .map(|unit| unit.key.clone()),
        )
    }

    fn allocate(keys: impl IntoIterator<Item = SourceKey>) -> Result<Self, SourceSetError> {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        let mut files = Vec::with_capacity(keys.len());
        let mut by_key = BTreeMap::new();
        for (offset, key) in keys.into_iter().enumerate() {
            let raw = u32::try_from(offset + 1).map_err(|_| SourceSetError::TooManyFiles)?;
            let id = ArtifactFileId(raw);
            by_key.insert(key.clone(), id);
            files.push(ArtifactFile { id, key });
        }
        Ok(Self {
            files: files.into(),
            by_key,
        })
    }

    #[must_use]
    pub fn files(&self) -> &[ArtifactFile] {
        &self.files
    }

    #[must_use]
    pub fn id_for(&self, key: &SourceKey) -> Option<ArtifactFileId> {
        self.by_key.get(key).copied()
    }

    #[must_use]
    pub fn key_for(&self, id: ArtifactFileId) -> Option<&SourceKey> {
        let index = usize::try_from(id.0).ok()?.checked_sub(1)?;
        self.files.get(index).map(|file| &file.key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSetError {
    Identity(IdentityError),
    DuplicateSource(SourceKey),
    CaseFoldConflict(NormalizedPackagePath),
    SourceFileTooLarge(SourceKey),
    PackageSourceTooLarge,
    TestSourceTooLarge,
    DependencyClosureTooLarge,
    TooManyModules(usize),
    TooManyTestModules(usize),
    TooManyFiles,
}

impl fmt::Display for SourceSetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SourceSetError {}

#[derive(Debug)]
pub enum SourceDiscoveryError {
    Io(std::io::Error),
    Identity(IdentityError),
    SourceSet(SourceSetError),
    Symlink(PathBuf),
    EscapedRoot(PathBuf),
    NestedSourceRoot(PathBuf),
    NonUtf8Source(PathBuf),
}

impl fmt::Display for SourceDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SourceDiscoveryError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn package() -> PackageId {
        PackageId::new("example.package").unwrap()
    }

    #[test]
    fn artifact_file_ids_are_sorted_and_candidate_local() {
        let mut first = SourceSetBuilder::new(package(), CompilationLimits::default());
        first
            .add(
                NormalizedPackagePath::new("src/z.nexa").unwrap(),
                "module z;",
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("src/a.nexa").unwrap(),
                "module a;",
                SourceRole::Production,
            )
            .unwrap();
        let first = first.build().unwrap();
        assert_eq!(
            first.artifact_files().files()[0].key.path.as_str(),
            "src/a.nexa"
        );
        assert_eq!(first.artifact_files().files()[0].id, ArtifactFileId(1));
        assert_eq!(first.artifact_files().files()[1].id, ArtifactFileId(2));
    }

    #[test]
    fn tests_are_excluded_from_production_accounting() {
        let mut builder = SourceSetBuilder::new(package(), CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "module main;",
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("tests/food/poison.nexa").unwrap(),
                "module test.food.poison;",
                SourceRole::Test,
            )
            .unwrap();
        let set = builder.build().unwrap();
        assert_eq!(set.production_units().count(), 1);
        assert_eq!(set.test_units().count(), 1);
        assert_eq!(
            set.test_units()
                .next()
                .unwrap()
                .expected_module_path()
                .unwrap()
                .as_str(),
            "test.food.poison"
        );
    }

    #[test]
    fn virtual_snippet_preserves_source_bytes_and_uses_explicit_module_identity() {
        let source = "fn main() -> string {\r\n    return \"𐐷\";\r\n}\r\n";
        let module = ModulePath::new("main").unwrap();
        let mut builder = SourceSetBuilder::new(package(), CompilationLimits::default());
        builder
            .add_virtual_snippet(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                source,
                module.clone(),
            )
            .unwrap();
        let set = builder.build().unwrap();
        let unit = set.production_units().next().unwrap();

        assert_eq!(unit.text.as_ref(), source);
        assert_eq!(unit.text.as_bytes(), source.as_bytes());
        assert_eq!(unit.expected_module_path().unwrap(), module);
        assert_eq!(unit.virtual_module_path(), Some(&module));
        let expected_line_starts = std::iter::once(0)
            .chain(
                source
                    .match_indices('\n')
                    .map(|(offset, _)| u32::try_from(offset + 1).unwrap()),
            )
            .collect::<Vec<_>>();
        assert_eq!(unit.line_starts.as_ref(), expected_line_starts.as_slice());
    }

    #[test]
    fn virtual_snippet_keeps_an_explicit_header_for_semantic_validation() {
        let source = "module declared.name;\r\nfn value() -> i32 { return 1; }\r\n";
        let virtual_module = ModulePath::new("main").unwrap();
        let mut builder = SourceSetBuilder::new(package(), CompilationLimits::default());
        builder
            .add_virtual_snippet(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                source,
                virtual_module.clone(),
            )
            .unwrap();
        let set = builder.build().unwrap();
        let unit = set.production_units().next().unwrap();

        assert_eq!(unit.text.as_ref(), source);
        assert_eq!(unit.expected_module_path().unwrap(), virtual_module);
    }

    #[test]
    fn case_fold_conflicts_are_rejected() {
        let mut builder = SourceSetBuilder::new(package(), CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/a.nexa").unwrap(),
                "",
                SourceRole::Production,
            )
            .unwrap();
        assert!(matches!(
            builder.add(
                NormalizedPackagePath::new("src/A.nexa").unwrap(),
                "",
                SourceRole::Production
            ),
            Err(SourceSetError::CaseFoldConflict(_) | SourceSetError::Identity(_))
        ));
    }

    #[test]
    fn closure_file_ids_distinguish_packages_with_identical_paths() {
        let build = |package: &str| {
            let mut builder = SourceSetBuilder::new(
                PackageId::new(package).unwrap(),
                CompilationLimits::default(),
            );
            builder
                .add(
                    NormalizedPackagePath::new("src/main.nexa").unwrap(),
                    "module main;",
                    SourceRole::Production,
                )
                .unwrap();
            builder.build().unwrap()
        };
        let root = build("a.root");
        let first = build("b.dependency");
        let second = build("c.dependency");
        let table = ArtifactFileTable::for_closure([&second, &root, &first]).unwrap();
        assert_eq!(table.files().len(), 3);
        assert_eq!(table.files()[0].key.package_id.as_str(), "a.root");
        assert_eq!(table.files()[0].id, ArtifactFileId(1));
        assert_eq!(table.files()[1].id, ArtifactFileId(2));
        assert_eq!(table.files()[2].id, ArtifactFileId(3));
    }

    #[test]
    fn test_sources_obey_byte_and_module_limits() {
        let byte_limits = CompilationLimits {
            source_file_bytes: 16,
            source_bytes_per_package: 5,
            ..CompilationLimits::default()
        };
        let mut bytes = SourceSetBuilder::new(package(), byte_limits);
        assert!(matches!(
            bytes.add(
                NormalizedPackagePath::new("tests/a.nexa").unwrap(),
                "module",
                SourceRole::Test,
            ),
            Err(SourceSetError::TestSourceTooLarge)
        ));

        let module_limits = CompilationLimits {
            modules_per_package: 1,
            ..CompilationLimits::default()
        };
        let mut modules = SourceSetBuilder::new(package(), module_limits);
        modules
            .add(
                NormalizedPackagePath::new("tests/a.nexa").unwrap(),
                "module test.a;",
                SourceRole::Test,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("tests/b.nexa").unwrap(),
                "module test.b;",
                SourceRole::Test,
            )
            .unwrap();
        assert!(matches!(
            modules.build(),
            Err(SourceSetError::TooManyTestModules(2))
        ));
    }
}
