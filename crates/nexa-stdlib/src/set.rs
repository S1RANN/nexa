//! Compiler-provided nominal Set operations.

use std::collections::HashSet;
use std::hash::{BuildHasher, Hash};

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor, TypeDescriptor, TypeKind,
};

const SOURCE: &str = include_str!("sources/set.nexa");

const TYPES: &[TypeDescriptor] = &[TypeDescriptor {
    name: "Set",
    kind: TypeKind::Struct,
    fields: &[],
    contract: "compiler-provided nominal hash set",
}];

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "set_new",
        type_parameters: &["T"],
        parameters: &[],
        result: "Set<T>",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetNew),
        behavior: FunctionBehavior::ALLOCATES,
        contract: "empty set",
    },
    FunctionDescriptor {
        name: "set_len",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("set", "Set<T>")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetLen),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of elements in the set",
    },
    FunctionDescriptor {
        name: "set_contains",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("set", "Set<T>"),
            ParameterDescriptor::new("value", "T"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetContains),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when value is present",
    },
    FunctionDescriptor {
        name: "set_insert",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("set", "Set<T>"),
            ParameterDescriptor::new("value", "T"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetInsert),
        behavior: FunctionBehavior::MUTATES_AND_ALLOCATES,
        contract: "inserts value and returns true when the value was not already present",
    },
    FunctionDescriptor {
        name: "set_remove",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("set", "Set<T>"),
            ParameterDescriptor::new("value", "T"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetRemove),
        behavior: FunctionBehavior::MUTATES,
        contract: "removes value and returns true when the value was present",
    },
    FunctionDescriptor {
        name: "set_clear",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("set", "Set<T>")],
        result: "()",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::SetClear),
        behavior: FunctionBehavior::MUTATES,
        contract: "removes all elements",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "set",
    path: "std.set",
    prelude: false,
    source: SOURCE,
    types: TYPES,
    functions: FUNCTIONS,
};

#[must_use]
pub fn set_new<T>() -> HashSet<T> {
    HashSet::new()
}

#[must_use]
pub fn set_len<T, S: BuildHasher>(values: &HashSet<T, S>) -> usize {
    values.len()
}

#[must_use]
pub fn set_contains<T: Eq + Hash, S: BuildHasher>(values: &HashSet<T, S>, value: &T) -> bool {
    values.contains(value)
}

pub fn set_insert<T: Eq + Hash, S: BuildHasher>(values: &mut HashSet<T, S>, value: T) -> bool {
    values.insert(value)
}

pub fn set_remove<T: Eq + Hash, S: BuildHasher>(values: &mut HashSet<T, S>, value: &T) -> bool {
    values.remove(value)
}

pub fn set_clear<T, S: BuildHasher>(values: &mut HashSet<T, S>) {
    values.clear();
}
