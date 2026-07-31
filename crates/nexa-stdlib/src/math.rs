//! Pure numeric reference operations.

use std::fmt;

use nexa_core::deterministic_math;

use crate::{
    FieldDescriptor, FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor, TypeDescriptor, TypeKind,
};

const SOURCE: &str = include_str!("sources/math.nexa");

macro_rules! numeric_descriptor {
    ($name:literal, $type:literal, $behavior:expr, $contract:literal, [$($parameter:literal),+]) => {
        FunctionDescriptor {
            name: $name,
            type_parameters: &[],
            parameters: &[$(ParameterDescriptor::new($parameter, $type)),+],
            result: $type,
            lowering: Lowering::EmbeddedSource,
            behavior: $behavior,
            contract: $contract,
        }
    };
}

macro_rules! unary_intrinsic_descriptor {
    ($name:literal, $type:literal, $intrinsic:expr, $contract:literal) => {
        FunctionDescriptor {
            name: $name,
            type_parameters: &[],
            parameters: &[ParameterDescriptor::new("value", $type)],
            result: $type,
            lowering: Lowering::CompilerIntrinsic($intrinsic),
            behavior: FunctionBehavior::TOTAL,
            contract: $contract,
        }
    };
}

const TYPES: &[TypeDescriptor] = &[
    TypeDescriptor {
        name: "Vec2",
        kind: TypeKind::Struct,
        fields: &[
            FieldDescriptor::new("x", "f32"),
            FieldDescriptor::new("y", "f32"),
        ],
        contract: "two-dimensional f32 vector",
    },
    TypeDescriptor {
        name: "Vec3",
        kind: TypeKind::Struct,
        fields: &[
            FieldDescriptor::new("x", "f32"),
            FieldDescriptor::new("y", "f32"),
            FieldDescriptor::new("z", "f32"),
        ],
        contract: "three-dimensional f32 vector",
    },
];

