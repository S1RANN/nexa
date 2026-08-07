use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, BuildFingerprintInput, CompilationOptions, DependencyAlias,
    DependencyEdge, HostContractSurface, HostFunctionMode, HostFunctionSurface, LockFile,
    NormalizedPackagePath, PackageId, PackageManifest, QueryDatabase, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole, SourceSetBuilder, SurfaceType,
    analyze_package, source_set_fingerprint,
};
use nexa_bytecode::{FunctionEffect, Instruction, StandardIntrinsic, ValueType};
use nexa_compiler::PackageCompileOutput;
use nexa_core::{CanonicalSymbolIdentity, FileId, StableId, SymbolKind};
use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, FrameLimits, FuelState, Heap, HostTrap,
    InterpreterHost, InterpreterHostArguments, InterpreterHostOutcome, InterpreterOutcome,
    OpcodeCostTable, RuntimeValue, TrapKind,
};

const EVIDENCE_PACKAGE: &str = "nexa.compiler.evidence";
const EVIDENCE_MODULE: &str = "compiler.evidence";

fn compile_typed_evidence_package(source: &str) -> PackageCompileOutput {
    compile_typed_evidence_package_with_environment(source, &AnalysisEnvironment::default())
}

fn compile_typed_evidence_package_with_environment(
    source: &str,
    environment: &AnalysisEnvironment,
) -> PackageCompileOutput {
    let compilation_options = CompilationOptions::default();
    let package = PackageId::new(EVIDENCE_PACKAGE).expect("valid evidence package ID");
    let manifest = Arc::new(
        PackageManifest::parse(&format!(
            r#"
schema = 2
kind = "application"
id = "{EVIDENCE_PACKAGE}"
name = "Compiler evidence"
version = "1.0.0"
source_root = "src"
entry = "{EVIDENCE_MODULE}"
activation = "default-enabled"
"#
        ))
        .expect("valid evidence manifest"),
    );
    let mut source_set = SourceSetBuilder::new(package.clone(), compilation_options.limits);
    source_set
        .add(
            NormalizedPackagePath::new("src/compiler/evidence.nexa")
                .expect("normalized evidence source path"),
            Arc::<str>::from(source),
            SourceRole::Production,
        )
        .expect("valid evidence source");
    let source_set = Arc::new(source_set.build().expect("valid evidence source set"));
    let graph = Arc::new(ResolvedDependencyGraph {
        root: package.clone(),
        packages: BTreeMap::from([(
            package.clone(),
            ResolvedPackage {
                id: package.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("compiler-evidence").expect("valid evidence source ID"),
                directory: NormalizedPackagePath::new("virtual/compiler-evidence")
                    .expect("normalized evidence package path"),
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint = BuildFingerprintInput {
        root_package: package,
        root_manifest: manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&source_set),
        dependency_manifests: BTreeMap::new(),
        dependency_source_sets: BTreeMap::new(),
        host_contract: Vec::new(),
        contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
        host_contract_source: Vec::new(),
        host_required_entrypoints: nexa_contract::required_entrypoints_descriptor(
            std::iter::empty::<&str>(),
        ),
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
        manifest,
        source_set,
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
    .expect("valid resolved evidence input");
    let mut queries = QueryDatabase::new();
    let outcome = analyze_package(&input, environment, &mut queries);
    assert!(
        outcome.diagnostics.is_empty(),
        "evidence source must analyze without diagnostics: {:?}",
        outcome.diagnostics.diagnostics()
    );
    let ir = outcome
        .ir
        .expect("successful evidence analysis emits typed IR");
    nexa_compiler::compile_typed_package(&ir).expect("evidence typed IR compiles")
}

#[allow(clippy::too_many_lines)]
fn compile_dependency_host_evidence(environment: &AnalysisEnvironment) -> PackageCompileOutput {
    const ROOT: &str = "nexa.compiler.host.root";
    const DEPENDENCY: &str = "nexa.compiler.host.dependency";
    let compilation_options = CompilationOptions::default();
    let root = PackageId::new(ROOT).expect("valid root package ID");
    let dependency = PackageId::new(DEPENDENCY).expect("valid dependency package ID");
    let root_manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "application"
id = "nexa.compiler.host.root"
name = "Compiler Host Root"
version = "1.0.0"
source_root = "src"
entry = "app.main"
activation = "default-enabled"

[dependencies]
host_dependency = { path = "../dependency" }
"#,
        )
        .expect("valid root manifest"),
    );
    let dependency_manifest = Arc::new(
        PackageManifest::parse(
            r#"
schema = 2
kind = "library"
id = "nexa.compiler.host.dependency"
name = "Compiler Host Dependency"
version = "1.0.0"
source_root = "src"
"#,
        )
        .expect("valid dependency manifest"),
    );
    let mut root_sources = SourceSetBuilder::new(root.clone(), compilation_options.limits);
    root_sources
        .add(
            NormalizedPackagePath::new("src/app/main.nexa").expect("normalized root path"),
            Arc::<str>::from(
                r"
use host_dependency::api as dependency;

fn run() -> i32 {
    return dependency::read();
}
",
            ),
            SourceRole::Production,
        )
        .expect("valid root source");
    let root_sources = Arc::new(root_sources.build().expect("valid root source set"));
    let mut dependency_sources =
        SourceSetBuilder::new(dependency.clone(), compilation_options.limits);
    dependency_sources
        .add(
            NormalizedPackagePath::new("src/api.nexa").expect("normalized dependency path"),
            Arc::<str>::from(
                r"
pub fn read() -> i32 {
    return host::third();
}
",
            ),
            SourceRole::Production,
        )
        .expect("valid dependency source");
    let dependency_sources = Arc::new(
        dependency_sources
            .build()
            .expect("valid dependency source set"),
    );
    let source_id = SourceId::new("compiler-host-evidence").expect("valid source ID");
    let graph = Arc::new(ResolvedDependencyGraph {
        root: root.clone(),
        packages: BTreeMap::from([
            (
                root.clone(),
                ResolvedPackage {
                    id: root.clone(),
                    version: root_manifest.version.clone(),
                    source_id: source_id.clone(),
                    directory: NormalizedPackagePath::new("virtual/root")
                        .expect("normalized root directory"),
                    kind: root_manifest.kind,
                },
            ),
            (
                dependency.clone(),
                ResolvedPackage {
                    id: dependency.clone(),
                    version: dependency_manifest.version.clone(),
                    source_id,
                    directory: NormalizedPackagePath::new("virtual/dependency")
                        .expect("normalized dependency directory"),
                    kind: dependency_manifest.kind,
                },
            ),
        ]),
        edges: BTreeSet::from([DependencyEdge {
            from: root.clone(),
            alias: DependencyAlias::new("host_dependency").expect("valid dependency alias"),
            to: dependency.clone(),
        }]),
    });
    let lock = Arc::new(LockFile::from_graph(&graph));
    let dependency_manifests =
        BTreeMap::from([(dependency.clone(), Arc::clone(&dependency_manifest))]);
    let dependency_source_sets =
        BTreeMap::from([(dependency.clone(), Arc::clone(&dependency_sources))]);
    let host_contract = b"compiler-host-subset-v2".to_vec();
    let host_contract_source = b"virtual/dispatch-host.nidl\0compiler-host-subset-v2".to_vec();
    let host_required_entrypoints =
        nexa_contract::required_entrypoints_descriptor(std::iter::empty::<&str>());
    let fingerprint = BuildFingerprintInput {
        root_package: root,
        root_manifest: root_manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&root_sources),
        dependency_manifests: BTreeMap::from([(
            dependency.clone(),
            dependency_manifest.canonical_bytes(),
        )]),
        dependency_source_sets: BTreeMap::from([(
            dependency,
            source_set_fingerprint(&dependency_sources),
        )]),
        host_contract: host_contract.clone(),
        contract_syntax_version: nexa_contract::CONTRACT_SYNTAX_VERSION,
        host_contract_source: host_contract_source.clone(),
        host_required_entrypoints: host_required_entrypoints.clone(),
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
        canonical_lock_graph: lock.canonical_bytes(),
    };
    let input = ResolvedBuildInput::new(
        root_manifest,
        root_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        Some(lock),
        host_contract,
        host_contract_source,
        host_required_entrypoints,
        compilation_options,
        fingerprint,
    )
    .expect("valid dependency Host build input");
    let mut queries = QueryDatabase::new();
    let outcome = analyze_package(&input, environment, &mut queries);
    assert!(
        outcome.diagnostics.is_empty(),
        "dependency Host source must analyze without diagnostics: {:?}",
        outcome.diagnostics.diagnostics()
    );
    nexa_compiler::compile_typed_package(
        &outcome
            .ir
            .expect("successful dependency analysis emits typed IR"),
    )
    .expect("dependency Host Typed IR compiles")
}

fn assert_defer_cleanup_evidence(compiled: &PackageCompileOutput) {
    let (owner_index, cleanup_index, capture_base, capture_count) = compiled
        .module
        .functions
        .iter()
        .enumerate()
        .find_map(|(owner, function)| {
            function.code.iter().find_map(|instruction| {
                let Instruction::DeferPush {
                    function,
                    args_base,
                    args_count,
                } = instruction
                else {
                    return None;
                };
                Some((
                    u32::try_from(owner).expect("function index fits u32"),
                    *function,
                    *args_base,
                    *args_count,
                ))
            })
        })
        .expect("the Task body emits DeferPush");
    assert_eq!(capture_count, 1, "the defer captures exactly `value`");

    let work_debug = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| function.function_index == owner_index)
        .expect("DeferPush owner has debug metadata");
    assert_eq!(work_debug.name, "work");
    assert_eq!(work_debug.effect, FunctionEffect::Task);

    let cleanup_debug = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| function.function_index == cleanup_index)
        .expect("DeferPush target has debug metadata");
    let expected_identity = CanonicalSymbolIdentity::automatic(
        EVIDENCE_PACKAGE,
        EVIDENCE_MODULE,
        SymbolKind::Function,
        "__defer_work_0",
    );
    assert_eq!(cleanup_debug.name, "__defer_work_0");
    assert_eq!(cleanup_debug.effect, FunctionEffect::Cleanup);
    assert_eq!(cleanup_debug.canonical_identity, expected_identity);
    assert_eq!(cleanup_debug.stable_id, expected_identity.runtime_id());

    let cleanup = &compiled.module.functions[usize::try_from(cleanup_index).unwrap()];
    assert_eq!(cleanup.effect, FunctionEffect::Cleanup);
    assert_eq!(cleanup.signature.parameters, [ValueType::I32]);
    let finalize_index = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| function.name == "finalize")
        .expect("finalize has debug metadata")
        .function_index;
    assert!(
        cleanup.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::Call {
                function,
                args_count: 1,
                ..
            } if *function == finalize_index
        )),
        "the analyzer-generated cleanup body calls finalize with its captured value"
    );
    assert_eq!(cleanup.code.last(), Some(&Instruction::CleanupReturn));
    assert!(
        compiled.module.functions[usize::try_from(owner_index).unwrap()]
            .code
            .iter()
            .any(|instruction| matches!(
                instruction,
                Instruction::DeferPush {
                    function,
                    args_base,
                    args_count: 1,
                } if *function == cleanup_index && *args_base == capture_base
            ))
    );
}

