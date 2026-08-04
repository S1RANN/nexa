//! M5 WP58 collection/string stress gate.
//!
//! These cases pin the required production scales while staying deterministic:
//! no wall-clock assertions, randomized inputs, or release-only behavior.

use std::num::NonZeroU16;
use std::sync::Arc;

use nexa_bytecode::{Instruction, ValueType};
use nexa_core::StableId;
use nexa_runtime::{
    CheckedInterpreter, ExecutableModule, GcBudget, GcPhase, GcRoots, Heap, MapSetOutcome,
    OpcodeCostTable, RuntimeValue,
};

fn reference_of(value: RuntimeValue) -> nexa_runtime::GcRef {
    let RuntimeValue::NamedRef { reference, .. } = value else {
        panic!("expected a named reference");
    };
    reference
}

fn set_map(heap: &mut Heap, map: RuntimeValue, key: RuntimeValue, value: RuntimeValue) -> usize {
    for attempts in 1..=20_000 {
        match heap.map_set(map, key, value).expect("map set") {
            MapSetOutcome::Complete => return attempts,
            MapSetOutcome::RehashPending => {}
        }
    }
    panic!("bounded incremental map rehash did not converge");
}

#[test]
fn ten_thousand_struct_rows_stay_flat_without_element_objects() {
    const ROWS: usize = 10_000;
    const FIELDS: usize = 3;

    let struct_type = StableId::from_name("wp58::Row");
    let element = ValueType::Named(struct_type);
    let mut heap = Heap::new_with_arena_limits(8, 1 << 20, ROWS, ROWS * FIELDS, 32);
    let rows = heap
        .allocate_value_row_array(
            nexa_bytecode::array_type(element),
            element,
            NonZeroU16::new(u16::try_from(FIELDS).unwrap()).unwrap(),
        )
        .expect("flattened row array");
    let before = heap.vm_allocation_counters();

    for index in 0..ROWS {
        heap.array_push_row(
            rows,
            &[
                RuntimeValue::I32(i32::try_from(index).unwrap()),
                RuntimeValue::Bool(index % 2 == 0),
                RuntimeValue::Rune(u32::from('界')),
            ],
        )
        .expect("row push");
    }

    assert_eq!(heap.array_len(rows), Ok(ROWS));
    let view = heap
        .array_rows(rows)
        .expect("row view")
        .expect("flattened layout");
    assert_eq!(view.cells.len() / view.stride, ROWS);
    assert_eq!(view.stride, FIELDS);
    assert_eq!(
        heap.array_field_get(rows, ROWS - 1, 0),
        Ok(RuntimeValue::I32(i32::try_from(ROWS - 1).unwrap()))
    );
    assert_eq!(
        heap.array_field_get(rows, ROWS - 1, 2),
        Ok(RuntimeValue::Rune(u32::from('界')))
    );

    let delta = heap.vm_allocation_counters().delta_since(before);
    assert_eq!(delta.struct_materializations, 0);
    assert_eq!(delta.object_allocations, 0);
    assert_eq!(
        delta.collection_relocation_bytes, 0,
        "an unblocked arena tail must extend without relocating struct rows"
    );
}

#[test]
fn hundred_thousand_push_pop_and_repeated_map_rehash_converge() {
    const OPERATIONS: usize = 100_000;
    const MAP_ENTRIES: i32 = 8_192;

    let mut heap = Heap::new_with_arena_limits(32, 1 << 20, OPERATIONS, OPERATIONS, 256);
    let array = heap
        .allocate_array(nexa_bytecode::array_type(ValueType::I32), ValueType::I32)
        .expect("i32 array");
    let before = heap.vm_allocation_counters();
    for index in 0..OPERATIONS {
        heap.array_push(array, RuntimeValue::I32(i32::try_from(index).unwrap()))
            .expect("array push");
    }
    assert_eq!(heap.array_len(array), Ok(OPERATIONS));
    for expected in (0..OPERATIONS).rev() {
        assert_eq!(
            heap.array_pop(array),
            Ok(RuntimeValue::I32(i32::try_from(expected).unwrap()))
        );
    }
    assert_eq!(heap.array_len(array), Ok(0));
    assert_eq!(
        heap.vm_allocation_counters()
            .delta_since(before)
            .collection_relocation_bytes,
        0,
        "an unblocked typed i32 arena tail must extend in place"
    );
    heap.array_shrink_to_fit(array)
        .expect("release the empty array extent before reusing the typed arena");
    assert_eq!(heap.array_capacity(array), Ok(0));

    let map = heap
        .allocate_map(
            nexa_bytecode::map_type(ValueType::I32, ValueType::I32),
            ValueType::I32,
            ValueType::I32,
        )
        .expect("i32 map");
    let mut pending_attempts = 0_usize;
    for key in 0..MAP_ENTRIES {
        pending_attempts += set_map(
            &mut heap,
            map,
            RuntimeValue::I32(key),
            RuntimeValue::I32(key.wrapping_mul(3)),
        ) - 1;
    }
    assert_eq!(heap.map_len(map), Ok(usize::try_from(MAP_ENTRIES).unwrap()));
    assert!(
        pending_attempts > 100,
        "the corpus must cross several bounded rehash generations"
    );
    for key in [0, 1, 255, 4_096, MAP_ENTRIES - 1] {
        assert_eq!(
            heap.map_get(map, RuntimeValue::I32(key)),
            Ok(Some(RuntimeValue::I32(key.wrapping_mul(3))))
        );
    }
}