const FUNCTIONS: &[FunctionDescriptor] = &[
    numeric_descriptor!(
        "clamp_i32",
        "i32",
        FunctionBehavior::TOTAL,
        "low when value is below low, high when above high, otherwise value",
        ["value", "low", "high"]
    ),
    numeric_descriptor!(
        "abs_i32",
        "i32",
        FunctionBehavior::MAY_TRAP,
        "absolute value; traps when value is i32 minimum",
        ["value"]
    ),
    numeric_descriptor!(
        "clamp_i64",
        "i64",
        FunctionBehavior::TOTAL,
        "low when value is below low, high when above high, otherwise value",
        ["value", "low", "high"]
    ),
    numeric_descriptor!(
        "abs_i64",
        "i64",
        FunctionBehavior::MAY_TRAP,
        "absolute value; traps when value is i64 minimum",
        ["value"]
    ),
    numeric_descriptor!(
        "clamp_f32",
        "f32",
        FunctionBehavior::TOTAL,
        "low when value is below low, high when above high, otherwise value",
        ["value", "low", "high"]
    ),
    numeric_descriptor!(
        "abs_f32",
        "f32",
        FunctionBehavior::TOTAL,
        "negates negative finite values and preserves all other bit patterns",
        ["value"]
    ),
    numeric_descriptor!(
        "clamp_f64",
        "f64",
        FunctionBehavior::TOTAL,
        "low when value is below low, high when above high, otherwise value",
        ["value", "low", "high"]
    ),
    numeric_descriptor!(
        "abs_f64",
        "f64",
        FunctionBehavior::TOTAL,
        "negates negative finite values and preserves all other bit patterns",
        ["value"]
    ),
    unary_intrinsic_descriptor!(
        "floor_f32",
        "f32",
        Intrinsic::F32Floor,
        "greatest integral f32 less than or equal to value"
    ),
    unary_intrinsic_descriptor!(
        "floor_f64",
        "f64",
        Intrinsic::F64Floor,
        "greatest integral f64 less than or equal to value"
    ),
    unary_intrinsic_descriptor!(
        "ceil_f32",
        "f32",
        Intrinsic::F32Ceil,
        "least integral f32 greater than or equal to value"
    ),
    unary_intrinsic_descriptor!(
        "ceil_f64",
        "f64",
        Intrinsic::F64Ceil,
        "least integral f64 greater than or equal to value"
    ),
    unary_intrinsic_descriptor!(
        "round_f32",
        "f32",
        Intrinsic::F32Round,
        "nearest integral f32 with halfway cases away from zero"
    ),
    unary_intrinsic_descriptor!(
        "round_f64",
        "f64",
        Intrinsic::F64Round,
        "nearest integral f64 with halfway cases away from zero"
    ),
    unary_intrinsic_descriptor!(
        "sqrt_f32",
        "f32",
        Intrinsic::F32Sqrt,
        "principal square root, or NaN for a negative finite value"
    ),
    unary_intrinsic_descriptor!(
        "sqrt_f64",
        "f64",
        Intrinsic::F64Sqrt,
        "principal square root, or NaN for a negative finite value"
    ),
    unary_intrinsic_descriptor!(
        "sin_f32",
        "f32",
        Intrinsic::F32Sin,
        "sine of value measured in radians"
    ),
    unary_intrinsic_descriptor!(
        "sin_f64",
        "f64",
        Intrinsic::F64Sin,
        "sine of value measured in radians"
    ),
    unary_intrinsic_descriptor!(
        "cos_f32",
        "f32",
        Intrinsic::F32Cos,
        "cosine of value measured in radians"
    ),
    unary_intrinsic_descriptor!(
        "cos_f64",
        "f64",
        Intrinsic::F64Cos,
        "cosine of value measured in radians"
    ),
    FunctionDescriptor {
        name: "vec2",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("x", "f32"),
            ParameterDescriptor::new("y", "f32"),
        ],
        result: "Vec2",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "constructs Vec2 with the supplied components",
    },
    FunctionDescriptor {
        name: "vec2_add",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec2"),
            ParameterDescriptor::new("right", "Vec2"),
        ],
        result: "Vec2",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "component-wise vector addition",
    },
    FunctionDescriptor {
        name: "vec2_sub",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec2"),
            ParameterDescriptor::new("right", "Vec2"),
        ],
        result: "Vec2",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "component-wise vector subtraction",
    },
    FunctionDescriptor {
        name: "vec2_scale",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "Vec2"),
            ParameterDescriptor::new("factor", "f32"),
        ],
        result: "Vec2",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "multiplies every component by factor",
    },
    FunctionDescriptor {
        name: "vec2_dot",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec2"),
            ParameterDescriptor::new("right", "Vec2"),
        ],
        result: "f32",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::TOTAL,
        contract: "sum of component products",
    },
    FunctionDescriptor {
        name: "vec2_length",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("value", "Vec2")],
        result: "f32",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::TOTAL,
        contract: "Euclidean vector length",
    },
    FunctionDescriptor {
        name: "vec3",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("x", "f32"),
            ParameterDescriptor::new("y", "f32"),
            ParameterDescriptor::new("z", "f32"),
        ],
        result: "Vec3",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "constructs Vec3 with the supplied components",
    },
    FunctionDescriptor {
        name: "vec3_add",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec3"),
            ParameterDescriptor::new("right", "Vec3"),
        ],
        result: "Vec3",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "component-wise vector addition",
    },
    FunctionDescriptor {
        name: "vec3_sub",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec3"),
            ParameterDescriptor::new("right", "Vec3"),
        ],
        result: "Vec3",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "component-wise vector subtraction",
    },
    FunctionDescriptor {
        name: "vec3_scale",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "Vec3"),
            ParameterDescriptor::new("factor", "f32"),
        ],
        result: "Vec3",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::ALLOCATES,
        contract: "multiplies every component by factor",
    },
    FunctionDescriptor {
        name: "vec3_dot",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("left", "Vec3"),
            ParameterDescriptor::new("right", "Vec3"),
        ],
        result: "f32",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::TOTAL,
        contract: "sum of component products",
    },
    FunctionDescriptor {
        name: "vec3_length",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("value", "Vec3")],
        result: "f32",
        lowering: Lowering::EmbeddedSource,
        behavior: FunctionBehavior::TOTAL,
        contract: "Euclidean vector length",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "math",
    path: "std.math",
    prelude: false,
    source: SOURCE,
    types: TYPES,
    functions: FUNCTIONS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericError {
    AbsoluteValueOverflow { ty: &'static str },
}

impl fmt::Display for NumericError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AbsoluteValueOverflow { ty } => {
                write!(formatter, "absolute value overflows {ty}")
            }
        }
    }
}

impl std::error::Error for NumericError {}

