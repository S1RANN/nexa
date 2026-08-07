//! Bytecode v8 focused wire tests: Set type metadata, Set/iteration
//! instruction and intrinsic roundtrips, and strict version rejection.

use nexa_bytecode::{
    BYTECODE_VERSION, CollectionIteratorKind, DecodeError, Function, FunctionBuilder,
    FunctionEffect, Instruction, IteratorStateRegisters, Module, ModuleBuilder, SetType, Signature,
    StandardIntrinsic, ValueType, set_type,
};
use nexa_core::{CanonicalValueType, StableId, canonical_set_type_id};

const TYPE_ID: StableId = StableId(0x5e7_a11_5e7_a11);

fn builder_with_set() -> ModuleBuilder {
    let mut builder = ModuleBuilder::new();
    builder.set_type(SetType::new(ValueType::I32));
    builder
}

fn function_with_set_code() -> Function {
    let mut builder = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        16,
    );
    builder
        .effect(FunctionEffect::Ordinary)
        .emit(Instruction::SetNew {
            type_id: TYPE_ID,
            dst: 1,
        })
        .emit(Instruction::SetLen { source: 1, dst: 2 })
        .emit(Instruction::SetContains {
            source: 1,
            value: 3,
            dst: 4,
        })
        .emit(Instruction::SetInsert {
            source: 1,
            value: 3,
            dst: 4,
        })
        .emit(Instruction::SetRemove {
            source: 1,
            value: 3,
            dst: 4,
        })
        .emit(Instruction::SetClear { source: 1 })
        .emit(Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::SetLen {
                element: ValueType::I32,
            },
            args_base: 1,
            args_count: 1,
            dst: 2,
        })
        .emit(Instruction::StandardIntrinsic {
            intrinsic: StandardIntrinsic::SetInsert {
                element: ValueType::String,
            },
            args_base: 1,
            args_count: 2,
            dst: 4,
        })
        .emit(Instruction::IterNew {
            kind: CollectionIteratorKind::Set {
                element: ValueType::I32,
            },
            state: IteratorStateRegisters {
                collection: 1,
                phase: 5,
                slot: 6,
                epoch: 7,
            },
        })
        .emit(Instruction::IterNext {
            kind: CollectionIteratorKind::Set {
                element: ValueType::I32,
            },
            state: IteratorStateRegisters {
                collection: 1,
                phase: 5,
                slot: 6,
                epoch: 7,
            },
            dst: 8,
        })
        .emit(Instruction::IterNext {
            kind: CollectionIteratorKind::Map {
                key: ValueType::String,
                value: ValueType::I32,
            },
            state: IteratorStateRegisters {
                collection: 1,
                phase: 5,
                slot: 6,
                epoch: 7,
            },
            dst: 8,
        })
        .emit(Instruction::IterNext {
            kind: CollectionIteratorKind::Range,
            state: IteratorStateRegisters {
                collection: 1,
                phase: 5,
                slot: 6,
                epoch: 7,
            },
            dst: 8,
        })
        .emit(Instruction::ReturnVoid);
    builder.finish().expect("fixture function builds")
}

#[test]
fn set_type_identity_is_canonical() {
    assert_eq!(
        set_type(ValueType::I32),
        SetType::new(ValueType::I32).type_id
    );
    assert_eq!(
        set_type(ValueType::String),
        canonical_set_type_id(CanonicalValueType::String)
    );
    assert_ne!(set_type(ValueType::I32), set_type(ValueType::String));
    assert_ne!(set_type(ValueType::I32), set_type(ValueType::F64));
}

#[test]
fn set_and_iteration_instructions_roundtrip_bytecode_v8() {
    let mut builder = builder_with_set();
    builder.function(function_with_set_code());
    let module = builder.finish();
    let bytes = module.encode();
    let decoded = Module::decode(&bytes).expect("v8 artifact decodes");
    assert_eq!(decoded, module);
    assert_eq!(decoded.set_types, module.set_types);
    assert_eq!(
        decoded.functions[0].code, module.functions[0].code,
        "Set and iteration instruction streams survive the v8 codec"
    );
}

