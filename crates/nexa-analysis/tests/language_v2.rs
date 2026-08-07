use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CompilationLimits,
    CompilationOptions, IrEffect, NormalizedPackagePath, PackageKind, PackageManifest,
    QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage, SourceId,
    SourceRole, SourceSetBuilder, analyze_package, canonical_compilation_options,
    source_set_fingerprint,
};

fn analyze_sources(sources: &[(&str, &str)]) -> AnalysisOutcome {
    let manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "test.language-v2"
name = "Language V2"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
        )
        .expect("valid language-v2 fixture manifest"),
    );
    let options = CompilationOptions::default();
    let mut source_builder =
        SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    for (path, text) in sources {
        source_builder
            .add(
                NormalizedPackagePath::new(path).expect("normalized fixture path"),
                *text,
                SourceRole::Production,
            )
            .expect("valid fixture source");
    }
    let source_set = Arc::new(source_builder.build().expect("valid fixture source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("language-v2-tests").expect("valid source id"),
                directory: NormalizedPackagePath::new("packages/language-v2")
                    .expect("normalized package path"),
                kind: PackageKind::Application,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint_input = BuildFingerprintInput {
        root_package: manifest.id.clone(),
        root_manifest: manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&source_set),
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
        compiler_options: canonical_compilation_options(&options),
        canonical_lock_graph: Vec::new(),
    };
    let input = ResolvedBuildInput::new(
        Arc::clone(&manifest),
        source_set,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        options,
        fingerprint_input,
    )
    .expect("valid resolved language-v2 fixture");
    analyze_package(
        &input,
        &AnalysisEnvironment::default(),
        &mut QueryDatabase::new(),
    )
}

fn assert_analysis_rejected(outcome: &AnalysisOutcome) {
    assert!(
        !outcome.diagnostics.diagnostics().is_empty(),
        "invalid source must emit diagnostics"
    );
    assert!(
        outcome.ir.is_none(),
        "invalid source must not produce TypedPackageIr"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn language_v2_positive_matrix_analyzes() {
    const SUPPORT: &str = r"
pub(package) fn base_value() -> i32 {
    return 2;
}
";
    const MAIN: &str = r#"
use package::support;

const MAX_SCORE: i32 = 10;

struct Cell {
    x: i32,
    y: i32,
}

enum Choice {
    Empty,
    Value(i32),
}

const ORIGIN: Cell = Cell { x: 0, y: 0 };
const EMPTY_SCORE: Option<i32> = Option::None;
const SOME_SCORE: Option<i32> = Option::Some(1);
const OK_SCORE: Result<i32, string> = Result::Ok(2);
const DEFAULT_CHOICE: Choice = Choice::Value(3);
const FLAGS: (i32, bool) = (1, true);

class Counter {
    value: i32,
    label: string,
}

@state(version = 1)
class SavedState {
    score: i32,
}

async fn load_score() -> i32 {
    return MAX_SCORE;
}

async fn load_twice() -> i32 {
    let score: i32 = load_score().await;
    defer score;
    return score + score;
}

pub fn run() -> i32 {
    let cell = Cell { x: 1, y: support::base_value() };
    cell.x = 2;
    let moved = Cell { y: 3, ..cell };
    let counter = Counter { value: moved.x, label: "v2" };
    counter.value = counter.value + 1;
    let choice = Choice::Value(counter.value);
    return match choice {
        Choice::Empty => 0,
        Choice::Value(value) => value,
    };
}
"#;

    let outcome = analyze_sources(&[("src/main.nexa", MAIN), ("src/support.nexa", SUPPORT)]);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert_eq!(
        outcome.resolved_import_edges.len(),
        1,
        "the package-root use must resolve to the support module"
    );
    let ir = outcome
        .ir
        .as_ref()
        .expect("valid source produces TypedPackageIr");
    for expected in [
        "MAX_SCORE",
        "Cell",
        "Choice",
        "ORIGIN",
        "EMPTY_SCORE",
        "SOME_SCORE",
        "OK_SCORE",
        "DEFAULT_CHOICE",
        "FLAGS",
        "Counter",
        "SavedState",
        "load_score",
        "load_twice",
        "run",
    ] {
        assert!(
            ir.definitions()
                .iter()
                .any(|definition| definition.name == expected),
            "missing typed definition `{expected}`"
        );
    }
    assert!(
        ir.definitions().iter().any(
            |definition| definition.name == "load_twice" && definition.effect == IrEffect::Task
        ),
        "async fn must lower with the internal task effect"
    );
    assert!(
        ir.definitions().iter().any(|definition| definition
            .name
            .starts_with("__defer_load_twice_")
            && definition.effect == IrEffect::Cleanup),
        "defer must produce a typed cleanup definition"
    );
}

#[test]
fn legacy_surface_matrix_is_rejected() {
    const CASES: &[(&str, &str)] = &[
        (
            "module",
            "module wrong.name;\npub fn run() -> i32 { return 0; }\n",
        ),
        (
            "import",
            "import package.support;\npub fn run() -> i32 { return 0; }\n",
        ),
        (
            "var",
            "pub fn run() -> i32 { var value: i32 = 0; return value; }\n",
        ),
        ("task", "task fn run() -> i32 { return 0; }\n"),
        (
            "prefix-await",
            "async fn load() -> i32 { return 1; }\nasync fn run() -> i32 { return await load(); }\n",
        ),
        ("stateful", "stateful class State { value: i32, }\n"),
        (
            "migration-function",
            "pub migration fn migrate() -> bool { return true; }\n",
        ),
        (
            "activation-function",
            "pub activation fn activate() -> bool { return true; }\n",
        ),
        (
            "cleanup-function",
            "pub cleanup fn cleanup() -> bool { return true; }\n",
        ),
        (
            "immediate-function",
            "pub immediate fn calculate() -> i32 { return 1; }\n",
        ),
        (
            "with-update",
            "struct Cell { x: i32, }\npub fn run() -> i32 { let cell = Cell { x: 1 }; let moved = cell with { x: 2 }; return moved.x; }\n",
        ),
    ];

    for (name, source) in CASES {
        let outcome = analyze_sources(&[("src/main.nexa", source)]);
        assert!(
            !outcome.diagnostics.diagnostics().is_empty(),
            "legacy `{name}` source was accepted"
        );
        assert_analysis_rejected(&outcome);
    }
}
