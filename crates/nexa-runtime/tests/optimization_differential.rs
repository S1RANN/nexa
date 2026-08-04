//! M5 WP36 differential gate: the optimized emission pipeline versus the
//! reference pipeline (`nexa_compiler::compile_reference`) over one corpus.
//!
//! Ruling (`baseline/performance/BENCHMARK_PROTOCOL_V1.md`): cross-pipeline
//! comparisons require identical results, traps, and task lifecycles. Fuel
//! totals are exempt, so this gate records fuel divergence but never asserts
//! on it.

use std::fmt::Write;

use nexa_bytecode::Instruction;
use nexa_core::SourceSpan;
use nexa_runtime::{CheckedInterpreter, Heap, InterpreterOutcome, RuntimeValue, TrapKind};
use nexa_verifier::VerifiedModule;

/// Declaration order fixes the function indices used by the cases below.
const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
class Cell { mut value: i32, next: Option<Cell>, }
enum Signal { Quiet, Loud(i32), }

fn fold_chain(x: i32) -> i32 {
    let a: i32 = 3 * 7 + 1;
    let b: i32 = a - 2;
    return x + b;
}
fn dead_locals(x: i32) -> i32 {
    let unused: i32 = x * 2;
    let shadowed: i32 = unused + 3;
    let kept: i32 = x + 1;
    return kept;
}
fn const_branch(x: i32) -> i32 {
    let flag: bool = 1 < 2;
    if flag {
        return x + 10;
    }
    return x - 10;
}
fn struct_read(x: i32) -> i32 {
    let cell: Pair = Pair { first: x, second: x + 3 };
    return cell.first * 100 + cell.second;
}
fn struct_mut(x: i32) -> i32 {
    let mut cell: Pair = Pair { first: x, second: 1 };
    cell.first = cell.first + 5;
    cell = Pair { first: cell.second, second: cell.first };
    return cell.second * 10 + cell.first;
}
fn pair_sum(cell: Pair) -> i32 {
    return cell.first + cell.second;
}
fn struct_escape(x: i32) -> i32 {
    let cell: Pair = Pair { first: x, second: 7 };
    return pair_sum(cell);
}
fn array_sweep(n: i32) -> i32 {
    let values: Array<i32> = Array::new();
    let mut index: i32 = 0;
    while index < n {
        values.push(index * 2);
        index = index + 1;
    }
    let mut total: i32 = 0;
    let mut cursor: i32 = 0;
    while cursor < values.len() {
        total = total + values.get(cursor);
        cursor = cursor + 1;
    }
    return total;
}
fn string_walk(n: i32) -> i32 {
    let mut total: i32 = 0;
    let mut index: i32 = 0;
    while index < n {
        let text: string = "interned-literal";
        total = total + text.byte_len();
        index = index + 1;
    }
    return total;
}
fn map_round(n: i32) -> i32 {
    let values: Map<i32, i32> = Map::new();
    let mut index: i32 = 0;
    while index < n {
        values.set(index, index * 3);
        index = index + 1;
    }
    let mut total: i32 = 0;
    let mut cursor: i32 = 0;
    while cursor < n {
        total = total + match values.get(cursor) {
            Option::Some(value) => value,
            Option::None => 0,
        };
        cursor = cursor + 1;
    }
    return total;
}
fn enum_round(x: i32) -> i32 {
    let signal: Signal = Signal::Loud(x);
    return match signal {
        Signal::Quiet => 0,
        Signal::Loud(value) => value + 1,
    };
}
fn enum_static_quiet(x: i32) -> i32 {
    let signal: Signal = Signal::Quiet;
    return match signal {
        Signal::Loud(value) => value,
        Signal::Quiet => x + 2,
    };
}
fn enum_escape(x: i32) -> i32 {
    let signal: Signal = Signal::Loud(x);
    return match signal {
        other => match other {
            Signal::Quiet => 0,
            Signal::Loud(value) => value + 3,
        },
    };
}
fn div_trap(a: i32, b: i32) -> i32 {
    return a / b;
}
fn index_trap(n: i32) -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    return values.get(n);
}
fn row_projection(n: i32) -> i32 {
    let cells: Array<Pair> = Array::new();
    let mut index: i32 = 0;
    while index < n {
        cells.push(Pair { first: index, second: index * 2 });
        index = index + 1;
    }
    let mut total: i32 = 0;
    let mut cursor: i32 = 0;
    while cursor < cells.len() {
        let cell: Pair = cells.get(cursor);
        total = total + cell.first + cell.second;
        cursor = cursor + 1;
    }
    return total;
}
fn row_projection_trap(n: i32) -> i32 {
    let cells: Array<Pair> = Array::new();
    cells.push(Pair { first: 1, second: 2 });
    let cell: Pair = cells.get(n);
    return cell.first;
}
fn scalar_array() -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    values.push(2);
    values.set(0, 3);
    return values.get(0) + values.len();
}
fn scalar_map_hit() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(1) {
        Option::Some(value) => value.byte_len(),
        Option::None => 0,
    };
}
fn scalar_map_miss() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(2) {
        Option::Some(value) => value.byte_len(),
        Option::None => 0,
    };
}
fn scalar_map_binding_fallback() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(1) {
        option => match option {
            Option::Some(value) => value.byte_len(),
            Option::None => 0,
        },
    };
}
fn class_value(cell: Cell) -> i32 {
    return cell.value;
}
fn scalar_class() -> i32 {
    let cell: Cell = new Cell { value: 7, next: Option::None };
    cell.value = cell.value + 1;
    return cell.value;
}
fn scalar_class_escape() -> i32 {
    let cell: Cell = new Cell { value: 7, next: Option::None };
    return class_value(cell);
}
"#;

