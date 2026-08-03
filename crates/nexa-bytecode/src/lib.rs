//! Versioned, safe-to-construct Nexa bytecode representation.

use std::collections::BTreeSet;
use std::fmt;

use nexa_core::{
    CanonicalStateField, CanonicalStateSchema, CanonicalStateType, CanonicalValueType, FileId,
    SourceSpan, StableId, StateSchemaFingerprint,
};

pub const MAGIC: [u8; 4] = *b"NXBC";
/// Current wire-format version.
///
/// Version 7 adds physical ValueLayout/ABI metadata and explicit Host opaque
/// scalar identities. The decoder intentionally accepts only the current version:
/// bytecode is an internal package artifact and has no cross-version decoding
/// compatibility promise.
pub use nexa_core::BYTECODE_VERSION;

pub mod layout;
pub const MAX_STRUCT_FIELDS: usize = 16;
pub const MAX_CLASS_FIELDS: usize = 16;
pub const MAX_HOST_CAPABILITIES: usize = 64;
pub const MAX_HOST_CAPABILITY_BYTES: usize = 128;
/// Fixed stack buffer used by scalar-to-string lowering.
///
/// Fuel charges the complete buffer bound before formatting starts, so
/// formatting, copying into the VM heap, and hashing the result cannot perform
/// unmetered value-dependent work.
pub const SCALAR_TO_STRING_BUFFER_BYTES: usize = 64;
pub const SCALAR_TO_STRING_MAX_BYTES: u64 = 64;
pub const SCALAR_TO_STRING_FUEL_PASSES: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SectionKind {
    Strings = 1,
    Types = 2,
    Constants = 3,
    Enums = 4,
    Structs = 5,
    Classes = 6,
    HostImports = 7,
    StateSchemas = 8,
    Exports = 9,
    Functions = 10,
    Code = 11,
    RootMaps = 12,
    Safepoints = 13,
    LoopBounds = 14,
    SourceMap = 15,
    ReloadMetadata = 16,
}

pub const SECTION_FLAG_MANDATORY: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionEntry {
    pub kind: u16,
    pub flags: u16,
    pub offset: u32,
    pub length: u32,
    pub count: u32,
    pub checksum: u32,
}

impl SectionKind {
    pub const ALL: [Self; 16] = [
        Self::Strings,
        Self::Types,
        Self::Constants,
        Self::Enums,
        Self::Structs,
        Self::Classes,
        Self::HostImports,
        Self::StateSchemas,
        Self::Exports,
        Self::Functions,
        Self::Code,
        Self::RootMaps,
        Self::Safepoints,
        Self::LoopBounds,
        Self::SourceMap,
        Self::ReloadMetadata,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Strings => "strings",
            Self::Types => "types",
            Self::Constants => "constants",
            Self::Enums => "enums",
            Self::Structs => "structs",
            Self::Classes => "classes",
            Self::HostImports => "host-imports",
            Self::StateSchemas => "state-schemas",
            Self::Exports => "exports",
            Self::Functions => "functions",
            Self::Code => "code",
            Self::RootMaps => "root-maps",
            Self::Safepoints => "safepoints",
            Self::LoopBounds => "loop-bounds",
            Self::SourceMap => "source-map",
            Self::ReloadMetadata => "reload-metadata",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    String,
    Ref,
    Named(StableId),
}

impl ValueType {
    #[must_use]
    pub const fn is_reference(self) -> bool {
        matches!(self, Self::String | Self::Ref | Self::Named(_))
    }
}

/// A versioned, typed operation supplied by Nexa's capability-free standard
/// library.
///
/// Generic source functions are monomorphized before bytecode emission. Their
/// concrete types are retained here so the verifier can prove the complete
/// register signature without trusting the compiler or resolving source-level
/// names at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardIntrinsic {
    OptionIsSome {
        value: ValueType,
    },
    OptionIsNone {
        value: ValueType,
    },
    ResultIsOk {
        success: ValueType,
        error: ValueType,
    },
    ResultIsErr {
        success: ValueType,
        error: ValueType,
    },
    OptionUnwrapOr {
        value: ValueType,
    },
    ResultUnwrapOr {
        success: ValueType,
        error: ValueType,
    },
    F32Floor,
    F64Floor,
    F32Ceil,
    F64Ceil,
    F32Round,
    F64Round,
    F32Sqrt,
    F64Sqrt,
    F32Sin,
    F64Sin,
    F32Cos,
    F64Cos,
    StringContains,
    StringStartsWith,
    StringEndsWith,
    StringLen,
    StringByteLen,
    StringSubstring,
    StringTrim,
    StringSplit,
    ArrayLen {
        element: ValueType,
    },
    ArrayIsEmpty {
        element: ValueType,
    },
    ArrayGet {
        element: ValueType,
    },
    ArrayPush {
        element: ValueType,
    },
    ArrayPop {
        element: ValueType,
    },
    ArrayReserve {
        element: ValueType,
    },
    ArrayCapacity {
        element: ValueType,
    },
    ArrayClear {
        element: ValueType,
    },
    ArrayShrinkToFit {
        element: ValueType,
    },
    MapLen {
        key: ValueType,
        value: ValueType,
    },
    MapContains {
        key: ValueType,
        value: ValueType,
    },
    MapGet {
        key: ValueType,
        value: ValueType,
    },
    MapInsert {
        key: ValueType,
        value: ValueType,
    },
    MapRemove {
        key: ValueType,
        value: ValueType,
    },
    DebugAssert,
    DebugTrap,
}

/// Runtime work that must be added to an intrinsic's fixed base fuel before
/// the instruction is allowed to execute.
///
/// This policy is bytecode metadata rather than an interpreter detail: the
/// verifier uses it to reject variable-work intrinsics from effects whose
/// worst-case cost cannot be proven without a bound resource profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandardIntrinsicFuelModel {
    Fixed,
    /// Scan one or more string arguments in 32-byte work blocks.
    StringBytes {
        argument_count: u8,
        passes: u8,
    },
    /// Scan and copy a string split, including its bounded result objects.
    StringSplit,
    /// Copy the current array range in 8-element work blocks.
    ArrayCopy,
    /// Resize retained array storage, including claim/release metadata.
    ArrayResize,
    /// Clear the live array prefix while retaining its capacity.
    ArrayClear,
    /// Scan the current map storage in 8-slot work blocks.
    MapLookup,
    /// Charge only the work performed by the current insert/rehash attempt.
    MapInsertAttempt,
}

impl StandardIntrinsicFuelModel {
    #[must_use]
    pub const fn is_variable(self) -> bool {
        !matches!(self, Self::Fixed)
    }
}

pub const STANDARD_STRING_FUEL_BLOCK_BYTES: u64 = 32;
pub const STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS: u64 = 8;