#[test]
fn virtual_snippet_executes_typed_arithmetic_without_a_module_declaration() {
    let verified = nexa_compiler::compile(
        r"
fn add(left: i32, right: i32) -> i32 {
    return left + right;
}
",
    )
    .unwrap();
    let outcome = CheckedInterpreter::run(
        &verified,
        0,
        &[RuntimeValue::I32(20), RuntimeValue::I32(22)],
        100,
    )
    .unwrap();
    assert!(matches!(
        outcome,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(42)),
            ..
        }
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn unit_runtime_values_preserve_return_parameter_container_and_async_semantics() {
    let compiled = compile_typed_evidence_package(
        r"
fn noop() -> unit {}

fn consume(value: unit) -> i32 {
    return 17;
}

fn via_return() -> unit {
    return noop();
}

fn via_parameter() -> i32 {
    return consume(noop());
}

fn option_unit() -> Option<unit> {
    return Option::Some(noop());
}

fn array_unit() -> Array<unit> {
    return [noop()];
}

async fn unit_task() -> unit {
    yield;
    return noop();
}

async fn await_unit() -> i32 {
    let value: unit = unit_task().await;
    return consume(value);
}
",
    );
    let function_index = |name: &str| {
        compiled
            .debug_info
            .functions
            .iter()
            .find(|function| function.package_id == EVIDENCE_PACKAGE && function.name == name)
            .unwrap_or_else(|| panic!("missing debug metadata for `{name}`"))
            .function_index
    };
    let via_return = function_index("via_return");
    let via_parameter = function_index("via_parameter");
    let option_unit = function_index("option_unit");
    let array_unit = function_index("array_unit");
    let await_unit = function_index("await_unit");

    let via_return_code = &compiled.module.functions
        [usize::try_from(via_return).expect("function index fits usize")]
    .code;
    assert_eq!(
        via_return_code.last(),
        Some(&Instruction::ReturnVoid),
        "returning a Unit expression must execute it and return without a bytecode result"
    );

    let verified = nexa_verifier::verify(compiled.module, nexa_verifier::VerifierLimits::default())
        .expect("Unit runtime-value bytecode verifies");

    let returned = CheckedInterpreter::run(&verified, via_parameter, &[], 1_000)
        .expect("Unit parameter program executes");
    assert!(matches!(
        returned,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(17)),
            ..
        }
    ));

    let returned = CheckedInterpreter::run(&verified, via_return, &[], 1_000)
        .expect("Unit return program executes");
    assert!(matches!(
        returned,
        InterpreterOutcome::Returned { value: None, .. }
    ));

    let mut heap = Heap::new(64);
    let option = CheckedInterpreter::run_with_heap(&verified, option_unit, &[], 1_000, &mut heap)
        .expect("Option<Unit> program executes");
    let option = match option {
        InterpreterOutcome::Returned {
            value: Some(value), ..
        } => value,
        other => panic!("Option<Unit> must return a value, got {other:?}"),
    };
    assert_eq!(
        heap.enum_parts(option)
            .expect("Option<Unit> is represented as an enum")
            .3,
        Some(RuntimeValue::I32(0))
    );

    let array = CheckedInterpreter::run_with_heap(&verified, array_unit, &[], 1_000, &mut heap)
        .expect("Array<Unit> program executes");
    let array = match array {
        InterpreterOutcome::Returned {
            value: Some(value), ..
        } => value,
        other => panic!("Array<Unit> must return a value, got {other:?}"),
    };
    assert_eq!(
        heap.array_get(array, 0).expect("Array<Unit> has one value"),
        RuntimeValue::I32(0)
    );

    let suspended =
        CheckedInterpreter::run(&verified, await_unit, &[], 1_000).expect("Task<Unit> starts");
    let (continuation, fuel) = match suspended {
        InterpreterOutcome::Suspended {
            continuation, fuel, ..
        } => (continuation, fuel),
        other => panic!("Task<Unit> must suspend at yield, got {other:?}"),
    };
    let resumed =
        CheckedInterpreter::poll(&verified, continuation, fuel, &OpcodeCostTable::default())
            .expect("Task<Unit> resumes");
    assert!(matches!(
        resumed,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(17)),
            ..
        }
    ));
}

