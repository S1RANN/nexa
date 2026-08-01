use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CompilationOptions, IrType,
    LockFile, ModuleKey, ModulePath, NormalizedPackagePath, PackageCatalog, PackageId,
    PackageLocation, PackageManifest, PackageSourceSet, QueryDatabase, QueryExecutionReport,
    QueryKey, ResolvedBuildInput, ResolvedImportTarget, SourceId, SourceKey, SourceRole,
    SourceSetBuilder, TypedDeclarationBody, TypedStatementIr, analyze_package,
    canonical_compilation_options, source_set_fingerprint,
};
use serde::Serialize;

const ROOT_PACKAGE: &str = "incremental.app";
const DEPENDENCY_PACKAGE: &str = "incremental.lib";
const SOURCE_ID: &str = "incremental-workspace";
const ROOT_DIRECTORY: &str = "workspace/app";
const DEPENDENCY_DIRECTORY: &str = "workspace/lib";
const CONTRACT_A: &[u8] = b"contract Host { host { fn version() -> i32; } }\n";
const CONTRACT_B: &[u8] =
    b"contract Host { host { fn version() -> i32; fn revision() -> i32; } }\n";
const CONTRACT_SOURCE_IDENTITY: &[u8] = b"standalone:contracts/incremental.nidl";

const LOCAL_A: &str = r"pub(package) fn package_value() -> i32 { return 1; }
pub fn public_value() -> i32 { return 1; }
";
const LOCAL_A_PRIVATE_B: &str = r"pub(package) fn package_value() -> i32 { return 2; }
pub fn public_value() -> i32 { return 1; }
";
const LOCAL_A_PACKAGE_API_B: &str = r"pub(package) fn package_value() -> i64 { return 1; }
pub fn public_value() -> i32 { return 1; }
";
const LOCAL_RENAMED: &str = r"pub(package) fn package_value() -> i32 { return 1; }
pub fn public_value() -> i32 { return 1; }
";
const LOCAL_B: &str = r"use package::app::a;
pub fn entry() -> i32 { return 0; }
";
const LOCAL_B_WITHOUT_IMPORT: &str = r"pub fn entry() -> i32 { return 0; }
";
const LOCAL_B_RENAMED_IMPORT: &str = r"use package::app::renamed;
pub fn entry() -> i32 { return 0; }
";
const LOCAL_UNRELATED: &str = r"pub fn value() -> i32 { return 7; }
";
const LOCAL_NEW: &str = r"pub fn value() -> i32 { return 9; }
";

const DEPENDENCY_API_A: &str = r"pub fn value() -> i32 { return 1; }
";
const DEPENDENCY_API_PRIVATE_B: &str = r"pub fn value() -> i32 { return 2; }
";
const DEPENDENCY_API_PUBLIC_B: &str = r"pub fn value() -> i64 { return 1; }
";
const CONSUMER_CALL: &str = r"use shared::api as shared;
pub fn entry() -> i32 {
    let ignored = shared::value();
    return 0;
}
";
const DEPENDENCY_OTHER: &str = r"pub fn value() -> i32 { return 5; }
";
const CONSUMER: &str = r"use shared::api as shared;
pub fn entry() -> i32 { return 0; }
";
const DOWNSTREAM: &str = r"use package::app::consumer;
pub fn value() -> i32 { return 0; }
";
const DEPENDENCY_UNRELATED: &str = r"use shared::other;
pub fn value() -> i32 { return 0; }
";

#[derive(Serialize)]
struct IncrementalReport {
    schema: u32,
    status: &'static str,
    scenarios: Vec<ScenarioEvidence>,
}

#[derive(Debug, Serialize)]
struct ScenarioEvidence {
    name: &'static str,
    expected: ExactSets,
    observed: ExactSets,
    hot_cache_hits: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct ExactSets {
    parsed: Vec<String>,
    analyzed: Vec<String>,
    reused: Vec<String>,
    invalidated: Vec<String>,
}

#[derive(Clone)]
struct Fixture {
    root_name: &'static str,
    root_entry: &'static str,
    root_sources: Vec<(&'static str, &'static str)>,
    dependency_version: Option<&'static str>,
    dependency_sources: Vec<(&'static str, &'static str)>,
    contract: &'static [u8],
}

impl Fixture {
    fn local(root_sources: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            root_name: "Incremental Application",
            root_entry: "app.b",
            root_sources,
            dependency_version: None,
            dependency_sources: Vec::new(),
            contract: CONTRACT_A,
        }
    }

