use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CompilationLimits,
    CompilationOptions, DependencyGraphError, LockFile, ModuleGraph, ModuleGraphError, ModulePath,
    NormalizedPackagePath, PackageCatalog, PackageId, PackageKind, PackageLocation,
    PackageManifest, PackageSourceSet, QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph,
    ResolvedPackage, SourceId, SourceRole, SourceSetBuilder, SourceSetError, analyze_package,
    source_set_fingerprint,
};
use nexa_core::BuildFingerprint;

#[test]
fn module_cycle_reports_a_complete_canonical_chain() {
    let limits = CompilationLimits::default();
    let mut graph = ModuleGraph::new();
    let modules = ["z", "a", "m"].map(|name| ModulePath::new(name).unwrap());
    for module in modules.iter().rev() {
        graph.add_module(module.clone());
    }
    graph.add_import(&modules[0], &modules[1], limits).unwrap();
    graph.add_import(&modules[1], &modules[2], limits).unwrap();
    graph.add_import(&modules[2], &modules[0], limits).unwrap();
    let ModuleGraphError::Cycle(cycle) = graph.validate_acyclic().unwrap_err() else {
        panic!("expected module cycle");
    };
    assert_eq!(
        cycle
            .chain
            .iter()
            .map(ModulePath::as_str)
            .collect::<Vec<_>>(),
        ["a", "m", "z", "a"]
    );
    assert_eq!(cycle.chain.first(), cycle.chain.last());
    assert_eq!(cycle.chain.len(), modules.len() + 1);
    for edge in cycle.chain.windows(2) {
        assert!(
            graph
                .imports(&edge[0])
                .is_some_and(|targets| targets.contains(&edge[1])),
            "reported cycle contains a non-edge: {} -> {}",
            edge[0],
            edge[1]
        );
    }
    assert_eq!(cycle.to_string(), "a -> m -> z -> a");
}

#[test]
fn compilation_limits_enforce_exact_module_graph_boundaries() {
    let a = ModulePath::new("a").unwrap();
    let b = ModulePath::new("b").unwrap();
    let c = ModulePath::new("c").unwrap();
    let one_edge = CompilationLimits {
        imports_per_module: 1,
        module_edges: 1,
        ..CompilationLimits::default()
    };
    let mut modules = ModuleGraph::new();
    for module in [&a, &b, &c] {
        modules.add_module(module.clone());
    }
    modules.add_import(&a, &b, one_edge).unwrap();
    modules.add_import(&a, &b, one_edge).unwrap();
    assert!(matches!(
        modules.add_import(&a, &c, one_edge),
        Err(ModuleGraphError::TooManyImports(module)) if module == a
    ));
    assert_eq!(modules.edge_count(), 1);
    assert_eq!(modules.imports(&a), Some(&BTreeSet::from([b.clone()])));

    let mut global_edges = ModuleGraph::new();
    for module in [&a, &b, &c] {
        global_edges.add_module(module.clone());
    }
    let one_global_edge = CompilationLimits {
        imports_per_module: usize::MAX,
        module_edges: 1,
        ..CompilationLimits::default()
    };
    global_edges.add_import(&a, &c, one_global_edge).unwrap();
    assert!(matches!(
        global_edges.add_import(&b, &c, one_global_edge),
        Err(ModuleGraphError::TooManyEdges)
    ));
    assert_eq!(global_edges.edge_count(), 1);
    assert_eq!(global_edges.imports(&b), Some(&BTreeSet::new()));
}

#[test]
fn compilation_limits_enforce_exact_dependency_boundaries() {
    let source_id = SourceId::new("limit-workspace").unwrap();
    let root_directory = NormalizedPackagePath::new("packages/app").unwrap();
    let library_directory = NormalizedPackagePath::new("packages/library").unwrap();
    let root_manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.limit-app"
name = "Limit App"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
[dependencies]
library = { path = "../library" }
"#,
        )
        .unwrap(),
    );
    let library_manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "library"
id = "example.limit-library"
name = "Limit Library"
version = "1.0.0"
source_root = "src"
"#,
        )
        .unwrap(),
    );
    let mut catalog = PackageCatalog::new();
    for (directory, manifest) in [
        (root_directory.clone(), Arc::clone(&root_manifest)),
        (library_directory, Arc::clone(&library_manifest)),
    ] {
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory,
                manifest,
            })
            .unwrap();
    }
    let exact_dependency = CompilationLimits {
        dependency_packages: 1,
        ..CompilationLimits::default()
    };
    catalog
        .resolve(&source_id, &root_directory, exact_dependency)
        .unwrap();
    let no_dependencies = CompilationLimits {
        dependency_packages: 0,
        ..CompilationLimits::default()
    };
    assert!(matches!(
        catalog.resolve(&source_id, &root_directory, no_dependencies),
        Err(DependencyGraphError::TooManyPackages)
    ));

    let source_set = |package: PackageId, text: &str| {
        let mut builder = SourceSetBuilder::new(package, CompilationLimits::default());
        builder
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                text,
                SourceRole::Production,
            )
            .unwrap();
        builder.build().unwrap()
    };
    let root_sources = source_set(root_manifest.id.clone(), "const ROOT: i32 = 1;");
    let library_sources = source_set(library_manifest.id.clone(), "const LIBRARY: i32 = 1;");
    let closure_bytes = root_sources.production_bytes() + library_sources.production_bytes();
    let closure = [&root_sources, &library_sources];
    assert_eq!(
        PackageSourceSet::validate_dependency_closure(
            closure,
            CompilationLimits {
                dependency_closure_bytes: closure_bytes,
                ..CompilationLimits::default()
            }
        ),
        Ok(closure_bytes)
    );
    assert!(matches!(
        PackageSourceSet::validate_dependency_closure(
            closure,
            CompilationLimits {
                dependency_closure_bytes: closure_bytes - 1,
                ..CompilationLimits::default()
            }
        ),
        Err(SourceSetError::DependencyClosureTooLarge)
    ));
}

