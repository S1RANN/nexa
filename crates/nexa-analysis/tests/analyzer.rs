use std::collections::BTreeMap;
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, BuiltinOperationIr,
    CompilationLimits, CompilationOptions, ExternalFieldSurface, ExternalSourceOrigin,
    ExternalTypeKind, ExternalTypeSurface, ExternalVariantSurface, HostAsyncResultSurface,
    HostContractSurface, HostFunctionMode, HostFunctionSurface, IrAbandonPolicy, IrCancelPolicy,
    IrEffect, IrType, ModuleKey, ModulePath, NormalizedPackagePath, PackageCatalog,
    PackageLocation, PackageManifest, PackageSourceSet, QueryDatabase, QueryKey,
    ResolvedBuildInput, ResolvedImportTarget, ResolvedTestInput, SourceId, SourceRole,
    SourceSetBuilder, StaticModuleSurface, SurfaceType, TypedDeclarationBody, TypedExpressionKind,
    TypedStatementIr, analyze_package, analyze_package_tests, source_set_fingerprint,
};
use nexa_core::{CanonicalSymbolIdentity, StableId, SymbolKind};
use nexa_diagnostics::{ByteRange, Diagnostic, ErrorCode, LabelStyle, SourceIdentity};

const ROOT_PACKAGE: &str = "example.app";
const DEPENDENCY_PACKAGE: &str = "example.lib";
const ROOT_DIRECTORY: &str = "workspace/app";
const DEPENDENCY_DIRECTORY: &str = "workspace/lib";
const SOURCE_ID: &str = "workspace";
const HOST_CONTRACT: &[u8] = b"test-host-contract-v1";

#[derive(Clone)]
struct FixturePackage {
    manifest: Arc<PackageManifest>,
    sources: Arc<PackageSourceSet>,
    directory: NormalizedPackagePath,
}

fn application_manifest(dependency: bool) -> Arc<PackageManifest> {
    application_manifest_with_capabilities(dependency, &[])
}

fn application_manifest_with_capabilities(
    dependency: bool,
    capabilities: &[&str],
) -> Arc<PackageManifest> {
    let dependency = dependency.then_some(
        r#"

[dependencies]
shared_api = { path = "../lib" }
"#,
    );
    let capabilities = capabilities
        .iter()
        .map(|capability| format!("\"{capability}\""))
        .collect::<Vec<_>>()
        .join(", ");
    Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{ROOT_PACKAGE}"
name = "Analyzer Fixture"
version = "1.0.0"
source_root = "src"
entry = "app.main"
activation = "programmatic"
capabilities = [{capabilities}]
{}"#,
            dependency.unwrap_or_default()
        ))
        .expect("valid application fixture"),
    )
}

fn library_manifest() -> Arc<PackageManifest> {
    Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "library"
id = "{DEPENDENCY_PACKAGE}"
name = "Analyzer Library"
version = "1.0.0"
source_root = "src"
"#
        ))
        .expect("valid library fixture"),
    )
}

fn source_set(package: &str, units: &[(&str, &str, SourceRole)]) -> Arc<PackageSourceSet> {
    let package = nexa_analysis::PackageId::new(package).expect("valid package id");
    let mut builder = SourceSetBuilder::new(package, CompilationLimits::default());
    for (path, source, role) in units {
        builder
            .add(
                NormalizedPackagePath::new(path).expect("normalized source path"),
                *source,
                *role,
            )
            .expect("valid source fixture");
    }
    Arc::new(builder.build().expect("valid source set"))
}

fn root_fixture(units: &[(&str, &str, SourceRole)], dependency: bool) -> FixturePackage {
    FixturePackage {
        manifest: application_manifest(dependency),
        sources: source_set(ROOT_PACKAGE, units),
        directory: NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap(),
    }
}

fn dependency_fixture(units: &[(&str, &str, SourceRole)]) -> FixturePackage {
    FixturePackage {
        manifest: library_manifest(),
        sources: source_set(DEPENDENCY_PACKAGE, units),
        directory: NormalizedPackagePath::new(DEPENDENCY_DIRECTORY).unwrap(),
    }
}

fn resolved_input(root: FixturePackage, dependencies: &[FixturePackage]) -> ResolvedBuildInput {
    resolved_input_with_options(root, dependencies, CompilationOptions::default())
}

fn resolved_input_with_options(
    root: FixturePackage,
    dependencies: &[FixturePackage],
    compilation_options: CompilationOptions,
) -> ResolvedBuildInput {
    resolved_input_with_contract_and_options(root, dependencies, HOST_CONTRACT, compilation_options)
}

fn resolved_input_with_contract(
    root: FixturePackage,
    dependencies: &[FixturePackage],
    host_contract: &[u8],
) -> ResolvedBuildInput {
    resolved_input_with_contract_and_options(
        root,
        dependencies,
        host_contract,
        CompilationOptions::default(),
    )
}

fn resolved_input_with_contract_and_options(
    root: FixturePackage,
    dependencies: &[FixturePackage],
    host_contract: &[u8],
    compilation_options: CompilationOptions,
) -> ResolvedBuildInput {
    let host_contract_source = [b"fixture-host.nidl".as_slice(), b"\0", host_contract].concat();
    let source_id = SourceId::new(SOURCE_ID).unwrap();
    let mut catalog = PackageCatalog::new();
    catalog
        .insert(PackageLocation {
            source_id: source_id.clone(),
            directory: root.directory.clone(),
            manifest: Arc::clone(&root.manifest),
        })
        .unwrap();
    for dependency in dependencies {
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: dependency.directory.clone(),
                manifest: Arc::clone(&dependency.manifest),
            })
            .unwrap();
    }
    let graph = Arc::new(
        catalog
            .resolve(&source_id, &root.directory, compilation_options.limits)
            .expect("resolved fixture graph"),
    );
    let dependency_manifests = dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.manifest.id.clone(),
                Arc::clone(&dependency.manifest),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let dependency_source_sets = dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.manifest.id.clone(),
                Arc::clone(&dependency.sources),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let lock =
        (!graph.edges.is_empty()).then(|| Arc::new(nexa_analysis::LockFile::from_graph(&graph)));
    let canonical_lock_graph = lock
        .as_deref()
        .map_or_else(Vec::new, nexa_analysis::LockFile::canonical_bytes);
    let fingerprint_input = BuildFingerprintInput {
        root_package: root.manifest.id.clone(),
        root_manifest: root.manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&root.sources),
        dependency_manifests: dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect(),
        dependency_source_sets: dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect(),
        host_contract: host_contract.to_vec(),
        contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
        host_contract_source: host_contract_source.clone(),
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
        canonical_lock_graph,
    };
    ResolvedBuildInput::new(
        root.manifest,
        root.sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        lock,
        host_contract,
        host_contract_source,
        fingerprint_input.host_required_entrypoints.clone(),
        compilation_options,
        fingerprint_input,
    )
    .expect("valid resolved build input")
}

fn analyze_deterministically(
    input: &ResolvedBuildInput,
    environment: &AnalysisEnvironment,
) -> AnalysisOutcome {
    let mut first_db = QueryDatabase::new();
    let first = analyze_package(input, environment, &mut first_db);
    let mut second_db = QueryDatabase::new();
    let second = analyze_package(input, environment, &mut second_db);
    assert_eq!(
        first.diagnostics.diagnostics(),
        second.diagnostics.diagnostics(),
        "analysis diagnostics must be deterministic"
    );
    first
}

fn analyze_main_source(source: &str) -> AnalysisOutcome {
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", source, SourceRole::Production)],
            false,
        ),
        &[],
    );
    analyze_deterministically(&input, &AnalysisEnvironment::default())
}

fn analyze_tests_deterministically(
    input: &ResolvedTestInput,
    environment: &AnalysisEnvironment,
) -> AnalysisOutcome {
    let mut first_db = QueryDatabase::new();
    let first = analyze_package_tests(input, environment, &mut first_db);
    let mut second_db = QueryDatabase::new();
    let second = analyze_package_tests(input, environment, &mut second_db);
    assert_eq!(
        first.diagnostics.diagnostics(),
        second.diagnostics.diagnostics(),
        "test analysis diagnostics must be deterministic"
    );
    first
}

fn diagnostics_with_code(outcome: &AnalysisOutcome, code: ErrorCode) -> Vec<&Diagnostic> {
    outcome
        .diagnostics
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.code == code)
        .collect()
}

fn one_diagnostic(outcome: &AnalysisOutcome, code: ErrorCode) -> &Diagnostic {
    let diagnostics = diagnostics_with_code(outcome, code);
    assert_eq!(
        diagnostics.len(),
        1,
        "expected exactly one {code}, got {diagnostics:#?}"
    );
    diagnostics[0]
}

fn identity(package: &str, path: &str) -> SourceIdentity {
    SourceIdentity::package(package, path)
}

fn range(source: &str, needle: &str) -> ByteRange {
    let start = source.find(needle).expect("needle is present");
    ByteRange::new(
        u32::try_from(start).unwrap(),
        u32::try_from(start + needle.len()).unwrap(),
    )
}

fn last_range(source: &str, needle: &str) -> ByteRange {
    let start = source.rfind(needle).expect("needle is present");
    ByteRange::new(
        u32::try_from(start).unwrap(),
        u32::try_from(start + needle.len()).unwrap(),
    )
}

fn nth_range(source: &str, needle: &str, occurrence: usize) -> ByteRange {
    let start = source
        .match_indices(needle)
        .nth(occurrence.saturating_sub(1))
        .map(|(start, _)| start)
        .expect("needle occurrence is present");
    ByteRange::new(
        u32::try_from(start).unwrap(),
        u32::try_from(start + needle.len()).unwrap(),
    )
}

fn assert_primary(diagnostic: &Diagnostic, source: &SourceIdentity, range: ByteRange) {
    let primary = diagnostic.primary_label().expect("primary source label");
    assert_eq!(&primary.source, source);
    assert_eq!(primary.range, range);
}

fn host_environment() -> AnalysisEnvironment {
    AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "FixtureHost".into(),
            contract_stable_id: StableId::from_name("FixtureHost"),
            types: Vec::new(),
            functions: vec![HostFunctionSurface {
                name: "clock".into(),
                parameters: Vec::new(),
                result: SurfaceType::I32,
                mode: HostFunctionMode::Sync,
                stable_id: StableId::from_name("FixtureHost.clock"),
                declaration_fingerprint: [0x41; 32],
                import_index: 0,
                fuel_cost: 1,
                async_result: None,
                required_capabilities: Vec::new(),
                source: None,
            }],
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: None,
        }),
        ..AnalysisEnvironment::default()
    }
}

