//! `LayoutTable` derivation tests (WP19-WP22): determinism, nesting, precise
//! GC bitmaps, enum tag+payload ranges, and recursion rejection.

use nexa_bytecode::layout::{
    CopyStrategy, EqualityStrategy, FunctionAbi, LayoutError, LayoutTable, ModuleAbi,
    PhysicalSlotKind,
};
use nexa_bytecode::{
    ArrayType, ClassType, EnumType, EnumVariant, FunctionBuilder, Instruction, ModuleBuilder,
    Signature, StructField, StructType, ValueType, option_type,
};
use nexa_core::StableId;

fn field(name: &str, ty: ValueType) -> StructField {
    StructField {
        stable_id: StableId::from_name(name),
        ty,
    }
}

fn module_with_nested_types() -> nexa_bytecode::Module {
    let inner = StableId::from_name("Inner");
    let outer = StableId::from_name("Outer");
    let node = StableId::from_name("Node");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.class_type(ClassType {
        type_id: node,
        fields: vec![field("Node::value", ValueType::I32)],
    });
    builder.struct_type(StructType {
        type_id: inner,
        fields: vec![
            field("Inner::scalar", ValueType::I64),
            field("Inner::label", ValueType::String),
        ],
    });
    // Outer nests a struct, a class reference, and an Option<class> enum.
    builder.enum_type(option_type(ValueType::Named(node)));
    let option_node = option_type(ValueType::Named(node)).type_id;
    builder.struct_type(StructType {
        type_id: outer,
        fields: vec![
            field("Outer::flag", ValueType::Bool),
            field("Outer::inner", ValueType::Named(inner)),
            field("Outer::target", ValueType::Named(node)),
            field("Outer::maybe", ValueType::Named(option_node)),
        ],
    });
    builder.array_type(ArrayType::new(ValueType::I32));
    builder.finish()
}

#[test]
fn layout_table_is_deterministic_and_flattens_nested_aggregates() {
    let module = module_with_nested_types();
    let first = LayoutTable::for_module(&module).expect("layout derivation");
    let second = LayoutTable::for_module(&module).expect("layout derivation");
    assert_eq!(first, second, "two derivations must be identical");

    let outer = first
        .layout_of(ValueType::Named(StableId::from_name("Outer")))
        .expect("outer layout");
    // bool + (i64 + string) + class-ref + (tag + payload class-ref)
    assert_eq!(outer.physical_slots, 6);
    assert_eq!(
        outer.slot_kinds,
        vec![
            PhysicalSlotKind::Bool,
            PhysicalSlotKind::I64,
            PhysicalSlotKind::GcReference,
            PhysicalSlotKind::GcReference,
            PhysicalSlotKind::I32,
            PhysicalSlotKind::GcReference,
        ]
    );
    assert_eq!(
        outer.gc_bitmap,
        vec![false, false, true, true, false, true],
        "only reference slots are traced (WP26 input)"
    );
    assert_eq!(outer.copy_strategy, CopyStrategy::SlotMemcpy);
    assert_eq!(outer.equality_strategy, EqualityStrategy::StructFieldwise);

    let offsets = &outer.field_offsets;
    assert_eq!(offsets.len(), 4);
    assert_eq!((offsets[0].offset, offsets[0].slots), (0, 1));
    assert_eq!((offsets[1].offset, offsets[1].slots), (1, 2));
    assert_eq!(
        offsets[1].logical_type,
        ValueType::Named(StableId::from_name("Inner"))
    );
    assert_eq!((offsets[2].offset, offsets[2].slots), (3, 1));
    assert_eq!((offsets[3].offset, offsets[3].slots), (4, 2));
}

#[test]
fn enum_layout_uses_tag_plus_widest_payload_with_per_variant_bitmaps() {
    let module = module_with_nested_types();
    let table = LayoutTable::for_module(&module).expect("layout derivation");
    let node = StableId::from_name("Node");
    let option_node = option_type(ValueType::Named(node)).type_id;
    let layout = table
        .layout_of(ValueType::Named(option_node))
        .expect("option layout");
    let enum_layout = layout.enum_layout.expect("enum layout present");
    assert_eq!(layout.physical_slots, 2);
    assert_eq!(enum_layout.tag_offset, 0);
    assert_eq!(enum_layout.payload_offset, 1);
    assert_eq!(enum_layout.payload_slots, 1);
    // None carries no payload: its bitmap is empty, so inactive payload
    // slots never reach root scanning (WP28).
    let none_variant = &enum_layout.variants[0];
    assert_eq!(none_variant.payload_slots, 0);
    assert!(none_variant.payload_gc_bitmap.is_empty());
    let some_variant = &enum_layout.variants[1];
    assert_eq!(some_variant.payload_slots, 1);
    assert_eq!(some_variant.payload_gc_bitmap, vec![true]);
}

