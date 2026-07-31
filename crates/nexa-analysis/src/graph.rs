use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use semver::Version;

use crate::{
    CompilationLimits, DependencyAlias, IdentityError, ModulePath, NormalizedPackagePath,
    PackageId, PackageKind, PackageManifest, SourceId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphCycle<T> {
    /// A canonical cycle with the first node repeated at the end.
    pub chain: Vec<T>,
}

impl<T: fmt::Display> fmt::Display for GraphCycle<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, node) in self.chain.iter().enumerate() {
            if index != 0 {
                formatter.write_str(" -> ")?;
            }
            node.fmt(formatter)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModuleGraph {
    edges: BTreeMap<ModulePath, BTreeSet<ModulePath>>,
    edge_count: usize,
}

impl ModuleGraph {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_module(&mut self, module: ModulePath) -> bool {
        self.edges.insert(module, BTreeSet::new()).is_none()
    }

    pub fn add_import(
        &mut self,
        from: &ModulePath,
        to: &ModulePath,
        limits: CompilationLimits,
    ) -> Result<(), ModuleGraphError> {
        if !self.edges.contains_key(from) {
            return Err(ModuleGraphError::UnknownModule(from.clone()));
        }
        if !self.edges.contains_key(to) {
            return Err(ModuleGraphError::UnknownModule(to.clone()));
        }
        let imports = self.edges.get(from).expect("checked module");
        if imports.contains(to) {
            return Ok(());
        }
        if imports.len() >= limits.imports_per_module {
            return Err(ModuleGraphError::TooManyImports(from.clone()));
        }
        if self.edge_count >= limits.module_edges {
            return Err(ModuleGraphError::TooManyEdges);
        }
        self.edges
            .get_mut(from)
            .expect("checked module")
            .insert(to.clone());
        self.edge_count += 1;
        Ok(())
    }

    pub fn modules(&self) -> impl Iterator<Item = &ModulePath> {
        self.edges.keys()
    }

    #[must_use]
    pub fn imports(&self, module: &ModulePath) -> Option<&BTreeSet<ModulePath>> {
        self.edges.get(module)
    }

    #[must_use]
    pub const fn edge_count(&self) -> usize {
        self.edge_count
    }

    pub fn validate_acyclic(&self) -> Result<(), ModuleGraphError> {
        detect_cycle(&self.edges).map_or(Ok(()), |cycle| Err(ModuleGraphError::Cycle(cycle)))
    }

    #[must_use]
    pub fn reverse_dependencies(&self) -> BTreeMap<ModulePath, BTreeSet<ModulePath>> {
        let mut reverse = self
            .edges
            .keys()
            .cloned()
            .map(|module| (module, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for (from, targets) in &self.edges {
            for target in targets {
                reverse
                    .entry(target.clone())
                    .or_default()
                    .insert(from.clone());
            }
        }
        reverse
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModuleGraphError {
    UnknownModule(ModulePath),
    TooManyImports(ModulePath),
    TooManyEdges,
    Cycle(GraphCycle<ModulePath>),
}

impl fmt::Display for ModuleGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModule(module) => write!(formatter, "unknown module {module}"),
            Self::TooManyImports(module) => write!(formatter, "too many imports in {module}"),
            Self::TooManyEdges => formatter.write_str("module graph edge limit exceeded"),
            Self::Cycle(cycle) => write!(formatter, "module cycle: {cycle}"),
        }
    }
}

impl std::error::Error for ModuleGraphError {}

#[derive(Clone, Debug)]
pub struct PackageLocation {
    pub source_id: SourceId,
    pub directory: NormalizedPackagePath,
    pub manifest: Arc<PackageManifest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub id: PackageId,
    pub version: Version,
    pub source_id: SourceId,
    pub directory: NormalizedPackagePath,
    pub kind: PackageKind,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DependencyEdge {
    pub from: PackageId,
    pub alias: DependencyAlias,
    pub to: PackageId,
}

#[derive(Clone, Debug)]
pub struct ResolvedDependencyGraph {
    pub root: PackageId,
    pub packages: BTreeMap<PackageId, ResolvedPackage>,
    pub edges: BTreeSet<DependencyEdge>,
}

impl ResolvedDependencyGraph {
    #[must_use]
    pub fn package(&self, id: &PackageId) -> Option<&ResolvedPackage> {
        self.packages.get(id)
    }

