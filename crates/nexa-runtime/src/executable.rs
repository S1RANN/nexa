//! M5 stage-F `ExecutableModule` v1 (F1 slice): the formal load path
//! `Verified Bytecode -> ExecutableModuleBuilder -> ExecutableModule`.
//!
//! Each row predecodes the per-instruction static data the hot loop
//! recomputes today: the load-time attempt fuel (via the single source of
//! truth `static_instruction_fuel`, so rows and the portable interpreter
//! cannot diverge), the folded host-import surcharge, and the safepoint
//! flag. Operand-dependent surcharges stay dynamic and are marked as such.
//!
//! Portable bytecode remains the cache and safety boundary: nothing here is
//! ever serialized. Later F slices add dense identity resolution, hot/cold
//! metadata separation, and the interpreter switch-over.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexa_bytecode::{Function, HostCallMode, Instruction};
use nexa_core::{SourceSpan, StableId};
use nexa_verifier::{NominalIndexShape, ResolvedNominalOperand, VerifiedModule};

use crate::interpreter::{OpcodeCostTable, static_instruction_fuel};

static NEXT_STRING_POOL_ID: AtomicU64 = AtomicU64::new(1);
const STATIC_LEAF_MAX_INSTRUCTIONS: usize = 24;

/// Dense verifier result used by executable rows.
///
/// The portable verifier form also carries a Struct `StableId`, which is
/// useful while building cold allocation provenance but redundant during
/// execution. Allocation rows retain only dense type/layout slots, Struct
/// operations need only the proven field slot, and Class operations need the
/// proven type and field slots. Dropping cold identities keeps this value in
/// a compact dense layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExecutableNominalOperand {
    #[default]
    None,
    EnumVariant {
        type_index: u16,
        variant_index: u16,
    },
    StructField {
        index: u16,
    },
    ClassField {
        type_index: u16,
        index: u16,
    },
    ArrayType {
        type_index: u16,
        row_fields: u8,
    },
    MapType {
        type_index: u16,
    },
}

impl From<ResolvedNominalOperand> for ExecutableNominalOperand {
    fn from(resolved: ResolvedNominalOperand) -> Self {
        match resolved {
            ResolvedNominalOperand::None => Self::None,
            ResolvedNominalOperand::EnumVariant {
                type_index,
                variant_index,
            } => Self::EnumVariant {
                type_index,
                variant_index,
            },
            ResolvedNominalOperand::StructField { index, .. } => Self::StructField { index },
            ResolvedNominalOperand::ClassField { type_index, index } => {
                Self::ClassField { type_index, index }
            }
            ResolvedNominalOperand::ArrayType {
                type_index,
                row_fields,
            } => Self::ArrayType {
                type_index,
                row_fields,
            },
            ResolvedNominalOperand::MapType { type_index } => Self::MapType { type_index },
        }
    }
}

#[derive(Clone, Debug)]
pub struct PooledStringConstant {
    pub value: Arc<str>,
    pub hash: u64,
}

/// One predecoded metadata row parallel to verified bytecode.
///
/// The 48-byte portable `Instruction` remains in the verifier-owned function
/// code instead of being duplicated here. Keeping only dense operands, fuel,
/// and flags makes the hot row small enough for substantially better cache
/// density while preserving portable bytecode as the safety boundary.
#[derive(Clone, Copy, Debug)]
pub struct ExecutableInstruction {
    /// Verifier-proven dense nominal operand for allocation and field rows.
    pub(crate) resolved_nominal: ExecutableNominalOperand,
    /// Full attempt charge for static rows, base opcode charge for dynamic
    /// rows. The dynamic flag is a compact discriminator (unlike
    /// `Option<u64>`, which occupies 16 bytes).
    pub attempt_fuel: u64,
    /// Three execution flags plus the eight-bit profiler code share one word.
    /// This keeps the row at 24 bytes while avoiding a cold metadata read.
    flags: u16,
}

impl ExecutableInstruction {
    const DYNAMIC_FUEL: u16 = 1 << 0;
    const SAFEPOINT: u16 = 1 << 1;
    const FUEL_BOUNDARY: u16 = 1 << 2;
    const PROFILE_SHIFT: u32 = 3;

    #[inline]
    pub(crate) const fn dynamic_fuel(self) -> bool {
        self.flags & Self::DYNAMIC_FUEL != 0
    }

    #[cfg(test)]
    pub(crate) const fn safepoint(self) -> bool {
        self.flags & Self::SAFEPOINT != 0
    }

    #[inline]
    pub(crate) const fn fuel_boundary(self) -> bool {
        self.flags & Self::FUEL_BOUNDARY != 0
    }

    #[inline]
    pub(crate) fn profile_opcode(self) -> usize {
        usize::from(((self.flags >> Self::PROFILE_SHIFT) & 0x7f) as u8)
    }

    #[inline]
    pub(crate) const fn has_profile_event(self) -> bool {
        self.flags & (0x80_u16 << Self::PROFILE_SHIFT) != 0
    }

    fn with_profile_code(mut self, profile_code: u8) -> Self {
        self.flags |= u16::from(profile_code) << Self::PROFILE_SHIFT;
        self
    }
}

/// Cold profiler metadata parallel to one executable instruction row.
///
/// Keeping this out of [`ExecutableInstruction`] preserves dispatch-row cache
/// density when profiling is disabled. Enabled runs resolve allocation
/// provenance and Host identity with one indexed cold-row read, without
/// source-map scans or `StableId` work in the instruction loop.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ExecutableProfileRow {
    pub allocation: Option<(crate::profiler::AllocationKind, StableId)>,
    pub source_span: Option<SourceSpan>,
    pub host_call: Option<(StableId, HostCallMode)>,
}

