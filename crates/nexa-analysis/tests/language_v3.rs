use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CollectionIterationKindIr,
    CompilationLimits, CompilationOptions, IrType, NormalizedPackagePath, PackageKind,
    PackageManifest, QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage,
    SourceId, SourceRole, SourceSetBuilder, TypedDeclarationBody, TypedStatementIr,
    analyze_package, canonical_compilation_options, source_set_fingerprint,
};

fn analyze_main(source: &str) -> AnalysisOutcome {
    let manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "test.language-v3"
name = "Language V3"
version = "1.0.0"
source_root = "src"
entry = "main"
activation = "programmatic"
"#,
        )
        .expect("valid language-v3 fixture manifest"),
    );
    let options = CompilationOptions::default();
    let mut source_builder =
        SourceSetBuilder::new(manifest.id.clone(), CompilationLimits::default());
    source_builder
        .add(
            NormalizedPackagePath::new("src/main.nexa").expect("normalized fixture path"),
            source,
            SourceRole::Production,
        )
        .expect("valid fixture source");
    let source_set = Arc::new(source_builder.build().expect("valid fixture source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("language-v3-tests").expect("valid source id"),
                directory: NormalizedPackagePath::new("packages/language-v3")
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
    .expect("valid resolved language-v3 fixture");
    analyze_package(
        &input,
        &AnalysisEnvironment::default(),
        &mut QueryDatabase::new(),
    )
}

fn first_for_statement(outcome: &AnalysisOutcome) -> &TypedStatementIr {
    let ir = outcome.ir.as_ref().expect("analysis succeeds");
    for module in ir.modules() {
        for declaration in module.declarations.iter() {
            let TypedDeclarationBody::Function(function) = &declaration.body else {
                continue;
            };
            for statement in &function.body.statements {
                if matches!(
                    statement,
                    TypedStatementIr::StaticRangeFor { .. }
                        | TypedStatementIr::DynamicRangeFor { .. }
                        | TypedStatementIr::CollectionFor { .. }
                ) {
                    return statement;
                }
            }
        }
    }
    panic!("no for statement in IR");
}

#[test]
fn set_new_and_methods_type_check() {
    const SOURCE: &str = r"
pub fn run() -> i32 {
    let s: Set<i32> = Set::new();
    let inserted: bool = s.insert(1);
    let present: bool = s.contains(1);
    let removed: bool = s.remove(1);
    s.clear();
    return s.len();
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());
}

#[test]
fn set_clear_returns_unit() {
    const SOURCE: &str = r"
pub fn run() {
    let s: Set<i32> = Set::new();
    let result = s.clear();
    return;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
}

#[test]
fn static_range_for_still_lowers_to_static_range_for() {
    const SOURCE: &str = r"
pub fn run() -> i32 {
    let total: i32 = 0;
    for i in 0..4 { let total: i32 = total + i; }
    return 0;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let statement = first_for_statement(&outcome);
    let TypedStatementIr::StaticRangeFor { max_iterations, .. } = statement else {
        panic!("constant endpoints must stay StaticRangeFor: {statement:#?}");
    };
    assert_eq!(*max_iterations, 4);
}

#[test]
fn dynamic_range_for_is_accepted_and_carries_loop_limit() {
    const SOURCE: &str = r"
pub fn run(n: i32) -> i32 {
    let total: i32 = 0;
    for i in 0..n { let total: i32 = total + i; }
    return 0;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let statement = first_for_statement(&outcome);
    let TypedStatementIr::DynamicRangeFor { max_iterations, .. } = statement else {
        panic!("dynamic endpoints must lower to DynamicRangeFor: {statement:#?}");
    };
    assert!(*max_iterations > 0);
}

#[test]
fn collection_for_carries_kind_and_element_types() {
    const SOURCE: &str = r"
pub fn run(buffer: Buffer<i64>) -> i32 {
    let array: Array<i32> = Array::new();
    for item in array { let item: i32 = item; }
    let set: Set<string> = Set::new();
    for item in set { let item: string = item; }
    for item in buffer { let item: i64 = item; }
    let map: Map<string, i32> = Map::new();
    for (key, value) in map { let key: string = key; let value: i32 = value; }
    return 0;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.diagnostics().is_empty(),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("analysis succeeds");
    let mut kinds = Vec::new();
    for module in ir.modules() {
        for declaration in module.declarations.iter() {
            let TypedDeclarationBody::Function(function) = &declaration.body else {
                continue;
            };
            kinds.extend(
                function
                    .body
                    .statements
                    .iter()
                    .filter_map(|statement| match statement {
                        TypedStatementIr::CollectionFor {
                            collection,
                            key_type,
                            element_type,
                            bindings,
                            ..
                        } => Some((
                            *collection,
                            key_type.clone(),
                            element_type.clone(),
                            bindings.len(),
                        )),
                        _ => None,
                    }),
            );
        }
    }
    assert_eq!(
        kinds,
        vec![
            (CollectionIterationKindIr::Array, None, IrType::I32, 1),
            (CollectionIterationKindIr::Set, None, IrType::String, 1),
            (CollectionIterationKindIr::Buffer, None, IrType::I64, 1),
            (
                CollectionIterationKindIr::Map,
                Some(IrType::String),
                IrType::I32,
                2
            ),
        ]
    );
}

#[test]
fn single_binding_over_map_is_rejected() {
    const SOURCE: &str = r"
pub fn run() {
    let map: Map<string, i32> = Map::new();
    for entry in map { }
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("requires two bindings")),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn pair_bindings_over_set_are_rejected() {
    const SOURCE: &str = r"
pub fn run() {
    let set: Set<i32> = Set::new();
    for (key, value) in set { }
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("requires a Map iterable")),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn non_iterable_expression_is_rejected() {
    const SOURCE: &str = r"
pub fn run() {
    let value: i32 = 1;
    for item in value { }
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("not iterable")),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn direct_mutation_of_iterated_collection_is_rejected() {
    const SOURCE: &str = r"
pub fn run() -> i32 {
    let set: Set<i32> = Set::new();
    for item in set { set.insert(item); }
    return 0;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("statically provable")),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_none());
}

#[test]
fn reassignment_of_iterated_collection_is_rejected() {
    const SOURCE: &str = r"
pub fn run() -> i32 {
    let set: Set<i32> = Set::new();
    for item in set { set = Set::new(); }
    return 0;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("iterated") }),
        "{:#?}",
        outcome.diagnostics.diagnostics()
    );
}
