//! Stage-F differential gate (`EXECUTABLE_MODULE_V1.md`): the portable
//! reference interpreter and the predecoded-row interpreter execute the
//! same compiled artifact under the same cost-table version, so results,
//! traps, per-slice charges, suspend points, and fuel totals must match
//! item by item - exactly, not modulo the cross-pipeline exemption.

use nexa_bytecode::{
    FunctionBuilder, Instruction, ModuleBuilder, Signature, StateSchema, ValueType,
};
use nexa_core::StableId;
use nexa_runtime::{
    CheckedInterpreter, ContinuationReservation, ExecutableModule, FrameLimits, FuelState, Heap,
    InterpreterOutcome, OpcodeCostTable, RuntimeValue, SuspendReason, TrapKind,
};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
enum Signal { Quiet, Loud(i32), }

fn arithmetic(x: i32) -> i32 {
    let a: i32 = x * 3 + 7;
    let b: i32 = a - x / 2;
    return a * 100 + b;
}
fn strings(n: i32) -> i32 {
    let total: i32 = 0;
    let index: i32 = 0;
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
    let index: i32 = 0;
    while index < n {
        values.push(index * 2);
        table.set(index, index + 1);
        index = index + 1;
    }
    let total: i32 = 0;
    let cursor: i32 = 0;
    while cursor < values.len() {
        total = total + values[cursor] + match table.get(cursor) {
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
    return values[n];
}
fn enum_collection(n: i32) -> i32 {
    let values: Array<Signal> = Array::new();
    let index: i32 = 0;
    while index < n {
        values.push(Signal::Loud(index + 17));
        index = index + 1;
    }
    return match values[0] {
        Signal::Quiet => 0,
        Signal::Loud(value) => value,
    };
}
"#;

const ARITHMETIC: u32 = 0;
const STRINGS: u32 = 1;
const AGGREGATES: u32 = 3;
const COLLECTIONS: u32 = 4;
const DIV_TRAP: u32 = 5;
const INDEX_TRAP: u32 = 6;
const ENUM_COLLECTION: u32 = 7;

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
            None => {
                CheckedInterpreter::poll_with_heap(module, continuation, fuel, &costs, &mut heap)
            }
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
                assert!(
                    charge.instructions != 0 || fuel.cumulative_used > cumulative,
                    "replay budget {slice} cannot execute the next instruction"
                );
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

const GENERATED_BYTECODE_FUNCTIONS: usize = 64;
const GENERATED_BYTECODE_REGISTERS: u16 = 8;

fn next_bytecode_seed(state: &mut u64) -> u32 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    u32::try_from(value & u64::from(u32::MAX)).expect("masked seed fits u32")
}

fn generated_bytecode_module() -> VerifiedModule {
    let signature = Signature {
        parameters: vec![ValueType::I32, ValueType::I32],
        result: Some(ValueType::I32),
    };
    let mut seed = 0x5750_3730_5f4d_355fu64;
    let mut module = ModuleBuilder::new();
    module.metadata(
        StableId::from_name("m5.generated-bytecode"),
        StateSchema::default().fingerprint(),
    );
    for _ in 0..GENERATED_BYTECODE_FUNCTIONS {
        let mut function = FunctionBuilder::new(signature.clone(), GENERATED_BYTECODE_REGISTERS);
        for register in 2..GENERATED_BYTECODE_REGISTERS {
            function.emit(Instruction::LoadI32 {
                dst: register,
                value: next_bytecode_seed(&mut seed).cast_signed(),
            });
        }
        for _ in 0..32 {
            let destination = 2 + (next_bytecode_seed(&mut seed) % 6) as u16;
            let lhs = (next_bytecode_seed(&mut seed) % 8) as u16;
            let rhs = (next_bytecode_seed(&mut seed) % 8) as u16;
            let instruction = match next_bytecode_seed(&mut seed) % 4 {
                0 => Instruction::Add {
                    dst: destination,
                    lhs,
                    rhs,
                },
                1 => Instruction::Sub {
                    dst: destination,
                    lhs,
                    rhs,
                },
                2 => Instruction::Mul {
                    dst: destination,
                    lhs,
                    rhs,
                },
                _ => Instruction::Move {
                    dst: destination,
                    source: lhs,
                },
            };
            function.emit(instruction);
        }
        function.emit(Instruction::Return {
            source: (next_bytecode_seed(&mut seed) % 8) as u16,
        });
        module.function(function.finish().expect("generated function is legal"));
    }
    verify(module.finish(), VerifierLimits::default()).expect("generated bytecode verifies")
}

#[test]
fn seeded_generated_bytecode_matches_portable_execution_item_by_item() {
    let module = generated_bytecode_module();
    let executable = ExecutableModule::build(&module, &OpcodeCostTable::default())
        .expect("generated bytecode predecodes");
    let mut seed = 0x4558_4543_5f4d_355fu64;
    for function in 0..GENERATED_BYTECODE_FUNCTIONS {
        let arguments = [
            RuntimeValue::I32(next_bytecode_seed(&mut seed).cast_signed()),
            RuntimeValue::I32(next_bytecode_seed(&mut seed).cast_signed()),
        ];
        for slice in [48, 127, 1_000_000] {
            let function = u32::try_from(function).expect("generated function index fits u32");
            assert_eq!(
                replay(&module, Some(&executable), function, &arguments, slice),
                replay(&module, None, function, &arguments, slice),
                "generated function {function} diverges at slice budget {slice}"
            );
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
        (ENUM_COLLECTION, vec![RuntimeValue::I32(2)]),
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

const STATIC_LEAF_CORPUS: &str = r#"
enum Inner { Value(i32), }
enum Outer { Wrap(Inner), }
class Counter { value: i32, }

fn leaf_add(x: i32) -> i32 {
    return x + 1;
}
fn leaf_string_length() -> i32 {
    return "static-leaf".byte_len();
}
fn leaf_nested(x: i32) -> Outer {
    return Outer::Wrap(Inner::Value(x));
}
fn leaf_class() -> i32 {
    let value: Counter = Counter { value: 7 };
    value.value = value.value + 1;
    return value.value;
}
fn class_argument(value: Counter) -> i32 {
    return value.value;
}
fn leaf_array() -> i32 {
    let values: Array<i32> = Array::new();
    values.push(1);
    values.push(2);
    values.set(0, 3);
    return 3 + values.len();
}
fn array_argument(values: Array<i32>) -> i32 {
    return values[0];
}
fn leaf_buffer(destination: Buffer<i32>, source: Buffer<i32>) -> i32 {
    destination.copy(source, 0, 0, 3);
    return destination.get(2);
}
fn not_a_leaf(lhs: i32, rhs: i32) -> i32 {
    return lhs / rhs;
}
fn leaf_map_hit() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(1) {
        Option::Some(value) => value.byte_len(),
        Option::None => 0,
    };
}
fn leaf_map_miss() -> i32 {
    let values: Map<i32, string> = Map::new();
    values.set(1, "one");
    return match values.get(2) {
        Option::Some(value) => value.byte_len(),
        Option::None => 0,
    };
}
"#;

fn run_executable_once(
    module: &VerifiedModule,
    executable: &ExecutableModule,
    function: u32,
    arguments: &[RuntimeValue],
    heap: &mut Heap,
) -> (
    Option<RuntimeValue>,
    nexa_runtime::ExecutionCharge,
    FuelState,
) {
    let limits = FrameLimits::default();
    let continuation = CheckedInterpreter::start(
        module,
        function,
        arguments,
        limits,
        ContinuationReservation::for_limits(limits),
    )
    .expect("start full interpreter");
    let outcome = CheckedInterpreter::poll_with_heap_and_executable(
        module,
        continuation,
        FuelState::new(1_000_000, 0, u64::MAX),
        OpcodeCostTable::canonical(),
        heap,
        executable,
    )
    .expect("run full interpreter");
    let InterpreterOutcome::Returned {
        value,
        charge,
        fuel,
    } = outcome
    else {
        panic!("static-leaf reference must return");
    };
    (value, charge, fuel)
}

fn value_shape(heap: &Heap, value: RuntimeValue) -> String {
    match value {
        RuntimeValue::NamedRef { .. } => {
            let (type_id, variant, tag, payload) = heap
                .enum_parts(value)
                .expect("named leaf result is an enum");
            let payload =
                payload.map_or_else(|| "none".to_owned(), |payload| value_shape(heap, payload));
            format!("{type_id:?}/{variant:?}/{tag}/{payload}")
        }
        RuntimeValue::String { reference, .. } => {
            format!("string:{:?}", heap.string(reference).expect("leaf string"))
        }
        scalar => format!("{scalar:?}"),
    }
}

fn assert_static_leaf_certification(module: &VerifiedModule, executable: &ExecutableModule) {
    assert!(
        executable.functions()[0].static_leaf_fuel().is_some(),
        "arithmetic leaf is certified"
    );
    assert!(
        executable.functions()[1].static_leaf_fuel().is_some(),
        "string leaf is certified"
    );
    assert!(
        executable.functions()[2].static_leaf_fuel().is_some(),
        "nested enum leaf is certified"
    );
    assert!(
        executable.functions()[3].static_leaf_fuel().is_some(),
        "locally allocated class leaf is certified"
    );
    assert_eq!(
        executable.functions()[4].static_leaf_fuel(),
        None,
        "a class argument may be state-backed and retains the full interpreter"
    );
    assert!(
        executable.functions()[5].static_leaf_fuel().is_some(),
        "local array shape and indexes are certified: {:?}",
        module.module().functions[5]
    );
    assert_eq!(
        executable.functions()[6].static_leaf_fuel(),
        None,
        "an argument array has no load-time shape proof"
    );
    assert!(
        executable.functions()[7].static_leaf_fuel().is_some(),
        "constant-range buffer copy is certified with runtime bounds preflight"
    );
    assert_eq!(
        executable.functions()[8].static_leaf_fuel(),
        None,
        "division retains the full trapping interpreter"
    );
    assert!(
        executable.functions()[9].static_leaf_fuel().is_some(),
        "one-entry local map hit is certified"
    );
    assert!(
        executable.functions()[10].static_leaf_fuel().is_some(),
        "one-entry local map miss is certified"
    );
}

fn assert_static_leaf_case(
    module: &VerifiedModule,
    executable: &ExecutableModule,
    function: u32,
    arguments: &[RuntimeValue],
) {
    let mut reference_heap = Heap::new_with_limits(64, 4_096, 64);
    let (reference_value, reference_charge, reference_fuel) =
        run_executable_once(module, executable, function, arguments, &mut reference_heap);
    let mut leaf_heap = Heap::new_with_limits(64, 4_096, 64);
    let leaf = CheckedInterpreter::try_run_static_leaf(
        module,
        function,
        arguments,
        FuelState::new(1_000_000, 0, u64::MAX),
        OpcodeCostTable::canonical(),
        &mut leaf_heap,
        executable,
    )
    .expect("static leaf executes")
    .unwrap_or_else(|| panic!("function {function} is certified"));
    let leaf_value = leaf
        .result
        .as_ref()
        .expect("certified leaf returns rather than traps")
        .as_ref()
        .copied();

    assert_eq!(
        leaf.charge, reference_charge,
        "identical instruction charge"
    );
    assert_eq!(leaf.fuel, reference_fuel, "identical fuel settlement");
    assert_eq!(
        leaf_value.map(|value| value_shape(&leaf_heap, value)),
        reference_value.map(|value| value_shape(&reference_heap, value)),
        "identical returned value shape"
    );
    assert_eq!(
        leaf_heap.byte_inspection(),
        reference_heap.byte_inspection(),
        "identical heap accounting"
    );
}

fn buffer_arguments(module: &VerifiedModule, heap: &mut Heap) -> Vec<RuntimeValue> {
    let buffer_type = module.module().buffer_types[0].type_id;
    vec![
        heap.allocate_buffer(
            buffer_type,
            nexa_bytecode::ValueType::I32,
            &[
                RuntimeValue::I32(1),
                RuntimeValue::I32(2),
                RuntimeValue::I32(3),
            ],
        )
        .expect("destination buffer"),
        heap.allocate_buffer(
            buffer_type,
            nexa_bytecode::ValueType::I32,
            &[
                RuntimeValue::I32(7),
                RuntimeValue::I32(8),
                RuntimeValue::I32(9),
            ],
        )
        .expect("source buffer"),
    ]
}

fn assert_static_buffer_leaf_case(module: &VerifiedModule, executable: &ExecutableModule) {
    let mut reference_heap = Heap::new_with_limits(64, 4_096, 64);
    let reference_arguments = buffer_arguments(module, &mut reference_heap);
    let (reference_value, reference_charge, reference_fuel) = run_executable_once(
        module,
        executable,
        7,
        &reference_arguments,
        &mut reference_heap,
    );
    let mut leaf_heap = Heap::new_with_limits(64, 4_096, 64);
    let leaf_arguments = buffer_arguments(module, &mut leaf_heap);
    let leaf = CheckedInterpreter::try_run_static_leaf(
        module,
        7,
        &leaf_arguments,
        FuelState::new(1_000_000, 0, u64::MAX),
        OpcodeCostTable::canonical(),
        &mut leaf_heap,
        executable,
    )
    .expect("buffer leaf executes")
    .expect("buffer leaf is certified and in bounds");
    assert_eq!(
        leaf.result
            .as_ref()
            .expect("buffer leaf returns rather than traps"),
        &reference_value
    );
    assert_eq!(leaf.charge, reference_charge);
    assert_eq!(leaf.fuel, reference_fuel);
    assert_eq!(
        leaf_heap.byte_inspection(),
        reference_heap.byte_inspection()
    );
}

#[test]
fn certified_static_leaves_match_full_execution_exactly() {
    let module = nexa_compiler::compile(STATIC_LEAF_CORPUS).expect("static-leaf corpus compiles");
    let executable = ExecutableModule::build(&module, OpcodeCostTable::canonical())
        .expect("predecode static-leaf corpus");
    assert_static_leaf_certification(&module, &executable);
    for (function, arguments) in [
        (0, vec![RuntimeValue::I32(41)]),
        (1, vec![]),
        (3, vec![]),
        (5, vec![]),
        (9, vec![]),
        (10, vec![]),
    ] {
        assert_static_leaf_case(&module, &executable, function, &arguments);
    }
    let mut aggregate_heap = Heap::new_with_limits(64, 4_096, 64);
    assert!(
        CheckedInterpreter::try_run_static_leaf(
            &module,
            2,
            &[RuntimeValue::I32(9)],
            FuelState::new(1_000_000, 0, u64::MAX),
            OpcodeCostTable::canonical(),
            &mut aggregate_heap,
            &executable,
        )
        .expect("aggregate leaf admission is checked")
        .is_none(),
        "aggregate results deliberately fall back to transactional materialization"
    );
    assert_static_buffer_leaf_case(&module, &executable);
}

#[test]
fn static_leaf_fuel_rejection_precedes_heap_mutation() {
    let module = nexa_compiler::compile(STATIC_LEAF_CORPUS).expect("static-leaf corpus compiles");
    let executable = ExecutableModule::build(&module, OpcodeCostTable::canonical())
        .expect("predecode static-leaf corpus");
    let upper = executable.functions()[2]
        .static_leaf_fuel()
        .expect("nested enum is certified");
    assert!(upper > 0);
    let mut heap = Heap::new_with_limits(64, 4_096, 64);
    let before = heap.byte_inspection();
    let result = CheckedInterpreter::try_run_static_leaf(
        &module,
        2,
        &[RuntimeValue::I32(9)],
        FuelState::new(upper - 1, 0, u64::MAX),
        OpcodeCostTable::canonical(),
        &mut heap,
        &executable,
    )
    .expect("budget rejection is not an interpreter error");
    assert_eq!(
        result, None,
        "the caller falls back to suspension semantics"
    );
    assert_eq!(heap.live_len(), 0, "no enum was allocated");
    assert_eq!(
        heap.byte_inspection(),
        before,
        "all heap counters stay flat"
    );

    let buffer_type = module.module().buffer_types[0].type_id;
    let mut heap = Heap::new_with_limits(64, 4_096, 64);
    let destination = heap
        .allocate_buffer(
            buffer_type,
            nexa_bytecode::ValueType::I32,
            &[RuntimeValue::I32(1)],
        )
        .expect("short destination");
    let source = heap
        .allocate_buffer(
            buffer_type,
            nexa_bytecode::ValueType::I32,
            &[RuntimeValue::I32(7)],
        )
        .expect("short source");
    let before = heap.byte_inspection();
    let result = CheckedInterpreter::try_run_static_leaf(
        &module,
        7,
        &[destination, source],
        FuelState::new(1_000_000, 0, u64::MAX),
        OpcodeCostTable::canonical(),
        &mut heap,
        &executable,
    )
    .expect("invalid bounds fall back to the trapping interpreter");
    assert_eq!(result, None);
    assert_eq!(heap.buffer_get(destination, 0), Ok(RuntimeValue::I32(1)));
    assert_eq!(heap.byte_inspection(), before);
}