    fn dependency(
        dependency_version: &'static str,
        dependency_sources: Vec<(&'static str, &'static str)>,
    ) -> Self {
        Self {
            root_name: "Incremental Application",
            root_entry: "app.consumer",
            root_sources: dependency_root_sources(),
            dependency_version: Some(dependency_version),
            dependency_sources,
            contract: CONTRACT_A,
        }
    }

    fn input(&self) -> ResolvedBuildInput {
        resolved_input(self)
    }
}

fn local_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/app/a.nexa", LOCAL_A),
        ("src/app/b.nexa", LOCAL_B),
        ("src/app/unrelated.nexa", LOCAL_UNRELATED),
    ]
}

fn dependency_root_sources() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/app/consumer.nexa", CONSUMER),
        ("src/app/downstream.nexa", DOWNSTREAM),
        ("src/app/unrelated.nexa", DEPENDENCY_UNRELATED),
    ]
}

fn dependency_sources(api: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("src/api.nexa", api), ("src/other.nexa", DEPENDENCY_OTHER)]
}

fn manifest(fixture: &Fixture) -> Arc<PackageManifest> {
    let dependency = fixture.dependency_version.map_or("", |_| {
        r#"

[dependencies]
shared = { path = "../lib" }
"#
    });
    Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{ROOT_PACKAGE}"
name = "{}"
version = "1.0.0"
source_root = "src"
entry = "{}"
activation = "programmatic"
{dependency}"#,
            fixture.root_name, fixture.root_entry
        ))
        .unwrap(),
    )
}

fn dependency_manifest(version: &str) -> Arc<PackageManifest> {
    Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "library"
id = "{DEPENDENCY_PACKAGE}"
name = "Incremental Library"
version = "{version}"
source_root = "src"
"#
        ))
        .unwrap(),
    )
}

fn source_set(
    package: &PackageId,
    sources: &[(&'static str, &'static str)],
) -> Arc<PackageSourceSet> {
    let mut builder = SourceSetBuilder::new(package.clone(), CompilationOptions::default().limits);
    for (path, source) in sources {
        builder
            .add(
                NormalizedPackagePath::new(*path).unwrap(),
                *source,
                SourceRole::Production,
            )
            .unwrap();
    }
    Arc::new(builder.build().unwrap())
}

#[allow(clippy::too_many_lines)]
fn resolved_input(fixture: &Fixture) -> ResolvedBuildInput {
    let source_id = SourceId::new(SOURCE_ID).unwrap();
    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let root_manifest = manifest(fixture);
    let root_sources = source_set(&root_manifest.id, &fixture.root_sources);
    let mut catalog = PackageCatalog::new();
    catalog
        .insert(PackageLocation {
            source_id: source_id.clone(),
            directory: root_directory.clone(),
            manifest: Arc::clone(&root_manifest),
        })
        .unwrap();

    let dependency = fixture.dependency_version.map(|version| {
        let manifest = dependency_manifest(version);
        let sources = source_set(&manifest.id, &fixture.dependency_sources);
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: NormalizedPackagePath::new(DEPENDENCY_DIRECTORY).unwrap(),
                manifest: Arc::clone(&manifest),
            })
            .unwrap();
        (manifest, sources)
    });
    let graph = Arc::new(
        catalog
            .resolve(
                &source_id,
                &root_directory,
                CompilationOptions::default().limits,
            )
            .unwrap(),
    );
    let dependency_manifests = dependency
        .iter()
        .map(|(manifest, _)| (manifest.id.clone(), Arc::clone(manifest)))
        .collect::<BTreeMap<_, _>>();
    let dependency_source_sets = dependency
        .iter()
        .map(|(manifest, sources)| (manifest.id.clone(), Arc::clone(sources)))
        .collect::<BTreeMap<_, _>>();
    let lock = (!graph.edges.is_empty()).then(|| Arc::new(LockFile::from_graph(&graph)));
    let canonical_lock_graph = lock
        .as_deref()
        .map_or_else(Vec::new, LockFile::canonical_bytes);
    let options = CompilationOptions::default();
    let fingerprint_input = BuildFingerprintInput {
        root_package: root_manifest.id.clone(),
        root_manifest: root_manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&root_sources),
        dependency_manifests: dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect(),
        dependency_source_sets: dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect(),
        host_contract: fixture.contract.to_vec(),
        host_contract_source: CONTRACT_SOURCE_IDENTITY.to_vec(),
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
        compiler_options: canonical_compilation_options(&options),
        canonical_lock_graph,
    };
    ResolvedBuildInput::new(
        root_manifest,
        root_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        lock,
        fixture.contract,
        CONTRACT_SOURCE_IDENTITY,
        fingerprint_input.host_required_entrypoints.clone(),
        options,
        fingerprint_input,
    )
    .unwrap()
}

