use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CollectionIterationKindIr,
    CompilationLimits, CompilationOptions, IrType, NormalizedPackagePath, PackageKind,
    PackageManifest, QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage,
    SourceId, SourceRole, SourceSetBuilder, TypedDeclarationBody, TypedStatementIr,
    analyze_package, call_signature_at, canonical_compilation_options, definition_at,
    source_set_fingerprint, type_at,
};

fn analyze_main(source: &str) -> AnalysisOutcome {
    analyze_main_with_options(source, CompilationOptions::default())
}

fn analyze_main_with_options(source: &str, options: CompilationOptions) -> AnalysisOutcome {
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
    let mut source_builder = SourceSetBuilder::new(manifest.id.clone(), options.limits);
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
fn generics_identity_supports_inferred_and_explicit_calls() {
    let outcome = analyze_main(
        r"
fn identity<T>(value: T) -> T {
    return value;
}

fn wrap<T>(value: T) -> Option<T> {
    return Option::Some(value);
}

fn main() -> i32 {
    let inferred = identity(10);
    let explicit = identity<i32>(20);
    let contextual: Option<i64> = wrap(30);
    return inferred + explicit;
}
",
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("generic calls produce concrete typed IR");
    let instances = ir
        .definitions()
        .iter()
        .filter(|definition| definition.name.starts_with("identity$instance$"))
        .collect::<Vec<_>>();
    assert_eq!(instances.len(), 1, "same concrete type reuses one instance");
    assert_eq!(instances[0].ty, IrType::I32);
    assert!(
        ir.definitions()
            .iter()
            .all(|definition| definition.ty != IrType::TypeParameter(0)),
        "type parameters must not escape into executable IR"
    );
}

#[test]
fn semantic_queries_expose_the_generic_declaration_and_concrete_call_signature() {
    const SOURCE: &str = r"
fn identity<T>(value: T) -> T {
    return value;
}

fn main() -> i32 {
    let result = identity(10);
    return result;
}
";
    let outcome = analyze_main(SOURCE);
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("semantic queries require typed IR");
    let source = ir
        .modules()
        .iter()
        .find(|module| module.source.path.as_str() == "src/main.nexa")
        .map(|module| module.source.clone())
        .expect("main source key");
    let call_offset = u32::try_from(SOURCE.rfind("identity(10)").expect("call exists") + 2)
        .expect("fixture offset fits u32");

    let target = definition_at(&ir, &source, call_offset).expect("resolved call target");
    let target = ir.definition(target).expect("resolved definition exists");
    assert_eq!(target.name, "identity");

    assert_eq!(type_at(&ir, &source, call_offset), Some(IrType::I32));
    let signature = call_signature_at(&ir, &source, call_offset).expect("concrete signature");
    assert_eq!(signature.name, "identity<i32>");
    assert_eq!(signature.parameters.len(), 1);
    assert_eq!(signature.parameters[0].name, "value");
    assert_eq!(signature.parameters[0].ty, IrType::I32);
    assert_eq!(signature.result, IrType::I32);
    assert_eq!(
        ir.definition(signature.declaration)
            .expect("generic declaration")
            .name,
        "identity"
    );
}

#[test]
fn generics_identity_emits_distinct_concrete_instances() {
    let outcome = analyze_main(
        r"
fn identity<T>(value: T) -> T {
    return value;
}

fn main() -> i32 {
    let integer = identity(10);
    let wide = identity<i64>(20);
    return integer;
}
",
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome.ir.expect("generic calls produce concrete typed IR");
    let instance_types = ir
        .definitions()
        .iter()
        .filter(|definition| definition.name.starts_with("identity$instance$"))
        .map(|definition| definition.ty.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(instance_types, BTreeSet::from([IrType::I32, IrType::I64]));
}

#[test]
fn generics_can_call_another_generic_function() {
    let outcome = analyze_main(
        r"
fn identity<T>(value: T) -> T {
    return value;
}

fn forward<T>(value: T) -> T {
    return identity(value);
}

fn main() -> i32 {
    return forward(42);
}
",
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("nested generic calls produce concrete IR");
    for prefix in ["identity$instance$", "forward$instance$"] {
        assert_eq!(
            ir.definitions()
                .iter()
                .filter(|definition| definition.name.starts_with(prefix))
                .count(),
            1
        );
    }
}

#[test]
fn generics_allow_same_instance_recursion_and_reject_polymorphic_recursion() {
    let recursive = analyze_main(
        r"
fn countdown<T: Copy>(value: T, remaining: i32) -> T {
    if remaining <= 0 {
        return value;
    }
    return countdown(value, remaining - 1);
}

fn main() -> i32 {
    return countdown(42, 3);
}
",
    );
    assert!(
        recursive.diagnostics.is_empty(),
        "{:?}",
        recursive.diagnostics.diagnostics()
    );
    let ir = recursive.ir.expect("same generic instance may recurse");
    assert_eq!(
        ir.definitions()
            .iter()
            .filter(|definition| definition.name.starts_with("countdown$instance$"))
            .count(),
        1
    );

    let polymorphic = analyze_main(
        r"
fn expand<T>(value: T) -> i32 {
    return expand<Array<T>>([value]);
}

fn main() -> i32 {
    return expand(1);
}
",
    );
    assert!(
        polymorphic
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("generic function instantiation does not converge")),
        "{:?}",
        polymorphic.diagnostics.diagnostics()
    );
}

#[test]
fn generics_enforce_shared_instance_and_instantiation_depth_limits() {
    let one_instance = CompilationOptions {
        limits: CompilationLimits {
            max_generic_instances: 1,
            ..CompilationLimits::default()
        },
        ..CompilationOptions::default()
    };
    let too_many = analyze_main_with_options(
        r"
fn identity<T>(value: T) -> T {
    return value;
}

fn main() -> i32 {
    let integer = identity(1);
    let wide = identity<i64>(2);
    return integer;
}
",
        one_instance,
    );
    assert!(
        too_many
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("generic instance limit exceeded")),
        "{:?}",
        too_many.diagnostics.diagnostics()
    );

    let two_levels = CompilationOptions {
        limits: CompilationLimits {
            max_generic_instantiation_depth: 2,
            ..CompilationLimits::default()
        },
        ..CompilationOptions::default()
    };
    let too_deep = analyze_main_with_options(
        r"
fn third<T>(value: T) -> T {
    return value;
}

fn second<T>(value: T) -> T {
    return third(value);
}

fn first<T>(value: T) -> T {
    return second(value);
}

fn main() -> i32 {
    return first(42);
}
",
        two_levels,
    );
    assert!(
        too_deep
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("generic instantiation depth limit exceeded")),
        "{:?}",
        too_deep.diagnostics.diagnostics()
    );
}

#[test]
fn generics_report_type_argument_arity_and_inference_conflicts() {
    let wrong_arity = analyze_main(
        r"
fn pair<T, U>(left: T, right: U) -> T {
    return left;
}

fn main() -> i32 {
    return pair<i32>(1, 2);
}
",
    );
    assert!(
        wrong_arity
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| {
                diagnostic
                    .message
                    .contains("expected 2 type arguments for `pair`, found 1")
            })
    );

    let conflict = analyze_main(
        r#"
fn same<T>(left: T, right: T) -> T {
    return left;
}

fn main() -> i32 {
    return same(1, "two");
}
"#,
    );
    assert!(conflict.diagnostics.diagnostics().iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("conflicts with an earlier type inference")
    }));
}

