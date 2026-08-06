use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, BuildFingerprintInput, CompilationLimits, CompilationOptions,
    NormalizedPackagePath, PackageManifest, PackageSourceSet, QueryDatabase, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, ResolvedTestInput, SourceId, SourceRole,
    SourceSetBuilder, analyze_package, analyze_package_tests, source_set_fingerprint,
};
use nexa_diagnostics::{ByteRange, DiagnosticRenderer};

struct ExpectedDiagnostic {
    code: &'static str,
    directory: &'static str,
    package: &'static str,
    path: &'static str,
    range: ByteRange,
}

const CASES: &[ExpectedDiagnostic] = &[
    ExpectedDiagnostic {
        code: "NX2701",
        directory: "NX2701",
        package: "diagnostic.nx2701",
        path: "src/main.nexa",
        range: ByteRange::new(7, 17),
    },
    ExpectedDiagnostic {
        code: "NX2702",
        directory: "NX2702",
        package: "diagnostic.nx2702",
        path: "src/cycle/a.nexa",
        range: ByteRange::new(4, 21),
    },
    ExpectedDiagnostic {
        code: "NX2703",
        directory: "NX2703",
        package: "diagnostic.nx2703",
        path: "src/main.nexa",
        range: ByteRange::new(4, 28),
    },
    ExpectedDiagnostic {
        code: "NX2704",
        directory: "NX2704",
        package: "diagnostic.nx2704",
        path: "src/main.nexa",
        range: ByteRange::new(67, 73),
    },
    ExpectedDiagnostic {
        code: "NX2705",
        directory: "NX2705",
        package: "diagnostic.nx2705",
        path: "src/main.nexa",
        range: ByteRange::new(65, 78),
    },
    ExpectedDiagnostic {
        code: "NX2706",
        directory: "NX2706",
        package: "diagnostic.nx2706",
        path: "src/main.nexa",
        range: ByteRange::new(49, 55),
    },
    ExpectedDiagnostic {
        code: "NX2710",
        directory: "NX2710",
        package: "diagnostic.nx2710",
        path: "src/main.nexa",
        range: ByteRange::new(9, 22),
    },
    ExpectedDiagnostic {
        code: "NX2711",
        directory: "NX2711",
        package: "diagnostic.nx2711",
        path: "src/main.nexa",
        range: ByteRange::new(69, 76),
    },
    ExpectedDiagnostic {
        code: "NX2720",
        directory: "NX2720",
        package: "diagnostic.nx2720",
        path: "src/main.nexa",
        range: ByteRange::new(61, 69),
    },
    ExpectedDiagnostic {
        code: "NX2730",
        directory: "NX2730",
        package: "diagnostic.nx2730",
        path: "tests/bad_test.nexa",
        range: ByteRange::new(9, 17),
    },
    ExpectedDiagnostic {
        code: "NX2740",
        directory: "NX2740",
        package: "diagnostic.nx2740",
        path: "src/support.nexa",
        range: ByteRange::new(19, 27),
    },
];

#[test]
fn m4_semantic_codes_come_from_real_package_analysis() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/diagnostics/analysis");
    for expected in CASES {
        let input = load_fixture(&root.join(expected.directory));
        let first = analyze(&input);
        let second = analyze(&input);
        assert_eq!(
            DiagnosticRenderer::ndjson(&first.diagnostics).unwrap(),
            DiagnosticRenderer::ndjson(&second.diagnostics).unwrap(),
            "{} output is not deterministic",
            expected.code
        );
        let diagnostics = first.diagnostics.diagnostics();
        assert_eq!(
            diagnostics.len(),
            1,
            "{} emitted unexpected extra diagnostics: {diagnostics:#?}",
            expected.code
        );
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.code.as_str(), expected.code);
        let primary = diagnostic
            .primary_label()
            .unwrap_or_else(|| panic!("{} has no primary source label", expected.code));
        assert_eq!(
            primary.source.package_id(),
            Some(expected.package),
            "{} primary package changed",
            expected.code
        );
        assert_eq!(
            primary.source.path(),
            expected.path,
            "{} primary source changed",
            expected.code
        );
        assert_eq!(
            primary.range, expected.range,
            "{} primary byte range changed",
            expected.code
        );
        let source = first
            .diagnostics
            .sources()
            .get(&primary.source)
            .unwrap_or_else(|| panic!("{} has an unregistered primary source", expected.code));
        assert!(
            usize::try_from(primary.range.end).unwrap() <= source.text().len(),
            "{} primary range exceeds its source",
            expected.code
        );
        assert!(
            DiagnosticRenderer::human(&first.diagnostics).contains(expected.code),
            "{} is absent from human output",
            expected.code
        );
        let json = DiagnosticRenderer::json(&first.diagnostics).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(
            json["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["code"] == expected.code),
            "{} is absent from JSON output",
            expected.code
        );
        for related in &diagnostic.related {
            let source = first
                .diagnostics
                .sources()
                .get(&related.source)
                .unwrap_or_else(|| panic!("{} has an unregistered related source", expected.code));
            assert!(
                !related.range.is_empty()
                    && usize::try_from(related.range.end).unwrap() <= source.text().len(),
                "{} related range is invalid",
                expected.code
            );
        }
    }
}