const FOLD_CHAIN: u32 = 0;
const DEAD_LOCALS: u32 = 1;
const CONST_BRANCH: u32 = 2;
const STRUCT_READ: u32 = 3;
const STRUCT_MUT: u32 = 4;
const STRUCT_ESCAPE: u32 = 6;
const ARRAY_SWEEP: u32 = 7;
const STRING_WALK: u32 = 8;
const MAP_ROUND: u32 = 9;
const ENUM_ROUND: u32 = 10;
const ENUM_STATIC_QUIET: u32 = 11;
const ENUM_ESCAPE: u32 = 12;
const DIV_TRAP: u32 = 13;
const INDEX_TRAP: u32 = 14;
const ROW_PROJECTION: u32 = 15;
const ROW_PROJECTION_TRAP: u32 = 16;
const SCALAR_ARRAY: u32 = 17;
const SCALAR_MAP_HIT: u32 = 18;
const SCALAR_MAP_MISS: u32 = 19;
const SCALAR_MAP_BINDING_FALLBACK: u32 = 20;
const SCALAR_CLASS: u32 = 22;
const SCALAR_CLASS_ESCAPE: u32 = 23;

const FUEL: u64 = 1_000_000;

fn pipelines() -> (VerifiedModule, VerifiedModule) {
    let optimized = nexa_compiler::compile(CORPUS).expect("optimized pipeline compiles");
    let reference = nexa_compiler::compile_reference(CORPUS).expect("reference pipeline compiles");
    assert_eq!(
        optimized.module().functions.len(),
        reference.module().functions.len(),
        "both pipelines lower the same declaration list"
    );
    for (index, (left, right)) in optimized
        .module()
        .functions
        .iter()
        .zip(&reference.module().functions)
        .enumerate()
    {
        assert_eq!(
            left.signature, right.signature,
            "function {index} keeps one signature across pipelines"
        );
    }
    (optimized, reference)
}

/// Observable outcome of one call: the exempt fuel dimension is carried
/// separately and never enters the equality assertion.
#[derive(Debug, PartialEq)]
enum Observed {
    Returned(Option<RuntimeValue>),
    Trapped(TrapKind),
}

#[derive(Debug, PartialEq)]
struct TrapEvidence {
    source_span: Option<SourceSpan>,
    script_stack: Vec<(u32, Option<SourceSpan>)>,
    host_boundary: Option<(u32, Option<SourceSpan>)>,
}

#[derive(Debug, PartialEq)]
struct Observation {
    outcome: Observed,
    trap: Option<TrapEvidence>,
}