    pub fn dependencies_of(&self, package: &PackageId) -> impl Iterator<Item = &DependencyEdge> {
        self.edges.iter().filter(move |edge| &edge.from == package)
    }

    #[must_use]
    pub fn canonical_identity_bytes(&self) -> Vec<u8> {
        let mut output = Vec::new();
        append_framed(&mut output, self.root.as_str().as_bytes());
        for package in self.packages.values() {
            append_framed(&mut output, package.id.as_str().as_bytes());
            append_framed(&mut output, package.version.to_string().as_bytes());
            append_framed(&mut output, package.directory.as_str().as_bytes());
        }
        for edge in &self.edges {
            append_framed(&mut output, edge.from.as_str().as_bytes());
            append_framed(&mut output, edge.alias.as_str().as_bytes());
            append_framed(&mut output, edge.to.as_str().as_bytes());
        }
        output
    }

    pub fn validate_acyclic(&self) -> Result<(), DependencyGraphError> {
        let mut adjacency = self
            .packages
            .keys()
            .cloned()
            .map(|id| (id, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for edge in &self.edges {
            adjacency
                .entry(edge.from.clone())
                .or_default()
                .insert(edge.to.clone());
        }
        detect_cycle(&adjacency).map_or(Ok(()), |cycle| Err(DependencyGraphError::Cycle(cycle)))
    }

    #[must_use]
    pub fn reverse_dependencies(&self) -> BTreeMap<PackageId, BTreeSet<PackageId>> {
        let mut reverse = self
            .packages
            .keys()
            .cloned()
            .map(|id| (id, BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        for edge in &self.edges {
            reverse
                .entry(edge.to.clone())
                .or_default()
                .insert(edge.from.clone());
        }
        reverse
    }
}

#[derive(Clone, Debug, Default)]
pub struct PackageCatalog {
    locations: BTreeMap<(SourceId, NormalizedPackagePath), PackageLocation>,
}

impl PackageCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, location: PackageLocation) -> Result<(), DependencyGraphError> {
        let key = (location.source_id.clone(), location.directory.clone());
        if self.locations.insert(key.clone(), location).is_some() {
            return Err(DependencyGraphError::DuplicateLocation {
                source_id: key.0,
                path: key.1,
            });
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        source_id: &SourceId,
        root_directory: &NormalizedPackagePath,
        limits: CompilationLimits,
    ) -> Result<ResolvedDependencyGraph, DependencyGraphError> {
        let root = self
            .locations
            .get(&(source_id.clone(), root_directory.clone()))
            .ok_or_else(|| DependencyGraphError::MissingPackage {
                source_id: source_id.clone(),
                path: root_directory.clone(),
            })?;
        let root_id = root.manifest.id.clone();
        let mut graph = ResolvedDependencyGraph {
            root: root_id.clone(),
            packages: BTreeMap::new(),
            edges: BTreeSet::new(),
        };
        let mut pending = vec![root_directory.clone()];
        let mut canonical_identity = BTreeMap::<PackageId, (Version, NormalizedPackagePath)>::new();
        let mut visited = BTreeSet::new();

        while let Some(directory) = pending.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            let location = self
                .locations
                .get(&(source_id.clone(), directory.clone()))
                .ok_or_else(|| DependencyGraphError::MissingPackage {
                    source_id: source_id.clone(),
                    path: directory.clone(),
                })?;
            let manifest = &location.manifest;
            if let Some((version, prior_path)) = canonical_identity.get(&manifest.id) {
                if version != &manifest.version || prior_path != &directory {
                    return Err(DependencyGraphError::IdentityConflict(Box::new(
                        DependencyIdentityConflict {
                            package: manifest.id.clone(),
                            first_version: version.clone(),
                            first_path: prior_path.clone(),
                            second_version: manifest.version.clone(),
                            second_path: directory,
                        },
                    )));
                }
            } else {
                canonical_identity.insert(
                    manifest.id.clone(),
                    (manifest.version.clone(), directory.clone()),
                );
            }
            graph.packages.insert(
                manifest.id.clone(),
                ResolvedPackage {
                    id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source_id: source_id.clone(),
                    directory: directory.clone(),
                    kind: manifest.kind,
                },
            );
            if graph.packages.len() > limits.dependency_packages.saturating_add(1) {
                return Err(DependencyGraphError::TooManyPackages);
            }
            for (alias, dependency) in &manifest.dependencies {
                let dependency_directory = dependency
                    .path
                    .resolve_from(&directory)
                    .map_err(DependencyGraphError::Identity)?;
                let target = self
                    .locations
                    .get(&(source_id.clone(), dependency_directory.clone()))
                    .ok_or_else(|| DependencyGraphError::MissingPackage {
                        source_id: source_id.clone(),
                        path: dependency_directory.clone(),
                    })?;
                if target.manifest.kind != PackageKind::Library {
                    return Err(DependencyGraphError::DependencyIsNotLibrary(
                        target.manifest.id.clone(),
                    ));
                }
                graph.edges.insert(DependencyEdge {
                    from: manifest.id.clone(),
                    alias: alias.clone(),
                    to: target.manifest.id.clone(),
                });
                pending.push(dependency_directory);
            }
        }
        graph.validate_acyclic()?;
        Ok(graph)
    }
}

fn append_framed(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    output.extend_from_slice(value);
}

fn detect_cycle<T>(edges: &BTreeMap<T, BTreeSet<T>>) -> Option<GraphCycle<T>>
where
    T: Clone + Ord,
{
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Visiting,
        Complete,
    }

    fn visit<T>(
        node: &T,
        edges: &BTreeMap<T, BTreeSet<T>>,
        states: &mut BTreeMap<T, State>,
        stack: &mut Vec<T>,
    ) -> Option<GraphCycle<T>>
    where
        T: Clone + Ord,
    {
        states.insert(node.clone(), State::Visiting);
        stack.push(node.clone());
        if let Some(targets) = edges.get(node) {
            for target in targets {
                match states.get(target) {
                    Some(State::Visiting) => {
                        let start = stack.iter().position(|item| item == target)?;
                        let cycle = stack[start..].to_vec();
                        return Some(canonical_cycle(cycle));
                    }
                    Some(State::Complete) => {}
                    None => {
                        if let Some(cycle) = visit(target, edges, states, stack) {
                            return Some(cycle);
                        }
                    }
                }
            }
        }
        stack.pop();
        states.insert(node.clone(), State::Complete);
        None
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    for node in edges.keys() {
        if !states.contains_key(node)
            && let Some(cycle) = visit(node, edges, &mut states, &mut stack)
        {
            return Some(cycle);
        }
    }
    None
}

fn canonical_cycle<T: Clone + Ord>(mut cycle: Vec<T>) -> GraphCycle<T> {
    let start = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, node)| *node)
        .map_or(0, |(index, _)| index);
    cycle.rotate_left(start);
    if let Some(first) = cycle.first().cloned() {
        cycle.push(first);
    }
    GraphCycle { chain: cycle }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DependencyGraphError {
    Identity(IdentityError),
    DuplicateLocation {
        source_id: SourceId,
        path: NormalizedPackagePath,
    },
    MissingPackage {
        source_id: SourceId,
        path: NormalizedPackagePath,
    },
    DependencyIsNotLibrary(PackageId),
    IdentityConflict(Box<DependencyIdentityConflict>),
    TooManyPackages,
    Cycle(GraphCycle<PackageId>),
}

impl fmt::Display for DependencyGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => error.fmt(formatter),
            Self::DuplicateLocation { source_id, path } => {
                write!(formatter, "duplicate package location {source_id}:{path}")
            }
            Self::MissingPackage { source_id, path } => {
                write!(formatter, "missing package {source_id}:{path}")
            }
            Self::DependencyIsNotLibrary(package) => {
                write!(formatter, "dependency {package} is not a library")
            }
            Self::IdentityConflict(conflict) => write!(
                formatter,
                "dependency identity conflict for {package}: {first_version} at {first_path}, \
                 {second_version} at {second_path}",
                package = conflict.package,
                first_version = conflict.first_version,
                first_path = conflict.first_path,
                second_version = conflict.second_version,
                second_path = conflict.second_path,
            ),
            Self::TooManyPackages => formatter.write_str("dependency package limit exceeded"),
            Self::Cycle(cycle) => write!(formatter, "dependency cycle: {cycle}"),
        }
    }
}

impl std::error::Error for DependencyGraphError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyIdentityConflict {
    pub package: PackageId,
    pub first_version: Version,
    pub first_path: NormalizedPackagePath,
    pub second_version: Version,
    pub second_path: NormalizedPackagePath,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_cycle_is_canonical() {
        let limits = CompilationLimits::default();
        let mut graph = ModuleGraph::new();
        let a = ModulePath::new("a").unwrap();
        let b = ModulePath::new("b").unwrap();
        let c = ModulePath::new("c").unwrap();
        for module in [&c, &a, &b] {
            graph.add_module(module.clone());
        }
        graph.add_import(&b, &c, limits).unwrap();
        graph.add_import(&c, &a, limits).unwrap();
        graph.add_import(&a, &b, limits).unwrap();
        let ModuleGraphError::Cycle(cycle) = graph.validate_acyclic().unwrap_err() else {
            panic!("expected cycle");
        };
        assert_eq!(cycle.chain, vec![a.clone(), b, c, a]);
    }