#[derive(Default)]
struct UnitHost {
    calls: usize,
}

impl InterpreterHost for UnitHost {
    fn call(
        &mut self,
        import: u32,
        arguments: InterpreterHostArguments<'_>,
        _heap: Option<&mut Heap>,
    ) -> Result<InterpreterHostOutcome, HostTrap> {
        if import != 0 {
            return Err(HostTrap::Host(nexa_runtime::RuntimeMessage::inline(
                &format!("unexpected module-local Host import slot {import}"),
            )));
        }
        if !arguments.is_empty() {
            return Err(HostTrap::Arity);
        }
        self.calls += 1;
        Ok(InterpreterHostOutcome::Immediate(RuntimeValue::Unit))
    }
}

#[test]
fn synchronous_unit_host_results_materialize_only_after_the_host_call() {
    let host_contract = StableId::from_name("compiler-evidence-unit-host");
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "UnitHost".into(),
            contract_stable_id: host_contract,
            types: Vec::new(),
            functions: vec![HostFunctionSurface {
                name: "touch".into(),
                parameters: Vec::new(),
                result: SurfaceType::Unit,
                mode: HostFunctionMode::Sync,
                stable_id: StableId::from_name("compiler-evidence-unit-host.touch"),
                declaration_fingerprint: [1; 32],
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
        static_modules: Vec::new(),
    };
    let compiled = compile_typed_evidence_package_with_environment(
        r"
fn consume(value: unit) -> i32 {
    return 23;
}

fn host_unit() -> i32 {
    return consume(host::touch());
}
",
        &environment,
    );
    let consume = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| function.package_id == EVIDENCE_PACKAGE && function.name == "consume")
        .expect("consume has debug metadata")
        .function_index;
    let host_unit = compiled
        .debug_info
        .functions
        .iter()
        .find(|function| function.package_id == EVIDENCE_PACKAGE && function.name == "host_unit")
        .expect("host_unit has debug metadata")
        .function_index;
    let code = &compiled.module.functions
        [usize::try_from(host_unit).expect("function index fits usize")]
    .code;
    assert!(
        code.windows(3).any(|instructions| matches!(
            instructions,
            [
                Instruction::HostCall { import: 0, .. },
                Instruction::LoadI32 { value: 0, .. },
                Instruction::Call { function, .. }
            ] if *function == consume
        )),
        "the Unit sentinel must be materialized after the Host call and before its consumer"
    );

    let verified = nexa_verifier::verify(compiled.module, nexa_verifier::VerifierLimits::default())
        .expect("synchronous Unit Host-call bytecode verifies");
    assert_eq!(verified.module().host_contract_id, Some(host_contract));
    let limits = FrameLimits::default();
    let continuation = CheckedInterpreter::start(
        &verified,
        host_unit,
        &[],
        limits,
        ContinuationReservation::for_limits(limits),
    )
    .expect("Unit Host-call program starts");
    let mut host = UnitHost::default();
    let outcome = CheckedInterpreter::poll_with_host(
        &verified,
        continuation,
        FuelState::new(1_000, 0, u64::MAX),
        &OpcodeCostTable::default(),
        &mut host,
    )
    .expect("Unit Host-call program executes");
    assert_eq!(host.calls, 1);
    assert!(matches!(
        outcome,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(23)),
            ..
        }
    ));
}

#[test]
fn unused_generic_standard_templates_are_not_emitted_as_bytecode_functions() {
    let compiled = compile_typed_evidence_package(
        r"
pub fn value() -> i32 {
    return 7;
}
",
    );
    assert!(
        compiled
            .debug_info
            .functions
            .iter()
            .any(|function| function.package_id == EVIDENCE_PACKAGE && function.name == "value")
    );
    assert_eq!(
        compiled.module.functions.len(),
        1,
        "unused embedded-source standard-library functions must also be removed from bytecode"
    );
    for template in [
        "is_some",
        "option_unwrap_or",
        "array_get",
        "array_push",
        "map_get",
        "map_insert",
        "trap",
    ] {
        assert!(
            !compiled.debug_info.functions.iter().any(|function| {
                function.package_id == nexa_stdlib::PACKAGE_ID && function.name == template
            }),
            "generic intrinsic template `{template}` must not become a bytecode function"
        );
    }
}