fn observe(
    module: &VerifiedModule,
    function: u32,
    arguments: &[RuntimeValue],
) -> (Observation, u64) {
    let mut heap = Heap::new_with_limits(256, 16_384, 256);
    let outcome = CheckedInterpreter::run_with_heap(module, function, arguments, FUEL, &mut heap)
        .unwrap_or_else(|error| {
            panic!(
                "differential function {function} with {arguments:?} stays within limits: {error}"
            )
        });
    let counters = heap.vm_allocation_counters();
    assert_eq!(
        counters.struct_materializations, 0,
        "function {function} must not materialize a physical Struct"
    );
    assert_eq!(
        counters.enum_materializations, 0,
        "function {function} must not materialize a physical Enum"
    );
    match outcome {
        InterpreterOutcome::Returned { value, charge, .. } => (
            Observation {
                outcome: Observed::Returned(value),
                trap: None,
            },
            charge.fuel_used,
        ),
        InterpreterOutcome::Trapped { trap, charge, .. } => {
            let evidence = TrapEvidence {
                source_span: trap.source_span,
                script_stack: trap
                    .script_call_stack
                    .as_slice()
                    .iter()
                    .map(|frame| (frame.function, frame.source_span))
                    .collect(),
                host_boundary: trap
                    .host_call_boundary
                    .map(|boundary| (boundary.import, boundary.source_span)),
            };
            (
                Observation {
                    outcome: Observed::Trapped(trap.kind),
                    trap: Some(evidence),
                },
                charge.fuel_used,
            )
        }
        InterpreterOutcome::Suspended { .. } | InterpreterOutcome::HostPending { .. } => {
            panic!("corpus never suspends or calls the host under test fuel")
        }
    }
}

fn assert_case(
    modules: &(VerifiedModule, VerifiedModule),
    function: u32,
    arguments: &[RuntimeValue],
    expected: &Observed,
) {
    let (optimized_outcome, optimized_fuel) = observe(&modules.0, function, arguments);
    let (reference_outcome, reference_fuel) = observe(&modules.1, function, arguments);
    assert_eq!(
        optimized_outcome, reference_outcome,
        "function {function} diverges across pipelines for {arguments:?} \
         (optimized fuel {optimized_fuel}, reference fuel {reference_fuel})"
    );
    assert_eq!(
        &optimized_outcome.outcome, expected,
        "function {function} disagrees with the pinned corpus expectation"
    );
}

const GENERATED_LAYOUT_CASES: usize = 48;

fn next_seeded(state: &mut u64) -> u32 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    u32::try_from(value & u64::from(u32::MAX)).expect("masked seed fits u32")
}

/// WP36 generated-program authority. One fixed seed produces a reproducible
/// mix of nested Struct, Enum, Option, update-construction, and Class-bearing
/// layouts. All functions are compiled together so this adds one optimized
/// and one reference compilation rather than one compiler invocation per
/// generated case.
fn generated_layout_corpus() -> String {
    let mut source = String::from(
        "struct GeneratedPair { left: i32, right: i32, }\n\
         class GeneratedNode { mut value: i32, }\n\
         struct GeneratedHolder { node: GeneratedNode, bias: i32, }\n\
         enum GeneratedValue { Empty, Pair(GeneratedPair), Scalar(i32), }\n",
    );
    let mut seed = 0x4e45_5841_5f4d_355fu64;
    for index in 0..GENERATED_LAYOUT_CASES {
        let left = i32::try_from(next_seeded(&mut seed) % 17).unwrap_or(0) - 8;
        let right = i32::try_from(next_seeded(&mut seed) % 19).unwrap_or(0) - 9;
        let factor = i32::try_from(next_seeded(&mut seed) % 5).unwrap_or(0) + 1;
        let mode = next_seeded(&mut seed) % 6;
        writeln!(source, "fn generated_{index:03}(x: i32) -> i32 {{")
            .expect("writing to String is infallible");
        match mode {
            0 => writeln!(
                source,
                "  let pair: GeneratedPair = GeneratedPair {{ left: x + {left}, right: x + {right} }};\n\
                 return pair.left * {factor} + pair.right;\n\
                 }}"
            ),
            1 => writeln!(
                source,
                "  let pair: GeneratedPair = GeneratedPair {{ left: x + {left}, right: x + {right} }};\n\
                 let value: GeneratedValue = GeneratedValue::Pair(pair);\n\
                 return match value {{\n\
                   GeneratedValue::Empty => {right},\n\
                   GeneratedValue::Scalar(item) => item,\n\
                   GeneratedValue::Pair(item) => item.left + item.right * {factor},\n\
                 }};\n\
                 }}"
            ),
            2 => writeln!(
                source,
                "  let value: GeneratedValue = GeneratedValue::Scalar(x + {left});\n\
                 return match value {{\n\
                   GeneratedValue::Empty => {right},\n\
                   GeneratedValue::Pair(item) => item.left,\n\
                   GeneratedValue::Scalar(item) => item * {factor},\n\
                 }};\n\
                 }}"
            ),
            3 => writeln!(
                source,
                "  let original: GeneratedPair = GeneratedPair {{ left: x + {left}, right: x + {right} }};\n\
                 let updated: GeneratedPair = GeneratedPair {{ left: x * {factor}, ..original }};\n\
                 return updated.left + updated.right;\n\
                 }}"
            ),
            4 => writeln!(
                source,
                "  let node: GeneratedNode = new GeneratedNode {{ value: x + {left} }};\n\
                 let holder: GeneratedHolder = GeneratedHolder {{ node: node, bias: {right} }};\n\
                 return holder.node.value * {factor} + holder.bias;\n\
                 }}"
            ),
            _ => writeln!(
                source,
                "  let pair: GeneratedPair = GeneratedPair {{ left: x + {left}, right: x + {right} }};\n\
                 let value: Option<GeneratedPair> = Option::Some(pair);\n\
                 return match value {{\n\
                   Option::Some(item) => item.left * {factor} + item.right,\n\
                   Option::None => {right},\n\
                 }};\n\
                 }}"
            ),
        }
        .expect("writing to String is infallible");
    }
    source
}

