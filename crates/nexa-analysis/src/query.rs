use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use nexa_core::FingerprintBuilder;

use crate::{
    Definition, DefinitionId, ModulePath, PackageId, PackageSourceSet, ResolvedBuildInput,
    SourceKey, TypedModuleIr,
};

#[derive(Clone, Debug)]
pub enum QueryValue {
    Bytes(Arc<[u8]>),
    Syntax(Arc<nexa_syntax::SyntaxTree>),
    ModuleHeader(Arc<ModuleHeader>),
    TypedModule(Arc<CachedTypedModule>),
}

/// A Typed Module is reusable only while the package-wide dense Definition layout is identical.
///
/// Typed IR deliberately uses compact [`crate::DefinitionId`] values. Comparing the complete
/// ordered canonical layout prevents a cached module from retaining stale numeric IDs after a
/// declaration/local is inserted or removed elsewhere. A layout change is a conservative cache
/// miss; it is never a stale reuse.
#[derive(Clone, Debug)]
pub struct CachedTypedModule {
    pub module: Arc<TypedModuleIr>,
    definition_layout: Arc<[String]>,
    owned_definitions: Arc<[Definition]>,
    semantic_context: [u8; 32],
}

impl CachedTypedModule {
    fn new(
        module: Arc<TypedModuleIr>,
        definitions: &[Definition],
        semantic_context: [u8; 32],
    ) -> Self {
        let owned_definitions = definitions
            .iter()
            .filter(|definition| {
                definition.package_id == module.package_id
                    && definition.module == module.module
                    && definition.span.source == module.source
            })
            .cloned()
            .collect::<Vec<_>>();
        Self {
            module,
            definition_layout: canonical_definition_layout(definitions),
            owned_definitions: owned_definitions.into(),
            semantic_context,
        }
    }

    fn matches(&self, definitions: &[Definition], semantic_context: &[u8; 32]) -> bool {
        self.semantic_context == *semantic_context
            && self.definition_layout.as_ref() == canonical_definition_layout(definitions).as_ref()
    }
}

impl From<Arc<[u8]>> for QueryValue {
    fn from(value: Arc<[u8]>) -> Self {
        Self::Bytes(value)
    }
}

impl From<Arc<nexa_syntax::SyntaxTree>> for QueryValue {
    fn from(value: Arc<nexa_syntax::SyntaxTree>) -> Self {
        Self::Syntax(value)
    }
}

impl From<Arc<ModuleHeader>> for QueryValue {
    fn from(value: Arc<ModuleHeader>) -> Self {
        Self::ModuleHeader(value)
    }
}