#[test]
fn array_and_map_class_references_survive_mid_cycle_publication() {
    let class_type = StableId::from_name("wp58::Node");
    let class_value = ValueType::Named(class_type);
    let mut heap = Heap::new_with_arena_limits(128, 1 << 20, 1_024, 4_096, 256);
    let array = heap
        .allocate_array(nexa_bytecode::array_type(class_value), class_value)
        .expect("class array");
    let map = heap
        .allocate_map(
            nexa_bytecode::map_type(ValueType::I32, class_value),
            ValueType::I32,
            class_value,
        )
        .expect("class map");
    let first = heap
        .allocate_class(class_type, &[RuntimeValue::I32(1)])
        .expect("first class");
    let second = heap
        .allocate_class(class_type, &[RuntimeValue::I32(2)])
        .expect("second class");
    heap.array_push(array, first)
        .expect("root first through array");
    set_map(&mut heap, map, RuntimeValue::I32(2), second);
    let roots = GcRoots {
        running_frames: vec![reference_of(array), reference_of(map)],
        ..GcRoots::default()
    };
    heap.collect_incremental(&roots, GcBudget::objects(1))
        .expect("enter incremental mark");
    assert_eq!(heap.gc_phase(), GcPhase::Mark);

    let third = heap
        .allocate_class(class_type, &[RuntimeValue::I32(3)])
        .expect("mark-phase class");
    let fourth = heap
        .allocate_class(class_type, &[RuntimeValue::I32(4)])
        .expect("mark-phase map class");
    heap.array_push(array, third)
        .expect("publish through array barrier");
    set_map(&mut heap, map, RuntimeValue::I32(4), fourth);

    let mut completed = false;
    for _ in 0..1_000 {
        if heap
            .collect_incremental(&roots, GcBudget::objects(1))
            .expect("incremental step")
            .completed
            .is_some()
        {
            completed = true;
            break;
        }
    }
    assert!(completed, "incremental cycle converges");
    for value in [first, second, third, fourth] {
        assert!(
            heap.resolve(reference_of(value)).is_ok(),
            "container-published class stays live"
        );
    }
}

#[test]
fn long_unicode_interpolation_uses_one_builder_and_one_publication() {
    let source = r#"
fn render(text: string, number: i32, glyph: rune, flag: bool) -> string {
    return "头:${text}:${number}:${glyph}:${flag}:尾";
}
"#;
    let module = nexa_compiler::compile(source).expect("compile builder corpus");
    let code = &module.module().functions[0].code;
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

    let input = "界🙂".repeat(10_000);
    let expected = format!("头:{input}:{}:{}:{}:尾", i32::MIN, '界', true);
    let mut heap = Heap::new_with_string_limit(32, expected.len() + 1);
    let input_reference = heap.allocate_string(&input).expect("long input string");
    for literal in &module.module().strings {
        heap.load_string_literal(literal).expect("warm literal");
    }
    let before = heap.vm_allocation_counters();
    let outcome = CheckedInterpreter::run_with_heap(
        &module,
        0,
        &[
            RuntimeValue::String {
                reference: input_reference,
                hash: heap.string_hash(input_reference).unwrap(),
            },
            RuntimeValue::I32(i32::MIN),
            RuntimeValue::Rune(u32::from('界')),
            RuntimeValue::Bool(true),
        ],
        100_000,
        &mut heap,
    )
    .expect("execute long interpolation");
    let nexa_runtime::InterpreterOutcome::Returned {
        value: Some(RuntimeValue::String { reference, .. }),
        ..
    } = outcome
    else {
        panic!("long interpolation must return a string");
    };
    assert_eq!(heap.string(reference), Ok(expected.as_str()));
    let delta = heap.vm_allocation_counters().delta_since(before);
    assert_eq!(delta.string_allocations, 1);
    assert_eq!(
        delta.string_copy_bytes,
        u64::try_from(expected.len()).unwrap()
    );
}

#[test]
fn retired_execution_image_releases_its_string_constant_pool() {
    let module = nexa_compiler::compile(r#"fn text() -> string { return "reload-owned-常量"; }"#)
        .expect("compile constant pool fixture");
    let executable =
        ExecutableModule::build(&module, OpcodeCostTable::canonical()).expect("execution image");
    let constant = &executable.pooled_string(0).expect("pooled literal").1.value;
    let weak = Arc::downgrade(constant);
    assert_eq!(Arc::strong_count(constant), 1);
    drop(executable);
    assert!(
        weak.upgrade().is_none(),
        "retiring an old reload execution image releases its constant pool"
    );
}
