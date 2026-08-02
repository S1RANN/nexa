//! Stage-F differential gate (`EXECUTABLE_MODULE_V1.md`): the portable
//! reference interpreter and the predecoded-row interpreter execute the
//! same compiled artifact under the same cost-table version, so results,
//! traps, per-slice charges, suspend points, and fuel totals must match
//! item by item - exactly, not modulo the cross-pipeline exemption.

use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, ExecutableModule, FrameLimits, FuelState, Heap,
    InterpreterOutcome, OpcodeCostTable, RuntimeValue, SuspendReason, TrapKind,
};
use nexa_verifier::VerifiedModule;

const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
enum Signal { Quiet, Loud(i32), }

fn arithmetic(x: i32) -> i32 {
    let a: i32 = x * 3 + 7;
    let b: i32 = a - x / 2;
    return a * 100 + b;
}
fn strings(n: i32) -> i32 {
    let mut total: i32 = 0;
    let mut index: i32 = 0;
    while index < n {
        let text: string = "parity" + "-corpus";
        total = total + text.byte_len();
        index = index + 1;
    }
    return total;
}
fn pair_sum(cell: Pair) -> i32 {
    return cell.first + cell.second;
}
fn aggregates(x: i32) -> i32 {
    let cell: Pair = Pair { first: x, second: x + 3 };
    let escaping: Pair = Pair { first: x + 1, second: 2 };
    let signal: Signal = Signal::Loud(x);
    let selected: i32 = match signal {
        Signal::Quiet => 0,
        Signal::Loud(value) => value,
    };
    return pair_sum(escaping) + cell.first * selected;
}
fn collections(n: i32) -> i32 {
    let values: Array<i32> = Array::new();
    let table: Map<i32, i32> = Map::new();
    let mut index: i32 = 0;
    while index < n {
        values.push(index * 2);
        table.set(index, index + 1);
        index = index + 1;
    }
    let mut total: i32 = 0;
    let mut cursor: i32 = 0;
    while cursor < values.len() {
        total = total + values.get(cursor) + match table.get(cursor) {
            Option::Some(value) => value,
            Option::None => 0,
        };
        cursor = cursor + 1;
    }
    return total;
}
fn div_trap(a: i32, b: i32) -> i32 {
    return a / b;
}
fn index_trap(n: i32) -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    return values.get(n);
}
"#;

const ARITHMETIC: u32 = 0;
const STRINGS: u32 = 1;
const AGGREGATES: u32 = 3;
const COLLECTIONS: u32 = 4;
const DIV_TRAP: u32 = 5;
const INDEX_TRAP: u32 = 6;

/// Everything both interpreters must agree on, item by item.
#[derive(Debug, PartialEq)]
struct Replay {
    outcome: Outcome,
    cumulative_fuel: u64,
    slices: Vec<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Returned(Option<RuntimeValue>),
    Trapped(TrapKind),
}

fn replay(
    module: &VerifiedModule,
    executable: Option<&ExecutableModule>,
    function: u32,
    arguments: &[RuntimeValue],
    slice: u64,
) -> Replay {
    let costs = OpcodeCostTable::default();
    let limits = FrameLimits::default();
    let mut heap = Heap::new_with_limits(256, 16_384, 256);
    let mut continuation = CheckedInterpreter::start(
        module,
        function,
        arguments,
        limits,
        ContinuationReservation::for_limits(limits),
    )
    .expect("start parity continuation");
    let mut cumulative = 0;
    let mut slices = Vec::new();
    loop {
        let fuel = FuelState::new(slice, cumulative, u64::MAX);
        let outcome = match executable {
            Some(rows) => CheckedInterpreter::poll_with_heap_and_executable(
                module,
                continuation,
                fuel,
                &costs,
                &mut heap,
                rows,
            ),
            None => CheckedInterpreter::poll_with_heap(module, continuation, fuel, &costs, &mut heap),
        }
        .expect("parity slice executes");
        match outcome {
            InterpreterOutcome::Suspended {
                continuation: next,
                reason,
                charge,
                fuel,
            } => {
                assert_eq!(reason, SuspendReason::Fuel, "corpus only suspends on fuel");
                slices.push((charge.fuel_used, charge.instructions));
                cumulative = fuel.cumulative_used;
                continuation = next;
            }
            InterpreterOutcome::Returned {
                value,
                charge,
                fuel,
            } => {
                slices.push((charge.fuel_used, charge.instructions));
                return Replay {
                    outcome: Outcome::Returned(value),
                    cumulative_fuel: fuel.cumulative_used,
                    slices,
                };
            }
            InterpreterOutcome::Trapped { trap, charge, fuel } => {
                slices.push((charge.fuel_used, charge.instructions));
                return Replay {
                    outcome: Outcome::Trapped(trap.kind),
                    cumulative_fuel: fuel.cumulative_used,
                    slices,
                };
            }
            InterpreterOutcome::HostPending { .. } => {
                panic!("parity corpus never calls the host")
            }
        }
    }
}

#[test]
fn portable_and_executable_interpreters_match_item_by_item() {
    let module = nexa_compiler::compile(CORPUS).expect("parity corpus compiles");
    let executable = ExecutableModule::build(&module, &OpcodeCostTable::default())
        .expect("predecode parity corpus");
    let cases: &[(u32, Vec<RuntimeValue>)] = &[
        (ARITHMETIC, vec![RuntimeValue::I32(9)]),
        (STRINGS, vec![RuntimeValue::I32(24)]),
        (AGGREGATES, vec![RuntimeValue::I32(6)]),
        (COLLECTIONS, vec![RuntimeValue::I32(20)]),
        (DIV_TRAP, vec![RuntimeValue::I32(7), RuntimeValue::I32(2)]),
        (DIV_TRAP, vec![RuntimeValue::I32(7), RuntimeValue::I32(0)]),
        (INDEX_TRAP, vec![RuntimeValue::I32(5)]),
    ];
    // Small slices force many fuel suspensions, so safepoint placement and
    // per-slice settlement are compared, not just the end state.
    for slice in [48, 512, 1_000_000] {
        for (function, arguments) in cases {
            let portable = replay(&module, None, *function, arguments, slice);
            let rows = replay(&module, Some(&executable), *function, arguments, slice);
            assert_eq!(
                rows, portable,
                "function {function} diverges at slice budget {slice}"
            );
        }
    }
}

#[test]
fn trap_kinds_survive_the_row_path() {
    let module = nexa_compiler::compile(CORPUS).expect("parity corpus compiles");
    let executable = ExecutableModule::build(&module, &OpcodeCostTable::default())
        .expect("predecode parity corpus");
    let divide = replay(
        &module,
        Some(&executable),
        DIV_TRAP,
        &[RuntimeValue::I32(1), RuntimeValue::I32(0)],
        1_000_000,
    );
    assert_eq!(divide.outcome, Outcome::Trapped(TrapKind::DivideByZero));
    let index = replay(
        &module,
        Some(&executable),
        INDEX_TRAP,
        &[RuntimeValue::I32(9)],
        1_000_000,
    );
    assert_eq!(
        index.outcome,
        Outcome::Trapped(TrapKind::ArrayIndexOutOfBounds)
    );
}