#[derive(Clone, Debug)]
pub struct ExecutableFunction {
    rows: Vec<ExecutableInstruction>,
    profile_rows: Vec<ExecutableProfileRow>,
    /// Process-local identity of the verifier-owned instruction backing used
    /// to build these rows. It prevents a direct caller from pairing a valid
    /// certificate with a different same-shaped module.
    code_identity: usize,
    /// Load-time proof for the bounded static leaf executor.
    /// `None` keeps the full continuation interpreter as the only path.
    static_leaf: Option<StaticLeafCertificate>,
    /// Load-time partial evaluation for a straight-line leaf returning a
    /// constant i32 while replaying at most one allocation effect.
    constant_leaf: Option<StaticLeafConstantKernel>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StaticLeafCertificate {
    pub fixed_fuel: u64,
    pub array_pushes: u8,
    pub array_push_element_fuel: u64,
    pub buffer_copy: Option<StaticLeafBufferCopy>,
    pub buffer_get: Option<StaticLeafBufferGet>,
    pub buffer_work_fuel: u64,
    /// Exact instruction count for the load-time-proven copy-then-get
    /// buffer kernel. The fused executor preserves this logical charge.
    pub buffer_kernel_instructions: Option<u8>,
    pub map_sets: u8,
    pub map_lookups: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StaticLeafBufferCopy {
    pub destination: u16,
    pub source: u16,
    pub source_start: usize,
    pub destination_start: usize,
    pub length: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StaticLeafBufferGet {
    pub source: u16,
    pub index: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StaticLeafConstantEffect {
    None,
    LoadString {
        string: u32,
    },
    EnumNew {
        type_id: StableId,
        variant: StableId,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StaticLeafConstantKernel {
    pub result: i32,
    pub instructions: u8,
    pub effect: StaticLeafConstantEffect,
}

impl ExecutableFunction {
    fn new(
        function: &nexa_bytecode::Function,
        rows: Vec<ExecutableInstruction>,
        profile_rows: Vec<ExecutableProfileRow>,
        static_leaf: Option<StaticLeafCertificate>,
        constant_leaf: Option<StaticLeafConstantKernel>,
    ) -> Self {
        Self {
            rows,
            profile_rows,
            code_identity: function.code.as_ptr() as usize,
            static_leaf,
            constant_leaf,
        }
    }

    #[must_use]
    pub fn rows(&self) -> &[ExecutableInstruction] {
        &self.rows
    }

    pub(crate) fn profile_rows(&self) -> &[ExecutableProfileRow] {
        &self.profile_rows
    }

    #[must_use]
    pub const fn static_leaf_fuel(&self) -> Option<u64> {
        match self.static_leaf {
            Some(certificate) => Some(certificate.fixed_fuel),
            None => None,
        }
    }

    pub(crate) const fn static_leaf_certificate(&self) -> Option<StaticLeafCertificate> {
        self.static_leaf
    }

    pub(crate) const fn static_leaf_constant_kernel(&self) -> Option<StaticLeafConstantKernel> {
        self.constant_leaf
    }

    pub(crate) const fn code_identity(&self) -> usize {
        self.code_identity
    }
}

/// Build-time self-validation failures: the module never becomes
/// executable when any row is inconsistent with the verified bytecode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutableBuildError {
    CostTableVersionMismatch { supported: u32, table: u32 },
    MissingHostImport { function: u32, pc: u32, import: u32 },
    JumpOutOfFunction { function: u32, pc: u32, target: u32 },
    RootMapOutOfFunction { function: u32, pc: u32 },
    FuelOverflow { function: u32, pc: u32 },
}

impl std::fmt::Display for ExecutableBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CostTableVersionMismatch { supported, table } => write!(
                formatter,
                "cost table version {table} is not the supported version {supported}"
            ),
            Self::MissingHostImport {
                function,
                pc,
                import,
            } => write!(
                formatter,
                "function {function} pc {pc} calls unresolved host import {import}"
            ),
            Self::JumpOutOfFunction {
                function,
                pc,
                target,
            } => write!(
                formatter,
                "function {function} pc {pc} jumps outside the function to {target}"
            ),
            Self::RootMapOutOfFunction { function, pc } => write!(
                formatter,
                "function {function} carries a root map for out-of-range pc {pc}"
            ),
            Self::FuelOverflow { function, pc } => write!(
                formatter,
                "function {function} pc {pc} overflows the static fuel charge"
            ),
        }
    }
}

impl std::error::Error for ExecutableBuildError {}

/// Predecoded execution form of one verified module (F1 slice).
#[derive(Clone, Debug)]
pub struct ExecutableModule {
    functions: Vec<ExecutableFunction>,
    string_pool: Vec<PooledStringConstant>,
    string_pool_id: u64,
    cost_table_version: u32,
}

impl ExecutableModule {
    /// Builds and self-validates the predecoded rows for every function.
    ///
    /// Validation covers what F1 materializes: host-import resolvability
    /// (the folded surcharge must exist), jump-target legality, root-map pc
    /// mapping, and static fuel overflow. Full operand range legality
    /// remains the verifier's admission contract on the input.
    pub fn build(
        module: &VerifiedModule,
        costs: &OpcodeCostTable,
    ) -> Result<Self, ExecutableBuildError> {
        if costs.version != nexa_core::OPCODE_COST_TABLE_VERSION {
            return Err(ExecutableBuildError::CostTableVersionMismatch {
                supported: nexa_core::OPCODE_COST_TABLE_VERSION,
                table: costs.version,
            });
        }
        let nominal_shape = module.nominal_index_shape();
        let bytecode = module.module();
        let string_pool = bytecode
            .strings
            .iter()
            .map(|value| PooledStringConstant {
                value: Arc::<str>::from(value.as_str()),
                hash: crate::heap::fnv_content_hash(value),
            })
            .collect();
        let mut functions = Vec::with_capacity(bytecode.functions.len());
        for function_index in 0..bytecode.functions.len() {
            functions.push(build_executable_function(
                module,
                nominal_shape,
                function_index,
                costs,
            )?);
        }
        Ok(Self {
            functions,
            string_pool,
            string_pool_id: NEXT_STRING_POOL_ID.fetch_add(1, Ordering::Relaxed),
            cost_table_version: costs.version,
        })
    }

    #[must_use]
    pub fn functions(&self) -> &[ExecutableFunction] {
        &self.functions
    }

    #[must_use]
    pub const fn cost_table_version(&self) -> u32 {
        self.cost_table_version
    }

    #[must_use]
    pub fn pooled_string(&self, index: u32) -> Option<(u64, &PooledStringConstant)> {
        self.string_pool
            .get(index as usize)
            .map(|constant| (self.string_pool_id, constant))
    }

    /// Share of instruction rows whose whole charge is settled at load
    /// time (F1 coverage metric for the stage-F profile).
    #[must_use]
    pub fn static_fuel_coverage(&self) -> (usize, usize) {
        let mut static_rows = 0;
        let mut total = 0;
        for function in &self.functions {
            total += function.rows.len();
            static_rows += function
                .rows
                .iter()
                .filter(|row| !row.dynamic_fuel())
                .count();
        }
        (static_rows, total)
    }
}

fn build_executable_function(
    module: &VerifiedModule,
    nominal_shape: NominalIndexShape,
    function_index: usize,
    costs: &OpcodeCostTable,
) -> Result<ExecutableFunction, ExecutableBuildError> {
    let bytecode = module.module();
    let function = &bytecode.functions[function_index];
    let function_id = u32::try_from(function_index).unwrap_or(u32::MAX);
    let code_len = u32::try_from(function.code.len()).unwrap_or(u32::MAX);
    for root_map in &function.root_maps {
        if root_map.pc >= code_len {
            return Err(ExecutableBuildError::RootMapOutOfFunction {
                function: function_id,
                pc: root_map.pc,
            });
        }
    }
    let mut rows = Vec::with_capacity(function.code.len());
    let mut profile_rows = Vec::with_capacity(function.code.len());
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        let pc = u32::try_from(pc).unwrap_or(u32::MAX);
        let resolved = module.resolved_operand(function_id as usize, pc as usize);
        let allocation = crate::interpreter::allocation_profile(instruction, resolved);
        let host_call = if let Instruction::HostCall { import, .. } = instruction {
            bytecode
                .host_imports
                .get(import as usize)
                .map(|host| (host.stable_id, host.mode))
        } else {
            None
        };
        let mut profile_code = u8::try_from(crate::interpreter::opcode_index(instruction))
            .expect("opcode table fits u8");
        if allocation.is_some() || host_call.is_some() {
            profile_code |= 0x80;
        }
        rows.push(
            build_executable_row(
                module,
                nominal_shape,
                function,
                function_id,
                pc,
                instruction,
                costs,
            )?
            .with_profile_code(profile_code),
        );
        profile_rows.push(ExecutableProfileRow {
            allocation,
            source_span: allocation.and_then(|_| bytecode.source_span(function_id, pc)),
            host_call,
        });
    }
    let static_leaf = certify_static_leaf(function, &rows);
    let constant_leaf =
        static_leaf.and_then(|_| certify_static_leaf_constant_kernel(module, function));
    Ok(ExecutableFunction::new(
        function,
        rows,
        profile_rows,
        static_leaf,
        constant_leaf,
    ))
}

fn build_executable_row(
    module: &VerifiedModule,
    nominal_shape: NominalIndexShape,
    function: &Function,
    function_id: u32,
    pc: u32,
    instruction: Instruction,
    costs: &OpcodeCostTable,
) -> Result<ExecutableInstruction, ExecutableBuildError> {
    let bytecode = module.module();
    let code_len = u32::try_from(function.code.len()).unwrap_or(u32::MAX);
    match instruction {
        Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. }
            if target > code_len =>
        {
            return Err(ExecutableBuildError::JumpOutOfFunction {
                function: function_id,
                pc,
                target,
            });
        }
        _ => {}
    }
    let Ok(static_fuel) = static_instruction_fuel(bytecode, nominal_shape, instruction, costs)
    else {
        return Err(ExecutableBuildError::FuelOverflow {
            function: function_id,
            pc,
        });
    };
    // The interpreter charges the host-import surcharge on top of the
    // attempt fuel; the row folds it at build time.
    let static_fuel = if let Instruction::HostCall { import, .. } = instruction {
        let host_import = bytecode.host_imports.get(import as usize).ok_or(
            ExecutableBuildError::MissingHostImport {
                function: function_id,
                pc,
                import,
            },
        )?;
        static_fuel
            .map(|fuel| {
                fuel.checked_add(u64::from(host_import.fuel_cost)).ok_or(
                    ExecutableBuildError::FuelOverflow {
                        function: function_id,
                        pc,
                    },
                )
            })
            .transpose()?
    } else {
        static_fuel
    };
    let safepoint = crate::interpreter::is_safepoint(instruction, pc);
    let fuel_boundary = pc == 0
        || safepoint
        || function
            .code
            .get(pc.saturating_sub(1) as usize)
            .is_some_and(|previous| matches!(previous, Instruction::HostCall { .. }));
    let mut flags = 0;
    if static_fuel.is_none() {
        flags |= ExecutableInstruction::DYNAMIC_FUEL;
    }
    if safepoint {
        flags |= ExecutableInstruction::SAFEPOINT;
    }
    if fuel_boundary {
        flags |= ExecutableInstruction::FUEL_BOUNDARY;
    }
    Ok(ExecutableInstruction {
        resolved_nominal: module
            .resolved_operand(function_id as usize, pc as usize)
            .into(),
        attempt_fuel: static_fuel.unwrap_or_else(|| costs.cost(instruction)),
        flags,
    })
}