#[test]
fn class_and_collection_types_stay_single_reference_slots() {
    let module = module_with_nested_types();
    let table = LayoutTable::for_module(&module).expect("layout derivation");
    let class_layout = table
        .layout_of(ValueType::Named(StableId::from_name("Node")))
        .expect("class layout");
    assert_eq!(class_layout.physical_slots, 1);
    assert_eq!(class_layout.slot_kinds, vec![PhysicalSlotKind::GcReference]);
    assert_eq!(class_layout.copy_strategy, CopyStrategy::ReferenceShare);
    assert_eq!(
        class_layout.equality_strategy,
        EqualityStrategy::ReferenceIdentity
    );

    let array_layout = table
        .layout_of(ValueType::Named(nexa_bytecode::array_type(ValueType::I32)))
        .expect("array layout");
    assert_eq!(array_layout.physical_slots, 1);
    assert!(array_layout.gc_bitmap[0]);
}

#[test]
fn recursive_struct_nesting_is_rejected() {
    let cyclic = StableId::from_name("Cyclic");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.struct_type(StructType {
        type_id: cyclic,
        fields: vec![field("Cyclic::next", ValueType::Named(cyclic))],
    });
    let module = builder.finish();
    assert_eq!(
        LayoutTable::for_module(&module),
        Err(LayoutError::RecursiveValueType(cyclic))
    );
}

#[test]
fn bytecode_v7_rejects_dangling_field_types() {
    let dangling = StableId::from_name("Dangling");
    let holder = StableId::from_name("Holder");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.struct_type(StructType {
        type_id: holder,
        fields: vec![field("Holder::value", ValueType::Named(dangling))],
    });
    let module = builder.finish();
    assert_eq!(
        LayoutTable::for_module(&module),
        Err(LayoutError::UnknownType(dangling))
    );
}

#[test]
fn host_opaque_scalars_are_explicit_non_gc_slots() {
    let entity = StableId::from_name("host::Entity");
    let wrapper = StableId::from_name("EntityWrapper");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.opaque_type(entity);
    builder.struct_type(StructType {
        type_id: wrapper,
        fields: vec![field("EntityWrapper::entity", ValueType::Named(entity))],
    });
    let module = builder.finish();
    let encoded = module.encode();
    let module = nexa_bytecode::Module::decode(&encoded).expect("opaque type wire round-trip");
    assert_eq!(module.opaque_types, vec![entity]);
    let table = LayoutTable::for_module(&module).expect("explicit opaque layout");
    let opaque = table
        .layout_of(ValueType::Named(entity))
        .expect("opaque scalar");
    assert_eq!(opaque.slot_kinds, vec![PhysicalSlotKind::Opaque]);
    assert_eq!(opaque.gc_bitmap, vec![false]);
    let wrapper = table
        .layout_of(ValueType::Named(wrapper))
        .expect("opaque-containing struct");
    assert_eq!(wrapper.slot_kinds, vec![PhysicalSlotKind::Opaque]);
    assert_eq!(wrapper.gc_bitmap, vec![false]);
}

#[test]
fn module_abi_rejects_a_dangling_signature_type() {
    let dangling = StableId::from_name("DanglingParameter");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: vec![ValueType::Named(dangling)],
            result: None,
        },
        1,
    );
    function.emit(Instruction::ReturnVoid);
    builder.function(function.finish().expect("syntactic function"));
    let module = builder.finish();
    let table = LayoutTable::for_module(&module).expect("empty type table derives");
    assert_eq!(
        ModuleAbi::for_module(&module, &table),
        Err(LayoutError::UnknownType(dangling))
    );
}