macro_rules! integer_operations {
    ($clamp:ident, $abs:ident, $type:ty, $name:literal) => {
        #[must_use]
        pub const fn $clamp(value: $type, low: $type, high: $type) -> $type {
            if value < low {
                low
            } else if value > high {
                high
            } else {
                value
            }
        }

        pub const fn $abs(value: $type) -> Result<$type, NumericError> {
            match value.checked_abs() {
                Some(value) => Ok(value),
                None => Err(NumericError::AbsoluteValueOverflow { ty: $name }),
            }
        }
    };
}

integer_operations!(clamp_i32, abs_i32, i32, "i32");
integer_operations!(clamp_i64, abs_i64, i64, "i64");

macro_rules! float_operations {
    ($clamp:ident, $abs:ident, $type:ty) => {
        #[must_use]
        pub fn $clamp(value: $type, low: $type, high: $type) -> $type {
            if value < low {
                low
            } else if value > high {
                high
            } else {
                value
            }
        }

        #[must_use]
        pub fn $abs(value: $type) -> $type {
            if value < 0.0 { -value } else { value }
        }
    };
}

float_operations!(clamp_f32, abs_f32, f32);
float_operations!(clamp_f64, abs_f64, f64);

#[must_use]
pub fn floor_f32(value: f32) -> f32 {
    deterministic_math::floor_f32(value)
}

#[must_use]
pub fn floor_f64(value: f64) -> f64 {
    deterministic_math::floor_f64(value)
}

#[must_use]
pub fn ceil_f32(value: f32) -> f32 {
    deterministic_math::ceil_f32(value)
}

#[must_use]
pub fn ceil_f64(value: f64) -> f64 {
    deterministic_math::ceil_f64(value)
}

#[must_use]
pub fn round_f32(value: f32) -> f32 {
    deterministic_math::round_f32(value)
}

#[must_use]
pub fn round_f64(value: f64) -> f64 {
    deterministic_math::round_f64(value)
}

#[must_use]
pub fn sqrt_f32(value: f32) -> f32 {
    deterministic_math::sqrt_f32(value)
}

#[must_use]
pub fn sqrt_f64(value: f64) -> f64 {
    deterministic_math::sqrt_f64(value)
}

#[must_use]
pub fn sin_f32(value: f32) -> f32 {
    deterministic_math::sin_f32(value)
}

#[must_use]
pub fn sin_f64(value: f64) -> f64 {
    deterministic_math::sin_f64(value)
}

#[must_use]
pub fn cos_f32(value: f32) -> f32 {
    deterministic_math::cos_f32(value)
}

#[must_use]
pub fn cos_f64(value: f64) -> f64 {
    deterministic_math::cos_f64(value)
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

#[must_use]
pub const fn vec2(x: f32, y: f32) -> Vec2 {
    Vec2::new(x, y)
}

#[must_use]
pub const fn vec2_add(left: Vec2, right: Vec2) -> Vec2 {
    Vec2::new(left.x + right.x, left.y + right.y)
}

#[must_use]
pub const fn vec2_sub(left: Vec2, right: Vec2) -> Vec2 {
    Vec2::new(left.x - right.x, left.y - right.y)
}

#[must_use]
pub const fn vec2_scale(value: Vec2, factor: f32) -> Vec2 {
    Vec2::new(value.x * factor, value.y * factor)
}

#[must_use]
pub const fn vec2_dot(left: Vec2, right: Vec2) -> f32 {
    left.x * right.x + left.y * right.y
}

#[must_use]
pub fn vec2_length(value: Vec2) -> f32 {
    sqrt_f32(vec2_dot(value, value))
}

#[must_use]
pub const fn vec3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3::new(x, y, z)
}

#[must_use]
pub const fn vec3_add(left: Vec3, right: Vec3) -> Vec3 {
    Vec3::new(left.x + right.x, left.y + right.y, left.z + right.z)
}

#[must_use]
pub const fn vec3_sub(left: Vec3, right: Vec3) -> Vec3 {
    Vec3::new(left.x - right.x, left.y - right.y, left.z - right.z)
}

#[must_use]
pub const fn vec3_scale(value: Vec3, factor: f32) -> Vec3 {
    Vec3::new(value.x * factor, value.y * factor, value.z * factor)
}

#[must_use]
pub const fn vec3_dot(left: Vec3, right: Vec3) -> f32 {
    left.x * right.x + left.y * right.y + left.z * right.z
}

#[must_use]
pub fn vec3_length(value: Vec3) -> f32 {
    sqrt_f32(vec3_dot(value, value))
}