    #[test]
    fn module_edge_limit_failures_do_not_modify_the_graph() {
        let a = ModulePath::new("a").unwrap();
        let b = ModulePath::new("b").unwrap();
        let c = ModulePath::new("c").unwrap();
        let mut graph = ModuleGraph::new();
        for module in [&a, &b, &c] {
            graph.add_module(module.clone());
        }

        let one_edge = CompilationLimits {
            module_edges: 1,
            ..CompilationLimits::default()
        };
        graph.add_import(&a, &b, one_edge).unwrap();
        assert!(matches!(
            graph.add_import(&a, &c, one_edge),
            Err(ModuleGraphError::TooManyEdges)
        ));
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(
            graph.imports(&a),
            Some(&BTreeSet::from([b.clone()])),
            "the rejected edge must not remain in the graph"
        );

        let one_import = CompilationLimits {
            imports_per_module: 1,
            module_edges: usize::MAX,
            ..CompilationLimits::default()
        };
        assert!(matches!(
            graph.add_import(&a, &c, one_import),
            Err(ModuleGraphError::TooManyImports(module)) if module == a
        ));
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(graph.imports(&a), Some(&BTreeSet::from([b])));
    }

    fn manifest(source: &str) -> Arc<PackageManifest> {
        Arc::new(PackageManifest::parse(source).unwrap())
    }

    #[test]
    fn resolves_only_same_source_path_libraries() {
        let source = SourceId::new("local").unwrap();
        let root_path = NormalizedPackagePath::new("packages/app").unwrap();
        let library_path = NormalizedPackagePath::new("packages/common").unwrap();
        let root = manifest(
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
common = { path = "../common" }
"#,
        );
        let library = manifest(
            r#"
schema = 2
kind = "library"
id = "example.common"
name = "Common"
version = "1.0.0"
source_root = "src"
"#,
        );
        let mut catalog = PackageCatalog::new();
        catalog
            .insert(PackageLocation {
                source_id: source.clone(),
                directory: root_path.clone(),
                manifest: root,
            })
            .unwrap();
        catalog
            .insert(PackageLocation {
                source_id: source.clone(),
                directory: library_path,
                manifest: library,
            })
            .unwrap();
        let graph = catalog
            .resolve(&source, &root_path, CompilationLimits::default())
            .unwrap();
        assert_eq!(graph.packages.len(), 2);
        assert_eq!(graph.edges.len(), 1);
    }
}