impl From<Arc<CachedTypedModule>> for QueryValue {
    fn from(value: Arc<CachedTypedModule>) -> Self {
        Self::TypedModule(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModuleHeader {
    pub module: ModulePath,
    pub imports: Vec<ImportHeader>,
    pub declarations: Vec<DeclarationHeader>,
    pub syntax_error_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportHeader {
    pub path: ModulePath,
    pub alias: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum DeclarationVisibility {
    Private,
    Package,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum HeaderDeclarationKind {
    Function,
    Struct,
    Enum,
    Class,
    Const,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeclarationHeader {
    pub name: String,
    pub kind: HeaderDeclarationKind,
    pub visibility: DeclarationVisibility,
    pub attributes: Vec<String>,
    /// Trivia-free declaration surface. Function bodies are deliberately excluded.
    pub signature: String,
}

#[derive(Clone, Debug)]
pub struct SourceUpdate {
    pub tree: Arc<nexa_syntax::SyntaxTree>,
    pub header: Arc<ModuleHeader>,
    pub impact: ChangeImpact,
    pub invalidation: InvalidationReport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSetChange {
    Add {
        source: SourceKey,
        module: ModuleKey,
    },
    Delete {
        source: SourceKey,
        module: ModuleKey,
    },
    Rename {
        old_source: SourceKey,
        old_module: ModuleKey,
        new_source: SourceKey,
        new_module: ModuleKey,
    },
    Aba {
        source: SourceKey,
        module: ModuleKey,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildInputUpdate {
    pub invalidation: InvalidationReport,
    pub relinked_modules: BTreeSet<ModuleKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModuleKey {
    pub package_id: PackageId,
    pub module: ModulePath,
}

impl ModuleKey {
    #[must_use]
    pub const fn new(package_id: PackageId, module: ModulePath) -> Self {
        Self { package_id, module }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum QueryKey {
    Parse(SourceKey),
    ModuleHeaders(ModuleKey),
    ResolvedImports(ModuleKey),
    TypedModule(ModuleKey),
    PackagePublicApi(PackageId),
    PackageStateSchema(PackageId),
    SourceSet(PackageId),
    PackageManifest(PackageId),
    DependencyGraph(PackageId),
    HostContract(PackageId),
    LinkedArtifact(PackageId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeImpact {
    /// The declaration surface is unchanged.
    PrivateImplementation,
    /// A module-local declaration surface or import set changed.
    ModuleLocalSurface,
    /// A `pub(package)` surface changed.
    PackageApi,
    /// A `pub` surface changed and dependency consumers may need re-analysis.
    PublicApi,
    /// Persistent-state layout changed.
    StateSchema,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueryStats {
    pub hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub invalidations: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InvalidationReport {
    pub revision: u64,
    pub parsed_sources: BTreeSet<SourceKey>,
    pub analyzed_modules: BTreeSet<ModuleKey>,
    pub invalidated_queries: BTreeSet<QueryKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueryExecutionReport {
    pub revision: u64,
    pub parsed_sources: BTreeSet<SourceKey>,
    pub analyzed_modules: BTreeSet<ModuleKey>,
    pub reused_queries: BTreeSet<QueryKey>,
    pub invalidated_queries: BTreeSet<QueryKey>,
}

/// A deterministic in-memory query database with explicit dependency edges.
///
/// Query values are opaque canonical bytes at this layer. Syntax and type-system crates can store
/// stable IDs into side tables while this database owns freshness and exact dependency invalidation.
#[derive(Clone, Debug, Default)]
pub struct QueryDatabase {
    revision: u64,
    values: BTreeMap<QueryKey, QueryValue>,
    dependencies: BTreeMap<QueryKey, BTreeSet<QueryKey>>,
    reverse_dependencies: BTreeMap<QueryKey, BTreeSet<QueryKey>>,
    module_sources: BTreeMap<ModuleKey, SourceKey>,
    reverse_module_imports: BTreeMap<ModuleKey, BTreeSet<ModuleKey>>,
    external_consumers: BTreeMap<ModuleKey, BTreeSet<ModuleKey>>,
    artifact_consumers: BTreeMap<PackageId, BTreeSet<PackageId>>,
    artifact_dependencies: BTreeMap<PackageId, BTreeSet<PackageId>>,
    registered_artifact_edges: BTreeMap<PackageId, BTreeSet<(PackageId, PackageId)>>,
    registered_module_sources: BTreeMap<PackageId, BTreeMap<ModuleKey, SourceKey>>,
    registered_test_module_sources: BTreeMap<PackageId, BTreeMap<ModuleKey, SourceKey>>,
    registered_build_input_values: BTreeMap<PackageId, BTreeMap<QueryKey, Arc<[u8]>>>,
    pending_invalidated: BTreeSet<QueryKey>,
    execution: Option<QueryExecutionReport>,
    stats: QueryStats,
}

impl QueryDatabase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn stats(&self) -> QueryStats {
        self.stats
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn get(&mut self, key: &QueryKey) -> Option<QueryValue> {
        if let Some(value) = self.values.get(key) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            if let Some(execution) = &mut self.execution {
                execution.reused_queries.insert(key.clone());
            }
            Some(value.clone())
        } else {
            self.stats.misses = self.stats.misses.saturating_add(1);
            None
        }
    }

    pub fn begin_analysis(&mut self) {
        self.execution = Some(QueryExecutionReport {
            revision: self.revision,
            invalidated_queries: std::mem::take(&mut self.pending_invalidated),
            ..QueryExecutionReport::default()
        });
    }

    #[must_use]
    pub fn finish_analysis(&mut self) -> QueryExecutionReport {
        let mut report = self.execution.take().unwrap_or_default();
        report.revision = self.revision;
        report
    }

    pub fn typed_module(
        &mut self,
        module: &ModuleKey,
        definitions: &[Definition],
        semantic_context: &[u8; 32],
    ) -> Option<Arc<TypedModuleIr>> {
        let key = QueryKey::TypedModule(module.clone());
        match self.values.get(&key) {
            Some(QueryValue::TypedModule(cached))
                if cached.matches(definitions, semantic_context) =>
            {
                self.stats.hits = self.stats.hits.saturating_add(1);
                if let Some(execution) = &mut self.execution {
                    execution.reused_queries.insert(key);
                }
                Some(Arc::clone(&cached.module))
            }
            Some(
                QueryValue::TypedModule(_)
                | QueryValue::Bytes(_)
                | QueryValue::Syntax(_)
                | QueryValue::ModuleHeader(_),
            )
            | None => {
                self.stats.misses = self.stats.misses.saturating_add(1);
                None
            }
        }
    }

    /// Materializes a cached module into the current dense Definition table.
    ///
    /// Module-local definitions absent from the fresh declaration prepass (notably locals and
    /// analyzer-generated cleanup parameters) are appended deterministically. Every cached ID is
    /// then remapped through canonical identities. Missing identities make this a cache miss,
    /// never a partially remapped module.
    pub fn materialize_typed_module(
        &mut self,
        module: &ModuleKey,
        definitions: &mut Vec<Definition>,
    ) -> Option<Arc<TypedModuleIr>> {
        let key = QueryKey::TypedModule(module.clone());
        let QueryValue::TypedModule(cached) = self.values.get(&key)?.clone() else {
            self.stats.misses = self.stats.misses.saturating_add(1);
            return None;
        };
        let mut current = definitions
            .iter()
            .map(|definition| (definition.canonical_identity.clone(), definition.id))
            .collect::<BTreeMap<_, _>>();
        if current.len() != definitions.len() {
            self.stats.misses = self.stats.misses.saturating_add(1);
            return None;
        }

        let original_len = definitions.len();
        let mut appended = Vec::new();
        for cached_definition in cached.owned_definitions.iter() {
            if current.contains_key(&cached_definition.canonical_identity) {
                continue;
            }
            let id = DefinitionId(u32::try_from(definitions.len()).ok()?);
            let mut definition = cached_definition.clone();
            definition.id = id;
            current.insert(definition.canonical_identity.clone(), id);
            appended.push((definitions.len(), cached_definition.clone()));
            definitions.push(definition);
        }

        let mapping = cached
            .definition_layout
            .iter()
            .enumerate()
            .filter_map(|(old, identity)| {
                let old = DefinitionId(u32::try_from(old).ok()?);
                current.get(identity).copied().map(|new| (old, new))
            })
            .collect::<BTreeMap<_, _>>();
        for (index, cached_definition) in appended {
            let Ok(remapped) = crate::ir::remap_definition(cached_definition, &mapping) else {
                definitions.truncate(original_len);
                self.stats.misses = self.stats.misses.saturating_add(1);
                return None;
            };
            definitions[index] = remapped;
        }
        let Ok(remapped) = crate::ir::remap_typed_module(cached.module.as_ref(), &mapping) else {
            definitions.truncate(original_len);
            self.stats.misses = self.stats.misses.saturating_add(1);
            return None;
        };
        self.stats.hits = self.stats.hits.saturating_add(1);
        if let Some(execution) = &mut self.execution {
            execution.reused_queries.insert(key);
        }
        Some(Arc::new(remapped))
    }

    pub fn cached_bytes(&mut self, key: &QueryKey) -> Option<Arc<[u8]>> {
        match self.get(key) {
            Some(QueryValue::Bytes(bytes)) => Some(bytes),
            _ => None,
        }
    }

    pub fn record_resolved_imports(
        &mut self,
        module: ModuleKey,
        canonical: impl Into<Arc<[u8]>>,
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        let canonical: Arc<[u8]> = canonical.into();
        let key = QueryKey::ResolvedImports(module);
        let changed = match self.values.get(&key) {
            Some(QueryValue::Bytes(existing)) => existing.as_ref() != canonical.as_ref(),
            Some(
                QueryValue::Syntax(_) | QueryValue::ModuleHeader(_) | QueryValue::TypedModule(_),
            ) => true,
            None => false,
        };
        if changed {
            // A resolved namespace retarget is a real query-value change. Invalidate the current
            // value before replacing it so TypedModule/LinkedArtifact reverse dependents cannot
            // survive a direct overwrite. Equal bytes deliberately keep those hot dependents while
            // `insert` below refreshes the exact dependency edges for this analysis revision.
            self.invalidate_keys([key.clone()]);
        }
        self.insert(key, canonical, dependencies);
    }

    pub fn record_package_public_api(
        &mut self,
        package: PackageId,
        fingerprint: impl Into<Arc<[u8]>>,
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        let fingerprint: Arc<[u8]> = fingerprint.into();
        self.insert(
            QueryKey::PackagePublicApi(package),
            fingerprint,
            dependencies,
        );
    }

    pub fn record_package_state_schema(
        &mut self,
        package: PackageId,
        fingerprint: impl Into<Arc<[u8]>>,
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        let fingerprint: Arc<[u8]> = fingerprint.into();
        self.insert(
            QueryKey::PackageStateSchema(package),
            fingerprint,
            dependencies,
        );
    }

    pub fn record_linked_artifact(
        &mut self,
        package: PackageId,
        build_fingerprint: impl Into<Arc<[u8]>>,
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        let build_fingerprint: Arc<[u8]> = build_fingerprint.into();
        self.insert(
            QueryKey::LinkedArtifact(package),
            build_fingerprint,
            dependencies,
        );
    }

    pub fn store_typed_module(
        &mut self,
        module: ModuleKey,
        typed: Arc<TypedModuleIr>,
        definitions: &[Definition],
        semantic_context: [u8; 32],
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        if let Some(execution) = &mut self.execution {
            execution.analyzed_modules.insert(module.clone());
        }
        self.insert(
            QueryKey::TypedModule(module),
            Arc::new(CachedTypedModule::new(typed, definitions, semantic_context)),
            dependencies,
        );
    }

    pub fn insert(
        &mut self,
        key: QueryKey,
        value: impl Into<QueryValue>,
        dependencies: impl IntoIterator<Item = QueryKey>,
    ) {
        self.remove_dependency_edges(&key);
        let dependencies = dependencies.into_iter().collect::<BTreeSet<_>>();
        for dependency in &dependencies {
            self.reverse_dependencies
                .entry(dependency.clone())
                .or_default()
                .insert(key.clone());
        }
        self.dependencies.insert(key.clone(), dependencies);
        self.values.insert(key, value.into());
        self.stats.writes = self.stats.writes.saturating_add(1);
    }

    /// Parses and caches a lossless Nexa syntax tree under its stable [`SourceKey`].
    ///
    /// Reusing a key with different text invalidates every registered dependent query first.
    #[allow(clippy::needless_pass_by_value)]
    pub fn parse(
        &mut self,
        key: SourceKey,
        source: &str,
    ) -> Result<Arc<nexa_syntax::SyntaxTree>, nexa_syntax::SourceTooLarge> {
        let query = QueryKey::Parse(key.clone());
        if let Some(QueryValue::Syntax(old_tree)) = self.values.get(&query).cloned() {
            if old_tree.source.as_str() == source {
                self.stats.hits = self.stats.hits.saturating_add(1);
                if let Some(execution) = &mut self.execution {
                    execution.reused_queries.insert(query);
                }
                return Ok(old_tree);
            }

            let tree = Arc::new(nexa_syntax::parse_nexa(source)?);
            let old_header = extract_module_header(&old_tree, &key).ok();
            let new_header = extract_module_header(&tree, &key).ok();
            match (old_header, new_header) {
                (Some(old), Some(new)) if old.module == new.module => {
                    let module = ModuleKey::new(key.package_id.clone(), new.module.clone());
                    self.module_sources.insert(module.clone(), key.clone());
                    self.invalidate_change(&module, classify_header_change(&old, &new));
                }
                _ => {
                    // A malformed or retargeted module header cannot be classified narrowly.
                    // Follow the recorded dependency graph so no typed/header/import value can
                    // survive under the same stable SourceKey.
                    self.invalidate_keys([query.clone()]);
                }
            }
            if let Some(execution) = &mut self.execution {
                execution.parsed_sources.insert(key);
            }
            self.insert(query, Arc::clone(&tree), []);
            return Ok(tree);
        }

        self.stats.misses = self.stats.misses.saturating_add(1);
        let tree = Arc::new(nexa_syntax::parse_nexa(source)?);
        if let Some(execution) = &mut self.execution {
            execution.parsed_sources.insert(key.clone());
        }
        self.insert(query, Arc::clone(&tree), []);
        Ok(tree)
    }

    /// Replaces one source and compares typed syntax headers before selecting an invalidation
    /// scope. A private body edit therefore preserves header/import caches.
    #[allow(clippy::needless_pass_by_value)]
    pub fn update_source(
        &mut self,
        key: SourceKey,
        source: &str,
    ) -> Result<SourceUpdate, SourceUpdateError> {
        let parse_key = QueryKey::Parse(key.clone());
        let old_tree = match self.values.get(&parse_key) {
            Some(QueryValue::Syntax(tree)) => Some(Arc::clone(tree)),
            _ => None,
        };
        let new_tree =
            Arc::new(nexa_syntax::parse_nexa(source).map_err(SourceUpdateError::SourceTooLarge)?);
        let new_header =
            Arc::new(extract_module_header(&new_tree, &key).map_err(SourceUpdateError::Header)?);
        let old_header = old_tree
            .as_deref()
            .and_then(|tree| extract_module_header(tree, &key).ok());
        let impact = old_header.as_ref().map_or(ChangeImpact::PublicApi, |old| {
            classify_header_change(old, &new_header)
        });
        let module_key = ModuleKey::new(key.package_id.clone(), new_header.module.clone());
        self.module_sources.insert(module_key.clone(), key.clone());
        let invalidation = self.invalidate_change(&module_key, impact);
        self.insert(parse_key.clone(), Arc::clone(&new_tree), []);
        if impact != ChangeImpact::PrivateImplementation {
            self.insert(
                QueryKey::ModuleHeaders(module_key),
                Arc::clone(&new_header),
                [parse_key],
            );
        }
        Ok(SourceUpdate {
            tree: new_tree,
            header: new_header,
            impact,
            invalidation,
        })
    }

    /// Extracts typed module/import/declaration headers from the cached lossless tree.
    pub fn module_header(&mut self, source: &SourceKey) -> Result<Arc<ModuleHeader>, HeaderError> {
        let parse_key = QueryKey::Parse(source.clone());
        let tree = match self.values.get(&parse_key) {
            Some(QueryValue::Syntax(tree)) => Arc::clone(tree),
            _ => return Err(HeaderError::SourceNotParsed(source.clone())),
        };
        let extracted = extract_module_header(&tree, source)?;
        let module = extracted.module.clone();
        let module_key = ModuleKey::new(source.package_id.clone(), module.clone());
        let query_key = QueryKey::ModuleHeaders(module_key.clone());
        if let Some(QueryValue::ModuleHeader(header)) = self.values.get(&query_key) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            if let Some(execution) = &mut self.execution {
                execution.reused_queries.insert(query_key);
            }
            return Ok(Arc::clone(header));
        }
        let header = Arc::new(extracted);
        self.insert(
            query_key,
            Arc::clone(&header),
            [QueryKey::Parse(source.clone())],
        );
        self.module_sources.insert(module_key, source.clone());
        Ok(header)
    }

    pub fn record_module_source(&mut self, module: ModuleKey, source: SourceKey) {
        self.module_sources.insert(module, source);
    }

    /// Registers the exact package/module and static-artifact closure for one immutable build
    /// input. Re-registering a root replaces its prior artifact edges instead of accumulating stale
    /// dependencies.
    pub fn register_resolved_build_input(&mut self, input: &ResolvedBuildInput) {
        let root = input.root_package().clone();
        self.replace_registered_build_input_values(
            root.clone(),
            canonical_build_input_query_values(input),
        );
        let modules = input
            .all_source_sets()
            .flat_map(PackageSourceSet::production_units)
            .filter_map(|source| {
                source.expected_module_path().ok().map(|module| {
                    (
                        ModuleKey::new(source.key.package_id.clone(), module),
                        source.key.clone(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        self.replace_registered_module_sources(root.clone(), modules);
        self.registered_artifact_edges.insert(
            root,
            input
                .dependency_graph
                .edges
                .iter()
                .map(|edge| (edge.from.clone(), edge.to.clone()))
                .collect(),
        );
        self.artifact_dependencies.clear();
        self.artifact_consumers.clear();
        for edges in self.registered_artifact_edges.values() {
            for (from, to) in edges {
                self.artifact_dependencies
                    .entry(from.clone())
                    .or_default()
                    .insert(to.clone());
                self.artifact_consumers
                    .entry(to.clone())
                    .or_default()
                    .insert(from.clone());
            }
        }
    }

    /// Registers the exact test-only modules participating in the current root's test target.
    ///
    /// Test modules use the same parse/header/typed query machinery as product modules, but their
    /// ownership is tracked separately so a later product analysis can remove them before its
    /// execution report begins.
    pub fn register_test_module_sources(&mut self, test_sources: &PackageSourceSet) {
        let root = test_sources.package_id().clone();
        let modules = test_sources
            .test_units()
            .filter_map(|source| {
                source.expected_module_path().ok().map(|module| {
                    (
                        ModuleKey::new(source.key.package_id.clone(), module),
                        source.key.clone(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        let prior = self
            .registered_test_module_sources
            .insert(root, modules.clone())
            .unwrap_or_default();
        let mut invalidation_roots = Vec::new();
        for (module, prior_source) in prior {
            if modules.contains_key(&module) {
                continue;
            }
            self.remove_test_module_source(&module, &prior_source, &mut invalidation_roots);
        }
        for (module, source) in modules {
            if let Some(prior_source) = self.module_sources.insert(module.clone(), source.clone())
                && prior_source != source
            {
                invalidation_roots.extend([
                    QueryKey::Parse(prior_source),
                    QueryKey::ModuleHeaders(module.clone()),
                    QueryKey::ResolvedImports(module.clone()),
                    QueryKey::TypedModule(module),
                ]);
            }
        }
        if !invalidation_roots.is_empty() {
            self.invalidate_keys(invalidation_roots);
        }
    }

    /// Removes all test-only query ownership and import edges for one root.
    ///
    /// Product analysis invokes this before [`Self::begin_analysis`], so stale test cleanup cannot
    /// appear as a product invalidation or leak test module edges into `PackageCheckReport`.
    pub fn discard_test_module_sources(&mut self, root: &PackageId) {
        let prior = self
            .registered_test_module_sources
            .remove(root)
            .unwrap_or_default();
        let mut invalidation_roots = Vec::new();
        for (module, source) in prior {
            self.remove_test_module_source(&module, &source, &mut invalidation_roots);
        }
        if !invalidation_roots.is_empty() {
            let pending_before_cleanup = self.pending_invalidated.clone();
            let cleanup = self.invalidate_keys(invalidation_roots);
            // `begin_analysis` promotes `pending_invalidated` into the next execution report.
            // This cleanup belongs to the prior explicit test target, not the product revision
            // about to start, so remove its entire dependency closure from that pending evidence.
            for key in cleanup.invalidated_queries {
                if !pending_before_cleanup.contains(&key) {
                    self.pending_invalidated.remove(&key);
                }
            }
        }
    }

    fn remove_test_module_source(
        &mut self,
        module: &ModuleKey,
        source: &SourceKey,
        invalidation_roots: &mut Vec<QueryKey>,
    ) {
        let replacement = self
            .registered_module_sources
            .values()
            .find_map(|registered| registered.get(module).cloned())
            .or_else(|| {
                self.registered_test_module_sources
                    .values()
                    .find_map(|registered| registered.get(module).cloned())
            });
        if let Some(replacement) = replacement {
            self.module_sources.insert(module.clone(), replacement);
        } else if self.module_sources.get(module) == Some(source) {
            self.module_sources.remove(module);
        }
        let affected_consumers = self.remove_module_import_edges(module);
        invalidation_roots.extend([
            QueryKey::Parse(source.clone()),
            QueryKey::ModuleHeaders(module.clone()),
            QueryKey::ResolvedImports(module.clone()),
            QueryKey::TypedModule(module.clone()),
        ]);
        extend_consumer_invalidation_roots(invalidation_roots, affected_consumers);
    }

    fn replace_registered_build_input_values(
        &mut self,
        root: PackageId,
        values: BTreeMap<QueryKey, Arc<[u8]>>,
    ) {
        let prior = self
            .registered_build_input_values
            .remove(&root)
            .unwrap_or_default();
        let keys = prior
            .keys()
            .chain(values.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in keys {
            let replacement = values.get(&key).cloned().or_else(|| {
                self.registered_build_input_values
                    .values()
                    .find_map(|registered| registered.get(&key).cloned())
            });
            if let Some(replacement) = replacement {
                self.register_input_value(key, replacement);
            } else {
                self.invalidate_keys([key]);
            }
        }
        self.registered_build_input_values.insert(root, values);
    }

    fn register_input_value(&mut self, key: QueryKey, value: Arc<[u8]>) {
        if matches!(
            self.values.get(&key),
            Some(QueryValue::Bytes(existing)) if existing.as_ref() == value.as_ref()
        ) {
            self.stats.hits = self.stats.hits.saturating_add(1);
            if let Some(execution) = &mut self.execution {
                execution.reused_queries.insert(key);
            }
            return;
        }
        self.invalidate_keys([key.clone()]);
        self.insert(key, value, []);
    }

    pub fn unregister_resolved_build_input(&mut self, root: &PackageId) {
        self.discard_test_module_sources(root);
        let prior_values = self
            .registered_build_input_values
            .remove(root)
            .unwrap_or_default();
        for key in prior_values.keys() {
            let replacement = self
                .registered_build_input_values
                .values()
                .find_map(|registered| registered.get(key).cloned());
            if let Some(replacement) = replacement {
                self.register_input_value(key.clone(), replacement);
            } else {
                self.invalidate_keys([key.clone()]);
            }
        }
        self.registered_artifact_edges.remove(root);
        self.replace_registered_module_sources(root.clone(), BTreeMap::new());
        self.registered_module_sources.remove(root);
        self.artifact_dependencies.clear();
        self.artifact_consumers.clear();
        for edges in self.registered_artifact_edges.values() {
            for (from, to) in edges {
                self.artifact_dependencies
                    .entry(from.clone())
                    .or_default()
                    .insert(to.clone());
                self.artifact_consumers
                    .entry(to.clone())
                    .or_default()
                    .insert(from.clone());
            }
        }
    }

    fn replace_registered_module_sources(
        &mut self,
        root: PackageId,
        modules: BTreeMap<ModuleKey, SourceKey>,
    ) {
        let prior = self
            .registered_module_sources
            .insert(root, modules.clone())
            .unwrap_or_default();
        let removed = prior
            .keys()
            .filter(|module| !modules.contains_key(*module))
            .cloned()
            .collect::<Vec<_>>();
        let mut invalidation_roots = Vec::new();
        for module in removed {
            let prior_source = prior
                .get(&module)
                .expect("removed module belonged to prior registration")
                .clone();
            let replacement = self
                .registered_module_sources
                .values()
                .find_map(|registered| registered.get(&module).cloned());
            if let Some(replacement) = replacement {
                let previous = self
                    .module_sources
                    .insert(module.clone(), replacement.clone());
                if let Some(previous) = previous
                    && previous != replacement
                {
                    let affected_consumers = self.module_import_consumers(&module);
                    self.clear_module_imports(&module);
                    invalidation_roots.extend([
                        QueryKey::Parse(previous),
                        QueryKey::ModuleHeaders(module.clone()),
                        QueryKey::ResolvedImports(module.clone()),
                        QueryKey::TypedModule(module),
                    ]);
                    extend_consumer_invalidation_roots(&mut invalidation_roots, affected_consumers);
                }
            } else {
                self.module_sources.remove(&module);
                let affected_consumers = self.remove_module_import_edges(&module);
                invalidation_roots.extend([
                    QueryKey::Parse(prior_source),
                    QueryKey::ModuleHeaders(module.clone()),
                    QueryKey::ResolvedImports(module.clone()),
                    QueryKey::TypedModule(module),
                ]);
                extend_consumer_invalidation_roots(&mut invalidation_roots, affected_consumers);
            }
        }
        for (module, source) in modules {
            if let Some(prior_source) = self.module_sources.insert(module.clone(), source.clone())
                && prior_source != source
            {
                let affected_consumers = self.module_import_consumers(&module);
                self.clear_module_imports(&module);
                invalidation_roots.extend([
                    QueryKey::Parse(prior_source),
                    QueryKey::ModuleHeaders(module.clone()),
                    QueryKey::ResolvedImports(module.clone()),
                    QueryKey::TypedModule(module),
                ]);
                extend_consumer_invalidation_roots(&mut invalidation_roots, affected_consumers);
            }
        }
        if !invalidation_roots.is_empty() {
            self.invalidate_keys(invalidation_roots);
        }
    }

    /// Removes all previously resolved source/dependency imports made by one module before a new
    /// analysis revision records its exact import set.
    pub fn clear_module_imports(&mut self, importer: &ModuleKey) {
        self.reverse_module_imports.retain(|_, consumers| {
            consumers.remove(importer);
            !consumers.is_empty()
        });
        self.external_consumers.retain(|_, consumers| {
            consumers.remove(importer);
            !consumers.is_empty()
        });
    }

    fn module_import_consumers(&self, target: &ModuleKey) -> BTreeSet<ModuleKey> {
        // Removing a module is conservatively a public-surface change. Capture the complete
        // consumer closure before detaching the incoming edges so downstream typed modules cannot
        // retain a DefinitionId/type view derived through an intermediate importer.
        self.public_reverse_closure(target)
    }

    fn remove_module_import_edges(&mut self, module: &ModuleKey) -> BTreeSet<ModuleKey> {
        let affected_consumers = self.module_import_consumers(module);
        self.clear_module_imports(module);
        self.reverse_module_imports.remove(module);
        self.external_consumers.remove(module);
        affected_consumers
    }

    /// Records a resolved source-module import (`imported -> importer` for invalidation).
    pub fn record_module_import(&mut self, importer: ModuleKey, target_module: ModuleKey) {
        self.reverse_module_imports
            .entry(target_module)
            .or_default()
            .insert(importer);
    }

    /// Records a module that imports an exact public module from a dependency package.
    pub fn record_dependency_import(&mut self, importer: ModuleKey, dependency_module: ModuleKey) {
        self.external_consumers
            .entry(dependency_module)
            .or_default()
            .insert(importer);
    }

    /// Returns the exact resolved source-module import graph in canonical `(importer, target)`
    /// order. This is analysis evidence, not a reconstruction from source text.
    #[must_use]
    pub fn resolved_module_imports(&self) -> Vec<(ModuleKey, ModuleKey)> {
        let mut edges = self
            .reverse_module_imports
            .iter()
            .flat_map(|(target, importers)| {
                importers
                    .iter()
                    .cloned()
                    .map(|importer| (importer, target.clone()))
            })
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }

    /// Returns dependency namespace uses in canonical `(importer, dependency module)` order.
    #[must_use]
    pub fn resolved_dependency_imports(&self) -> Vec<(ModuleKey, ModuleKey)> {
        let mut edges = self
            .external_consumers
            .iter()
            .flat_map(|(target, importers)| {
                importers
                    .iter()
                    .cloned()
                    .map(|importer| (importer, target.clone()))
            })
            .collect::<Vec<_>>();
        edges.sort();
        edges
    }

    /// Records that a root package artifact statically links a dependency package.
    pub fn record_artifact_dependency(
        &mut self,
        root_package: PackageId,
        dependency_package: PackageId,
    ) {
        self.artifact_consumers
            .entry(dependency_package.clone())
            .or_default()
            .insert(root_package.clone());
        self.artifact_dependencies
            .entry(root_package)
            .or_default()
            .insert(dependency_package);
    }

    pub fn update_source_set(
        &mut self,
        root_package: &PackageId,
        change: SourceSetChange,
    ) -> BuildInputUpdate {
        let mut exact = BTreeSet::from([
            QueryKey::SourceSet(root_package.clone()),
            QueryKey::LinkedArtifact(root_package.clone()),
        ]);
        let mut scheduled_sources = BTreeSet::new();
        let mut scheduled_modules = BTreeSet::new();
        let mut relinked_modules = self.artifact_modules(root_package);
        match change {
            SourceSetChange::Add { source, module } => {
                scheduled_sources.insert(source.clone());
                scheduled_modules.insert(module.clone());
                self.module_sources.insert(module.clone(), source);
                relinked_modules.insert(module);
            }
            SourceSetChange::Delete { source, module } => {
                scheduled_sources.insert(source.clone());
                scheduled_modules.insert(module.clone());
                let affected_consumers = self.remove_module_import_edges(&module);
                scheduled_modules.extend(affected_consumers.iter().cloned());
                exact.extend([
                    QueryKey::Parse(source),
                    QueryKey::ModuleHeaders(module.clone()),
                    QueryKey::ResolvedImports(module.clone()),
                    QueryKey::TypedModule(module.clone()),
                ]);
                extend_consumer_invalidation_keys(&mut exact, affected_consumers);
                self.module_sources.remove(&module);
                relinked_modules.remove(&module);
            }
            SourceSetChange::Rename {
                old_source,
                old_module,
                new_source,
                new_module,
            } => {
                scheduled_sources.extend([old_source.clone(), new_source.clone()]);
                scheduled_modules.extend([old_module.clone(), new_module.clone()]);
                let affected_consumers = self.remove_module_import_edges(&old_module);
                scheduled_modules.extend(affected_consumers.iter().cloned());
                exact.extend([
                    QueryKey::Parse(old_source),
                    QueryKey::ModuleHeaders(old_module.clone()),
                    QueryKey::ResolvedImports(old_module.clone()),
                    QueryKey::TypedModule(old_module.clone()),
                ]);
                extend_consumer_invalidation_keys(&mut exact, affected_consumers);
                self.module_sources.remove(&old_module);
                self.module_sources.insert(new_module.clone(), new_source);
                relinked_modules.remove(&old_module);
                relinked_modules.insert(new_module);
            }
            SourceSetChange::Aba { source, module } => {
                scheduled_sources.insert(source.clone());
                scheduled_modules.insert(module.clone());
                exact.extend([QueryKey::Parse(source), QueryKey::TypedModule(module)]);
            }
        }
        // Direct source-set updates are public invalidation inputs, not merely scheduling hints.
        // Follow reverse dependencies so a consumer TypedModule eviction also removes every linked
        // artifact derived from it; otherwise delete/rename/ABA can leave a stale artifact cached.
        let mut invalidation = self.invalidate_keys(exact);
        invalidation.parsed_sources.extend(scheduled_sources);
        invalidation.analyzed_modules.extend(scheduled_modules);
        BuildInputUpdate {
            invalidation,
            relinked_modules,
        }
    }

    pub fn update_manifest(
        &mut self,
        package: &PackageId,
        manifest_source: SourceKey,
    ) -> BuildInputUpdate {
        let relinked_modules = self.artifact_modules(package);
        let mut invalidation = self.invalidate_exact([
            QueryKey::PackageManifest(package.clone()),
            QueryKey::LinkedArtifact(package.clone()),
        ]);
        invalidation.parsed_sources.insert(manifest_source);
        invalidation
            .analyzed_modules
            .extend(relinked_modules.iter().cloned());
        BuildInputUpdate {
            invalidation,
            relinked_modules,
        }
    }

    pub fn update_lock(&mut self, package: &PackageId, lock_source: SourceKey) -> BuildInputUpdate {
        let relinked_modules = self.artifact_modules(package);
        let mut invalidation = self.invalidate_exact([
            QueryKey::DependencyGraph(package.clone()),
            QueryKey::LinkedArtifact(package.clone()),
        ]);
        invalidation.parsed_sources.insert(lock_source);
        invalidation
            .analyzed_modules
            .extend(relinked_modules.iter().cloned());
        BuildInputUpdate {
            invalidation,
            relinked_modules,
        }
    }

    pub fn update_contract(
        &mut self,
        package: &PackageId,
        contract_source: SourceKey,
    ) -> BuildInputUpdate {
        let relinked_modules = self.artifact_modules(package);
        let mut exact = BTreeSet::from([
            QueryKey::HostContract(package.clone()),
            QueryKey::LinkedArtifact(package.clone()),
        ]);
        exact.extend(relinked_modules.iter().cloned().map(QueryKey::TypedModule));
        let mut invalidation = self.invalidate_exact(exact);
        invalidation.parsed_sources.insert(contract_source);
        invalidation
            .analyzed_modules
            .extend(relinked_modules.iter().cloned());
        BuildInputUpdate {
            invalidation,
            relinked_modules,
        }
    }

    pub fn invalidate_change(
        &mut self,
        changed: &ModuleKey,
        impact: ChangeImpact,
    ) -> InvalidationReport {
        let mut exact = BTreeSet::new();
        if let Some(source) = self.module_sources.get(changed) {
            exact.insert(QueryKey::Parse(source.clone()));
        }
        exact.insert(QueryKey::TypedModule(changed.clone()));

        let mut affected_modules = BTreeSet::from([changed.clone()]);
        match impact {
            ChangeImpact::PrivateImplementation => {}
            ChangeImpact::ModuleLocalSurface => {
                exact.insert(QueryKey::ModuleHeaders(changed.clone()));
                exact.insert(QueryKey::ResolvedImports(changed.clone()));
            }
            ChangeImpact::PackageApi | ChangeImpact::StateSchema => {
                affected_modules.extend(self.local_reverse_closure(changed));
                exact.insert(QueryKey::ModuleHeaders(changed.clone()));
                exact.insert(QueryKey::ResolvedImports(changed.clone()));
            }
            ChangeImpact::PublicApi => {
                affected_modules.extend(self.public_reverse_closure(changed));
                exact.insert(QueryKey::ModuleHeaders(changed.clone()));
                exact.insert(QueryKey::ResolvedImports(changed.clone()));
                exact.insert(QueryKey::PackagePublicApi(changed.package_id.clone()));
            }
        }
        if impact == ChangeImpact::StateSchema {
            exact.insert(QueryKey::PackageStateSchema(changed.package_id.clone()));
        }
        for module in affected_modules {
            exact.insert(QueryKey::TypedModule(module));
        }
        exact.insert(QueryKey::LinkedArtifact(changed.package_id.clone()));
        for root in self.artifact_reverse_closure(&changed.package_id) {
            exact.insert(QueryKey::LinkedArtifact(root));
        }
        self.invalidate_exact(exact)
    }

    pub fn invalidate_keys(
        &mut self,
        roots: impl IntoIterator<Item = QueryKey>,
    ) -> InvalidationReport {
        let mut pending = roots.into_iter().collect::<VecDeque<_>>();
        let mut invalidated = BTreeSet::new();
        while let Some(key) = pending.pop_front() {
            if !self.values.contains_key(&key) || !invalidated.insert(key.clone()) {
                continue;
            }
            if let Some(dependents) = self.reverse_dependencies.get(&key) {
                pending.extend(dependents.iter().cloned());
            }
        }
        self.finish_invalidation(invalidated)
    }

    fn invalidate_exact(&mut self, keys: impl IntoIterator<Item = QueryKey>) -> InvalidationReport {
        let invalidated = keys
            .into_iter()
            .filter(|key| self.values.contains_key(key))
            .collect::<BTreeSet<_>>();
        self.finish_invalidation(invalidated)
    }

    fn finish_invalidation(&mut self, invalidated: BTreeSet<QueryKey>) -> InvalidationReport {
        for key in &invalidated {
            self.values.remove(key);
            self.remove_dependency_edges(key);
        }
        for key in &invalidated {
            let remove_reverse = self
                .reverse_dependencies
                .get(key)
                .is_some_and(|dependents| {
                    dependents
                        .iter()
                        .all(|dependent| !self.values.contains_key(dependent))
                });
            if remove_reverse {
                self.reverse_dependencies.remove(key);
            }
        }
        if !invalidated.is_empty() {
            self.revision = self.revision.saturating_add(1);
            self.stats.invalidations = self
                .stats
                .invalidations
                .saturating_add(u64::try_from(invalidated.len()).unwrap_or(u64::MAX));
        }
        if let Some(execution) = &mut self.execution {
            execution
                .invalidated_queries
                .extend(invalidated.iter().cloned());
        } else {
            self.pending_invalidated.extend(invalidated.iter().cloned());
        }
        let parsed_sources = invalidated
            .iter()
            .filter_map(|key| match key {
                QueryKey::Parse(source) => Some(source.clone()),
                _ => None,
            })
            .collect();
        let analyzed_modules = invalidated
            .iter()
            .filter_map(|key| match key {
                QueryKey::ModuleHeaders(module)
                | QueryKey::ResolvedImports(module)
                | QueryKey::TypedModule(module) => Some(module.clone()),
                QueryKey::Parse(_)
                | QueryKey::PackagePublicApi(_)
                | QueryKey::PackageStateSchema(_)
                | QueryKey::SourceSet(_)
                | QueryKey::PackageManifest(_)
                | QueryKey::DependencyGraph(_)
                | QueryKey::HostContract(_)
                | QueryKey::LinkedArtifact(_) => None,
            })
            .collect();
        InvalidationReport {
            revision: self.revision,
            parsed_sources,
            analyzed_modules,
            invalidated_queries: invalidated,
        }
    }

    fn local_reverse_closure(&self, changed: &ModuleKey) -> BTreeSet<ModuleKey> {
        let mut result = BTreeSet::new();
        let mut pending = VecDeque::from([changed.clone()]);
        while let Some(module) = pending.pop_front() {
            if let Some(consumers) = self.reverse_module_imports.get(&module) {
                for consumer in consumers {
                    if consumer.package_id == changed.package_id && result.insert(consumer.clone())
                    {
                        pending.push_back(consumer.clone());
                    }
                }
            }
        }
        result
    }

    fn public_reverse_closure(&self, changed: &ModuleKey) -> BTreeSet<ModuleKey> {
        let mut result = BTreeSet::new();
        let mut seen = BTreeSet::from([changed.clone()]);
        let mut pending = VecDeque::from([changed.clone()]);
        while let Some(target) = pending.pop_front() {
            for consumer in self
                .reverse_module_imports
                .get(&target)
                .into_iter()
                .chain(self.external_consumers.get(&target))
                .flatten()
            {
                if seen.insert(consumer.clone()) {
                    result.insert(consumer.clone());
                    pending.push_back(consumer.clone());
                }
            }
        }
        result
    }

    fn artifact_reverse_closure(&self, changed: &PackageId) -> BTreeSet<PackageId> {
        let mut result = BTreeSet::new();
        let mut pending = VecDeque::from([changed.clone()]);
        while let Some(package) = pending.pop_front() {
            if let Some(consumers) = self.artifact_consumers.get(&package) {
                for consumer in consumers {
                    if result.insert(consumer.clone()) {
                        pending.push_back(consumer.clone());
                    }
                }
            }
        }
        result
    }

    fn artifact_modules(&self, root: &PackageId) -> BTreeSet<ModuleKey> {
        let mut packages = BTreeSet::from([root.clone()]);
        let mut pending = VecDeque::from([root.clone()]);
        while let Some(package) = pending.pop_front() {
            if let Some(dependencies) = self.artifact_dependencies.get(&package) {
                for dependency in dependencies {
                    if packages.insert(dependency.clone()) {
                        pending.push_back(dependency.clone());
                    }
                }
            }
        }
        self.module_sources
            .keys()
            .filter(|module| packages.contains(&module.package_id))
            .cloned()
            .collect()
    }

    fn remove_dependency_edges(&mut self, key: &QueryKey) {
        if let Some(dependencies) = self.dependencies.remove(key) {
            for dependency in dependencies {
                let remove_entry =
                    self.reverse_dependencies
                        .get_mut(&dependency)
                        .is_some_and(|dependents| {
                            dependents.remove(key);
                            dependents.is_empty()
                        });
                if remove_entry {
                    self.reverse_dependencies.remove(&dependency);
                }
            }
        }
    }
}

fn extend_consumer_invalidation_roots(
    roots: &mut Vec<QueryKey>,
    consumers: impl IntoIterator<Item = ModuleKey>,
) {
    for consumer in consumers {
        roots.extend([
            QueryKey::ResolvedImports(consumer.clone()),
            QueryKey::TypedModule(consumer),
        ]);
    }
}

fn extend_consumer_invalidation_keys(
    keys: &mut BTreeSet<QueryKey>,
    consumers: impl IntoIterator<Item = ModuleKey>,
) {
    for consumer in consumers {
        keys.extend([
            QueryKey::ResolvedImports(consumer.clone()),
            QueryKey::TypedModule(consumer),
        ]);
    }
}

fn extract_module_header(
    tree: &nexa_syntax::SyntaxTree,
    source: &SourceKey,
) -> Result<ModuleHeader, HeaderError> {
    let ast = nexa_syntax::ast::parse_nexa_ast(tree);
    let module = module_path_for_source(source).map_err(HeaderError::InvalidIdentity)?;
    let imports = ast
        .uses
        .iter()
        .map(|usage| {
            let path = std::iter::once(usage.root.name.text.as_str())
                .chain(usage.segments.iter().map(|segment| segment.text.as_str()))
                .collect::<Vec<_>>()
                .join(".");
            Ok(ImportHeader {
                path: ModulePath::new(path).map_err(HeaderError::InvalidIdentity)?,
                alias: usage.alias.as_ref().map(|alias| alias.text.clone()),
            })
        })
        .collect::<Result<Vec<_>, HeaderError>>()?;
    let declarations = ast
        .declarations
        .iter()
        .filter_map(|declaration| {
            let (name, kind) = match &declaration.kind {
                nexa_syntax::ast::DeclarationKind::Function(function) => {
                    (&function.name.text, HeaderDeclarationKind::Function)
                }
                nexa_syntax::ast::DeclarationKind::Type(ty) => (
                    &ty.name.text,
                    match ty.kind {
                        nexa_syntax::ast::TypeDeclarationKind::Struct => {
                            HeaderDeclarationKind::Struct
                        }
                        nexa_syntax::ast::TypeDeclarationKind::Enum => HeaderDeclarationKind::Enum,
                        nexa_syntax::ast::TypeDeclarationKind::Class => {
                            HeaderDeclarationKind::Class
                        }
                    },
                ),
                nexa_syntax::ast::DeclarationKind::Const(constant) => {
                    (&constant.name.text, HeaderDeclarationKind::Const)
                }
                nexa_syntax::ast::DeclarationKind::Error => return None,
            };
            let visibility = match declaration.visibility {
                nexa_syntax::ast::Visibility::Private => DeclarationVisibility::Private,
                nexa_syntax::ast::Visibility::Package => DeclarationVisibility::Package,
                nexa_syntax::ast::Visibility::Public => DeclarationVisibility::Public,
            };
            Some(DeclarationHeader {
                name: name.clone(),
                kind,
                visibility,
                attributes: declaration
                    .attributes
                    .iter()
                    .map(|attribute| attribute.name.text.clone())
                    .collect(),
                signature: declaration_surface(tree, declaration.range, kind),
            })
        })
        .collect();
    Ok(ModuleHeader {
        module,
        imports,
        declarations,
        syntax_error_count: tree.errors.len().saturating_add(ast.errors.len()),
    })
}

fn module_path_for_source(source: &SourceKey) -> Result<ModulePath, crate::IdentityError> {
    if let Some(relative) = source
        .path
        .as_str()
        .strip_prefix("tests/")
        .and_then(|path| path.strip_suffix(".nexa"))
    {
        return ModulePath::new(format!("test.{}", relative.replace('/', ".")));
    }
    ModulePath::from_source_path(&source.path)
}

fn declaration_surface(
    tree: &nexa_syntax::SyntaxTree,
    range: nexa_syntax::TextRange,
    kind: HeaderDeclarationKind,
) -> String {
    let mut output = String::new();
    for token in &tree.tokens {
        if token.range.start < range.start || token.range.end > range.end || token.kind.is_trivia()
        {
            continue;
        }
        if kind == HeaderDeclarationKind::Function && token.kind == nexa_syntax::TokenKind::LBrace {
            break;
        }
        output.push_str(tree.token_text(token));
    }
    output
}

fn classify_header_change(old: &ModuleHeader, new: &ModuleHeader) -> ChangeImpact {
    if old == new {
        return ChangeImpact::PrivateImplementation;
    }
    if old.module != new.module {
        return ChangeImpact::PublicApi;
    }
    let visibility_surface = |header: &ModuleHeader, visibility| {
        header
            .declarations
            .iter()
            .filter(|declaration| declaration.visibility == visibility)
            .cloned()
            .collect::<Vec<_>>()
    };
    let state_surface = |header: &ModuleHeader| {
        header
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.kind == HeaderDeclarationKind::Class
                    && declaration
                        .attributes
                        .iter()
                        .any(|attribute| attribute == "state")
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    if state_surface(old) != state_surface(new) {
        return ChangeImpact::StateSchema;
    }
    let old_public = old
        .declarations
        .iter()
        .filter(|declaration| declaration.visibility == DeclarationVisibility::Public)
        .cloned()
        .collect::<Vec<_>>();
    let new_public = new
        .declarations
        .iter()
        .filter(|declaration| declaration.visibility == DeclarationVisibility::Public)
        .cloned()
        .collect::<Vec<_>>();
    if old_public != new_public {
        ChangeImpact::PublicApi
    } else if visibility_surface(old, DeclarationVisibility::Package)
        != visibility_surface(new, DeclarationVisibility::Package)
    {
        ChangeImpact::PackageApi
    } else {
        ChangeImpact::ModuleLocalSurface
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderError {
    SourceNotParsed(SourceKey),
    InvalidImport(SourceKey),
    InvalidIdentity(crate::IdentityError),
}

impl std::fmt::Display for HeaderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HeaderError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceUpdateError {
    SourceTooLarge(nexa_syntax::SourceTooLarge),
    Header(HeaderError),
}

impl std::fmt::Display for SourceUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SourceUpdateError {}

pub(crate) fn semantic_input_query_keys(input: &ResolvedBuildInput) -> BTreeSet<QueryKey> {
    let root = input.root_package().clone();
    let mut keys = BTreeSet::from([
        QueryKey::PackageManifest(root.clone()),
        QueryKey::DependencyGraph(root.clone()),
        QueryKey::HostContract(root),
    ]);
    keys.extend(
        input
            .dependency_manifests
            .keys()
            .cloned()
            .map(QueryKey::PackageManifest),
    );
    keys
}

pub(crate) fn linked_input_query_keys(input: &ResolvedBuildInput) -> BTreeSet<QueryKey> {
    let mut keys = semantic_input_query_keys(input);
    keys.insert(QueryKey::SourceSet(input.root_package().clone()));
    keys.extend(
        input
            .dependency_source_sets
            .keys()
            .cloned()
            .map(QueryKey::SourceSet),
    );
    keys
}

pub(crate) fn module_header_query_keys(input: &ResolvedBuildInput) -> BTreeSet<QueryKey> {
    input
        .all_source_sets()
        .flat_map(PackageSourceSet::production_units)
        .filter_map(|source| {
            source.expected_module_path().ok().map(|module| {
                QueryKey::ModuleHeaders(ModuleKey::new(source.key.package_id.clone(), module))
            })
        })
        .collect()
}

/// Fingerprint of inputs that can change typed semantics without changing source text.
///
/// Source-set fingerprints are intentionally excluded: `parse` and the recorded module/import
/// dependency graph invalidate only the modules affected by an edit. Add/delete/rename changes
/// are additionally guarded by the dense definition layout. Manifests, dependency retargeting,
/// Host ABI, compiler options, and compiler-provided surfaces must all force a cache miss even
/// when declaration identities happen to remain unchanged.
pub(crate) fn typed_module_semantic_context(input: &ResolvedBuildInput) -> [u8; 32] {
    let fingerprint = input.fingerprint_input.as_ref();
    let mut builder = FingerprintBuilder::new("nexa.analysis.typed-module-context", 1);
    builder.field_str("root-package", fingerprint.root_package.as_str());
    builder.field_bytes("root-manifest", &fingerprint.root_manifest);
    builder.field_u64(
        "dependency-manifest-count",
        u64::try_from(fingerprint.dependency_manifests.len()).unwrap_or(u64::MAX),
    );
    for (package, manifest) in &fingerprint.dependency_manifests {
        builder.field_str("dependency-package", package.as_str());
        builder.field_bytes("dependency-manifest", manifest);
    }
    builder.field_bytes(
        "dependency-graph",
        &input.dependency_graph.canonical_identity_bytes(),
    );
    builder.field_bytes("host-contract", &fingerprint.host_contract);
    builder.field_bytes(
        "host-required-exports",
        &fingerprint.host_required_entrypoints,
    );
    builder.field_bytes(
        "language-version",
        &fingerprint.language_version.to_le_bytes(),
    );
    builder.field_str(
        "standard-library-version",
        &fingerprint.standard_library_version,
    );
    builder.field_bytes(
        "standard-library-descriptor",
        &fingerprint.standard_library_descriptor,
    );
    builder.field_str("compiler-version", &fingerprint.compiler_version);
    builder.field_u32("bytecode-version", fingerprint.bytecode_version);
    builder.field_u32(
        "runtime-semantics-version",
        fingerprint.runtime_semantics_version,
    );
    builder.field_u32(
        "opcode-cost-table-version",
        fingerprint.opcode_cost_table_version,
    );
    builder.field_str(
        "deterministic-math-backend",
        &fingerprint.deterministic_math_backend,
    );
    builder.field_bytes("compiler-options", &fingerprint.compiler_options);
    builder.finish_bytes()
}

fn canonical_build_input_query_values(input: &ResolvedBuildInput) -> BTreeMap<QueryKey, Arc<[u8]>> {
    let root = input.root_package().clone();
    let mut values = BTreeMap::from([
        (
            QueryKey::SourceSet(root.clone()),
            Arc::<[u8]>::from(canonical_source_set_membership(&input.root_source_set)),
        ),
        (
            QueryKey::PackageManifest(root.clone()),
            Arc::<[u8]>::from(input.root_manifest.canonical_bytes()),
        ),
        (
            QueryKey::DependencyGraph(root.clone()),
            Arc::<[u8]>::from(input.dependency_graph.canonical_identity_bytes()),
        ),
        (
            QueryKey::HostContract(root),
            Arc::<[u8]>::from(canonical_host_contract_query_value(input)),
        ),
    ]);
    for (package, manifest) in input.dependency_manifests.iter() {
        values.insert(
            QueryKey::PackageManifest(package.clone()),
            Arc::from(manifest.canonical_bytes()),
        );
    }
    for (package, sources) in input.dependency_source_sets.iter() {
        values.insert(
            QueryKey::SourceSet(package.clone()),
            Arc::<[u8]>::from(canonical_source_set_membership(sources)),
        );
    }
    values
}

fn canonical_source_set_membership(source_set: &PackageSourceSet) -> [u8; 32] {
    let mut builder = FingerprintBuilder::new("nexa.analysis.source-set-membership", 1);
    builder.field_str("package", source_set.package_id().as_str());
    let units = source_set.production_units().collect::<Vec<_>>();
    builder.field_u64(
        "source-count",
        u64::try_from(units.len()).unwrap_or(u64::MAX),
    );
    for unit in units {
        builder.field_str("source-path", unit.key.path.as_str());
        if let Some(module) = unit.virtual_module_path() {
            builder.field_str("module-kind", "virtual");
            builder.field_str("module", module.as_str());
        } else {
            builder.field_str("module-kind", "path-derived");
            match unit.expected_module_path() {
                Ok(module) => builder.field_str("module", module.as_str()),
                Err(error) => builder.field_str("invalid-module", &error.to_string()),
            }
        }
    }
    builder.finish_bytes()
}

fn canonical_host_contract_query_value(input: &ResolvedBuildInput) -> [u8; 32] {
    let mut builder = FingerprintBuilder::new("nexa.analysis.host-contract-input", 1);
    builder.field_bytes("semantic-contract", &input.canonical_host_contract);
    builder.field_bytes("source-identity", &input.host_contract_source_identity);
    builder.field_bytes(
        "required-exports",
        &input.fingerprint_input.host_required_entrypoints,
    );
    builder.finish_bytes()
}

fn canonical_definition_layout(definitions: &[Definition]) -> Arc<[String]> {
    definitions
        .iter()
        .map(|definition| definition.canonical_identity.clone())
        .collect::<Vec<_>>()
        .into()
}

#[cfg(test)]
mod tests {
    use crate::{ModulePath, NormalizedPackagePath};

    use super::*;

    fn module(package: &str, module: &str) -> ModuleKey {
        ModuleKey::new(
            PackageId::new(package).unwrap(),
            ModulePath::new(module).unwrap(),
        )
    }

    fn source(module: &ModuleKey) -> SourceKey {
        SourceKey::new(
            module.package_id.clone(),
            NormalizedPackagePath::new(format!(
                "src/{}.nexa",
                module.module.as_str().replace('.', "/")
            ))
            .unwrap(),
        )
    }

    fn seed(db: &mut QueryDatabase, modules: &[ModuleKey]) {
        for module in modules {
            let source = source(module);
            db.record_module_source(module.clone(), source.clone());
            db.insert(
                QueryKey::Parse(source),
                Arc::<[u8]>::from(&b"parse"[..]),
                [],
            );
            db.insert(
                QueryKey::ModuleHeaders(module.clone()),
                Arc::<[u8]>::from(&b"headers"[..]),
                [],
            );
            db.insert(
                QueryKey::ResolvedImports(module.clone()),
                Arc::<[u8]>::from(&b"imports"[..]),
                [],
            );
            db.insert(
                QueryKey::TypedModule(module.clone()),
                Arc::<[u8]>::from(&b"typed"[..]),
                [],
            );
        }
    }

    #[test]
    fn private_body_change_reanalyzes_only_changed_module() {
        let a = module("example.app", "a");
        let b = module("example.app", "b");
        let mut db = QueryDatabase::new();
        seed(&mut db, &[a.clone(), b.clone()]);
        db.record_module_import(b.clone(), a.clone());
        let report = db.invalidate_change(&a, ChangeImpact::PrivateImplementation);
        assert_eq!(report.parsed_sources, BTreeSet::from([source(&a)]));
        assert_eq!(report.analyzed_modules, BTreeSet::from([a]));
    }

    #[test]
    fn package_api_change_follows_exact_local_reverse_edges() {
        let a = module("example.app", "a");
        let b = module("example.app", "b");
        let unrelated = module("example.app", "unrelated");
        let mut db = QueryDatabase::new();
        seed(&mut db, &[a.clone(), b.clone(), unrelated.clone()]);
        db.record_module_import(b.clone(), a.clone());
        let report = db.invalidate_change(&a, ChangeImpact::PackageApi);
        assert_eq!(report.analyzed_modules, BTreeSet::from([a, b]));
        assert!(!report.analyzed_modules.contains(&unrelated));
    }

    #[test]
    fn public_api_change_reaches_dependency_consumers() {
        let library = module("example.library", "api");
        let consumer = module("example.app", "consumer");
        let downstream = module("example.app", "downstream");
        let mut db = QueryDatabase::new();
        seed(
            &mut db,
            &[library.clone(), consumer.clone(), downstream.clone()],
        );
        db.record_dependency_import(consumer.clone(), library.clone());
        db.record_module_import(downstream.clone(), consumer.clone());
        let report = db.invalidate_change(&library, ChangeImpact::PublicApi);
        assert_eq!(
            report.analyzed_modules,
            BTreeSet::from([library, consumer, downstream])
        );
    }

    #[test]
    fn dependency_imports_track_exact_target_modules() {
        let library_api = module("example.library", "api");
        let library_other = module("example.library", "other");
        let api_consumer = module("example.app", "api_consumer");
        let other_consumer = module("example.app", "other_consumer");
        let downstream = module("example.app", "downstream");
        let mut db = QueryDatabase::new();
        seed(
            &mut db,
            &[
                library_api.clone(),
                library_other.clone(),
                api_consumer.clone(),
                other_consumer.clone(),
                downstream.clone(),
            ],
        );
        db.record_dependency_import(api_consumer.clone(), library_api.clone());
        db.record_dependency_import(other_consumer.clone(), library_other.clone());
        db.record_module_import(downstream.clone(), api_consumer.clone());

        assert_eq!(
            db.resolved_dependency_imports(),
            vec![
                (api_consumer.clone(), library_api.clone()),
                (other_consumer.clone(), library_other),
            ]
        );
        let report = db.invalidate_change(&library_api, ChangeImpact::PublicApi);
        assert_eq!(
            report.analyzed_modules,
            BTreeSet::from([library_api, api_consumer, downstream])
        );
        assert!(!report.analyzed_modules.contains(&other_consumer));
    }

    #[test]
    fn real_private_body_update_preserves_header_and_import_queries() {
        let module = module("example.app", "main");
        let source = source(&module);
        let mut db = QueryDatabase::new();
        db.parse(source.clone(), "pub fn value() -> i32 { return 1; }")
            .unwrap();
        db.module_header(&source).unwrap();
        db.insert(
            QueryKey::ResolvedImports(module.clone()),
            Arc::<[u8]>::from(&b"imports"[..]),
            [QueryKey::ModuleHeaders(module.clone())],
        );
        db.insert(
            QueryKey::TypedModule(module.clone()),
            Arc::<[u8]>::from(&b"typed"[..]),
            [QueryKey::ResolvedImports(module.clone())],
        );
        db.insert(
            QueryKey::LinkedArtifact(module.package_id.clone()),
            Arc::<[u8]>::from(&b"artifact"[..]),
            [QueryKey::TypedModule(module.clone())],
        );
        let update = db
            .update_source(source, "pub fn value() -> i32 { return 2; }")
            .unwrap();
        assert_eq!(update.impact, ChangeImpact::PrivateImplementation);
        assert!(
            !update
                .invalidation
                .invalidated_queries
                .contains(&QueryKey::ModuleHeaders(module.clone()))
        );
        assert!(db.get(&QueryKey::ModuleHeaders(module.clone())).is_some());
        assert!(db.get(&QueryKey::ResolvedImports(module)).is_some());
    }

    #[test]
    fn changed_resolved_imports_invalidate_typed_and_linked_dependents() {
        let module = module("example.app", "main");
        let imports = QueryKey::ResolvedImports(module.clone());
        let typed = QueryKey::TypedModule(module.clone());
        let linked = QueryKey::LinkedArtifact(module.package_id.clone());
        let header = QueryKey::ModuleHeaders(module.clone());
        let mut db = QueryDatabase::new();
        db.insert(header.clone(), Arc::<[u8]>::from(&b"header"[..]), []);
        db.record_resolved_imports(
            module.clone(),
            Arc::<[u8]>::from(&b"first-target"[..]),
            [header.clone()],
        );
        db.insert(
            typed.clone(),
            Arc::<[u8]>::from(&b"typed"[..]),
            [imports.clone()],
        );
        db.insert(
            linked.clone(),
            Arc::<[u8]>::from(&b"linked"[..]),
            [typed.clone()],
        );

        db.begin_analysis();
        db.record_resolved_imports(module, Arc::<[u8]>::from(&b"second-target"[..]), [header]);
        let report = db.finish_analysis();

        assert!(report.invalidated_queries.contains(&imports));
        assert!(report.invalidated_queries.contains(&typed));
        assert!(report.invalidated_queries.contains(&linked));
        assert!(db.get(&typed).is_none());
        assert!(db.get(&linked).is_none());
        assert_eq!(
            db.cached_bytes(&imports).as_deref(),
            Some(&b"second-target"[..])
        );
    }

    #[test]
    fn equal_resolved_imports_refresh_dependencies_without_evicting_typed_ir() {
        let importer = module("example.app", "main");
        let old_target = module("example.app", "old_target");
        let new_target = module("example.app", "new_target");
        let old_header = QueryKey::ModuleHeaders(old_target);
        let new_header = QueryKey::ModuleHeaders(new_target);
        let imports = QueryKey::ResolvedImports(importer.clone());
        let typed = QueryKey::TypedModule(importer.clone());
        let mut db = QueryDatabase::new();
        db.insert(
            old_header.clone(),
            Arc::<[u8]>::from(&b"old-header"[..]),
            [],
        );
        db.insert(
            new_header.clone(),
            Arc::<[u8]>::from(&b"new-header"[..]),
            [],
        );
        db.record_resolved_imports(
            importer.clone(),
            Arc::<[u8]>::from(&b"same-target"[..]),
            [old_header.clone()],
        );
        db.insert(
            typed.clone(),
            Arc::<[u8]>::from(&b"typed"[..]),
            [imports.clone()],
        );

        db.record_resolved_imports(
            importer,
            Arc::<[u8]>::from(&b"same-target"[..]),
            [new_header.clone()],
        );
        let old_report = db.invalidate_keys([old_header]);
        assert!(!old_report.invalidated_queries.contains(&imports));
        assert!(db.get(&imports).is_some());
        assert!(db.get(&typed).is_some());

        let new_report = db.invalidate_keys([new_header]);
        assert!(new_report.invalidated_queries.contains(&imports));
        assert!(new_report.invalidated_queries.contains(&typed));
    }

    #[test]
    fn direct_source_delete_removes_all_module_edges_and_invalidates_consumers() {
        let target = module("example.app", "target");
        let local_consumer = module("example.app", "consumer");
        let downstream = module("example.app", "downstream");
        let external_consumer = module("example.other", "consumer");
        let mut db = QueryDatabase::new();
        seed(
            &mut db,
            &[
                target.clone(),
                local_consumer.clone(),
                downstream.clone(),
                external_consumer.clone(),
            ],
        );
        db.record_module_import(local_consumer.clone(), target.clone());
        db.record_module_import(downstream.clone(), local_consumer.clone());
        db.record_module_import(target.clone(), local_consumer.clone());
        db.record_dependency_import(external_consumer.clone(), target.clone());

        let update = db.update_source_set(
            &target.package_id,
            SourceSetChange::Delete {
                source: source(&target),
                module: target.clone(),
            },
        );

        assert!(
            db.resolved_module_imports()
                .iter()
                .all(|(importer, imported)| importer != &target && imported != &target)
        );
        assert!(
            db.resolved_dependency_imports()
                .iter()
                .all(|(importer, imported)| importer != &target && imported != &target)
        );
        for consumer in [local_consumer, downstream, external_consumer] {
            assert!(
                update
                    .invalidation
                    .invalidated_queries
                    .contains(&QueryKey::ResolvedImports(consumer.clone()))
            );
            assert!(
                update
                    .invalidation
                    .invalidated_queries
                    .contains(&QueryKey::TypedModule(consumer))
            );
        }
    }

    #[test]
    fn direct_source_rename_removes_old_module_edges_and_invalidates_consumers() {
        let old = module("example.app", "old");
        let new = module("example.app", "new");
        let consumer = module("example.app", "consumer");
        let dependency_consumer = module("example.other", "consumer");
        let mut db = QueryDatabase::new();
        seed(
            &mut db,
            &[old.clone(), consumer.clone(), dependency_consumer.clone()],
        );
        db.record_module_import(consumer.clone(), old.clone());
        db.record_module_import(old.clone(), consumer.clone());
        db.record_dependency_import(dependency_consumer.clone(), old.clone());

        let update = db.update_source_set(
            &old.package_id,
            SourceSetChange::Rename {
                old_source: source(&old),
                old_module: old.clone(),
                new_source: source(&new),
                new_module: new,
            },
        );

        assert!(
            db.resolved_module_imports()
                .iter()
                .all(|(importer, imported)| importer != &old && imported != &old)
        );
        assert!(
            db.resolved_dependency_imports()
                .iter()
                .all(|(importer, imported)| importer != &old && imported != &old)
        );
        for affected in [consumer, dependency_consumer] {
            assert!(
                update
                    .invalidation
                    .invalidated_queries
                    .contains(&QueryKey::ResolvedImports(affected.clone()))
            );
            assert!(
                update
                    .invalidation
                    .invalidated_queries
                    .contains(&QueryKey::TypedModule(affected))
            );
        }
    }

    #[test]
    fn replacing_registered_root_removes_stale_modules_but_keeps_shared_owners() {
        let root_a = PackageId::new("example.root_a").unwrap();
        let root_b = PackageId::new("example.root_b").unwrap();
        let shared = module("example.shared", "api");
        let removed = module("example.root_a", "old");
        let surviving_consumer = module("example.root_b", "consumer");
        let shared_source = source(&shared);
        let removed_source = source(&removed);
        let consumer_source = source(&surviving_consumer);
        let mut db = QueryDatabase::new();
        db.replace_registered_module_sources(
            root_a.clone(),
            BTreeMap::from([
                (shared.clone(), shared_source.clone()),
                (removed.clone(), removed_source.clone()),
            ]),
        );
        db.replace_registered_module_sources(
            root_b,
            BTreeMap::from([
                (shared.clone(), shared_source.clone()),
                (surviving_consumer.clone(), consumer_source),
            ]),
        );
        seed(&mut db, &[removed.clone(), surviving_consumer.clone()]);
        db.record_module_import(surviving_consumer.clone(), removed.clone());

        db.replace_registered_module_sources(root_a, BTreeMap::new());

        assert_eq!(db.module_sources.get(&shared), Some(&shared_source));
        assert!(!db.module_sources.contains_key(&removed));
        assert!(db.get(&QueryKey::Parse(removed_source)).is_none());
        assert!(db.get(&QueryKey::TypedModule(removed)).is_none());
        assert!(
            db.resolved_module_imports()
                .iter()
                .all(|(_, target)| target != &module("example.root_a", "old"))
        );
        assert!(db.get(&QueryKey::TypedModule(surviving_consumer)).is_none());
    }
}