#[test]
fn generics_builtin_bounds_enable_abstract_operations_and_check_instances() {
    let outcome = analyze_main(
        r#"
fn equal<T: PartialEq>(left: T, right: T) -> bool {
    return left == right;
}

fn smaller<T>(left: T, right: T) -> T
where
    T: Copy + PartialOrd,
{
    if left < right {
        return left;
    }
    return right;
}

fn show<T: Display>(value: T) -> string {
    return value.to_string();
}

fn main() -> i32 {
    let text = show(10);
    if equal(text, "10") {
        return smaller(20, 30);
    }
    return 1;
}
"#,
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());

    let missing_bound = analyze_main(
        r"
fn invalid<T>(left: T, right: T) -> bool {
    return left == right;
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        missing_bound
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("missing `PartialEq` or `Eq`") })
    );

    let unsatisfied = analyze_main(
        r"
fn ordered<T: PartialOrd>(left: T, right: T) -> bool {
    return left < right;
}

fn main() -> i32 {
    let values = [1, 2];
    if ordered(values, values) {
        return 1;
    }
    return 0;
}
",
    );
    assert!(
        unsatisfied
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.message.contains("does not satisfy `PartialOrd`") })
    );
}

#[test]
fn generics_operator_and_nominal_type_bounds_are_closed_and_checked() {
    let valid = analyze_main(
        r#"
struct LookupTable<K, V>
where
    K: Eq + Hash,
{
    values: Map<K, V>,
}

struct WrappedLookup<K, V>
where
    K: Eq + Hash,
{
    table: LookupTable<K, V>,
}

fn add<T>(left: T, right: T) -> T
where
    T: Add<Output = T>,
{
    return left + right;
}

fn negate<T: Neg<Output = T>>(value: T) -> T {
    return -value;
}

fn equal<T: PartialEq>(left: T, right: T) -> bool {
    return left == right;
}

fn equal_ordered<T: Eq>(left: T, right: T) -> bool {
    return equal(left, right);
}

fn main() -> i32 {
    let table: LookupTable<string, i32> = LookupTable {
        values: Map::new(),
    };
    if equal_ordered("same", "same") {
        return add(negate(-20), 22) + table.values.len();
    }
    return 0;
}
"#,
    );
    assert!(
        valid.diagnostics.is_empty(),
        "{:?}",
        valid.diagnostics.diagnostics()
    );

    let missing_map_bounds = analyze_main(
        r"
struct LookupTable<K, V> {
    values: Map<K, V>,
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        missing_map_bounds
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("does not have the bounds required by `Map<K, V>`")),
        "{:?}",
        missing_map_bounds.diagnostics.diagnostics()
    );

    let unsatisfied = analyze_main(
        r"
fn add<T: Add<Output = T>>(left: T, right: T) -> T {
    return left + right;
}

fn main() -> bool {
    return add(true, false);
}
",
    );
    assert!(
        unsatisfied
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("does not satisfy `Add`")),
        "{:?}",
        unsatisfied.diagnostics.diagnostics()
    );
}

