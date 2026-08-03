//! M5 WP36 differential gate: the optimized emission pipeline versus the
//! reference pipeline (`nexa_compiler::compile_reference`) over one corpus.
//!
//! Ruling (`baseline/performance/BENCHMARK_PROTOCOL_V1.md`): cross-pipeline
//! comparisons require identical results, traps, and task lifecycles. Fuel
//! totals are exempt, so this gate records fuel divergence but never asserts
//! on it.

use nexa_bytecode::Instruction;
use nexa_runtime::{CheckedInterpreter, Heap, InterpreterOutcome, RuntimeValue, TrapKind};
use nexa_verifier::VerifiedModule;

/// Declaration order fixes the function indices used by the cases below.
const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
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

fn observe(module: &VerifiedModule, function: u32, arguments: &[RuntimeValue]) -> (Observed, u64) {
    let mut heap = Heap::new_with_limits(256, 16_384, 256);
    let outcome = CheckedInterpreter::run_with_heap(module, function, arguments, FUEL, &mut heap)
        .expect("differential corpus stays within limits");
    match outcome {
        InterpreterOutcome::Returned { value, charge, .. } => {
            (Observed::Returned(value), charge.fuel_used)
        }
        InterpreterOutcome::Trapped { trap, charge, .. } => {
            (Observed::Trapped(trap.kind), charge.fuel_used)
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
        &optimized_outcome, expected,
        "function {function} disagrees with the pinned corpus expectation"
    );
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

#[test]
fn reference_pipeline_actually_disables_the_optimizations() {
    let (optimized, reference) = pipelines();
    let materializations = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::StructNew { .. }))
            .count()
    };
    // WP27/WP45: the read-only struct local is physically inlined only by
    // the optimized pipeline; the reference side must keep the heap path.
    assert_eq!(
        materializations(&optimized, STRUCT_READ),
        0,
        "optimized pipeline inlines the read-only struct local"
    );
    assert!(
        materializations(&reference, STRUCT_READ) > 0,
        "reference pipeline materializes the struct on the heap"
    );
    // Stage-C enum slice: the statically selectable match binding loses its
    // EnumNew in the optimized pipeline only, while the escaping binding
    // (top-level binding pattern) keeps the heap path on both sides.
    let enum_materializations = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::EnumNew { .. }))
            .count()
    };
    assert_eq!(
        enum_materializations(&optimized, ENUM_ROUND),
        0,
        "optimized pipeline inlines the match-only enum local"
    );
    assert!(
        enum_materializations(&reference, ENUM_ROUND) > 0,
        "reference pipeline materializes the enum on the heap"
    );
    assert_eq!(
        enum_materializations(&optimized, ENUM_STATIC_QUIET),
        0,
        "optimized pipeline inlines the payload-less enum local"
    );
    assert!(
        enum_materializations(&optimized, ENUM_ESCAPE) > 0,
        "a top-level binding pattern disqualifies the enum local on both sides"
    );
    // WP52: the struct-element read fuses into ArrayFieldGet only in the
    // optimized pipeline; the reference side keeps the materializing get.
    let field_gets = |module: &VerifiedModule, function: u32| {
        module.module().functions[function as usize]
            .code
            .iter()
            .filter(|instruction| matches!(instruction, Instruction::ArrayFieldGet { .. }))
            .count()
    };
    assert!(
        field_gets(&optimized, ROW_PROJECTION) > 0,
        "optimized pipeline projects struct array fields without materializing"
    );
    assert_eq!(
        field_gets(&reference, ROW_PROJECTION),
        0,
        "reference pipeline keeps the materializing array get"
    );
    // WP52 push side: the pushed struct literal fuses into ArrayPushRow,
    // so the optimized row_projection body materializes no struct at all.
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
    assert_eq!(
        push_rows(&reference, ROW_PROJECTION),
        0,
        "reference pipeline keeps the materializing push"
    );
    assert_eq!(
        materializations(&optimized, ROW_PROJECTION),
        0,
        "the fused row workload emits no StructNew anywhere"
    );
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