fn module(package: &str, name: &str) -> ModuleKey {
    ModuleKey::new(
        PackageId::new(package).unwrap(),
        ModulePath::new(name).unwrap(),
    )
}

fn source(package: &str, path: &str) -> SourceKey {
    SourceKey::new(
        PackageId::new(package).unwrap(),
        NormalizedPackagePath::new(path).unwrap(),
    )
}

fn format_query(query: &QueryKey) -> String {
    match query {
        QueryKey::Parse(source) => format!("parse({}:{})", source.package_id, source.path),
        QueryKey::ModuleHeaders(module) => {
            format!("module_headers({}:{})", module.package_id, module.module)
        }
        QueryKey::ResolvedImports(module) => {
            format!("resolved_imports({}:{})", module.package_id, module.module)
        }
        QueryKey::TypedModule(module) => {
            format!("typed_module({}:{})", module.package_id, module.module)
        }
        QueryKey::PackagePublicApi(package) => format!("package_public_api({package})"),
        QueryKey::PackageStateSchema(package) => format!("package_state_schema({package})"),
        QueryKey::SourceSet(package) => format!("source_set({package})"),
        QueryKey::PackageManifest(package) => format!("package_manifest({package})"),
        QueryKey::DependencyGraph(package) => format!("dependency_graph({package})"),
        QueryKey::HostContract(package) => format!("host_contract({package})"),
        QueryKey::LinkedArtifact(package) => format!("linked_artifact({package})"),
    }
}

fn fixture_package(package: &PackageId) -> bool {
    matches!(package.as_str(), ROOT_PACKAGE | DEPENDENCY_PACKAGE)
}

fn fixture_source(source: &SourceKey) -> bool {
    fixture_package(&source.package_id)
}

fn fixture_module(module: &ModuleKey) -> bool {
    fixture_package(&module.package_id)
}

fn fixture_query(query: &QueryKey) -> bool {
    match query {
        QueryKey::Parse(source) => fixture_source(source),
        QueryKey::ModuleHeaders(module)
        | QueryKey::ResolvedImports(module)
        | QueryKey::TypedModule(module) => fixture_module(module),
        QueryKey::PackagePublicApi(package)
        | QueryKey::PackageStateSchema(package)
        | QueryKey::SourceSet(package)
        | QueryKey::PackageManifest(package)
        | QueryKey::DependencyGraph(package)
        | QueryKey::HostContract(package)
        | QueryKey::LinkedArtifact(package) => fixture_package(package),
    }
}

fn exact_from_reports<'a>(
    reports: impl IntoIterator<Item = &'a QueryExecutionReport>,
) -> ExactSets {
    let mut parsed = BTreeSet::new();
    let mut analyzed = BTreeSet::new();
    let mut reused = BTreeSet::new();
    let mut invalidated = BTreeSet::new();
    for report in reports {
        parsed.extend(
            report
                .parsed_sources
                .iter()
                .filter(|source| fixture_source(source))
                .map(|source| format!("{}:{}", source.package_id, source.path)),
        );
        analyzed.extend(
            report
                .analyzed_modules
                .iter()
                .filter(|module| fixture_module(module))
                .map(|module| format!("{}:{}", module.package_id, module.module)),
        );
        reused.extend(
            report
                .reused_queries
                .iter()
                .filter(|query| fixture_query(query))
                .map(format_query),
        );
        invalidated.extend(
            report
                .invalidated_queries
                .iter()
                .filter(|query| fixture_query(query))
                .map(format_query),
        );
    }
    ExactSets {
        parsed: parsed.into_iter().collect(),
        analyzed: analyzed.into_iter().collect(),
        reused: reused.into_iter().collect(),
        invalidated: invalidated.into_iter().collect(),
    }
}