const NUMERIC_AND_SCALAR_TEXT_SOURCE: &str = r#"
fn numeric(a: i32, b: i64, c: f32, d: f64) -> bool {
    let i: i32 = -a + 2;
    let j: i64 = -b * 3;
    let x: f32 = -c / 2.0;
    let y: f64 = -d % 3.0;
    return i < 3 && j <= 4 && x > 0.0 && y >= 0.0;
}

fn scalar_text(
    text: string,
    i: i32,
    j: i64,
    x: f32,
    y: f64,
    flag: bool,
    glyph: rune,
) -> string {
    return "${text}:${i}:${j}:${x}:${y}:${flag}:${glyph}";
}
"#;

#[test]
fn typed_numeric_domains_and_string_builder_lower_exhaustively() {
    let verified = nexa_compiler::compile(NUMERIC_AND_SCALAR_TEXT_SOURCE).unwrap();
    let reference = nexa_compiler::compile_reference(NUMERIC_AND_SCALAR_TEXT_SOURCE).unwrap();
    let instructions = verified
        .module()
        .functions
        .iter()
        .flat_map(|function| &function.code)
        .collect::<Vec<_>>();

    for expected in [
        "i32 arithmetic",
        "i64 arithmetic",
        "f32 arithmetic",
        "f64 arithmetic",
    ] {
        let present = match expected {
            "i32 arithmetic" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::Add { .. })),
            "i64 arithmetic" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::MulI64 { .. })),
            "f32 arithmetic" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::DivF32 { .. })),
            "f64 arithmetic" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::RemF64 { .. })),
            _ => false,
        };
        assert!(present, "missing {expected}");
    }
    assert_eq!(
        instructions
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::StringBuild { .. }))
            .count(),
        1
    );
    assert!(!instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StringConcat { .. }
            | Instruction::I32ToString { .. }
            | Instruction::I64ToString { .. }
            | Instruction::F32ToString { .. }
            | Instruction::F64ToString { .. }
            | Instruction::BoolToString { .. }
            | Instruction::RuneToString { .. }
            | Instruction::StringToString { .. }
    )));

    let reference_instructions = reference
        .module()
        .functions
        .iter()
        .flat_map(|function| &function.code)
        .collect::<Vec<_>>();
    for conversion in ["i32", "i64", "f32", "f64", "bool", "rune", "string"] {
        let present = reference_instructions
            .iter()
            .any(|instruction| match conversion {
                "i32" => matches!(instruction, Instruction::I32ToString { .. }),
                "i64" => matches!(instruction, Instruction::I64ToString { .. }),
                "f32" => matches!(instruction, Instruction::F32ToString { .. }),
                "f64" => matches!(instruction, Instruction::F64ToString { .. }),
                "bool" => matches!(instruction, Instruction::BoolToString { .. }),
                "rune" => matches!(instruction, Instruction::RuneToString { .. }),
                "string" => matches!(instruction, Instruction::StringToString { .. }),
                _ => false,
            });
        assert!(
            present,
            "reference lowering is missing {conversion} conversion"
        );
    }
    assert!(
        reference_instructions
            .iter()
            .any(|instruction| matches!(instruction, Instruction::StringConcat { .. }))
    );
}

#[test]
fn string_builder_formats_all_scalar_domains_and_publishes_once() {
    let verified = nexa_compiler::compile(NUMERIC_AND_SCALAR_TEXT_SOURCE).unwrap();
    let signature = vec![
        ValueType::String,
        ValueType::I32,
        ValueType::I64,
        ValueType::F32,
        ValueType::F64,
        ValueType::Bool,
        ValueType::Rune,
    ];
    let function = verified
        .module()
        .functions
        .iter()
        .position(|function| function.signature.parameters == signature)
        .expect("optimized scalar_text function");
    let mut heap = Heap::new(128);
    let text = heap.allocate_string("prefix").unwrap();
    for literal in &verified.module().strings {
        heap.load_string_literal(literal).unwrap();
    }
    let before = heap.vm_allocation_counters().string_allocations;
    let outcome = CheckedInterpreter::run_with_heap(
        &verified,
        u32::try_from(function).unwrap(),
        &[
            RuntimeValue::String {
                reference: text,
                hash: heap.string_hash(text).unwrap(),
            },
            RuntimeValue::I32(-7),
            RuntimeValue::I64(42),
            RuntimeValue::F32(1.25_f32.to_bits()),
            RuntimeValue::F64((-2.5_f64).to_bits()),
            RuntimeValue::Bool(true),
            RuntimeValue::Rune(u32::from('界')),
        ],
        10_000,
        &mut heap,
    )
    .unwrap();
    let InterpreterOutcome::Returned {
        value: Some(RuntimeValue::String { reference, .. }),
        ..
    } = outcome
    else {
        panic!("optimized string builder must return a string");
    };
    assert_eq!(
        heap.string(reference).unwrap(),
        "prefix:-7:42:1.25:-2.5:true:界"
    );
    assert_eq!(
        heap.vm_allocation_counters().string_allocations - before,
        1,
        "optimized interpolation must publish exactly one owned result string"
    );
}

#[test]
fn chained_string_concat_fuses_to_one_builder_and_one_result_allocation() {
    let source = r"
fn chain(first: string, second: string, third: string) -> string {
    return first + second + third;
}
";
    let verified = nexa_compiler::compile(source).unwrap();
    let code = &verified.module().functions[0].code;
    assert_eq!(
        code.iter()
            .filter(|instruction| matches!(instruction, Instruction::StringBuild { .. }))
            .count(),
        1
    );
    assert!(
        !code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::StringConcat { .. }))
    );

    let reference = nexa_compiler::compile_reference(source).unwrap();
    assert_eq!(
        reference.module().functions[0]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::StringConcat { .. }))
            .count(),
        2
    );

    let mut heap = Heap::new(32);
    let first = heap.allocate_string("Nexa").unwrap();
    let second = heap.allocate_string("界").unwrap();
    let third = heap.allocate_string("!").unwrap();
    let before = heap.vm_allocation_counters().string_allocations;
    let outcome = CheckedInterpreter::run_with_heap(
        &verified,
        0,
        &[
            RuntimeValue::String {
                reference: first,
                hash: heap.string_hash(first).unwrap(),
            },
            RuntimeValue::String {
                reference: second,
                hash: heap.string_hash(second).unwrap(),
            },
            RuntimeValue::String {
                reference: third,
                hash: heap.string_hash(third).unwrap(),
            },
        ],
        1_000,
        &mut heap,
    )
    .unwrap();
    let InterpreterOutcome::Returned {
        value: Some(RuntimeValue::String { reference, .. }),
        ..
    } = outcome
    else {
        panic!("fused concat must return a string");
    };
    assert_eq!(heap.string(reference).unwrap(), "Nexa界!");
    assert_eq!(heap.vm_allocation_counters().string_allocations - before, 1);
}

