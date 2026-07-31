//! Deterministic debug traps without output or ambient logging.

use std::fmt;

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor,
};

const SOURCE: &str = include_str!("sources/debug.nexa");

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "assert",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("condition", "bool")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::DebugAssert),
        behavior: FunctionBehavior::MAY_TRAP,
        contract: "returns true when condition is true; otherwise traps with the canonical message",
    },
    FunctionDescriptor {
        name: "trap",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("message", "string")],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::DebugTrap),
        behavior: FunctionBehavior::ALWAYS_TRAPS,
        contract: "always terminates evaluation with an explicit deterministic trap",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "debug",
    path: "std.debug",
    prelude: false,
    source: SOURCE,
    types: &[],
    functions: FUNCTIONS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    AssertionFailed,
    Explicit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trap {
    pub kind: TrapKind,
    pub message: String,
}

impl fmt::Display for Trap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Trap {}

/// Models `std.debug.assert` without panicking or writing output.
pub fn assert(condition: bool) -> Result<bool, Trap> {
    if condition {
        Ok(true)
    } else {
        Err(Trap {
            kind: TrapKind::AssertionFailed,
            message: "assertion failed".to_owned(),
        })
    }
}

/// Models `std.debug.trap` as data rather than panicking or writing output.
pub fn trap(message: impl Into<String>) -> Result<bool, Trap> {
    Err(Trap {
        kind: TrapKind::Explicit,
        message: message.into(),
    })
}