fn analyze_valid(input: &ResolvedBuildInput, database: &mut QueryDatabase) -> AnalysisOutcome {
    let outcome = analyze_package(input, &AnalysisEnvironment::default(), database);
    assert!(
        outcome.ir.is_some(),
        "real analysis failed: {:#?}",
        outcome.diagnostics.diagnostics()
    );
    outcome
}

fn analyze_change(
    baseline: &ResolvedBuildInput,
    changed: &ResolvedBuildInput,
) -> (QueryDatabase, AnalysisOutcome, u64) {
    let mut database = QueryDatabase::new();
    let cold = analyze_valid(baseline, &mut database);
    assert!(
        cold.query_report.reused_queries.is_empty(),
        "cold analysis unexpectedly reported cache reuse"
    );
    let hits_before = database.stats().hits;
    let outcome = analyze_valid(changed, &mut database);
    let hot_cache_hits = database.stats().hits.saturating_sub(hits_before);
    assert!(
        hot_cache_hits > 0,
        "changed analysis did not hit the real cache"
    );
    assert!(
        !outcome.query_report.reused_queries.is_empty(),
        "changed analysis did not report real query reuse"
    );
    (database, outcome, hot_cache_hits)
}

fn root_modules(outcome: &AnalysisOutcome) -> BTreeSet<String> {
    outcome
        .ir
        .as_ref()
        .unwrap()
        .modules()
        .iter()
        .filter(|module| module.package_id.as_str() == ROOT_PACKAGE)
        .map(|module| module.module.to_string())
        .collect()
}

fn assert_module_absent(database: &mut QueryDatabase, module: &ModuleKey, source: &SourceKey) {
    assert!(database.get(&QueryKey::Parse(source.clone())).is_none());
    assert!(
        database
            .get(&QueryKey::ModuleHeaders(module.clone()))
            .is_none()
    );
    assert!(
        database
            .get(&QueryKey::ResolvedImports(module.clone()))
            .is_none()
    );
    assert!(
        database
            .get(&QueryKey::TypedModule(module.clone()))
            .is_none()
    );
    assert!(
        database
            .resolved_module_imports()
            .iter()
            .all(|(importer, target)| importer != module && target != module)
    );
    assert!(
        database
            .resolved_dependency_imports()
            .iter()
            .all(|(importer, target)| importer != module && target != module)
    );
}

fn consumer_call_type(outcome: &AnalysisOutcome) -> IrType {
    let ir = outcome.ir.as_ref().expect("analysis succeeds");
    let function = ir
        .modules()
        .iter()
        .find(|module| {
            module.package_id.as_str() == ROOT_PACKAGE && module.module.as_str() == "app.consumer"
        })
        .and_then(|module| {
            module
                .declarations
                .iter()
                .find_map(|declaration| match &declaration.body {
                    TypedDeclarationBody::Function(function) => Some(function),
                    TypedDeclarationBody::Const(_)
                    | TypedDeclarationBody::TypeLayout(_)
                    | TypedDeclarationBody::External => None,
                })
        })
        .expect("consumer entry function exists");
    let TypedStatementIr::Let {
        value: Some(value), ..
    } = &function.body.statements[0]
    else {
        panic!("consumer begins with one initialized binding");
    };
    value.ty.clone()
}