#[test]
fn seeded_generated_layout_programs_match_the_reference_pipeline() {
    let source = generated_layout_corpus();
    let optimized = nexa_compiler::compile(&source).expect("generated optimized corpus compiles");
    let reference =
        nexa_compiler::compile_reference(&source).expect("generated reference corpus compiles");
    assert_eq!(optimized.module().functions.len(), GENERATED_LAYOUT_CASES);
    assert_eq!(reference.module().functions.len(), GENERATED_LAYOUT_CASES);

    let mut seed = 0x5750_3336_5f4d_355fu64;
    for function in 0..GENERATED_LAYOUT_CASES {
        let generated = next_seeded(&mut seed).cast_signed();
        for input in [-31, -1, 0, 19, generated] {
            let arguments = [RuntimeValue::I32(input)];
            let (optimized_outcome, optimized_fuel) = observe(
                &optimized,
                u32::try_from(function).expect("generated function index fits u32"),
                &arguments,
            );
            let (reference_outcome, reference_fuel) = observe(
                &reference,
                u32::try_from(function).expect("generated function index fits u32"),
                &arguments,
            );
            assert_eq!(
                optimized_outcome, reference_outcome,
                "generated function {function} diverges for input {input} \
                 (optimized fuel {optimized_fuel}, reference fuel {reference_fuel})"
            );
        }
    }
}

#[test]
fn optimized_and_reference_pipelines_agree_on_results_and_traps() {
    let modules = pipelines();
    let returns = |value: i32| Observed::Returned(Some(RuntimeValue::I32(value)));
    let cases: &[(u32, Vec<RuntimeValue>, Observed)] = &[
        (FOLD_CHAIN, vec![RuntimeValue::I32(9)], returns(29)),
        (DEAD_LOCALS, vec![RuntimeValue::I32(9)], returns(10)),
        (CONST_BRANCH, vec![RuntimeValue::I32(9)], returns(19)),
        (CONST_BRANCH, vec![RuntimeValue::I32(-40)], returns(-30)),
        (STRUCT_READ, vec![RuntimeValue::I32(4)], returns(407)),
        (STRUCT_MUT, vec![RuntimeValue::I32(2)], returns(71)),
        (STRUCT_ESCAPE, vec![RuntimeValue::I32(5)], returns(12)),
        (ARRAY_SWEEP, vec![RuntimeValue::I32(40)], returns(1_560)),
        (STRING_WALK, vec![RuntimeValue::I32(32)], returns(512)),
        (MAP_ROUND, vec![RuntimeValue::I32(16)], returns(360)),
        (ENUM_ROUND, vec![RuntimeValue::I32(41)], returns(42)),
        (ENUM_STATIC_QUIET, vec![RuntimeValue::I32(9)], returns(11)),
        (ENUM_ESCAPE, vec![RuntimeValue::I32(20)], returns(23)),
        (
            DIV_TRAP,
            vec![RuntimeValue::I32(7), RuntimeValue::I32(2)],
            returns(3),
        ),
        (
            DIV_TRAP,
            vec![RuntimeValue::I32(7), RuntimeValue::I32(0)],
            Observed::Trapped(TrapKind::DivideByZero),
        ),
        (
            INDEX_TRAP,
            vec![RuntimeValue::I32(5)],
            Observed::Trapped(TrapKind::ArrayIndexOutOfBounds),
        ),
        // WP52: flattened rows behind fused field projection agree with
        // the materializing reference pipeline on results and traps.
        (ROW_PROJECTION, vec![RuntimeValue::I32(16)], returns(360)),
        (ROW_PROJECTION, vec![RuntimeValue::I32(0)], returns(0)),
        (ROW_PROJECTION_TRAP, vec![RuntimeValue::I32(0)], returns(1)),
        (SCALAR_ARRAY, vec![], returns(5)),
        (SCALAR_MAP_HIT, vec![], returns(3)),
        (SCALAR_MAP_MISS, vec![], returns(0)),
        (SCALAR_MAP_BINDING_FALLBACK, vec![], returns(3)),
        (SCALAR_CLASS, vec![], returns(8)),
        (SCALAR_CLASS_ESCAPE, vec![], returns(7)),
        (
            ROW_PROJECTION_TRAP,
            vec![RuntimeValue::I32(3)],
            Observed::Trapped(TrapKind::ArrayIndexOutOfBounds),
        ),
    ];
    for (function, arguments, expected) in cases {
        assert_case(&modules, *function, arguments, expected);
    }
}

