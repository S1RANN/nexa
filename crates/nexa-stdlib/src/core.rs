//! Pure core reference operations.

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor,
};

const SOURCE: &str = include_str!("sources/core.nexa");

macro_rules! numeric_descriptor {
    ($name:literal, $type:literal, $contract:literal) => {
        FunctionDescriptor {
            name: $name,
            type_parameters: &[],
            parameters: &[
                ParameterDescriptor::new("left", $type),
                ParameterDescriptor::new("right", $type),
            ],
            result: $type,
            lowering: Lowering::EmbeddedSource,
            behavior: FunctionBehavior::TOTAL,
            contract: $contract,
        }
    };
}

macro_rules! conversion_descriptor {
    ($name:literal, $type:literal, $intrinsic:expr, $behavior:expr, $contract:literal) => {
        FunctionDescriptor {
            name: $name,
            type_parameters: &[],
            parameters: &[ParameterDescriptor::new("value", $type)],
            result: "string",
            lowering: Lowering::CompilerIntrinsic($intrinsic),
            behavior: $behavior,
            contract: $contract,
        }
    };
}

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "is_some",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("value", "Option<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::OptionIsSome),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when value is Some",
    },
    FunctionDescriptor {
        name: "is_none",
        type_parameters: &["T"],
        parameters: &[ParameterDescriptor::new("value", "Option<T>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::OptionIsNone),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when value is None",
    },
    FunctionDescriptor {
        name: "is_ok",
        type_parameters: &["T", "E"],
        parameters: &[ParameterDescriptor::new("value", "Result<T,E>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ResultIsOk),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when value is Ok",
    },
    FunctionDescriptor {
        name: "is_err",
        type_parameters: &["T", "E"],
        parameters: &[ParameterDescriptor::new("value", "Result<T,E>")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ResultIsErr),
        behavior: FunctionBehavior::TOTAL,
        contract: "true exactly when value is Err",
    },
    FunctionDescriptor {
        name: "option_unwrap_or",
        type_parameters: &["T"],
        parameters: &[
            ParameterDescriptor::new("value", "Option<T>"),
            ParameterDescriptor::new("fallback", "T"),
        ],
        result: "T",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::OptionUnwrapOr),
        behavior: FunctionBehavior::TOTAL,
        contract: "Some payload, otherwise fallback",
    },
    FunctionDescriptor {
        name: "result_unwrap_or",
        type_parameters: &["T", "E"],
        parameters: &[
            ParameterDescriptor::new("value", "Result<T,E>"),
            ParameterDescriptor::new("fallback", "T"),
        ],
        result: "T",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::ResultUnwrapOr),
        behavior: FunctionBehavior::TOTAL,
        contract: "Ok payload, otherwise fallback",
    },
    numeric_descriptor!("min_i32", "i32", "the lesser operand; ties return right"),
    numeric_descriptor!("max_i32", "i32", "the greater operand; ties return right"),
    numeric_descriptor!("min_i64", "i64", "the lesser operand; ties return right"),
    numeric_descriptor!("max_i64", "i64", "the greater operand; ties return right"),
    numeric_descriptor!(
        "min_f32",
        "f32",
        "left when left is less than right, otherwise right"
    ),
    numeric_descriptor!(
        "max_f32",
        "f32",
        "left when left is greater than right, otherwise right"
    ),
    numeric_descriptor!(
        "min_f64",
        "f64",
        "left when left is less than right, otherwise right"
    ),
    numeric_descriptor!(
        "max_f64",
        "f64",
        "left when left is greater than right, otherwise right"
    ),
    conversion_descriptor!(
        "to_string_string",
        "string",
        Intrinsic::StringToString,
        FunctionBehavior::TOTAL,
        "identity conversion preserving the exact string value"
    ),
    conversion_descriptor!(
        "to_string_i32",
        "i32",
        Intrinsic::I32ToString,
        FunctionBehavior::ALLOCATES,
        "locale-free base-10 representation"
    ),
    conversion_descriptor!(
        "to_string_i64",
        "i64",
        Intrinsic::I64ToString,
        FunctionBehavior::ALLOCATES,
        "locale-free base-10 representation"
    ),
    conversion_descriptor!(
        "to_string_f32",
        "f32",
        Intrinsic::F32ToString,
        FunctionBehavior::ALLOCATES,
        "locale-free shortest round-tripping decimal representation"
    ),
    conversion_descriptor!(
        "to_string_f64",
        "f64",
        Intrinsic::F64ToString,
        FunctionBehavior::ALLOCATES,
        "locale-free shortest round-tripping decimal representation"
    ),
    conversion_descriptor!(
        "to_string_bool",
        "bool",
        Intrinsic::BoolToString,
        FunctionBehavior::ALLOCATES,
        "true or false in lowercase ASCII"
    ),
    conversion_descriptor!(
        "to_string_rune",
        "rune",
        Intrinsic::RuneToString,
        FunctionBehavior::ALLOCATES,
        "the single Unicode scalar encoded as UTF-8"
    ),
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "core",
    path: "std.core",
    prelude: false,
    source: SOURCE,
    types: &[],
    functions: FUNCTIONS,
};

#[must_use]
pub const fn is_some<T>(value: &Option<T>) -> bool {
    value.is_some()
}

#[must_use]
pub const fn is_none<T>(value: &Option<T>) -> bool {
    value.is_none()
}

#[must_use]
pub const fn is_ok<T, E>(value: &Result<T, E>) -> bool {
    value.is_ok()
}

#[must_use]
pub const fn is_err<T, E>(value: &Result<T, E>) -> bool {
    value.is_err()
}

#[must_use]
pub fn option_unwrap_or<T>(value: Option<T>, fallback: T) -> T {
    value.unwrap_or(fallback)
}

#[must_use]
pub fn result_unwrap_or<T, E>(value: Result<T, E>, fallback: T) -> T {
    value.unwrap_or(fallback)
}

macro_rules! numeric_operations {
    ($min:ident, $max:ident, $type:ty) => {
        #[must_use]
        pub const fn $min(left: $type, right: $type) -> $type {
            if left < right { left } else { right }
        }

        #[must_use]
        pub const fn $max(left: $type, right: $type) -> $type {
            if left > right { left } else { right }
        }
    };
}

numeric_operations!(min_i32, max_i32, i32);
numeric_operations!(min_i64, max_i64, i64);

#[must_use]
pub fn min_f32(left: f32, right: f32) -> f32 {
    if left < right { left } else { right }
}

#[must_use]
pub fn max_f32(left: f32, right: f32) -> f32 {
    if left > right { left } else { right }
}

#[must_use]
pub fn min_f64(left: f64, right: f64) -> f64 {
    if left < right { left } else { right }
}

#[must_use]
pub fn max_f64(left: f64, right: f64) -> f64 {
    if left > right { left } else { right }
}

#[must_use]
pub const fn to_string_string(value: &str) -> &str {
    value
}

#[must_use]
pub fn to_string_i32(value: i32) -> String {
    value.to_string()
}

#[must_use]
pub fn to_string_i64(value: i64) -> String {
    value.to_string()
}

#[must_use]
pub fn to_string_f32(value: f32) -> String {
    value.to_string()
}

#[must_use]
pub fn to_string_f64(value: f64) -> String {
    value.to_string()
}

#[must_use]
pub fn to_string_bool(value: bool) -> String {
    value.to_string()
}

#[must_use]
pub fn to_string_rune(value: char) -> String {
    value.to_string()
}
