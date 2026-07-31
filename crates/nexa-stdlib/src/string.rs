//! Pure UTF-8 string reference operations.
//!
//! Unless a function name explicitly contains `byte`, all lengths and indices
//! in this module count Unicode scalar values (`char` in Rust), not bytes or
//! grapheme clusters.

use std::fmt;

use crate::{
    FunctionBehavior, FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor,
    ParameterDescriptor,
};

const SOURCE: &str = include_str!("sources/string.nexa");

const FUNCTIONS: &[FunctionDescriptor] = &[
    FunctionDescriptor {
        name: "len",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("value", "string")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringLen),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of Unicode scalar values",
    },
    FunctionDescriptor {
        name: "byte_len",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("value", "string")],
        result: "i32",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringByteLen),
        behavior: FunctionBehavior::TOTAL,
        contract: "number of UTF-8 bytes",
    },
    FunctionDescriptor {
        name: "contains",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "string"),
            ParameterDescriptor::new("needle", "string"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringContains),
        behavior: FunctionBehavior::TOTAL,
        contract: "true when needle is an exact scalar subsequence",
    },
    FunctionDescriptor {
        name: "starts_with",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "string"),
            ParameterDescriptor::new("prefix", "string"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringStartsWith),
        behavior: FunctionBehavior::TOTAL,
        contract: "true when value starts with the exact prefix sequence",
    },
    FunctionDescriptor {
        name: "ends_with",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "string"),
            ParameterDescriptor::new("suffix", "string"),
        ],
        result: "bool",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringEndsWith),
        behavior: FunctionBehavior::TOTAL,
        contract: "true when value ends with the exact suffix sequence",
    },
    FunctionDescriptor {
        name: "substring",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "string"),
            ParameterDescriptor::new("start", "i32"),
            ParameterDescriptor::new("length", "i32"),
        ],
        result: "string",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringSubstring),
        behavior: FunctionBehavior::ALLOCATES_OR_TRAPS,
        contract: "length Unicode scalars from scalar index start; traps for an invalid range",
    },
    FunctionDescriptor {
        name: "trim",
        type_parameters: &[],
        parameters: &[ParameterDescriptor::new("value", "string")],
        result: "string",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringTrim),
        behavior: FunctionBehavior::ALLOCATES,
        contract: "removes leading and trailing Unicode whitespace",
    },
    FunctionDescriptor {
        name: "split",
        type_parameters: &[],
        parameters: &[
            ParameterDescriptor::new("value", "string"),
            ParameterDescriptor::new("delimiter", "string"),
        ],
        result: "Array<string>",
        lowering: Lowering::CompilerIntrinsic(Intrinsic::StringSplit),
        behavior: FunctionBehavior::ALLOCATES,
        contract: "splits on every non-overlapping exact delimiter match and preserves empty parts",
    },
];

pub(crate) const MODULE: ModuleDescriptor = ModuleDescriptor {
    name: "string",
    path: "std.string",
    prelude: false,
    source: SOURCE,
    types: &[],
    functions: FUNCTIONS,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringIndexError {
    NegativeIndex {
        index: i32,
    },
    RangeOutOfBounds {
        start: usize,
        length: usize,
        scalar_len: usize,
    },
    ScalarLengthOverflow {
        scalar_len: usize,
    },
}

impl fmt::Display for StringIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeIndex { index } => write!(formatter, "negative scalar index {index}"),
            Self::RangeOutOfBounds {
                start,
                length,
                scalar_len,
            } => write!(
                formatter,
                "scalar range {start}..{} exceeds string length {scalar_len}",
                start.saturating_add(*length)
            ),
            Self::ScalarLengthOverflow { scalar_len } => {
                write!(formatter, "scalar length {scalar_len} exceeds i32")
            }
        }
    }
}

impl std::error::Error for StringIndexError {}

/// Unicode scalar count.
#[must_use]
pub fn len(value: &str) -> usize {
    value.chars().count()
}

/// Unicode scalar count in the Nexa `i32` representation.
pub fn len_i32(value: &str) -> Result<i32, StringIndexError> {
    let scalar_len = len(value);
    i32::try_from(scalar_len).map_err(|_| StringIndexError::ScalarLengthOverflow { scalar_len })
}

/// UTF-8 byte count.
#[must_use]
pub const fn byte_len(value: &str) -> usize {
    value.len()
}

/// Returns `length` Unicode scalar values starting at scalar index `start`.
pub fn substring(value: &str, start: usize, length: usize) -> Result<String, StringIndexError> {
    let scalar_len = len(value);
    let end = start
        .checked_add(length)
        .filter(|end| *end <= scalar_len)
        .ok_or(StringIndexError::RangeOutOfBounds {
            start,
            length,
            scalar_len,
        })?;
    if start > scalar_len {
        return Err(StringIndexError::RangeOutOfBounds {
            start,
            length,
            scalar_len,
        });
    }
    let start_byte = scalar_boundary(value, start);
    let end_byte = scalar_boundary(value, end);
    Ok(value[start_byte..end_byte].to_owned())
}

pub fn substring_i32(value: &str, start: i32, length: i32) -> Result<String, StringIndexError> {
    let start =
        usize::try_from(start).map_err(|_| StringIndexError::NegativeIndex { index: start })?;
    let length =
        usize::try_from(length).map_err(|_| StringIndexError::NegativeIndex { index: length })?;
    substring(value, start, length)
}

/// Returns the half-open scalar range `start..end`.
pub fn slice(value: &str, start: usize, end: usize) -> Result<String, StringIndexError> {
    let length = end
        .checked_sub(start)
        .ok_or(StringIndexError::RangeOutOfBounds {
            start,
            length: 0,
            scalar_len: len(value),
        })?;
    substring(value, start, length)
}

#[must_use]
pub fn contains(value: &str, needle: &str) -> bool {
    value.contains(needle)
}

#[must_use]
pub fn starts_with(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
}

#[must_use]
pub fn ends_with(value: &str, suffix: &str) -> bool {
    value.ends_with(suffix)
}

#[must_use]
pub fn trim(value: &str) -> String {
    value.trim().to_owned()
}

#[must_use]
pub fn split(value: &str, delimiter: &str) -> Vec<String> {
    value.split(delimiter).map(str::to_owned).collect()
}

fn scalar_boundary(value: &str, scalar_index: usize) -> usize {
    if scalar_index == len(value) {
        value.len()
    } else {
        value
            .char_indices()
            .nth(scalar_index)
            .map_or(value.len(), |(byte_index, _)| byte_index)
    }
}
