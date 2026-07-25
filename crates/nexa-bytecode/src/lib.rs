//! Versioned, safe-to-construct Nexa bytecode representation.

use std::fmt;

use nexa_core::StableId;

pub const MAGIC: [u8; 4] = *b"NXBC";
pub const BYTECODE_VERSION: u16 = 2;

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
pub enum HostCallMode {
    Immediate,
    Async,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostImport {
    pub stable_id: StableId,
    pub parameters: Vec<ValueType>,
    pub result: Option<ValueType>,
    pub mode: HostCallMode,
    pub fuel_cost: u32,
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
    StateHandleRemap {
        old_id: StableId,
        target: u16,
    },
    StateDelete {
        stable_id: StableId,
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
    pub host_imports: Vec<HostImport>,
    pub exports: Vec<ScriptExport>,
    pub state_schema: StateSchema,
    pub host_interface_hash: Option<StableId>,
    pub schema_hash: Option<StableId>,
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
    ChecksumMismatch(u8),
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
    pub max_exports: usize,
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
            max_exports: 65_536,
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
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        put_optional_id(&mut output, self.host_interface_hash);
        put_optional_id(&mut output, self.schema_hash);
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
        }
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
        let metadata = output;
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
        encode_sections(&[(1, metadata), (2, output)])
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
        let metadata = sections
            .iter()
            .find_map(|(kind, bytes)| (*kind == 1).then_some(*bytes))
            .ok_or(DecodeError::InvalidSectionDirectory)?;
        let function_bytes = sections
            .iter()
            .find_map(|(kind, bytes)| (*kind == 2).then_some(*bytes))
            .ok_or(DecodeError::InvalidSectionDirectory)?;
        let mut reader = Reader {
            bytes: metadata,
            cursor: 0,
        };
        let host_interface_hash = read_optional_id(&mut reader)?;
        let schema_hash = read_optional_id(&mut reader)?;
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
            host_imports.push(HostImport {
                stable_id,
                parameters,
                result,
                mode,
                fuel_cost,
            });
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
        Ok(Self {
            functions,
            host_imports,
            exports,
            state_schema: StateSchema { types: state_types },
            host_interface_hash,
            schema_hash,
        })
    }
}

fn encode_sections(sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    const DIRECTORY_ENTRY_BYTES: usize = 13;
    let header_bytes = 8_usize
        .checked_add(sections.len().saturating_mul(DIRECTORY_ENTRY_BYTES))
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
        output.push(*kind);
        put_u32(
            &mut output,
            u32::try_from(offset).expect("section offset exceeds wire format"),
        );
        put_u32(
            &mut output,
            u32::try_from(bytes.len()).expect("section length exceeds wire format"),
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

fn decode_sections(bytes: &[u8], max_sections: usize) -> Result<Vec<(u8, &[u8])>, DecodeError> {
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
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        entries.push((reader.u8()?, reader.u32()?, reader.u32()?, reader.u32()?));
    }
    let directory_end = reader.cursor;
    entries.sort_by_key(|(_, offset, _, _)| *offset);
    let mut previous_end = directory_end;
    let mut kinds = 0_u32;
    let mut sections = Vec::with_capacity(count);
    for (kind, offset, length, expected_checksum) in entries {
        let bit = 1_u32
            .checked_shl(u32::from(kind))
            .ok_or(DecodeError::InvalidSectionDirectory)?;
        if kinds & bit != 0 {
            return Err(DecodeError::InvalidSectionDirectory);
        }
        kinds |= bit;
        let start = usize::try_from(offset).map_err(|_| DecodeError::SizeOverflow)?;
        let end = start
            .checked_add(usize::try_from(length).map_err(|_| DecodeError::SizeOverflow)?)
            .ok_or(DecodeError::SizeOverflow)?;
        if start < previous_end || end > bytes.len() {
            return Err(DecodeError::InvalidSectionDirectory);
        }
        let section = &bytes[start..end];
        if checksum(section) != expected_checksum {
            return Err(DecodeError::ChecksumMismatch(kind));
        }
        sections.push((kind, section));
        previous_end = end;
    }
    if previous_end != bytes.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(sections)
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
        Instruction::StateHandleRemap { old_id, target } => {
            output.push(22);
            put_u64(output, old_id.0);
            put_u16(output, target);
        }
        Instruction::StateDelete { stable_id } => {
            output.push(23);
            put_u64(output, stable_id.0);
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
        22 => Instruction::StateHandleRemap {
            old_id: StableId(reader.u64()?),
            target: reader.u16()?,
        },
        23 => Instruction::StateDelete {
            stable_id: StableId(reader.u64()?),
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

fn read_optional_id(reader: &mut Reader<'_>) -> Result<Option<StableId>, DecodeError> {
    match reader.u8()? {
        0 => Ok(None),
        1 => Ok(Some(StableId(reader.u64()?))),
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
    host_imports: Vec<HostImport>,
    exports: Vec<ScriptExport>,
    state_schema: StateSchema,
    host_interface_hash: Option<StableId>,
    schema_hash: Option<StableId>,
}

impl ModuleBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            functions: Vec::new(),
            host_imports: Vec::new(),
            exports: Vec::new(),
            state_schema: StateSchema { types: Vec::new() },
            host_interface_hash: None,
            schema_hash: None,
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

    pub fn script_export(&mut self, export: ScriptExport) -> &mut Self {
        self.exports.push(export);
        self
    }

    pub fn state_schema(&mut self, schema: StateSchema) -> &mut Self {
        self.state_schema = schema;
        self
    }

    #[must_use]
    pub fn finish(self) -> Module {
        Module {
            functions: self.functions,
            host_imports: self.host_imports,
            exports: self.exports,
            state_schema: self.state_schema,
            host_interface_hash: self.host_interface_hash,
            schema_hash: self.schema_hash,
        }
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
    use super::{
        DecodeError, FunctionBuilder, Instruction, Module, ModuleBuilder, Signature, ValueType,
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
    fn wire_format_round_trips_and_rejects_corrupt_instruction_bytes() {
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
        let module = builder.finish();
        let encoded = module.encode();
        assert_eq!(Module::decode(&encoded), Ok(module));
        assert_eq!(
            Module::decode(&encoded[..encoded.len() - 1]),
            Err(DecodeError::InvalidSectionDirectory)
        );
        let mut corrupt = encoded;
        let opcode = corrupt.len() - 3;
        corrupt[opcode] = u8::MAX;
        assert_eq!(
            Module::decode(&corrupt),
            Err(DecodeError::ChecksumMismatch(2))
        );
    }
}
