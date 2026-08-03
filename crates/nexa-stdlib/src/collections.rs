//! Pure collection reference operations.

use std::collections::BTreeMap;
use std::fmt;

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor,
};

const SOURCE: &str = include_str!("sources/collections.nexa");

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "array_len",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayLen),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of array elements",
    },
    FunctionDescriptor {
        name: "array_is_empty",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayIsEmpty),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when the array has no elements",
    },
    FunctionDescriptor {
        name: "array_get",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("values", "Array<T>"),
            ParameterDescriptor::new("index", "i32"),
        ],
        result: "Option<T>",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayGet),
        behavior: FunctionBehavior::ALLOCATES,
        contract: "element at index, or None when index is outside the array",
    },
    FunctionDescriptor {
        name: "array_push",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("values", "Array<T>"),
            ParameterDescriptor::new("value", "T"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayPush),
        behavior: FunctionBehavior::MUTATES_AND_ALLOCATES,
        contract: "appends value and returns true",
    },
    FunctionDescriptor {
        name: "array_pop",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "T",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayPop),
        behavior: FunctionBehavior::MUTATES_OR_TRAPS,
        contract: "removes and returns the last element; traps when empty",
    },
    FunctionDescriptor {
        name: "array_reserve",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("values", "Array<T>"),
            ParameterDescriptor::new("additional", "i32"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayReserve),
        behavior: FunctionBehavior::MUTATES_ALLOCATES_OR_TRAPS,
        contract: "retains capacity for at least non-negative additional elements and returns true",
    },
    FunctionDescriptor {
        name: "array_capacity",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayCapacity),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of elements that fit without relocating storage",
    },
    FunctionDescriptor {
        name: "array_clear",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayClear),
        behavior: FunctionBehavior::MUTATES,
        contract: "removes all elements while retaining capacity and returns true",
    },
    FunctionDescriptor {
        name: "array_shrink_to_fit",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("values", "Array<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ArrayShrinkToFit),
        behavior: FunctionBehavior::MUTATES,
        contract: "releases unused capacity and returns true",
    },
    FunctionDescriptor {
        name: "map_len",
        type_parameters: &["K", "V"],
        parameters: &[ParameterDescriptor::new("values", "Map<K,V>")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::MapLen),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of map entries",
    },
    FunctionDescriptor {
        name: "map_contains",
        type_parameters: &["K", "V"],
        parameters: &[
            ParameterDescriptor::new("values", "Map<K,V>"),
            ParameterDescriptor::new("key", "K"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::MapContains),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when key is present",
    },
    FunctionDescriptor {
        name: "map_get",
        type_parameters: &["K", "V"],
        parameters: &[
            ParameterDescriptor::new("values", "Map<K,V>"),
            ParameterDescriptor::new("key", "K"),
        ],
        result: "Option<V>",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::MapGet),
        behavior: FunctionBehavior::ALLOCATES,
        contract: "value associated with key, or None",
    },
    FunctionDescriptor {
        name: "map_insert",
        type_parameters: &["K", "V"],
        parameters: &[
            ParameterDescriptor::new("values", "Map<K,V>"),
            ParameterDescriptor::new("key", "K"),
            ParameterDescriptor::new("value", "V"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::MapInsert),
        behavior: FunctionBehavior::MUTATES_AND_ALLOCATES,
        contract: "inserts or replaces key with value and returns true",
    },
    FunctionDescriptor {
        name: "map_remove",
        type_parameters: &["K", "V"],
        parameters: &[
            ParameterDescriptor::new("values", "Map<K,V>"),
            ParameterDescriptor::new("key", "K"),
        ],
        result: "Option<V>",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::MapRemove),
        behavior: FunctionBehavior::MUTATES_AND_ALLOCATES,
        contract: "removes and returns the value associated with key, or None",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "collections",
    path: "std.collections",
    prelude: false,
    source: SOURCE,
    types: &[],
    functions: FUNCTIONS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionError {
    EmptyArray,
}

impl fmt::Display for CollectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyArray => formatter.write_str("cannot pop an empty array"),
        }
    }
}

impl std::error::Error for CollectionError {}

#[must_use]
pub const fn array_len<T>(values: &[T]) -> usize {
    values.len()
}

#[must_use]
pub const fn array_is_empty<T>(values: &[T]) -> bool {
    values.is_empty()
}

#[must_use]
pub fn array_get<T>(values: &[T], index: usize) -> Option<&T> {
    values.get(index)
}

pub fn array_push<T>(values: &mut Vec<T>, value: T) -> bool {
    values.push(value);
    true
}

pub fn array_pop<T>(values: &mut Vec<T>) -> Result<T, CollectionError> {
    values.pop().ok_or(CollectionError::EmptyArray)
}

pub fn array_reserve<T>(values: &mut Vec<T>, additional: usize) -> bool {
    values.reserve(additional);
    true
}

#[must_use]
#[allow(clippy::ptr_arg)]
pub fn array_capacity<T>(values: &Vec<T>) -> usize {
    values.capacity()
}

pub fn array_clear<T>(values: &mut Vec<T>) -> bool {
    values.clear();
    true
}

pub fn array_shrink_to_fit<T>(values: &mut Vec<T>) -> bool {
    values.shrink_to_fit();
    true
}

#[must_use]
pub fn map_len<K, V>(values: &BTreeMap<K, V>) -> usize {
    values.len()
}

#[must_use]
pub fn map_contains<K: Ord, V>(values: &BTreeMap<K, V>, key: &K) -> bool {
    values.contains_key(key)
}

#[must_use]
pub fn map_get<'a, K: Ord, V>(values: &'a BTreeMap<K, V>, key: &K) -> Option<&'a V> {
    values.get(key)
}

pub fn map_insert<K: Ord, V>(values: &mut BTreeMap<K, V>, key: K, value: V) -> bool {
    values.insert(key, value);
    true
}

pub fn map_remove<K: Ord, V>(values: &mut BTreeMap<K, V>, key: &K) -> Option<V> {
    values.remove(key)
}