fn expected(
    parsed: &[&str],
    analyzed: &[&str],
    reused: &[&str],
    invalidated: &[&str],
) -> ExactSets {
    let sorted = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    ExactSets {
        parsed: sorted(parsed),
        analyzed: sorted(analyzed),
        reused: sorted(reused),
        invalidated: sorted(invalidated),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn m4_incremental_evidence() {
    let local = Fixture::local(local_sources());

    let mut private = local.clone();
    private.root_sources[0].1 = LOCAL_A_PRIVATE_B;
    let (_, private_outcome, private_hits) = analyze_change(&local.input(), &private.input());

    let mut package_api = local.clone();
    package_api.root_sources[0].1 = LOCAL_A_PACKAGE_API_B;
    let (_, package_api_outcome, package_api_hits) =
        analyze_change(&local.input(), &package_api.input());

    let dependency = Fixture::dependency("1.0.0", dependency_sources(DEPENDENCY_API_A));
    let mut dependency_api = dependency.clone();
    dependency_api.dependency_sources[0].1 = DEPENDENCY_API_PUBLIC_B;
    let (_, dependency_api_outcome, dependency_api_hits) =
        analyze_change(&dependency.input(), &dependency_api.input());

    let mut dependency_impl = dependency.clone();
    dependency_impl.dependency_sources[0].1 = DEPENDENCY_API_PRIVATE_B;
    let (_, dependency_impl_outcome, dependency_impl_hits) =
        analyze_change(&dependency.input(), &dependency_impl.input());

    let mut add = local.clone();
    add.root_sources.push(("src/app/added.nexa", LOCAL_NEW));
    let (_, add_outcome, add_hits) = analyze_change(&local.input(), &add.input());
    assert!(root_modules(&add_outcome).contains("app.added"));

    let mut delete = local.clone();
    delete.root_sources.remove(0);
    delete.root_sources[0].1 = LOCAL_B_WITHOUT_IMPORT;
    let (mut delete_database, delete_outcome, delete_hits) =
        analyze_change(&local.input(), &delete.input());
    assert_eq!(
        root_modules(&delete_outcome),
        BTreeSet::from(["app.b".to_owned(), "app.unrelated".to_owned()])
    );
    let old_a = module(ROOT_PACKAGE, "app.a");
    let old_a_source = source(ROOT_PACKAGE, "src/app/a.nexa");
    assert_module_absent(&mut delete_database, &old_a, &old_a_source);
    assert!(
        delete_outcome
            .resolved_import_edges
            .iter()
            .all(|edge| !matches!(
                &edge.target,
                ResolvedImportTarget::Module(target) if target == &old_a
            ))
    );

    let mut rename = local.clone();
    rename.root_sources[0] = ("src/app/renamed.nexa", LOCAL_RENAMED);
    rename.root_sources[1].1 = LOCAL_B_RENAMED_IMPORT;
    let (mut rename_database, rename_outcome, rename_hits) =
        analyze_change(&local.input(), &rename.input());
    assert_eq!(
        root_modules(&rename_outcome),
        BTreeSet::from([
            "app.b".to_owned(),
            "app.renamed".to_owned(),
            "app.unrelated".to_owned(),
        ])
    );
    assert_module_absent(&mut rename_database, &old_a, &old_a_source);
    let renamed = module(ROOT_PACKAGE, "app.renamed");
    assert!(rename_outcome.resolved_import_edges.iter().any(|edge| {
        edge.importer == module(ROOT_PACKAGE, "app.b")
            && matches!(
                &edge.target,
                ResolvedImportTarget::Module(target) if target == &renamed
            )
    }));
    assert!(
        rename_database
            .resolved_module_imports()
            .contains(&(module(ROOT_PACKAGE, "app.b"), renamed))
    );

    let mut aba_database = QueryDatabase::new();
    let aba_a = local.input();
    let aba_b = private.input();
    analyze_valid(&aba_a, &mut aba_database);
    let hits_before_b = aba_database.stats().hits;
    let outcome_after_b = analyze_valid(&aba_b, &mut aba_database);
    let hits_after_b = aba_database.stats().hits.saturating_sub(hits_before_b);
    let hits_before_a = aba_database.stats().hits;
    let outcome_after_a = analyze_valid(&aba_a, &mut aba_database);
    let hits_after_a = aba_database.stats().hits.saturating_sub(hits_before_a);
    assert!(hits_after_b > 0 && hits_after_a > 0);
    assert!(!outcome_after_b.query_report.reused_queries.is_empty());
    assert!(!outcome_after_a.query_report.reused_queries.is_empty());

    let mut manifest_change = local.clone();
    manifest_change.root_name = "Incremental Application Renamed";
    let (_, manifest_outcome, manifest_hits) =
        analyze_change(&local.input(), &manifest_change.input());

    let mut lock_change = dependency.clone();
    lock_change.dependency_version = Some("1.0.1");
    let (_, lock_outcome, lock_hits) = analyze_change(&dependency.input(), &lock_change.input());

    let mut contract_change = local.clone();
    contract_change.contract = CONTRACT_B;
    let (_, contract_outcome, contract_hits) =
        analyze_change(&local.input(), &contract_change.input());

    let observed = vec![
        (
            "private-body-change",
            exact_from_reports([&private_outcome.query_report]),
            private_hits,
        ),
        (
            "package-api-change",
            exact_from_reports([&package_api_outcome.query_report]),
            package_api_hits,
        ),
        (
            "dependency-api-change",
            exact_from_reports([&dependency_api_outcome.query_report]),
            dependency_api_hits,
        ),
        (
            "dependency-implementation-change",
            exact_from_reports([&dependency_impl_outcome.query_report]),
            dependency_impl_hits,
        ),
        (
            "source-add",
            exact_from_reports([&add_outcome.query_report]),
            add_hits,
        ),
        (
            "source-delete",
            exact_from_reports([&delete_outcome.query_report]),
            delete_hits,
        ),
        (
            "source-rename",
            exact_from_reports([&rename_outcome.query_report]),
            rename_hits,
        ),
        (
            "source-aba",
            exact_from_reports([&outcome_after_b.query_report, &outcome_after_a.query_report]),
            hits_after_b.saturating_add(hits_after_a),
        ),
        (
            "manifest-change",
            exact_from_reports([&manifest_outcome.query_report]),
            manifest_hits,
        ),
        (
            "lock-drift",
            exact_from_reports([&lock_outcome.query_report]),
            lock_hits,
        ),
        (
            "contract-change",
            exact_from_reports([&contract_outcome.query_report]),
            contract_hits,
        ),
    ];

    // These independent literals are filled from the normative incremental contract, never from
    // the observed report. Keeping the comparison here makes the machine report self-checking.
    let expected_sets = vec![
        expected(
            &["incremental.app:src/app/a.nexa"],
            &["incremental.app:app.a"],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "source_set(incremental.app)",
                "typed_module(incremental.app:app.b)",
                "typed_module(incremental.app:app.unrelated)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "typed_module(incremental.app:app.a)",
            ],
        ),
        expected(
            &["incremental.app:src/app/a.nexa"],
            &["incremental.app:app.a", "incremental.app:app.b"],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "source_set(incremental.app)",
                "typed_module(incremental.app:app.unrelated)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "parse(incremental.app:src/app/a.nexa)",
                "resolved_imports(incremental.app:app.a)",
                "typed_module(incremental.app:app.a)",
                "typed_module(incremental.app:app.b)",
            ],
        ),
        expected(
            &["incremental.lib:src/api.nexa"],
            &[
                "incremental.app:app.consumer",
                "incremental.app:app.downstream",
                "incremental.lib:api",
            ],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.consumer)",
                "module_headers(incremental.app:app.downstream)",
                "module_headers(incremental.app:app.unrelated)",
                "module_headers(incremental.lib:other)",
                "package_manifest(incremental.app)",
                "package_manifest(incremental.lib)",
                "parse(incremental.app:src/app/consumer.nexa)",
                "parse(incremental.app:src/app/downstream.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "parse(incremental.lib:src/other.nexa)",
                "source_set(incremental.app)",
                "source_set(incremental.lib)",
                "typed_module(incremental.app:app.unrelated)",
                "typed_module(incremental.lib:other)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "module_headers(incremental.lib:api)",
                "parse(incremental.lib:src/api.nexa)",
                "resolved_imports(incremental.lib:api)",
                "typed_module(incremental.app:app.consumer)",
                "typed_module(incremental.app:app.downstream)",
                "typed_module(incremental.lib:api)",
            ],
        ),
        expected(
            &["incremental.lib:src/api.nexa"],
            &["incremental.lib:api"],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.consumer)",
                "module_headers(incremental.app:app.downstream)",
                "module_headers(incremental.app:app.unrelated)",
                "module_headers(incremental.lib:api)",
                "module_headers(incremental.lib:other)",
                "package_manifest(incremental.app)",
                "package_manifest(incremental.lib)",
                "parse(incremental.app:src/app/consumer.nexa)",
                "parse(incremental.app:src/app/downstream.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "parse(incremental.lib:src/other.nexa)",
                "source_set(incremental.app)",
                "source_set(incremental.lib)",
                "typed_module(incremental.app:app.consumer)",
                "typed_module(incremental.app:app.downstream)",
                "typed_module(incremental.app:app.unrelated)",
                "typed_module(incremental.lib:other)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "parse(incremental.lib:src/api.nexa)",
                "typed_module(incremental.lib:api)",
            ],
        ),
        expected(
            &["incremental.app:src/app/added.nexa"],
            &[
                "incremental.app:app.a",
                "incremental.app:app.added",
                "incremental.app:app.b",
                "incremental.app:app.unrelated",
            ],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "source_set(incremental.app)",
            ],
        ),
        expected(
            &["incremental.app:src/app/b.nexa"],
            &["incremental.app:app.b", "incremental.app:app.unrelated"],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/unrelated.nexa)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "package_public_api(incremental.app)",
                "package_state_schema(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "parse(incremental.app:src/app/b.nexa)",
                "resolved_imports(incremental.app:app.a)",
                "resolved_imports(incremental.app:app.b)",
                "source_set(incremental.app)",
                "typed_module(incremental.app:app.a)",
                "typed_module(incremental.app:app.b)",
            ],
        ),
        expected(
            &[
                "incremental.app:src/app/b.nexa",
                "incremental.app:src/app/renamed.nexa",
            ],
            &[
                "incremental.app:app.b",
                "incremental.app:app.renamed",
                "incremental.app:app.unrelated",
            ],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/unrelated.nexa)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "package_public_api(incremental.app)",
                "package_state_schema(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "parse(incremental.app:src/app/b.nexa)",
                "resolved_imports(incremental.app:app.a)",
                "resolved_imports(incremental.app:app.b)",
                "source_set(incremental.app)",
                "typed_module(incremental.app:app.a)",
                "typed_module(incremental.app:app.b)",
            ],
        ),
        expected(
            &["incremental.app:src/app/a.nexa"],
            &["incremental.app:app.a"],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "source_set(incremental.app)",
                "typed_module(incremental.app:app.b)",
                "typed_module(incremental.app:app.unrelated)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "typed_module(incremental.app:app.a)",
            ],
        ),
        expected(
            &[],
            &[
                "incremental.app:app.a",
                "incremental.app:app.b",
                "incremental.app:app.unrelated",
            ],
            &[
                "dependency_graph(incremental.app)",
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "parse(incremental.app:src/app/a.nexa)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "source_set(incremental.app)",
            ],
            &[
                "linked_artifact(incremental.app)",
                "package_manifest(incremental.app)",
                "package_public_api(incremental.app)",
                "package_state_schema(incremental.app)",
                "resolved_imports(incremental.app:app.a)",
                "resolved_imports(incremental.app:app.b)",
                "resolved_imports(incremental.app:app.unrelated)",
                "typed_module(incremental.app:app.a)",
                "typed_module(incremental.app:app.b)",
                "typed_module(incremental.app:app.unrelated)",
            ],
        ),
        expected(
            &[],
            &[
                "incremental.app:app.consumer",
                "incremental.app:app.downstream",
                "incremental.app:app.unrelated",
                "incremental.lib:api",
                "incremental.lib:other",
            ],
            &[
                "host_contract(incremental.app)",
                "module_headers(incremental.app:app.consumer)",
                "module_headers(incremental.app:app.downstream)",
                "module_headers(incremental.app:app.unrelated)",
                "module_headers(incremental.lib:api)",
                "module_headers(incremental.lib:other)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/consumer.nexa)",
                "parse(incremental.app:src/app/downstream.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "parse(incremental.lib:src/api.nexa)",
                "parse(incremental.lib:src/other.nexa)",
                "source_set(incremental.app)",
                "source_set(incremental.lib)",
            ],
            &[
                "dependency_graph(incremental.app)",
                "linked_artifact(incremental.app)",
                "package_manifest(incremental.lib)",
                "package_public_api(incremental.app)",
                "package_state_schema(incremental.app)",
                "resolved_imports(incremental.app:app.consumer)",
                "resolved_imports(incremental.app:app.downstream)",
                "resolved_imports(incremental.app:app.unrelated)",
                "resolved_imports(incremental.lib:api)",
                "resolved_imports(incremental.lib:other)",
                "typed_module(incremental.app:app.consumer)",
                "typed_module(incremental.app:app.downstream)",
                "typed_module(incremental.app:app.unrelated)",
                "typed_module(incremental.lib:api)",
                "typed_module(incremental.lib:other)",
            ],
        ),
        expected(
            &[],
            &[
                "incremental.app:app.a",
                "incremental.app:app.b",
                "incremental.app:app.unrelated",
            ],
            &[
                "dependency_graph(incremental.app)",
                "module_headers(incremental.app:app.a)",
                "module_headers(incremental.app:app.b)",
                "module_headers(incremental.app:app.unrelated)",
                "package_manifest(incremental.app)",
                "parse(incremental.app:src/app/a.nexa)",
                "parse(incremental.app:src/app/b.nexa)",
                "parse(incremental.app:src/app/unrelated.nexa)",
                "source_set(incremental.app)",
            ],
            &[
                "host_contract(incremental.app)",
                "linked_artifact(incremental.app)",
                "package_public_api(incremental.app)",
                "package_state_schema(incremental.app)",
                "resolved_imports(incremental.app:app.a)",
                "resolved_imports(incremental.app:app.b)",
                "resolved_imports(incremental.app:app.unrelated)",
                "typed_module(incremental.app:app.a)",
                "typed_module(incremental.app:app.b)",
                "typed_module(incremental.app:app.unrelated)",
            ],
        ),
    ];
    let scenarios = observed
        .into_iter()
        .zip(expected_sets)
        .map(|((name, observed, hot_cache_hits), expected)| {
            assert_eq!(observed, expected, "{name} exact query evidence drifted");
            ScenarioEvidence {
                name,
                expected,
                observed,
                hot_cache_hits,
            }
        })
        .collect();

    let report = IncrementalReport {
        schema: 2,
        status: "PASS",
        scenarios,
    };
    if let Some(path) = std::env::var_os("NEXA_M4_INCREMENTAL_REPORT") {
        let path = PathBuf::from(path);
        fs::create_dir_all(path.parent().expect("report has parent")).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}