#[test]
fn enum_payload_union_keeps_active_variant_root_metadata_exact() {
    let node = StableId::from_name("UnionNode");
    let mixed = StableId::from_name("Mixed");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.class_type(ClassType {
        type_id: node,
        fields: vec![],
    });
    builder.enum_type(EnumType {
        type_id: mixed,
        variants: vec![
            EnumVariant {
                stable_id: StableId::from_parts(&["Mixed", "::Bits"]),
                tag: 0,
                payload_type: Some(ValueType::I64),
            },
            EnumVariant {
                stable_id: StableId::from_parts(&["Mixed", "::Node"]),
                tag: 1,
                payload_type: Some(ValueType::Named(node)),
            },
        ],
    });
    let module = builder.finish();
    let table = LayoutTable::for_module(&module).expect("mixed enum layout");
    let layout = table
        .layout_of(ValueType::Named(mixed))
        .expect("mixed enum");
    assert_eq!(
        layout.slot_kinds,
        vec![PhysicalSlotKind::I32, PhysicalSlotKind::Opaque],
        "a variant-dependent slot is not assigned a forged fixed kind"
    );
    assert_eq!(
        layout.gc_bitmap,
        vec![false, true],
        "the top-level bitmap records that the payload may contain a root"
    );
    let variants = &layout.enum_layout.expect("enum metadata").variants;
    assert_eq!(variants[0].payload_slot_kinds, vec![PhysicalSlotKind::I64]);
    assert_eq!(variants[0].payload_gc_bitmap, vec![false]);
    assert_eq!(
        variants[1].payload_slot_kinds,
        vec![PhysicalSlotKind::GcReference]
    );
    assert_eq!(variants[1].payload_gc_bitmap, vec![true]);
}

#[test]
fn scalar_layouts_carry_float_aware_equality_and_no_gc_slots() {
    let module = module_with_nested_types();
    let table = LayoutTable::for_module(&module).expect("layout derivation");
    let f64_layout = table.layout_of(ValueType::F64).expect("f64 layout");
    assert_eq!(f64_layout.physical_slots, 1);
    assert_eq!(f64_layout.equality_strategy, EqualityStrategy::FloatAware);
    assert!(!f64_layout.gc_bitmap[0]);
    let string_layout = table.layout_of(ValueType::String).expect("string layout");
    assert_eq!(
        string_layout.equality_strategy,
        EqualityStrategy::StringContent
    );
    assert!(string_layout.gc_bitmap[0]);
}

#[test]
fn function_abi_flattens_parameters_into_contiguous_slot_ranges() {
    let module = module_with_nested_types();
    let table = LayoutTable::for_module(&module).expect("layout derivation");
    let outer = StableId::from_name("Outer");
    let signature = Signature {
        parameters: vec![ValueType::I32, ValueType::Named(outer), ValueType::String],
        result: Some(ValueType::Named(outer)),
    };
    let abi = FunctionAbi::for_signature(&table, &signature).expect("function abi");
    assert_eq!(abi.parameters.len(), 3);
    assert_eq!(
        (abi.parameters[0].slot_offset, abi.parameters[0].slot_count),
        (0, 1)
    );
    // Outer flattens to six slots directly inside the argument range.
    assert_eq!(
        (abi.parameters[1].slot_offset, abi.parameters[1].slot_count),
        (1, 6)
    );
    assert_eq!(
        (abi.parameters[2].slot_offset, abi.parameters[2].slot_count),
        (7, 1)
    );
    assert_eq!(abi.parameter_slots, 8);
    assert_eq!(
        abi.parameter_gc_bitmap,
        vec![false, false, false, true, true, false, true, true]
    );
    let result = abi.result.expect("caller-allocated result range");
    assert_eq!(result.slot_count, 6);

    // Builtin runtime handle names never appear in module type tables but
    // still lay out as single handle slots.
    let request = FunctionAbi::for_signature(
        &table,
        &Signature {
            parameters: vec![ValueType::Named(StableId::from_name("HostRequest"))],
            result: None,
        },
    )
    .expect("builtin handle abi");
    assert_eq!(request.parameter_slots, 1);
    assert_eq!(request.parameter_gc_bitmap, vec![false]);

    let module_abi = ModuleAbi::for_module(&module, &table).expect("module abi");
    assert_eq!(module_abi.len(), module.functions.len());
}

#[test]
fn enum_variants_missing_payload_never_widen_the_range() {
    let empty = StableId::from_name("Signal");
    let mut builder = ModuleBuilder::new();
    builder.metadata(
        StableId::from_name("layout-host"),
        nexa_bytecode::StateSchema::default().fingerprint(),
    );
    builder.enum_type(EnumType {
        type_id: empty,
        variants: vec![
            EnumVariant {
                stable_id: StableId::from_parts(&["Signal", "::Go"]),
                tag: 0,
                payload_type: None,
            },
            EnumVariant {
                stable_id: StableId::from_parts(&["Signal", "::Stop"]),
                tag: 1,
                payload_type: None,
            },
        ],
    });
    let module = builder.finish();
    let table = LayoutTable::for_module(&module).expect("layout derivation");
    let layout = table
        .layout_of(ValueType::Named(empty))
        .expect("payloadless enum layout");
    assert_eq!(layout.physical_slots, 1, "tag slot only");
    assert_eq!(layout.gc_bitmap, vec![false]);
}
