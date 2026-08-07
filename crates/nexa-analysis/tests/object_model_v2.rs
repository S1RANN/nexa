use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CompilationLimits,
    CompilationOptions, NormalizedPackagePath, PackageKind, PackageManifest, QueryDatabase,
    ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole,
    SourceSetBuilder, analyze_package, canonical_compilation_options, source_set_fingerprint,
};

fn analyze_sources(sources: &[(&str, &str)]) -> AnalysisOutcome {
    let manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "test.object-model-v2"
name = "Object Model V2"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
        )
        .expect("valid object-model fixture manifest"),
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
                source_id: SourceId::new("object-model-v2-tests").expect("valid source id"),
                directory: NormalizedPackagePath::new("packages/object-model-v2")
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
    .expect("valid resolved object-model fixture");
    analyze_package(
        &input,
        &AnalysisEnvironment::default(),
        &mut QueryDatabase::new(),
    )
}

fn assert_analysis_succeeds(outcome: &AnalysisOutcome) {
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(
        outcome.ir.is_some(),
        "valid source must produce TypedPackageIr"
    );
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
fn const_struct_place_immutability_and_default_mutable_fields_are_enforced() {
    const VALID: &str = r#"
struct Cell {
    value: i32,
}

class Counter {
    value: i32,
}

struct References {
    counter: Counter,
    values: Array<i32>,
}

enum Choice {
    Empty,
    Stored(Cell),
}

pub fn run(buffer: Buffer<i32>) -> i32 {
    let cell = Cell { value: 1 };
    cell.value = 2;
    const counter = Counter { value: cell.value };
    counter.value = 3;
    const values: Array<i32> = Array::new();
    values[0] = counter.value;
    const table: Map<string, i32> = Map::new();
    table["counter"] = values[0];
    const frozen_buffer = buffer;
    frozen_buffer[0] = table["counter"];
    const references = References { counter, values };
    references.counter.value = frozen_buffer[0];
    references.values[0] = references.counter.value;
    let choice = Choice::Stored(cell);
    return match choice {
        Choice::Empty => 0,
        Choice::Stored(value) => value.value + references.counter.value,
    };
}
"#;
    const INVALID: &str = r"
struct Cell {
    value: i32,
}

class Counter {
    value: i32,
}

pub fn run() -> i32 {
    const cell = Cell { value: 1 };
    cell.value = 2;
    let counter = Counter { value: 1 };
    counter.value = 2;
    return 0;
}
";

    assert_analysis_succeeds(&analyze_sources(&[("src/main.nexa", VALID)]));

    let invalid = analyze_sources(&[("src/main.nexa", INVALID)]);
    assert_analysis_rejected(&invalid);
    let messages = invalid
        .diagnostics
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.message.as_ref())
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("binding `cell` is immutable")),
        "{messages:#?}"
    );
}

#[test]
fn recursive_value_types_are_rejected() {
    const SOURCE: &str = r"
struct RecursiveStruct {
    next: Option<RecursiveStruct>,
}

enum RecursiveEnum {
    End,
    Next(RecursiveEnum),
}

pub fn run() -> i32 {
    return 0;
}
";

    let outcome = analyze_sources(&[("src/main.nexa", SOURCE)]);
    assert_analysis_rejected(&outcome);
    let messages = outcome
        .diagnostics
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.message.as_ref())
        .filter(|message| message.contains("recursive inline value layout"))
        .collect::<Vec<_>>();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("RecursiveStruct")),
        "{messages:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("RecursiveEnum")),
        "{messages:#?}"
    );

    let reference_breaks = r"
class Node {
    next: Option<Node>,
}

struct Batch {
    items: Array<Batch>,
}

struct Holder {
    node: Node,
}

pub fn run() -> i32 {
    return 0;
}
";
    assert_analysis_succeeds(&analyze_sources(&[("src/main.nexa", reference_breaks)]));
}

#[test]
fn box_and_pointer_surface_types_are_rejected() {
    const CASES: &[(&str, &str)] = &[
        (
            "Box",
            "pub fn reject_box(value: Box<i32>) -> i32 { return 0; }\n",
        ),
        (
            "Gc",
            "pub fn reject_gc(value: Gc<i32>) -> i32 { return 0; }\n",
        ),
        (
            "Ref",
            "pub fn reject_ref(value: Ref<i32>) -> i32 { return 0; }\n",
        ),
        (
            "raw pointer",
            "pub fn reject_pointer(value: *i32) -> i32 { return 0; }\n",
        ),
        (
            "borrow",
            "pub fn reject_borrow(value: &i32) -> i32 { return 0; }\n",
        ),
    ];

    for (name, source) in CASES {
        let outcome = analyze_sources(&[("src/main.nexa", source)]);
        assert!(
            !outcome.diagnostics.diagnostics().is_empty(),
            "forbidden {name} surface type was accepted"
        );
        assert_analysis_rejected(&outcome);
    }
}