#[test]
fn set_intrinsic_metadata_is_consistent() {
    let set = |element| ValueType::Named(set_type(element));
    assert_eq!(
        StandardIntrinsic::SetLen {
            element: ValueType::I32
        }
        .argument_count(),
        1
    );
    assert_eq!(
        StandardIntrinsic::SetLen {
            element: ValueType::I32
        }
        .argument_type(0),
        Some(set(ValueType::I32))
    );
    for intrinsic in [
        StandardIntrinsic::SetContains {
            element: ValueType::I32,
        },
        StandardIntrinsic::SetInsert {
            element: ValueType::I32,
        },
        StandardIntrinsic::SetRemove {
            element: ValueType::I32,
        },
    ] {
        assert_eq!(intrinsic.argument_count(), 2);
        assert_eq!(intrinsic.argument_type(0), Some(set(ValueType::I32)));
        assert_eq!(
            intrinsic.argument_type(1),
            Some(ValueType::I32),
            "set element is the second wire argument for {intrinsic:?}"
        );
        assert_eq!(intrinsic.result_type(), ValueType::Bool);
    }
    assert_eq!(StandardIntrinsic::WIRE_VARIANT_COUNT, 56);
}

#[test]
fn extended_collection_intrinsic_metadata_is_consistent() {
    let array = |element| ValueType::Named(nexa_bytecode::array_type(element));
    let map = |key, value| ValueType::Named(nexa_bytecode::map_type(key, value));
    let buffer = |element| ValueType::Named(nexa_bytecode::buffer_type(element));

    assert_eq!(
        StandardIntrinsic::ArrayFirst {
            element: ValueType::I32
        }
        .result_type(),
        ValueType::Named(nexa_bytecode::option_type(ValueType::I32).type_id)
    );
    assert_eq!(
        StandardIntrinsic::ArrayLast {
            element: ValueType::I32
        }
        .argument_type(0),
        Some(array(ValueType::I32))
    );
    let swap = StandardIntrinsic::ArraySwap {
        element: ValueType::I32,
    };
    assert_eq!(swap.argument_count(), 3);
    assert_eq!(swap.argument_type(0), Some(array(ValueType::I32)));
    assert_eq!(swap.argument_type(1), Some(ValueType::I32));
    assert_eq!(swap.argument_type(2), Some(ValueType::I32));
    assert!(swap.mutates_collection());
    let reverse = StandardIntrinsic::ArrayReverse {
        element: ValueType::I32,
    };
    assert_eq!(reverse.argument_count(), 1);
    assert!(reverse.mutates_collection());
    assert_eq!(reverse.result_type(), ValueType::Bool);

    let is_empty = StandardIntrinsic::MapIsEmpty {
        key: ValueType::String,
        value: ValueType::I32,
    };
    assert_eq!(is_empty.argument_count(), 1);
    assert_eq!(
        is_empty.argument_type(0),
        Some(map(ValueType::String, ValueType::I32))
    );
    assert_eq!(is_empty.result_type(), ValueType::Bool);
    let get_or = StandardIntrinsic::MapGetOr {
        key: ValueType::String,
        value: ValueType::I32,
    };
    assert_eq!(get_or.argument_count(), 3);
    assert_eq!(get_or.argument_type(1), Some(ValueType::String));
    assert_eq!(get_or.argument_type(2), Some(ValueType::I32));
    assert_eq!(get_or.result_type(), ValueType::I32);
    assert!(!get_or.mutates_collection());
    let insert_if_absent = StandardIntrinsic::MapInsertIfAbsent {
        key: ValueType::String,
        value: ValueType::I32,
    };
    assert_eq!(insert_if_absent.argument_count(), 3);
    assert!(insert_if_absent.mutates_collection());
    assert_eq!(insert_if_absent.result_type(), ValueType::Bool);

    let buffer_is_empty = StandardIntrinsic::BufferIsEmpty {
        element: ValueType::I32,
    };
    assert_eq!(buffer_is_empty.argument_count(), 1);
    assert_eq!(
        buffer_is_empty.argument_type(0),
        Some(buffer(ValueType::I32))
    );
    let fill = StandardIntrinsic::BufferFill {
        element: ValueType::I32,
    };
    assert_eq!(fill.argument_count(), 2);
    assert_eq!(fill.argument_type(1), Some(ValueType::I32));
    assert!(fill.mutates_collection());
}

#[test]
fn decoder_rejects_retired_wire_versions() {
    let module = builder_with_set().finish();
    let mut bytes = module.encode();
    bytes[4..6].copy_from_slice(&(BYTECODE_VERSION - 1).to_le_bytes());
    assert_eq!(
        Module::decode(&bytes),
        Err(DecodeError::UnsupportedVersion(BYTECODE_VERSION - 1))
    );
    let mut bytes = module.encode();
    bytes[4..6].copy_from_slice(&(BYTECODE_VERSION + 1).to_le_bytes());
    assert_eq!(
        Module::decode(&bytes),
        Err(DecodeError::UnsupportedVersion(BYTECODE_VERSION + 1))
    );
}