fn host_environment_with_clock(result: SurfaceType, mode: HostFunctionMode) -> AnalysisEnvironment {
    let async_result = (mode == HostFunctionMode::Request).then(|| HostAsyncResultSurface {
        result_type: match &result {
            SurfaceType::I32 => nexa_core::canonical_result_type_id(
                nexa_core::CanonicalValueType::I32,
                nexa_core::CanonicalValueType::Bool,
            ),
            _ => panic!("the async clock fixture has an i32 success payload"),
        },
        success: result.clone(),
        error: SurfaceType::Bool,
        cancel_policy: IrCancelPolicy::ReturnError,
        abandon_policy: IrAbandonPolicy::ReturnError,
        cancel_error: Some(0),
        abandon_error: Some(1),
    });
    AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "FixtureHost".into(),
            contract_stable_id: StableId::from_name("FixtureHost"),
            types: Vec::new(),
            functions: vec![HostFunctionSurface {
                name: "clock".into(),
                parameters: Vec::new(),
                result,
                mode,
                stable_id: StableId::from_name("FixtureHost.clock"),
                declaration_fingerprint: [0x41; 32],
                import_index: 0,
                fuel_cost: 1,
                async_result,
                required_capabilities: Vec::new(),
                source: None,
            }],
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: None,
        }),
        ..AnalysisEnvironment::default()
    }
}

fn opaque_host_type(name: &str) -> ExternalTypeSurface {
    ExternalTypeSurface {
        name: name.to_owned(),
        kind: ExternalTypeKind::Opaque,
        stable_id: Some(StableId::from_name(name)),
        type_parameters: Vec::new(),
        fields: Vec::new(),
        variants: Vec::new(),
        source: None,
    }
}

fn only_expression_statement_type(outcome: &AnalysisOutcome) -> IrType {
    let ir = outcome.ir.as_ref().unwrap_or_else(|| {
        panic!(
            "analysis succeeds; diagnostics: {:#?}",
            outcome.diagnostics.diagnostics()
        )
    });
    let module = ir
        .modules()
        .iter()
        .find(|module| {
            module.package_id.as_str() == ROOT_PACKAGE && module.module.as_str() == "app.main"
        })
        .expect("root module exists");
    let function = module
        .declarations
        .iter()
        .find_map(|declaration| match &declaration.body {
            TypedDeclarationBody::Function(function) => Some(function),
            _ => None,
        })
        .expect("fixture function exists");
    let TypedStatementIr::Let {
        value: Some(expression),
        ..
    } = &function.body.statements[0]
    else {
        panic!("fixture body begins with one initialized binding");
    };
    assert!(matches!(
        expression.kind,
        TypedExpressionKind::HostCall { .. }
    ));
    expression.ty.clone()
}

fn dependency_alias_fixture(root_source: &str, swap_aliases: bool) -> ResolvedBuildInput {
    let (chosen_path, retained_path) = if swap_aliases {
        ("../lib-b", "../lib-a")
    } else {
        ("../lib-a", "../lib-b")
    };
    let root_manifest = Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{ROOT_PACKAGE}"
name = "Alias Retarget Fixture"
version = "1.0.0"
source_root = "src"
entry = "app.main"
activation = "programmatic"

[dependencies]
chosen = {{ path = "{chosen_path}" }}
retained = {{ path = "{retained_path}" }}
"#
        ))
        .expect("valid retarget manifest"),
    );
    let library = |id: &str, name: &str, directory: &str| FixturePackage {
        manifest: Arc::new(
            PackageManifest::parse(&format!(
                r#"
schema = 2
kind = "library"
id = "{id}"
name = "{name}"
version = "1.0.0"
source_root = "src"
"#
            ))
            .expect("valid retarget library manifest"),
        ),
        sources: source_set(
            id,
            &[(
                "src/math.nexa",
                "pub fn value() -> i32 { return 1; }\n",
                SourceRole::Production,
            )],
        ),
        directory: NormalizedPackagePath::new(directory).unwrap(),
    };
    let dependencies = [
        library("example.alpha", "Alpha", "workspace/lib-a"),
        library("example.beta", "Beta", "workspace/lib-b"),
    ];
    resolved_input(
        FixturePackage {
            manifest: root_manifest,
            sources: source_set(
                ROOT_PACKAGE,
                &[("src/app/main.nexa", root_source, SourceRole::Production)],
            ),
            directory: NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap(),
        },
        &dependencies,
    )
}

fn called_value_package(outcome: &AnalysisOutcome) -> String {
    let ir = outcome.ir.as_ref().expect("analysis succeeds");
    let module = ir
        .modules()
        .iter()
        .find(|module| {
            module.package_id.as_str() == ROOT_PACKAGE && module.module.as_str() == "app.main"
        })
        .expect("root module exists");
    module
        .resolved_references
        .iter()
        .map(|reference| &ir.definitions()[reference.target.0 as usize])
        .find(|definition| {
            definition.name == "value" && definition.package_id.as_str() != ROOT_PACKAGE
        })
        .expect("dependency call target is recorded")
        .package_id
        .as_str()
        .to_owned()
}

fn mixed_import_fixture(
    compilation_options: CompilationOptions,
) -> (ResolvedBuildInput, AnalysisEnvironment, &'static str) {
    const MAIN: &str = "use package::app::util as local;\nuse shared_api::math as dependency;\nuse std::core as standard;\nuse fixture::static as static_api;\nuse host::fixture_host as host_api;\npub fn run() -> i32 { return 0; }\n";
    const UTIL: &str = "pub(package) fn value() -> i32 { return 1; }\n";
    const LIBRARY: &str = "pub fn twice(value: i32) -> i32 { return value + value; }\n";
    let dependency = dependency_fixture(&[("src/math.nexa", LIBRARY, SourceRole::Production)]);
    let input = resolved_input_with_options(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/util.nexa", UTIL, SourceRole::Production),
            ],
            true,
        ),
        std::slice::from_ref(&dependency),
        compilation_options,
    );
    let mut environment = host_environment();
    environment.static_modules.push(StaticModuleSurface {
        module: ModulePath::new("fixture.static").unwrap(),
        types: Vec::new(),
        constants: Vec::new(),
        functions: Vec::new(),
    });
    (input, environment, MAIN)
}

fn main_import_edges(outcome: &AnalysisOutcome) -> Vec<(&str, &ResolvedImportTarget)> {
    outcome
        .resolved_import_edges
        .iter()
        .filter(|edge| {
            edge.importer.package_id.as_str() == ROOT_PACKAGE
                && edge.importer.module.as_str() == "app.main"
        })
        .map(|edge| (edge.alias.as_str(), &edge.target))
        .collect()
}

#[test]
fn imports_per_module_counts_every_resolved_namespace_kind() {
    let mut compilation_options = CompilationOptions::default();
    compilation_options.limits.imports_per_module = 4;
    compilation_options.limits.module_edges = usize::MAX;
    let (input, environment, main) = mixed_import_fixture(compilation_options);
    let outcome = analyze_deterministically(&input, &environment);
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2702);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(main, "use host::fixture_host as host_api;"),
    );

    let edges = main_import_edges(&outcome);
    assert_eq!(
        edges.len(),
        5,
        "limit diagnostics must not erase exact edges"
    );
    assert_eq!(
        edges.iter().map(|(alias, _)| *alias).collect::<Vec<_>>(),
        ["dependency", "host_api", "local", "standard", "static_api"]
    );
    assert!(edges.iter().any(|(alias, target)| {
        *alias == "dependency"
            && matches!(
                target,
                ResolvedImportTarget::Module(module)
                    if module.package_id.as_str() == DEPENDENCY_PACKAGE
                        && module.module.as_str() == "math"
            )
    }));
    assert!(edges.iter().any(|(alias, target)| {
        *alias == "local"
            && matches!(
                target,
                ResolvedImportTarget::Module(module)
                    if module.package_id.as_str() == ROOT_PACKAGE
                        && module.module.as_str() == "app.util"
            )
    }));
    assert!(edges.iter().any(|(alias, target)| {
        *alias == "standard"
            && matches!(
                target,
                ResolvedImportTarget::Module(module)
                    if module.package_id.as_str() == nexa_stdlib::PACKAGE_ID
                        && module.module.as_str() == "std.core"
            )
    }));
    assert!(edges.iter().any(|(alias, target)| {
        *alias == "static_api"
            && matches!(
                target,
                ResolvedImportTarget::Static(module) if module.as_str() == "fixture.static"
            )
    }));
    assert!(edges.iter().any(
        |(alias, target)| *alias == "host_api" && matches!(target, ResolvedImportTarget::Host)
    ));
}

#[test]
fn module_edge_limit_counts_non_source_imports_across_the_closure() {
    let mut compilation_options = CompilationOptions::default();
    compilation_options.limits.imports_per_module = usize::MAX;
    compilation_options.limits.module_edges = 4;
    let (input, environment, main) = mixed_import_fixture(compilation_options);
    let outcome = analyze_deterministically(&input, &environment);
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2702);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(main, "use host::fixture_host as host_api;"),
    );
    assert_eq!(main_import_edges(&outcome).len(), 5);
}

#[test]
fn dynamic_while_limit_changes_both_build_identity_and_typed_ir() {
    const SOURCE: &str = "pub fn run() -> unit {\n    while false { break; }\n}\n";
    let build = |max_while_iterations| {
        let options = CompilationOptions {
            max_while_iterations,
            ..CompilationOptions::default()
        };
        let input = resolved_input_with_options(
            root_fixture(
                &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
                false,
            ),
            &[],
            options,
        );
        let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
        let ir = outcome.ir.as_ref().unwrap_or_else(|| {
            panic!(
                "while fixture analyzes: {:?}",
                outcome.diagnostics.diagnostics()
            )
        });
        let function = ir
            .modules()
            .iter()
            .find(|module| module.module.as_str() == "app.main")
            .and_then(|module| {
                module
                    .declarations
                    .iter()
                    .find_map(|declaration| match &declaration.body {
                        TypedDeclarationBody::Function(function) => Some(function),
                        _ => None,
                    })
            })
            .expect("run function exists");
        let TypedStatementIr::While { max_iterations, .. } = &function.body.statements[0] else {
            panic!("run begins with a while loop");
        };
        (input.build_fingerprint, *max_iterations)
    };

    let first = build(17);
    let second = build(29);
    assert_ne!(first.0, second.0);
    assert_eq!(first.1, 17);
    assert_eq!(second.1, 29);
}

#[test]
fn assignable_places_and_index_key_types_match_typed_codegen() {
    const SOURCE: &str = "struct Record { value: i32, }\nclass Boxed { mut value: i32, }\npub fn run(buffer: Buffer<i32>) -> i32 {\n    let object: Boxed = new Boxed { value: 0 };\n    object.value = 1;\n    let record: Record = Record { value: 1 };\n    let changed: Record = Record { value: 2, ..record };\n    let array: Array<i32> = [0];\n    array[0] = changed.value;\n    let table: Map<bool, i32> = Map::new();\n    table[true] = object.value;\n    buffer[0] = table[true];\n    return buffer[0];\n}\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());
}

