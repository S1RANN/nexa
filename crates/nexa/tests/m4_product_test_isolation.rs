use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa::{
    CandidateIdentity, HostContractInput, PackageBuildError, PackageBuildSession,
    PackageTestOptions, SourceIdentity, TestError, TestStatus,
    canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input_with_contract,
};
use nexa_analysis::{
    CompilationLimits, NormalizedPackagePath, PackageManifest, QueryKey, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, ResolvedTestInput, SourceId, SourceRole,
    SourceSetBuilder,
};

const EMPTY_HOST_NIDL: &str = "contract Empty {}";
const EMPTY_HOST_URI: &str = "nidl://tests/m4-product-test-isolation/empty.nidl";

fn host_contract<'a>(
    contract: &'a nexa_contract::ValidatedContract,
    uri: &str,
    source: &str,
) -> HostContractInput<'a> {
    HostContractInput::with_source(contract, SourceIdentity::standalone(uri), source)
        .expect("exact test NIDL source")
}

fn resolved_product(
    manifest: PackageManifest,
    contract: &HostContractInput<'_>,
) -> Arc<ResolvedBuildInput> {
    resolved_product_with_source(manifest, contract, "pub fn value() -> i32 { return 7; }\n")
}

fn resolved_product_with_source(
    manifest: PackageManifest,
    contract: &HostContractInput<'_>,
    source: impl Into<Arc<str>>,
) -> Arc<ResolvedBuildInput> {
    resolved_product_with_sources(manifest, contract, [("src/main.nexa", source.into())])
}

