use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use semver::Version;
use serde::Deserialize;

use crate::{DependencyAlias, NormalizedPackagePath, PackageId, ResolvedDependencyGraph};

pub const LOCK_SCHEMA: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedPackage {
    pub id: PackageId,
    pub version: Version,
    pub path: NormalizedPackagePath,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockedDependencyEdge {
    pub from: PackageId,
    pub alias: DependencyAlias,
    pub to: PackageId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockFile {
    pub schema: u32,
    pub root: PackageId,
    pub packages: BTreeMap<PackageId, LockedPackage>,
    pub edges: BTreeSet<LockedDependencyEdge>,
}

impl LockFile {
    #[must_use]
    pub fn from_graph(graph: &ResolvedDependencyGraph) -> Self {
        Self {
            schema: LOCK_SCHEMA,
            root: graph.root.clone(),
            packages: graph
                .packages
                .values()
                .map(|package| {
                    (
                        package.id.clone(),
                        LockedPackage {
                            id: package.id.clone(),
                            version: package.version.clone(),
                            path: package.directory.clone(),
                        },
                    )
                })
                .collect(),
            edges: graph
                .edges
                .iter()
                .map(|edge| LockedDependencyEdge {
                    from: edge.from.clone(),
                    alias: edge.alias.clone(),
                    to: edge.to.clone(),
                })
                .collect(),
        }
    }

    pub fn parse(source: &str) -> Result<Self, LockError> {
        let raw: RawLockFile =
            toml::from_str(source).map_err(|error| LockError::Toml(error.to_string()))?;
        if raw.schema != LOCK_SCHEMA {
            return Err(LockError::UnsupportedSchema(raw.schema));
        }
        let root = PackageId::new(raw.root).map_err(LockError::Identity)?;
        let mut packages = BTreeMap::new();
        for package in raw.packages {
            let id = PackageId::new(package.id).map_err(LockError::Identity)?;
            let version = Version::parse(&package.version)
                .map_err(|error| LockError::InvalidVersion(error.to_string()))?;
            let path = NormalizedPackagePath::new(package.path).map_err(LockError::Identity)?;
            let locked = LockedPackage {
                id: id.clone(),
                version,
                path,
            };
            if packages.insert(id.clone(), locked).is_some() {
                return Err(LockError::DuplicatePackage(id));
            }
        }
        if !packages.contains_key(&root) {
            return Err(LockError::MissingRoot(root));
        }
        let mut edges = BTreeSet::new();
        for edge in raw.edges {
            let edge = LockedDependencyEdge {
                from: PackageId::new(edge.from).map_err(LockError::Identity)?,
                alias: DependencyAlias::new(edge.alias).map_err(LockError::Identity)?,
                to: PackageId::new(edge.to).map_err(LockError::Identity)?,
            };
            if !packages.contains_key(&edge.from) {
                return Err(LockError::UnknownEdgePackage(edge.from));
            }
            if !packages.contains_key(&edge.to) {
                return Err(LockError::UnknownEdgePackage(edge.to));
            }
            if !edges.insert(edge.clone()) {
                return Err(LockError::DuplicateEdge(edge));
            }
        }
        Ok(Self {
            schema: LOCK_SCHEMA,
            root,
            packages,
            edges,
        })
    }

    /// Canonical TOML with package and edge arrays sorted by their normative identities.
    #[must_use]
    pub fn render(&self) -> String {
        let mut output = format!(
            "schema = {LOCK_SCHEMA}\nroot = {}\n",
            quote(self.root.as_str())
        );
        for package in self.packages.values() {
            output.push_str("\n[[packages]]\n");
            writeln!(output, "id = {}", quote(package.id.as_str()))
                .expect("writing to a String cannot fail");
            writeln!(output, "version = {}", quote(&package.version.to_string()))
                .expect("writing to a String cannot fail");
            writeln!(output, "path = {}", quote(package.path.as_str()))
                .expect("writing to a String cannot fail");
        }
        for edge in &self.edges {
            output.push_str("\n[[edges]]\n");
            writeln!(output, "from = {}", quote(edge.from.as_str()))
                .expect("writing to a String cannot fail");
            writeln!(output, "alias = {}", quote(edge.alias.as_str()))
                .expect("writing to a String cannot fail");
            writeln!(output, "to = {}", quote(edge.to.as_str()))
                .expect("writing to a String cannot fail");
        }
        output
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.render().into_bytes()
    }

    pub fn verify(&self, graph: &ResolvedDependencyGraph) -> Result<(), LockDrift> {
        let expected = Self::from_graph(graph);
        if self == &expected {
            Ok(())
        } else {
            Err(LockDrift {
                expected: Box::new(expected),
                actual: Box::new(self.clone()),
            })
        }
    }
}

fn quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockFile {
    schema: u32,
    root: String,
    #[serde(default)]
    packages: Vec<RawLockedPackage>,
    #[serde(default)]
    edges: Vec<RawLockedDependencyEdge>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockedPackage {
    id: String,
    version: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLockedDependencyEdge {
    from: String,
    alias: String,
    to: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockDrift {
    pub expected: Box<LockFile>,
    pub actual: Box<LockFile>,
}

impl fmt::Display for LockDrift {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("nexa.lock is missing or stale; run `nexa lock`")
    }
}

impl std::error::Error for LockDrift {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockError {
    Toml(String),
    UnsupportedSchema(u32),
    Identity(crate::IdentityError),
    InvalidVersion(String),
    DuplicatePackage(PackageId),
    MissingRoot(PackageId),
    UnknownEdgePackage(PackageId),
    DuplicateEdge(LockedDependencyEdge),
}

impl fmt::Display for LockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for LockError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{CompilationLimits, PackageCatalog, PackageLocation, PackageManifest, SourceId};

    use super::*;

    fn graph() -> ResolvedDependencyGraph {
        let source_id = SourceId::new("local").unwrap();
        let root = NormalizedPackagePath::new("packages/app").unwrap();
        let library = NormalizedPackagePath::new("packages/lib").unwrap();
        let mut catalog = PackageCatalog::new();
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: root.clone(),
                manifest: Arc::new(
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
[dependencies]
library = { path = "../lib" }
"#,
                    )
                    .unwrap(),
                ),
            })
            .unwrap();
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: library,
                manifest: Arc::new(
                    PackageManifest::parse(
                        r#"
schema = 2
kind = "library"
id = "example.lib"
name = "Lib"
version = "1.0.0"
source_root = "src"
"#,
                    )
                    .unwrap(),
                ),
            })
            .unwrap();
        catalog
            .resolve(&source_id, &root, CompilationLimits::default())
            .unwrap()
    }

    #[test]
    fn lock_render_is_canonical_and_round_trips() {
        let lock = LockFile::from_graph(&graph());
        let rendered = lock.render();
        assert_eq!(LockFile::parse(&rendered).unwrap(), lock);
        assert_eq!(LockFile::parse(&rendered).unwrap().render(), rendered);
        assert!(!rendered.contains("/Users/"));
    }

    #[test]
    fn drift_is_structural() {
        let graph = graph();
        let mut lock = LockFile::from_graph(&graph);
        lock.edges.clear();
        assert!(lock.verify(&graph).is_err());
    }
}