#[test]
fn generics_numeric_receiver_method_surface_type_checks() {
    let outcome = analyze_main(
        r#"
fn integer_methods(value: i32) -> i32 {
    return value.abs().min(20).max(5).clamp(0, 15);
}

fn wide_methods(value: i64) -> i64 {
    return value.abs().min(30).max(10).clamp(0, 25);
}

fn single_methods(value: f32) -> f32 {
    return value.abs().min(5.0).max(1.0).clamp(0.0, 4.0)
        + value.floor() + value.ceil() + value.round()
        + value.sqrt() + value.sin() + value.cos();
}

fn double_methods(value: f64) -> f64 {
    return value.abs().min(3.0).max(1.0).clamp(0.0, 3.0)
        + value.floor() + value.ceil() + value.round()
        + value.sqrt() + value.sin() + value.cos();
}

fn main() -> i32 {
    let integer = integer_methods(-10);
    let wide = wide_methods(20);
    let single = single_methods(4.0);
    let double = double_methods(2.75);
    let text = integer.to_string();
    if wide > 0 && single > 0.0 && double > 0.0 && text == "15" {
        return integer;
    }
    return 0;
}
"#,
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    assert!(outcome.ir.is_some());

    let removed_static_surface = analyze_main(
        r"
use std::math as math;

fn main() -> i32 {
    return math::abs_i32(-1);
}
",
    );
    assert!(
        removed_static_surface
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("was replaced by receiver method `value.abs()`")),
        "{:?}",
        removed_static_surface.diagnostics.diagnostics()
    );
}