fn resolved_product_with_sources(
    manifest: PackageManifest,
    contract: &HostContractInput<'_>,
    units: impl IntoIterator<Item = (&'static str, Arc<str>)>,
) -> Arc<ResolvedBuildInput> {
    let mut sources = SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    for (path, source) in units {
        sources
            .add(
                NormalizedPackagePath::new(path).unwrap(),
                source,
                SourceRole::Production,
            )
            .unwrap();
    }
    let sources = sources.build().unwrap();
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("m4-product-test-isolation").unwrap(),
                directory: NormalizedPackagePath::new("packages/example").unwrap(),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint_input = canonical_package_build_fingerprint_input_with_contract(
        &manifest,
        &sources,
        &BTreeMap::new(),
        &BTreeMap::new(),
        contract,
        None,
    );
    let canonical_host_contract = fingerprint_input.host_contract.clone();
    let host_contract_source_identity = canonical_host_contract_source_identity(contract);
    Arc::new(
        ResolvedBuildInput::new(
            Arc::new(manifest),
            Arc::new(sources),
            BTreeMap::new(),
            BTreeMap::new(),
            graph,
            None,
            Arc::<[u8]>::from(canonical_host_contract),
            host_contract_source_identity,
            fingerprint_input.host_required_entrypoints.clone(),
            nexa_analysis::CompilationOptions::default(),
            fingerprint_input,
        )
        .unwrap(),
    )
}

fn test_input(product: &Arc<ResolvedBuildInput>, result: bool) -> ResolvedTestInput {
    test_input_with_source(
        product,
        format!("@test fn product_is_valid() -> bool {{ return {result}; }}\n"),
    )
}

fn test_input_with_source(
    product: &Arc<ResolvedBuildInput>,
    source: impl Into<Arc<str>>,
) -> ResolvedTestInput {
    let mut sources = SourceSetBuilder::new(
        product.root_manifest.id.clone(),
        CompilationLimits::default(),
    );
    sources
        .add(
            NormalizedPackagePath::new("tests/checks.nexa").unwrap(),
            source,
            SourceRole::Test,
        )
        .unwrap();
    ResolvedTestInput::new(Arc::clone(product), Arc::new(sources.build().unwrap())).unwrap()
}

fn identity(product: &ResolvedBuildInput) -> CandidateIdentity {
    CandidateIdentity::new(
        product.root_manifest.id.clone(),
        1,
        product.build_fingerprint,
    )
    .unwrap()
}

fn application_manifest() -> PackageManifest {
    PackageManifest::parse(
        r#"
schema = 2
kind = "application"
id = "example.app"
name = "Example"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
    )
    .unwrap()
}

#[test]
fn test_only_changes_do_not_contaminate_the_product_build() {
    let manifest = application_manifest();
    let parsed_contract = nexa_contract::parse_contract(EMPTY_HOST_NIDL).unwrap();
    let contract = host_contract(&parsed_contract, EMPTY_HOST_URI, EMPTY_HOST_NIDL);
    let product = resolved_product(manifest, &contract);
    let tests_returning_true = test_input(&product, true);
    let tests_returning_false = test_input(&product, false);

    let mut session = PackageBuildSession::new();
    let product_before = session
        .compile_package_with_contract(&product, &contract, identity(&product))
        .unwrap();
    let true_artifact = session
        .compile_package_tests_with_contract(&tests_returning_true, &contract, identity(&product))
        .unwrap();
    let false_artifact = session
        .compile_package_tests_with_contract(&tests_returning_false, &contract, identity(&product))
        .unwrap();
    let product_after = session
        .compile_package_with_contract(&product, &contract, identity(&product))
        .unwrap();

    assert_eq!(
        product_before.encode_module(),
        product_after.encode_module()
    );
    assert_eq!(
        product_before.source_set_fingerprint,
        product_after.source_set_fingerprint
    );
    assert_eq!(
        product_before.public_api_fingerprint,
        product_after.public_api_fingerprint
    );
    assert_eq!(
        product_before.state_schema_fingerprint,
        product_after.state_schema_fingerprint
    );
    assert_eq!(
        product_before.build_fingerprint,
        product_after.build_fingerprint
    );
    assert_eq!(
        product_before.source_files.source_paths(),
        product_after.source_files.source_paths()
    );
    assert!(product_before.source_files.files().iter().all(|source| {
        source
            .key()
            .is_none_or(|key| !key.path.as_str().starts_with("tests/"))
    }));

    let true_run = true_artifact.run(PackageTestOptions::default()).unwrap();
    let false_run = false_artifact.run(PackageTestOptions::default()).unwrap();
    assert_eq!(true_run.results[0].status, TestStatus::Pass);
    assert_eq!(false_run.results[0].status, TestStatus::Fail);
}

#[test]
fn product_check_discards_prior_test_modules_edges_and_invalidation_evidence() {
    let parsed_contract = nexa_contract::parse_contract(EMPTY_HOST_NIDL).unwrap();
    let contract = host_contract(&parsed_contract, EMPTY_HOST_URI, EMPTY_HOST_NIDL);
    let product = resolved_product(application_manifest(), &contract);
    let tests = test_input_with_source(
        &product,
        "use package::main as product;\n@test fn succeeds() -> bool { return true; }\n",
    );
    let mut session = PackageBuildSession::new();
    session
        .compile_package_tests_with_contract(&tests, &contract, identity(&product))
        .expect("test analysis records a test module and its product import");

    let report = session
        .check_package_with_contract(&product, &contract)
        .expect("the same session returns to an exact product analysis");
    let cold = PackageBuildSession::new()
        .check_package_with_contract(&product, &contract)
        .expect("cold product check");

    assert_eq!(report.modules, cold.modules);
    assert_eq!(report.symbols, cold.symbols);
    assert_eq!(report.resolved_references, cold.resolved_references);
    assert_eq!(report.resolved_module_imports, 0);
    assert_eq!(report.resolved_dependency_imports, 0);
    assert_eq!(report.source_set_fingerprint, cold.source_set_fingerprint);
    assert_eq!(report.public_api_fingerprint, cold.public_api_fingerprint);
    assert_eq!(
        report.state_schema_fingerprint,
        cold.state_schema_fingerprint
    );
    assert!(
        report
            .query_report
            .parsed_sources
            .iter()
            .all(|source| !source.path.as_str().starts_with("tests/"))
    );
    assert!(
        report
            .query_report
            .analyzed_modules
            .iter()
            .all(|module| !module.module.as_str().starts_with("test."))
    );
    assert!(
        report
            .query_report
            .reused_queries
            .iter()
            .chain(&report.query_report.invalidated_queries)
            .all(|key| !query_key_belongs_to_tests(key)),
        "{:#?}",
        report.query_report
    );
}

#[test]
fn canonical_compiler_accepts_an_imported_namespace_value_field_method_chain() {
    let parsed_contract = nexa_contract::parse_contract(EMPTY_HOST_NIDL).unwrap();
    let contract = host_contract(&parsed_contract, EMPTY_HOST_URI, EMPTY_HOST_NIDL);
    let product = resolved_product_with_sources(
        application_manifest(),
        &contract,
        [
            (
                "src/main.nexa",
                Arc::from(
                    "use package::util as u;\npub fn value() -> i32 { return u::VALUE.text.len(); }\n",
                ),
            ),
            (
                "src/util.nexa",
                Arc::from(
                    "pub(package) struct Record { text: string, }\npub(package) const VALUE: Record = Record { text: \"compiled\", };\n",
                ),
            ),
        ],
    );
    let artifact = PackageBuildSession::new()
        .compile_package_with_contract(&product, &contract, identity(&product))
        .expect("analysis, typed codegen, and verifier accept the qualified receiver");
    assert!(!artifact.encode_module().is_empty());
}

fn query_key_belongs_to_tests(key: &QueryKey) -> bool {
    match key {
        QueryKey::Parse(source) => source.path.as_str().starts_with("tests/"),
        QueryKey::ModuleHeaders(module)
        | QueryKey::ResolvedImports(module)
        | QueryKey::TypedModule(module) => module.module.as_str().starts_with("test."),
        QueryKey::PackagePublicApi(_)
        | QueryKey::PackageStateSchema(_)
        | QueryKey::SourceSet(_)
        | QueryKey::PackageManifest(_)
        | QueryKey::DependencyGraph(_)
        | QueryKey::HostContract(_)
        | QueryKey::LinkedArtifact(_) => false,
    }
}

#[test]
fn canonical_test_build_traps_inside_a_fresh_realm_with_source_evidence() {
    let parsed_contract = nexa_contract::parse_contract(EMPTY_HOST_NIDL).unwrap();
    let contract = host_contract(&parsed_contract, EMPTY_HOST_URI, EMPTY_HOST_NIDL);
    let product = resolved_product(application_manifest(), &contract);
    let tests = test_input_with_source(
        &product,
        r#"use std::debug;

@test
fn a_explicit_trap() -> bool {
    return debug::trap("canonical test trap");
}

@test
fn b_still_runs_after_trap() -> bool {
    return true;
}
"#,
    );

    let artifact = PackageBuildSession::new()
        .compile_package_tests_with_contract(&tests, &contract, identity(&product))
        .expect("canonical analysis, typed compilation, and verification must succeed");
    assert_eq!(artifact.test_count(), 2);
    assert!(artifact.source_files().files().iter().any(|source| {
        source
            .key()
            .is_some_and(|key| key.path.as_str() == "tests/checks.nexa")
    }));

    let run = artifact
        .run(PackageTestOptions::default())
        .expect("the verified test artifact must enter a fresh Realm");
    assert_eq!(run.summary.passed, 1);
    assert_eq!(run.summary.errors, 1);
    let result = &run.results[0];
    assert_eq!(result.name, "a_explicit_trap");
    assert_eq!(result.status, TestStatus::Error);
    assert_eq!(
        result.error,
        Some(TestError::Trap {
            message: "canonical test trap".into(),
        })
    );
    assert_eq!(result.span.source, "example.app:tests/checks.nexa");
    assert_eq!(result.stack.len(), 1);
    assert_eq!(
        result.stack[0].span.as_ref().unwrap().source,
        "example.app:tests/checks.nexa"
    );
    assert!(result.instructions > 0);
    assert!(result.fuel > 0);
    assert_eq!(run.results[1].name, "b_still_runs_after_trap");
    assert_eq!(run.results[1].status, TestStatus::Pass);
    assert!(run.results[1].error.is_none());
}

#[test]
fn canonical_test_build_reports_realm_fuel_exhaustion_with_exact_stack() {
    let parsed_contract = nexa_contract::parse_contract(EMPTY_HOST_NIDL).unwrap();
    let contract = host_contract(&parsed_contract, EMPTY_HOST_URI, EMPTY_HOST_NIDL);
    let product = resolved_product(application_manifest(), &contract);
    let tests = test_input_with_source(
        &product,
        r"@test
fn exhausts_fuel() -> bool {
    for step in 0..64 {
        step + 1;
    }
    return true;
}
",
    );

    let artifact = PackageBuildSession::new()
        .compile_package_tests_with_contract(&tests, &contract, identity(&product))
        .expect("canonical analysis, typed compilation, and verification must succeed");
    let run = artifact
        .run(PackageTestOptions { fuel_limit: 1 })
        .expect("the verified test artifact must enter a fresh Realm");
    assert_eq!(run.summary.errors, 1);
    let result = &run.results[0];
    assert_eq!(result.status, TestStatus::Error);
    assert_eq!(result.error, Some(TestError::FuelExhaustion));
    assert_eq!(result.stack.len(), 1);
    assert_eq!(result.stack[0].function, "exhausts_fuel");
    assert_eq!(
        result.stack[0].span.as_ref().unwrap().source,
        "example.app:tests/checks.nexa"
    );
    assert!(result.instructions > 0);
    assert!(result.fuel > 0);
}

#[test]
fn canonical_analysis_rejects_an_indirect_host_call_before_test_codegen() {
    let contract_source = r"
contract TestHost {
    host {
        fn clock() -> i32;
    }
}
";
    let parsed_contract = nexa_contract::parse_contract(contract_source).unwrap();
    let contract = host_contract(
        &parsed_contract,
        "nidl://tests/m4-product-test-isolation/test-host.nidl",
        contract_source,
    );
    let product = resolved_product_with_source(
        application_manifest(),
        &contract,
        r"use host::test_host as host;

pub(package) fn host_value() -> i32 {
    return host::clock();
}
",
    );
    let tests = test_input_with_source(
        &product,
        r"use package::main as app;

@test
fn indirect_host() -> bool {
    return app::host_value() == 0;
}
",
    );

    let error = PackageBuildSession::new()
        .compile_package_tests_with_contract(&tests, &contract, identity(&product))
        .expect_err("analysis must reject indirect Host reachability");
    let PackageBuildError::AnalysisFailed(diagnostics) = error else {
        panic!("expected analysis failure, got {error}");
    };
    let diagnostic = diagnostics
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.code.as_str() == "NX2730")
        .expect("indirect Host rejection must use NX2730");
    assert!(
        diagnostic
            .message
            .contains("@test reaches forbidden Host operation through indirect_host -> host_value"),
        "{}",
        diagnostic.message
    );
    let primary = diagnostic
        .primary_label()
        .expect("primary test source span");
    assert_eq!(primary.source.to_string(), "example.app:tests/checks.nexa");
}