struct FixtureInput {
    product: Arc<ResolvedBuildInput>,
    tests: Option<Arc<PackageSourceSet>>,
}

fn analyze(input: &FixtureInput) -> nexa_analysis::AnalysisOutcome {
    let environment = AnalysisEnvironment::default();
    let mut database = QueryDatabase::new();
    if let Some(tests) = &input.tests {
        let tests = ResolvedTestInput::new(Arc::clone(&input.product), Arc::clone(tests)).unwrap();
        analyze_package_tests(&tests, &environment, &mut database)
    } else {
        analyze_package(&input.product, &environment, &mut database)
    }
}

fn build_source_set(
    directory: &Path,
    package: nexa_analysis::PackageId,
    paths: Vec<PathBuf>,
    role: SourceRole,
    limits: CompilationLimits,
) -> Arc<PackageSourceSet> {
    let mut sources = SourceSetBuilder::new(package, limits);
    for path in paths {
        sources
            .add(
                NormalizedPackagePath::from_path(path.strip_prefix(directory).unwrap()).unwrap(),
                std::fs::read_to_string(path).unwrap(),
                role,
            )
            .unwrap();
    }
    Arc::new(sources.build().unwrap())
}

fn load_fixture(directory: &Path) -> FixtureInput {
    let manifest = Arc::new(
        PackageManifest::parse(&std::fs::read_to_string(directory.join("package.toml")).unwrap())
            .unwrap(),
    );
    let compilation_options = CompilationOptions::default();
    let mut paths = Vec::new();
    collect_sources(&directory.join("src"), &mut paths);
    let mut test_paths = Vec::new();
    if directory.join("tests").exists() {
        collect_sources(&directory.join("tests"), &mut test_paths);
    }
    paths.sort();
    test_paths.sort();
    let limits = compilation_options.limits;
    let sources = build_source_set(
        directory,
        manifest.id.clone(),
        paths,
        SourceRole::Production,
        limits,
    );
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("diagnostic-corpus").unwrap(),
                directory: NormalizedPackagePath::new(format!(
                    "diagnostics/{}",
                    manifest.id.as_str().replace('.', "/")
                ))
                .unwrap(),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
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
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.to_owned(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.to_owned(),
        compiler_options: nexa_analysis::canonical_compilation_options(&compilation_options),
        canonical_lock_graph: Vec::new(),
    };
    let product = ResolvedBuildInput::new(
        manifest,
        sources,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        fingerprint.host_required_entrypoints.clone(),
        compilation_options,
        fingerprint,
    )
    .unwrap();
    let tests = (!test_paths.is_empty()).then(|| {
        build_source_set(
            directory,
            product.root_manifest.id.clone(),
            test_paths,
            SourceRole::Test,
            CompilationLimits::default(),
        )
    });
    FixtureInput {
        product: Arc::new(product),
        tests,
    }
}

fn collect_sources(directory: &Path, output: &mut Vec<PathBuf>) {
    let mut entries = std::fs::read_dir(directory)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            collect_sources(&path, output);
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "nexa") {
            output.push(path);
        }
    }
}