#[test]
fn analyzer_rejects_immutable_places_and_noncomparable_values() {
    const SOURCE: &str = "struct Record { value: i32, }\nclass Boxed { value: i32, }\npub fn run() -> bool {\n    let record: Record = Record { value: 1 };\n    record.value = 2;\n    let object: Boxed = new Boxed { value: 1 };\n    object.value = 2;\n    let text: string = \"abc\";\n    text[0] = 'x';\n    let values: Array<i32> = [1];\n    return values == values;\n}\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostics = outcome.diagnostics.diagnostics();
    assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code, diagnostic.message.as_ref()))
            .collect::<Vec<_>>(),
        [
            (ErrorCode::NX2501, "binding `record` is immutable",),
            (ErrorCode::NX2501, "class field `value` is immutable",),
            (
                ErrorCode::NX2101,
                "assignment index requires Array, Map, or Buffer",
            ),
            (ErrorCode::NX2101, "invalid binary operand type"),
        ]
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn type_mismatch_points_at_expression_and_carries_structured_types() {
    const SOURCE: &str = "pub fn run() -> i32 { return true; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2101);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "true"),
    );
    assert_eq!(
        diagnostic
            .notes
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>(),
        ["expected type: `i32`", "actual type: `bool`"]
    );
}

#[test]
fn poison_types_never_suppress_unrelated_real_errors() {
    const SOURCE: &str = "pub fn run() -> i32 {\n    let value: u32 = 1;\n    return true;\n}\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostics = outcome.diagnostics.diagnostics();
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, ErrorCode::NX2002);
    assert_eq!(diagnostics[1].code, ErrorCode::NX2101);
    assert!(diagnostics[1].message.contains("expected i32, found bool"));
}

#[test]
fn rust_macro_shapes_suppress_downstream_unknown_function() {
    const SOURCE: &str = "pub fn run() -> i32 {\n    println!(\"hi\");\n    return 0;\n}\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostics = outcome.diagnostics.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, ErrorCode::NX1002);
    assert!(diagnostics[0].message.contains("Rust macro"));
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == ErrorCode::NX2001));
}

#[test]
fn return_statements_strictly_distinguish_unit_from_recovery_types() {
    const MISSING_VALUE: &str = "fn bad() -> i32 { return; }\n";
    const UNEXPECTED_VALUE: &str = "fn bad() -> unit { return 1; }\n";
    const RECOVERY_VALUE: &str = "fn bad() -> i32 { return missing; }\n";
    const UNIT_CALL_WITH_BAD_ARGUMENT: &str = concat!(
        "",
        "fn sink(value: i32) -> unit {}\n",
        "fn bad() -> i32 { return sink(true); }\n",
    );

    let missing_value = analyze_main_source(MISSING_VALUE);
    let diagnostic = one_diagnostic(&missing_value, ErrorCode::NX2101);
    assert_eq!(diagnostic.message.as_ref(), "expected i32, found unit");
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(MISSING_VALUE, "return;"),
    );
    assert!(missing_value.ir.is_none());

    let unexpected_value = analyze_main_source(UNEXPECTED_VALUE);
    let diagnostic = one_diagnostic(&unexpected_value, ErrorCode::NX2101);
    assert_eq!(diagnostic.message.as_ref(), "expected unit, found i32");
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(UNEXPECTED_VALUE, "1"),
    );
    assert!(unexpected_value.ir.is_none());

    let recovery_value = analyze_main_source(RECOVERY_VALUE);
    assert_eq!(recovery_value.diagnostics.len(), 1);
    assert_eq!(
        recovery_value.diagnostics.diagnostics()[0].code,
        ErrorCode::NX2001
    );

    let unit_call = analyze_main_source(UNIT_CALL_WITH_BAD_ARGUMENT);
    let mut messages = diagnostics_with_code(&unit_call, ErrorCode::NX2101)
        .iter()
        .map(|diagnostic| diagnostic.message.as_ref())
        .collect::<Vec<_>>();
    messages.sort_unstable();
    assert_eq!(
        messages,
        ["expected i32, found bool", "expected i32, found unit"]
    );
}

#[test]
fn enum_match_reports_missing_variants_on_the_whole_match() {
    const SOURCE: &str = "enum Choice { A, B }\nfn run(value: Choice) -> i32 { return match value { Choice::A => 1 }; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2201);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "match value { Choice::A => 1 }"),
    );
    assert!(
        diagnostic
            .labels
            .iter()
            .all(|label| { label.style != LabelStyle::Secondary })
    );
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.as_ref() == "missing variant: B")
    );
}

#[test]
fn enum_match_reports_duplicate_arm_and_first_arm_exactly() {
    const SOURCE: &str = "enum Choice { A }\nfn run(value: Choice) -> i32 { return match value { Choice::A => 1, Choice::A => 2 }; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2202);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "Choice::A => 2"),
    );
    let secondary = diagnostic
        .labels
        .iter()
        .filter(|label| label.style == LabelStyle::Secondary)
        .collect::<Vec<_>>();
    assert_eq!(secondary.len(), 1);
    assert_eq!(secondary[0].range, range(SOURCE, "Choice::A => 1"));
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.as_ref() == "duplicate variant: A")
    );
}

#[test]
fn constructor_and_try_diagnostics_are_structured_and_source_exact() {
    const CONSTRUCTOR: &str = "fn run() -> i32 { let value = Option::None; return 0; }\n";
    const NON_RESULT: &str = "fn run() -> i32 { return 1?; }\n";
    const ERROR_MISMATCH: &str = "fn run() -> Result<i32, bool> { let value: Result<i32, i32> = Result::Err(1); return value?; }\n";

    let constructor = analyze_main_source(CONSTRUCTOR);
    let diagnostic = one_diagnostic(&constructor, ErrorCode::NX2210);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(CONSTRUCTOR, "Option::None"),
    );

    let non_result = analyze_main_source(NON_RESULT);
    let diagnostic = one_diagnostic(&non_result, ErrorCode::NX2220);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(NON_RESULT, "?"),
    );
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.as_ref() == "actual type: `i32`")
    );

    let error_mismatch = analyze_main_source(ERROR_MISMATCH);
    let diagnostic = one_diagnostic(&error_mismatch, ErrorCode::NX2221);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(ERROR_MISMATCH, "?"),
    );
    assert_eq!(
        diagnostic
            .notes
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>(),
        ["function error type: `bool`", "expression error type: `i32`"]
    );
}

#[test]
fn async_diagnostics_point_at_await_and_the_unawaited_call() {
    const OUTSIDE_TASK: &str =
        "fn work() -> i32 { return 1; }\nfn run() -> i32 { return work().await; }\n";
    const MISSING_AWAIT: &str =
        "async fn work() -> i32 { return 1; }\nasync fn run() -> i32 { return work(); }\n";

    let outside_task = analyze_main_source(OUTSIDE_TASK);
    let diagnostic = one_diagnostic(&outside_task, ErrorCode::NX2301);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(OUTSIDE_TASK, ".await"),
    );

    let missing_await = analyze_main_source(MISSING_AWAIT);
    let diagnostic = one_diagnostic(&missing_await, ErrorCode::NX2302);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(MISSING_AWAIT, "work()"),
    );
}

#[test]
fn async_await_rejects_literal_and_ordinary_function_operands() {
    const LITERAL: &str = "async fn run() -> i32 { return 1.await; }\n";
    const ORDINARY: &str =
        "fn ordinary() -> i32 { return 1; }\nasync fn run() -> i32 { return ordinary().await; }\n";

    for source in [LITERAL, ORDINARY] {
        let outcome = analyze_main_source(source);
        let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2301);
        assert_primary(
            diagnostic,
            &identity(ROOT_PACKAGE, "src/app/main.nexa"),
            last_range(source, ".await"),
        );
        assert_eq!(
            diagnostic.message.as_ref(),
            "`.await` requires an asynchronous call result"
        );
        assert!(
            outcome.ir.is_none(),
            "invalid await must not produce typed IR"
        );
    }
}

#[test]
fn conversion_and_field_diagnostics_point_at_the_complete_invalid_expression() {
    const CONVERSION: &str = "fn run() -> i32 { let value: i64 = 1; return value; }\n";
    const FIELD: &str =
        "struct Record { value: i32, }\nfn run(record: Record) -> i32 { return record.missing; }\n";

    let conversion = analyze_main_source(CONVERSION);
    let diagnostic = one_diagnostic(&conversion, ErrorCode::NX2401);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(CONVERSION, "value"),
    );

    let field = analyze_main_source(FIELD);
    let diagnostic = one_diagnostic(&field, ErrorCode::NX2501);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(FIELD, "record.missing"),
    );
}

#[test]
fn migration_diagnostics_enforce_context_finalization_and_exact_forwarding() {
    const OUTSIDE: &str = "fn run() -> i32 { return old.get<i32>(legacy); }\n";
    const UNFINISHED: &str = "@migration\npub fn migrate() -> bool { return true; }\n";
    const MISSING_FORWARDING: &str = "@migration\npub fn migrate() -> bool { old.get<i32>(legacy); finish_migration(); return true; }\n";
    const DUPLICATE_FORWARDING: &str = "@migration\npub fn migrate() -> bool { preserve(legacy); preserve(legacy); finish_migration(); return true; }\n";

    let outside = analyze_main_source(OUTSIDE);
    let diagnostic = one_diagnostic(&outside, ErrorCode::NX2601);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(OUTSIDE, "old.get<i32>(legacy)"),
    );

    let unfinished = analyze_main_source(UNFINISHED);
    let diagnostic = one_diagnostic(&unfinished, ErrorCode::NX2602);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(UNFINISHED, "{ return true; }"),
    );

    let missing_forwarding = analyze_main_source(MISSING_FORWARDING);
    let diagnostic = one_diagnostic(&missing_forwarding, ErrorCode::NX2603);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(
            MISSING_FORWARDING,
            "{ old.get<i32>(legacy); finish_migration(); return true; }",
        ),
    );
    assert!(
        diagnostic
            .notes
            .iter()
            .any(|note| note.starts_with("missing stable ID:"))
    );

    let duplicate_forwarding = analyze_main_source(DUPLICATE_FORWARDING);
    let diagnostic = one_diagnostic(&duplicate_forwarding, ErrorCode::NX2604);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(DUPLICATE_FORWARDING, "preserve(legacy)"),
    );
}