#[test]
fn generics_nominal_types_support_struct_enum_and_class_vertical_slices() {
    let outcome = analyze_main(
        r#"
struct Holder<T> {
    value: T,
}

struct Pair<T, U> {
    first: T,
    second: U,
}

enum Maybe<T> {
    None,
    Some(T),
}

class Box<T> {
    value: T,
}

fn unwrap<T>(holder: Holder<T>) -> T {
    return holder.value;
}

fn unwrap_or<T>(value: Maybe<T>, fallback: T) -> T {
    return match value {
        Maybe::Some(payload) => payload,
        Maybe::None => fallback,
    };
}

fn main() -> i32 {
    let holder = Holder { value: 10 };
    let explicit = Holder<i32> { value: 20 };
    let pair = Pair { first: "score", second: 100 };
    let updated = Pair<string, i32> { second: 200, ..pair };
    let present = Maybe::Some(30);
    let absent: Maybe<i32> = Maybe::None;
    let explicit_present = Maybe<i32>::Some(50);
    let explicit_absent = Maybe<i32>::None;
    let boxed = Box { value: 40 };

    return unwrap(holder)
        + unwrap(explicit)
        + unwrap_or(present, 0)
        + unwrap_or(absent, 0)
        + unwrap_or(explicit_present, 0)
        + unwrap_or(explicit_absent, 0)
        + updated.second
        + boxed.value;
}
"#,
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("generic nominal types lower to concrete typed IR");
    for instance in ["Holder<i32>", "Pair<string, i32>", "Maybe<i32>", "Box<i32>"] {
        assert_eq!(
            ir.definitions()
                .iter()
                .filter(|definition| definition.name == instance)
                .count(),
            1,
            "repeated uses must reuse `{instance}`"
        );
    }
}

#[test]
fn generics_named_enum_variants_infer_and_accept_explicit_arguments() {
    let outcome = analyze_main(
        r"
enum Event<T> {
    Payload { value: T },
}

fn main() -> i32 {
    let inferred = Event::Payload { value: 20 };
    let explicit = Event<i32>::Payload { value: 22 };
    return match inferred {
        Event::Payload { value } => value,
    } + match explicit {
        Event::Payload { value } => value,
    };
}
",
    );
    assert!(
        outcome.diagnostics.is_empty(),
        "{:?}",
        outcome.diagnostics.diagnostics()
    );
    outcome
        .ir
        .expect("named generic Enum variants lower to concrete IR");
}

#[test]
fn generics_reject_non_converging_and_inline_recursive_nominal_layouts() {
    let recursive_class = analyze_main(
        r"
class Node<T> {
    value: T,
    next: Option<Node<T>>,
}

fn consume(value: Node<i32>) -> i32 {
    return value.value;
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        recursive_class.diagnostics.is_empty(),
        "{:?}",
        recursive_class.diagnostics.diagnostics()
    );

    let inline = analyze_main(
        r"
struct Node<T> {
    value: T,
    next: Node<T>,
}

fn consume(value: Node<i32>) -> i32 {
    return value.value;
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        inline
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("recursive inline value layout")),
        "{:?}",
        inline.diagnostics.diagnostics()
    );

    let expanding = analyze_main(
        r"
class Expanding<T> {
    next: Expanding<Array<T>>,
}

fn consume(value: Expanding<i32>) -> i32 {
    return 0;
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        expanding
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("generic type instantiation does not converge")),
        "{:?}",
        expanding.diagnostics.diagnostics()
    );

    let generic_state = analyze_main(
        r"
@state(version = 1)
class Store<T> {
    value: T,
}

fn main() -> i32 {
    return 0;
}
",
    );
    assert!(
        generic_state
            .diagnostics
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic
                .message
                .contains("generic @state Class declarations are not supported")),
        "{:?}",
        generic_state.diagnostics.diagnostics()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn generic_parameter_names_do_not_change_public_api_identity() {
    let first = analyze_main(
        r"
pub struct Pair<T, U> {
    first: T,
    second: U,
}

pub fn identity<T: Copy>(value: T) -> T {
    return value;
}
",
    );
    let renamed = analyze_main(
        r"
pub struct Pair<First, Second> {
    first: First,
    second: Second,
}

pub fn identity<Value: Copy>(value: Value) -> Value {
    return value;
}
",
    );
    assert!(first.diagnostics.is_empty());
    assert!(renamed.diagnostics.is_empty());
    assert_eq!(
        first.public_api_fingerprint, renamed.public_api_fingerprint,
        "generic parameter spelling is not semantic identity"
    );

    let changed_bound = analyze_main(
        r"
pub struct Pair<T: Copy, U> {
    first: T,
    second: U,
}

pub fn identity<T: Eq>(value: T) -> T {
    return value;
}
",
    );
    assert!(changed_bound.diagnostics.is_empty());
    assert_ne!(
        first.public_api_fingerprint, changed_bound.public_api_fingerprint,
        "generic bounds participate in public API identity"
    );

    let instantiated_first = analyze_main(
        r"
struct Holder<T> {
    value: T,
}

fn identity<T>(value: T) -> T {
    return value;
}

fn main() -> i32 {
    let holder = Holder { value: 42 };
    return identity(holder.value);
}
",
    )
    .ir
    .expect("first generic instance");
    let instantiated_renamed = analyze_main(
        r"
struct Holder<Value> {
    value: Value,
}

fn identity<Value>(value: Value) -> Value {
    return value;
}

fn main() -> i32 {
    let holder = Holder { value: 42 };
    return identity(holder.value);
}
",
    )
    .ir
    .expect("renamed generic instance");
    let instance_id = |ir: &nexa_analysis::TypedPackageIr| {
        ir.definitions()
            .iter()
            .find(|definition| definition.name.starts_with("identity$instance$"))
            .and_then(|definition| definition.stable_symbol.as_ref())
            .map(|identity| identity.runtime_id)
            .expect("concrete instance has stable identity")
    };
    assert_eq!(
        instance_id(&instantiated_first),
        instance_id(&instantiated_renamed),
        "renaming a type parameter must not perturb concrete instance identity"
    );
    let type_instance_id = |ir: &nexa_analysis::TypedPackageIr| {
        ir.definitions()
            .iter()
            .find(|definition| definition.name == "Holder<i32>")
            .and_then(|definition| definition.stable_symbol.as_ref())
            .map(|identity| identity.runtime_id)
            .expect("concrete generic type has stable identity")
    };
    assert_eq!(
        type_instance_id(&instantiated_first),
        type_instance_id(&instantiated_renamed),
        "renaming a type parameter must not perturb concrete type identity"
    );
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