struct StaticLeafAnalysis {
    // A class value can also denote state-backed `Opaque` storage, which
    // requires a registry unavailable to this executor. Class provenance
    // therefore records only values created by a local `ClassNew`.
    local_class: [bool; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
    // Identity plus exact length for locally created arrays.
    local_array: [Option<(u8, usize)>; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
    i32_constant: [Option<i32>; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
    // Original argument register for buffers; moves retain the origin so
    // preflight never needs to inspect an uninitialized temporary register.
    buffer_value: [Option<u16>; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
    // Identity for maps allocated inside the leaf. Argument maps are never
    // admitted because their shape cannot be proven at load time.
    local_map: [Option<u8>; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
    next_array: u8,
    next_map: u8,
    array_pushes: u8,
    array_push_element_fuel: u64,
    buffer_copy: Option<StaticLeafBufferCopy>,
    buffer_get: Option<StaticLeafBufferGet>,
    buffer_work_fuel: u64,
    map_sets: u8,
    map_lookups: u8,
    saw_control_flow: bool,
}

impl StaticLeafAnalysis {
    fn new(parameter_count: usize) -> Option<Self> {
        let mut buffer_value = [None; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY];
        for (index, parameter) in buffer_value.iter_mut().take(parameter_count).enumerate() {
            *parameter = Some(u16::try_from(index).ok()?);
        }
        Some(Self {
            local_class: [false; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
            local_array: [None; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
            i32_constant: [None; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
            buffer_value,
            local_map: [None; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY],
            next_array: 0,
            next_map: 0,
            array_pushes: 0,
            array_push_element_fuel: 0,
            buffer_copy: None,
            buffer_get: None,
            buffer_work_fuel: 0,
            map_sets: 0,
            map_lookups: 0,
            saw_control_flow: false,
        })
    }

    fn clear_destination(&mut self, dst: u16) -> Option<()> {
        *self.local_class.get_mut(usize::from(dst))? = false;
        *self.local_array.get_mut(usize::from(dst))? = None;
        *self.i32_constant.get_mut(usize::from(dst))? = None;
        *self.buffer_value.get_mut(usize::from(dst))? = None;
        *self.local_map.get_mut(usize::from(dst))? = None;
        Some(())
    }

    fn observe(&mut self, instruction: Instruction) -> Option<()> {
        // Once paths split, only scalar/enum control-tail operations remain
        // admissible. This deliberately avoids pretending that a linear
        // provenance walk can prove a class/array/map created on just one
        // side of a branch.
        if self.saw_control_flow && !static_leaf_control_tail_instruction(instruction) {
            return None;
        }
        match instruction {
            instruction @ (Instruction::ClassNew { .. }
            | Instruction::ClassGet { .. }
            | Instruction::ClassSet { .. }) => self.observe_class(instruction),
            instruction @ (Instruction::ArrayNew { .. }
            | Instruction::ArrayPush { .. }
            | Instruction::ArraySet { .. }
            | Instruction::ArrayGet { .. }
            | Instruction::ArrayLen { .. }) => self.observe_array(instruction),
            instruction @ (Instruction::BufferCopy { .. } | Instruction::BufferGet { .. }) => {
                self.observe_buffer(instruction)
            }
            instruction @ (Instruction::MapNew { .. }
            | Instruction::MapSet { .. }
            | Instruction::MapGet { .. }) => self.observe_map(instruction),
            Instruction::Move { dst, source } => {
                *self.local_class.get_mut(usize::from(dst))? =
                    *self.local_class.get(usize::from(source))?;
                *self.local_array.get_mut(usize::from(dst))? =
                    *self.local_array.get(usize::from(source))?;
                *self.i32_constant.get_mut(usize::from(dst))? =
                    *self.i32_constant.get(usize::from(source))?;
                *self.buffer_value.get_mut(usize::from(dst))? =
                    *self.buffer_value.get(usize::from(source))?;
                *self.local_map.get_mut(usize::from(dst))? =
                    *self.local_map.get(usize::from(source))?;
                Some(())
            }
            Instruction::LoadI32 { dst, value } => {
                self.clear_destination(dst)?;
                *self.i32_constant.get_mut(usize::from(dst))? = Some(value);
                Some(())
            }
            Instruction::Add { dst, lhs, rhs } => {
                let value = match (
                    *self.i32_constant.get(usize::from(lhs))?,
                    *self.i32_constant.get(usize::from(rhs))?,
                ) {
                    (Some(lhs), Some(rhs)) => Some(lhs.wrapping_add(rhs)),
                    _ => None,
                };
                self.clear_destination(dst)?;
                *self.i32_constant.get_mut(usize::from(dst))? = value;
                Some(())
            }
            Instruction::LoadString { dst, .. }
            | Instruction::StringByteLen { dst, .. }
            | Instruction::EnumNew { dst, .. }
            | Instruction::EnumTag { dst, .. }
            | Instruction::EnumPayload { dst, .. }
            | Instruction::CompareEq { dst, .. } => self.clear_destination(dst),
            Instruction::Jump { .. } | Instruction::JumpIfFalse { .. } => {
                self.saw_control_flow = true;
                Some(())
            }
            Instruction::Return { .. } | Instruction::Trap => Some(()),
            _ => None,
        }
    }

    fn observe_class(&mut self, instruction: Instruction) -> Option<()> {
        match instruction {
            Instruction::ClassNew { dst, .. } => {
                self.clear_destination(dst)?;
                *self.local_class.get_mut(usize::from(dst))? = true;
            }
            Instruction::ClassGet { source, dst, .. } => {
                if !*self.local_class.get(usize::from(source))? {
                    return None;
                }
                self.clear_destination(dst)?;
            }
            Instruction::ClassSet { source, .. } => {
                if !*self.local_class.get(usize::from(source))? {
                    return None;
                }
            }
            _ => unreachable!("class analysis receives only class instructions"),
        }
        Some(())
    }

    fn observe_array(&mut self, instruction: Instruction) -> Option<()> {
        match instruction {
            Instruction::ArrayNew { dst, .. } => {
                let identity = self.next_array;
                self.next_array = self.next_array.checked_add(1)?;
                self.clear_destination(dst)?;
                *self.local_array.get_mut(usize::from(dst))? = Some((identity, 0));
            }
            Instruction::ArrayPush { source, .. } => {
                let (identity, length) = (*self.local_array.get(usize::from(source))?)?;
                self.array_pushes = self.array_pushes.checked_add(1)?;
                self.array_push_element_fuel = self.array_push_element_fuel.checked_add(
                    u64::try_from(length.max(1))
                        .ok()?
                        .div_ceil(nexa_bytecode::STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS),
                )?;
                let next_length = length.checked_add(1)?;
                for array in &mut self.local_array {
                    if array.is_some_and(|(candidate, _)| candidate == identity) {
                        *array = Some((identity, next_length));
                    }
                }
            }
            Instruction::ArraySet { source, index, .. } => {
                self.validate_array_index(source, index)?;
            }
            Instruction::ArrayGet {
                source, index, dst, ..
            } => {
                self.validate_array_index(source, index)?;
                self.clear_destination(dst)?;
            }
            Instruction::ArrayLen { source, dst } => {
                let (_, length) = (*self.local_array.get(usize::from(source))?)?;
                self.clear_destination(dst)?;
                *self.i32_constant.get_mut(usize::from(dst))? = i32::try_from(length).ok();
            }
            _ => unreachable!("array analysis receives only array instructions"),
        }
        Some(())
    }

    fn validate_array_index(&self, source: u16, index: u16) -> Option<()> {
        let (_, length) = (*self.local_array.get(usize::from(source))?)?;
        let index = usize::try_from((*self.i32_constant.get(usize::from(index))?)?).ok()?;
        (index < length).then_some(())
    }

    fn observe_buffer(&mut self, instruction: Instruction) -> Option<()> {
        match instruction {
            Instruction::BufferCopy {
                destination,
                source,
                source_start,
                destination_start,
                length,
            } => {
                let destination = (*self.buffer_value.get(usize::from(destination))?)?;
                let source = (*self.buffer_value.get(usize::from(source))?)?;
                if self.buffer_copy.is_some() {
                    return None;
                }
                let source_start =
                    usize::try_from((*self.i32_constant.get(usize::from(source_start))?)?).ok()?;
                let destination_start =
                    usize::try_from((*self.i32_constant.get(usize::from(destination_start))?)?)
                        .ok()?;
                let length =
                    usize::try_from((*self.i32_constant.get(usize::from(length))?)?).ok()?;
                self.buffer_work_fuel = self.buffer_work_fuel.checked_add(
                    u64::try_from(length)
                        .ok()?
                        .div_ceil(nexa_bytecode::STANDARD_COLLECTION_FUEL_BLOCK_ELEMENTS),
                )?;
                self.buffer_copy = Some(StaticLeafBufferCopy {
                    destination,
                    source,
                    source_start,
                    destination_start,
                    length,
                });
            }
            Instruction::BufferGet { source, index, dst } => {
                let source = (*self.buffer_value.get(usize::from(source))?)?;
                if self.buffer_get.is_some() {
                    return None;
                }
                let index = usize::try_from((*self.i32_constant.get(usize::from(index))?)?).ok()?;
                self.buffer_get = Some(StaticLeafBufferGet { source, index });
                self.clear_destination(dst)?;
            }
            _ => unreachable!("buffer analysis receives only buffer instructions"),
        }
        Some(())
    }

    fn observe_map(&mut self, instruction: Instruction) -> Option<()> {
        match instruction {
            Instruction::MapNew { dst, .. } => {
                let identity = self.next_map;
                self.next_map = self.next_map.checked_add(1)?;
                self.clear_destination(dst)?;
                *self.local_map.get_mut(usize::from(dst))? = Some(identity);
            }
            Instruction::MapSet { source, key, .. } => {
                (*self.local_map.get(usize::from(source))?)?;
                (*self.i32_constant.get(usize::from(key))?)?;
                // One insertion into a fresh local table cannot enter the
                // incremental rehash protocol on admitted heap shapes.
                if self.map_sets != 0 {
                    return None;
                }
                self.map_sets = 1;
            }
            Instruction::MapGet {
                source, key, dst, ..
            } => {
                (*self.local_map.get(usize::from(source))?)?;
                (*self.i32_constant.get(usize::from(key))?)?;
                if self.map_lookups != 0 {
                    return None;
                }
                self.map_lookups = 1;
                self.clear_destination(dst)?;
            }
            _ => unreachable!("map analysis receives only map instructions"),
        }
        Some(())
    }

    const fn finish(
        self,
        fixed_fuel: u64,
        buffer_kernel_instructions: Option<u8>,
    ) -> StaticLeafCertificate {
        StaticLeafCertificate {
            fixed_fuel,
            array_pushes: self.array_pushes,
            array_push_element_fuel: self.array_push_element_fuel,
            buffer_copy: self.buffer_copy,
            buffer_get: self.buffer_get,
            buffer_work_fuel: self.buffer_work_fuel,
            buffer_kernel_instructions,
            map_sets: self.map_sets,
            map_lookups: self.map_lookups,
        }
    }
}

fn certify_static_leaf(
    function: &nexa_bytecode::Function,
    rows: &[ExecutableInstruction],
) -> Option<StaticLeafCertificate> {
    if usize::from(function.registers) > crate::trusted::STATIC_LEAF_REGISTER_CAPACITY
        || function.code.len() > STATIC_LEAF_MAX_INSTRUCTIONS
    {
        return None;
    }
    let mut analysis = StaticLeafAnalysis::new(function.signature.parameters.len())?;
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        if !static_leaf_instruction_supported(instruction) {
            return None;
        }
        if let Instruction::Jump { target } | Instruction::JumpIfFalse { target, .. } = instruction
        {
            let target = usize::try_from(target).ok()?;
            if target <= pc || target >= function.code.len() {
                return None;
            }
        }
        analysis.observe(instruction)?;
    }
    let fixed_fuel = rows
        .iter()
        .try_fold(0_u64, |fuel, row| fuel.checked_add(row.attempt_fuel))?;
    Some(analysis.finish(fixed_fuel, certify_static_leaf_buffer_kernel(function)))
}

fn certify_static_leaf_buffer_kernel(function: &nexa_bytecode::Function) -> Option<u8> {
    let mut buffer_result = [false; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY];
    let mut copied = false;
    let mut loaded = false;
    let mut returned = false;
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        if returned {
            return None;
        }
        match instruction {
            Instruction::LoadI32 { dst, .. } => {
                *buffer_result.get_mut(usize::from(dst))? = false;
            }
            Instruction::Move { dst, source } => {
                *buffer_result.get_mut(usize::from(dst))? =
                    *buffer_result.get(usize::from(source))?;
            }
            Instruction::BufferCopy { .. } if !copied && !loaded => {
                copied = true;
            }
            Instruction::BufferGet { dst, .. } if copied && !loaded => {
                *buffer_result.get_mut(usize::from(dst))? = true;
                loaded = true;
            }
            Instruction::Return { source }
                if loaded
                    && *buffer_result.get(usize::from(source))?
                    && pc + 1 == function.code.len() =>
            {
                returned = true;
            }
            _ => return None,
        }
    }
    if !(copied && loaded && returned) {
        return None;
    }
    u8::try_from(function.code.len()).ok()
}

#[derive(Clone, Copy)]
enum StaticConstantValue {
    Unknown,
    I32(i32),
    StringLength(i32),
}

fn certify_static_leaf_constant_kernel(
    module: &VerifiedModule,
    function: &nexa_bytecode::Function,
) -> Option<StaticLeafConstantKernel> {
    let mut values = [StaticConstantValue::Unknown; crate::trusted::STATIC_LEAF_REGISTER_CAPACITY];
    let mut effect = None;
    let mut result = None;
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        match instruction {
            Instruction::LoadI32 { dst, value } => {
                *values.get_mut(usize::from(dst))? = StaticConstantValue::I32(value);
            }
            Instruction::LoadString { dst, string } if effect.is_none() => {
                let length = module.module().strings.get(string as usize)?.len();
                let length = i32::try_from(length).ok()?;
                effect = Some(StaticLeafConstantEffect::LoadString { string });
                *values.get_mut(usize::from(dst))? = StaticConstantValue::StringLength(length);
            }
            Instruction::Move { dst, source } => {
                *values.get_mut(usize::from(dst))? = *values.get(usize::from(source))?;
            }
            Instruction::Add { dst, lhs, rhs } => {
                let (StaticConstantValue::I32(lhs), StaticConstantValue::I32(rhs)) = (
                    *values.get(usize::from(lhs))?,
                    *values.get(usize::from(rhs))?,
                ) else {
                    return None;
                };
                *values.get_mut(usize::from(dst))? =
                    StaticConstantValue::I32(lhs.wrapping_add(rhs));
            }
            Instruction::StringByteLen { dst, source } => {
                let StaticConstantValue::StringLength(length) = *values.get(usize::from(source))?
                else {
                    return None;
                };
                *values.get_mut(usize::from(dst))? = StaticConstantValue::I32(length);
            }
            Instruction::EnumNew {
                type_id,
                variant,
                payload: None,
                dst,
            } if effect.is_none() => {
                effect = Some(StaticLeafConstantEffect::EnumNew { type_id, variant });
                *values.get_mut(usize::from(dst))? = StaticConstantValue::Unknown;
            }
            Instruction::Return { source } if pc + 1 == function.code.len() => {
                let StaticConstantValue::I32(value) = *values.get(usize::from(source))? else {
                    return None;
                };
                result = Some(value);
            }
            _ => return None,
        }
    }
    Some(StaticLeafConstantKernel {
        result: result?,
        instructions: u8::try_from(function.code.len()).ok()?,
        effect: effect.unwrap_or(StaticLeafConstantEffect::None),
    })
}

const fn static_leaf_instruction_supported(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadI32 { .. }
            | Instruction::LoadString { .. }
            | Instruction::Move { .. }
            | Instruction::Add { .. }
            | Instruction::CompareEq { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::StringByteLen { .. }
            | Instruction::EnumNew { .. }
            | Instruction::EnumTag { .. }
            | Instruction::EnumPayload { .. }
            | Instruction::ClassNew { .. }
            | Instruction::ClassGet { .. }
            | Instruction::ClassSet { .. }
            | Instruction::ArrayNew { .. }
            | Instruction::ArrayPush { .. }
            | Instruction::ArraySet { .. }
            | Instruction::ArrayGet { .. }
            | Instruction::ArrayLen { .. }
            | Instruction::MapNew { .. }
            | Instruction::MapSet { .. }
            | Instruction::MapGet { .. }
            | Instruction::BufferCopy { .. }
            | Instruction::BufferGet { .. }
            | Instruction::Return { .. }
            | Instruction::Trap
    )
}

const fn static_leaf_control_tail_instruction(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadI32 { .. }
            | Instruction::LoadString { .. }
            | Instruction::Move { .. }
            | Instruction::Add { .. }
            | Instruction::CompareEq { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::StringByteLen { .. }
            | Instruction::EnumTag { .. }
            | Instruction::EnumPayload { .. }
            | Instruction::Return { .. }
            | Instruction::Trap
    )
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder,
        Signature, ValueType,
    };
    use nexa_core::StableId;
    use nexa_verifier::{VerifierLimits, verify};

    use super::{ExecutableBuildError, ExecutableModule, static_leaf_instruction_supported};
    use crate::interpreter::{OpcodeCostTable, is_safepoint};

    const CORPUS: &str = r#"
struct Pair { first: i32, second: i32, }
class Counter { mut value: i32, }
enum Signal { Quiet, Loud(i32), }

fn mixed(x: i32) -> i32 {
    let text: string = "predecode";
    let cell: Pair = Pair { first: x, second: text.byte_len() };
    let values: Array<i32> = Array::new();
    values.push(cell.first);
    let table: Map<i32, i32> = Map::new();
    table.set(1, cell.second);
    let signal: Signal = Signal::Loud(x);
    let selected: i32 = match signal {
        Signal::Quiet => 0,
        Signal::Loud(value) => value,
    };
    return helper(selected) + values.len() + table.len();
}
fn helper(x: i32) -> i32 {
    return x + 1;
}
fn update_counter() -> i32 {
    let counter: Counter = new Counter { value: 1 };
    counter.value = counter.value + 1;
    return counter.value;
}
"#;

    /// The operand-dependent surface frozen by F1: a row may be dynamic
    /// only for these instructions. Static misclassification of any of
    /// them would silently change charged fuel, so the set is pinned.
    fn dynamic_surface(instruction: Instruction) -> bool {
        matches!(
            instruction,
            Instruction::StandardIntrinsic { .. }
                | Instruction::StringLen { .. }
                | Instruction::StringRuneAt { .. }
                | Instruction::StringEqual { .. }
                | Instruction::StringConcat { .. }
                | Instruction::StringBuild { .. }
                | Instruction::StructNew { .. }
                | Instruction::StructWith { .. }
                | Instruction::EnumEqual { .. }
                | Instruction::StructEqual { .. }
                | Instruction::ArrayPush { .. }
                | Instruction::ArrayInsert { .. }
                | Instruction::ArrayPop { .. }
                | Instruction::ArrayRemove { .. }
                | Instruction::ArrayClear { .. }
                | Instruction::MapGet { .. }
                | Instruction::MapRemove { .. }
                | Instruction::MapContains { .. }
                | Instruction::MapSet { .. }
                | Instruction::MapClear { .. }
                | Instruction::BufferSlice { .. }
                | Instruction::BufferCopy { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::CleanupReturn
        )
    }

    #[test]
    fn rows_cover_the_corpus_and_pin_the_dynamic_surface() {
        assert!(
            std::mem::size_of::<super::ExecutableInstruction>() <= 24,
            "hot metadata rows must not duplicate the 48-byte portable instruction: row={} dense_nominal={} verifier_nominal={}",
            std::mem::size_of::<super::ExecutableInstruction>(),
            std::mem::size_of::<super::ExecutableNominalOperand>(),
            std::mem::size_of::<nexa_verifier::ResolvedNominalOperand>()
        );
        let module = nexa_compiler::compile(CORPUS).expect("F1 corpus compiles");
        let costs = OpcodeCostTable::default();
        let executable = ExecutableModule::build(&module, &costs).expect("build executable");
        assert_eq!(
            executable.functions().len(),
            module.module().functions.len()
        );
        for (function, rows) in module.module().functions.iter().zip(executable.functions()) {
            assert_eq!(function.code.len(), rows.rows().len());
            for (pc, (instruction, row)) in
                function.code.iter().copied().zip(rows.rows()).enumerate()
            {
                let pc = u32::try_from(pc).expect("test corpus pcs fit u32");
                assert_eq!(
                    row.safepoint(),
                    is_safepoint(instruction, pc),
                    "safepoint flag diverges at pc {pc}"
                );
                if row.dynamic_fuel() {
                    assert!(
                        dynamic_surface(instruction),
                        "instruction unexpectedly dynamic at pc {pc}: {instruction:?}"
                    );
                } else {
                    assert!(
                        !dynamic_surface(instruction),
                        "operand-dependent instruction misclassified static at pc {pc}: {instruction:?}"
                    );
                    assert!(row.attempt_fuel > 0, "static rows charge fuel");
                }
            }
        }
        let (static_rows, total) = executable.static_fuel_coverage();
        assert!(total > 0);
        assert!(
            static_rows * 2 > total,
            "most corpus rows settle at load time ({static_rows}/{total})"
        );
        assert!(
            static_rows < total,
            "the corpus keeps a dynamic remainder ({static_rows}/{total})"
        );
        for (instruction, row) in module
            .module()
            .functions
            .iter()
            .zip(executable.functions())
            .flat_map(|(function, rows)| function.code.iter().copied().zip(rows.rows()))
        {
            match instruction {
                Instruction::EnumNew { .. } => assert!(
                    matches!(
                        row.resolved_nominal,
                        super::ExecutableNominalOperand::EnumVariant { .. }
                    ),
                    "enum construction rows carry dense type and variant slots"
                ),
                Instruction::StructGet { .. } | Instruction::StructWith { .. } => assert!(
                    matches!(
                        row.resolved_nominal,
                        super::ExecutableNominalOperand::StructField { .. }
                    ),
                    "struct field rows carry a verifier-proven dense index"
                ),
                Instruction::ClassGet { .. } | Instruction::ClassSet { .. } => assert!(
                    matches!(
                        row.resolved_nominal,
                        super::ExecutableNominalOperand::ClassField { .. }
                    ),
                    "class field rows carry a verifier-proven dense index and type"
                ),
                Instruction::ArrayNew { .. } => assert!(
                    matches!(
                        row.resolved_nominal,
                        super::ExecutableNominalOperand::ArrayType { .. }
                    ),
                    "array construction rows carry dense type and row-layout slots"
                ),
                Instruction::MapNew { .. } => assert!(
                    matches!(
                        row.resolved_nominal,
                        super::ExecutableNominalOperand::MapType { .. }
                    ),
                    "map construction rows carry a dense type slot"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn host_call_rows_fold_the_import_surcharge() {
        let host = StableId::from_name("executable-f1-host");
        let import = HostImport {
            stable_id: StableId::from_name("executable-f1-host.effect"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: vec![],
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 37,
            async_result: None,
        };
        let mut builder = ModuleBuilder::new();
        builder.metadata(host, nexa_bytecode::StateSchema::default().fingerprint());
        let import_index = builder.host_import(import);
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![],
                result: Some(ValueType::I32),
            },
            1,
        );
        function.effect(FunctionEffect::Task);
        function
            .emit(Instruction::HostCall {
                import: import_index,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        builder.function(function.finish().expect("host call function"));
        let module = verify(builder.finish(), VerifierLimits::default()).expect("verify module");
        let costs = OpcodeCostTable::default();
        let executable = ExecutableModule::build(&module, &costs).expect("build executable");
        let row = executable.functions()[0].rows()[0];
        let instruction = module.module().functions[0].code[0];
        let bare = costs.cost(instruction);
        assert_eq!(
            row.attempt_fuel,
            bare + 37,
            "HostCall rows carry the folded import surcharge"
        );
        assert!(!row.dynamic_fuel());
    }

    #[test]
    fn build_rejects_a_foreign_cost_table_version() {
        let module = nexa_compiler::compile("fn value() -> i32 { return 7; }")
            .expect("version corpus compiles");
        let mut costs = OpcodeCostTable::default();
        costs.version += 1;
        assert!(matches!(
            ExecutableModule::build(&module, &costs),
            Err(ExecutableBuildError::CostTableVersionMismatch { .. })
        ));
    }

    #[test]
    fn static_leaf_scalar_and_control_instruction_surface_is_narrow() {
        for instruction in [
            Instruction::LoadI32 { dst: 0, value: 7 },
            Instruction::LoadString { dst: 0, string: 0 },
            Instruction::Move { dst: 1, source: 0 },
            Instruction::Add {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            Instruction::CompareEq {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            Instruction::Jump { target: 1 },
            Instruction::JumpIfFalse {
                condition: 0,
                target: 1,
            },
            Instruction::StringByteLen { dst: 1, source: 0 },
            Instruction::EnumNew {
                type_id: StableId(1),
                variant: StableId(2),
                payload: Some(0),
                dst: 1,
            },
            Instruction::EnumTag { source: 0, dst: 1 },
            Instruction::EnumPayload {
                source: 0,
                variant: StableId(8),
                dst: 1,
            },
            Instruction::Return { source: 0 },
            Instruction::Trap,
        ] {
            assert!(
                static_leaf_instruction_supported(instruction),
                "certified leaf instruction: {instruction:?}"
            );
        }
        for instruction in [
            Instruction::Div {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
            Instruction::Call {
                function: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            },
            Instruction::DeferPush {
                function: 0,
                args_base: 0,
                args_count: 0,
            },
            Instruction::Yield,
        ] {
            assert!(
                !static_leaf_instruction_supported(instruction),
                "effectful, suspending, or unbounded instruction: {instruction:?}"
            );
        }
    }

    #[test]
    fn static_leaf_collection_instruction_surface_is_narrow() {
        for instruction in [
            Instruction::ClassNew {
                type_id: StableId(3),
                fields_base: 0,
                fields_count: 1,
                dst: 1,
            },
            Instruction::ClassGet {
                source: 1,
                field: StableId(4),
                dst: 2,
            },
            Instruction::ClassSet {
                source: 1,
                field: StableId(4),
                value: 2,
            },
            Instruction::ArrayNew {
                type_id: StableId(5),
                dst: 0,
            },
            Instruction::ArrayPush {
                source: 0,
                value: 1,
            },
            Instruction::ArraySet {
                source: 0,
                index: 1,
                value: 2,
            },
            Instruction::ArrayGet {
                source: 0,
                index: 1,
                dst: 2,
            },
            Instruction::ArrayLen { source: 0, dst: 1 },
            Instruction::MapNew {
                type_id: StableId(6),
                dst: 0,
            },
            Instruction::MapSet {
                source: 0,
                key: 1,
                value: 2,
            },
            Instruction::MapGet {
                source: 0,
                key: 1,
                result_type: StableId(7),
                dst: 2,
            },
            Instruction::BufferCopy {
                destination: 0,
                source: 1,
                source_start: 2,
                destination_start: 2,
                length: 2,
            },
            Instruction::BufferGet {
                source: 0,
                index: 1,
                dst: 2,
            },
        ] {
            assert!(
                static_leaf_instruction_supported(instruction),
                "certified leaf instruction: {instruction:?}"
            );
        }
    }

    #[test]
    fn static_leaf_certificate_is_register_bounded() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![],
                result: Some(ValueType::I32),
            },
            25,
        );
        function
            .emit(Instruction::LoadI32 { dst: 24, value: 7 })
            .emit(Instruction::Return { source: 24 });
        let mut builder = ModuleBuilder::new();
        builder.metadata(
            StableId::from_name("static-leaf-register-bound"),
            nexa_bytecode::StateSchema::default().fingerprint(),
        );
        builder.function(function.finish().expect("wide leaf function"));
        let module = verify(builder.finish(), VerifierLimits::default()).expect("verify wide leaf");
        let executable =
            ExecutableModule::build(&module, OpcodeCostTable::canonical()).expect("predecode");
        assert_eq!(
            executable.functions()[0].static_leaf_fuel(),
            None,
            "a verified function wider than the fixed leaf bank must fall back"
        );
    }
}