#[test]
fn migration_cfg_tracks_mutually_exclusive_branches_and_every_normal_exit() {
    const BRANCH_OK: &str = "@migration\npub fn migrate() -> bool {\n    let flag: bool = old.get<bool>(switch);\n    preserve(switch);\n    old.get<i32>(legacy);\n    if flag { preserve(legacy); } else { delete(legacy); }\n    finish_migration();\n    return true;\n}\n";
    const EXIT_OK: &str = "@migration\npub fn migrate() -> bool {\n    let flag: bool = old.get<bool>(switch);\n    preserve(switch);\n    if flag { finish_migration(); return true; }\n    finish_migration();\n    return true;\n}\n";
    const CROSS_BRANCH_MISSING: &str = "@migration\npub fn migrate() -> bool {\n    let flag: bool = old.get<bool>(switch);\n    preserve(switch);\n    if flag { old.get<i32>(legacy); } else { preserve(legacy); }\n    finish_migration();\n    return true;\n}\n";
    const CONDITIONAL_FINISH: &str = "@migration\npub fn migrate() -> bool {\n    let flag: bool = old.get<bool>(switch);\n    preserve(switch);\n    if flag { finish_migration(); }\n    return true;\n}\n";
    const UNREACHABLE_FINISH: &str =
        "@migration\npub fn migrate() -> bool {\n    return true;\n    finish_migration();\n}\n";

    for source in [BRANCH_OK, EXIT_OK] {
        let outcome = analyze_main_source(source);
        assert!(
            outcome.diagnostics.diagnostics().is_empty(),
            "{:#?}",
            outcome.diagnostics.diagnostics()
        );
        assert!(outcome.ir.is_some());
    }
    for source in [CONDITIONAL_FINISH, UNREACHABLE_FINISH] {
        let outcome = analyze_main_source(source);
        assert!(
            !diagnostics_with_code(&outcome, ErrorCode::NX2602).is_empty(),
            "{:#?}",
            outcome.diagnostics.diagnostics()
        );
    }
    let cross_branch = analyze_main_source(CROSS_BRANCH_MISSING);
    assert_eq!(
        diagnostics_with_code(&cross_branch, ErrorCode::NX2603).len(),
        1,
        "{:#?}",
        cross_branch.diagnostics.diagnostics()
    );
}

#[test]
fn migration_cfg_models_short_circuit_try_finalization_barrier_and_loops() {
    const SHORT_CIRCUIT: &str = "@migration\npub fn migrate() -> bool {\n    false && finish_migration();\n    return true;\n}\n";
    const TRY_EXIT: &str = "@migration\npub fn migrate() -> Result<bool, i32> {\n    let value: bool = old.get<Result<bool, i32>>(legacy)?;\n    preserve(legacy);\n    finish_migration();\n    return Result::Ok(value);\n}\n";
    const AFTER_FINISH: &str = "@migration\npub fn migrate() -> bool {\n    old.get<i32>(legacy);\n    finish_migration();\n    preserve(legacy);\n    return true;\n}\n";
    const MULTIPLE_FINISH: &str = "@migration\npub fn migrate() -> bool {\n    finish_migration();\n    finish_migration();\n    return true;\n}\n";
    const LOOP_DUPLICATE: &str = "@migration\npub fn migrate() -> bool {\n    for step in 0..2 { preserve(legacy); continue; }\n    finish_migration();\n    return true;\n}\n";
    const LOOP_BREAK_OK: &str = "@migration\npub fn migrate() -> bool {\n    for step in 0..10 { preserve(legacy); break; }\n    finish_migration();\n    return true;\n}\n";

    for source in [SHORT_CIRCUIT, TRY_EXIT, AFTER_FINISH, MULTIPLE_FINISH] {
        let outcome = analyze_main_source(source);
        assert!(
            !diagnostics_with_code(&outcome, ErrorCode::NX2602).is_empty(),
            "{:#?}",
            outcome.diagnostics.diagnostics()
        );
    }
    let after_finish = analyze_main_source(AFTER_FINISH);
    assert!(
        !diagnostics_with_code(&after_finish, ErrorCode::NX2603).is_empty(),
        "{:#?}",
        after_finish.diagnostics.diagnostics()
    );
    let loop_duplicate = analyze_main_source(LOOP_DUPLICATE);
    assert!(
        !diagnostics_with_code(&loop_duplicate, ErrorCode::NX2604).is_empty(),
        "{:#?}",
        loop_duplicate.diagnostics.diagnostics()
    );
    let loop_break = analyze_main_source(LOOP_BREAK_OK);
    assert!(
        loop_break.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        loop_break.diagnostics.diagnostics()
    );
}

#[test]
fn migration_static_ranges_propagate_each_binding_and_model_zero_break_continue() {
    const ZERO_ITERATIONS: &str = "@migration\npub fn migrate() -> bool {\n    old.get<i32>(legacy);\n    for step in 0..0 { preserve(legacy); }\n    finish_migration();\n    return true;\n}\n";
    const ONE_ITERATION: &str = "@migration\npub fn migrate() -> bool {\n    old.get<i32>(legacy);\n    for step in 0..1 { if step == 0 { preserve(legacy); } }\n    finish_migration();\n    return true;\n}\n";
    const BINDING_DEPENDENT: &str = "@migration\npub fn migrate() -> bool {\n    old.get<i32>(legacy);\n    for step in 0..2 {\n        if step == 0 { preserve(legacy); continue; }\n        break;\n    }\n    finish_migration();\n    return true;\n}\n";
    const DUPLICATE_ON_SECOND: &str = "@migration\npub fn migrate() -> bool {\n    for step in 0..2 {\n        if step < 2 { preserve(legacy); }\n    }\n    finish_migration();\n    return true;\n}\n";

    let zero = analyze_main_source(ZERO_ITERATIONS);
    assert_eq!(
        diagnostics_with_code(&zero, ErrorCode::NX2603).len(),
        1,
        "{:#?}",
        zero.diagnostics.diagnostics()
    );
    for source in [ONE_ITERATION, BINDING_DEPENDENT] {
        let outcome = analyze_main_source(source);
        assert!(
            outcome.diagnostics.diagnostics().is_empty(),
            "{:#?}",
            outcome.diagnostics.diagnostics()
        );
        assert!(outcome.ir.is_some());
    }
    let duplicate = analyze_main_source(DUPLICATE_ON_SECOND);
    assert!(
        !diagnostics_with_code(&duplicate, ErrorCode::NX2604).is_empty(),
        "{:#?}",
        duplicate.diagnostics.diagnostics()
    );
}