#[test]
fn typed_string_index_lowers_to_rune_at_and_executes_unicode_scalars() {
    let verified = nexa_compiler::compile(r#"fn second() -> rune { return "a界"[1]; }"#).unwrap();
    assert!(
        verified.module().functions[0]
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::StringRuneAt { .. }))
    );
    let mut heap = Heap::new(16);
    let outcome = CheckedInterpreter::run_with_heap(&verified, 0, &[], 100, &mut heap).unwrap();
    assert!(matches!(
        outcome,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::Rune(value)),
            ..
        } if value == u32::from('界')
    ));
}

#[test]
fn array_capacity_intrinsics_execute_with_clear_retention_and_tail_shrink() {
    let verified = nexa_compiler::compile(
        r"
fn capacity_lifecycle() -> i32 {
    let values: Array<i32> = [];
    values.reserve(6);
    values.push(1);
    values.clear();
    let retained: i32 = values.capacity();
    values.shrink_to_fit();
    return retained * 10 + values.capacity();
}
",
    )
    .unwrap();
    let mut heap = Heap::new(16);
    let outcome = CheckedInterpreter::run_with_heap(&verified, 0, &[], 1_000, &mut heap).unwrap();
    assert!(matches!(
        outcome,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(60)),
            ..
        }
    ));
}

#[test]
fn array_reserve_rejects_negative_capacity_before_storage_mutation() {
    let verified = nexa_compiler::compile(
        r"
fn invalid_reserve() -> i32 {
    let values: Array<i32> = [];
    values.reserve(-1);
    return values.capacity();
}
",
    )
    .unwrap();
    let mut heap = Heap::new(16);
    let outcome = CheckedInterpreter::run_with_heap(&verified, 0, &[], 1_000, &mut heap).unwrap();
    assert!(matches!(
        outcome,
        InterpreterOutcome::Trapped { trap, .. }
            if trap.kind == TrapKind::StandardLibrary
                && trap.message.to_string().contains("must be non-negative")
    ));
    assert_eq!(heap.vm_allocation_counters().collection_relocation_bytes, 0);
}

#[test]
fn generic_standard_calls_carry_concrete_call_site_types() {
    let verified = nexa_compiler::compile(
        r"
use std::collections as collections;
use std::core as core;

fn inspect_i32(values: Array<i32>) -> bool {
    let element: Option<i32> = collections::array_get<i32>(values, 0);
    return core::is_some<i32>(element);
}

fn inspect_nested(values: Array<i32>) -> bool {
    return core::is_some(collections::array_get(values, 0));
}

fn append_i64(values: Array<i64>) -> bool {
    return collections::array_push(values, 1);
}

fn empty_i32_len() -> i32 {
    return collections::array_len<i32>([]);
}

fn option_default() -> i32 {
    return core::option_unwrap_or(Option::None, 7);
}

fn result_default() -> i32 {
    return core::result_unwrap_or(Result::Err(false), 7);
}
",
    )
    .unwrap();
    let instructions = verified
        .module()
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect::<Vec<_>>();
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::ArrayGet {
                element: ValueType::I32
            },
            ..
        }
    )));
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::OptionIsSome {
                value: ValueType::I32
            },
            ..
        }
    )));
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::ArrayPush {
                element: ValueType::I64
            },
            ..
        }
    )));
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::ArrayLen {
                element: ValueType::I32
            },
            ..
        }
    )));
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::OptionUnwrapOr {
                value: ValueType::I32
            },
            ..
        }
    )));
    assert!(instructions.iter().any(|instruction| matches!(
        instruction,
        Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::ResultUnwrapOr {
                success: ValueType::I32,
                error: ValueType::Bool
            },
            ..
        }
    )));
}

#[test]
fn array_capacity_calls_carry_concrete_types() {
    let verified = nexa_compiler::compile(
        r"
use std::collections as collections;

fn manage_i32(values: Array<i32>) -> i32 {
    collections::array_reserve(values, 8);
    collections::array_clear(values);
    collections::array_shrink_to_fit(values);
    return collections::array_capacity(values);
}

fn manage_i32_methods(values: Array<i32>) -> i32 {
    values.reserve(8);
    values.clear();
    values.shrink_to_fit();
    return values.capacity();
}
",
    )
    .unwrap();
    let instructions = verified
        .module()
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect::<Vec<_>>();
    for expected in [
        StandardIntrinsic::ArrayReserve {
            element: ValueType::I32,
        },
        StandardIntrinsic::ArrayCapacity {
            element: ValueType::I32,
        },
        StandardIntrinsic::ArrayClear {
            element: ValueType::I32,
        },
        StandardIntrinsic::ArrayShrinkToFit {
            element: ValueType::I32,
        },
    ] {
        assert!(instructions.iter().any(|instruction| matches!(
            instruction,
            Instruction::StandardIntrinsic { intrinsic, .. } if *intrinsic == expected
        )));
    }
    assert!(
        instructions
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ArrayClear { .. })),
        "method clear keeps the dedicated array opcode"
    );
}

#[test]
fn generic_standard_calls_reject_mismatched_or_unresolved_type_arguments() {
    for source in [
        r"
use std::collections as collections;
fn invalid(values: Array<i32>) -> Option<bool> {
    return collections::array_get<bool>(values, 0);
}
",
        r"
use std::core as core;
fn invalid() -> i32 {
    return core::min_i32<bool>(1, 2);
}
",
        r"
use std::core as core;
fn invalid() -> bool {
    return core::is_some<i32, bool>(Option::None);
}
",
        r"
use std::collections as collections;
fn invalid() -> i32 {
    return collections::array_len([]);
}
",
    ] {
        assert!(
            nexa_compiler::compile(source).is_err(),
            "invalid generic standard call unexpectedly compiled:\n{source}"
        );
    }
}

#[test]
fn analyzer_generated_defer_cleanup_reaches_typed_codegen_with_stable_debug_identity() {
    let compiled = compile_typed_evidence_package(
        r"
fn finalize(value: i32) -> i32 {
    return value;
}

async fn work(value: i32) -> i32 {
    defer finalize(value);
    yield;
    return value + 1;
}
",
    );
    assert_defer_cleanup_evidence(&compiled);
}

