use crate::{FunctionBehavior, Intrinsic, Lowering, MethodDescriptor, ParameterDescriptor};

macro_rules! method {
    ($receiver:literal, $name:literal, $module:literal, $implementation:expr,
     [$($parameter:expr),* $(,)?], $result:literal, $lowering:expr, $behavior:expr,
     $contract:literal) => {
        MethodDescriptor {
            receiver: $receiver,
            name: $name,
            implementation_module: $module,
            implementation: $implementation,
            parameters: &[$($parameter),*],
            result: $result,
            lowering: $lowering,
            behavior: $behavior,
            contract: $contract,
        }
    };
}

macro_rules! integer_methods {
    ($type:literal, $suffix:literal, $to_string:expr) => {
        &[
            method!(
                $type,
                "abs",
                "math",
                concat!("abs_", $suffix),
                [],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::MAY_TRAP,
                "absolute value; traps for the minimum signed value"
            ),
            method!(
                $type,
                "min",
                "core",
                concat!("min_", $suffix),
                [ParameterDescriptor::new("other", $type)],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "the lesser operand"
            ),
            method!(
                $type,
                "max",
                "core",
                concat!("max_", $suffix),
                [ParameterDescriptor::new("other", $type)],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "the greater operand"
            ),
            method!(
                $type,
                "clamp",
                "math",
                concat!("clamp_", $suffix),
                [
                    ParameterDescriptor::new("low", $type),
                    ParameterDescriptor::new("high", $type)
                ],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "clamps the receiver to the inclusive low and high bounds"
            ),
            method!(
                $type,
                "to_string",
                "core",
                concat!("to_string_", $suffix),
                [],
                "string",
                Lowering::CompilerIntrinsic($to_string),
                FunctionBehavior::ALLOCATES,
                "locale-free base-10 representation"
            ),
        ]
    };
}

macro_rules! float_methods {
    ($type:literal, $suffix:literal, $to_string:expr,
     $floor:expr, $ceil:expr, $round:expr, $sqrt:expr, $sin:expr, $cos:expr) => {
        &[
            method!(
                $type,
                "abs",
                "math",
                concat!("abs_", $suffix),
                [],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "negates negative finite values and preserves all other bit patterns"
            ),
            method!(
                $type,
                "min",
                "core",
                concat!("min_", $suffix),
                [ParameterDescriptor::new("other", $type)],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "the lesser operand"
            ),
            method!(
                $type,
                "max",
                "core",
                concat!("max_", $suffix),
                [ParameterDescriptor::new("other", $type)],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "the greater operand"
            ),
            method!(
                $type,
                "clamp",
                "math",
                concat!("clamp_", $suffix),
                [
                    ParameterDescriptor::new("low", $type),
                    ParameterDescriptor::new("high", $type)
                ],
                $type,
                Lowering::EmbeddedSource,
                FunctionBehavior::TOTAL,
                "clamps the receiver to the inclusive low and high bounds"
            ),
            method!(
                $type,
                "floor",
                "math",
                concat!("floor_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($floor),
                FunctionBehavior::TOTAL,
                "greatest integral value less than or equal to the receiver"
            ),
            method!(
                $type,
                "ceil",
                "math",
                concat!("ceil_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($ceil),
                FunctionBehavior::TOTAL,
                "least integral value greater than or equal to the receiver"
            ),
            method!(
                $type,
                "round",
                "math",
                concat!("round_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($round),
                FunctionBehavior::TOTAL,
                "nearest integral value with halfway cases away from zero"
            ),
            method!(
                $type,
                "sqrt",
                "math",
                concat!("sqrt_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($sqrt),
                FunctionBehavior::TOTAL,
                "principal square root"
            ),
            method!(
                $type,
                "sin",
                "math",
                concat!("sin_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($sin),
                FunctionBehavior::TOTAL,
                "sine of the receiver measured in radians"
            ),
            method!(
                $type,
                "cos",
                "math",
                concat!("cos_", $suffix),
                [],
                $type,
                Lowering::CompilerIntrinsic($cos),
                FunctionBehavior::TOTAL,
                "cosine of the receiver measured in radians"
            ),
            method!(
                $type,
                "to_string",
                "core",
                concat!("to_string_", $suffix),
                [],
                "string",
                Lowering::CompilerIntrinsic($to_string),
                FunctionBehavior::ALLOCATES,
                "locale-free shortest round-tripping decimal representation"
            ),
        ]
    };
}

pub(crate) const METHOD_GROUPS: &[&[MethodDescriptor]] = &[
    integer_methods!("i32", "i32", Intrinsic::I32ToString),
    integer_methods!("i64", "i64", Intrinsic::I64ToString),
    float_methods!(
        "f32",
        "f32",
        Intrinsic::F32ToString,
        Intrinsic::F32Floor,
        Intrinsic::F32Ceil,
        Intrinsic::F32Round,
        Intrinsic::F32Sqrt,
        Intrinsic::F32Sin,
        Intrinsic::F32Cos
    ),
    float_methods!(
        "f64",
        "f64",
        Intrinsic::F64ToString,
        Intrinsic::F64Floor,
        Intrinsic::F64Ceil,
        Intrinsic::F64Round,
        Intrinsic::F64Sqrt,
        Intrinsic::F64Sin,
        Intrinsic::F64Cos
    ),
];