#[test]
fn static_range_binding_is_read_only() {
    const SOURCE: &str =
        "pub fn run() -> i32 {\n    for step in 0..2 { step = 0; }\n    return 0;\n}\n";
    let outcome = analyze_main_source(SOURCE);

    assert!(outcome.ir.is_none());
    assert!(
        outcome.diagnostics.diagnostics().iter().any(|diagnostic| {
            diagnostic.code == ErrorCode::NX2101
                && diagnostic
                    .message
                    .contains("static-range binding `step` is read-only")
        }),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
}

#[test]
fn migration_intrinsics_require_exact_state_class_owners_and_targets() {
    const WRONG_OLD_OWNER: &str = "@state(version = 1) class Left { mut value: i32, }\n@state(version = 1) class Right { mut value: i32, }\n@migration\npub fn migrate() -> bool {\n    let object: Left = old.get<Left>(legacy);\n    old.field<i32>(object, Right::value);\n    preserve(legacy);\n    finish_migration();\n    return true;\n}\n";
    const WRONG_NEW_OWNER: &str = "@state(version = 1) class Left { mut value: i32, }\n@state(version = 1) class Right { mut value: i32, }\n@migration\npub fn migrate() -> bool {\n    let object: Left = new.create<Left>(replacement);\n    new.set(object, Right::value, 1);\n    replace(legacy, object);\n    finish_migration();\n    return true;\n}\n";
    const WRONG_REPLACE_TARGET: &str = "struct Plain { value: i32, }\n@migration\npub fn migrate() -> bool {\n    let object: Plain = Plain { value: 1 };\n    replace(legacy, object);\n    finish_migration();\n    return true;\n}\n";

    for source in [WRONG_OLD_OWNER, WRONG_NEW_OWNER, WRONG_REPLACE_TARGET] {
        let outcome = analyze_main_source(source);
        assert!(
            !diagnostics_with_code(&outcome, ErrorCode::NX2101).is_empty(),
            "{:#?}",
            outcome.diagnostics.diagnostics()
        );
        assert!(outcome.ir.is_none());
    }
}

#[test]
fn direct_state_class_construction_is_rejected_for_literal_and_new_syntax() {
    const SOURCE: &str = r"@state(version = 1) class State { mut value: i32, }
fn construct_literal() -> State { return State { value: 1 }; }
fn construct_new() -> State { return new State { value: 2 }; }
";
    let outcome = analyze_main_source(SOURCE);
    let diagnostics = diagnostics_with_code(&outcome, ErrorCode::NX2101);
    assert_eq!(diagnostics.len(), 2, "{:#?}", outcome.diagnostics);
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_ref())
            .collect::<Vec<_>>(),
        [
            "Class construction requires `new`; a Struct constructor cannot name this type",
            "@state Class values cannot be constructed directly",
        ]
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn direct_state_class_field_read_is_rejected() {
    const SOURCE: &str = r"@state(version = 1) class State { mut value: i32, }
fn read(state: State) -> i32 { return state.value; }
";
    let outcome = analyze_main_source(SOURCE);
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2101);
    assert_eq!(
        diagnostic.message.as_ref(),
        "@state fields cannot be accessed directly"
    );
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "state.value"),
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn direct_state_class_field_assignment_is_rejected() {
    const SOURCE: &str = r"@state(version = 1) class State { mut value: i32, }
fn write(state: State) -> i32 {
    state.value = 1;
    return 0;
}
";
    let outcome = analyze_main_source(SOURCE);
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2101);
    assert_eq!(
        diagnostic.message.as_ref(),
        "@state fields cannot be accessed directly"
    );
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "state.value"),
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn migration_intrinsics_and_state_handle_resolution_remain_the_state_value_paths() {
    const SOURCE: &str = r"@state(version = 1) class State { mut value: i32, }
fn resolve(handle: StateHandle<State>) -> Result<State, StateHandleError> {
    return handle.resolve();
}
@migration
pub fn migrate() -> bool {
    let old_state: State = old.get<State>(legacy);
    let value: i32 = old.field<i32>(old_state, State::value);
    let replacement: State = new.create<State>(replacement);
    new.set(replacement, State::value, value);
    replace(legacy, replacement);
    finish_migration();
    return true;
}
";
    let outcome = analyze_main_source(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("migration and StateHandle paths produce TypedPackageIr");
    assert!(ir.definitions().iter().any(|definition| {
        definition.name == "resolve" && definition.module.as_str() == "app.main"
    }));
    assert!(ir.metadata().lifecycle.migration.is_some());
}

#[test]
fn generated_defer_cleanup_has_an_analysis_assigned_stable_symbol() {
    const SOURCE: &str =
        "pub fn run() -> i32 {\n    let value: i32 = 1;\n    defer value;\n    return value;\n}\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("valid defer package");
    let cleanup = ir
        .definitions()
        .iter()
        .find(|definition| definition.name == "__defer_run_0")
        .expect("generated cleanup definition");
    assert_eq!(cleanup.effect, IrEffect::Cleanup);
    let expected = CanonicalSymbolIdentity::automatic(
        ROOT_PACKAGE,
        "app.main",
        SymbolKind::Function,
        "__defer_run_0",
    );
    let stable = cleanup
        .stable_symbol
        .as_ref()
        .expect("emitted cleanup has a stable symbol");
    assert_eq!(stable.canonical, expected);
    assert_eq!(stable.runtime_id, expected.runtime_id());
}

#[test]
fn cross_file_namespace_call_resolves_into_typed_package_ir() {
    const MAIN: &str =
        "use package::app::util as util;\npub fn run() -> i32 { return util::value(); }\n";
    const UTIL: &str = "pub(package) fn value() -> i32 { return 41 + 1; }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/util.nexa", UTIL, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("valid package has TypedPackageIr");
    let target = ir
        .definitions()
        .iter()
        .find(|definition| {
            definition.package_id.as_str() == ROOT_PACKAGE
                && definition.module.as_str() == "app.util"
                && definition.name == "value"
        })
        .expect("utility function definition");
    let main = ir
        .modules()
        .iter()
        .find(|module| module.module.as_str() == "app.main")
        .expect("entry module");
    let reference = main
        .resolved_references
        .iter()
        .find(|reference| reference.target == target.id)
        .expect("namespace call resolves to the utility definition");
    assert_eq!(reference.span.source.path.as_str(), "src/app/main.nexa");
    let expected = range(MAIN, "util::value");
    assert_eq!(
        (reference.span.start, reference.span.end),
        (expected.start, expected.end)
    );
}

#[test]
fn imported_namespace_value_fields_and_method_lower_from_the_nominal_receiver() {
    const MAIN: &str =
        "use package::app::util as u;\npub fn run() -> i32 { return u::VALUE.text.len(); }\n";
    const UTIL: &str = "pub(package) struct Record { text: string, }\npub(package) const VALUE: Record = Record { text: \"scale\", };\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/util.nexa", UTIL, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert_namespace_value_field_method_shape(&outcome, ROOT_PACKAGE, "app.util");
}

#[test]
fn dependency_namespace_value_field_and_method_use_the_dependency_nominal_owner() {
    const MAIN: &str =
        "use shared_api::math as u;\npub fn run() -> i32 { return u::VALUE.text.len(); }\n";
    const LIBRARY: &str = "pub struct Record { text: string, }\npub const VALUE: Record = Record { text: \"dependency\", };\n";
    let dependency = dependency_fixture(&[("src/math.nexa", LIBRARY, SourceRole::Production)]);
    let input = resolved_input(
        root_fixture(&[("src/app/main.nexa", MAIN, SourceRole::Production)], true),
        std::slice::from_ref(&dependency),
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert_namespace_value_field_method_shape(&outcome, DEPENDENCY_PACKAGE, "math");
}

#[test]
fn qualified_namespace_types_constructors_and_variants_remain_symbolic() {
    const MAIN: &str = r#"use package::app::model;
pub fn run() -> i32 {
    let record: model::Record = model::Record { text: "qualified", };
    let choice: model::Choice = model::Choice::Some(record);
    let empty: model::Choice = model::Choice::Empty;
    return match choice {
        model::Choice::Some(value) => value.text.len(),
        model::Choice::Empty => 0,
    };
}
"#;
    const MODEL: &str = r"pub(package) struct Record { text: string, }
pub(package) enum Choice { Empty, Some(Record), }
";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/model.nexa", MODEL, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("qualified constructors produce TypedPackageIr");
    for name in ["Record", "Choice", "Some", "Empty", "text"] {
        let target = ir
            .definitions()
            .iter()
            .find(|definition| definition.module.as_str() == "app.model" && definition.name == name)
            .unwrap_or_else(|| panic!("model definition `{name}`"));
        assert!(
            ir.modules()
                .iter()
                .find(|module| module.module.as_str() == "app.main")
                .expect("entry module")
                .resolved_references
                .iter()
                .any(|reference| reference.target == target.id),
            "qualified use records a reference to `{name}`"
        );
    }
}

#[test]
fn lexical_receiver_shadows_an_imported_namespace_for_fields_and_methods() {
    const MAIN: &str = r"use package::app::util as value;
pub(package) struct Record { text: string, }
fn run(value: Record) -> i32 { return value.text.len(); }
";
    const UTIL: &str = r#"pub(package) fn text() -> string { return "namespace"; }
"#;
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/util.nexa", UTIL, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("lexical receiver produces TypedPackageIr");
    let module = ir
        .modules()
        .iter()
        .find(|module| module.module.as_str() == "app.main")
        .expect("entry module");
    let function = module
        .declarations
        .iter()
        .find_map(|declaration| match &declaration.body {
            TypedDeclarationBody::Function(function) => Some(function),
            _ => None,
        })
        .expect("run function");
    let TypedStatementIr::Return(Some(returned)) = &function.body.statements[0] else {
        panic!("run returns the receiver method");
    };
    let TypedExpressionKind::BuiltinCall {
        operation: BuiltinOperationIr::StringLen,
        arguments,
        ..
    } = &returned.kind
    else {
        panic!("qualified lexical receiver ends in string len");
    };
    let TypedExpressionKind::Field { base, field } = &arguments[0].kind else {
        panic!("lexical receiver field is lowered as a field");
    };
    assert!(matches!(base.kind, TypedExpressionKind::Reference(_)));
    assert_eq!(ir.definitions()[field.0 as usize].name, "text");
    assert_eq!(
        ir.definitions()[field.0 as usize].module.as_str(),
        "app.main"
    );
}

fn assert_namespace_value_field_method_shape(
    outcome: &AnalysisOutcome,
    receiver_package: &str,
    receiver_module: &str,
) {
    let ir = outcome.ir.as_ref().expect("analysis succeeds");
    let module = ir
        .modules()
        .iter()
        .find(|module| {
            module.package_id.as_str() == ROOT_PACKAGE && module.module.as_str() == "app.main"
        })
        .expect("root module");
    let function = module
        .declarations
        .iter()
        .find_map(|declaration| match &declaration.body {
            TypedDeclarationBody::Function(function) => Some(function),
            _ => None,
        })
        .expect("run function");
    let TypedStatementIr::Return(Some(returned)) = &function.body.statements[0] else {
        panic!("run returns the qualified receiver method");
    };
    let TypedExpressionKind::BuiltinCall {
        operation: BuiltinOperationIr::StringLen,
        arguments,
        ..
    } = &returned.kind
    else {
        panic!("final member is lowered as the string len method");
    };
    let TypedExpressionKind::Field { base, field } = &arguments[0].kind else {
        panic!("namespace value member is lowered as a field access");
    };
    let TypedExpressionKind::Reference(value) = base.kind else {
        panic!("field base is the imported namespace value");
    };
    let value = &ir.definitions()[value.0 as usize];
    assert_eq!(value.package_id.as_str(), receiver_package);
    assert_eq!(value.module.as_str(), receiver_module);
    assert_eq!(value.name, "VALUE");
    let field = &ir.definitions()[field.0 as usize];
    assert_eq!(field.package_id.as_str(), receiver_package);
    assert_eq!(field.module.as_str(), receiver_module);
    assert_eq!(field.name, "text");
}

#[test]
fn private_cross_file_access_has_use_site_and_related_declaration() {
    const MAIN: &str =
        "use package::app::util as util;\npub fn run() -> i32 { return util::secret(); }\n";
    const UTIL: &str = "fn secret() -> i32 { return 7; }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/util.nexa", UTIL, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2705);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(MAIN, "secret"),
    );
    assert_eq!(diagnostic.related.len(), 1);
    assert_eq!(
        diagnostic.related[0].source,
        identity(ROOT_PACKAGE, "src/app/util.nexa")
    );
    assert_eq!(diagnostic.related[0].range, range(UTIL, "secret"));
}

#[test]
fn dependency_alias_allows_public_call_and_rejects_private_call() {
    const LIBRARY: &str = "pub fn twice(value: i32) -> i32 { return value + value; }\nfn hidden() -> i32 { return 9; }\n";
    const PUBLIC_MAIN: &str =
        "use shared_api::math;\npub fn run() -> i32 { return math::twice(3); }\n";
    const PRIVATE_MAIN: &str =
        "use shared_api::math;\npub fn run() -> i32 { return math::hidden(); }\n";
    let dependency = dependency_fixture(&[("src/math.nexa", LIBRARY, SourceRole::Production)]);
    let public_input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", PUBLIC_MAIN, SourceRole::Production)],
            true,
        ),
        std::slice::from_ref(&dependency),
    );
    let public = analyze_deterministically(&public_input, &AnalysisEnvironment::default());
    assert!(
        public.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        public.diagnostics.diagnostics()
    );
    assert!(public.ir.is_some());

    let private_input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", PRIVATE_MAIN, SourceRole::Production)],
            true,
        ),
        std::slice::from_ref(&dependency),
    );
    let private = analyze_deterministically(&private_input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&private, ErrorCode::NX2705);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(PRIVATE_MAIN, "hidden"),
    );
    assert_eq!(
        diagnostic.related[0].source,
        identity(DEPENDENCY_PACKAGE, "src/math.nexa")
    );
    assert_eq!(diagnostic.related[0].range, range(LIBRARY, "hidden"));
}

#[test]
fn source_path_is_the_only_module_identity() {
    const SOURCE: &str = "pub fn run() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.as_ref().is_some_and(|ir| {
        ir.modules()
            .iter()
            .any(|module| module.module.as_str() == "app.main")
    }));
}

#[test]
fn unknown_use_points_at_the_qualified_path() {
    const SOURCE: &str = "use missing::target;\npub fn run() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2703);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "missing::target"),
    );
}

#[test]
fn module_cycle_is_canonical_and_source_exact() {
    const A: &str = "use package::app::b;\npub fn a() -> i32 { return 1; }\n";
    const B: &str = "use package::app::a;\npub fn b() -> i32 { return 2; }\n";
    const MAIN: &str = "pub fn run() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/a.nexa", A, SourceRole::Production),
                ("src/app/b.nexa", B, SourceRole::Production),
                ("src/app/main.nexa", MAIN, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2702);
    assert_eq!(
        diagnostic.message.as_ref(),
        "module cycle: app.a -> app.b -> app.a"
    );
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/a.nexa"),
        range(A, "package::app::b"),
    );
}

#[test]
fn duplicate_use_alias_points_at_the_second_use() {
    const MAIN: &str = "use package::app::a as same;\nuse package::app::b as same;\npub fn run() -> i32 { return 0; }\n";
    const A: &str = "pub(package) fn a() -> i32 { return 1; }\n";
    const B: &str = "pub(package) fn b() -> i32 { return 2; }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/a.nexa", A, SourceRole::Production),
                ("src/app/b.nexa", B, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2704);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(MAIN, "same"),
    );
}