#[test]
fn defer_cleanup_captures_the_complete_physical_struct_range() {
    let compiled = compile_typed_evidence_package(
        r"
struct Pair {
    first: i32,
    second: i32,
}

fn finalize(value: Pair) -> i32 {
    return value.first + value.second;
}

async fn work() -> i32 {
    let value: Pair = Pair { first: 3, second: 5 };
    defer finalize(value);
    yield;
    return value.first;
}
",
    );
    let (cleanup, slots) = compiled
        .module
        .functions
        .iter()
        .flat_map(|function| &function.code)
        .find_map(|instruction| match instruction {
            Instruction::DeferPush {
                function,
                args_count,
                ..
            } => Some((*function, *args_count)),
            _ => None,
        })
        .expect("typed defer emits a physical capture");
    assert_eq!(slots, 2);
    assert_eq!(
        compiled.module.functions[cleanup as usize].parameter_slots,
        2
    );
}

#[test]
fn qualified_host_snippet_uses_typed_analysis_and_preserves_file_id() {
    let idl = nexa_contract::parse_contract(
        r"
contract GameHost;
    enum AnimationError { Missing, Cancelled }
    host {
        @cancel(return_error)
        @abandon(trap)
        async fn animation(entity: i32) -> Result<i32, AnimationError>;
    }
    nexa {
        fn update(entity: i32) -> i32;
    }
",
    )
    .unwrap();
    let source = r"
pub async fn update(entity: i32) -> i32 {
    let result: Result<i32, host::AnimationError> = host::animation(entity).await;
    return match result {
        Result::Ok(value) => value,
        Result::Err(error) => 0,
    };
}
";
    let file = FileId(77);
    let verified = nexa_compiler::compile_with_contract_file(source, file, &idl).unwrap();
    assert_eq!(
        verified.module().host_contract_id,
        Some(nexa_contract::contract_runtime_id(&idl))
    );
    assert_eq!(verified.module().host_imports.len(), 1);
    let module = verified.module();
    assert!(
        !module.source_map.is_empty(),
        "typed codegen must emit source mappings"
    );
    let target_function = module
        .exports
        .first()
        .expect("the required update entrypoint is emitted")
        .function;
    let target_mappings = module
        .source_map
        .iter()
        .filter(|entry| entry.function == target_function)
        .collect::<Vec<_>>();
    assert!(
        !target_mappings.is_empty(),
        "the exported update function must have source mappings"
    );
    assert!(
        target_mappings.iter().all(|entry| entry.span.file == file),
        "every update mapping must retain the caller's virtual FileId"
    );
}

#[test]
fn host_import_subset_keeps_the_referenced_contract_stable_identity() {
    let contract = nexa_contract::parse_contract(
        r"
contract DispatchHost;
    host {
        fn first() -> i32;
        fn second() -> i32;
        fn third() -> i32;
    }
",
    )
    .expect("valid three-function Host Contract");
    let third = contract
        .host_functions
        .iter()
        .find(|function| function.name == "third")
        .expect("the third Host function is present");
    let verified = nexa_compiler::compile_with_contract(
        r"
fn invoke_third() -> i32 {
    return host::third();
}
",
        &contract,
    )
    .expect("the referenced Host subset compiles");

    assert_eq!(
        verified.module().host_imports.len(),
        1,
        "unused Contract functions must not widen the effective import set"
    );
    assert_eq!(
        verified.module().host_imports[0].stable_id,
        third.stable_id,
        "the module-local import slot dispatches by the referenced Contract stable identity"
    );
    assert_eq!(
        verified.module().host_imports[0].declaration_fingerprint,
        third.declaration_fingerprint.into_bytes(),
        "Host declaration authority must be propagated verbatim"
    );
}

#[test]
fn virtual_contract_adapter_preserves_multiple_host_capabilities() {
    let contract = nexa_contract::parse_contract(
        r#"
contract CapabilityHost;
    host {
        @capability("world.read")
        @capability("world.write")
        fn privileged() -> i32;
    }
"#,
    )
    .expect("multiple distinct capabilities are valid NIDL v2");
    let verified =
        nexa_compiler::compile_with_contract("fn local() -> i32 { return 7; }", &contract)
            .expect("an unused multi-capability Host declaration remains a valid Contract surface");

    assert!(
        verified.module().host_imports.is_empty(),
        "unused privileged functions must not widen the effective Host import set"
    );
}

#[test]
fn distinct_nidl_handles_emit_distinct_typed_resource_tokens() {
    let contract = nexa_contract::parse_contract(
        r"
contract TokenHost;
    handle First;
    handle Second;

    host {
        fn echo_first(value: Token<First>) -> Token<First>;
        fn echo_second(value: Token<Second>) -> Token<Second>;
    }
",
    )
    .expect("valid nominal Handle contract");
    let verified = nexa_compiler::compile_with_contract(
        r"
fn first(value: Token<host::First>) -> Token<host::First> {
    return host::echo_first(value);
}

fn second(value: Token<host::Second>) -> Token<host::Second> {
    return host::echo_second(value);
}
",
        &contract,
    )
    .expect("nominal Handle tokens compile");
    let mut expected = contract
        .handles
        .iter()
        .map(|handle| nexa_bytecode::resource_token_type(handle.stable_id))
        .collect::<Vec<_>>();
    expected.sort();
    let mut actual = verified
        .module()
        .resource_token_types
        .iter()
        .map(|token| token.type_id)
        .collect::<Vec<_>>();
    actual.sort();

    assert_eq!(actual, expected);
    assert_ne!(actual[0], actual[1]);
}

#[test]
fn dependency_only_host_call_is_included_in_the_effective_import_subset() {
    let contract_id = StableId::from_name("compiler-dispatch-host");
    let functions = ["first", "second", "third"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| HostFunctionSurface {
            name: name.into(),
            parameters: Vec::new(),
            result: SurfaceType::I32,
            mode: HostFunctionMode::Sync,
            stable_id: StableId::from_name(&format!("compiler-dispatch-host.{name}")),
            declaration_fingerprint: [u8::try_from(index + 1).expect("small index"); 32],
            import_index: u32::try_from(index).expect("three Host functions fit u32"),
            fuel_cost: 1,
            async_result: None,
            required_capabilities: Vec::new(),
            source: None,
        })
        .collect::<Vec<_>>();
    let third = functions[2].stable_id;
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            contract_name: "DispatchHost".into(),
            contract_stable_id: contract_id,
            types: Vec::new(),
            functions,
            nexa_entrypoints: Vec::new(),
            required_entrypoints: Vec::new(),
            source: None,
        }),
        static_modules: Vec::new(),
    };
    let compiled = compile_dependency_host_evidence(&environment);

    assert_eq!(compiled.module.host_contract_id, Some(contract_id));
    assert_eq!(
        compiled.module.host_imports.len(),
        1,
        "a Host call that exists only in dependency Typed IR must still be emitted"
    );
    assert_eq!(compiled.module.host_imports[0].stable_id, third);
    assert!(
        compiled.debug_info.functions.iter().any(|function| {
            function.package_id == "nexa.compiler.host.dependency" && function.name == "read"
        }),
        "the dependency function containing the Host call is part of cumulative codegen"
    );
}

