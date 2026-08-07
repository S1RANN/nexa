//! Compiler-provided Buffer operations.

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor, TypeDescriptor, TypeKind,
};

const SOURCE: &str = include_str!("sources/buffer.nexa");

const TYPES: &[TypeDescriptor] = &[TypeDescriptor {
    name: "Buffer",
    kind: TypeKind::Struct,
    fields: &[],
    contract: "compiler-provided fixed-length buffer",
}];

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "buffer_is_empty",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("buffer", "Buffer<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::BufferIsEmpty),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when the buffer has no elements",
    },
    FunctionDescriptor {
        name: "buffer_fill",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("buffer", "Buffer<T>"),
            ParameterDescriptor::new("value", "T"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::BufferFill),
        behavior: FunctionBehavior::MUTATES,
        contract: "fills every element with value and returns true",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "buffer",
    path: "std.buffer",
    prelude: false,
    source: SOURCE,
    types: TYPES,
    functions: FUNCTIONS,
};

#[must_use]
pub fn buffer_is_empty<T>(values: &[T]) -> bool {
    values.is_empty()
}

pub fn buffer_fill<T: Clone>(values: &mut [T], value: T) -> bool {
    values.fill(value);
    true
}