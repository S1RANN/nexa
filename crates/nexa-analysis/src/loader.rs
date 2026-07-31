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

/// Validates one in-memory production source using the same complete syntax/module-path rules as
/// directory discovery.
pub fn validate_module_source(
    path: &NormalizedPackagePath,
    source: &str,
) -> Result<ModulePath, PackageLoadError> {
    validate_module_source_for_role(path, source, SourceRole::Production)
}

pub fn validate_module_source_for_role(
    path: &NormalizedPackagePath,
    source: &str,
    role: SourceRole,
) -> Result<ModulePath, PackageLoadError> {
    let expected = match role {
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
    .map_err(PackageLoadError::Identity)?;
    let declared =
        scan_module_header(source).map_err(|reason| PackageLoadError::InvalidModuleHeader {
            path: path.clone(),
            reason,
        })?;
    if declared != expected {
        return Err(PackageLoadError::ModulePathMismatch {
            path: path.clone(),
            expected,
            declared,
        });
    }
    Ok(declared)
}

fn scan_module_header(source: &str) -> Result<ModulePath, String> {
    use nexa_syntax::{Keyword, TokenKind};

    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lexed = nexa_syntax::lex_nexa(source).map_err(|error| error.to_string())?;
    let mut tokens = lexed
        .tokens
        .iter()
        .filter(|token| !token.kind.is_trivia())
        .peekable();
    let Some(module_keyword) = tokens.next() else {
        return Err("source must begin with a module declaration".to_owned());
    };
    if module_keyword.kind != TokenKind::Keyword(Keyword::Module) {
        return Err("source must begin with a module declaration".to_owned());
    }

    let mut path = String::new();
    let mut expect_segment = true;
    loop {
        let Some(token) = tokens.next() else {
            return Err("module declaration must end with `;`".to_owned());
        };
        match (expect_segment, token.kind) {
            (true, TokenKind::Identifier) => {
                path.push_str(
                    lexed
                        .source
                        .slice(token.range)
                        .expect("lexer token ranges are valid"),
                );
                expect_segment = false;
            }
            (false, TokenKind::Dot) => {
                path.push('.');
                expect_segment = true;
            }
            (false, TokenKind::Semicolon) => break,
            _ => return Err("module declaration contains an invalid path".to_owned()),
        }
    }
    let declared = ModulePath::new(path).map_err(|error| error.to_string())?;

    // Only additional top-level declarations are header errors. Syntax errors inside function
    // bodies are intentionally left for package analysis so they retain structured diagnostics.
    let mut brace_depth = 0_u32;
    let mut module_count = 1_usize;
    for token in tokens {
        match token.kind {
            TokenKind::LBrace => brace_depth = brace_depth.saturating_add(1),
            TokenKind::RBrace => brace_depth = brace_depth.saturating_sub(1),
            TokenKind::Keyword(Keyword::Module) if brace_depth == 0 => {
                module_count = module_count.saturating_add(1);
            }
            _ => {}
        }
    }
    if module_count != 1 {
        return Err(format!(
            "expected exactly one module declaration, found {module_count}"
        ));
    }
    Ok(declared)
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
    InvalidModuleHeader {
        path: NormalizedPackagePath,
        reason: String,
    },
    ModulePathMismatch {
        path: NormalizedPackagePath,
        expected: ModulePath,
        declared: ModulePath,
    },
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
    fn module_header_scanner_skips_nexa_comments() {
        assert_eq!(
            scan_module_header(
                "\u{feff}/// docs\n/* package docs */\nmodule food.effects;\npub fn x() {}"
            )
            .unwrap()
            .as_str(),
            "food.effects"
        );
    }

    #[test]
    fn module_header_scanner_rejects_path_invalid_names() {
        assert!(scan_module_header("module Food.effects;").is_err());
        assert!(scan_module_header("fn main() {}").is_err());
    }

    #[test]
    fn module_header_scanner_leaves_body_errors_for_analysis() {
        assert_eq!(
            scan_module_header("module food.effects;\npub fn broken( { module nested;")
                .unwrap()
                .as_str(),
            "food.effects"
        );
    }

    #[test]
    fn module_header_scanner_rejects_duplicate_top_level_declarations() {
        assert!(scan_module_header("module food.effects;\nmodule food.other;").is_err());
    }
}