impl StandardIntrinsic {
    /// Number of `StandardIntrinsic` tags reserved by the bytecode v7 wire codec.
    pub const WIRE_VARIANT_COUNT: usize = 42;

    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::OptionIsSome { .. } => "intrinsic.option.is_some.v1",
            Self::OptionIsNone { .. } => "intrinsic.option.is_none.v1",
            Self::ResultIsOk { .. } => "intrinsic.result.is_ok.v1",
            Self::ResultIsErr { .. } => "intrinsic.result.is_err.v1",
            Self::OptionUnwrapOr { .. } => "intrinsic.option.unwrap_or.v1",
            Self::ResultUnwrapOr { .. } => "intrinsic.result.unwrap_or.v1",
            Self::F32Floor => "intrinsic.math.f32.floor.v1",
            Self::F64Floor => "intrinsic.math.f64.floor.v1",
            Self::F32Ceil => "intrinsic.math.f32.ceil.v1",
            Self::F64Ceil => "intrinsic.math.f64.ceil.v1",
            Self::F32Round => "intrinsic.math.f32.round.v1",
            Self::F64Round => "intrinsic.math.f64.round.v1",
            Self::F32Sqrt => "intrinsic.math.f32.sqrt.v1",
            Self::F64Sqrt => "intrinsic.math.f64.sqrt.v1",
            Self::F32Sin => "intrinsic.math.f32.sin.v1",
            Self::F64Sin => "intrinsic.math.f64.sin.v1",
            Self::F32Cos => "intrinsic.math.f32.cos.v1",
            Self::F64Cos => "intrinsic.math.f64.cos.v1",
            Self::StringContains => "intrinsic.string.contains.v1",
            Self::StringStartsWith => "intrinsic.string.starts_with.v1",
            Self::StringEndsWith => "intrinsic.string.ends_with.v1",
            Self::StringLen => "intrinsic.string.len_scalar.v1",
            Self::StringByteLen => "intrinsic.string.byte_len_utf8.v1",
            Self::StringSubstring => "intrinsic.string.substring_scalar.v1",
            Self::StringTrim => "intrinsic.string.trim_unicode.v1",
            Self::StringSplit => "intrinsic.string.split_exact.v1",
            Self::ArrayLen { .. } => "intrinsic.array.len.v1",
            Self::ArrayIsEmpty { .. } => "intrinsic.array.is_empty.v1",
            Self::ArrayGet { .. } => "intrinsic.array.get.v1",
            Self::ArrayPush { .. } => "intrinsic.array.push.v1",
            Self::ArrayPop { .. } => "intrinsic.array.pop.v1",
            Self::ArrayReserve { .. } => "intrinsic.array.reserve.v1",
            Self::ArrayCapacity { .. } => "intrinsic.array.capacity.v1",
            Self::ArrayClear { .. } => "intrinsic.array.clear.v1",
            Self::ArrayShrinkToFit { .. } => "intrinsic.array.shrink_to_fit.v1",
            Self::MapLen { .. } => "intrinsic.map.len.v1",
            Self::MapContains { .. } => "intrinsic.map.contains.v1",
            Self::MapGet { .. } => "intrinsic.map.get.v1",
            Self::MapInsert { .. } => "intrinsic.map.insert.v1",
            Self::MapRemove { .. } => "intrinsic.map.remove.v1",
            Self::DebugAssert => "intrinsic.debug.assert.v1",
            Self::DebugTrap => "intrinsic.debug.trap.v1",
        }
    }

    #[must_use]
    pub const fn argument_count(self) -> u16 {
        match self {
            Self::OptionIsSome { .. }
            | Self::OptionIsNone { .. }
            | Self::ResultIsOk { .. }
            | Self::ResultIsErr { .. }
            | Self::F32Floor
            | Self::F64Floor
            | Self::F32Ceil
            | Self::F64Ceil
            | Self::F32Round
            | Self::F64Round
            | Self::F32Sqrt
            | Self::F64Sqrt
            | Self::F32Sin
            | Self::F64Sin
            | Self::F32Cos
            | Self::F64Cos
            | Self::StringLen
            | Self::StringByteLen
            | Self::StringTrim
            | Self::ArrayLen { .. }
            | Self::ArrayIsEmpty { .. }
            | Self::ArrayPop { .. }
            | Self::ArrayCapacity { .. }
            | Self::ArrayClear { .. }
            | Self::ArrayShrinkToFit { .. }
            | Self::MapLen { .. }
            | Self::DebugAssert
            | Self::DebugTrap => 1,
            Self::OptionUnwrapOr { .. }
            | Self::ResultUnwrapOr { .. }
            | Self::StringContains
            | Self::StringStartsWith
            | Self::StringEndsWith
            | Self::StringSplit
            | Self::ArrayGet { .. }
            | Self::ArrayPush { .. }
            | Self::ArrayReserve { .. }
            | Self::MapContains { .. }
            | Self::MapGet { .. }
            | Self::MapRemove { .. } => 2,
            Self::StringSubstring | Self::MapInsert { .. } => 3,
        }
    }

    #[must_use]
    pub fn argument_type(self, index: u16) -> Option<ValueType> {
        let option = |value| ValueType::Named(option_type(value).type_id);
        let result = |success, error| ValueType::Named(result_type(success, error).type_id);
        let array = |element| ValueType::Named(array_type(element));
        let map = |key, value| ValueType::Named(map_type(key, value));
        match (self, index) {
            (
                Self::OptionIsSome { value }
                | Self::OptionIsNone { value }
                | Self::OptionUnwrapOr { value },
                0,
            ) => Some(option(value)),
            (Self::ResultIsOk { success, error } | Self::ResultIsErr { success, error }, 0) => {
                Some(result(success, error))
            }
            (Self::ResultUnwrapOr { success, error }, 0) => Some(result(success, error)),
            (
                Self::OptionUnwrapOr { value: ty }
                | Self::ResultUnwrapOr { success: ty, .. }
                | Self::ArrayPush { element: ty }
                | Self::MapContains { key: ty, .. }
                | Self::MapGet { key: ty, .. }
                | Self::MapRemove { key: ty, .. }
                | Self::MapInsert { key: ty, .. },
                1,
            )
            | (Self::MapInsert { value: ty, .. }, 2) => Some(ty),
            (
                Self::F32Floor
                | Self::F32Ceil
                | Self::F32Round
                | Self::F32Sqrt
                | Self::F32Sin
                | Self::F32Cos,
                0,
            ) => Some(ValueType::F32),
            (
                Self::F64Floor
                | Self::F64Ceil
                | Self::F64Round
                | Self::F64Sqrt
                | Self::F64Sin
                | Self::F64Cos,
                0,
            ) => Some(ValueType::F64),
            (
                Self::StringContains
                | Self::StringStartsWith
                | Self::StringEndsWith
                | Self::StringSplit,
                0 | 1,
            )
            | (
                Self::StringSubstring
                | Self::StringLen
                | Self::StringByteLen
                | Self::StringTrim
                | Self::DebugTrap,
                0,
            ) => Some(ValueType::String),
            (Self::StringSubstring, 1 | 2)
            | (Self::ArrayGet { .. } | Self::ArrayReserve { .. }, 1) => Some(ValueType::I32),
            (
                Self::ArrayLen { element }
                | Self::ArrayIsEmpty { element }
                | Self::ArrayGet { element }
                | Self::ArrayPush { element }
                | Self::ArrayPop { element }
                | Self::ArrayReserve { element }
                | Self::ArrayCapacity { element }
                | Self::ArrayClear { element }
                | Self::ArrayShrinkToFit { element },
                0,
            ) => Some(array(element)),
            (
                Self::MapLen { key, value }
                | Self::MapContains { key, value }
                | Self::MapGet { key, value }
                | Self::MapRemove { key, value }
                | Self::MapInsert { key, value },
                0,
            ) => Some(map(key, value)),
            (Self::DebugAssert, 0) => Some(ValueType::Bool),
            _ => None,
        }
    }

    #[must_use]
    pub fn result_type(self) -> ValueType {
        match self {
            Self::OptionIsSome { .. }
            | Self::OptionIsNone { .. }
            | Self::ResultIsOk { .. }
            | Self::ResultIsErr { .. }
            | Self::StringContains
            | Self::StringStartsWith
            | Self::StringEndsWith
            | Self::ArrayIsEmpty { .. }
            | Self::ArrayPush { .. }
            | Self::ArrayReserve { .. }
            | Self::ArrayClear { .. }
            | Self::ArrayShrinkToFit { .. }
            | Self::MapContains { .. }
            | Self::MapInsert { .. }
            | Self::DebugAssert
            | Self::DebugTrap => ValueType::Bool,
            Self::OptionUnwrapOr { value } => value,
            Self::ResultUnwrapOr { success, .. } => success,
            Self::F32Floor
            | Self::F32Ceil
            | Self::F32Round
            | Self::F32Sqrt
            | Self::F32Sin
            | Self::F32Cos => ValueType::F32,
            Self::F64Floor
            | Self::F64Ceil
            | Self::F64Round
            | Self::F64Sqrt
            | Self::F64Sin
            | Self::F64Cos => ValueType::F64,
            Self::StringLen
            | Self::StringByteLen
            | Self::ArrayLen { .. }
            | Self::ArrayCapacity { .. }
            | Self::MapLen { .. } => ValueType::I32,
            Self::StringSubstring | Self::StringTrim => ValueType::String,
            Self::StringSplit => ValueType::Named(array_type(ValueType::String)),
            Self::ArrayGet { element } => ValueType::Named(option_type(element).type_id),
            Self::ArrayPop { element } => element,
            Self::MapGet { value, .. } | Self::MapRemove { value, .. } => {
                ValueType::Named(option_type(value).type_id)
            }
        }
    }

    #[must_use]
    pub const fn mutates_collection(self) -> bool {
        matches!(
            self,
            Self::ArrayPush { .. }
                | Self::ArrayPop { .. }
                | Self::ArrayReserve { .. }
                | Self::ArrayClear { .. }
                | Self::ArrayShrinkToFit { .. }
                | Self::MapInsert { .. }
                | Self::MapRemove { .. }
        )
    }

    /// Bytecode v7 opcode-cost-table deterministic base fuel cost.
    ///
    /// Variable work declared by [`Self::fuel_model`] is charged separately
    /// from read-only register and heap metadata before any mutation.
    #[must_use]
    pub const fn base_fuel_cost(self) -> u16 {
        match self {
            Self::F32Sin | Self::F64Sin | Self::F32Cos | Self::F64Cos => 16,
            Self::F32Sqrt | Self::F64Sqrt | Self::StringSplit => 12,
            Self::StringContains
            | Self::StringStartsWith
            | Self::StringEndsWith
            | Self::StringLen
            | Self::StringByteLen
            | Self::StringSubstring
            | Self::StringTrim
            | Self::MapContains { .. }
            | Self::MapGet { .. }
            | Self::MapInsert { .. }
            | Self::MapRemove { .. } => 8,
            Self::ArrayGet { .. }
            | Self::ArrayPush { .. }
            | Self::ArrayPop { .. }
            | Self::ArrayReserve { .. }
            | Self::ArrayClear { .. }
            | Self::ArrayShrinkToFit { .. } => 4,
            Self::OptionUnwrapOr { .. }
            | Self::ResultUnwrapOr { .. }
            | Self::F32Floor
            | Self::F64Floor
            | Self::F32Ceil
            | Self::F64Ceil
            | Self::F32Round
            | Self::F64Round
            | Self::DebugTrap => 2,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn fuel_model(self) -> StandardIntrinsicFuelModel {
        match self {
            Self::StringContains | Self::StringStartsWith | Self::StringEndsWith => {
                StandardIntrinsicFuelModel::StringBytes {
                    argument_count: 2,
                    passes: 1,
                }
            }
            Self::StringLen => StandardIntrinsicFuelModel::StringBytes {
                argument_count: 1,
                passes: 1,
            },
            Self::StringSubstring => StandardIntrinsicFuelModel::StringBytes {
                argument_count: 1,
                passes: 4,
            },
            Self::StringTrim => StandardIntrinsicFuelModel::StringBytes {
                argument_count: 1,
                passes: 3,
            },
            Self::StringSplit => StandardIntrinsicFuelModel::StringSplit,
            Self::ArrayPush { .. } | Self::ArrayPop { .. } => StandardIntrinsicFuelModel::ArrayCopy,
            Self::ArrayReserve { .. } | Self::ArrayShrinkToFit { .. } => {
                StandardIntrinsicFuelModel::ArrayResize
            }
            Self::ArrayClear { .. } => StandardIntrinsicFuelModel::ArrayClear,
            Self::MapContains { .. } | Self::MapGet { .. } | Self::MapRemove { .. } => {
                StandardIntrinsicFuelModel::MapLookup
            }
            Self::MapInsert { .. } => StandardIntrinsicFuelModel::MapInsertAttempt,
            _ => StandardIntrinsicFuelModel::Fixed,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub parameters: Vec<ValueType>,
    pub result: Option<ValueType>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FunctionEffect {
    #[default]
    Ordinary,
    Task,
    Immediate,
    Migration,
    Cleanup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootMap {
    pub pc: u32,
    pub bitmap: Vec<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopBound {
    pub back_edge: u32,
    pub max_iterations: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMapEntry {
    pub function: u32,
    pub pc_start: u32,
    pub pc_end: u32,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCallMode {
    Immediate,
    Async,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelPolicy {
    ReturnError,
    CancelTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbandonPolicy {
    ReturnError,
    Trap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsyncResultType {
    pub result_type: StableId,
    pub success: ValueType,
    pub error: ValueType,
    pub cancel_policy: CancelPolicy,
    pub abandon_policy: AbandonPolicy,
    pub cancel_error: Option<u32>,
    pub abandon_error: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImport {
    pub stable_id: StableId,
    pub declaration_fingerprint: [u8; 32],
    pub capabilities: Vec<String>,
    pub parameters: Vec<ValueType>,
    pub result: Option<ValueType>,
    pub mode: HostCallMode,
    pub fuel_cost: u32,
    pub async_result: Option<AsyncResultType>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariant {
    pub stable_id: StableId,
    pub tag: u32,
    pub payload_type: Option<ValueType>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumType {
    pub type_id: StableId,
    pub variants: Vec<EnumVariant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StructField {
    pub stable_id: StableId,
    pub ty: ValueType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructType {
    pub type_id: StableId,
    pub fields: Vec<StructField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassType {
    pub type_id: StableId,
    pub fields: Vec<StructField>,
}

#[must_use]
pub fn option_type(payload: ValueType) -> EnumType {
    let type_id = parameterized_type_id("Option", &[payload]);
    EnumType {
        type_id,
        variants: vec![
            EnumVariant {
                stable_id: StableId::from_parts(&["Option", "::None"]),
                tag: 0,
                payload_type: None,
            },
            EnumVariant {
                stable_id: StableId::from_parts(&["Option", "::Some"]),
                tag: 1,
                payload_type: Some(payload),
            },
        ],
    }
}

#[must_use]
pub fn result_type(success: ValueType, error: ValueType) -> EnumType {
    let type_id = parameterized_type_id("Result", &[success, error]);
    EnumType {
        type_id,
        variants: vec![
            EnumVariant {
                stable_id: StableId::from_parts(&["Result", "::Ok"]),
                tag: 0,
                payload_type: Some(success),
            },
            EnumVariant {
                stable_id: StableId::from_parts(&["Result", "::Err"]),
                tag: 1,
                payload_type: Some(error),
            },
        ],
    }
}

#[must_use]
pub fn state_handle_type(target: ValueType) -> StableId {
    parameterized_type_id("StateHandle", &[target])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateHandleType {
    pub type_id: StableId,
    pub target: ValueType,
}

impl StateHandleType {
    #[must_use]
    pub fn new(target: ValueType) -> Self {
        Self {
            type_id: state_handle_type(target),
            target,
        }
    }
}

#[must_use]
pub fn array_type(element: ValueType) -> StableId {
    parameterized_type_id("Array", &[element])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayType {
    pub type_id: StableId,
    pub element: ValueType,
}

impl ArrayType {
    #[must_use]
    pub fn new(element: ValueType) -> Self {
        Self {
            type_id: array_type(element),
            element,
        }
    }
}

#[must_use]
pub fn map_type(key: ValueType, value: ValueType) -> StableId {
    parameterized_type_id("Map", &[key, value])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapType {
    pub type_id: StableId,
    pub key: ValueType,
    pub value: ValueType,
}

impl MapType {
    #[must_use]
    pub fn new(key: ValueType, value: ValueType) -> Self {
        Self {
            type_id: map_type(key, value),
            key,
            value,
        }
    }
}

#[must_use]
pub fn buffer_type(element: ValueType) -> StableId {
    parameterized_type_id("Buffer", &[element])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferType {
    pub type_id: StableId,
    pub element: ValueType,
}

impl BufferType {
    #[must_use]
    pub fn new(element: ValueType) -> Self {
        Self {
            type_id: buffer_type(element),
            element,
        }
    }
}

#[must_use]
pub fn snapshot_type(content_type: StableId) -> StableId {
    parameterized_type_id("Snapshot", &[ValueType::Named(content_type)])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotType {
    pub type_id: StableId,
    pub content_type: StableId,
}

impl SnapshotType {
    #[must_use]
    pub fn new(content_type: StableId) -> Self {
        Self {
            type_id: snapshot_type(content_type),
            content_type,
        }
    }
}

#[must_use]
pub fn resource_token_type(content_type: StableId) -> StableId {
    nexa_core::canonical_resource_token_type_id(content_type)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceTokenType {
    pub type_id: StableId,
    pub content_type: StableId,
}

impl ResourceTokenType {
    #[must_use]
    pub fn new(content_type: StableId) -> Self {
        Self {
            type_id: resource_token_type(content_type),
            content_type,
        }
    }
}

#[must_use]
pub fn stable_id_type() -> ValueType {
    ValueType::Named(StableId::from_name("StableId"))
}

#[must_use]
pub fn state_handle_error_type() -> EnumType {
    EnumType {
        type_id: StableId::from_name("StateHandleError"),
        variants: [
            "WrongDomain",
            "Missing",
            "StaleGeneration",
            "GenerationExhausted",
        ]
        .into_iter()
        .enumerate()
        .map(|(tag, name)| EnumVariant {
            stable_id: StableId::from_parts(&["StateHandleError", "::", name]),
            tag: u32::try_from(tag).expect("state handle error variant count is fixed"),
            payload_type: None,
        })
        .collect(),
    }
}

#[must_use]
pub fn parameterized_type_id(name: &str, arguments: &[ValueType]) -> StableId {
    nexa_core::canonical_parameterized_type_id_iter(
        name,
        arguments.iter().copied().map(canonical_value_type),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateField {
    pub stable_id: StableId,
    pub ty: ValueType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateType {
    pub stable_id: StableId,
    pub version: u32,
    pub fields: Vec<StateField>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateSchema {
    pub types: Vec<StateType>,
}

impl StateSchema {
    #[must_use]
    pub fn fingerprint(&self) -> StateSchemaFingerprint {
        CanonicalStateSchema {
            types: self
                .types
                .iter()
                .map(|state_type| CanonicalStateType {
                    stable_id: state_type.stable_id,
                    version: state_type.version,
                    fields: state_type
                        .fields
                        .iter()
                        .map(|field| CanonicalStateField {
                            stable_id: field.stable_id,
                            ty: canonical_value_type(field.ty),
                        })
                        .collect(),
                })
                .collect(),
        }
        .fingerprint()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrationLimitRequirements {
    pub max_objects: u32,
    pub max_fields: u32,
    pub max_forwarding_entries: u32,
    pub max_state_bytes: u64,
    pub max_gc_roots: u32,
    pub max_fuel: u64,
    pub max_call_depth: u16,
}

impl MigrationLimitRequirements {
    #[must_use]
    pub const fn satisfies(self, required: Self) -> bool {
        self.max_objects >= required.max_objects
            && self.max_fields >= required.max_fields
            && self.max_forwarding_entries >= required.max_forwarding_entries
            && self.max_state_bytes >= required.max_state_bytes
            && self.max_gc_roots >= required.max_gc_roots
            && self.max_fuel >= required.max_fuel
            && self.max_call_depth >= required.max_call_depth
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReloadMetadata {
    pub migration_entry: Option<u32>,
    pub activation_entry: Option<u32>,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub minimum_migration_limits: MigrationLimitRequirements,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptExport {
    pub stable_id: StableId,
    pub function: u32,
    pub signature: Signature,
    pub effect: FunctionEffect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instruction {
    LoadI32 {
        dst: u16,
        value: i32,
    },
    LoadBool {
        dst: u16,
        value: bool,
    },
    LoadI64 {
        dst: u16,
        value: i64,
    },
    LoadF32 {
        dst: u16,
        bits: u32,
    },
    LoadF64 {
        dst: u16,
        bits: u64,
    },
    LoadRune {
        dst: u16,
        value: u32,
    },
    LoadString {
        dst: u16,
        string: u32,
    },
    Move {
        dst: u16,
        source: u16,
    },
    Add {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    Sub {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    Mul {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    Div {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    RemI32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    AddI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    SubI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    MulI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    DivI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    RemI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    AddF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    SubF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    MulF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    DivF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    RemF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    AddF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    SubF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    MulF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    DivF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    RemF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    StringLen {
        dst: u16,
        source: u16,
    },
    StringByteLen {
        dst: u16,
        source: u16,
    },
    StringEqual {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    StringConcat {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    /// WP57: converts and concatenates a contiguous scalar/string register
    /// window into one freshly published string.
    StringBuild {
        dst: u16,
        parts_base: u16,
        parts_count: u16,
    },
    StringRuneAt {
        dst: u16,
        source: u16,
        index: u16,
    },
    StringHash {
        dst: u16,
        source: u16,
    },
    I32ToString {
        dst: u16,
        source: u16,
    },
    I64ToString {
        dst: u16,
        source: u16,
    },
    F32ToString {
        dst: u16,
        source: u16,
    },
    F64ToString {
        dst: u16,
        source: u16,
    },
    BoolToString {
        dst: u16,
        source: u16,
    },
    RuneToString {
        dst: u16,
        source: u16,
    },
    StringToString {
        dst: u16,
        source: u16,
    },
    StandardIntrinsic {
        intrinsic: StandardIntrinsic,
        args_base: u16,
        args_count: u16,
        dst: u16,
    },
    CompareEq {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    CompareLtI32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    CompareLtI64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    CompareLtF32 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    CompareLtF64 {
        dst: u16,
        lhs: u16,
        rhs: u16,
    },
    Jump {
        target: u32,
    },
    JumpIfFalse {
        condition: u16,
        target: u32,
    },
    Call {
        function: u32,
        args_base: u16,
        args_count: u16,
        dst: u16,
    },
    HostCall {
        import: u32,
        args_base: u16,
        args_count: u16,
        dst: u16,
    },
    StateOldGet {
        stable_id: StableId,
        ty: ValueType,
        dst: u16,
    },
    StateCurrentGet {
        stable_id: StableId,
        type_id: StableId,
        dst: u16,
    },
    StateNewCreate {
        stable_id: StableId,
        type_id: StableId,
        dst: u16,
    },
    StateNewSet {
        object: u16,
        field_id: StableId,
        source: u16,
    },
    StateReplace {
        old_id: StableId,
        target: u16,
    },
    StatePreserve {
        stable_id: StableId,
    },
    StateDelete {
        stable_id: StableId,
    },
    EnumNew {
        type_id: StableId,
        variant: StableId,
        payload: Option<u16>,
        dst: u16,
    },
    EnumTag {
        source: u16,
        dst: u16,
    },
    EnumPayload {
        source: u16,
        variant: StableId,
        dst: u16,
    },
    EnumEqual {
        lhs: u16,
        rhs: u16,
        dst: u16,
    },
    StructNew {
        type_id: StableId,
        fields_base: u16,
        fields_count: u16,
        dst: u16,
    },
    StructGet {
        source: u16,
        field: StableId,
        dst: u16,
    },
    StructWith {
        source: u16,
        field: StableId,
        value: u16,
        dst: u16,
    },
    StructEqual {
        lhs: u16,
        rhs: u16,
        dst: u16,
    },
    ClassNew {
        type_id: StableId,
        fields_base: u16,
        fields_count: u16,
        dst: u16,
    },
    ClassGet {
        source: u16,
        field: StableId,
        dst: u16,
    },
    ClassSet {
        source: u16,
        field: StableId,
        value: u16,
    },
    ClassEqual {
        lhs: u16,
        rhs: u16,
        dst: u16,
    },
    ArrayNew {
        type_id: StableId,
        dst: u16,
    },
    ArrayLen {
        source: u16,
        dst: u16,
    },
    ArrayGet {
        source: u16,
        index: u16,
        dst: u16,
    },
    /// WP52: reads one field of a struct array element without
    /// materializing the element - flattened rows read their arena cell
    /// directly, cell layouts project the stored struct's field.
    ArrayFieldGet {
        source: u16,
        index: u16,
        field: u16,
        dst: u16,
    },
    ArraySet {
        source: u16,
        index: u16,
        value: u16,
    },
    ArrayPush {
        source: u16,
        value: u16,
    },
    /// WP52: pushes one struct element built from a contiguous register
    /// range - flattened rows receive the fields directly, so no source
    /// struct object is ever materialized on the push path.
    ArrayPushRow {
        source: u16,
        fields_base: u16,
        fields_count: u16,
    },
    ArrayPop {
        source: u16,
        dst: u16,
    },
    ArrayInsert {
        source: u16,
        index: u16,
        value: u16,
    },
    ArrayRemove {
        source: u16,
        index: u16,
        dst: u16,
    },
    ArrayClear {
        source: u16,
    },
    MapNew {
        type_id: StableId,
        dst: u16,
    },
    MapLen {
        source: u16,
        dst: u16,
    },
    MapGet {
        source: u16,
        key: u16,
        result_type: StableId,
        dst: u16,
    },
    MapSet {
        source: u16,
        key: u16,
        value: u16,
    },
    MapRemove {
        source: u16,
        key: u16,
        result_type: StableId,
        dst: u16,
    },
    MapContains {
        source: u16,
        key: u16,
        dst: u16,
    },
    MapClear {
        source: u16,
    },
    BufferLen {
        source: u16,
        dst: u16,
    },
    BufferGet {
        source: u16,
        index: u16,
        dst: u16,
    },
    BufferSet {
        source: u16,
        index: u16,
        value: u16,
    },
    BufferSlice {
        source: u16,
        start: u16,
        length: u16,
        dst: u16,
    },
    BufferCopy {
        destination: u16,
        source: u16,
        source_start: u16,
        destination_start: u16,
        length: u16,
    },
    StateFinish,
    StateOldFieldGet {
        object: u16,
        field_id: StableId,
        ty: ValueType,
        dst: u16,
    },
    StateHandleResolve {
        handle: u16,
        target: ValueType,
        result_type: StableId,
        dst: u16,
    },
    StateHandleIsAlive {
        handle: u16,
        target: ValueType,
        dst: u16,
    },
    StateHandleStableId {
        handle: u16,
        target: ValueType,
        dst: u16,
    },
    StateHandleGeneration {
        handle: u16,
        target: ValueType,
        dst: u16,
    },
    StateHandleEqual {
        lhs: u16,
        rhs: u16,
        target: ValueType,
        dst: u16,
    },
    StateHandleHash {
        handle: u16,
        target: ValueType,
        dst: u16,
    },
    DeferPush {
        function: u32,
        args_base: u16,
        args_count: u16,
    },
    DeferPop,
    CleanupReturn,
    Return {
        source: u16,
    },
    ReturnVoid,
    Safepoint,
    Yield,
    Trap,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub signature: Signature,
    /// Width of the verifier-derived physical parameter prefix. Logical
    /// parameter types remain in `signature`; execution places each value at
    /// its `FunctionAbi::parameters[*].slot_offset`.
    pub parameter_slots: u16,
    pub registers: u16,
    pub frame_bytes: u32,
    pub root_bitmap: Vec<bool>,
    pub root_maps: Vec<RootMap>,
    pub safepoints: Vec<u32>,
    pub loop_bounds: Vec<LoopBound>,
    pub effect: FunctionEffect,
    pub max_static_call_depth: u16,
    pub code: Vec<Instruction>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Module {
    pub strings: Vec<String>,
    pub functions: Vec<Function>,
    pub state_handle_types: Vec<StateHandleType>,
    pub array_types: Vec<ArrayType>,
    pub map_types: Vec<MapType>,
    pub buffer_types: Vec<BufferType>,
    pub snapshot_types: Vec<SnapshotType>,
    pub resource_token_types: Vec<ResourceTokenType>,
    /// Host-defined scalar identities carried in `PhysicalSlotKind::Opaque`.
    pub opaque_types: Vec<StableId>,
    pub enum_types: Vec<EnumType>,
    pub struct_types: Vec<StructType>,
    pub class_types: Vec<ClassType>,
    pub host_imports: Vec<HostImport>,
    pub exports: Vec<ScriptExport>,
    pub state_schema: StateSchema,
    pub host_contract_id: Option<StableId>,
    pub state_schema_fingerprint: StateSchemaFingerprint,
    pub reload_metadata: ReloadMetadata,
    pub source_map: Vec<SourceMapEntry>,
}

#[must_use]
pub fn minimum_migration_limits(
    module: &Module,
    migration_entry: Option<u32>,
) -> MigrationLimitRequirements {
    let Some(entry) = migration_entry.and_then(|entry| usize::try_from(entry).ok()) else {
        return MigrationLimitRequirements::default();
    };
    if entry >= module.functions.len() {
        return MigrationLimitRequirements::default();
    }

    let mut reachable = vec![false; module.functions.len()];
    let mut pending = vec![entry];
    while let Some(function_index) = pending.pop() {
        if reachable[function_index] {
            continue;
        }
        reachable[function_index] = true;
        for instruction in &module.functions[function_index].code {
            if let Some(function) = stack_callee(*instruction)
                && let Ok(callee) = usize::try_from(function)
                && callee < module.functions.len()
            {
                pending.push(callee);
            }
        }
    }

    let mut objects = BTreeSet::new();
    let mut forwarding = BTreeSet::new();
    let mut max_fields = 0_u32;
    let mut max_gc_roots = 0_u32;
    let mut max_fuel = 0_u64;
    for (function_index, function) in module.functions.iter().enumerate() {
        if !reachable[function_index] {
            continue;
        }
        max_fuel = max_fuel.saturating_add(u64::try_from(function.code.len()).unwrap_or(u64::MAX));
        for root_map in &function.root_maps {
            let roots = root_map.bitmap.iter().filter(|root| **root).count();
            max_gc_roots = max_gc_roots.max(u32::try_from(roots).unwrap_or(u32::MAX));
        }
        for instruction in &function.code {
            match instruction {
                Instruction::StateNewCreate { stable_id, .. } => {
                    objects.insert(*stable_id);
                }
                Instruction::StateNewSet { .. } => {
                    max_fields = max_fields.saturating_add(1);
                }
                Instruction::StateReplace { old_id, .. } => {
                    forwarding.insert(*old_id);
                }
                Instruction::StatePreserve { stable_id } => {
                    objects.insert(*stable_id);
                    forwarding.insert(*stable_id);
                }
                Instruction::StateDelete { stable_id } => {
                    forwarding.insert(*stable_id);
                }
                _ => {}
            }
        }
    }
    let max_objects = u32::try_from(objects.len()).unwrap_or(u32::MAX);
    let max_forwarding_entries = u32::try_from(forwarding.len()).unwrap_or(u32::MAX);
    let max_state_bytes = u64::from(max_objects)
        .saturating_mul(32)
        .saturating_add(u64::from(max_fields).saturating_mul(16));
    MigrationLimitRequirements {
        max_objects,
        max_fields,
        max_forwarding_entries,
        max_state_bytes,
        max_gc_roots,
        max_fuel,
        max_call_depth: migration_call_depth(module, entry, &mut Vec::new()),
    }
}

fn migration_call_depth(module: &Module, function: usize, visiting: &mut Vec<usize>) -> u16 {
    if visiting.contains(&function) {
        return u16::MAX;
    }
    let Some(body) = module.functions.get(function) else {
        return 0;
    };
    visiting.push(function);
    let mut depth = 1_u16;
    for instruction in &body.code {
        if let Some(callee) = stack_callee(*instruction)
            && let Ok(callee) = usize::try_from(callee)
        {
            depth = depth.max(migration_call_depth(module, callee, visiting).saturating_add(1));
        }
    }
    visiting.pop();
    depth
}

const fn stack_callee(instruction: Instruction) -> Option<u32> {
    match instruction {
        Instruction::Call { function, .. } | Instruction::DeferPush { function, .. } => {
            Some(function)
        }
        _ => None,
    }
}

const fn canonical_value_type(value: ValueType) -> CanonicalValueType {
    match value {
        ValueType::I32 => CanonicalValueType::I32,
        ValueType::I64 => CanonicalValueType::I64,
        ValueType::F32 => CanonicalValueType::F32,
        ValueType::F64 => CanonicalValueType::F64,
        ValueType::Bool => CanonicalValueType::Bool,
        ValueType::Rune => CanonicalValueType::Rune,
        ValueType::String => CanonicalValueType::String,
        ValueType::Ref => CanonicalValueType::Ref,
        ValueType::Named(stable_id) => CanonicalValueType::Named(stable_id),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    InvalidMagic,
    UnsupportedVersion(u16),
    InvalidType(u8),
    InvalidOpcode(u8),
    InvalidStandardIntrinsic(u8),
    InvalidBoolean(u8),
    InvalidUtf8,
    TrailingBytes,
    SizeOverflow,
    InvalidSectionDirectory,
    OffsetOverflow,
    LengthOverflow,
    SectionOverlap,
    DuplicateRequiredSection(u16),
    UnknownMandatorySection(u16),
    CountMismatch(u16),
    InconsistentSection(u16),
    InvalidSourceMap,
    ChecksumMismatch(u16),
    ResourceLimit(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_bytes: usize,
    pub max_sections: usize,
    pub max_strings: usize,
    pub max_string_bytes: usize,
    pub max_types: usize,
    pub max_constants: usize,
    pub max_functions: usize,
    pub max_instructions: usize,
    pub max_registers: usize,
    pub max_root_maps: usize,
    pub max_root_map_bytes: usize,
    pub max_loop_bounds: usize,
    pub max_safepoints: usize,
    pub max_host_imports: usize,
    pub max_state_types: usize,
    pub max_enum_types: usize,
    pub max_enum_variants: usize,
    pub max_structs: usize,
    pub max_classes: usize,
    pub max_fields: usize,
    pub max_exports: usize,
    pub max_source_map_entries: usize,
    pub max_reload_metadata_bytes: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_sections: 64,
            max_strings: 65_536,
            max_string_bytes: 4 * 1024 * 1024,
            max_types: 65_536,
            max_constants: 65_536,
            max_functions: 65_536,
            max_instructions: 1_000_000,
            max_registers: u16::MAX as usize,
            max_root_maps: 1_000_000,
            max_root_map_bytes: 4 * 1024 * 1024,
            max_loop_bounds: 1_000_000,
            max_safepoints: 1_000_000,
            max_host_imports: 65_536,
            max_state_types: 65_536,
            max_enum_types: 65_536,
            max_enum_variants: 1_000_000,
            max_structs: 65_536,
            max_classes: 65_536,
            max_fields: 1_000_000,
            max_exports: 65_536,
            max_source_map_entries: 1_000_000,
            max_reload_metadata_bytes: 64 * 1024,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for DecodeError {}

impl Module {
    pub fn inspect_section_directory(
        bytes: &[u8],
        limits: DecodeLimits,
    ) -> Result<Vec<SectionEntry>, DecodeError> {
        if bytes.len() > limits.max_bytes {
            return Err(DecodeError::ResourceLimit("byte length"));
        }
        let _ = decode_sections(bytes, limits.max_sections)?;
        let mut reader = Reader { bytes, cursor: 0 };
        reader.take(4)?;
        reader.u16()?;
        let count = usize::from(reader.u16()?);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(SectionEntry {
                kind: reader.u16()?,
                flags: reader.u16()?,
                offset: reader.u32()?,
                length: reader.u32()?,
                count: reader.u32()?,
                checksum: reader.u32()?,
            });
        }
        entries.sort_by_key(|entry| entry.kind);
        Ok(entries)
    }

    #[must_use]
    pub fn source_span(&self, function: u32, pc: u32) -> Option<SourceSpan> {
        self.source_map
            .iter()
            .rev()
            .find(|entry| entry.function == function && entry.pc_start <= pc && pc < entry.pc_end)
            .map(|entry| entry.span)
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        put_u32(&mut output, 1);
        put_optional_id(&mut output, self.host_contract_id);
        output.extend_from_slice(self.state_schema_fingerprint.as_bytes());
        put_optional_u32(&mut output, self.reload_metadata.migration_entry);
        put_optional_u32(&mut output, self.reload_metadata.activation_entry);
        output.extend_from_slice(self.reload_metadata.state_schema_fingerprint.as_bytes());
        let migration_limits = self.reload_metadata.minimum_migration_limits;
        put_u32(&mut output, migration_limits.max_objects);
        put_u32(&mut output, migration_limits.max_fields);
        put_u32(&mut output, migration_limits.max_forwarding_entries);
        put_u64(&mut output, migration_limits.max_state_bytes);
        put_u32(&mut output, migration_limits.max_gc_roots);
        put_u64(&mut output, migration_limits.max_fuel);
        put_u16(&mut output, migration_limits.max_call_depth);
        let reload_metadata = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.host_imports.len()).expect("host import count exceeds wire format"),
        );
        for import in &self.host_imports {
            put_u64(&mut output, import.stable_id.0);
            output.extend_from_slice(&import.declaration_fingerprint);
            put_u16(
                &mut output,
                u16::try_from(import.capabilities.len())
                    .expect("host capability count exceeds wire format"),
            );
            for capability in &import.capabilities {
                put_u16(
                    &mut output,
                    u16::try_from(capability.len())
                        .expect("host capability length exceeds wire format"),
                );
                output.extend_from_slice(capability.as_bytes());
            }
            output.push(match import.mode {
                HostCallMode::Immediate => 0,
                HostCallMode::Async => 1,
            });
            put_u32(&mut output, import.fuel_cost);
            put_u16(
                &mut output,
                u16::try_from(import.parameters.len())
                    .expect("host parameter count exceeds wire format"),
            );
            for ty in &import.parameters {
                encode_type(&mut output, *ty);
            }
            output.push(u8::from(import.result.is_some()));
            if let Some(result) = import.result {
                encode_type(&mut output, result);
            }
            output.push(u8::from(import.async_result.is_some()));
            if let Some(async_result) = import.async_result {
                put_u64(&mut output, async_result.result_type.0);
                encode_type(&mut output, async_result.success);
                encode_type(&mut output, async_result.error);
                output.push(match async_result.cancel_policy {
                    CancelPolicy::ReturnError => 0,
                    CancelPolicy::CancelTask => 1,
                });
                output.push(match async_result.abandon_policy {
                    AbandonPolicy::ReturnError => 0,
                    AbandonPolicy::Trap => 1,
                });
                put_optional_u32(&mut output, async_result.cancel_error);
                put_optional_u32(&mut output, async_result.abandon_error);
            }
        }
        let host_imports = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.enum_types.len()).expect("enum type count exceeds wire format"),
        );
        for enum_type in &self.enum_types {
            put_u64(&mut output, enum_type.type_id.0);
            put_u16(
                &mut output,
                u16::try_from(enum_type.variants.len())
                    .expect("enum variant count exceeds wire format"),
            );
            for variant in &enum_type.variants {
                put_u64(&mut output, variant.stable_id.0);
                put_u32(&mut output, variant.tag);
                output.push(u8::from(variant.payload_type.is_some()));
                if let Some(payload_type) = variant.payload_type {
                    encode_type(&mut output, payload_type);
                }
            }
        }
        let enums = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.struct_types.len()).expect("struct type count exceeds wire format"),
        );
        for struct_type in &self.struct_types {
            put_u64(&mut output, struct_type.type_id.0);
            put_u16(
                &mut output,
                u16::try_from(struct_type.fields.len())
                    .expect("struct field count exceeds wire format"),
            );
            for field in &struct_type.fields {
                put_u64(&mut output, field.stable_id.0);
                encode_type(&mut output, field.ty);
            }
        }
        let structs = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.class_types.len()).expect("class type count exceeds wire format"),
        );
        for class_type in &self.class_types {
            put_u64(&mut output, class_type.type_id.0);
            put_u16(
                &mut output,
                u16::try_from(class_type.fields.len())
                    .expect("class field count exceeds wire format"),
            );
            for field in &class_type.fields {
                put_u64(&mut output, field.stable_id.0);
                encode_type(&mut output, field.ty);
            }
        }
        let classes = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.state_schema.types.len())
                .expect("state type count exceeds wire format"),
        );
        for state_type in &self.state_schema.types {
            put_u64(&mut output, state_type.stable_id.0);
            put_u32(&mut output, state_type.version);
            put_u16(
                &mut output,
                u16::try_from(state_type.fields.len())
                    .expect("state field count exceeds wire format"),
            );
            for field in &state_type.fields {
                put_u64(&mut output, field.stable_id.0);
                encode_type(&mut output, field.ty);
            }
        }
        let state_schemas = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.exports.len()).expect("export count exceeds wire format"),
        );
        for export in &self.exports {
            put_u64(&mut output, export.stable_id.0);
            put_u32(&mut output, export.function);
            output.push(encode_effect(export.effect));
            put_u16(
                &mut output,
                u16::try_from(export.signature.parameters.len())
                    .expect("export parameter count exceeds wire format"),
            );
            for ty in &export.signature.parameters {
                encode_type(&mut output, *ty);
            }
            output.push(u8::from(export.signature.result.is_some()));
            if let Some(result) = export.signature.result {
                encode_type(&mut output, result);
            }
        }
        let exports = output;
        let mut output = Vec::new();
        put_u32(
            &mut output,
            u32::try_from(self.functions.len()).expect("function count exceeds wire format"),
        );
        for function in &self.functions {
            put_u16(
                &mut output,
                u16::try_from(function.signature.parameters.len())
                    .expect("parameter count exceeds wire format"),
            );
            for ty in &function.signature.parameters {
                encode_type(&mut output, *ty);
            }
            output.push(u8::from(function.signature.result.is_some()));
            if let Some(result) = function.signature.result {
                encode_type(&mut output, result);
            }
            put_u16(&mut output, function.parameter_slots);
            put_u16(&mut output, function.registers);
            put_u32(&mut output, function.frame_bytes);
            output.push(encode_effect(function.effect));
            put_u16(&mut output, function.max_static_call_depth);
            put_u16(
                &mut output,
                u16::try_from(function.root_bitmap.len()).expect("root bitmap exceeds wire format"),
            );
            output.extend(function.root_bitmap.iter().map(|root| u8::from(*root)));
            put_u32(
                &mut output,
                u32::try_from(function.root_maps.len())
                    .expect("root map count exceeds wire format"),
            );
            for root_map in &function.root_maps {
                put_u32(&mut output, root_map.pc);
                put_u16(
                    &mut output,
                    u16::try_from(root_map.bitmap.len()).expect("root bitmap exceeds wire format"),
                );
                output.extend(root_map.bitmap.iter().map(|root| u8::from(*root)));
            }
            put_u32(
                &mut output,
                u32::try_from(function.safepoints.len())
                    .expect("safepoint count exceeds wire format"),
            );
            for safepoint in &function.safepoints {
                put_u32(&mut output, *safepoint);
            }
            put_u32(
                &mut output,
                u32::try_from(function.loop_bounds.len())
                    .expect("loop-bound count exceeds wire format"),
            );
            for loop_bound in &function.loop_bounds {
                put_u32(&mut output, loop_bound.back_edge);
                put_u32(&mut output, loop_bound.max_iterations);
            }
            put_u32(
                &mut output,
                u32::try_from(function.code.len()).expect("instruction count exceeds wire format"),
            );
            for instruction in &function.code {
                encode_instruction(&mut output, *instruction);
            }
        }
        let functions = output;
        let mut code = Vec::new();
        put_u32(
            &mut code,
            u32::try_from(self.functions.len()).expect("function count exceeds wire format"),
        );
        let mut root_maps = Vec::new();
        put_u32(
            &mut root_maps,
            u32::try_from(self.functions.len()).expect("function count exceeds wire format"),
        );
        let mut safepoints = Vec::new();
        put_u32(
            &mut safepoints,
            u32::try_from(self.functions.len()).expect("function count exceeds wire format"),
        );
        let mut loop_bounds = Vec::new();
        put_u32(
            &mut loop_bounds,
            u32::try_from(self.functions.len()).expect("function count exceeds wire format"),
        );
        for function in &self.functions {
            put_u32(
                &mut code,
                u32::try_from(function.code.len()).expect("instruction count exceeds wire format"),
            );
            for instruction in &function.code {
                encode_instruction(&mut code, *instruction);
            }
            put_u16(
                &mut root_maps,
                u16::try_from(function.root_bitmap.len()).expect("root bitmap exceeds wire format"),
            );
            root_maps.extend(function.root_bitmap.iter().map(|root| u8::from(*root)));
            put_u32(
                &mut root_maps,
                u32::try_from(function.root_maps.len())
                    .expect("root map count exceeds wire format"),
            );
            for root_map in &function.root_maps {
                put_u32(&mut root_maps, root_map.pc);
                put_u16(
                    &mut root_maps,
                    u16::try_from(root_map.bitmap.len()).expect("root bitmap exceeds wire format"),
                );
                root_maps.extend(root_map.bitmap.iter().map(|root| u8::from(*root)));
            }
            put_u32(
                &mut safepoints,
                u32::try_from(function.safepoints.len())
                    .expect("safepoint count exceeds wire format"),
            );
            for safepoint in &function.safepoints {
                put_u32(&mut safepoints, *safepoint);
            }
            put_u32(
                &mut loop_bounds,
                u32::try_from(function.loop_bounds.len())
                    .expect("loop-bound count exceeds wire format"),
            );
            for loop_bound in &function.loop_bounds {
                put_u32(&mut loop_bounds, loop_bound.back_edge);
                put_u32(&mut loop_bounds, loop_bound.max_iterations);
            }
        }
        let mut source_map = Vec::new();
        put_u32(
            &mut source_map,
            u32::try_from(self.source_map.len()).expect("source-map count exceeds wire format"),
        );
        for entry in &self.source_map {
            put_u32(&mut source_map, entry.function);
            put_u32(&mut source_map, entry.pc_start);
            put_u32(&mut source_map, entry.pc_end);
            put_u32(&mut source_map, entry.span.file.0);
            put_u32(&mut source_map, entry.span.start);
            put_u32(&mut source_map, entry.span.end);
        }
        let mut strings = Vec::new();
        put_u32(
            &mut strings,
            u32::try_from(self.strings.len()).expect("string count exceeds wire format"),
        );
        for string in &self.strings {
            put_u32(
                &mut strings,
                u32::try_from(string.len()).expect("string length exceeds wire format"),
            );
            strings.extend_from_slice(string.as_bytes());
        }
        let mut types = Vec::new();
        put_u32(
            &mut types,
            u32::try_from(
                self.state_handle_types
                    .len()
                    .saturating_add(self.array_types.len())
                    .saturating_add(self.map_types.len())
                    .saturating_add(self.buffer_types.len())
                    .saturating_add(self.snapshot_types.len())
                    .saturating_add(self.resource_token_types.len())
                    .saturating_add(self.opaque_types.len()),
            )
            .expect("parameterized type count exceeds wire format"),
        );
        for state_handle in &self.state_handle_types {
            types.push(1);
            put_u64(&mut types, state_handle.type_id.0);
            encode_type(&mut types, state_handle.target);
        }
        for array in &self.array_types {
            types.push(2);
            put_u64(&mut types, array.type_id.0);
            encode_type(&mut types, array.element);
        }
        for map in &self.map_types {
            types.push(3);
            put_u64(&mut types, map.type_id.0);
            encode_type(&mut types, map.key);
            encode_type(&mut types, map.value);
        }
        for buffer in &self.buffer_types {
            types.push(4);
            put_u64(&mut types, buffer.type_id.0);
            encode_type(&mut types, buffer.element);
        }
        for snapshot in &self.snapshot_types {
            types.push(5);
            put_u64(&mut types, snapshot.type_id.0);
            put_u64(&mut types, snapshot.content_type.0);
        }
        for token in &self.resource_token_types {
            types.push(6);
            put_u64(&mut types, token.type_id.0);
            put_u64(&mut types, token.content_type.0);
        }
        for type_id in &self.opaque_types {
            types.push(7);
            put_u64(&mut types, type_id.0);
        }
        let empty = || {
            let mut section = Vec::new();
            put_u32(&mut section, 0);
            section
        };
        encode_sections(&[
            (SectionKind::Strings, strings),
            (SectionKind::Types, types),
            (SectionKind::Constants, empty()),
            (SectionKind::Enums, enums),
            (SectionKind::Structs, structs),
            (SectionKind::Classes, classes),
            (SectionKind::HostImports, host_imports),
            (SectionKind::StateSchemas, state_schemas),
            (SectionKind::Exports, exports),
            (SectionKind::Functions, functions),
            (SectionKind::Code, code),
            (SectionKind::RootMaps, root_maps),
            (SectionKind::Safepoints, safepoints),
            (SectionKind::LoopBounds, loop_bounds),
            (SectionKind::SourceMap, source_map),
            (SectionKind::ReloadMetadata, reload_metadata),
        ])
    }

    #[allow(clippy::too_many_lines)]
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Self::decode_with_limits(bytes, DecodeLimits::default())
    }

    #[allow(clippy::too_many_lines)]
    pub fn decode_with_limits(bytes: &[u8], limits: DecodeLimits) -> Result<Self, DecodeError> {
        if bytes.len() > limits.max_bytes {
            return Err(DecodeError::ResourceLimit("byte length"));
        }
        let sections = decode_sections(bytes, limits.max_sections)?;
        for kind in SectionKind::ALL {
            required_section(&sections, kind)?;
        }
        validate_empty_section(
            required_section(&sections, SectionKind::Constants)?,
            SectionKind::Constants,
        )?;
        enforce_section_limit(
            &sections,
            SectionKind::Strings,
            limits.max_strings,
            "strings",
        )?;
        let string_bytes = required_section(&sections, SectionKind::Strings)?
            .len()
            .checked_sub(4)
            .ok_or(DecodeError::SizeOverflow)?;
        enforce_limit(string_bytes, limits.max_string_bytes, "string bytes")?;
        enforce_section_limit(&sections, SectionKind::Types, limits.max_types, "types")?;
        enforce_section_limit(
            &sections,
            SectionKind::Structs,
            limits.max_structs,
            "structs",
        )?;
        enforce_section_limit(
            &sections,
            SectionKind::Classes,
            limits.max_classes,
            "classes",
        )?;
        let root_map_bytes = required_section(&sections, SectionKind::RootMaps)?
            .len()
            .checked_sub(4)
            .ok_or(DecodeError::SizeOverflow)?;
        enforce_limit(root_map_bytes, limits.max_root_map_bytes, "root map bytes")?;
        enforce_limit(
            required_section(&sections, SectionKind::ReloadMetadata)?.len(),
            limits.max_reload_metadata_bytes,
            "reload metadata bytes",
        )?;
        let mut string_reader = Reader {
            bytes: required_section(&sections, SectionKind::Strings)?,
            cursor: 0,
        };
        let string_count =
            usize::try_from(string_reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        enforce_limit(string_count, limits.max_strings, "strings")?;
        let mut strings = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            let length =
                usize::try_from(string_reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            let value = std::str::from_utf8(string_reader.take(length)?)
                .map_err(|_| DecodeError::InvalidUtf8)?;
            strings.push(value.to_owned());
        }
        if string_reader.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        let mut types_reader = Reader {
            bytes: required_section(&sections, SectionKind::Types)?,
            cursor: 0,
        };
        let state_handle_type_count =
            usize::try_from(types_reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        enforce_limit(state_handle_type_count, limits.max_types, "types")?;
        let mut state_handle_types = Vec::with_capacity(state_handle_type_count);
        let mut array_types = Vec::new();
        let mut map_types = Vec::new();
        let mut buffer_types = Vec::new();
        let mut snapshot_types = Vec::new();
        let mut resource_token_types = Vec::new();
        let mut opaque_types = Vec::new();
        for _ in 0..state_handle_type_count {
            let kind = types_reader.u8()?;
            let type_id = StableId(types_reader.u64()?);
            match kind {
                1 => state_handle_types.push(StateHandleType {
                    type_id,
                    target: decode_type(&mut types_reader)?,
                }),
                2 => array_types.push(ArrayType {
                    type_id,
                    element: decode_type(&mut types_reader)?,
                }),
                3 => map_types.push(MapType {
                    type_id,
                    key: decode_type(&mut types_reader)?,
                    value: decode_type(&mut types_reader)?,
                }),
                4 => buffer_types.push(BufferType {
                    type_id,
                    element: decode_type(&mut types_reader)?,
                }),
                5 => snapshot_types.push(SnapshotType {
                    type_id,
                    content_type: StableId(types_reader.u64()?),
                }),
                6 => resource_token_types.push(ResourceTokenType {
                    type_id,
                    content_type: StableId(types_reader.u64()?),
                }),
                7 => opaque_types.push(type_id),
                _ => return Err(DecodeError::InvalidType(kind)),
            }
        }
        if types_reader.remaining() != 0 {
            return Err(DecodeError::TrailingBytes);
        }
        let mut metadata = Vec::new();
        metadata.extend_from_slice(
            required_section(&sections, SectionKind::ReloadMetadata)?
                .get(4..)
                .ok_or(DecodeError::Truncated)?,
        );
        metadata.extend_from_slice(required_section(&sections, SectionKind::HostImports)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Enums)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Structs)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Classes)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::StateSchemas)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Exports)?);
        let function_bytes = required_section(&sections, SectionKind::Functions)?;
        let source_map_bytes = required_section(&sections, SectionKind::SourceMap)?;
        let mut reader = Reader {
            bytes: &metadata,
            cursor: 0,
        };
        let host_contract_id = read_optional_id(&mut reader)?;
        let state_schema_fingerprint = StateSchemaFingerprint::from_bytes(reader.array()?);
        let migration_entry = read_optional_u32(&mut reader)?;
        let activation_entry = read_optional_u32(&mut reader)?;
        let reload_state_schema_fingerprint = StateSchemaFingerprint::from_bytes(reader.array()?);
        let minimum_migration_limits = MigrationLimitRequirements {
            max_objects: reader.u32()?,
            max_fields: reader.u32()?,
            max_forwarding_entries: reader.u32()?,
            max_state_bytes: reader.u64()?,
            max_gc_roots: reader.u32()?,
            max_fuel: reader.u64()?,
            max_call_depth: reader.u16()?,
        };
        let reload_metadata = ReloadMetadata {
            migration_entry,
            activation_entry,
            state_schema_fingerprint: reload_state_schema_fingerprint,
            minimum_migration_limits,
        };
        let host_import_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if host_import_count > limits.max_host_imports {
            return Err(DecodeError::ResourceLimit("host imports"));
        }
        if host_import_count > reader.remaining() {
            return Err(DecodeError::Truncated);
        }
        let mut host_imports = Vec::with_capacity(host_import_count);
        for _ in 0..host_import_count {
            let stable_id = StableId(reader.u64()?);
            let declaration_fingerprint = reader.array()?;
            let capability_count = usize::from(reader.u16()?);
            enforce_limit(capability_count, MAX_HOST_CAPABILITIES, "host capabilities")?;
            let mut capabilities = Vec::with_capacity(capability_count);
            for _ in 0..capability_count {
                let length = usize::from(reader.u16()?);
                enforce_limit(length, MAX_HOST_CAPABILITY_BYTES, "host capability bytes")?;
                let capability = std::str::from_utf8(reader.take(length)?)
                    .map_err(|_| DecodeError::InvalidUtf8)?;
                capabilities.push(capability.to_owned());
            }
            let mode = match reader.u8()? {
                0 => HostCallMode::Immediate,
                1 => HostCallMode::Async,
                value => return Err(DecodeError::InvalidType(value)),
            };
            let fuel_cost = reader.u32()?;
            let parameter_count = usize::from(reader.u16()?);
            if parameter_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut parameters = Vec::with_capacity(parameter_count);
            for _ in 0..parameter_count {
                parameters.push(decode_type(&mut reader)?);
            }
            let result = match reader.u8()? {
                0 => None,
                1 => Some(decode_type(&mut reader)?),
                value => return Err(DecodeError::InvalidBoolean(value)),
            };
            let async_result = match reader.u8()? {
                0 => None,
                1 => {
                    let result_type = StableId(reader.u64()?);
                    let success = decode_type(&mut reader)?;
                    let error = decode_type(&mut reader)?;
                    let cancel_policy = match reader.u8()? {
                        0 => CancelPolicy::ReturnError,
                        1 => CancelPolicy::CancelTask,
                        value => return Err(DecodeError::InvalidType(value)),
                    };
                    let abandon_policy = match reader.u8()? {
                        0 => AbandonPolicy::ReturnError,
                        1 => AbandonPolicy::Trap,
                        value => return Err(DecodeError::InvalidType(value)),
                    };
                    let cancel_error = read_optional_u32(&mut reader)?;
                    let abandon_error = read_optional_u32(&mut reader)?;
                    Some(AsyncResultType {
                        result_type,
                        success,
                        error,
                        cancel_policy,
                        abandon_policy,
                        cancel_error,
                        abandon_error,
                    })
                }
                value => return Err(DecodeError::InvalidBoolean(value)),
            };
            host_imports.push(HostImport {
                stable_id,
                declaration_fingerprint,
                capabilities,
                parameters,
                result,
                mode,
                fuel_cost,
                async_result,
            });
        }
        let enum_type_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if enum_type_count > limits.max_enum_types {
            return Err(DecodeError::ResourceLimit("enum types"));
        }
        let mut enum_types = Vec::with_capacity(enum_type_count);
        let mut total_enum_variants = 0_usize;
        for _ in 0..enum_type_count {
            let type_id = StableId(reader.u64()?);
            let variant_count = usize::from(reader.u16()?);
            total_enum_variants = total_enum_variants
                .checked_add(variant_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(
                total_enum_variants,
                limits.max_enum_variants,
                "enum variants",
            )?;
            let mut variants = Vec::with_capacity(variant_count);
            for _ in 0..variant_count {
                let stable_id = StableId(reader.u64()?);
                let tag = reader.u32()?;
                let payload_type = match reader.u8()? {
                    0 => None,
                    1 => Some(decode_type(&mut reader)?),
                    value => return Err(DecodeError::InvalidBoolean(value)),
                };
                variants.push(EnumVariant {
                    stable_id,
                    tag,
                    payload_type,
                });
            }
            enum_types.push(EnumType { type_id, variants });
        }
        let struct_type_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        enforce_limit(struct_type_count, limits.max_structs, "structs")?;
        let mut struct_types = Vec::with_capacity(struct_type_count);
        let mut total_fields = 0_usize;
        for _ in 0..struct_type_count {
            let type_id = StableId(reader.u64()?);
            let field_count = usize::from(reader.u16()?);
            total_fields = total_fields
                .checked_add(field_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_fields, limits.max_fields, "fields")?;
            let mut fields = Vec::with_capacity(field_count);
            for _ in 0..field_count {
                fields.push(StructField {
                    stable_id: StableId(reader.u64()?),
                    ty: decode_type(&mut reader)?,
                });
            }
            struct_types.push(StructType { type_id, fields });
        }
        let class_type_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        enforce_limit(class_type_count, limits.max_classes, "classes")?;
        let mut class_types = Vec::with_capacity(class_type_count);
        for _ in 0..class_type_count {
            let type_id = StableId(reader.u64()?);
            let field_count = usize::from(reader.u16()?);
            total_fields = total_fields
                .checked_add(field_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_fields, limits.max_fields, "fields")?;
            let mut fields = Vec::with_capacity(field_count);
            for _ in 0..field_count {
                fields.push(StructField {
                    stable_id: StableId(reader.u64()?),
                    ty: decode_type(&mut reader)?,
                });
            }
            class_types.push(ClassType { type_id, fields });
        }
        let state_type_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if state_type_count > limits.max_state_types {
            return Err(DecodeError::ResourceLimit("state types"));
        }
        if state_type_count > reader.remaining() {
            return Err(DecodeError::Truncated);
        }
        let mut state_types = Vec::with_capacity(state_type_count);
        for _ in 0..state_type_count {
            let stable_id = StableId(reader.u64()?);
            let version = reader.u32()?;
            let field_count = usize::from(reader.u16()?);
            total_fields = total_fields
                .checked_add(field_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_fields, limits.max_fields, "fields")?;
            if field_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut fields = Vec::with_capacity(field_count);
            for _ in 0..field_count {
                fields.push(StateField {
                    stable_id: StableId(reader.u64()?),
                    ty: decode_type(&mut reader)?,
                });
            }
            state_types.push(StateType {
                stable_id,
                version,
                fields,
            });
        }
        let export_count = usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if export_count > limits.max_exports {
            return Err(DecodeError::ResourceLimit("exports"));
        }
        let mut exports = Vec::with_capacity(export_count);
        for _ in 0..export_count {
            let stable_id = StableId(reader.u64()?);
            let function = reader.u32()?;
            let effect = decode_effect(reader.u8()?)?;
            let parameter_count = usize::from(reader.u16()?);
            if parameter_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut parameters = Vec::with_capacity(parameter_count);
            for _ in 0..parameter_count {
                parameters.push(decode_type(&mut reader)?);
            }
            let result = match reader.u8()? {
                0 => None,
                1 => Some(decode_type(&mut reader)?),
                value => return Err(DecodeError::InvalidBoolean(value)),
            };
            exports.push(ScriptExport {
                stable_id,
                function,
                signature: Signature { parameters, result },
                effect,
            });
        }
        if reader.cursor != metadata.len() {
            return Err(DecodeError::TrailingBytes);
        }
        let mut reader = Reader {
            bytes: function_bytes,
            cursor: 0,
        };
        let function_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if function_count > limits.max_functions {
            return Err(DecodeError::ResourceLimit("functions"));
        }
        if function_count > reader.remaining() {
            return Err(DecodeError::Truncated);
        }
        let mut functions = Vec::with_capacity(function_count);
        let mut total_instructions = 0_usize;
        let mut total_root_maps = 0_usize;
        let mut total_safepoints = 0_usize;
        let mut total_loop_bounds = 0_usize;
        for _ in 0..function_count {
            let parameter_count = usize::from(reader.u16()?);
            if parameter_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut parameters = Vec::with_capacity(parameter_count);
            for _ in 0..parameter_count {
                parameters.push(decode_type(&mut reader)?);
            }
            let result = match reader.u8()? {
                0 => None,
                1 => Some(decode_type(&mut reader)?),
                value => return Err(DecodeError::InvalidBoolean(value)),
            };
            let parameter_slots = reader.u16()?;
            let registers = reader.u16()?;
            if usize::from(registers) > limits.max_registers {
                return Err(DecodeError::ResourceLimit("registers"));
            }
            let frame_bytes = reader.u32()?;
            let effect = decode_effect(reader.u8()?)?;
            let max_static_call_depth = reader.u16()?;
            let root_count = usize::from(reader.u16()?);
            if root_count > limits.max_registers {
                return Err(DecodeError::ResourceLimit("root bitmap"));
            }
            if root_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut root_bitmap = Vec::with_capacity(root_count);
            for _ in 0..root_count {
                root_bitmap.push(match reader.u8()? {
                    0 => false,
                    1 => true,
                    value => return Err(DecodeError::InvalidBoolean(value)),
                });
            }
            let root_map_count =
                usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            total_root_maps = total_root_maps
                .checked_add(root_map_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_root_maps, limits.max_root_maps, "root maps")?;
            let mut root_maps = Vec::with_capacity(root_map_count);
            for _ in 0..root_map_count {
                let pc = reader.u32()?;
                let bitmap_len = usize::from(reader.u16()?);
                if bitmap_len > limits.max_registers {
                    return Err(DecodeError::ResourceLimit("root bitmap"));
                }
                let mut bitmap = Vec::with_capacity(bitmap_len);
                for _ in 0..bitmap_len {
                    bitmap.push(match reader.u8()? {
                        0 => false,
                        1 => true,
                        value => return Err(DecodeError::InvalidBoolean(value)),
                    });
                }
                root_maps.push(RootMap { pc, bitmap });
            }
            let safepoint_count =
                usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            total_safepoints = total_safepoints
                .checked_add(safepoint_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_safepoints, limits.max_safepoints, "safepoints")?;
            let mut safepoints = Vec::with_capacity(safepoint_count);
            for _ in 0..safepoint_count {
                safepoints.push(reader.u32()?);
            }
            let loop_bound_count =
                usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            total_loop_bounds = total_loop_bounds
                .checked_add(loop_bound_count)
                .ok_or(DecodeError::SizeOverflow)?;
            enforce_limit(total_loop_bounds, limits.max_loop_bounds, "loop bounds")?;
            let mut loop_bounds = Vec::with_capacity(loop_bound_count);
            for _ in 0..loop_bound_count {
                loop_bounds.push(LoopBound {
                    back_edge: reader.u32()?,
                    max_iterations: reader.u32()?,
                });
            }
            let instruction_count =
                usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            total_instructions = total_instructions
                .checked_add(instruction_count)
                .ok_or(DecodeError::SizeOverflow)?;
            if total_instructions > limits.max_instructions {
                return Err(DecodeError::ResourceLimit("instructions"));
            }
            if instruction_count > reader.remaining() {
                return Err(DecodeError::Truncated);
            }
            let mut code = Vec::with_capacity(instruction_count);
            for _ in 0..instruction_count {
                code.push(decode_instruction(&mut reader)?);
            }
            functions.push(Function {
                signature: Signature { parameters, result },
                parameter_slots,
                registers,
                frame_bytes,
                root_bitmap,
                root_maps,
                safepoints,
                loop_bounds,
                effect,
                max_static_call_depth,
                code,
            });
        }
        if reader.cursor != function_bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }
        validate_code_section(required_section(&sections, SectionKind::Code)?, &functions)?;
        validate_root_maps_section(
            required_section(&sections, SectionKind::RootMaps)?,
            &functions,
        )?;
        validate_safepoints_section(
            required_section(&sections, SectionKind::Safepoints)?,
            &functions,
        )?;
        validate_loop_bounds_section(
            required_section(&sections, SectionKind::LoopBounds)?,
            &functions,
        )?;
        let mut reader = Reader {
            bytes: source_map_bytes,
            cursor: 0,
        };
        let source_map_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if source_map_count > limits.max_source_map_entries {
            return Err(DecodeError::ResourceLimit("source map"));
        }
        if source_map_count > reader.remaining() / 24 {
            return Err(DecodeError::Truncated);
        }
        let mut source_map = Vec::with_capacity(source_map_count);
        for _ in 0..source_map_count {
            let entry = SourceMapEntry {
                function: reader.u32()?,
                pc_start: reader.u32()?,
                pc_end: reader.u32()?,
                span: SourceSpan::new(FileId(reader.u32()?), reader.u32()?, reader.u32()?),
            };
            if entry.pc_start >= entry.pc_end || entry.span.is_empty() {
                return Err(DecodeError::InvalidSourceMap);
            }
            source_map.push(entry);
        }
        if reader.cursor != source_map_bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(Self {
            strings,
            functions,
            state_handle_types,
            array_types,
            map_types,
            buffer_types,
            snapshot_types,
            resource_token_types,
            opaque_types,
            enum_types,
            struct_types,
            class_types,
            host_imports,
            exports,
            state_schema: StateSchema { types: state_types },
            host_contract_id,
            state_schema_fingerprint,
            reload_metadata,
            source_map,
        })
    }
}

fn encode_sections(sections: &[(SectionKind, Vec<u8>)]) -> Vec<u8> {
    const DIRECTORY_ENTRY_BYTES: usize = 20;
    let header_bytes = 8_usize
        .checked_add(
            sections
                .len()
                .checked_mul(DIRECTORY_ENTRY_BYTES)
                .expect("section directory size overflow"),
        )
        .expect("section directory size overflow");
    let mut offset = header_bytes;
    let mut output = Vec::with_capacity(
        header_bytes + sections.iter().map(|(_, bytes)| bytes.len()).sum::<usize>(),
    );
    output.extend_from_slice(&MAGIC);
    put_u16(&mut output, BYTECODE_VERSION);
    put_u16(
        &mut output,
        u16::try_from(sections.len()).expect("section count exceeds wire format"),
    );
    for (kind, bytes) in sections {
        put_u16(&mut output, *kind as u16);
        put_u16(&mut output, SECTION_FLAG_MANDATORY);
        put_u32(
            &mut output,
            u32::try_from(offset).expect("section offset exceeds wire format"),
        );
        put_u32(
            &mut output,
            u32::try_from(bytes.len()).expect("section length exceeds wire format"),
        );
        put_u32(
            &mut output,
            u32::from_le_bytes(
                bytes
                    .get(..4)
                    .expect("every v7 section starts with a count")
                    .try_into()
                    .expect("section count occupies four bytes"),
            ),
        );
        put_u32(&mut output, checksum(bytes));
        offset = offset
            .checked_add(bytes.len())
            .expect("section offset overflow");
    }
    for (_, bytes) in sections {
        output.extend_from_slice(bytes);
    }
    output
}

#[allow(clippy::too_many_lines)]
fn decode_sections(bytes: &[u8], max_sections: usize) -> Result<Vec<(u16, &[u8])>, DecodeError> {
    const DIRECTORY_ENTRY_BYTES: usize = 20;
    let mut reader = Reader { bytes, cursor: 0 };
    if reader.take(4)? != MAGIC {
        return Err(DecodeError::InvalidMagic);
    }
    let version = reader.u16()?;
    if version != BYTECODE_VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    let count = usize::from(reader.u16()?);
    if count == 0 || count > max_sections {
        return Err(DecodeError::InvalidSectionDirectory);
    }
    let directory_bytes = count
        .checked_mul(DIRECTORY_ENTRY_BYTES)
        .ok_or(DecodeError::OffsetOverflow)?;
    let directory_end = reader
        .cursor
        .checked_add(directory_bytes)
        .ok_or(DecodeError::OffsetOverflow)?;
    if directory_end > bytes.len() {
        return Err(DecodeError::Truncated);
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push(SectionEntry {
            kind: reader.u16()?,
            flags: reader.u16()?,
            offset: reader.u32()?,
            length: reader.u32()?,
            count: reader.u32()?,
            checksum: reader.u32()?,
        });
    }
    for entry in &entries {
        let start = usize::try_from(entry.offset).map_err(|_| DecodeError::OffsetOverflow)?;
        if start < directory_end || start > bytes.len() {
            return Err(DecodeError::OffsetOverflow);
        }
        let end = start
            .checked_add(usize::try_from(entry.length).map_err(|_| DecodeError::LengthOverflow)?)
            .ok_or(DecodeError::LengthOverflow)?;
        if end > bytes.len() {
            return Err(DecodeError::LengthOverflow);
        }
    }
    entries.sort_by_key(|entry| entry.offset);
    let mut previous_end = directory_end;
    let mut kinds = BTreeSet::new();
    let mut sections = Vec::with_capacity(count);
    for entry in entries {
        if entry.flags & !SECTION_FLAG_MANDATORY != 0 {
            return Err(DecodeError::InvalidSectionDirectory);
        }
        let known = SectionKind::ALL
            .iter()
            .any(|kind| *kind as u16 == entry.kind);
        if !known && entry.flags & SECTION_FLAG_MANDATORY != 0 {
            return Err(DecodeError::UnknownMandatorySection(entry.kind));
        }
        if !kinds.insert(entry.kind) {
            return Err(DecodeError::DuplicateRequiredSection(entry.kind));
        }
        let start = usize::try_from(entry.offset).map_err(|_| DecodeError::OffsetOverflow)?;
        let end = start
            .checked_add(usize::try_from(entry.length).map_err(|_| DecodeError::LengthOverflow)?)
            .ok_or(DecodeError::LengthOverflow)?;
        if start < directory_end {
            return Err(DecodeError::OffsetOverflow);
        }
        if start < previous_end {
            return Err(DecodeError::SectionOverlap);
        }
        if start > previous_end {
            return Err(DecodeError::TrailingBytes);
        }
        if end > bytes.len() {
            return Err(DecodeError::LengthOverflow);
        }
        let section = &bytes[start..end];
        if checksum(section) != entry.checksum {
            return Err(DecodeError::ChecksumMismatch(entry.kind));
        }
        if known {
            let actual_count = u32::from_le_bytes(
                section
                    .get(..4)
                    .ok_or(DecodeError::CountMismatch(entry.kind))?
                    .try_into()
                    .map_err(|_| DecodeError::CountMismatch(entry.kind))?,
            );
            if actual_count != entry.count {
                return Err(DecodeError::CountMismatch(entry.kind));
            }
            sections.push((entry.kind, section));
        }
        previous_end = end;
    }
    if previous_end != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(sections)
}

fn required_section<'a>(
    sections: &[(u16, &'a [u8])],
    kind: SectionKind,
) -> Result<&'a [u8], DecodeError> {
    sections
        .iter()
        .find_map(|(candidate, bytes)| (*candidate == kind as u16).then_some(*bytes))
        .ok_or(DecodeError::InvalidSectionDirectory)
}

fn validate_empty_section(bytes: &[u8], kind: SectionKind) -> Result<(), DecodeError> {
    if bytes != 0_u32.to_le_bytes() {
        return Err(DecodeError::InconsistentSection(kind as u16));
    }
    Ok(())
}

fn validate_section_function_count(
    reader: &mut Reader<'_>,
    kind: SectionKind,
    functions: &[Function],
) -> Result<(), DecodeError> {
    let count = usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
    if count != functions.len() {
        return Err(DecodeError::InconsistentSection(kind as u16));
    }
    Ok(())
}

fn validate_section_end(reader: &Reader<'_>, kind: SectionKind) -> Result<(), DecodeError> {
    if reader.remaining() != 0 {
        return Err(DecodeError::InconsistentSection(kind as u16));
    }
    Ok(())
}

fn validate_code_section(bytes: &[u8], functions: &[Function]) -> Result<(), DecodeError> {
    let kind = SectionKind::Code;
    let mut reader = Reader { bytes, cursor: 0 };
    validate_section_function_count(&mut reader, kind, functions)?;
    for function in functions {
        let instruction_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if instruction_count != function.code.len() {
            return Err(DecodeError::InconsistentSection(kind as u16));
        }
        for expected in &function.code {
            if decode_instruction(&mut reader)? != *expected {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
        }
    }
    validate_section_end(&reader, kind)
}

fn validate_root_maps_section(bytes: &[u8], functions: &[Function]) -> Result<(), DecodeError> {
    let kind = SectionKind::RootMaps;
    let mut reader = Reader { bytes, cursor: 0 };
    validate_section_function_count(&mut reader, kind, functions)?;
    for function in functions {
        let root_count = usize::from(reader.u16()?);
        if root_count != function.root_bitmap.len() {
            return Err(DecodeError::InconsistentSection(kind as u16));
        }
        for expected in &function.root_bitmap {
            if decode_boolean(&mut reader)? != *expected {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
        }

        let root_map_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if root_map_count != function.root_maps.len() {
            return Err(DecodeError::InconsistentSection(kind as u16));
        }
        for expected in &function.root_maps {
            if reader.u32()? != expected.pc {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
            let bitmap_len = usize::from(reader.u16()?);
            if bitmap_len != expected.bitmap.len() {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
            for expected in &expected.bitmap {
                if decode_boolean(&mut reader)? != *expected {
                    return Err(DecodeError::InconsistentSection(kind as u16));
                }
            }
        }
    }
    validate_section_end(&reader, kind)
}

fn validate_safepoints_section(bytes: &[u8], functions: &[Function]) -> Result<(), DecodeError> {
    let kind = SectionKind::Safepoints;
    let mut reader = Reader { bytes, cursor: 0 };
    validate_section_function_count(&mut reader, kind, functions)?;
    for function in functions {
        let safepoint_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if safepoint_count != function.safepoints.len() {
            return Err(DecodeError::InconsistentSection(kind as u16));
        }
        for expected in &function.safepoints {
            if reader.u32()? != *expected {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
        }
    }
    validate_section_end(&reader, kind)
}

fn validate_loop_bounds_section(bytes: &[u8], functions: &[Function]) -> Result<(), DecodeError> {
    let kind = SectionKind::LoopBounds;
    let mut reader = Reader { bytes, cursor: 0 };
    validate_section_function_count(&mut reader, kind, functions)?;
    for function in functions {
        let loop_bound_count =
            usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
        if loop_bound_count != function.loop_bounds.len() {
            return Err(DecodeError::InconsistentSection(kind as u16));
        }
        for expected in &function.loop_bounds {
            if reader.u32()? != expected.back_edge || reader.u32()? != expected.max_iterations {
                return Err(DecodeError::InconsistentSection(kind as u16));
            }
        }
    }
    validate_section_end(&reader, kind)
}

fn decode_boolean(reader: &mut Reader<'_>) -> Result<bool, DecodeError> {
    match reader.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(DecodeError::InvalidBoolean(value)),
    }
}

fn enforce_section_limit(
    sections: &[(u16, &[u8])],
    kind: SectionKind,
    limit: usize,
    name: &'static str,
) -> Result<(), DecodeError> {
    let bytes = required_section(sections, kind)?;
    let count = usize::try_from(u32::from_le_bytes(
        bytes
            .get(..4)
            .ok_or(DecodeError::Truncated)?
            .try_into()
            .map_err(|_| DecodeError::Truncated)?,
    ))
    .map_err(|_| DecodeError::SizeOverflow)?;
    enforce_limit(count, limit, name)
}

fn enforce_limit(value: usize, limit: usize, name: &'static str) -> Result<(), DecodeError> {
    if value > limit {
        Err(DecodeError::ResourceLimit(name))
    } else {
        Ok(())
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn encode_effect(effect: FunctionEffect) -> u8 {
    match effect {
        FunctionEffect::Ordinary => 0,
        FunctionEffect::Task => 1,
        FunctionEffect::Immediate => 2,
        FunctionEffect::Migration => 3,
        FunctionEffect::Cleanup => 4,
    }
}

fn decode_effect(value: u8) -> Result<FunctionEffect, DecodeError> {
    match value {
        0 => Ok(FunctionEffect::Ordinary),
        1 => Ok(FunctionEffect::Task),
        2 => Ok(FunctionEffect::Immediate),
        3 => Ok(FunctionEffect::Migration),
        4 => Ok(FunctionEffect::Cleanup),
        value => Err(DecodeError::InvalidType(value)),
    }
}

fn encode_type(output: &mut Vec<u8>, ty: ValueType) {
    match ty {
        ValueType::I32 => output.push(0),
        ValueType::Bool => output.push(1),
        ValueType::Ref => output.push(2),
        ValueType::Named(id) => {
            output.push(3);
            put_u64(output, id.0);
        }
        ValueType::I64 => output.push(4),
        ValueType::F32 => output.push(5),
        ValueType::F64 => output.push(6),
        ValueType::Rune => output.push(7),
        ValueType::String => output.push(8),
    }
}

fn decode_type(reader: &mut Reader<'_>) -> Result<ValueType, DecodeError> {
    match reader.u8()? {
        0 => Ok(ValueType::I32),
        1 => Ok(ValueType::Bool),
        2 => Ok(ValueType::Ref),
        3 => Ok(ValueType::Named(StableId(reader.u64()?))),
        4 => Ok(ValueType::I64),
        5 => Ok(ValueType::F32),
        6 => Ok(ValueType::F64),
        7 => Ok(ValueType::Rune),
        8 => Ok(ValueType::String),
        value => Err(DecodeError::InvalidType(value)),
    }
}

fn encode_standard_intrinsic(output: &mut Vec<u8>, intrinsic: StandardIntrinsic) {
    let (tag, types): (u8, &[ValueType]) = match &intrinsic {
        StandardIntrinsic::OptionIsSome { value } => (0, std::slice::from_ref(value)),
        StandardIntrinsic::OptionIsNone { value } => (1, std::slice::from_ref(value)),
        StandardIntrinsic::ResultIsOk { success, error } => (2, &[*success, *error]),
        StandardIntrinsic::ResultIsErr { success, error } => (3, &[*success, *error]),
        StandardIntrinsic::OptionUnwrapOr { value } => (4, std::slice::from_ref(value)),
        StandardIntrinsic::ResultUnwrapOr { success, error } => (5, &[*success, *error]),
        StandardIntrinsic::F32Floor => (6, &[]),
        StandardIntrinsic::F64Floor => (7, &[]),
        StandardIntrinsic::F32Ceil => (8, &[]),
        StandardIntrinsic::F64Ceil => (9, &[]),
        StandardIntrinsic::F32Round => (10, &[]),
        StandardIntrinsic::F64Round => (11, &[]),
        StandardIntrinsic::F32Sqrt => (12, &[]),
        StandardIntrinsic::F64Sqrt => (13, &[]),
        StandardIntrinsic::F32Sin => (14, &[]),
        StandardIntrinsic::F64Sin => (15, &[]),
        StandardIntrinsic::F32Cos => (16, &[]),
        StandardIntrinsic::F64Cos => (17, &[]),
        StandardIntrinsic::StringContains => (18, &[]),
        StandardIntrinsic::StringStartsWith => (19, &[]),
        StandardIntrinsic::StringEndsWith => (20, &[]),
        StandardIntrinsic::StringSubstring => (21, &[]),
        StandardIntrinsic::StringTrim => (22, &[]),
        StandardIntrinsic::StringSplit => (23, &[]),
        StandardIntrinsic::ArrayLen { element } => (24, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayIsEmpty { element } => (25, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayGet { element } => (26, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayPush { element } => (27, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayPop { element } => (28, std::slice::from_ref(element)),
        StandardIntrinsic::MapLen { key, value } => (29, &[*key, *value]),
        StandardIntrinsic::MapContains { key, value } => (30, &[*key, *value]),
        StandardIntrinsic::MapGet { key, value } => (31, &[*key, *value]),
        StandardIntrinsic::MapInsert { key, value } => (32, &[*key, *value]),
        StandardIntrinsic::MapRemove { key, value } => (33, &[*key, *value]),
        StandardIntrinsic::DebugAssert => (34, &[]),
        StandardIntrinsic::DebugTrap => (35, &[]),
        StandardIntrinsic::StringLen => (36, &[]),
        StandardIntrinsic::StringByteLen => (37, &[]),
        StandardIntrinsic::ArrayReserve { element } => (38, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayCapacity { element } => (39, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayClear { element } => (40, std::slice::from_ref(element)),
        StandardIntrinsic::ArrayShrinkToFit { element } => (41, std::slice::from_ref(element)),
    };
    output.push(tag);
    for ty in types {
        encode_type(output, *ty);
    }
}

fn decode_standard_intrinsic(reader: &mut Reader<'_>) -> Result<StandardIntrinsic, DecodeError> {
    let unary = |reader: &mut Reader<'_>| decode_type(reader);
    let binary = |reader: &mut Reader<'_>| Ok((decode_type(reader)?, decode_type(reader)?));
    Ok(match reader.u8()? {
        0 => StandardIntrinsic::OptionIsSome {
            value: unary(reader)?,
        },
        1 => StandardIntrinsic::OptionIsNone {
            value: unary(reader)?,
        },
        2 => {
            let (success, error) = binary(reader)?;
            StandardIntrinsic::ResultIsOk { success, error }
        }
        3 => {
            let (success, error) = binary(reader)?;
            StandardIntrinsic::ResultIsErr { success, error }
        }
        4 => StandardIntrinsic::OptionUnwrapOr {
            value: unary(reader)?,
        },
        5 => {
            let (success, error) = binary(reader)?;
            StandardIntrinsic::ResultUnwrapOr { success, error }
        }
        6 => StandardIntrinsic::F32Floor,
        7 => StandardIntrinsic::F64Floor,
        8 => StandardIntrinsic::F32Ceil,
        9 => StandardIntrinsic::F64Ceil,
        10 => StandardIntrinsic::F32Round,
        11 => StandardIntrinsic::F64Round,
        12 => StandardIntrinsic::F32Sqrt,
        13 => StandardIntrinsic::F64Sqrt,
        14 => StandardIntrinsic::F32Sin,
        15 => StandardIntrinsic::F64Sin,
        16 => StandardIntrinsic::F32Cos,
        17 => StandardIntrinsic::F64Cos,
        18 => StandardIntrinsic::StringContains,
        19 => StandardIntrinsic::StringStartsWith,
        20 => StandardIntrinsic::StringEndsWith,
        21 => StandardIntrinsic::StringSubstring,
        22 => StandardIntrinsic::StringTrim,
        23 => StandardIntrinsic::StringSplit,
        24 => StandardIntrinsic::ArrayLen {
            element: unary(reader)?,
        },
        25 => StandardIntrinsic::ArrayIsEmpty {
            element: unary(reader)?,
        },
        26 => StandardIntrinsic::ArrayGet {
            element: unary(reader)?,
        },
        27 => StandardIntrinsic::ArrayPush {
            element: unary(reader)?,
        },
        28 => StandardIntrinsic::ArrayPop {
            element: unary(reader)?,
        },
        29 => {
            let (key, value) = binary(reader)?;
            StandardIntrinsic::MapLen { key, value }
        }
        30 => {
            let (key, value) = binary(reader)?;
            StandardIntrinsic::MapContains { key, value }
        }
        31 => {
            let (key, value) = binary(reader)?;
            StandardIntrinsic::MapGet { key, value }
        }
        32 => {
            let (key, value) = binary(reader)?;
            StandardIntrinsic::MapInsert { key, value }
        }
        33 => {
            let (key, value) = binary(reader)?;
            StandardIntrinsic::MapRemove { key, value }
        }
        34 => StandardIntrinsic::DebugAssert,
        35 => StandardIntrinsic::DebugTrap,
        36 => StandardIntrinsic::StringLen,
        37 => StandardIntrinsic::StringByteLen,
        38 => StandardIntrinsic::ArrayReserve {
            element: unary(reader)?,
        },
        39 => StandardIntrinsic::ArrayCapacity {
            element: unary(reader)?,
        },
        40 => StandardIntrinsic::ArrayClear {
            element: unary(reader)?,
        },
        41 => StandardIntrinsic::ArrayShrinkToFit {
            element: unary(reader)?,
        },
        value => return Err(DecodeError::InvalidStandardIntrinsic(value)),
    })
}

#[allow(clippy::too_many_lines)]
fn encode_instruction(output: &mut Vec<u8>, instruction: Instruction) {
    match instruction {
        Instruction::LoadI32 { dst, value } => {
            output.push(0);
            put_u16(output, dst);
            output.extend_from_slice(&value.to_le_bytes());
        }
        Instruction::LoadBool { dst, value } => {
            output.push(1);
            put_u16(output, dst);
            output.push(u8::from(value));
        }
        Instruction::LoadI64 { dst, value } => {
            output.push(36);
            put_u16(output, dst);
            output.extend_from_slice(&value.to_le_bytes());
        }
        Instruction::LoadF32 { dst, bits } => {
            output.push(37);
            put_u16(output, dst);
            put_u32(output, bits);
        }
        Instruction::LoadF64 { dst, bits } => {
            output.push(38);
            put_u16(output, dst);
            put_u64(output, bits);
        }
        Instruction::LoadRune { dst, value } => {
            output.push(39);
            put_u16(output, dst);
            put_u32(output, value);
        }
        Instruction::LoadString { dst, string } => {
            output.push(53);
            put_u16(output, dst);
            put_u32(output, string);
        }
        Instruction::Move { dst, source } => {
            output.push(2);
            put_u16(output, dst);
            put_u16(output, source);
        }
        Instruction::Add { dst, lhs, rhs }
        | Instruction::Sub { dst, lhs, rhs }
        | Instruction::Mul { dst, lhs, rhs }
        | Instruction::CompareEq { dst, lhs, rhs }
        | Instruction::CompareLtI32 { dst, lhs, rhs }
        | Instruction::CompareLtI64 { dst, lhs, rhs }
        | Instruction::CompareLtF32 { dst, lhs, rhs }
        | Instruction::CompareLtF64 { dst, lhs, rhs } => {
            output.push(match instruction {
                Instruction::Add { .. } => 3,
                Instruction::Sub { .. } => 4,
                Instruction::Mul { .. } => 5,
                Instruction::CompareEq { .. } => 6,
                Instruction::CompareLtI32 { .. } => 95,
                Instruction::CompareLtI64 { .. } => 96,
                Instruction::CompareLtF32 { .. } => 97,
                Instruction::CompareLtF64 { .. } => 98,
                _ => unreachable!(),
            });
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
        }
        Instruction::Div { dst, lhs, rhs } => {
            output.push(44);
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
        }
        Instruction::RemI32 { dst, lhs, rhs } => {
            output.push(101);
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
        }
        Instruction::AddI64 { dst, lhs, rhs }
        | Instruction::SubI64 { dst, lhs, rhs }
        | Instruction::MulI64 { dst, lhs, rhs }
        | Instruction::DivI64 { dst, lhs, rhs }
        | Instruction::RemI64 { dst, lhs, rhs }
        | Instruction::AddF32 { dst, lhs, rhs }
        | Instruction::SubF32 { dst, lhs, rhs }
        | Instruction::MulF32 { dst, lhs, rhs }
        | Instruction::DivF32 { dst, lhs, rhs }
        | Instruction::RemF32 { dst, lhs, rhs }
        | Instruction::AddF64 { dst, lhs, rhs }
        | Instruction::SubF64 { dst, lhs, rhs }
        | Instruction::MulF64 { dst, lhs, rhs }
        | Instruction::DivF64 { dst, lhs, rhs }
        | Instruction::RemF64 { dst, lhs, rhs } => {
            output.push(match instruction {
                Instruction::AddI64 { .. } => 40,
                Instruction::SubI64 { .. } => 41,
                Instruction::MulI64 { .. } => 42,
                Instruction::DivI64 { .. } => 43,
                Instruction::RemI64 { .. } => 102,
                Instruction::AddF32 { .. } => 45,
                Instruction::SubF32 { .. } => 46,
                Instruction::MulF32 { .. } => 47,
                Instruction::DivF32 { .. } => 48,
                Instruction::RemF32 { .. } => 103,
                Instruction::AddF64 { .. } => 49,
                Instruction::SubF64 { .. } => 50,
                Instruction::MulF64 { .. } => 51,
                Instruction::DivF64 { .. } => 52,
                Instruction::RemF64 { .. } => 104,
                _ => unreachable!(),
            });
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
        }
        Instruction::StringLen { dst, source }
        | Instruction::StringByteLen { dst, source }
        | Instruction::StringHash { dst, source }
        | Instruction::I32ToString { dst, source }
        | Instruction::I64ToString { dst, source }
        | Instruction::F32ToString { dst, source }
        | Instruction::F64ToString { dst, source }
        | Instruction::BoolToString { dst, source }
        | Instruction::RuneToString { dst, source }
        | Instruction::StringToString { dst, source } => {
            output.push(match instruction {
                Instruction::StringLen { .. } => 54,
                Instruction::StringByteLen { .. } => 55,
                Instruction::StringHash { .. } => 59,
                Instruction::I32ToString { .. } => 89,
                Instruction::I64ToString { .. } => 90,
                Instruction::F32ToString { .. } => 91,
                Instruction::F64ToString { .. } => 92,
                Instruction::BoolToString { .. } => 93,
                Instruction::RuneToString { .. } => 94,
                Instruction::StringToString { .. } => 99,
                _ => unreachable!(),
            });
            put_u16(output, dst);
            put_u16(output, source);
        }
        Instruction::StringEqual { dst, lhs, rhs }
        | Instruction::StringConcat { dst, lhs, rhs }
        | Instruction::StringRuneAt {
            dst,
            source: lhs,
            index: rhs,
        } => {
            output.push(match instruction {
                Instruction::StringEqual { .. } => 56,
                Instruction::StringConcat { .. } => 57,
                Instruction::StringRuneAt { .. } => 58,
                _ => unreachable!(),
            });
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
        }
        Instruction::StringBuild {
            dst,
            parts_base,
            parts_count,
        } => {
            output.push(109);
            put_u16(output, dst);
            put_u16(output, parts_base);
            put_u16(output, parts_count);
        }
        Instruction::StandardIntrinsic {
            intrinsic,
            args_base,
            args_count,
            dst,
        } => {
            output.push(100);
            encode_standard_intrinsic(output, intrinsic);
            put_u16(output, args_base);
            put_u16(output, args_count);
            put_u16(output, dst);
        }
        Instruction::StructNew {
            type_id,
            fields_base,
            fields_count,
            dst,
        } => {
            output.push(60);
            put_u64(output, type_id.0);
            put_u16(output, fields_base);
            put_u16(output, fields_count);
            put_u16(output, dst);
        }
        Instruction::StructGet { source, field, dst } => {
            output.push(61);
            put_u16(output, source);
            put_u64(output, field.0);
            put_u16(output, dst);
        }
        Instruction::StructWith {
            source,
            field,
            value,
            dst,
        } => {
            output.push(62);
            put_u16(output, source);
            put_u64(output, field.0);
            put_u16(output, value);
            put_u16(output, dst);
        }
        Instruction::StructEqual { lhs, rhs, dst } => {
            output.push(63);
            put_u16(output, lhs);
            put_u16(output, rhs);
            put_u16(output, dst);
        }
        Instruction::ClassNew {
            type_id,
            fields_base,
            fields_count,
            dst,
        } => {
            output.push(64);
            put_u64(output, type_id.0);
            put_u16(output, fields_base);
            put_u16(output, fields_count);
            put_u16(output, dst);
        }
        Instruction::ClassGet { source, field, dst } => {
            output.push(65);
            put_u16(output, source);
            put_u64(output, field.0);
            put_u16(output, dst);
        }
        Instruction::ClassSet {
            source,
            field,
            value,
        } => {
            output.push(66);
            put_u16(output, source);
            put_u64(output, field.0);
            put_u16(output, value);
        }
        Instruction::ClassEqual { lhs, rhs, dst } => {
            output.push(67);
            put_u16(output, lhs);
            put_u16(output, rhs);
            put_u16(output, dst);
        }
        Instruction::ArrayNew { type_id, dst } => {
            output.push(68);
            put_u64(output, type_id.0);
            put_u16(output, dst);
        }
        Instruction::ArrayLen { source, dst } => {
            output.push(69);
            put_u16(output, source);
            put_u16(output, dst);
        }
        Instruction::ArrayGet { source, index, dst }
        | Instruction::ArrayRemove { source, index, dst } => {
            output.push(match instruction {
                Instruction::ArrayGet { .. } => 70,
                Instruction::ArrayRemove { .. } => 75,
                _ => unreachable!(),
            });
            put_u16(output, source);
            put_u16(output, index);
            put_u16(output, dst);
        }
        Instruction::ArrayFieldGet {
            source,
            index,
            field,
            dst,
        } => {
            output.push(107);
            put_u16(output, source);
            put_u16(output, index);
            put_u16(output, field);
            put_u16(output, dst);
        }
        Instruction::ArrayPushRow {
            source,
            fields_base,
            fields_count,
        } => {
            output.push(108);
            put_u16(output, source);
            put_u16(output, fields_base);
            put_u16(output, fields_count);
        }
        Instruction::ArraySet {
            source,
            index,
            value,
        }
        | Instruction::ArrayInsert {
            source,
            index,
            value,
        } => {
            output.push(match instruction {
                Instruction::ArraySet { .. } => 71,
                Instruction::ArrayInsert { .. } => 74,
                _ => unreachable!(),
            });
            put_u16(output, source);
            put_u16(output, index);
            put_u16(output, value);
        }
        Instruction::ArrayPush { source, value } => {
            output.push(72);
            put_u16(output, source);
            put_u16(output, value);
        }
        Instruction::ArrayPop { source, dst } => {
            output.push(73);
            put_u16(output, source);
            put_u16(output, dst);
        }
        Instruction::ArrayClear { source } => {
            output.push(76);
            put_u16(output, source);
        }
        Instruction::MapNew { type_id, dst } => {
            output.push(77);
            put_u64(output, type_id.0);
            put_u16(output, dst);
        }
        Instruction::MapLen { source, dst } => {
            output.push(78);
            put_u16(output, source);
            put_u16(output, dst);
        }
        Instruction::MapGet {
            source,
            key,
            result_type,
            dst,
        }
        | Instruction::MapRemove {
            source,
            key,
            result_type,
            dst,
        } => {
            output.push(match instruction {
                Instruction::MapGet { .. } => 79,
                Instruction::MapRemove { .. } => 81,
                _ => unreachable!(),
            });
            put_u16(output, source);
            put_u16(output, key);
            put_u64(output, result_type.0);
            put_u16(output, dst);
        }
        Instruction::MapSet { source, key, value } => {
            output.push(80);
            put_u16(output, source);
            put_u16(output, key);
            put_u16(output, value);
        }
        Instruction::MapContains { source, key, dst } => {
            output.push(82);
            put_u16(output, source);
            put_u16(output, key);
            put_u16(output, dst);
        }
        Instruction::MapClear { source } => {
            output.push(83);
            put_u16(output, source);
        }
        Instruction::BufferLen { source, dst } => {
            output.push(84);
            put_u16(output, source);
            put_u16(output, dst);
        }
        Instruction::BufferGet { source, index, dst } => {
            output.push(85);
            put_u16(output, source);
            put_u16(output, index);
            put_u16(output, dst);
        }
        Instruction::BufferSet {
            source,
            index,
            value,
        } => {
            output.push(86);
            put_u16(output, source);
            put_u16(output, index);
            put_u16(output, value);
        }
        Instruction::BufferSlice {
            source,
            start,
            length,
            dst,
        } => {
            output.push(87);
            put_u16(output, source);
            put_u16(output, start);
            put_u16(output, length);
            put_u16(output, dst);
        }
        Instruction::BufferCopy {
            destination,
            source,
            source_start,
            destination_start,
            length,
        } => {
            output.push(88);
            put_u16(output, destination);
            put_u16(output, source);
            put_u16(output, source_start);
            put_u16(output, destination_start);
            put_u16(output, length);
        }
        Instruction::Jump { target } => {
            output.push(7);
            put_u32(output, target);
        }
        Instruction::JumpIfFalse { condition, target } => {
            output.push(8);
            put_u16(output, condition);
            put_u32(output, target);
        }
        Instruction::Call {
            function,
            args_base,
            args_count,
            dst,
        } => {
            output.push(9);
            put_u32(output, function);
            put_u16(output, args_base);
            put_u16(output, args_count);
            put_u16(output, dst);
        }
        Instruction::HostCall {
            import,
            args_base,
            args_count,
            dst,
        } => {
            output.push(18);
            put_u32(output, import);
            put_u16(output, args_base);
            put_u16(output, args_count);
            put_u16(output, dst);
        }
        Instruction::StateOldGet { stable_id, ty, dst } => {
            output.push(19);
            put_u64(output, stable_id.0);
            encode_type(output, ty);
            put_u16(output, dst);
        }
        Instruction::StateCurrentGet {
            stable_id,
            type_id,
            dst,
        } => {
            output.push(105);
            put_u64(output, stable_id.0);
            put_u64(output, type_id.0);
            put_u16(output, dst);
        }
        Instruction::StateNewCreate {
            stable_id,
            type_id,
            dst,
        } => {
            output.push(20);
            put_u64(output, stable_id.0);
            put_u64(output, type_id.0);
            put_u16(output, dst);
        }
        Instruction::StateNewSet {
            object,
            field_id,
            source,
        } => {
            output.push(21);
            put_u16(output, object);
            put_u64(output, field_id.0);
            put_u16(output, source);
        }
        Instruction::StateReplace { old_id, target } => {
            output.push(22);
            put_u64(output, old_id.0);
            put_u16(output, target);
        }
        Instruction::StateDelete { stable_id } => {
            output.push(23);
            put_u64(output, stable_id.0);
        }
        Instruction::EnumNew {
            type_id,
            variant,
            payload,
            dst,
        } => {
            output.push(24);
            put_u64(output, type_id.0);
            put_u64(output, variant.0);
            output.push(u8::from(payload.is_some()));
            if let Some(payload) = payload {
                put_u16(output, payload);
            }
            put_u16(output, dst);
        }
        Instruction::EnumTag { source, dst } => {
            output.push(25);
            put_u16(output, source);
            put_u16(output, dst);
        }
        Instruction::EnumPayload {
            source,
            variant,
            dst,
        } => {
            output.push(26);
            put_u16(output, source);
            put_u64(output, variant.0);
            put_u16(output, dst);
        }
        Instruction::EnumEqual { lhs, rhs, dst } => {
            output.push(106);
            put_u16(output, lhs);
            put_u16(output, rhs);
            put_u16(output, dst);
        }
        Instruction::StatePreserve { stable_id } => {
            output.push(27);
            put_u64(output, stable_id.0);
        }
        Instruction::StateFinish => output.push(28),
        Instruction::StateOldFieldGet {
            object,
            field_id,
            ty,
            dst,
        } => {
            output.push(29);
            put_u16(output, object);
            put_u64(output, field_id.0);
            encode_type(output, ty);
            put_u16(output, dst);
        }
        Instruction::StateHandleResolve {
            handle,
            target,
            result_type,
            dst,
        } => {
            output.push(30);
            put_u16(output, handle);
            encode_type(output, target);
            put_u64(output, result_type.0);
            put_u16(output, dst);
        }
        Instruction::StateHandleIsAlive {
            handle,
            target,
            dst,
        } => {
            output.push(31);
            put_u16(output, handle);
            encode_type(output, target);
            put_u16(output, dst);
        }
        Instruction::StateHandleStableId {
            handle,
            target,
            dst,
        } => {
            output.push(32);
            put_u16(output, handle);
            encode_type(output, target);
            put_u16(output, dst);
        }
        Instruction::StateHandleGeneration {
            handle,
            target,
            dst,
        } => {
            output.push(33);
            put_u16(output, handle);
            encode_type(output, target);
            put_u16(output, dst);
        }
        Instruction::StateHandleEqual {
            lhs,
            rhs,
            target,
            dst,
        } => {
            output.push(34);
            put_u16(output, lhs);
            put_u16(output, rhs);
            encode_type(output, target);
            put_u16(output, dst);
        }
        Instruction::StateHandleHash {
            handle,
            target,
            dst,
        } => {
            output.push(35);
            put_u16(output, handle);
            encode_type(output, target);
            put_u16(output, dst);
        }
        Instruction::Return { source } => {
            output.push(10);
            put_u16(output, source);
        }
        Instruction::ReturnVoid => output.push(11),
        Instruction::Safepoint => output.push(12),
        Instruction::Yield => output.push(13),
        Instruction::Trap => output.push(14),
        Instruction::DeferPush {
            function,
            args_base,
            args_count,
        } => {
            output.push(15);
            put_u32(output, function);
            put_u16(output, args_base);
            put_u16(output, args_count);
        }
        Instruction::DeferPop => output.push(16),
        Instruction::CleanupReturn => output.push(17),
    }
}

#[allow(clippy::too_many_lines)]
fn decode_instruction(reader: &mut Reader<'_>) -> Result<Instruction, DecodeError> {
    Ok(match reader.u8()? {
        0 => Instruction::LoadI32 {
            dst: reader.u16()?,
            value: i32::from_le_bytes(reader.array()?),
        },
        1 => Instruction::LoadBool {
            dst: reader.u16()?,
            value: match reader.u8()? {
                0 => false,
                1 => true,
                value => return Err(DecodeError::InvalidBoolean(value)),
            },
        },
        2 => Instruction::Move {
            dst: reader.u16()?,
            source: reader.u16()?,
        },
        opcode @ (3..=6 | 95..=98) => {
            let dst = reader.u16()?;
            let lhs = reader.u16()?;
            let rhs = reader.u16()?;
            match opcode {
                3 => Instruction::Add { dst, lhs, rhs },
                4 => Instruction::Sub { dst, lhs, rhs },
                5 => Instruction::Mul { dst, lhs, rhs },
                6 => Instruction::CompareEq { dst, lhs, rhs },
                95 => Instruction::CompareLtI32 { dst, lhs, rhs },
                96 => Instruction::CompareLtI64 { dst, lhs, rhs },
                97 => Instruction::CompareLtF32 { dst, lhs, rhs },
                98 => Instruction::CompareLtF64 { dst, lhs, rhs },
                _ => unreachable!(),
            }
        }
        7 => Instruction::Jump {
            target: reader.u32()?,
        },
        8 => Instruction::JumpIfFalse {
            condition: reader.u16()?,
            target: reader.u32()?,
        },
        9 => Instruction::Call {
            function: reader.u32()?,
            args_base: reader.u16()?,
            args_count: reader.u16()?,
            dst: reader.u16()?,
        },
        10 => Instruction::Return {
            source: reader.u16()?,
        },
        11 => Instruction::ReturnVoid,
        12 => Instruction::Safepoint,
        13 => Instruction::Yield,
        14 => Instruction::Trap,
        15 => Instruction::DeferPush {
            function: reader.u32()?,
            args_base: reader.u16()?,
            args_count: reader.u16()?,
        },
        16 => Instruction::DeferPop,
        17 => Instruction::CleanupReturn,
        18 => Instruction::HostCall {
            import: reader.u32()?,
            args_base: reader.u16()?,
            args_count: reader.u16()?,
            dst: reader.u16()?,
        },
        19 => Instruction::StateOldGet {
            stable_id: StableId(reader.u64()?),
            ty: decode_type(reader)?,
            dst: reader.u16()?,
        },
        105 => Instruction::StateCurrentGet {
            stable_id: StableId(reader.u64()?),
            type_id: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        20 => Instruction::StateNewCreate {
            stable_id: StableId(reader.u64()?),
            type_id: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        21 => Instruction::StateNewSet {
            object: reader.u16()?,
            field_id: StableId(reader.u64()?),
            source: reader.u16()?,
        },
        22 => Instruction::StateReplace {
            old_id: StableId(reader.u64()?),
            target: reader.u16()?,
        },
        23 => Instruction::StateDelete {
            stable_id: StableId(reader.u64()?),
        },
        24 => Instruction::EnumNew {
            type_id: StableId(reader.u64()?),
            variant: StableId(reader.u64()?),
            payload: match reader.u8()? {
                0 => None,
                1 => Some(reader.u16()?),
                value => return Err(DecodeError::InvalidBoolean(value)),
            },
            dst: reader.u16()?,
        },
        25 => Instruction::EnumTag {
            source: reader.u16()?,
            dst: reader.u16()?,
        },
        26 => Instruction::EnumPayload {
            source: reader.u16()?,
            variant: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        106 => Instruction::EnumEqual {
            lhs: reader.u16()?,
            rhs: reader.u16()?,
            dst: reader.u16()?,
        },
        27 => Instruction::StatePreserve {
            stable_id: StableId(reader.u64()?),
        },
        28 => Instruction::StateFinish,
        29 => Instruction::StateOldFieldGet {
            object: reader.u16()?,
            field_id: StableId(reader.u64()?),
            ty: decode_type(reader)?,
            dst: reader.u16()?,
        },
        30 => Instruction::StateHandleResolve {
            handle: reader.u16()?,
            target: decode_type(reader)?,
            result_type: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        31 => Instruction::StateHandleIsAlive {
            handle: reader.u16()?,
            target: decode_type(reader)?,
            dst: reader.u16()?,
        },
        32 => Instruction::StateHandleStableId {
            handle: reader.u16()?,
            target: decode_type(reader)?,
            dst: reader.u16()?,
        },
        33 => Instruction::StateHandleGeneration {
            handle: reader.u16()?,
            target: decode_type(reader)?,
            dst: reader.u16()?,
        },
        34 => Instruction::StateHandleEqual {
            lhs: reader.u16()?,
            rhs: reader.u16()?,
            target: decode_type(reader)?,
            dst: reader.u16()?,
        },
        35 => Instruction::StateHandleHash {
            handle: reader.u16()?,
            target: decode_type(reader)?,
            dst: reader.u16()?,
        },
        36 => Instruction::LoadI64 {
            dst: reader.u16()?,
            value: i64::from_le_bytes(reader.array()?),
        },
        37 => Instruction::LoadF32 {
            dst: reader.u16()?,
            bits: reader.u32()?,
        },
        38 => Instruction::LoadF64 {
            dst: reader.u16()?,
            bits: reader.u64()?,
        },
        39 => Instruction::LoadRune {
            dst: reader.u16()?,
            value: reader.u32()?,
        },
        opcode @ 40..=52 => {
            let dst = reader.u16()?;
            let lhs = reader.u16()?;
            let rhs = reader.u16()?;
            match opcode {
                40 => Instruction::AddI64 { dst, lhs, rhs },
                41 => Instruction::SubI64 { dst, lhs, rhs },
                42 => Instruction::MulI64 { dst, lhs, rhs },
                43 => Instruction::DivI64 { dst, lhs, rhs },
                44 => Instruction::Div { dst, lhs, rhs },
                45 => Instruction::AddF32 { dst, lhs, rhs },
                46 => Instruction::SubF32 { dst, lhs, rhs },
                47 => Instruction::MulF32 { dst, lhs, rhs },
                48 => Instruction::DivF32 { dst, lhs, rhs },
                49 => Instruction::AddF64 { dst, lhs, rhs },
                50 => Instruction::SubF64 { dst, lhs, rhs },
                51 => Instruction::MulF64 { dst, lhs, rhs },
                52 => Instruction::DivF64 { dst, lhs, rhs },
                _ => unreachable!(),
            }
        }
        opcode @ 101..=104 => {
            let dst = reader.u16()?;
            let lhs = reader.u16()?;
            let rhs = reader.u16()?;
            match opcode {
                101 => Instruction::RemI32 { dst, lhs, rhs },
                102 => Instruction::RemI64 { dst, lhs, rhs },
                103 => Instruction::RemF32 { dst, lhs, rhs },
                104 => Instruction::RemF64 { dst, lhs, rhs },
                _ => unreachable!(),
            }
        }
        53 => Instruction::LoadString {
            dst: reader.u16()?,
            string: reader.u32()?,
        },
        opcode @ (54..=55 | 59 | 89..=94 | 99) => {
            let dst = reader.u16()?;
            let source = reader.u16()?;
            match opcode {
                54 => Instruction::StringLen { dst, source },
                55 => Instruction::StringByteLen { dst, source },
                59 => Instruction::StringHash { dst, source },
                89 => Instruction::I32ToString { dst, source },
                90 => Instruction::I64ToString { dst, source },
                91 => Instruction::F32ToString { dst, source },
                92 => Instruction::F64ToString { dst, source },
                93 => Instruction::BoolToString { dst, source },
                94 => Instruction::RuneToString { dst, source },
                99 => Instruction::StringToString { dst, source },
                _ => unreachable!(),
            }
        }
        opcode @ 56..=58 => {
            let dst = reader.u16()?;
            let lhs = reader.u16()?;
            let rhs = reader.u16()?;
            match opcode {
                56 => Instruction::StringEqual { dst, lhs, rhs },
                57 => Instruction::StringConcat { dst, lhs, rhs },
                58 => Instruction::StringRuneAt {
                    dst,
                    source: lhs,
                    index: rhs,
                },
                _ => unreachable!(),
            }
        }
        109 => Instruction::StringBuild {
            dst: reader.u16()?,
            parts_base: reader.u16()?,
            parts_count: reader.u16()?,
        },
        100 => Instruction::StandardIntrinsic {
            intrinsic: decode_standard_intrinsic(reader)?,
            args_base: reader.u16()?,
            args_count: reader.u16()?,
            dst: reader.u16()?,
        },
        60 => Instruction::StructNew {
            type_id: StableId(reader.u64()?),
            fields_base: reader.u16()?,
            fields_count: reader.u16()?,
            dst: reader.u16()?,
        },
        61 => Instruction::StructGet {
            source: reader.u16()?,
            field: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        62 => Instruction::StructWith {
            source: reader.u16()?,
            field: StableId(reader.u64()?),
            value: reader.u16()?,
            dst: reader.u16()?,
        },
        63 => Instruction::StructEqual {
            lhs: reader.u16()?,
            rhs: reader.u16()?,
            dst: reader.u16()?,
        },
        64 => Instruction::ClassNew {
            type_id: StableId(reader.u64()?),
            fields_base: reader.u16()?,
            fields_count: reader.u16()?,
            dst: reader.u16()?,
        },
        65 => Instruction::ClassGet {
            source: reader.u16()?,
            field: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        66 => Instruction::ClassSet {
            source: reader.u16()?,
            field: StableId(reader.u64()?),
            value: reader.u16()?,
        },
        67 => Instruction::ClassEqual {
            lhs: reader.u16()?,
            rhs: reader.u16()?,
            dst: reader.u16()?,
        },
        68 => Instruction::ArrayNew {
            type_id: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        69 => Instruction::ArrayLen {
            source: reader.u16()?,
            dst: reader.u16()?,
        },
        70 => Instruction::ArrayGet {
            source: reader.u16()?,
            index: reader.u16()?,
            dst: reader.u16()?,
        },
        107 => Instruction::ArrayFieldGet {
            source: reader.u16()?,
            index: reader.u16()?,
            field: reader.u16()?,
            dst: reader.u16()?,
        },
        108 => Instruction::ArrayPushRow {
            source: reader.u16()?,
            fields_base: reader.u16()?,
            fields_count: reader.u16()?,
        },
        71 => Instruction::ArraySet {
            source: reader.u16()?,
            index: reader.u16()?,
            value: reader.u16()?,
        },
        72 => Instruction::ArrayPush {
            source: reader.u16()?,
            value: reader.u16()?,
        },
        73 => Instruction::ArrayPop {
            source: reader.u16()?,
            dst: reader.u16()?,
        },
        74 => Instruction::ArrayInsert {
            source: reader.u16()?,
            index: reader.u16()?,
            value: reader.u16()?,
        },
        75 => Instruction::ArrayRemove {
            source: reader.u16()?,
            index: reader.u16()?,
            dst: reader.u16()?,
        },
        76 => Instruction::ArrayClear {
            source: reader.u16()?,
        },
        77 => Instruction::MapNew {
            type_id: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        78 => Instruction::MapLen {
            source: reader.u16()?,
            dst: reader.u16()?,
        },
        79 => Instruction::MapGet {
            source: reader.u16()?,
            key: reader.u16()?,
            result_type: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        80 => Instruction::MapSet {
            source: reader.u16()?,
            key: reader.u16()?,
            value: reader.u16()?,
        },
        81 => Instruction::MapRemove {
            source: reader.u16()?,
            key: reader.u16()?,
            result_type: StableId(reader.u64()?),
            dst: reader.u16()?,
        },
        82 => Instruction::MapContains {
            source: reader.u16()?,
            key: reader.u16()?,
            dst: reader.u16()?,
        },
        83 => Instruction::MapClear {
            source: reader.u16()?,
        },
        84 => Instruction::BufferLen {
            source: reader.u16()?,
            dst: reader.u16()?,
        },
        85 => Instruction::BufferGet {
            source: reader.u16()?,
            index: reader.u16()?,
            dst: reader.u16()?,
        },
        86 => Instruction::BufferSet {
            source: reader.u16()?,
            index: reader.u16()?,
            value: reader.u16()?,
        },
        87 => Instruction::BufferSlice {
            source: reader.u16()?,
            start: reader.u16()?,
            length: reader.u16()?,
            dst: reader.u16()?,
        },
        88 => Instruction::BufferCopy {
            destination: reader.u16()?,
            source: reader.u16()?,
            source_start: reader.u16()?,
            destination_start: reader.u16()?,
            length: reader.u16()?,
        },
        opcode => return Err(DecodeError::InvalidOpcode(opcode)),
    })
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_optional_id(output: &mut Vec<u8>, value: Option<StableId>) {
    output.push(u8::from(value.is_some()));
    if let Some(value) = value {
        put_u64(output, value.0);
    }
}

fn put_optional_u32(output: &mut Vec<u8>, value: Option<u32>) {
    output.push(u8::from(value.is_some()));
    if let Some(value) = value {
        put_u32(output, value);
    }
}

fn read_optional_id(reader: &mut Reader<'_>) -> Result<Option<StableId>, DecodeError> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(StableId(reader.u64()?))),
        value => Err(DecodeError::InvalidBoolean(value)),
    }
}

fn read_optional_u32(reader: &mut Reader<'_>) -> Result<Option<u32>, DecodeError> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(reader.u32()?)),
        value => Err(DecodeError::InvalidBoolean(value)),
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.cursor)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or(DecodeError::SizeOverflow)?;
        let bytes = self
            .bytes
            .get(self.cursor..end)
            .ok_or(DecodeError::Truncated)?;
        self.cursor = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    TooManyRegisters,
    EmptyFunction,
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BuildError {}

#[derive(Default)]
pub struct ModuleBuilder {
    strings: Vec<String>,
    functions: Vec<Function>,
    state_handle_types: Vec<StateHandleType>,
    array_types: Vec<ArrayType>,
    map_types: Vec<MapType>,
    buffer_types: Vec<BufferType>,
    snapshot_types: Vec<SnapshotType>,
    resource_token_types: Vec<ResourceTokenType>,
    opaque_types: Vec<StableId>,
    enum_types: Vec<EnumType>,
    struct_types: Vec<StructType>,
    class_types: Vec<ClassType>,
    host_imports: Vec<HostImport>,
    exports: Vec<ScriptExport>,
    state_schema: StateSchema,
    host_contract_id: Option<StableId>,
    state_schema_fingerprint: Option<StateSchemaFingerprint>,
    reload_metadata: ReloadMetadata,
    source_map: Vec<SourceMapEntry>,
}

impl ModuleBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            strings: Vec::new(),
            functions: Vec::new(),
            state_handle_types: Vec::new(),
            array_types: Vec::new(),
            map_types: Vec::new(),
            buffer_types: Vec::new(),
            snapshot_types: Vec::new(),
            resource_token_types: Vec::new(),
            opaque_types: Vec::new(),
            enum_types: Vec::new(),
            struct_types: Vec::new(),
            class_types: Vec::new(),
            host_imports: Vec::new(),
            exports: Vec::new(),
            state_schema: StateSchema { types: Vec::new() },
            host_contract_id: None,
            state_schema_fingerprint: None,
            reload_metadata: ReloadMetadata {
                migration_entry: None,
                activation_entry: None,
                state_schema_fingerprint: StateSchemaFingerprint::from_bytes([0; 32]),
                minimum_migration_limits: MigrationLimitRequirements {
                    max_objects: 0,
                    max_fields: 0,
                    max_forwarding_entries: 0,
                    max_state_bytes: 0,
                    max_gc_roots: 0,
                    max_fuel: 0,
                    max_call_depth: 0,
                },
            },
            source_map: Vec::new(),
        }
    }

    pub fn metadata(
        &mut self,
        host_contract_id: StableId,
        state_schema_fingerprint: StateSchemaFingerprint,
    ) -> &mut Self {
        self.host_contract_id = Some(host_contract_id);
        self.state_schema_fingerprint = Some(state_schema_fingerprint);
        self
    }

    pub fn function(&mut self, function: Function) -> u32 {
        let id = u32::try_from(self.functions.len()).expect("module function count exceeds u32");
        self.functions.push(function);
        id
    }

    pub fn string(&mut self, value: impl Into<String>) -> u32 {
        let value = value.into();
        if let Some(index) = self.strings.iter().position(|existing| *existing == value) {
            return u32::try_from(index).expect("module string count exceeds u32");
        }
        let index = u32::try_from(self.strings.len()).expect("module string count exceeds u32");
        self.strings.push(value);
        index
    }

    pub fn host_import(&mut self, import: HostImport) -> u32 {
        let id = u32::try_from(self.host_imports.len()).expect("host import count exceeds u32");
        self.host_imports.push(import);
        id
    }

    pub fn enum_type(&mut self, enum_type: EnumType) -> &mut Self {
        self.enum_types.push(enum_type);
        self
    }

    pub fn struct_type(&mut self, struct_type: StructType) -> &mut Self {
        self.struct_types.push(struct_type);
        self
    }

    pub fn class_type(&mut self, class_type: ClassType) -> &mut Self {
        self.class_types.push(class_type);
        self
    }

    pub fn state_handle_type(&mut self, state_handle_type: StateHandleType) -> &mut Self {
        self.state_handle_types.push(state_handle_type);
        self
    }

    pub fn array_type(&mut self, array_type: ArrayType) -> &mut Self {
        self.array_types.push(array_type);
        self
    }

    pub fn map_type(&mut self, map_type: MapType) -> &mut Self {
        self.map_types.push(map_type);
        self
    }

    pub fn buffer_type(&mut self, buffer_type: BufferType) -> &mut Self {
        self.buffer_types.push(buffer_type);
        self
    }

    pub fn snapshot_type(&mut self, snapshot_type: SnapshotType) -> &mut Self {
        self.snapshot_types.push(snapshot_type);
        self
    }

    pub fn resource_token_type(&mut self, resource_token_type: ResourceTokenType) -> &mut Self {
        self.resource_token_types.push(resource_token_type);
        self
    }

    pub fn opaque_type(&mut self, type_id: StableId) -> &mut Self {
        self.opaque_types.push(type_id);
        self
    }

    pub fn script_export(&mut self, export: ScriptExport) -> &mut Self {
        self.exports.push(export);
        self
    }

    pub fn state_schema(&mut self, schema: StateSchema) -> &mut Self {
        self.state_schema = schema;
        self
    }

    pub fn reload_entries(
        &mut self,
        migration_entry: Option<u32>,
        activation_entry: Option<u32>,
    ) -> &mut Self {
        self.reload_metadata.migration_entry = migration_entry;
        self.reload_metadata.activation_entry = activation_entry;
        self
    }

    pub fn reload_metadata(&mut self, metadata: ReloadMetadata) -> &mut Self {
        self.reload_metadata = metadata;
        self
    }

    pub fn source_map(&mut self, entries: impl IntoIterator<Item = SourceMapEntry>) -> &mut Self {
        self.source_map.extend(entries);
        self
    }

    #[must_use]
    pub fn finish(self) -> Module {
        let computed_state_schema_fingerprint = self.state_schema.fingerprint();
        let mut module = Module {
            strings: self.strings,
            functions: self.functions,
            state_handle_types: self.state_handle_types,
            array_types: self.array_types,
            map_types: self.map_types,
            buffer_types: self.buffer_types,
            snapshot_types: self.snapshot_types,
            resource_token_types: self.resource_token_types,
            opaque_types: self.opaque_types,
            enum_types: self.enum_types,
            struct_types: self.struct_types,
            class_types: self.class_types,
            host_imports: self.host_imports,
            exports: self.exports,
            state_schema: self.state_schema,
            host_contract_id: self.host_contract_id,
            state_schema_fingerprint: self
                .state_schema_fingerprint
                .unwrap_or(computed_state_schema_fingerprint),
            reload_metadata: self.reload_metadata,
            source_map: self.source_map,
        };
        let migration_entries = module
            .functions
            .iter()
            .enumerate()
            .filter(|(_, function)| function.effect == FunctionEffect::Migration)
            .map(|(index, _)| u32::try_from(index).expect("function count exceeds u32"))
            .collect::<Vec<_>>();
        if module.reload_metadata.migration_entry.is_none() && migration_entries.len() == 1 {
            module.reload_metadata.migration_entry = migration_entries.first().copied();
        }
        let activation_entries = module
            .functions
            .iter()
            .enumerate()
            .filter(|(_, function)| function.effect == FunctionEffect::Immediate)
            .map(|(index, _)| u32::try_from(index).expect("function count exceeds u32"))
            .collect::<Vec<_>>();
        if module.reload_metadata.activation_entry.is_none()
            && migration_entries.len() == 1
            && activation_entries.len() == 1
        {
            module.reload_metadata.activation_entry = activation_entries.first().copied();
        }
        if module.reload_metadata.state_schema_fingerprint == StateSchemaFingerprint::default() {
            module.reload_metadata.state_schema_fingerprint = computed_state_schema_fingerprint;
        }
        if module.reload_metadata.minimum_migration_limits == MigrationLimitRequirements::default()
        {
            module.reload_metadata.minimum_migration_limits =
                minimum_migration_limits(&module, module.reload_metadata.migration_entry);
        }
        module
    }
}

pub struct FunctionBuilder {
    signature: Signature,
    parameter_slots: u16,
    registers: u16,
    frame_bytes: u32,
    root_bitmap: Vec<bool>,
    loop_bounds: Vec<LoopBound>,
    effect: FunctionEffect,
    code: Vec<Instruction>,
}

impl FunctionBuilder {
    #[must_use]
    pub fn new(signature: Signature, registers: u16) -> Self {
        let parameter_slots = u16::try_from(signature.parameters.len()).unwrap_or(u16::MAX);
        Self {
            signature,
            parameter_slots,
            registers,
            frame_bytes: u32::from(registers) * 8,
            root_bitmap: vec![false; usize::from(registers)],
            loop_bounds: Vec::new(),
            effect: FunctionEffect::Ordinary,
            code: Vec::new(),
        }
    }

    pub fn effect(&mut self, effect: FunctionEffect) -> &mut Self {
        self.effect = effect;
        self
    }

    /// Declares the physical parameter-prefix width derived from the
    /// module's authoritative layout table.
    pub fn parameter_slots(&mut self, parameter_slots: u16) -> &mut Self {
        self.parameter_slots = parameter_slots;
        self
    }

    #[must_use]
    pub fn position(&self) -> u32 {
        u32::try_from(self.code.len()).expect("function instruction count exceeds u32")
    }

    pub fn emit(&mut self, instruction: Instruction) -> &mut Self {
        self.code.push(instruction);
        self
    }

    pub fn set_root(&mut self, register: u16) -> Result<&mut Self, BuildError> {
        let root = self
            .root_bitmap
            .get_mut(usize::from(register))
            .ok_or(BuildError::TooManyRegisters)?;
        *root = true;
        Ok(self)
    }

    pub fn loop_bound(&mut self, back_edge: u32, max_iterations: u32) -> &mut Self {
        self.loop_bounds.push(LoopBound {
            back_edge,
            max_iterations,
        });
        self
    }

    #[allow(clippy::too_many_lines)]
    pub fn finish(self) -> Result<Function, BuildError> {
        if self.code.is_empty() {
            return Err(BuildError::EmptyFunction);
        }
        let safepoints = self
            .code
            .iter()
            .enumerate()
            .filter_map(|(pc, instruction)| {
                let pc = u32::try_from(pc).ok()?;
                let explicit = matches!(
                    instruction,
                    Instruction::Safepoint
                        | Instruction::Yield
                        | Instruction::LoadString { .. }
                        | Instruction::StringLen { .. }
                        | Instruction::StringEqual { .. }
                        | Instruction::StringConcat { .. }
                        | Instruction::StringBuild { .. }
                        | Instruction::StringRuneAt { .. }
                        | Instruction::StringHash { .. }
                        | Instruction::I32ToString { .. }
                        | Instruction::I64ToString { .. }
                        | Instruction::F32ToString { .. }
                        | Instruction::F64ToString { .. }
                        | Instruction::BoolToString { .. }
                        | Instruction::RuneToString { .. }
                        | Instruction::StringToString { .. }
                        | Instruction::StandardIntrinsic { .. }
                        | Instruction::EnumNew { .. }
                        | Instruction::EnumEqual { .. }
                        | Instruction::StructNew { .. }
                        | Instruction::StructWith { .. }
                        | Instruction::StructEqual { .. }
                        | Instruction::ClassNew { .. }
                        | Instruction::Call { .. }
                        | Instruction::HostCall { .. }
                        | Instruction::StateCurrentGet { .. }
                        | Instruction::StateHandleResolve { .. }
                        | Instruction::ArrayNew { .. }
                        | Instruction::ArrayLen { .. }
                        | Instruction::ArrayGet { .. }
                        | Instruction::ArrayFieldGet { .. }
                        | Instruction::ArraySet { .. }
                        | Instruction::ArrayPush { .. }
                        | Instruction::ArrayPushRow { .. }
                        | Instruction::ArrayPop { .. }
                        | Instruction::ArrayInsert { .. }
                        | Instruction::ArrayRemove { .. }
                        | Instruction::ArrayClear { .. }
                        | Instruction::MapNew { .. }
                        | Instruction::MapLen { .. }
                        | Instruction::MapGet { .. }
                        | Instruction::MapSet { .. }
                        | Instruction::MapRemove { .. }
                        | Instruction::MapContains { .. }
                        | Instruction::MapClear { .. }
                        | Instruction::BufferLen { .. }
                        | Instruction::BufferGet { .. }
                        | Instruction::BufferSet { .. }
                        | Instruction::BufferSlice { .. }
                        | Instruction::BufferCopy { .. }
                        | Instruction::Return { .. }
                        | Instruction::ReturnVoid
                        | Instruction::Trap
                        | Instruction::CleanupReturn
                );
                let back_edge = matches!(
                    instruction,
                    Instruction::Jump { target } if *target <= pc
                ) || matches!(
                    instruction,
                    Instruction::JumpIfFalse { target, .. } if *target <= pc
                );
                (pc == 0 || explicit || back_edge).then_some(pc)
            })
            .chain(
                self.code
                    .iter()
                    .enumerate()
                    .filter_map(|(pc, instruction)| {
                        (matches!(instruction, Instruction::HostCall { .. })
                            && pc + 1 < self.code.len())
                        .then(|| u32::try_from(pc + 1).ok())
                        .flatten()
                    }),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let root_maps = safepoints
            .iter()
            .map(|pc| RootMap {
                pc: *pc,
                bitmap: self.root_bitmap.clone(),
            })
            .collect();
        Ok(Function {
            signature: self.signature,
            parameter_slots: self.parameter_slots,
            registers: self.registers,
            frame_bytes: self.frame_bytes,
            root_bitmap: self.root_bitmap,
            root_maps,
            safepoints,
            loop_bounds: self.loop_bounds,
            effect: self.effect,
            max_static_call_depth: 1,
            code: self.code,
        })
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::{FileId, SourceSpan, StableId};

    use super::{
        ArrayType, BufferType, ClassType, DecodeError, DecodeLimits, EnumType, EnumVariant,
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, MapType, Module,
        ModuleBuilder, ResourceTokenType, ScriptExport, SectionKind, Signature, SnapshotType,
        SourceMapEntry, StandardIntrinsic, StateField, StateHandleType, StateSchema, StateType,
        StructField, StructType, ValueType, minimum_migration_limits, option_type, result_type,
        state_handle_error_type, state_handle_type,
    };

    #[test]
    fn builder_positions_are_instruction_boundaries() {
        let mut builder = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        assert_eq!(builder.position(), 0);
        builder.emit(Instruction::LoadI32 { dst: 0, value: 7 });
        assert_eq!(builder.position(), 1);
        builder.emit(Instruction::Return { source: 0 });
        assert_eq!(builder.finish().unwrap().code.len(), 2);
    }

    #[test]
    fn wire_format_round_trips_source_maps_and_rejects_corrupt_section_bytes() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::DeferPush {
                function: 0,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::Return { source: 0 });
        function.loop_bound(99, 3);
        let mut builder = ModuleBuilder::new();
        builder.function(function.finish().unwrap());
        builder.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: SourceSpan::new(FileId(7), 4, 9),
        }]);
        let module = builder.finish();
        let encoded = module.encode();
        assert_eq!(u16::from_le_bytes([encoded[6], encoded[7]]), 16);
        assert_eq!(
            (0..16)
                .map(|index| {
                    u16::from_le_bytes([encoded[8 + index * 20], encoded[8 + index * 20 + 1]])
                })
                .collect::<Vec<_>>(),
            SectionKind::ALL
                .into_iter()
                .map(|kind| kind as u16)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            module.source_span(0, 0),
            Some(SourceSpan::new(FileId(7), 4, 9))
        );
        assert_eq!(Module::decode(&encoded), Ok(module));
        assert_eq!(
            Module::decode(&encoded[..encoded.len() - 1]),
            Err(DecodeError::LengthOverflow)
        );
        let mut corrupt = encoded;
        let opcode = corrupt.len() - 3;
        corrupt[opcode] = u8::MAX;
        assert_eq!(
            Module::decode(&corrupt),
            Err(DecodeError::ChecksumMismatch(
                SectionKind::ReloadMetadata as u16
            ))
        );
    }

    #[test]
    fn section_directory_rejects_every_structural_corruption_class() {
        const ENTRY_BYTES: usize = 20;
        const FIRST: usize = 8;
        let encoded = Module::default().encode();
        let write_u16 = |bytes: &mut [u8], offset: usize, value: u16| {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        };
        let write_u32 = |bytes: &mut [u8], offset: usize, value: u32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };

        let mut offset_overflow = encoded.clone();
        write_u32(&mut offset_overflow, FIRST + 4, u32::MAX);
        assert_eq!(
            Module::decode(&offset_overflow),
            Err(DecodeError::OffsetOverflow)
        );

        let mut length_overflow = encoded.clone();
        write_u32(&mut length_overflow, FIRST + 8, u32::MAX);
        assert_eq!(
            Module::decode(&length_overflow),
            Err(DecodeError::LengthOverflow)
        );

        let first_offset = u32::from_le_bytes(encoded[FIRST + 4..FIRST + 8].try_into().unwrap());
        let mut overlap = encoded.clone();
        write_u32(&mut overlap, FIRST + ENTRY_BYTES + 4, first_offset);
        assert_eq!(Module::decode(&overlap), Err(DecodeError::SectionOverlap));

        let mut duplicate = encoded.clone();
        write_u16(
            &mut duplicate,
            FIRST + ENTRY_BYTES,
            SectionKind::Strings as u16,
        );
        assert_eq!(
            Module::decode(&duplicate),
            Err(DecodeError::DuplicateRequiredSection(
                SectionKind::Strings as u16
            ))
        );

        let mut unknown_mandatory = encoded.clone();
        write_u16(&mut unknown_mandatory, FIRST, 999);
        assert_eq!(
            Module::decode(&unknown_mandatory),
            Err(DecodeError::UnknownMandatorySection(999))
        );

        let mut count_mismatch = encoded.clone();
        write_u32(&mut count_mismatch, FIRST + 12, 1);
        assert_eq!(
            Module::decode(&count_mismatch),
            Err(DecodeError::CountMismatch(SectionKind::Strings as u16))
        );

        let mut checksum_mismatch = encoded.clone();
        write_u32(&mut checksum_mismatch, FIRST + 16, 0);
        assert_eq!(
            Module::decode(&checksum_mismatch),
            Err(DecodeError::ChecksumMismatch(SectionKind::Strings as u16))
        );

        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(Module::decode(&trailing), Err(DecodeError::TrailingBytes));
    }

    fn canonical_duplicate_section_fixture() -> Module {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        function.set_root(0).unwrap();
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::ReturnVoid)
            .loop_bound(0, 3);
        let mut builder = ModuleBuilder::new();
        builder.function(function.finish().unwrap());
        builder.finish()
    }

    fn mutate_section_payload(
        encoded: &[u8],
        kind: SectionKind,
        relative_offset: usize,
        replacement: &[u8],
    ) -> Vec<u8> {
        const DIRECTORY_START: usize = 8;
        const DIRECTORY_ENTRY_BYTES: usize = 20;
        const CHECKSUM_OFFSET: usize = 16;

        let mut mutated = encoded.to_vec();
        let entry = Module::inspect_section_directory(encoded, DecodeLimits::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == kind as u16)
            .unwrap();
        let section_start = entry.offset as usize;
        let section_end = section_start + entry.length as usize;
        let mutation_start = section_start + relative_offset;
        let mutation_end = mutation_start + replacement.len();
        mutated[mutation_start..mutation_end].copy_from_slice(replacement);

        let directory_index = SectionKind::ALL
            .iter()
            .position(|candidate| *candidate == kind)
            .unwrap();
        let checksum_offset =
            DIRECTORY_START + directory_index * DIRECTORY_ENTRY_BYTES + CHECKSUM_OFFSET;
        let checksum = super::checksum(&mutated[section_start..section_end]);
        mutated[checksum_offset..checksum_offset + 4].copy_from_slice(&checksum.to_le_bytes());
        mutated
    }

    fn replace_section_payload(encoded: &[u8], kind: SectionKind, replacement: &[u8]) -> Vec<u8> {
        let directory =
            Module::inspect_section_directory(encoded, DecodeLimits::default()).unwrap();
        let sections = SectionKind::ALL
            .into_iter()
            .map(|candidate| {
                let entry = directory
                    .iter()
                    .find(|entry| entry.kind == candidate as u16)
                    .unwrap();
                let start = entry.offset as usize;
                let end = start + entry.length as usize;
                (
                    candidate,
                    if candidate == kind {
                        replacement.to_vec()
                    } else {
                        encoded[start..end].to_vec()
                    },
                )
            })
            .collect::<Vec<_>>();
        super::encode_sections(&sections)
    }

    #[test]
    fn constants_section_is_the_canonical_four_byte_zero_count() {
        let encoded = Module::default().encode();
        assert_eq!(Module::decode(&encoded), Ok(Module::default()));

        let nonzero =
            replace_section_payload(&encoded, SectionKind::Constants, &1_u32.to_le_bytes());
        assert_eq!(
            Module::decode(&nonzero),
            Err(DecodeError::InconsistentSection(
                SectionKind::Constants as u16
            ))
        );

        let trailing = replace_section_payload(&encoded, SectionKind::Constants, &[0, 0, 0, 0, 0]);
        assert_eq!(
            Module::decode(&trailing),
            Err(DecodeError::InconsistentSection(
                SectionKind::Constants as u16
            ))
        );
    }

    #[test]
    fn code_section_must_match_functions_instruction_streams() {
        let encoded = canonical_duplicate_section_fixture().encode();
        let mutated = mutate_section_payload(&encoded, SectionKind::Code, 11, &8_i32.to_le_bytes());
        assert_eq!(
            Module::decode(&mutated),
            Err(DecodeError::InconsistentSection(SectionKind::Code as u16))
        );
    }

    #[test]
    fn root_maps_section_must_match_functions_root_metadata() {
        let encoded = canonical_duplicate_section_fixture().encode();
        let mutated = mutate_section_payload(&encoded, SectionKind::RootMaps, 6, &[0]);
        assert_eq!(
            Module::decode(&mutated),
            Err(DecodeError::InconsistentSection(
                SectionKind::RootMaps as u16
            ))
        );
    }

    #[test]
    fn safepoints_section_must_match_functions_safepoints() {
        let encoded = canonical_duplicate_section_fixture().encode();
        let mutated = mutate_section_payload(
            &encoded,
            SectionKind::Safepoints,
            8,
            &u32::MAX.to_le_bytes(),
        );
        assert_eq!(
            Module::decode(&mutated),
            Err(DecodeError::InconsistentSection(
                SectionKind::Safepoints as u16
            ))
        );
    }

    #[test]
    fn loop_bounds_section_must_match_functions_loop_bounds() {
        let encoded = canonical_duplicate_section_fixture().encode();
        let mutated =
            mutate_section_payload(&encoded, SectionKind::LoopBounds, 12, &4_u32.to_le_bytes());
        assert_eq!(
            Module::decode(&mutated),
            Err(DecodeError::InconsistentSection(
                SectionKind::LoopBounds as u16
            ))
        );
    }

    #[test]
    #[allow(
        clippy::items_after_statements,
        clippy::too_many_lines,
        clippy::type_complexity
    )]
    fn every_decode_resource_class_has_an_independent_limit() {
        let type_id = StableId::from_name("Limited");
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        function
            .emit(Instruction::Safepoint)
            .emit(Instruction::Trap);
        let mut builder = ModuleBuilder::new();
        builder
            .enum_type(EnumType {
                type_id,
                variants: vec![EnumVariant {
                    stable_id: StableId::from_name("Limited::One"),
                    tag: 0,
                    payload_type: Some(ValueType::I32),
                }],
            })
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: type_id,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: StableId::from_name("Limited::value"),
                        ty: ValueType::I32,
                    }],
                }],
            })
            .source_map([SourceMapEntry {
                function: 0,
                pc_start: 0,
                pc_end: 1,
                span: SourceSpan::new(FileId(1), 0, 1),
            }]);
        builder.function(function.finish().unwrap());
        let encoded = builder.finish().encode();

        let cases: &[(DecodeLimits, &'static str)] = &[
            (
                DecodeLimits {
                    max_enum_variants: 0,
                    ..DecodeLimits::default()
                },
                "enum variants",
            ),
            (
                DecodeLimits {
                    max_fields: 0,
                    ..DecodeLimits::default()
                },
                "fields",
            ),
            (
                DecodeLimits {
                    max_root_map_bytes: 0,
                    ..DecodeLimits::default()
                },
                "root map bytes",
            ),
            (
                DecodeLimits {
                    max_safepoints: 0,
                    ..DecodeLimits::default()
                },
                "safepoints",
            ),
            (
                DecodeLimits {
                    max_reload_metadata_bytes: 0,
                    ..DecodeLimits::default()
                },
                "reload metadata bytes",
            ),
        ];
        for (limits, name) in cases {
            assert_eq!(
                Module::decode_with_limits(&encoded, *limits),
                Err(DecodeError::ResourceLimit(name))
            );
        }

        fn section_with_count(bytes: &[u8], kind: SectionKind, count: u32) -> Vec<u8> {
            const ENTRY_BYTES: usize = 20;
            let mut mutated = bytes.to_vec();
            let section_index = SectionKind::ALL
                .iter()
                .position(|candidate| *candidate == kind)
                .unwrap();
            let entry = 8 + section_index * ENTRY_BYTES;
            let offset =
                u32::from_le_bytes(mutated[entry + 4..entry + 8].try_into().unwrap()) as usize;
            let length =
                u32::from_le_bytes(mutated[entry + 8..entry + 12].try_into().unwrap()) as usize;
            mutated[offset..offset + 4].copy_from_slice(&count.to_le_bytes());
            mutated[entry + 12..entry + 16].copy_from_slice(&count.to_le_bytes());
            let checksum = super::checksum(&mutated[offset..offset + length]);
            mutated[entry + 16..entry + 20].copy_from_slice(&checksum.to_le_bytes());
            mutated
        }

        let empty_section_limits: [(SectionKind, fn(&mut DecodeLimits), &'static str); 4] = [
            (
                SectionKind::Strings,
                |limits: &mut DecodeLimits| limits.max_strings = 0,
                "strings",
            ),
            (
                SectionKind::Types,
                |limits: &mut DecodeLimits| limits.max_types = 0,
                "types",
            ),
            (
                SectionKind::Structs,
                |limits: &mut DecodeLimits| limits.max_structs = 0,
                "structs",
            ),
            (
                SectionKind::Classes,
                |limits: &mut DecodeLimits| limits.max_classes = 0,
                "classes",
            ),
        ];
        for (kind, configure, name) in empty_section_limits {
            let mutated = section_with_count(&encoded, kind, 1);
            let mut limits = DecodeLimits::default();
            configure(&mut limits);
            assert_eq!(
                Module::decode_with_limits(&mutated, limits),
                Err(DecodeError::ResourceLimit(name))
            );
        }
    }

    #[test]
    fn reload_metadata_round_trips_with_inferred_migration_requirements() {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);
        let mut builder = ModuleBuilder::new();
        builder.function(migration.finish().unwrap());
        let module = builder.finish();

        assert_eq!(module.reload_metadata.migration_entry, Some(0));
        assert_eq!(module.reload_metadata.minimum_migration_limits.max_fuel, 2);
        assert_eq!(
            module
                .reload_metadata
                .minimum_migration_limits
                .max_call_depth,
            1
        );
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn migration_requirements_follow_direct_and_nested_defer_stack_edges() {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::DeferPush {
                function: 1,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);

        let mut direct_cleanup = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        direct_cleanup
            .emit(Instruction::DeferPush {
                function: 2,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::ReturnVoid);

        let mut nested_cleanup = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        nested_cleanup.emit(Instruction::ReturnVoid);

        let mut builder = ModuleBuilder::new();
        builder.function(migration.finish().unwrap());
        builder.function(direct_cleanup.finish().unwrap());
        builder.function(nested_cleanup.finish().unwrap());
        let module = builder.finish();

        assert_eq!(
            module
                .reload_metadata
                .minimum_migration_limits
                .max_call_depth,
            3
        );
        assert_eq!(minimum_migration_limits(&module, Some(1)).max_call_depth, 2);
        assert_eq!(minimum_migration_limits(&module, Some(2)).max_call_depth, 1);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn state_current_get_round_trips_in_bytecode_v7() {
        let stable_id = StableId::from_name("repl::environment");
        let type_id = StableId::from_name("repl::Environment");
        let signature = Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(type_id)),
        };
        let mut function = FunctionBuilder::new(signature.clone(), 1);
        function
            .effect(FunctionEffect::Ordinary)
            .set_root(0)
            .unwrap()
            .emit(Instruction::StateCurrentGet {
                stable_id,
                type_id,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut builder = ModuleBuilder::new();
        builder.state_schema(StateSchema {
            types: vec![StateType {
                stable_id: type_id,
                version: 1,
                fields: Vec::new(),
            }],
        });
        let function = builder.function(function.finish().unwrap());
        builder.script_export(ScriptExport {
            stable_id: StableId::from_name("repl::cell_0"),
            function,
            signature,
            effect: FunctionEffect::Ordinary,
        });
        let module = builder.finish();
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn state_handle_opcodes_round_trip_in_bytecode_v7() {
        let target = ValueType::Named(nexa_core::StableId::from_name("EnemyBrain"));
        let result = result_type(target, ValueType::Named(state_handle_error_type().type_id));
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::Named(state_handle_type(target)),
                    ValueType::Named(state_handle_type(target)),
                ],
                result: Some(ValueType::I32),
            },
            8,
        );
        function
            .emit(Instruction::StateHandleResolve {
                handle: 0,
                target,
                result_type: result.type_id,
                dst: 2,
            })
            .emit(Instruction::StateHandleIsAlive {
                handle: 0,
                target,
                dst: 3,
            })
            .emit(Instruction::StateHandleStableId {
                handle: 0,
                target,
                dst: 4,
            })
            .emit(Instruction::StateHandleGeneration {
                handle: 0,
                target,
                dst: 5,
            })
            .emit(Instruction::StateHandleEqual {
                lhs: 0,
                rhs: 1,
                target,
                dst: 6,
            })
            .emit(Instruction::StateHandleHash {
                handle: 0,
                target,
                dst: 7,
            })
            .emit(Instruction::Return { source: 7 });
        let mut builder = ModuleBuilder::new();
        builder
            .state_handle_type(StateHandleType::new(target))
            .enum_type(state_handle_error_type())
            .enum_type(result)
            .function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(
            module.state_handle_types,
            vec![StateHandleType::new(target)]
        );
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn array_metadata_and_opcodes_round_trip_in_bytecode_v7() {
        let array = ArrayType::new(ValueType::I32);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::Named(array.type_id),
                    ValueType::I32,
                    ValueType::I32,
                ],
                result: Some(ValueType::I32),
            },
            6,
        );
        function
            .emit(Instruction::ArrayNew {
                type_id: array.type_id,
                dst: 3,
            })
            .emit(Instruction::ArrayLen { source: 0, dst: 5 })
            .emit(Instruction::ArrayGet {
                source: 0,
                index: 1,
                dst: 4,
            })
            .emit(Instruction::ArraySet {
                source: 0,
                index: 1,
                value: 2,
            })
            .emit(Instruction::ArrayPush {
                source: 0,
                value: 2,
            })
            .emit(Instruction::ArrayPop { source: 0, dst: 4 })
            .emit(Instruction::ArrayInsert {
                source: 0,
                index: 1,
                value: 2,
            })
            .emit(Instruction::ArrayRemove {
                source: 0,
                index: 1,
                dst: 4,
            })
            .emit(Instruction::ArrayClear { source: 0 })
            .emit(Instruction::Return { source: 5 });
        let mut builder = ModuleBuilder::new();
        builder
            .array_type(array)
            .function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(module.array_types, vec![array]);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn map_metadata_and_opcodes_round_trip_in_bytecode_v7() {
        let map = MapType::new(ValueType::I32, ValueType::String);
        let option = option_type(ValueType::String);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::Named(map.type_id),
                    ValueType::I32,
                    ValueType::String,
                ],
                result: Some(ValueType::I32),
            },
            7,
        );
        function
            .emit(Instruction::MapNew {
                type_id: map.type_id,
                dst: 3,
            })
            .emit(Instruction::MapLen { source: 0, dst: 4 })
            .emit(Instruction::MapGet {
                source: 0,
                key: 1,
                result_type: option.type_id,
                dst: 5,
            })
            .emit(Instruction::MapSet {
                source: 0,
                key: 1,
                value: 2,
            })
            .emit(Instruction::MapRemove {
                source: 0,
                key: 1,
                result_type: option.type_id,
                dst: 5,
            })
            .emit(Instruction::MapContains {
                source: 0,
                key: 1,
                dst: 6,
            })
            .emit(Instruction::MapClear { source: 0 })
            .emit(Instruction::Return { source: 4 });
        let mut builder = ModuleBuilder::new();
        builder
            .map_type(map)
            .enum_type(option)
            .function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(module.map_types, vec![map]);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn buffer_metadata_and_copy_opcodes_round_trip_in_bytecode_v7() {
        let buffer = BufferType::new(ValueType::I32);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::Named(buffer.type_id),
                    ValueType::Named(buffer.type_id),
                    ValueType::I32,
                ],
                result: Some(ValueType::I32),
            },
            7,
        );
        function
            .emit(Instruction::BufferLen { source: 0, dst: 3 })
            .emit(Instruction::BufferGet {
                source: 0,
                index: 2,
                dst: 4,
            })
            .emit(Instruction::BufferSet {
                source: 0,
                index: 2,
                value: 4,
            })
            .emit(Instruction::BufferSlice {
                source: 0,
                start: 2,
                length: 2,
                dst: 5,
            })
            .emit(Instruction::BufferCopy {
                destination: 0,
                source: 1,
                source_start: 2,
                destination_start: 2,
                length: 2,
            })
            .emit(Instruction::Return { source: 3 });
        let mut builder = ModuleBuilder::new();
        builder
            .buffer_type(buffer)
            .function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(module.buffer_types, vec![buffer]);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn typed_snapshot_metadata_round_trips_in_bytecode_v7() {
        let content_type = StableId::from_name("EnemyView");
        let snapshot = SnapshotType::new(content_type);
        let mut builder = ModuleBuilder::new();
        builder
            .struct_type(StructType {
                type_id: content_type,
                fields: Vec::new(),
            })
            .snapshot_type(snapshot);
        let module = builder.finish();
        assert_eq!(module.snapshot_types, vec![snapshot]);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn typed_resource_token_metadata_round_trips_in_bytecode_v7() {
        let action_lock = StableId::from_name("ActionLock");
        let motion_lock = StableId::from_name("MotionLock");
        let action_token = ResourceTokenType::new(action_lock);
        let motion_token = ResourceTokenType::new(motion_lock);
        assert_ne!(action_token.type_id, motion_token.type_id);

        let mut builder = ModuleBuilder::new();
        builder
            .resource_token_type(action_token)
            .resource_token_type(motion_token);
        let module = builder.finish();
        assert_eq!(
            module.resource_token_types,
            vec![action_token, motion_token]
        );
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn host_import_authority_metadata_round_trips_in_bytecode_v7() {
        let import = HostImport {
            stable_id: StableId::from_name("Host::read_profile"),
            declaration_fingerprint: [0xa5; 32],
            capabilities: vec!["profile.read".into(), "world-state_read".into()],
            parameters: vec![ValueType::I32],
            result: Some(ValueType::String),
            mode: HostCallMode::Immediate,
            fuel_cost: 7,
            async_result: None,
        };
        let mut builder = ModuleBuilder::new();
        builder.host_import(import.clone());
        let module = builder.finish();
        assert_eq!(module.host_imports, vec![import]);
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scalar_types_and_opcodes_round_trip_in_bytecode_v7() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::I32,
                    ValueType::I64,
                    ValueType::F32,
                    ValueType::F64,
                    ValueType::Rune,
                ],
                result: Some(ValueType::F64),
            },
            10,
        );
        function
            .emit(Instruction::LoadI64 {
                dst: 5,
                value: i64::MIN,
            })
            .emit(Instruction::LoadF32 {
                dst: 6,
                bits: f32::NAN.to_bits(),
            })
            .emit(Instruction::LoadF64 {
                dst: 7,
                bits: f64::INFINITY.to_bits(),
            })
            .emit(Instruction::LoadRune {
                dst: 8,
                value: '界' as u32,
            })
            .emit(Instruction::Div {
                dst: 0,
                lhs: 0,
                rhs: 0,
            })
            .emit(Instruction::RemI32 {
                dst: 0,
                lhs: 0,
                rhs: 0,
            })
            .emit(Instruction::AddI64 {
                dst: 1,
                lhs: 1,
                rhs: 5,
            })
            .emit(Instruction::SubI64 {
                dst: 1,
                lhs: 1,
                rhs: 5,
            })
            .emit(Instruction::MulI64 {
                dst: 1,
                lhs: 1,
                rhs: 5,
            })
            .emit(Instruction::DivI64 {
                dst: 1,
                lhs: 1,
                rhs: 5,
            })
            .emit(Instruction::RemI64 {
                dst: 1,
                lhs: 1,
                rhs: 5,
            })
            .emit(Instruction::AddF32 {
                dst: 2,
                lhs: 2,
                rhs: 6,
            })
            .emit(Instruction::SubF32 {
                dst: 2,
                lhs: 2,
                rhs: 6,
            })
            .emit(Instruction::MulF32 {
                dst: 2,
                lhs: 2,
                rhs: 6,
            })
            .emit(Instruction::DivF32 {
                dst: 2,
                lhs: 2,
                rhs: 6,
            })
            .emit(Instruction::RemF32 {
                dst: 2,
                lhs: 2,
                rhs: 6,
            })
            .emit(Instruction::AddF64 {
                dst: 3,
                lhs: 3,
                rhs: 7,
            })
            .emit(Instruction::SubF64 {
                dst: 3,
                lhs: 3,
                rhs: 7,
            })
            .emit(Instruction::MulF64 {
                dst: 3,
                lhs: 3,
                rhs: 7,
            })
            .emit(Instruction::DivF64 {
                dst: 9,
                lhs: 3,
                rhs: 7,
            })
            .emit(Instruction::RemF64 {
                dst: 9,
                lhs: 3,
                rhs: 7,
            })
            .emit(Instruction::Return { source: 9 });
        let mut builder = ModuleBuilder::new();
        builder.function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn scalar_to_string_opcodes_round_trip_in_bytecode_v7() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::I32,
                    ValueType::I64,
                    ValueType::F32,
                    ValueType::F64,
                    ValueType::Bool,
                    ValueType::Rune,
                    ValueType::String,
                ],
                result: Some(ValueType::String),
            },
            14,
        );
        function
            .emit(Instruction::CompareLtI32 {
                dst: 7,
                lhs: 0,
                rhs: 0,
            })
            .emit(Instruction::CompareLtI64 {
                dst: 7,
                lhs: 1,
                rhs: 1,
            })
            .emit(Instruction::CompareLtF32 {
                dst: 7,
                lhs: 2,
                rhs: 2,
            })
            .emit(Instruction::CompareLtF64 {
                dst: 7,
                lhs: 3,
                rhs: 3,
            })
            .emit(Instruction::I32ToString { dst: 7, source: 0 })
            .emit(Instruction::I64ToString { dst: 8, source: 1 })
            .emit(Instruction::F32ToString { dst: 9, source: 2 })
            .emit(Instruction::F64ToString { dst: 10, source: 3 })
            .emit(Instruction::BoolToString { dst: 11, source: 4 })
            .emit(Instruction::RuneToString { dst: 12, source: 5 })
            .emit(Instruction::StringToString { dst: 13, source: 6 })
            .emit(Instruction::Return { source: 13 });
        let mut builder = ModuleBuilder::new();
        builder.function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn bytecode_v7_rejects_an_older_header() {
        let module = ModuleBuilder::new().finish();
        let mut bytes = module.encode();
        bytes[4..6].copy_from_slice(&5_u16.to_le_bytes());
        assert_eq!(
            Module::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(5))
        );
    }

    #[test]
    fn utf8_string_pool_and_operations_round_trip_in_bytecode_v7() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I64),
            },
            8,
        );
        function
            .emit(Instruction::LoadString { dst: 0, string: 0 })
            .emit(Instruction::LoadString { dst: 1, string: 1 })
            .emit(Instruction::StringLen { dst: 2, source: 0 })
            .emit(Instruction::StringByteLen { dst: 3, source: 0 })
            .emit(Instruction::StringEqual {
                dst: 4,
                lhs: 0,
                rhs: 1,
            })
            .emit(Instruction::StringConcat {
                dst: 5,
                lhs: 0,
                rhs: 1,
            })
            .emit(Instruction::StringBuild {
                dst: 5,
                parts_base: 0,
                parts_count: 2,
            })
            .emit(Instruction::StringRuneAt {
                dst: 6,
                source: 5,
                index: 2,
            })
            .emit(Instruction::StringHash { dst: 7, source: 5 })
            .emit(Instruction::Return { source: 7 });
        let mut builder = ModuleBuilder::new();
        assert_eq!(builder.string("a界"), 0);
        assert_eq!(builder.string("!"), 1);
        assert_eq!(builder.string("a界"), 0);
        builder.function(function.finish().unwrap());
        let module = builder.finish();
        let encoded = module.encode();
        assert_eq!(Module::decode(&encoded), Ok(module));
        assert_eq!(
            Module::decode_with_limits(
                &encoded,
                DecodeLimits {
                    max_strings: 1,
                    ..DecodeLimits::default()
                }
            ),
            Err(DecodeError::ResourceLimit("strings"))
        );

        let mut invalid_utf8 = encoded.clone();
        let entry = Module::inspect_section_directory(&encoded, DecodeLimits::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == SectionKind::Strings as u16)
            .unwrap();
        let offset = entry.offset as usize;
        let length = entry.length as usize;
        invalid_utf8[offset + 8] = 0xff;
        let checksum = super::checksum(&invalid_utf8[offset..offset + length]);
        invalid_utf8[24..28].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(Module::decode(&invalid_utf8), Err(DecodeError::InvalidUtf8));
    }

    #[test]
    fn struct_metadata_and_opcodes_round_trip_in_bytecode_v7() {
        let type_id = nexa_core::StableId::from_name("Position");
        let x = nexa_core::StableId::from_parts(&["Position", "::x"]);
        let fields = vec![
            StructField {
                stable_id: x,
                ty: ValueType::I32,
            },
            StructField {
                stable_id: nexa_core::StableId::from_parts(&["Position", "::label"]),
                ty: ValueType::String,
            },
        ];
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32, ValueType::String],
                result: Some(ValueType::Bool),
            },
            7,
        );
        function
            .emit(Instruction::StructNew {
                type_id,
                fields_base: 0,
                fields_count: 2,
                dst: 2,
            })
            .emit(Instruction::StructGet {
                source: 2,
                field: x,
                dst: 3,
            })
            .emit(Instruction::StructWith {
                source: 2,
                field: x,
                value: 3,
                dst: 4,
            })
            .emit(Instruction::StructEqual {
                lhs: 2,
                rhs: 4,
                dst: 5,
            })
            .emit(Instruction::Return { source: 5 });
        let mut builder = ModuleBuilder::new();
        builder
            .struct_type(StructType { type_id, fields })
            .function(function.finish().unwrap());
        let module = builder.finish();
        let encoded = module.encode();
        assert_eq!(Module::decode(&encoded), Ok(module));
        assert_eq!(
            Module::decode_with_limits(
                &encoded,
                DecodeLimits {
                    max_structs: 0,
                    ..DecodeLimits::default()
                }
            ),
            Err(DecodeError::ResourceLimit("structs"))
        );
    }

    #[test]
    fn class_metadata_and_mutation_opcodes_round_trip_in_bytecode_v7() {
        let type_id = nexa_core::StableId::from_name("Node");
        let value = nexa_core::StableId::from_parts(&["Node", "::value"]);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::I32],
                result: Some(ValueType::Bool),
            },
            6,
        );
        function
            .emit(Instruction::ClassNew {
                type_id,
                fields_base: 0,
                fields_count: 1,
                dst: 1,
            })
            .emit(Instruction::ClassGet {
                source: 1,
                field: value,
                dst: 2,
            })
            .emit(Instruction::ClassSet {
                source: 1,
                field: value,
                value: 2,
            })
            .emit(Instruction::ClassEqual {
                lhs: 1,
                rhs: 1,
                dst: 3,
            })
            .emit(Instruction::Return { source: 3 });
        let mut builder = ModuleBuilder::new();
        builder
            .class_type(ClassType {
                type_id,
                fields: vec![StructField {
                    stable_id: value,
                    ty: ValueType::I32,
                }],
            })
            .function(function.finish().unwrap());
        let module = builder.finish();
        let encoded = module.encode();
        assert_eq!(Module::decode(&encoded), Ok(module));
        assert_eq!(
            Module::decode_with_limits(
                &encoded,
                DecodeLimits {
                    max_classes: 0,
                    ..DecodeLimits::default()
                }
            ),
            Err(DecodeError::ResourceLimit("classes"))
        );
    }

    #[test]
    fn every_standard_intrinsic_round_trips_in_bytecode_v7() {
        let value = ValueType::I32;
        let key = ValueType::String;
        let intrinsics = vec![
            StandardIntrinsic::OptionIsSome { value },
            StandardIntrinsic::OptionIsNone { value },
            StandardIntrinsic::ResultIsOk {
                success: value,
                error: ValueType::String,
            },
            StandardIntrinsic::ResultIsErr {
                success: value,
                error: ValueType::String,
            },
            StandardIntrinsic::OptionUnwrapOr { value },
            StandardIntrinsic::ResultUnwrapOr {
                success: value,
                error: ValueType::String,
            },
            StandardIntrinsic::F32Floor,
            StandardIntrinsic::F64Floor,
            StandardIntrinsic::F32Ceil,
            StandardIntrinsic::F64Ceil,
            StandardIntrinsic::F32Round,
            StandardIntrinsic::F64Round,
            StandardIntrinsic::F32Sqrt,
            StandardIntrinsic::F64Sqrt,
            StandardIntrinsic::F32Sin,
            StandardIntrinsic::F64Sin,
            StandardIntrinsic::F32Cos,
            StandardIntrinsic::F64Cos,
            StandardIntrinsic::StringContains,
            StandardIntrinsic::StringStartsWith,
            StandardIntrinsic::StringEndsWith,
            StandardIntrinsic::StringLen,
            StandardIntrinsic::StringByteLen,
            StandardIntrinsic::StringSubstring,
            StandardIntrinsic::StringTrim,
            StandardIntrinsic::StringSplit,
            StandardIntrinsic::ArrayLen { element: value },
            StandardIntrinsic::ArrayIsEmpty { element: value },
            StandardIntrinsic::ArrayGet { element: value },
            StandardIntrinsic::ArrayPush { element: value },
            StandardIntrinsic::ArrayPop { element: value },
            StandardIntrinsic::ArrayReserve { element: value },
            StandardIntrinsic::ArrayCapacity { element: value },
            StandardIntrinsic::ArrayClear { element: value },
            StandardIntrinsic::ArrayShrinkToFit { element: value },
            StandardIntrinsic::MapLen { key, value },
            StandardIntrinsic::MapContains { key, value },
            StandardIntrinsic::MapGet { key, value },
            StandardIntrinsic::MapInsert { key, value },
            StandardIntrinsic::MapRemove { key, value },
            StandardIntrinsic::DebugAssert,
            StandardIntrinsic::DebugTrap,
        ];
        assert_eq!(intrinsics.len(), StandardIntrinsic::WIRE_VARIANT_COUNT);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            4,
        );
        for intrinsic in intrinsics {
            function.emit(Instruction::StandardIntrinsic {
                intrinsic,
                args_base: 0,
                args_count: intrinsic.argument_count(),
                dst: 3,
            });
        }
        function.emit(Instruction::ReturnVoid);
        let mut builder = ModuleBuilder::new();
        builder.function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }

    #[test]
    fn state_schema_fingerprint_is_256_bit_type_order_independent_and_field_order_sensitive() {
        let nested_core = nexa_core::canonical_array_type_id(nexa_core::CanonicalValueType::Named(
            nexa_core::canonical_option_type_id(nexa_core::CanonicalValueType::I32),
        ));
        let nested_bytecode =
            super::array_type(ValueType::Named(super::option_type(ValueType::I32).type_id));
        assert_eq!(nested_core, nested_bytecode);

        let type_a = StateType {
            stable_id: StableId::from_name("A"),
            version: 1,
            fields: vec![
                StateField {
                    stable_id: StableId::from_name("A::z"),
                    ty: ValueType::String,
                },
                StateField {
                    stable_id: StableId::from_name("A::a"),
                    ty: ValueType::I32,
                },
            ],
        };
        let type_b = StateType {
            stable_id: StableId::from_name("B"),
            version: 2,
            fields: vec![StateField {
                stable_id: StableId::from_name("B::value"),
                ty: ValueType::Bool,
            }],
        };
        let first = StateSchema {
            types: vec![type_a.clone(), type_b.clone()],
        }
        .fingerprint();
        let type_reordered = StateSchema {
            types: vec![type_b.clone(), type_a.clone()],
        }
        .fingerprint();
        assert_eq!(first, type_reordered);
        assert_eq!(first.as_bytes().len(), 32);

        let mut fields_reordered = type_a.clone();
        fields_reordered.fields.reverse();
        assert_ne!(
            first,
            StateSchema {
                types: vec![fields_reordered, type_b.clone()]
            }
            .fingerprint()
        );

        let mut version_changed = type_a;
        version_changed.version = 2;
        assert_ne!(
            first,
            StateSchema {
                types: vec![version_changed, type_b]
            }
            .fingerprint()
        );

        let nested_state = StateSchema {
            types: vec![StateType {
                stable_id: StableId::from_name("Nested"),
                version: 1,
                fields: vec![StateField {
                    stable_id: StableId::from_name("Nested::values"),
                    ty: ValueType::Named(nested_bytecode),
                }],
            }],
        };
        let canonical = nexa_core::CanonicalStateSchema {
            types: vec![nexa_core::CanonicalStateType {
                stable_id: StableId::from_name("Nested"),
                version: 1,
                fields: vec![nexa_core::CanonicalStateField {
                    stable_id: StableId::from_name("Nested::values"),
                    ty: nexa_core::CanonicalValueType::Named(nested_core),
                }],
            }],
        };
        assert_eq!(nested_state.fingerprint(), canonical.fingerprint());
    }
}