#[test]
fn deleted_dependency_module_cannot_resurrect_stale_consumer_typed_ir() {
    let mut initial = Fixture::dependency("1.0.0", dependency_sources(DEPENDENCY_API_A));
    initial.root_sources[0].1 = CONSUMER_CALL;
    let mut deleted = initial.clone();
    deleted.dependency_sources.remove(0);
    let mut restored = Fixture::dependency("1.0.0", dependency_sources(DEPENDENCY_API_PUBLIC_B));
    restored.root_sources[0].1 = CONSUMER_CALL;

    let mut database = QueryDatabase::new();
    let initial_outcome = analyze_valid(&initial.input(), &mut database);
    assert_eq!(consumer_call_type(&initial_outcome), IrType::I32);

    let deleted_outcome = analyze_package(
        &deleted.input(),
        &AnalysisEnvironment::default(),
        &mut database,
    );
    assert!(deleted_outcome.ir.is_none());
    assert!(
        deleted_outcome
            .query_report
            .invalidated_queries
            .contains(&QueryKey::TypedModule(module(ROOT_PACKAGE, "app.consumer"))),
        "removing an exact dependency target must purge its consumer Typed IR"
    );

    let restored_outcome = analyze_valid(&restored.input(), &mut database);
    assert_eq!(consumer_call_type(&restored_outcome), IrType::I64);
    assert!(
        !restored_outcome
            .query_report
            .reused_queries
            .contains(&QueryKey::TypedModule(module(ROOT_PACKAGE, "app.consumer"))),
        "restoring a changed dependency module must not resurrect pre-delete Typed IR"
    );

    let mut cold_database = QueryDatabase::new();
    let cold = analyze_valid(&restored.input(), &mut cold_database);
    assert_eq!(
        consumer_call_type(&restored_outcome),
        consumer_call_type(&cold)
    );
}