#[test]
fn public_api_cannot_expose_a_private_type() {
    const SOURCE: &str =
        "struct Hidden { value: i32, }\npub fn leak(value: Hidden) -> Hidden { return value; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostics = diagnostics_with_code(&outcome, ErrorCode::NX2706);
    assert_eq!(diagnostics.len(), 2);
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        assert_primary(
            diagnostic,
            &identity(ROOT_PACKAGE, "src/app/main.nexa"),
            nth_range(SOURCE, "Hidden", index + 2),
        );
        assert_eq!(diagnostic.related.len(), 1);
        assert_eq!(
            diagnostic.related[0].source,
            identity(ROOT_PACKAGE, "src/app/main.nexa")
        );
        assert_eq!(diagnostic.related[0].range, range(SOURCE, "Hidden"));
    }
}

#[test]
fn inaccessible_qualified_api_type_points_at_only_the_type_token() {
    const MAIN: &str =
        "use package::app::types;\npub fn leak(value: types::Hidden) -> i32 { return 0; }\n";
    const TYPES: &str = "struct Hidden { value: i32, }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/types.nexa", TYPES, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2706);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(MAIN, "Hidden"),
    );
    assert_eq!(diagnostic.related.len(), 1);
    assert_eq!(
        diagnostic.related[0].source,
        identity(ROOT_PACKAGE, "src/app/types.nexa")
    );
    assert_eq!(diagnostic.related[0].range, range(TYPES, "Hidden"));
}

#[test]
fn stable_names_reject_invalid_and_duplicate_identities() {
    const SOURCE: &str = "@stable(\"1bad\") pub fn invalid() -> i32 { return 0; }\n@stable(\"same-name\") pub fn first() -> i32 { return 1; }\n@stable(\"same-name\") pub fn second() -> i32 { return 2; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let invalid = one_diagnostic(&outcome, ErrorCode::NX2710);
    assert_primary(
        invalid,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(SOURCE, "1bad"),
    );
    let duplicate = diagnostics_with_code(&outcome, ErrorCode::NX2711)
        .into_iter()
        .find(|diagnostic| !diagnostic.related.is_empty())
        .expect("duplicate stable name diagnostic");
    assert_primary(
        duplicate,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(SOURCE, "same-name"),
    );
    assert_eq!(duplicate.related[0].range, range(SOURCE, "same-name"));
}

#[test]
fn const_expression_cannot_call_a_function() {
    const SOURCE: &str = "fn make() -> i32 { return 1; }\npub const BAD: i32 = make();\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2720);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        last_range(SOURCE, "make()"),
    );
}

#[test]
fn tests_reject_bad_signatures_and_indirect_host_calls() {
    const MAIN: &str = "use host::fixture_host as host;\npub(package) fn host_value() -> i32 { return host::clock(); }\n";
    const TESTS: &str = "use package::app::main as app;\n@test fn bad_signature(value: i32) -> i32 { return value; }\n@test fn indirect_host() -> bool { return app::host_value() == 0; }\n";
    let product = Arc::new(resolved_input(
        root_fixture(
            &[("src/app/main.nexa", MAIN, SourceRole::Production)],
            false,
        ),
        &[],
    ));
    let tests = source_set(
        ROOT_PACKAGE,
        &[("tests/checks.nexa", TESTS, SourceRole::Test)],
    );
    let input = ResolvedTestInput::new(product, tests).expect("valid test build input");
    let outcome = analyze_tests_deterministically(&input, &host_environment());
    let diagnostics = diagnostics_with_code(&outcome, ErrorCode::NX2730);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_primary(
        diagnostics[0],
        &identity(ROOT_PACKAGE, "tests/checks.nexa"),
        range(TESTS, "bad_signature"),
    );
    assert_primary(
        diagnostics[1],
        &identity(ROOT_PACKAGE, "tests/checks.nexa"),
        range(TESTS, "indirect_host"),
    );
    assert!(
        diagnostics[1]
            .message
            .contains("indirect_host -> host_value"),
        "{}",
        diagnostics[1].message
    );
}

#[test]
fn test_analysis_preserves_production_abi_state_and_product_query_authority() {
    const MAIN: &str = "@state(version = 1) pub class ProductState { mut value: i32, }\npub fn value() -> i32 { return 7; }\n";
    const TESTS: &str = "@state(version = 99) pub class TestOnlyState { mut ignored: string, }\npub fn test_only_api() -> i32 { return 99; }\n@test fn succeeds() -> bool { return true; }\n";
    let product = Arc::new(resolved_input(
        root_fixture(
            &[("src/app/main.nexa", MAIN, SourceRole::Production)],
            false,
        ),
        &[],
    ));
    let tests = source_set(
        ROOT_PACKAGE,
        &[("tests/checks.nexa", TESTS, SourceRole::Test)],
    );
    let test_input =
        ResolvedTestInput::new(Arc::clone(&product), tests).expect("valid test build input");
    let environment = AnalysisEnvironment::default();
    let mut db = QueryDatabase::new();

    let product_outcome = analyze_package(&product, &environment, &mut db);
    let product_ir = product_outcome
        .ir
        .as_ref()
        .expect("production analysis succeeds");
    assert_eq!(product_ir.metadata().state_types.len(), 1);
    let package = product.root_manifest.id.clone();
    let public_key = QueryKey::PackagePublicApi(package.clone());
    let state_key = QueryKey::PackageStateSchema(package.clone());
    let linked_key = QueryKey::LinkedArtifact(package);
    let public_before = db
        .cached_bytes(&public_key)
        .expect("production public API query");
    let state_before = db
        .cached_bytes(&state_key)
        .expect("production State schema query");
    let linked_before = db
        .cached_bytes(&linked_key)
        .expect("production linked-artifact query");

    let test_outcome = analyze_package_tests(&test_input, &environment, &mut db);
    let test_ir = test_outcome.ir.as_ref().expect("test analysis succeeds");
    assert_eq!(
        test_outcome.public_api_fingerprint,
        product_outcome.public_api_fingerprint
    );
    assert_eq!(
        test_outcome.state_schema_fingerprint,
        product_outcome.state_schema_fingerprint
    );
    assert_eq!(
        test_outcome.public_api_records,
        product_outcome.public_api_records
    );
    assert_eq!(
        test_outcome.state_schema_records,
        product_outcome.state_schema_records
    );
    assert_eq!(test_outcome.state_types.len(), 1);
    assert_eq!(test_ir.metadata().state_types.len(), 1);
    assert_eq!(
        test_ir.metadata().state_types,
        product_ir.metadata().state_types,
        "the test Runtime module must carry the exact production State schema"
    );
    assert!(
        test_outcome
            .query_report
            .reused_queries
            .iter()
            .chain(&test_outcome.query_report.invalidated_queries)
            .all(|key| !matches!(
                key,
                QueryKey::PackagePublicApi(_)
                    | QueryKey::PackageStateSchema(_)
                    | QueryKey::LinkedArtifact(_)
            )),
        "{:#?}",
        test_outcome.query_report
    );
    assert_eq!(
        db.cached_bytes(&public_key).as_deref(),
        Some(public_before.as_ref())
    );
    assert_eq!(
        db.cached_bytes(&state_key).as_deref(),
        Some(state_before.as_ref())
    );
    assert_eq!(
        db.cached_bytes(&linked_key).as_deref(),
        Some(linked_before.as_ref())
    );
}

#[test]
fn lifecycle_function_outside_entry_module_is_rejected() {
    const MAIN: &str = "pub fn run() -> i32 { return 0; }\n";
    const OTHER: &str = "@activation\npub fn activate() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[
                ("src/app/main.nexa", MAIN, SourceRole::Production),
                ("src/app/other.nexa", OTHER, SourceRole::Production),
            ],
            false,
        ),
        &[],
    );
    let outcome = analyze_deterministically(&input, &AnalysisEnvironment::default());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2740);
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/other.nexa"),
        range(OTHER, "activate"),
    );
}

#[test]
fn unknown_external_nominal_type_is_a_fail_closed_analysis_error() {
    const SOURCE: &str = "pub fn run() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let environment = host_environment_with_clock(
        SurfaceType::Named {
            module: ModulePath::new("host").unwrap(),
            name: "Missing".into(),
        },
        HostFunctionMode::Sync,
    );
    let outcome = analyze_deterministically(&input, &environment);
    assert!(outcome.ir.is_none());
    let diagnostic = one_diagnostic(&outcome, ErrorCode::NX2101);
    assert_eq!(
        diagnostic.message.as_ref(),
        "unknown external nominal type `host::Missing`"
    );
}

#[test]
fn external_nominal_type_can_resolve_to_the_dependency_definition() {
    const MAIN: &str = "pub fn run() -> i32 { return 0; }\n";
    const LIBRARY: &str = "pub struct Record { value: i32, }\n";
    let dependency = dependency_fixture(&[("src/math.nexa", LIBRARY, SourceRole::Production)]);
    let input = resolved_input(
        root_fixture(&[("src/app/main.nexa", MAIN, SourceRole::Production)], true),
        std::slice::from_ref(&dependency),
    );
    let environment = host_environment_with_clock(
        SurfaceType::Named {
            module: ModulePath::new("math").unwrap(),
            name: "Record".into(),
        },
        HostFunctionMode::Sync,
    );
    let outcome = analyze_deterministically(&input, &environment);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("dependency nominal Host result is valid");
    let host_function = ir
        .definitions()
        .iter()
        .find(|definition| definition.module.as_str() == "host" && definition.name == "clock")
        .expect("Host function definition");
    let IrType::Named(result) = host_function.ty else {
        panic!("Host result preserves the dependency nominal type");
    };
    let result = &ir.definitions()[result.0 as usize];
    assert_eq!(result.package_id.as_str(), DEPENDENCY_PACKAGE);
    assert_eq!(result.module.as_str(), "math");
    assert_eq!(result.name, "Record");
}

#[test]
fn host_call_requires_every_declared_function_capability() {
    const SOURCE: &str =
        "use host::fixture_host as host;\npub fn run() -> i32 { return host::clock(); }\n";
    let environment = {
        let mut environment = host_environment();
        environment.host.as_mut().unwrap().functions[0].required_capabilities =
            vec!["clock.read".into(), "clock.use".into()];
        environment
    };

    let build = |capabilities: &[&str]| {
        let mut root = root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        );
        root.manifest = application_manifest_with_capabilities(false, capabilities);
        resolved_input(root, &[])
    };

    let accepted = analyze_deterministically(&build(&["clock.read", "clock.use"]), &environment);
    assert!(
        accepted.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        accepted.diagnostics.diagnostics()
    );
    let accepted_ir = accepted.ir.as_ref().expect("Host call analysis succeeds");
    assert_eq!(
        accepted_ir.metadata().host_bindings[0].functions[0].declaration_fingerprint,
        [0x41; 32],
        "the validated NIDL declaration fingerprint is propagated verbatim"
    );

    let rejected = analyze_deterministically(&build(&["clock.read"]), &environment);
    assert!(rejected.ir.is_none());
    let diagnostic = one_diagnostic(&rejected, ErrorCode::NX4002);
    assert_eq!(
        diagnostic.message.as_ref(),
        "Host function requires capability `clock.use`"
    );
}