#[test]
fn contract_id_snippet_lowers_real_typed_migration_ir() {
    let host_contract_id = StableId::from_name("m4-snippet-host");
    let verified = nexa_compiler::compile_with_contract_id(
        r"
@state(version = 1)
class State {
    value: i32,
}

@migration
pub fn migrate() -> bool {
    let old_state: State = old.get(root);
    let value: i32 = old.field(old_state, State::value);
    let new_state: State = new.create(root);
    new.set(new_state, State::value, value);
    replace(root, new_state);
    finish_migration();
    return true;
}
",
        host_contract_id,
    )
    .unwrap();
    let module = verified.module();
    assert_eq!(module.host_contract_id, Some(host_contract_id));
    let migration_entry = module
        .reload_metadata
        .migration_entry
        .expect("typed migration entry is emitted");
    assert_eq!(module.state_schema.types.len(), 1);
    let state_type = &module.state_schema.types[0];
    assert_eq!(state_type.fields.len(), 1);
    let root = StableId::from_name("root");
    let field = state_type.fields[0].stable_id;
    let migration = &module.functions[usize::try_from(migration_entry).unwrap()];
    let operations = migration
        .code
        .iter()
        .filter_map(|instruction| match instruction {
            Instruction::StateOldGet { stable_id, ty, .. } => {
                assert_eq!(
                    (*stable_id, *ty),
                    (root, ValueType::Named(state_type.stable_id))
                );
                Some("old.get")
            }
            Instruction::StateOldFieldGet { field_id, ty, .. } => {
                assert_eq!((*field_id, *ty), (field, ValueType::I32));
                Some("old.field")
            }
            Instruction::StateNewCreate {
                stable_id, type_id, ..
            } => {
                assert_eq!((*stable_id, *type_id), (root, state_type.stable_id));
                Some("new.create")
            }
            Instruction::StateNewSet { field_id, .. } => {
                assert_eq!(*field_id, field);
                Some("new.set")
            }
            Instruction::StateReplace { old_id, .. } => {
                assert_eq!(*old_id, root);
                Some("replace")
            }
            Instruction::StateFinish => Some("finish"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        operations,
        [
            "old.get",
            "old.field",
            "new.create",
            "new.set",
            "replace",
            "finish"
        ],
        "typed migration IR must lower to the complete ordered state operation sequence"
    );
}

#[test]
fn virtual_snippet_diagnostics_keep_the_callers_file_id() {
    let file = FileId(91);
    let error =
        nexa_compiler::compile_file("fn bad() -> i32 { return missing; }", file).unwrap_err();
    assert_eq!(error.source_span().unwrap().file, file);
    let nexa_compiler::CompileError::AnalysisDiagnostic(diagnostic) = error else {
        panic!("canonical analyzer diagnostic expected");
    };
    assert_eq!(
        diagnostic.primary.source,
        nexa_compiler::AnalysisDiagnosticSource::Caller
    );
}

#[test]
fn virtual_snippet_preserves_utf8_and_crlf_root_byte_ranges_without_rewriting() {
    let source = "// 界\r\nfn bad() -> i32 {\r\n    return missing;\r\n}\r\n";
    let expected_start = u32::try_from(source.find("missing").unwrap()).unwrap();
    let file = FileId(95);
    let error = nexa_compiler::compile_file(source, file).unwrap_err();
    let nexa_compiler::CompileError::AnalysisDiagnostic(diagnostic) = error else {
        panic!("canonical analyzer diagnostic expected");
    };
    assert_eq!(
        diagnostic.primary.source,
        nexa_compiler::AnalysisDiagnosticSource::Caller
    );
    assert_eq!(
        (
            diagnostic.primary.span.file,
            diagnostic.primary.span.start,
            diagnostic.primary.span.end,
        ),
        (
            file,
            expected_start,
            expected_start + u32::try_from("missing".len()).unwrap(),
        )
    );
    assert_eq!(
        &source[diagnostic.primary.span.start as usize..diagnostic.primary.span.end as usize],
        "missing"
    );
    let identity = nexa_diagnostics::SourceIdentity::standalone("caller.nexa");
    let mut registry = nexa_diagnostics::SourceSnapshotRegistry::builder();
    registry.insert(identity.clone(), source).unwrap();
    let snapshot = registry.build();
    let human = snapshot
        .get(&identity)
        .unwrap()
        .human_range(nexa_diagnostics::ByteRange::new(
            diagnostic.primary.span.start,
            diagnostic.primary.span.end,
        ));
    assert_eq!((human.start.line, human.start.column), (3, 12));
    assert_eq!((human.end.line, human.end.column), (3, 19));
}

#[test]
fn virtual_snippet_preserves_lexical_error_class_and_exact_byte_range() {
    let error = nexa_compiler::compile_file("#", FileId(92)).unwrap_err();
    let nexa_compiler::CompileError::AnalysisDiagnostic(diagnostic) = &error else {
        panic!("canonical analyzer diagnostics must be preserved losslessly");
    };
    assert_eq!(diagnostic.code.as_str(), "NX1001");
    assert!(diagnostic.message.contains("unexpected character"));
    assert_eq!(
        (
            diagnostic.primary.span.file,
            diagnostic.primary.span.start,
            diagnostic.primary.span.end
        ),
        (FileId(92), 0, 1)
    );
    assert_eq!(
        diagnostic.primary.source,
        nexa_compiler::AnalysisDiagnosticSource::Caller
    );
    assert!(diagnostic.secondary.is_empty());
    assert!(diagnostic.related.is_empty());
    assert!(diagnostic.notes.is_empty());
    let span = error.source_span().unwrap();
    assert_eq!((span.start, span.end), (0, 1));
}

#[test]
fn virtual_snippet_rejects_removed_module_syntax_without_losing_origin() {
    let file = FileId(93);
    let error = nexa_compiler::compile_file("module Game.Combat;\n", file).unwrap_err();
    let nexa_compiler::CompileError::AnalysisDiagnostic(diagnostic) = error else {
        panic!("removed module syntax must remain a canonical analysis diagnostic");
    };
    assert!(
        diagnostic.message.contains("module")
            && (diagnostic.message.contains("legacy")
                || diagnostic.message.contains("removed")
                || diagnostic.message.contains("unexpected")),
        "unexpected removed-module diagnostic: {}",
        diagnostic.message
    );
    assert_eq!(
        diagnostic.primary.source,
        nexa_compiler::AnalysisDiagnosticSource::Caller
    );
    assert_eq!(diagnostic.primary.span.file, file);
    assert!(
        diagnostic.primary.span.start < diagnostic.primary.span.end,
        "removed syntax must retain a nonempty caller span"
    );
}

#[test]
fn virtual_snippet_module_inference_preserves_source_limit_origin() {
    let limit = nexa_analysis::CompilationLimits::default().source_file_bytes;
    let source = " ".repeat(limit + 1);
    let file = FileId(94);
    let error = nexa_compiler::compile_file(&source, file).unwrap_err();
    let nexa_compiler::CompileError::UnknownName { name, span } = error else {
        panic!("oversized snippets must remain structured compiler errors");
    };
    assert!(name.contains("exceeding the virtual-snippet limit"));
    assert_eq!(
        (span.file, span.start, span.end),
        (file, 0, u32::try_from(source.len()).unwrap())
    );
}

#[test]
fn removed_nexa_v1_surface_forms_are_rejected() {
    let removed = [
        (
            "var",
            "fn old_var() -> i32 { var value: i32 = 1; return value; }",
        ),
        (
            "module",
            "module old.surface;\nfn value() -> i32 { return 1; }",
        ),
        (
            "import",
            "import std.core as core;\nfn value() -> i32 { return 1; }",
        ),
        ("task-function", "task fn old_task() -> i32 { return 1; }"),
        (
            "prefix await",
            "async fn child() -> i32 { return 1; }\n\
             async fn parent() -> i32 { return await child(); }",
        ),
        ("stateful", "stateful class OldState { value: i32; }"),
        (
            "migration-function",
            "migration fn migrate() -> bool { finish_migration(); return true; }",
        ),
        (
            "activation-function",
            "activation fn activate() -> i32 { return 1; }",
        ),
        ("cleanup-function", "cleanup fn cleanup() -> unit {}"),
        (
            "immediate-function",
            "immediate fn calculate() -> i32 { return 1; }",
        ),
        (
            "with update",
            "struct Cell { value: i32, }\n\
             fn moved(cell: Cell) -> Cell { return cell with { value: 2 }; }",
        ),
    ];

    for (name, source) in removed {
        assert!(
            nexa_compiler::compile(source).is_err(),
            "removed Nexa v1 `{name}` form unexpectedly compiled:\n{source}"
        );
    }
}

#[test]
fn contract_v3_flat_surface_parses_and_rejects_removed_surface_forms() {
    let current = r#"
contract SurfaceMatrix;
handle Ticket;

struct Point {
    x: i32,
    y: i32,
}

enum LoadError {
    Missing,
    Failed(Point),
    Cancelled,
}

host {
    fn log(message: string);

    @fuel(8)
    @cancel(return_error)
    @abandon(trap)
    @capability("surface.read")
    async fn load(ticket: Ticket) -> Result<Point, LoadError>;
}

nexa {
    fn on_event(points: Array<Point>) -> Option<Point>;
}
"#;
    nexa_contract::parse_contract(current).expect("the frozen NIDL v2 surface parses");

    let removed = [
        ("interface", "interface Old {}"),
        ("opaque", "contract Old { opaque Ticket; }"),
        (
            "sync-function",
            "contract Old { host { sync fn ping() -> i32; } }",
        ),
        (
            "request-function",
            "contract Old { host { request(return_error, trap) fn load() -> request<i32>; } }",
        ),
        (
            "export",
            "contract Old { export OnEvent(value: i32) -> i32; }",
        ),
        (
            "lowercase array",
            "contract Old { struct Values { items: array<i32>, } }",
        ),
        ("void", "contract Old { host { fn ping() -> void; } }"),
    ];
    for (name, source) in removed {
        assert!(
            nexa_contract::parse_contract(source).is_err(),
            "removed NIDL v1 `{name}` form unexpectedly parsed:\n{source}"
        );
    }
}

#[test]
fn async_postfix_await_preserves_task_lowering_and_execution() {
    let verified = nexa_compiler::compile(
        r"
async fn child(value: i32) -> i32 {
    yield;
    return value + 1;
}

async fn parent(value: i32) -> i32 {
    let completed: i32 = child(value).await;
    return completed + 1;
}
",
    )
    .expect("postfix await compiles through typed codegen");
    let parent = verified
        .module()
        .functions
        .iter()
        .enumerate()
        .find_map(|(index, function)| {
            (function.effect == FunctionEffect::Task && index != 0)
                .then_some(u32::try_from(index).expect("function index fits u32"))
        })
        .expect("parent task is emitted");
    let suspended = CheckedInterpreter::run(&verified, parent, &[RuntimeValue::I32(40)], 1_000)
        .expect("postfix-await task starts");
    let (continuation, fuel) = match suspended {
        InterpreterOutcome::Suspended {
            continuation, fuel, ..
        } => (continuation, fuel),
        other => panic!("child yield must suspend the parent task, got {other:?}"),
    };
    let completed =
        CheckedInterpreter::poll(&verified, continuation, fuel, &OpcodeCostTable::default())
            .expect("postfix-await task resumes");
    assert!(matches!(
        completed,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(42)),
            ..
        }
    ));
}

#[test]
fn multi_payload_enum_patterns_preserve_dynamic_tuple_payload_without_materialization() {
    let verified = nexa_compiler::compile(
        r"
enum Pair {
    Empty,
    Both(i32, i32),
}

fn sum(first: i32, second: i32) -> i32 {
    let pair = Pair::Both(first, second);
    return match pair {
        Pair::Empty => 0,
        Pair::Both(left, right) => left + right,
    };
}
",
    )
    .expect("multi-payload Enum construction and matching must reach typed codegen");
    let mut heap = Heap::new(16);
    let result = CheckedInterpreter::run_with_heap(
        &verified,
        0,
        &[RuntimeValue::I32(20), RuntimeValue::I32(22)],
        1_000,
        &mut heap,
    )
    .expect("sum executes");
    assert!(matches!(
        result,
        InterpreterOutcome::Returned {
            value: Some(RuntimeValue::I32(42)),
            ..
        }
    ));
    let counters = heap.vm_allocation_counters();
    assert_eq!(counters.struct_materializations, 0);
    assert_eq!(counters.enum_materializations, 0);
}
