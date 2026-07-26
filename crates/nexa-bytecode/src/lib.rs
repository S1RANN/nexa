//! Versioned, safe-to-construct Nexa bytecode representation.

use std::collections::BTreeSet;
use std::fmt;

use nexa_core::{FileId, SourceSpan, StableId};

pub const MAGIC: [u8; 4] = *b"NXBC";
pub const BYTECODE_VERSION: u16 = 4;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    I32,
    Bool,
    Ref,
    Named(StableId),
}

impl ValueType {
    #[must_use]
    pub const fn is_reference(self) -> bool {
        matches!(self, Self::Ref | Self::Named(_))
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
    let mut canonical = String::from(name);
    canonical.push('<');
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            canonical.push(',');
        }
        match argument {
            ValueType::I32 => canonical.push_str("i32"),
            ValueType::Bool => canonical.push_str("bool"),
            ValueType::Ref => canonical.push_str("ref"),
            ValueType::Named(id) => {
                use std::fmt::Write;
                write!(canonical, "named:{:016x}", id.0).expect("String writes do not fail");
            }
        }
    }
    canonical.push('>');
    StableId::from_name(&canonical)
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
    pub fn stable_hash(&self) -> StableId {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        hash_u64(
            &mut hash,
            u64::try_from(self.types.len()).unwrap_or(u64::MAX),
        );
        for state_type in &self.types {
            hash_u64(&mut hash, state_type.stable_id.0);
            hash_u64(&mut hash, u64::from(state_type.version));
            hash_u64(
                &mut hash,
                u64::try_from(state_type.fields.len()).unwrap_or(u64::MAX),
            );
            for field in &state_type.fields {
                hash_u64(&mut hash, field.stable_id.0);
                hash_value_type(&mut hash, field.ty);
            }
        }
        StableId(hash)
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
    pub stateful_schema_hash: StableId,
    pub minimum_migration_limits: MigrationLimitRequirements,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptExport {
    pub stable_id: StableId,
    pub function: u32,
    pub signature: Signature,
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
    CompareEq {
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
    pub functions: Vec<Function>,
    pub enum_types: Vec<EnumType>,
    pub host_imports: Vec<HostImport>,
    pub exports: Vec<ScriptExport>,
    pub state_schema: StateSchema,
    pub host_interface_hash: Option<StableId>,
    pub schema_hash: Option<StableId>,
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
            if let Instruction::Call { function, .. } = instruction
                && let Ok(callee) = usize::try_from(*function)
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
        if let Instruction::Call {
            function: callee, ..
        } = instruction
            && let Ok(callee) = usize::try_from(*callee)
        {
            depth = depth.max(migration_call_depth(module, callee, visiting).saturating_add(1));
        }
    }
    visiting.pop();
    depth
}

fn hash_u64(hash: &mut u64, value: u64) {
    for byte in value.to_le_bytes() {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn hash_value_type(hash: &mut u64, value: ValueType) {
    match value {
        ValueType::I32 => hash_u64(hash, 0),
        ValueType::Bool => hash_u64(hash, 1),
        ValueType::Ref => hash_u64(hash, 2),
        ValueType::Named(stable_id) => {
            hash_u64(hash, 3);
            hash_u64(hash, stable_id.0);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    InvalidMagic,
    UnsupportedVersion(u16),
    InvalidType(u8),
    InvalidOpcode(u8),
    InvalidBoolean(u8),
    TrailingBytes,
    SizeOverflow,
    InvalidSectionDirectory,
    OffsetOverflow,
    LengthOverflow,
    SectionOverlap,
    DuplicateRequiredSection(u16),
    UnknownMandatorySection(u16),
    CountMismatch(u16),
    InvalidSourceMap,
    ChecksumMismatch(u16),
    ResourceLimit(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_bytes: usize,
    pub max_sections: usize,
    pub max_functions: usize,
    pub max_instructions: usize,
    pub max_registers: usize,
    pub max_root_maps: usize,
    pub max_loop_bounds: usize,
    pub max_host_imports: usize,
    pub max_state_types: usize,
    pub max_enum_types: usize,
    pub max_exports: usize,
    pub max_source_map_entries: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            max_sections: 16,
            max_functions: 65_536,
            max_instructions: 1_000_000,
            max_registers: u16::MAX as usize,
            max_root_maps: 1_000_000,
            max_loop_bounds: 1_000_000,
            max_host_imports: 65_536,
            max_state_types: 65_536,
            max_enum_types: 65_536,
            max_exports: 65_536,
            max_source_map_entries: 1_000_000,
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
        put_optional_id(&mut output, self.host_interface_hash);
        put_optional_id(&mut output, self.schema_hash);
        put_optional_u32(&mut output, self.reload_metadata.migration_entry);
        put_optional_u32(&mut output, self.reload_metadata.activation_entry);
        put_u64(&mut output, self.reload_metadata.stateful_schema_hash.0);
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
        let empty = || {
            let mut section = Vec::new();
            put_u32(&mut section, 0);
            section
        };
        encode_sections(&[
            (SectionKind::Strings, empty()),
            (SectionKind::Types, empty()),
            (SectionKind::Constants, empty()),
            (SectionKind::Enums, enums),
            (SectionKind::Structs, empty()),
            (SectionKind::Classes, empty()),
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
        let mut metadata = Vec::new();
        metadata.extend_from_slice(
            required_section(&sections, SectionKind::ReloadMetadata)?
                .get(4..)
                .ok_or(DecodeError::Truncated)?,
        );
        metadata.extend_from_slice(required_section(&sections, SectionKind::HostImports)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Enums)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::StateSchemas)?);
        metadata.extend_from_slice(required_section(&sections, SectionKind::Exports)?);
        let function_bytes = required_section(&sections, SectionKind::Functions)?;
        let source_map_bytes = required_section(&sections, SectionKind::SourceMap)?;
        let mut reader = Reader {
            bytes: &metadata,
            cursor: 0,
        };
        let host_interface_hash = read_optional_id(&mut reader)?;
        let schema_hash = read_optional_id(&mut reader)?;
        let migration_entry = read_optional_u32(&mut reader)?;
        let activation_entry = read_optional_u32(&mut reader)?;
        let stateful_schema_hash = StableId(reader.u64()?);
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
            stateful_schema_hash,
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
        for _ in 0..enum_type_count {
            let type_id = StableId(reader.u64()?);
            let variant_count = usize::from(reader.u16()?);
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
            if root_map_count > limits.max_root_maps {
                return Err(DecodeError::ResourceLimit("root maps"));
            }
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
            let mut safepoints = Vec::with_capacity(safepoint_count);
            for _ in 0..safepoint_count {
                safepoints.push(reader.u32()?);
            }
            let loop_bound_count =
                usize::try_from(reader.u32()?).map_err(|_| DecodeError::SizeOverflow)?;
            if loop_bound_count > limits.max_loop_bounds {
                return Err(DecodeError::ResourceLimit("loop bounds"));
            }
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
            functions,
            enum_types,
            host_imports,
            exports,
            state_schema: StateSchema { types: state_types },
            host_interface_hash,
            schema_hash,
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
                    .expect("every v4 section starts with a count")
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
    }
}

fn decode_type(reader: &mut Reader<'_>) -> Result<ValueType, DecodeError> {
    match reader.u8()? {
        0 => Ok(ValueType::I32),
        1 => Ok(ValueType::Bool),
        2 => Ok(ValueType::Ref),
        3 => Ok(ValueType::Named(StableId(reader.u64()?))),
        value => Err(DecodeError::InvalidType(value)),
    }
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
        Instruction::Move { dst, source } => {
            output.push(2);
            put_u16(output, dst);
            put_u16(output, source);
        }
        Instruction::Add { dst, lhs, rhs }
        | Instruction::Sub { dst, lhs, rhs }
        | Instruction::Mul { dst, lhs, rhs }
        | Instruction::CompareEq { dst, lhs, rhs } => {
            output.push(match instruction {
                Instruction::Add { .. } => 3,
                Instruction::Sub { .. } => 4,
                Instruction::Mul { .. } => 5,
                Instruction::CompareEq { .. } => 6,
                _ => unreachable!(),
            });
            put_u16(output, dst);
            put_u16(output, lhs);
            put_u16(output, rhs);
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
        opcode @ 3..=6 => {
            let dst = reader.u16()?;
            let lhs = reader.u16()?;
            let rhs = reader.u16()?;
            match opcode {
                3 => Instruction::Add { dst, lhs, rhs },
                4 => Instruction::Sub { dst, lhs, rhs },
                5 => Instruction::Mul { dst, lhs, rhs },
                6 => Instruction::CompareEq { dst, lhs, rhs },
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
    functions: Vec<Function>,
    enum_types: Vec<EnumType>,
    host_imports: Vec<HostImport>,
    exports: Vec<ScriptExport>,
    state_schema: StateSchema,
    host_interface_hash: Option<StableId>,
    schema_hash: Option<StableId>,
    reload_metadata: ReloadMetadata,
    source_map: Vec<SourceMapEntry>,
}

impl ModuleBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            functions: Vec::new(),
            enum_types: Vec::new(),
            host_imports: Vec::new(),
            exports: Vec::new(),
            state_schema: StateSchema { types: Vec::new() },
            host_interface_hash: None,
            schema_hash: None,
            reload_metadata: ReloadMetadata {
                migration_entry: None,
                activation_entry: None,
                stateful_schema_hash: StableId(0),
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

    pub fn metadata(&mut self, host_interface_hash: StableId, schema_hash: StableId) -> &mut Self {
        self.host_interface_hash = Some(host_interface_hash);
        self.schema_hash = Some(schema_hash);
        self
    }

    pub fn function(&mut self, function: Function) -> u32 {
        let id = u32::try_from(self.functions.len()).expect("module function count exceeds u32");
        self.functions.push(function);
        id
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
        let mut module = Module {
            functions: self.functions,
            enum_types: self.enum_types,
            host_imports: self.host_imports,
            exports: self.exports,
            state_schema: self.state_schema,
            host_interface_hash: self.host_interface_hash,
            schema_hash: self.schema_hash,
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
        if module.reload_metadata.stateful_schema_hash == StableId(0) {
            module.reload_metadata.stateful_schema_hash = module.state_schema.stable_hash();
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
        Self {
            signature,
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
                        | Instruction::Call { .. }
                        | Instruction::HostCall { .. }
                        | Instruction::StateHandleResolve { .. }
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
    use nexa_core::{FileId, SourceSpan};

    use super::{
        DecodeError, FunctionBuilder, FunctionEffect, Instruction, Module, ModuleBuilder,
        SectionKind, Signature, SourceMapEntry, ValueType, result_type, state_handle_error_type,
        state_handle_type,
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
    fn state_handle_opcodes_round_trip_in_bytecode_v4() {
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
            .enum_type(state_handle_error_type())
            .enum_type(result)
            .function(function.finish().unwrap());
        let module = builder.finish();
        assert_eq!(Module::decode(&module.encode()), Ok(module));
    }
}