#[test]
fn async_host_result_preserves_both_nominal_arms() {
    const SOURCE: &str = "pub fn run() -> i32 { return 0; }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "FixtureHost".into(),
            contract_stable_id: StableId::from_name("FixtureHost"),
            types: vec![opaque_host_type("Failure"), opaque_host_type("Payload")],
            functions: vec![HostFunctionSurface {
                name: "load".into(),
                parameters: Vec::new(),
                result: SurfaceType::Unit,
                mode: HostFunctionMode::Request,
                stable_id: StableId::from_name("FixtureHost.load"),
                declaration_fingerprint: [0x42; 32],
                import_index: 0,
                fuel_cost: 1,
                async_result: Some(HostAsyncResultSurface {
                    result_type: nexa_core::canonical_result_type_id(
                        nexa_core::CanonicalValueType::Named(StableId::from_name("Payload")),
                        nexa_core::CanonicalValueType::Named(StableId::from_name("Failure")),
                    ),
                    success: SurfaceType::Named {
                        module: ModulePath::new("host").unwrap(),
                        name: "Payload".into(),
                    },
                    error: SurfaceType::Named {
                        module: ModulePath::new("host").unwrap(),
                        name: "Failure".into(),
                    },
                    cancel_policy: IrCancelPolicy::ReturnError,
                    abandon_policy: IrAbandonPolicy::ReturnError,
                    cancel_error: Some(0),
                    abandon_error: Some(1),
                }),
                required_capabilities: Vec::new(),
                source: None,
            }],
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: None,
        }),
        ..AnalysisEnvironment::default()
    };
    let outcome = analyze_deterministically(&input, &environment);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("known nominal async result is valid");
    let host_function = ir
        .definitions()
        .iter()
        .find(|definition| definition.module.as_str() == "host" && definition.name == "load")
        .expect("Host Request definition");
    let IrType::Result(success, error) = &host_function.ty else {
        panic!("Host Request result remains Result<S, E>");
    };
    for (ty, expected) in [(success.as_ref(), "Payload"), (error.as_ref(), "Failure")] {
        let IrType::Named(definition) = ty else {
            panic!("async Result arm remains nominal");
        };
        assert_eq!(ir.definitions()[definition.0 as usize].name, expected);
    }
}

#[test]
fn persistent_database_invalidates_typed_module_when_host_signature_changes() {
    const SOURCE: &str = "use host::fixture_host as api;\nfn run() -> i32 { let ignored = api::clock(); return 0; }\n";
    let first_input = resolved_input_with_contract(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
        b"fixture-host-clock-i32",
    );
    let second_input = resolved_input_with_contract(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
        b"fixture-host-clock-bool",
    );
    let module = ModuleKey::new(
        nexa_analysis::PackageId::new(ROOT_PACKAGE).unwrap(),
        ModulePath::new("app.main").unwrap(),
    );
    let mut database = QueryDatabase::new();

    let first = analyze_package(
        &first_input,
        &host_environment_with_clock(SurfaceType::I32, HostFunctionMode::Sync),
        &mut database,
    );
    assert_eq!(only_expression_statement_type(&first), IrType::I32);

    let second = analyze_package(
        &second_input,
        &host_environment_with_clock(SurfaceType::Bool, HostFunctionMode::Sync),
        &mut database,
    );
    assert_eq!(only_expression_statement_type(&second), IrType::Bool);
    assert!(
        second
            .query_report
            .invalidated_queries
            .contains(&QueryKey::HostContract(
                nexa_analysis::PackageId::new(ROOT_PACKAGE).unwrap()
            ))
    );
    assert!(
        second
            .query_report
            .invalidated_queries
            .contains(&QueryKey::TypedModule(module))
    );
    assert_eq!(second.query_report.revision, second.analyzed_revision);
}

#[test]
fn failed_host_mode_revision_cannot_pollute_next_successful_revision() {
    const SOURCE: &str = "use host::fixture_host as api;\nasync fn run() -> Result<i32, bool> { return api::clock().await; }\n";
    let sync_input = resolved_input_with_contract(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
        b"fixture-host-clock-sync",
    );
    let request_input = resolved_input_with_contract(
        root_fixture(
            &[("src/app/main.nexa", SOURCE, SourceRole::Production)],
            false,
        ),
        &[],
        b"fixture-host-clock-request",
    );
    let mut database = QueryDatabase::new();

    let failed = analyze_package(
        &sync_input,
        &host_environment_with_clock(SurfaceType::I32, HostFunctionMode::Sync),
        &mut database,
    );
    assert!(failed.ir.is_none());
    assert!(!failed.diagnostics.diagnostics().is_empty());

    let successful = analyze_package(
        &request_input,
        &host_environment_with_clock(SurfaceType::I32, HostFunctionMode::Request),
        &mut database,
    );
    assert!(
        successful.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        successful.diagnostics.diagnostics()
    );
    assert!(successful.ir.is_some());

    let hot = analyze_package(
        &request_input,
        &host_environment_with_clock(SurfaceType::I32, HostFunctionMode::Request),
        &mut database,
    );
    assert!(hot.ir.is_some());
    assert!(hot.query_report.reused_queries.iter().any(
        |key| matches!(key, QueryKey::TypedModule(module) if module.module.as_str() == "app.main")
    ));
}

#[test]
fn manifest_and_lock_alias_retarget_invalidates_import_and_typed_ir() {
    const SOURCE: &str = "use chosen::math as api;\nfn run() -> i32 { return api::value(); }\n";
    let first_input = dependency_alias_fixture(SOURCE, false);
    let second_input = dependency_alias_fixture(SOURCE, true);
    let root = nexa_analysis::PackageId::new(ROOT_PACKAGE).unwrap();
    let module = ModuleKey::new(root.clone(), ModulePath::new("app.main").unwrap());
    let mut database = QueryDatabase::new();

    let first = analyze_package(&first_input, &AnalysisEnvironment::default(), &mut database);
    assert_eq!(called_value_package(&first), "example.alpha");

    let second = analyze_package(
        &second_input,
        &AnalysisEnvironment::default(),
        &mut database,
    );
    assert_eq!(called_value_package(&second), "example.beta");
    for key in [
        QueryKey::PackageManifest(root.clone()),
        QueryKey::DependencyGraph(root),
        QueryKey::ResolvedImports(module.clone()),
        QueryKey::TypedModule(module),
    ] {
        assert!(
            second.query_report.invalidated_queries.contains(&key),
            "current analysis report omitted invalidated query {key:?}: {:#?}",
            second.query_report
        );
    }
}

#[test]
fn source_use_retarget_invalidates_current_typed_ir_without_global_context_change() {
    const FIRST_SOURCE: &str =
        "use chosen::math as api;\nfn run() -> i32 { return api::value(); }\n";
    const SECOND_SOURCE: &str =
        "use retained::math as api;\nfn run() -> i32 { return api::value(); }\n";
    let first_input = dependency_alias_fixture(FIRST_SOURCE, false);
    let second_input = dependency_alias_fixture(SECOND_SOURCE, false);
    let root = nexa_analysis::PackageId::new(ROOT_PACKAGE).unwrap();
    let module = ModuleKey::new(root, ModulePath::new("app.main").unwrap());
    let mut database = QueryDatabase::new();

    let first = analyze_package(&first_input, &AnalysisEnvironment::default(), &mut database);
    assert_eq!(called_value_package(&first), "example.alpha");

    let second = analyze_package(
        &second_input,
        &AnalysisEnvironment::default(),
        &mut database,
    );
    assert_eq!(called_value_package(&second), "example.beta");
    assert!(
        second
            .query_report
            .invalidated_queries
            .contains(&QueryKey::ResolvedImports(module.clone()))
    );
    assert!(
        second
            .query_report
            .invalidated_queries
            .contains(&QueryKey::TypedModule(module))
    );
    assert_eq!(second.query_report.revision, second.analyzed_revision);
}