fn assert_scalar_collection_materializations(
    optimized: &VerifiedModule,
    reference: &VerifiedModule,
) {
    let array_materializations = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::ArrayNew { .. }))
            .count()
    };
    assert_eq!(
        array_materializations(optimized, SCALAR_ARRAY),
        0,
        "optimized pipeline scalar-replaces the bounded local array"
    );
    assert!(
        array_materializations(reference, SCALAR_ARRAY) > 0,
        "reference pipeline materializes the bounded local array"
    );
    let map_materializations = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::MapNew { .. }))
            .count()
    };
    for function in [SCALAR_MAP_HIT, SCALAR_MAP_MISS] {
        assert_eq!(
            map_materializations(optimized, function),
            0,
            "optimized pipeline scalar-replaces the local map"
        );
        assert!(
            map_materializations(reference, function) > 0,
            "reference pipeline materializes the local map"
        );
    }
    assert!(
        map_materializations(optimized, SCALAR_MAP_BINDING_FALLBACK) > 0,
        "binding the complete get result keeps the map on the heap"
    );
    let class_materializations = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::ClassNew { .. }))
            .count()
    };
    assert_eq!(
        class_materializations(optimized, SCALAR_CLASS),
        0,
        "optimized pipeline scalar-replaces the non-escaping local class"
    );
    assert!(
        class_materializations(reference, SCALAR_CLASS) > 0,
        "reference pipeline materializes the local class"
    );
    assert!(
        class_materializations(optimized, SCALAR_CLASS_ESCAPE) > 0,
        "passing the class to another function preserves its identity"
    );
}

#[test]
fn reference_pipeline_actually_disables_the_optimizations() {
    let (optimized, reference) = pipelines();
    let struct_constructions = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::StructNew { .. }))
            .count()
    };
    // WP27 Struct representation is shared by both pipelines: construction
    // uses physical register ranges and never denotes a heap materialization.
    assert!(
        struct_constructions(&optimized, STRUCT_READ) > 0
            && struct_constructions(&reference, STRUCT_READ) > 0,
        "both pipelines retain the physical Struct construction"
    );
    let enum_constructions = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::EnumNew { .. }))
            .count()
    };
    for function in [ENUM_ROUND, ENUM_STATIC_QUIET, ENUM_ESCAPE] {
        assert_eq!(
            enum_constructions(&optimized, function),
            0,
            "WP43 removes a locally known Enum construction in function {function}"
        );
        assert!(
            enum_constructions(&reference, function) > 0,
            "reference function {function} retains physical Enum construction"
        );
    }
    // WP52 row storage is likewise a mandatory materialization boundary:
    // both pipelines push and read flattened physical rows.
    let push_rows = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::ArrayPushRow { .. }))
            .count()
    };
    assert!(
        push_rows(&optimized, ROW_PROJECTION) > 0,
        "optimized pipeline pushes struct literals as rows"
    );
    assert!(
        push_rows(&reference, ROW_PROJECTION) > 0,
        "reference pipeline also uses the physical row boundary"
    );
    assert_eq!(
        struct_constructions(&optimized, ROW_PROJECTION),
        0,
        "the fused row workload emits no StructNew anywhere"
    );
    assert_eq!(
        struct_constructions(&reference, ROW_PROJECTION),
        0,
        "the reference row boundary also avoids heap Struct materialization"
    );
    assert_scalar_collection_materializations(&optimized, &reference);
    // WP37/WP38: constant folding shortens the arithmetic chain, so the
    // optimized body must be strictly smaller. If this ever fails the
    // reference switch is wired wrong and the gate is comparing a pipeline
    // against itself.
    let code_len = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize].code.len()
    };
    assert!(
        code_len(&optimized, FOLD_CHAIN) < code_len(&reference, FOLD_CHAIN),
        "constant folding must shorten fold_chain in the optimized pipeline only"
    );
}
