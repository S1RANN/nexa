use nexa_core::StableId;
use nexa_runtime::{GcRef, GcRoots, Heap, HeapError, Object, RuntimeValue};

fn class_reference(value: RuntimeValue) -> GcRef {
    let RuntimeValue::NamedRef { reference, .. } = value else {
        panic!("expected a class reference");
    };
    reference
}

#[test]
fn struct_updates_copy_values_and_enum_payloads_keep_their_tag() {
    let mut heap = Heap::new(8);
    let point = StableId::from_name("object-model-v2::Point");
    let original = heap
        .allocate_struct(point, &[RuntimeValue::I32(2), RuntimeValue::I32(3)])
        .expect("allocate point");
    let copied = original;
    let updated = heap
        .struct_with(copied, 0, RuntimeValue::I32(10))
        .expect("copy-update point");

    assert_eq!(
        heap.struct_field(original, 0).expect("original x"),
        RuntimeValue::I32(2)
    );
    assert_eq!(
        heap.struct_field(updated, 0).expect("updated x"),
        RuntimeValue::I32(10)
    );
    assert_ne!(original, updated);

    let option = StableId::from_name("object-model-v2::OptionPoint");
    let some = StableId::from_name("object-model-v2::OptionPoint::Some");
    let tagged = heap
        .allocate_enum(option, some, 1, Some(updated))
        .expect("allocate tagged payload");
    assert_eq!(heap.enum_tag(tagged).expect("enum tag"), 1);
    assert_eq!(
        heap.enum_payload(tagged, some).expect("enum payload"),
        updated
    );
}

#[test]
fn class_copy_aliases_fields_and_equality_uses_object_identity() {
    let mut heap = Heap::new(8);
    let node = StableId::from_name("object-model-v2::Node");
    let original = heap
        .allocate_class(node, &[RuntimeValue::I32(1)])
        .expect("allocate original");
    let alias = original;
    let distinct = heap
        .allocate_class(node, &[RuntimeValue::I32(1)])
        .expect("allocate structurally equal instance");

    assert!(heap.class_equal(original, alias).expect("same identity"));
    assert!(
        !heap
            .class_equal(original, distinct)
            .expect("different identity")
    );

    heap.set_class_field(alias, 0, RuntimeValue::I32(7))
        .expect("mutate through alias");
    assert_eq!(
        heap.class_field(original, 0)
            .expect("read through original"),
        RuntimeValue::I32(7)
    );
}

#[test]
fn class_write_barrier_is_atomic_and_retains_the_published_child() {
    let mut heap = Heap::new(8);
    let node = StableId::from_name("object-model-v2::BarrierNode");
    let parent = heap
        .allocate_class(node, &[RuntimeValue::Unit])
        .expect("allocate parent");
    let child = heap
        .allocate_class(node, &[RuntimeValue::I32(9)])
        .expect("allocate child");

    let stale = RuntimeValue::NamedRef {
        reference: GcRef {
            index: u32::MAX,
            generation: u32::MAX,
        },
        type_id: node,
    };
    assert!(matches!(
        heap.set_class_field(parent, 0, stale),
        Err(HeapError::InvalidReference(_))
    ));
    assert_eq!(
        heap.class_field(parent, 0).expect("failed write is atomic"),
        RuntimeValue::Unit
    );

    heap.set_class_field(parent, 0, child)
        .expect("publish child through barrier");
    let roots = GcRoots {
        running_frames: vec![class_reference(parent)],
        ..GcRoots::default()
    };
    let stats = heap.collect(&roots).expect("collect rooted graph");
    assert_eq!(stats.live, 2);
    assert!(matches!(
        heap.resolve(class_reference(child)),
        Ok(Object::Class { .. })
    ));
}

#[test]
fn rooted_class_cycle_survives_and_unrooted_cycle_is_reclaimed() {
    let mut heap = Heap::new(8);
    let node = StableId::from_name("object-model-v2::CycleNode");
    let first = heap
        .allocate_class(node, &[RuntimeValue::Unit])
        .expect("allocate first");
    let second = heap
        .allocate_class(node, &[first])
        .expect("allocate second");
    heap.set_class_field(first, 0, second).expect("close cycle");

    let roots = GcRoots {
        suspended_tasks: vec![class_reference(first)],
        ..GcRoots::default()
    };
    assert_eq!(heap.collect(&roots).expect("collect rooted cycle").live, 2);

    let stats = heap
        .collect(&GcRoots::default())
        .expect("collect unrooted cycle");
    assert_eq!(stats.reclaimed, 2);
    assert_eq!(stats.live, 0);
}

#[test]
fn enum_and_struct_composites_trace_nested_class_references_exactly() {
    let mut heap = Heap::new(8);
    let class_type = StableId::from_name("object-model-v2::Leaf");
    let struct_type = StableId::from_name("object-model-v2::Wrapper");
    let option_type = StableId::from_name("object-model-v2::OptionLeaf");
    let some_variant = StableId::from_name("object-model-v2::OptionLeaf::Some");

    let class = heap
        .allocate_class(class_type, &[RuntimeValue::I32(11)])
        .expect("allocate class");
    let structure = heap
        .allocate_struct(struct_type, &[class])
        .expect("allocate struct containing class");
    let option = heap
        .allocate_enum(option_type, some_variant, 1, Some(structure))
        .expect("allocate Option-like enum containing struct");
    let RuntimeValue::NamedRef {
        reference: option_root,
        ..
    } = option
    else {
        panic!("enum must be GC-backed");
    };

    let roots = GcRoots {
        module_globals: vec![option_root],
        ..GcRoots::default()
    };
    let stats = heap.collect(&roots).expect("trace composite root");
    assert_eq!(stats.live, 3);
    assert!(matches!(
        heap.resolve(class_reference(class)),
        Ok(Object::Class { .. })
    ));

    let stats = heap
        .collect(&GcRoots::default())
        .expect("collect composite graph");
    assert_eq!(stats.reclaimed, 3);
    assert_eq!(stats.live, 0);
}