#[test]
fn documentation_changes_source_identity_but_not_analyzed_public_api() {
    const FIRST: &str = "/// first wording\npub fn value() -> i32 { return 1; }\n";
    const SECOND: &str =
        "/// completely different documentation\npub fn value() -> i32 { return 1; }\n";
    let input = |source| {
        resolved_input(
            root_fixture(
                &[("src/app/main.nexa", source, SourceRole::Production)],
                false,
            ),
            &[],
        )
    };
    let first_input = input(FIRST);
    let second_input = input(SECOND);
    let mut first_database = QueryDatabase::new();
    let mut second_database = QueryDatabase::new();
    let first = analyze_package(
        &first_input,
        &AnalysisEnvironment::default(),
        &mut first_database,
    );
    let second = analyze_package(
        &second_input,
        &AnalysisEnvironment::default(),
        &mut second_database,
    );
    assert!(
        first.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        first.diagnostics.diagnostics()
    );
    assert!(
        second.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        second.diagnostics.diagnostics()
    );
    assert_ne!(
        first.source_set_fingerprint, second.source_set_fingerprint,
        "source identity includes documentation bytes"
    );
    assert_eq!(
        first.public_api_records, second.public_api_records,
        "documentation is not a semantic public record"
    );
    assert_eq!(
        first.public_api_fingerprint, second.public_api_fingerprint,
        "documentation cannot change the package public ABI"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn diagnostic_registry_retains_every_host_and_static_module_origin() {
    const MAIN: &str = "pub fn run() -> i32 { return 0; }\n";
    const HOST: &str = "contract FixtureHost { host { fn ping() -> i32; } }\r\n";
    const HOST_STRUCT: &str = "struct HostRecord { value: i32, }\r\n";
    const HOST_ENUM: &str = "enum HostChoice { Ready }\r\n";
    const STATIC_STRUCT: &str = "struct StaticRecord { value: i32, }\n";
    const STATIC_ENUM: &str = "enum StaticChoice { Ready }\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", MAIN, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let origin = |path: &str, text: &str, needle: &str| ExternalSourceOrigin {
        identity: SourceIdentity::standalone(path.to_owned()),
        text: Arc::from(text),
        range: range(text, needle),
    };
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "FixtureHost".into(),
            contract_stable_id: StableId::from_name("FixtureHost"),
            types: vec![
                ExternalTypeSurface {
                    name: "HostRecord".into(),
                    kind: ExternalTypeKind::Struct,
                    stable_id: Some(StableId::from_name("HostRecord")),
                    type_parameters: Vec::new(),
                    fields: vec![ExternalFieldSurface {
                        name: "value".into(),
                        stable_id: None,
                        ty: SurfaceType::I32,
                        source: Some(origin("contracts/host-field.nidl", HOST_STRUCT, "value")),
                    }],
                    variants: Vec::new(),
                    source: Some(origin(
                        "contracts/host-type.nidl",
                        HOST_STRUCT,
                        "HostRecord",
                    )),
                },
                ExternalTypeSurface {
                    name: "HostChoice".into(),
                    kind: ExternalTypeKind::Enum,
                    stable_id: Some(StableId::from_name("HostChoice")),
                    type_parameters: Vec::new(),
                    fields: Vec::new(),
                    variants: vec![ExternalVariantSurface {
                        name: "Ready".into(),
                        stable_id: None,
                        payload: Vec::new(),
                        source: Some(origin("contracts/host-variant.nidl", HOST_ENUM, "Ready")),
                    }],
                    source: Some(origin("contracts/host-enum.nidl", HOST_ENUM, "HostChoice")),
                },
            ],
            functions: vec![HostFunctionSurface {
                name: "ping".into(),
                parameters: Vec::new(),
                result: SurfaceType::I32,
                mode: HostFunctionMode::Sync,
                stable_id: StableId::from_name("FixtureHost.ping"),
                declaration_fingerprint: [0x43; 32],
                import_index: 0,
                fuel_cost: 1,
                async_result: None,
                required_capabilities: Vec::new(),
                source: Some(origin("contracts/host.nidl", HOST, "ping")),
            }],
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: Some(origin("contracts/host.nidl", HOST, "FixtureHost")),
        }),
        static_modules: vec![StaticModuleSurface {
            module: ModulePath::new("fixture.static").unwrap(),
            types: vec![
                ExternalTypeSurface {
                    name: "StaticRecord".into(),
                    kind: ExternalTypeKind::Struct,
                    stable_id: Some(StableId::from_name("StaticRecord")),
                    type_parameters: Vec::new(),
                    fields: vec![ExternalFieldSurface {
                        name: "value".into(),
                        stable_id: Some(StableId::from_name("StaticRecord.value")),
                        ty: SurfaceType::I32,
                        source: Some(origin("stdlib/static-field.nexa", STATIC_STRUCT, "value")),
                    }],
                    variants: Vec::new(),
                    source: Some(origin(
                        "stdlib/static-type.nexa",
                        STATIC_STRUCT,
                        "StaticRecord",
                    )),
                },
                ExternalTypeSurface {
                    name: "StaticChoice".into(),
                    kind: ExternalTypeKind::Enum,
                    stable_id: Some(StableId::from_name("StaticChoice")),
                    type_parameters: Vec::new(),
                    fields: Vec::new(),
                    variants: vec![ExternalVariantSurface {
                        name: "Ready".into(),
                        stable_id: Some(StableId::from_name("StaticChoice.Ready")),
                        payload: Vec::new(),
                        source: Some(origin("stdlib/static-variant.nexa", STATIC_ENUM, "Ready")),
                    }],
                    source: Some(origin(
                        "stdlib/static-enum.nexa",
                        STATIC_ENUM,
                        "StaticChoice",
                    )),
                },
            ],
            constants: Vec::new(),
            functions: Vec::new(),
        }],
    };

    let outcome = analyze_deterministically(&input, &environment);
    let expected = [
        ("contracts/host.nidl", HOST),
        ("contracts/host-type.nidl", HOST_STRUCT),
        ("contracts/host-field.nidl", HOST_STRUCT),
        ("contracts/host-enum.nidl", HOST_ENUM),
        ("contracts/host-variant.nidl", HOST_ENUM),
        ("stdlib/static-type.nexa", STATIC_STRUCT),
        ("stdlib/static-field.nexa", STATIC_STRUCT),
        ("stdlib/static-enum.nexa", STATIC_ENUM),
        ("stdlib/static-variant.nexa", STATIC_ENUM),
    ];
    for (path, text) in expected {
        let identity = SourceIdentity::standalone(path);
        assert_eq!(
            outcome
                .diagnostics
                .sources()
                .get(&identity)
                .map(|snapshot| snapshot.text()),
            Some(text),
            "missing exact external source snapshot {path}"
        );
    }
    let labeled_sources = outcome
        .diagnostics
        .diagnostics()
        .iter()
        .flat_map(|diagnostic| diagnostic.labels.iter())
        .map(|label| label.source.path())
        .collect::<Vec<_>>();
    assert!(labeled_sources.contains(&"contracts/host-field.nidl"));
    assert!(labeled_sources.contains(&"contracts/host-variant.nidl"));
    for diagnostic in outcome.diagnostics.diagnostics() {
        for identity in diagnostic
            .labels
            .iter()
            .map(|label| &label.source)
            .chain(diagnostic.related.iter().map(|related| &related.source))
            .chain(
                diagnostic
                    .fixes
                    .iter()
                    .filter_map(|fix| fix.source.as_ref()),
            )
        {
            assert!(
                outcome.diagnostics.sources().get(identity).is_some(),
                "diagnostic retained identity {identity} without a source snapshot"
            );
        }
    }
}

#[test]
fn conflicting_external_source_identity_fails_closed_before_typed_ir() {
    const MAIN: &str = "pub fn run() -> i32 { return 0; }\n";
    const CONTRACT: &str = "contract FixtureHost {}\n";
    const FUNCTION: &str = "fn ping() -> i32;\n";
    let input = resolved_input(
        root_fixture(
            &[("src/app/main.nexa", MAIN, SourceRole::Production)],
            false,
        ),
        &[],
    );
    let identity = SourceIdentity::standalone("contracts/conflict.nidl");
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "FixtureHost".into(),
            contract_stable_id: StableId::from_name("FixtureHost"),
            types: Vec::new(),
            functions: vec![HostFunctionSurface {
                name: "ping".into(),
                parameters: Vec::new(),
                result: SurfaceType::I32,
                mode: HostFunctionMode::Sync,
                stable_id: StableId::from_name("FixtureHost.ping"),
                declaration_fingerprint: [0x43; 32],
                import_index: 0,
                fuel_cost: 1,
                async_result: None,
                required_capabilities: Vec::new(),
                source: Some(ExternalSourceOrigin {
                    identity: identity.clone(),
                    text: Arc::from(FUNCTION),
                    range: range(FUNCTION, "ping"),
                }),
            }],
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: Some(ExternalSourceOrigin {
                identity: identity.clone(),
                text: Arc::from(CONTRACT),
                range: range(CONTRACT, "FixtureHost"),
            }),
        }),
        ..AnalysisEnvironment::default()
    };

    let outcome = analyze_deterministically(&input, &environment);
    assert!(
        outcome.ir.is_none(),
        "ambiguous source bytes must block typed IR"
    );
    let conflict = one_diagnostic(&outcome, ErrorCode::NX2704);
    assert!(
        conflict.message.contains("conflicting immutable snapshots"),
        "{conflict:#?}"
    );
    assert!(conflict.labels.is_empty());
    assert_eq!(
        outcome
            .diagnostics
            .sources()
            .get(&identity)
            .map(|snapshot| snapshot.text()),
        Some(CONTRACT),
        "deduplication retains the deterministic first snapshot only for rendering the error batch"
    );
}

#[test]
fn recursive_inline_value_layouts_are_rejected_without_implicit_boxing() {
    const DIRECT: &str = "enum Expr { Add(Expr, Expr), }\n";
    let direct = analyze_main_source(DIRECT);
    let diagnostic = one_diagnostic(&direct, ErrorCode::NX2101);
    assert_eq!(
        diagnostic.message.as_ref(),
        "recursive inline value layout: Expr -> Expr"
    );
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(DIRECT, "Add(Expr, Expr),"),
    );
    assert_eq!(
        diagnostic
            .notes
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>(),
        ["use a Class node to break the recursive inline value layout"]
    );
    assert!(direct.ir.is_none());

    let through_wrappers = concat!(
        "struct Left { right: Option<(Right, i32)>, }\n",
        "enum Right { Again(Result<Left, i32>), }\n",
    );
    let through_wrappers_outcome = analyze_main_source(through_wrappers);
    let diagnostic = one_diagnostic(&through_wrappers_outcome, ErrorCode::NX2101);
    assert_eq!(
        diagnostic.message.as_ref(),
        "recursive inline value layout: Left -> Right -> Left"
    );
    assert_primary(
        diagnostic,
        &identity(ROOT_PACKAGE, "src/app/main.nexa"),
        range(through_wrappers, "Again(Result<Left, i32>),"),
    );
    assert!(through_wrappers_outcome.ir.is_none());
}

#[test]
fn reference_and_container_edges_break_recursive_inline_layouts() {
    const SOURCE: &str = concat!(
        "class Node { next: Option<Node>, }\n",
        "struct Indirect {\n",
        "    object: Node,\n",
        "    children: Array<Indirect>,\n",
        "    lookup: Map<string, Indirect>,\n",
        "    scratch: Buffer<Indirect>,\n",
        "}\n",
    );
    let outcome = analyze_main_source(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());
}

#[test]
fn equality_recurses_through_value_layouts_and_compares_classes_by_identity() {
    const SOURCE: &str = concat!(
        "struct Point { x: i32, label: string, }\n",
        "enum Shape { Empty, One(Point), Named { point: Option<(Point, string)>, }, }\n",
        "class Object { payload: Buffer<i32>, }\n",
        "fn same_point(left: Point, right: Point) -> bool { return left == right; }\n",
        "fn same_shape(left: Shape, right: Shape) -> bool { return left == right; }\n",
        "fn same_wrapped(left: Option<(Point, string)>, right: Option<(Point, string)>) -> bool {\n",
        "    return left == right;\n",
        "}\n",
        "fn same_object(left: Object, right: Object) -> bool { return left == right; }\n",
    );
    let outcome = analyze_main_source(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());
}

#[test]
fn equality_rejects_resources_nested_in_structs_and_enum_payloads() {
    const SOURCE: &str = concat!(
        "struct ResourceStruct { values: Array<i32>, }\n",
        "enum ResourceEnum { Values(Option<(string, Map<string, i32>)>), }\n",
        "fn same_struct(left: ResourceStruct, right: ResourceStruct) -> bool {\n",
        "    return left == right;\n",
        "}\n",
        "fn same_enum(left: ResourceEnum, right: ResourceEnum) -> bool {\n",
        "    return left == right;\n",
        "}\n",
    );
    let outcome = analyze_main_source(SOURCE);
    let diagnostics = diagnostics_with_code(&outcome, ErrorCode::NX2101);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.message.as_ref() == "invalid binary operand type"),
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_label().unwrap().range)
            .collect::<Vec<_>>(),
        [
            range(SOURCE, "left == right"),
            last_range(SOURCE, "left == right"),
        ]
    );
    assert!(outcome.ir.is_none());
}