#[test]
fn manifest_and_standard_library_are_build_inputs() {
    let package = PackageId::new("example.package").unwrap();
    let mut sources = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
    sources
        .add(
            NormalizedPackagePath::new("src/main.nexa").unwrap(),
            "const VALUE: i32 = 1;",
            SourceRole::Production,
        )
        .unwrap();
    let source_set = source_set_fingerprint(&sources.build().unwrap());
    let standard_library = nexa_stdlib::standard_library();
    let baseline_input = BuildFingerprintInput {
        root_package: package,
        root_manifest: b"priority=1".to_vec(),
        root_source_set: source_set,
        dependency_manifests: BTreeMap::new(),
        dependency_source_sets: BTreeMap::new(),
        host_contract: b"contract Host;".to_vec(),
        contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
        host_contract_source: b"host-contract.nidl\0contract Host;".to_vec(),
        host_required_entrypoints: Vec::new(),
        repl_session_context: Vec::new(),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION,
        standard_library_version: standard_library.version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: nexa_analysis::canonical_compilation_options(
            &CompilationOptions::default(),
        ),
        canonical_lock_graph: Vec::new(),
    };
    let baseline = baseline_input.fingerprint();
    let mutations: [fn(&mut BuildFingerprintInput); 6] = [
        |input| input.root_manifest = b"priority=2".to_vec(),
        |input| input.standard_library_version = "9.9.9".into(),
        |input| input.standard_library_descriptor = b"changed-stdlib-descriptor".to_vec(),
        |input| input.runtime_semantics_version += 1,
        |input| input.opcode_cost_table_version += 1,
        |input| input.deterministic_math_backend = "changed-math-backend".into(),
    ];
    for mutate in mutations {
        let mut changed = baseline_input.clone();
        mutate(&mut changed);
        assert_ne!(changed.fingerprint(), baseline);
    }
}

#[test]
fn docs_change_source_and_build_but_not_analyzed_api_or_state_schema() {
    let (baseline, baseline_build) = analyze_documented_package(
        "/// Persistent counter docs.\n@state(version = 1) class Counter {\n    mut value: i32,\n}\n\n/// Returns the score.\npub fn score(value: i32) -> i32 {\n    return value;\n}\n",
    );
    let (changed, changed_build) = analyze_documented_package(
        "/// Reload-safe counter documentation.\n@state(version = 1) class Counter {\n    mut value: i32,\n}\n\n/// Returns the unchanged score value.\npub fn score(value: i32) -> i32 {\n    return value;\n}\n",
    );

    assert!(
        baseline.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        baseline.diagnostics.diagnostics()
    );
    assert!(
        changed.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        changed.diagnostics.diagnostics()
    );
    assert!(baseline.ir.is_some());
    assert!(changed.ir.is_some());
    assert_ne!(
        baseline.source_set_fingerprint,
        changed.source_set_fingerprint
    );
    assert_ne!(baseline_build, changed_build);
    assert_eq!(
        baseline.public_api_fingerprint,
        changed.public_api_fingerprint
    );
    assert_eq!(
        baseline.state_schema_fingerprint,
        changed.state_schema_fingerprint
    );
    assert_eq!(baseline.public_api_records, changed.public_api_records);
    assert_eq!(baseline.state_schema_records, changed.state_schema_records);
}

fn analyze_documented_package(declarations: &str) -> (AnalysisOutcome, BuildFingerprint) {
    let manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "example.documented"
name = "Documented"
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
            declarations,
            SourceRole::Production,
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
                source_id: SourceId::new("semantic-doc-test").unwrap(),
                directory: NormalizedPackagePath::new("packages/documented").unwrap(),
                kind: PackageKind::Application,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let compilation_options = CompilationOptions::default();
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
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION,
        standard_library_version: nexa_stdlib::standard_library().version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: nexa_analysis::canonical_compilation_options(&compilation_options),
        canonical_lock_graph: Vec::new(),
    };
    let input = ResolvedBuildInput::new(
        Arc::clone(&manifest),
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        compilation_options,
        fingerprint,
    )
    .unwrap();
    let build_fingerprint = input.build_fingerprint;
    let mut database = QueryDatabase::new();
    let outcome = analyze_package(&input, &AnalysisEnvironment::default(), &mut database);
    (outcome, build_fingerprint)
}

#[test]
fn canonical_lock_contains_no_absolute_path() {
    let package = PackageId::new("example.package").unwrap();
    let graph = nexa_analysis::ResolvedDependencyGraph {
        root: package.clone(),
        packages: BTreeMap::from([(
            package.clone(),
            nexa_analysis::ResolvedPackage {
                id: package,
                version: semver::Version::new(1, 0, 0),
                source_id: nexa_analysis::SourceId::new("local").unwrap(),
                directory: NormalizedPackagePath::new("packages/example").unwrap(),
                kind: nexa_analysis::PackageKind::Application,
            },
        )]),
        edges: BTreeSet::new(),
    };
    let rendered = LockFile::from_graph(&graph).render();
    assert!(!rendered.contains("/Users/"));
    assert_eq!(LockFile::parse(&rendered).unwrap().render(), rendered);
}
