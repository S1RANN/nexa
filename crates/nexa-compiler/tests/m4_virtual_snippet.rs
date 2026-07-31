use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, BuildFingerprintInput, CompilationOptions, HostContractSurface,
    HostFunctionMode, HostFunctionSurface, NormalizedPackagePath, PackageId, PackageManifest,
    QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage, SourceId,
    SourceRole, SourceSetBuilder, SurfaceType, analyze_package, source_set_fingerprint,
};
use nexa_bytecode::{FunctionEffect, Instruction, StandardIntrinsic, ValueType};
use nexa_compiler::PackageCompileOutput;
use nexa_core::{CanonicalSymbolIdentity, FileId, StableId, SymbolKind};
use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, FrameLimits, FuelState, Heap, HostTrap,
    InterpreterHost, InterpreterHostOutcome, InterpreterOutcome, OpcodeCostTable, RuntimeValue,
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
        host_contract_source: Vec::new(),
        host_required_exports: nexa_idl::canonical_required_exports(std::iter::empty::<&str>()),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION.into(),
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
        fingerprint.host_required_exports.clone(),
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
fn unit_runtime_values_are_materialized_after_effects_and_inside_containers() {
    let compiled = compile_typed_evidence_package(
        r"
module compiler.evidence;

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
    return Some(noop());
}

fn array_unit() -> Array<unit> {
    return [noop()];
}

task fn unit_task() -> unit {
    yield;
    return noop();
}

task fn await_unit() -> i32 {
    let value: unit = await unit_task();
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
    let noop = function_index("noop");
    let consume = function_index("consume");
    let via_return = function_index("via_return");
    let via_parameter = function_index("via_parameter");
    let option_unit = function_index("option_unit");
    let array_unit = function_index("array_unit");
    let await_unit = function_index("await_unit");

    let via_return_code = &compiled.module.functions
        [usize::try_from(via_return).expect("function index fits usize")]
    .code;
    assert!(
        via_return_code.windows(2).any(|instructions| matches!(
            instructions,
            [
                Instruction::Call { function, .. },
                Instruction::LoadI32 { value: 0, .. }
            ] if *function == noop
        )),
        "a Unit call must materialize its sentinel only after the call completes"
    );
    assert_eq!(
        via_return_code.last(),
        Some(&Instruction::ReturnVoid),
        "returning a Unit expression must execute it and return without a bytecode result"
    );

    let via_parameter_code = &compiled.module.functions
        [usize::try_from(via_parameter).expect("function index fits usize")]
    .code;
    assert!(
        via_parameter_code.windows(3).any(|instructions| matches!(
            instructions,
            [
                Instruction::Call {
                    function: called_noop,
                    ..
                },
                Instruction::LoadI32 { value: 0, .. },
                Instruction::Call {
                    function: called_consume,
                    ..
                }
            ] if *called_noop == noop && *called_consume == consume
        )),
        "the Unit sentinel must be initialized before it is passed to another call"
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
        arguments: &[RuntimeValue],
        _heap: Option<&mut Heap>,
    ) -> Result<InterpreterHostOutcome, HostTrap> {
        if import != 0 {
            return Err(HostTrap::UnknownFunction(import));
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
    let host_interface = StableId::from_name("compiler-evidence-unit-host");
    let environment = AnalysisEnvironment {
        host: Some(HostContractSurface {
            interface_name: "UnitHost".into(),
            interface_stable_id: host_interface,
            types: Vec::new(),
            functions: vec![HostFunctionSurface {
                name: "touch".into(),
                parameters: Vec::new(),
                result: SurfaceType::Unit,
                mode: HostFunctionMode::Sync,
                stable_id: StableId::from_name("compiler-evidence-unit-host.touch"),
                import_index: 0,
                fuel_cost: 1,
                async_result: None,
                required_capability: None,
                source: None,
            }],
            required_exports: Vec::new(),
            source: None,
        }),
        static_modules: Vec::new(),
    };
    let compiled = compile_typed_evidence_package_with_environment(
        r"
module compiler.evidence;
import host as api;

fn consume(value: unit) -> i32 {
    return 23;
}

fn host_unit() -> i32 {
    return consume(api.touch());
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
    assert_eq!(verified.module().host_interface_hash, Some(host_interface));
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
module compiler.evidence;

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

#[test]
fn typed_numeric_and_scalar_domains_lower_exhaustively() {
    let verified = nexa_compiler::compile(
        r#"
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
"#,
    )
    .unwrap();
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
        "i32 conversion",
        "i64 conversion",
        "f32 conversion",
        "f64 conversion",
        "bool conversion",
        "rune conversion",
        "string conversion",
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
            "i32 conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::I32ToString { .. })),
            "i64 conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::I64ToString { .. })),
            "f32 conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::F32ToString { .. })),
            "f64 conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::F64ToString { .. })),
            "bool conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::BoolToString { .. })),
            "rune conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::RuneToString { .. })),
            "string conversion" => instructions
                .iter()
                .any(|instruction| matches!(instruction, Instruction::StringToString { .. })),
            _ => false,
        };
        assert!(present, "missing {expected}");
    }
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
fn generic_standard_calls_carry_concrete_call_site_types() {
    let verified = nexa_compiler::compile(
        r"
import std.collections as collections;
import std.core as core;

fn inspect_i32(values: Array<i32>) -> bool {
    let element: Option<i32> = collections.array_get<i32>(values, 0);
    return core.is_some<i32>(element);
}

fn inspect_nested(values: Array<i32>) -> bool {
    return core.is_some(collections.array_get(values, 0));
}

fn append_i64(values: Array<i64>) -> bool {
    return collections.array_push(values, 1);
}

fn empty_i32_len() -> i32 {
    return collections.array_len<i32>([]);
}

fn option_default() -> i32 {
    return core.option_unwrap_or(None, 7);
}

fn result_default() -> i32 {
    return core.result_unwrap_or(Err(false), 7);
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
fn generic_standard_calls_reject_mismatched_or_unresolved_type_arguments() {
    for source in [
        r"
import std.collections as collections;
fn invalid(values: Array<i32>) -> Option<bool> {
    return collections.array_get<bool>(values, 0);
}
",
        r"
import std.core as core;
fn invalid() -> i32 {
    return core.min_i32<bool>(1, 2);
}
",
        r"
import std.core as core;
fn invalid() -> bool {
    return core.is_some<i32, bool>(None);
}
",
        r"
import std.collections as collections;
fn invalid() -> i32 {
    return collections.array_len([]);
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
module compiler.evidence;

fn finalize(value: i32) -> i32 {
    return value;
}

task fn work(value: i32) -> i32 {
    defer finalize(value);
    yield;
    return value + 1;
}
",
    );
    assert_defer_cleanup_evidence(&compiled);
}

#[test]
fn qualified_host_snippet_uses_typed_analysis_and_preserves_file_id() {
    let idl = nexa_idl::parse(
        r"
interface GameHost {
    enum AnimationError { Missing, Cancelled }
    request(return_error, trap) fn animation(entity: i32)
        -> request<Result<i32, AnimationError>>;
    export Update(entity: i32) -> i32;
}
",
    )
    .unwrap();
    let source = r"
module game.combat;
import host as engine;

pub task fn Update(entity: i32) -> i32 {
    let result: Result<i32, engine.AnimationError> = await engine.animation(entity);
    return match result {
        Ok(value) => value,
        Err(error) => 0,
    };
}
";
    let file = FileId(77);
    let verified = nexa_compiler::compile_with_interface_file(source, file, &idl).unwrap();
    assert_eq!(
        verified.module().host_interface_hash,
        Some(nexa_idl::exact_hash(&idl))
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
        .expect("the required Update export is emitted")
        .function;
    let target_mappings = module
        .source_map
        .iter()
        .filter(|entry| entry.function == target_function)
        .collect::<Vec<_>>();
    assert!(
        !target_mappings.is_empty(),
        "the exported Update function must have source mappings"
    );
    assert!(
        target_mappings.iter().all(|entry| entry.span.file == file),
        "every Update mapping must retain the caller's virtual FileId"
    );
}

#[test]
fn metadata_snippet_lowers_real_typed_migration_ir() {
    let host_hash = StableId::from_name("m4-snippet-host");
    let verified = nexa_compiler::compile_with_metadata(
        r"
module reload;

@stateful(1) class State {
    value: i32;
}

pub migration fn migrate() -> bool {
    let old_state: State = old.get<State>(root);
    let value: i32 = old.field<i32>(old_state, State.value);
    let new_state: State = new.create<State>(root);
    new.set(new_state, State.value, value);
    replace(root, new_state);
    finish_migration();
    return true;
}
",
        host_hash,
    )
    .unwrap();
    let module = verified.module();
    assert_eq!(module.host_interface_hash, Some(host_hash));
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
fn virtual_snippet_module_inference_preserves_invalid_path_origin() {
    let file = FileId(93);
    let error = nexa_compiler::compile_file("module Game.Combat;\n", file).unwrap_err();
    let nexa_compiler::CompileError::AnalysisDiagnostic(diagnostic) = error else {
        panic!("invalid module paths must remain canonical analysis diagnostics");
    };
    assert_eq!(diagnostic.code.as_str(), "NX2701");
    assert_eq!(diagnostic.message, "invalid module path `Game.Combat`");
    assert_eq!(
        diagnostic.primary.source,
        nexa_compiler::AnalysisDiagnosticSource::Caller
    );
    assert_eq!(
        (
            diagnostic.primary.span.file,
            diagnostic.primary.span.start,
            diagnostic.primary.span.end
        ),
        (file, 7, 18)
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
