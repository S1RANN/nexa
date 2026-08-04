//! Structural, type and continuation verification for Nexa bytecode.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use nexa_bytecode::{
    ArrayType, EnumVariant, Function, FunctionEffect, HostCallMode, Instruction, MapType, Module,
    SCALAR_TO_STRING_FUEL_PASSES, SCALAR_TO_STRING_MAX_BYTES, STANDARD_STRING_FUEL_BLOCK_BYTES,
    StandardIntrinsic, StructField, StructType, ValueType, minimum_migration_limits,
};
use nexa_core::{FingerprintBuilder, SourceSpan, StableId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifierLimits {
    pub max_frame_bytes: u32,
    pub max_immediate_cost: u32,
    pub max_wcet_states: u32,
}

impl Default for VerifierLimits {
    fn default() -> Self {
        Self {
            max_frame_bytes: 64 * 1024,
            max_immediate_cost: 1_024,
            max_wcet_states: 100_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    pub function: usize,
    pub instruction: Option<usize>,
    pub kind: VerifyErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyErrorKind {
    EmptyFunction,
    RegisterOutOfRange(u16),
    FunctionOutOfRange(u32),
    HostImportOutOfRange(u32),
    ExportOutOfRange(u32),
    InvalidExportSignature,
    DuplicateExport,
    JumpOutOfRange(u32),
    TypeMismatch,
    ConflictingControlFlowTypes,
    InvalidReturn,
    FrameLimit,
    RootBitmapLength,
    ForgedRoot(u16),
    MissingRoot(u16),
    ImmediateCostLimit,
    MissingSafepoint(u32),
    InvalidSafepoint(u32),
    InvalidRootMap(u32),
    InvalidLoopBound(u32),
    InvalidEffect,
    InvalidHostImportMetadata,
    ImmediateRecursion,
    WcetComplexityLimit,
    InvalidEnumMetadata,
    InvalidStructMetadata,
    InvalidClassMetadata,
    InvalidStateMetadata,
    InvalidArrayMetadata,
    InvalidMapMetadata,
    InvalidBufferMetadata,
    InvalidSnapshotMetadata,
    InvalidResourceTokenMetadata,
    InvalidOpaqueMetadata,
    InvalidPhysicalAbi,
    InvalidSourceMap,
    /// M5 WP35: every verified module must yield a deterministic layout
    /// table and function ABI; recursion, dangling types, and slot
    /// overflows are rejected before execution.
    InvalidValueLayout(nexa_bytecode::layout::LayoutError),
    EnumTypeOutOfRange(u64),
    EnumVariantOutOfRange(u64),
    StructTypeOutOfRange(u64),
    StructFieldOutOfRange(u64),
    ClassTypeOutOfRange(u64),
    ClassFieldOutOfRange(u64),
    ArrayTypeOutOfRange(u64),
    MapTypeOutOfRange(u64),
    BufferTypeOutOfRange(u64),
    InvalidReloadMetadata,
    InvalidRune(u32),
    StringOutOfRange(u32),
}

impl fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "verify error in function {} at {:?}: {:?}",
            self.function, self.instruction, self.kind
        )
    }
}

impl std::error::Error for VerifyError {}

#[derive(Clone, Debug)]
pub struct VerifiedModule {
    module: Arc<Module>,
    layout_table: Arc<nexa_bytecode::layout::LayoutTable>,
    module_abi: Arc<nexa_bytecode::layout::ModuleAbi>,
    nominal_indexes: Arc<NominalIndexes>,
    resolved_operands: Arc<Vec<Vec<ResolvedNominalOperand>>>,
    portable_fingerprint: [u8; 32],
    profile_fingerprint: [u8; 32],
    profile_metadata: Option<Arc<ModuleProfileMetadata>>,
}

/// Process-local immutable verifier authorities reused across reload
/// candidates with equal content.
///
/// None of these flags weaken verification: both modules were admitted
/// independently, and sharing happens only after exact structural equality.
/// The result exists so Runtime can expose evidence that reload retained one
/// copy instead of merely rebuilding equal allocations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerifiedImmutableReuse {
    pub layout_table: bool,
    pub module_abi: bool,
    pub profile_metadata: bool,
}

/// Cold semantic identity catalog used by the bounded Runtime profiler.
///
/// Dense function indices remain the execution representation. This catalog
/// is attached by the Package façade after verification and lets enabled
/// profiling resolve those slots to stable Package/source identities without
/// putting strings, hashing, or allocation on the interpreter hot path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleProfileMetadata {
    functions: Arc<[FunctionProfileMetadata]>,
    fingerprint: [u8; 32],
}

/// Stable source identity for one verified dense function slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionProfileMetadata {
    pub function: u32,
    pub package_id: String,
    pub module: String,
    pub stable_id: StableId,
    pub definition_span: SourceSpan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileMetadataError {
    FunctionOutOfRange(u32),
    DuplicateFunction(u32),
}

impl fmt::Display for ProfileMetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FunctionOutOfRange(function) => {
                write!(
                    formatter,
                    "profile metadata function {function} is out of range"
                )
            }
            Self::DuplicateFunction(function) => {
                write!(
                    formatter,
                    "profile metadata function {function} is duplicated"
                )
            }
        }
    }
}

impl std::error::Error for ProfileMetadataError {}

impl ModuleProfileMetadata {
    #[must_use]
    pub fn new(mut functions: Vec<FunctionProfileMetadata>) -> Self {
        functions.sort_by_key(|function| function.function);
        let mut fingerprint = FingerprintBuilder::new("nexa.profiler.module-metadata", 1);
        fingerprint.field_u64(
            "functions",
            u64::try_from(functions.len()).unwrap_or(u64::MAX),
        );
        for function in &functions {
            fingerprint.field_u32("function", function.function);
            fingerprint.field_str("package", &function.package_id);
            fingerprint.field_str("module", &function.module);
            fingerprint.field_u64("stable-id", function.stable_id.0);
            fingerprint.field_u32("file", function.definition_span.file.0);
            fingerprint.field_u32("span-start", function.definition_span.start);
            fingerprint.field_u32("span-end", function.definition_span.end);
        }
        Self {
            functions: functions.into(),
            fingerprint: fingerprint.finish_bytes(),
        }
    }

    #[must_use]
    pub fn function(&self, function: u32) -> Option<&FunctionProfileMetadata> {
        self.functions
            .binary_search_by_key(&function, |metadata| metadata.function)
            .ok()
            .map(|index| &self.functions[index])
    }

    #[must_use]
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    #[must_use]
    pub fn functions(&self) -> &[FunctionProfileMetadata] {
        &self.functions
    }
}

/// Dense nominal metadata proven by the verifier for one instruction.
///
/// This data is derived from the exact register-type state at the
/// instruction and is never serialized. Runtime executable rows can use it
/// without repeating stable-ID lookups in the hot loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResolvedNominalOperand {
    #[default]
    None,
    EnumVariant {
        type_index: u16,
        variant_index: u16,
        tag: u32,
        payload_offset: u16,
        payload_slots: u16,
        owner_slots: u16,
    },
    EnumLayout {
        type_index: u16,
        slots: u16,
    },
    StructLayout {
        type_id: StableId,
        slots: u16,
    },
    StructField {
        type_id: StableId,
        index: u16,
        offset: u16,
        slots: u16,
        owner_slots: u16,
    },
    ClassField {
        type_index: u16,
        index: u16,
        offset: u16,
        slots: u16,
        owner_slots: u16,
        state_index: Option<u16>,
    },
    StateField {
        type_index: u16,
        field_index: u16,
        sorted_index: u16,
    },
    ArrayLayout {
        type_index: u16,
        element_slots: u16,
        row_slots: u16,
    },
    ArrayField {
        type_index: u16,
        offset: u16,
        slots: u16,
        row_slots: u16,
    },
    MapLayout {
        type_index: u16,
        key_slots: u16,
        value_slots: u16,
        option_slots: u16,
        option_payload_offset: u16,
    },
    StandardIntrinsic {
        argument_slots: [u16; 3],
        result_slots: u16,
        input_payload_offset: u16,
        result_payload_offset: u16,
    },
    CallFrame {
        register_count: u16,
        parameter_slots: u16,
        result_slots: u16,
    },
}

#[derive(Clone, Debug)]
struct NominalIndexes {
    enum_variants: Vec<((u64, u64), (usize, usize))>,
    struct_fields: Vec<((u64, u64), (usize, usize))>,
    class_fields: Vec<((u64, u64), (usize, usize))>,
    array_types: Vec<(u64, usize)>,
    map_types: Vec<(u64, usize)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NominalIndexShape {
    pub enum_variants: usize,
    pub struct_fields: usize,
    pub class_fields: usize,
    pub array_types: usize,
    pub map_types: usize,
}

impl NominalIndexes {
    fn new(module: &Module) -> Self {
        let mut enum_variants = module
            .enum_types
            .iter()
            .enumerate()
            .flat_map(|(type_index, enum_type)| {
                enum_type
                    .variants
                    .iter()
                    .enumerate()
                    .map(move |(variant_index, variant)| {
                        (
                            (enum_type.type_id.0, variant.stable_id.0),
                            (type_index, variant_index),
                        )
                    })
            })
            .collect::<Vec<_>>();
        enum_variants.sort_unstable_by_key(|(key, _)| *key);

        let mut struct_fields = module
            .struct_types
            .iter()
            .enumerate()
            .flat_map(|(type_index, struct_type)| {
                struct_type
                    .fields
                    .iter()
                    .enumerate()
                    .map(move |(field_index, field)| {
                        (
                            (struct_type.type_id.0, field.stable_id.0),
                            (type_index, field_index),
                        )
                    })
            })
            .collect::<Vec<_>>();
        struct_fields.sort_unstable_by_key(|(key, _)| *key);

        let mut class_fields = module
            .class_types
            .iter()
            .enumerate()
            .flat_map(|(type_index, class_type)| {
                class_type
                    .fields
                    .iter()
                    .enumerate()
                    .map(move |(field_index, field)| {
                        (
                            (class_type.type_id.0, field.stable_id.0),
                            (type_index, field_index),
                        )
                    })
            })
            .collect::<Vec<_>>();
        class_fields.sort_unstable_by_key(|(key, _)| *key);

        let mut array_types = module
            .array_types
            .iter()
            .enumerate()
            .map(|(index, array_type)| (array_type.type_id.0, index))
            .collect::<Vec<_>>();
        array_types.sort_unstable_by_key(|(type_id, _)| *type_id);

        let mut map_types = module
            .map_types
            .iter()
            .enumerate()
            .map(|(index, map_type)| (map_type.type_id.0, index))
            .collect::<Vec<_>>();
        map_types.sort_unstable_by_key(|(type_id, _)| *type_id);

        Self {
            enum_variants,
            struct_fields,
            class_fields,
            array_types,
            map_types,
        }
    }
}

impl VerifiedModule {
    fn new(
        module: Module,
        resolved_operands: Vec<Vec<ResolvedNominalOperand>>,
        layout_table: nexa_bytecode::layout::LayoutTable,
        module_abi: nexa_bytecode::layout::ModuleAbi,
    ) -> Self {
        let nominal_indexes = NominalIndexes::new(&module);
        let mut fingerprint = FingerprintBuilder::new("nexa.bytecode.portable-module", 1);
        fingerprint.field_bytes("module", &module.encode());
        let portable_fingerprint = fingerprint.finish_bytes();
        Self {
            module: Arc::new(module),
            layout_table: Arc::new(layout_table),
            module_abi: Arc::new(module_abi),
            nominal_indexes: Arc::new(nominal_indexes),
            resolved_operands: Arc::new(resolved_operands),
            portable_fingerprint,
            profile_fingerprint: portable_fingerprint,
            profile_metadata: None,
        }
    }

    #[must_use]
    pub fn module(&self) -> &Module {
        &self.module
    }

    /// The verifier-derived physical layout authority for this exact module.
    ///
    /// Runtime frame planning and aggregate operations consume this table
    /// directly; they must never rederive a potentially divergent view.
    #[must_use]
    pub fn layout_table(&self) -> &nexa_bytecode::layout::LayoutTable {
        &self.layout_table
    }

    /// The verifier-derived physical calling convention indexed by function.
    #[must_use]
    pub fn module_abi(&self) -> &nexa_bytecode::layout::ModuleAbi {
        &self.module_abi
    }

    /// Canonical portable-bytecode identity computed once at verification.
    ///
    /// Runtime execution-image caches combine this value with the opcode-cost
    /// table version; process-local dense metadata is never serialized.
    #[must_use]
    pub const fn portable_fingerprint(&self) -> [u8; 32] {
        self.portable_fingerprint
    }

    /// Attaches Package/source identities after structural verification.
    ///
    /// Profile metadata is deliberately not part of portable bytecode or its
    /// execution-image cache key. It is validated against the dense function
    /// table and contributes to a separate profiler key so identical code
    /// linked under different Package identities cannot be conflated.
    pub fn attach_profile_metadata(
        &mut self,
        metadata: ModuleProfileMetadata,
    ) -> Result<(), ProfileMetadataError> {
        let mut previous = None;
        for function in metadata.functions() {
            if usize::try_from(function.function)
                .ok()
                .is_none_or(|index| index >= self.module.functions.len())
            {
                return Err(ProfileMetadataError::FunctionOutOfRange(function.function));
            }
            if previous == Some(function.function) {
                return Err(ProfileMetadataError::DuplicateFunction(function.function));
            }
            previous = Some(function.function);
        }
        let mut fingerprint = FingerprintBuilder::new("nexa.profiler.verified-module", 1);
        fingerprint.field_bytes("portable-module", &self.portable_fingerprint);
        fingerprint.field_bytes("semantic-metadata", &metadata.fingerprint());
        self.profile_fingerprint = fingerprint.finish_bytes();
        self.profile_metadata = Some(Arc::new(metadata));
        Ok(())
    }

    #[must_use]
    pub const fn profile_fingerprint(&self) -> [u8; 32] {
        self.profile_fingerprint
    }

    #[must_use]
    pub fn profile_metadata(&self) -> Option<&Arc<ModuleProfileMetadata>> {
        self.profile_metadata.as_ref()
    }

    /// Rebind equal, immutable verifier products to the already resident
    /// allocations owned by `other`.
    ///
    /// Portable bytecode, epoch/state identity, and profiler identity remain
    /// those of `self`. Only content-equal derived authorities are shared.
    pub fn reuse_immutable_from(&mut self, other: &Self) -> VerifiedImmutableReuse {
        let mut reused = VerifiedImmutableReuse::default();
        if self.layout_table == other.layout_table {
            self.layout_table = Arc::clone(&other.layout_table);
            reused.layout_table = true;
        }
        if self.module_abi == other.module_abi {
            self.module_abi = Arc::clone(&other.module_abi);
            reused.module_abi = true;
        }
        if self.profile_metadata == other.profile_metadata {
            self.profile_metadata.clone_from(&other.profile_metadata);
            reused.profile_metadata = self.profile_metadata.is_some();
        }
        reused
    }

    #[must_use]
    pub fn into_module(self) -> Module {
        Arc::try_unwrap(self.module).unwrap_or_else(|module| (*module).clone())
    }

    #[must_use]
    pub fn nominal_index_shape(&self) -> NominalIndexShape {
        NominalIndexShape {
            enum_variants: self.nominal_indexes.enum_variants.len(),
            struct_fields: self.nominal_indexes.struct_fields.len(),
            class_fields: self.nominal_indexes.class_fields.len(),
            array_types: self.nominal_indexes.array_types.len(),
            map_types: self.nominal_indexes.map_types.len(),
        }
    }

    #[must_use]
    pub fn resolved_operand(&self, function: usize, instruction: usize) -> ResolvedNominalOperand {
        self.resolved_operands
            .get(function)
            .and_then(|operands| operands.get(instruction))
            .copied()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn enum_variant(&self, type_id: u64, variant: u64) -> Option<&EnumVariant> {
        let (_, (type_index, variant_index)) = self
            .nominal_indexes
            .enum_variants
            .binary_search_by_key(&(type_id, variant), |(key, _)| *key)
            .ok()
            .and_then(|index| self.nominal_indexes.enum_variants.get(index))?;
        self.module
            .enum_types
            .get(*type_index)?
            .variants
            .get(*variant_index)
    }

    #[must_use]
    pub fn struct_field(&self, type_id: u64, field: u64) -> Option<(usize, &StructField)> {
        let (_, (type_index, field_index)) = self
            .nominal_indexes
            .struct_fields
            .binary_search_by_key(&(type_id, field), |(key, _)| *key)
            .ok()
            .and_then(|index| self.nominal_indexes.struct_fields.get(index))?;
        self.module
            .struct_types
            .get(*type_index)?
            .fields
            .get(*field_index)
            .map(|field| (*field_index, field))
    }

    /// WP52: the full struct layout behind one stable type ID, resolved
    /// through the sorted nominal index (no linear module scan). Structs
    /// without fields have no index entries and resolve to `None`, which
    /// callers treat as "keep the plain layout".
    #[must_use]
    pub fn struct_type(&self, type_id: u64) -> Option<&StructType> {
        let index = self
            .nominal_indexes
            .struct_fields
            .partition_point(|(key, _)| *key < (type_id, 0));
        let ((candidate, _), (type_index, _)) = self.nominal_indexes.struct_fields.get(index)?;
        if *candidate != type_id {
            return None;
        }
        self.module.struct_types.get(*type_index)
    }

    #[must_use]
    pub fn class_field(&self, type_id: u64, field: u64) -> Option<(usize, &StructField)> {
        let (_, (type_index, field_index)) = self
            .nominal_indexes
            .class_fields
            .binary_search_by_key(&(type_id, field), |(key, _)| *key)
            .ok()
            .and_then(|index| self.nominal_indexes.class_fields.get(index))?;
        self.module
            .class_types
            .get(*type_index)?
            .fields
            .get(*field_index)
            .map(|field| (*field_index, field))
    }

    #[must_use]
    pub fn array_type(&self, type_id: u64) -> Option<&ArrayType> {
        let (_, type_index) = self
            .nominal_indexes
            .array_types
            .binary_search_by_key(&type_id, |(candidate, _)| *candidate)
            .ok()
            .and_then(|index| self.nominal_indexes.array_types.get(index))?;
        self.module.array_types.get(*type_index)
    }

    #[must_use]
    pub fn map_type(&self, type_id: u64) -> Option<&MapType> {
        let (_, type_index) = self
            .nominal_indexes
            .map_types
            .binary_search_by_key(&type_id, |(candidate, _)| *candidate)
            .ok()
            .and_then(|index| self.nominal_indexes.map_types.get(index))?;
        self.module.map_types.get(*type_index)
    }
}

pub fn verify(mut module: Module, limits: VerifierLimits) -> Result<VerifiedModule, VerifyError> {
    verify_reload_metadata(&module)?;
    verify_source_map(&module)?;
    verify_named_type_metadata(&module)?;
    verify_host_import_metadata(&module)?;
    let (layout_table, module_abi) = verify_value_layouts(&module)?;
    let mut export_ids = BTreeSet::new();
    for export in &module.exports {
        if !export_ids.insert(export.stable_id) {
            return Err(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::DuplicateExport,
            });
        }
        let function = module
            .functions
            .get(export.function as usize)
            .ok_or(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::ExportOutOfRange(export.function),
            })?;
        if function.signature != export.signature || function.effect != export.effect {
            return Err(VerifyError {
                function: export.function as usize,
                instruction: None,
                kind: VerifyErrorKind::InvalidExportSignature,
            });
        }
    }
    let depths = static_call_depths(&module)?;
    for (function, depth) in module.functions.iter_mut().zip(depths) {
        function.max_static_call_depth = depth;
    }
    let immediate_closure = immediate_call_closure(&module);
    let restricted_closure = restricted_effect_call_closure(&module);
    let mut resolved_operands = Vec::with_capacity(module.functions.len());
    let physical = PhysicalVerificationContext {
        layouts: &layout_table,
        module_abi: &module_abi,
    };
    for (index, function) in module.functions.iter().enumerate() {
        resolved_operands.push(verify_function(
            &module,
            physical,
            index,
            function,
            limits,
            immediate_closure[index],
            restricted_closure[index],
        )?);
    }
    let immediate_costs = immediate_wcets(&module, &immediate_closure, limits.max_wcet_states)?;
    for (index, function) in module.functions.iter().enumerate() {
        if function.effect == FunctionEffect::Immediate
            && immediate_costs[index].unwrap_or(u32::MAX) > limits.max_immediate_cost
        {
            return Err(VerifyError {
                function: index,
                instruction: None,
                kind: VerifyErrorKind::ImmediateCostLimit,
            });
        }
    }
    Ok(VerifiedModule::new(
        module,
        resolved_operands,
        layout_table,
        module_abi,
    ))
}

#[allow(clippy::too_many_lines)]
fn verify_named_type_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut enum_ids = BTreeSet::new();
    for enum_type in &module.enum_types {
        let mut variant_ids = BTreeSet::new();
        let mut tags = BTreeSet::new();
        if !enum_ids.insert(enum_type.type_id)
            || enum_type.variants.is_empty()
            || enum_type.variants.iter().any(|variant| {
                variant.tag > i32::MAX as u32
                    || !variant_ids.insert(variant.stable_id)
                    || !tags.insert(variant.tag)
            })
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidEnumMetadata,
            });
        }
    }
    let mut named_ids = enum_ids;
    for type_id in &module.opaque_types {
        if !named_ids.insert(*type_id) {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidOpaqueMetadata,
            });
        }
    }
    for struct_type in &module.struct_types {
        let mut field_ids = BTreeSet::new();
        if !named_ids.insert(struct_type.type_id)
            || struct_type.fields.len() > nexa_bytecode::MAX_STRUCT_FIELDS
            || struct_type
                .fields
                .iter()
                .any(|field| !field_ids.insert(field.stable_id))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidStructMetadata,
            });
        }
    }
    for class_type in &module.class_types {
        let mut field_ids = BTreeSet::new();
        if !named_ids.insert(class_type.type_id)
            || class_type.fields.len() > nexa_bytecode::MAX_CLASS_FIELDS
            || class_type
                .fields
                .iter()
                .any(|field| !field_ids.insert(field.stable_id))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidClassMetadata,
            });
        }
    }
    let mut state_ids = BTreeSet::new();
    for state_type in &module.state_schema.types {
        let mut field_ids = BTreeSet::new();
        let matching_class = module
            .class_types
            .iter()
            .find(|class_type| class_type.type_id == state_type.stable_id);
        let class_layout_matches = matching_class.is_none_or(|class_type| {
            class_type.fields.len() == state_type.fields.len()
                && class_type.fields.iter().zip(&state_type.fields).all(
                    |(class_field, state_field)| {
                        class_field.stable_id == state_field.stable_id
                            && class_field.ty == state_field.ty
                    },
                )
        });
        if !state_ids.insert(state_type.stable_id)
            || (matching_class.is_none() && !named_ids.insert(state_type.stable_id))
            || !class_layout_matches
            || state_type.version == 0
            || state_type
                .fields
                .iter()
                .any(|field| !field_ids.insert(field.stable_id))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidStateMetadata,
            });
        }
    }
    for handle_type in &module.state_handle_types {
        if !named_ids.insert(handle_type.type_id)
            || handle_type.type_id != nexa_bytecode::state_handle_type(handle_type.target)
            || !matches!(handle_type.target, ValueType::Named(target) if module
                .state_schema
                .types
                .iter()
                .any(|state_type| state_type.stable_id == target))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidStateMetadata,
            });
        }
    }
    for array_type in &module.array_types {
        if !named_ids.insert(array_type.type_id)
            || array_type.type_id != nexa_bytecode::array_type(array_type.element)
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidArrayMetadata,
            });
        }
    }
    if module.opaque_types.iter().any(|opaque| {
        module.map_types.iter().any(|ty| ty.type_id == *opaque)
            || module.buffer_types.iter().any(|ty| ty.type_id == *opaque)
            || module.snapshot_types.iter().any(|ty| ty.type_id == *opaque)
            || module
                .resource_token_types
                .iter()
                .any(|ty| ty.type_id == *opaque)
    }) {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidOpaqueMetadata,
        });
    }
    verify_map_metadata(module)?;
    verify_buffer_metadata(module)?;
    verify_snapshot_metadata(module)?;
    verify_resource_token_metadata(module)?;
    verify_state_storage_metadata(module)?;
    Ok(())
}

fn verify_resource_token_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut ids = BTreeSet::new();
    if module.resource_token_types.iter().any(|token| {
        !ids.insert(token.type_id)
            || token.type_id != nexa_bytecode::resource_token_type(token.content_type)
            || module
                .enum_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .struct_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .class_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .state_schema
                .types
                .iter()
                .any(|ty| ty.stable_id == token.type_id)
            || module
                .state_handle_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .array_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .map_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .buffer_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
            || module
                .snapshot_types
                .iter()
                .any(|ty| ty.type_id == token.type_id)
    }) {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidResourceTokenMetadata,
        });
    }
    Ok(())
}

fn verify_snapshot_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut ids = BTreeSet::new();
    let valid_content = |content_type| {
        module
            .enum_types
            .iter()
            .any(|ty| ty.type_id == content_type)
            || module
                .struct_types
                .iter()
                .any(|ty| ty.type_id == content_type)
            || module
                .class_types
                .iter()
                .any(|ty| ty.type_id == content_type)
            || module
                .state_schema
                .types
                .iter()
                .any(|ty| ty.stable_id == content_type)
    };
    if module.snapshot_types.iter().any(|snapshot| {
        !ids.insert(snapshot.type_id)
            || snapshot.type_id != nexa_bytecode::snapshot_type(snapshot.content_type)
            || !valid_content(snapshot.content_type)
            || module
                .enum_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .struct_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .class_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .state_schema
                .types
                .iter()
                .any(|ty| ty.stable_id == snapshot.type_id)
            || module
                .state_handle_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .array_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .map_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
            || module
                .buffer_types
                .iter()
                .any(|ty| ty.type_id == snapshot.type_id)
    }) {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidSnapshotMetadata,
        });
    }
    Ok(())
}

fn verify_buffer_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut ids = BTreeSet::new();
    if module.buffer_types.iter().any(|buffer| {
        !ids.insert(buffer.type_id.0)
            || buffer.type_id != nexa_bytecode::buffer_type(buffer.element)
            || module
                .enum_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .struct_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .class_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .state_schema
                .types
                .iter()
                .any(|ty| ty.stable_id == buffer.type_id)
            || module
                .state_handle_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .array_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .map_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
            || module
                .snapshot_types
                .iter()
                .any(|ty| ty.type_id == buffer.type_id)
    }) {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidBufferMetadata,
        });
    }
    Ok(())
}

fn verify_map_metadata(module: &Module) -> Result<(), VerifyError> {
    let mut map_ids = BTreeSet::new();
    let invalid = module.map_types.iter().any(|map_type| {
        !map_ids.insert(map_type.type_id.0)
            || map_type.type_id != nexa_bytecode::map_type(map_type.key, map_type.value)
            || !valid_map_key_type(module, map_type.key)
            || module
                .enum_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
            || module
                .struct_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
            || module
                .class_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
            || module
                .state_schema
                .types
                .iter()
                .any(|ty| ty.stable_id == map_type.type_id)
            || module
                .state_handle_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
            || module
                .array_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
            || module
                .snapshot_types
                .iter()
                .any(|ty| ty.type_id == map_type.type_id)
    });
    if invalid {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidMapMetadata,
        });
    }
    Ok(())
}

fn valid_map_key_type(module: &Module, key: ValueType) -> bool {
    match key {
        ValueType::I32 | ValueType::I64 | ValueType::Rune | ValueType::String => true,
        ValueType::Named(type_id) => {
            module
                .state_handle_types
                .iter()
                .any(|handle| handle.type_id == type_id)
                || key == nexa_bytecode::stable_id_type()
                || module
                    .host_imports
                    .iter()
                    .any(|import| import.parameters.contains(&key) || import.result == Some(key))
        }
        ValueType::F32 | ValueType::F64 | ValueType::Bool | ValueType::Ref => false,
    }
}

fn verify_state_storage_metadata(module: &Module) -> Result<(), VerifyError> {
    for state_type in &module.state_schema.types {
        if state_type
            .fields
            .iter()
            .any(|field| !valid_state_storage_type(module, field.ty, &mut BTreeSet::new()))
        {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidStateMetadata,
            });
        }
    }
    Ok(())
}

fn valid_state_storage_type(module: &Module, ty: ValueType, visiting: &mut BTreeSet<u64>) -> bool {
    let ValueType::Named(type_id) = ty else {
        return matches!(
            ty,
            ValueType::I32
                | ValueType::I64
                | ValueType::F32
                | ValueType::F64
                | ValueType::Bool
                | ValueType::Rune
                | ValueType::String
                | ValueType::Ref
        );
    };
    if module
        .state_handle_types
        .iter()
        .any(|handle_type| handle_type.type_id == type_id)
    {
        return true;
    }
    if !visiting.insert(type_id.0) {
        return true;
    }
    let valid = if let Some(struct_type) = module
        .struct_types
        .iter()
        .find(|struct_type| struct_type.type_id == type_id)
    {
        struct_type
            .fields
            .iter()
            .all(|field| valid_state_storage_type(module, field.ty, visiting))
    } else if let Some(enum_type) = module
        .enum_types
        .iter()
        .find(|enum_type| enum_type.type_id == type_id)
    {
        enum_type.variants.iter().all(|variant| {
            variant
                .payload_type
                .is_none_or(|payload| valid_state_storage_type(module, payload, visiting))
        })
    } else {
        false
    };
    visiting.remove(&type_id.0);
    valid
}

fn has_state_handle_type(module: &Module, target: ValueType) -> bool {
    let type_id = nexa_bytecode::state_handle_type(target);
    module
        .state_handle_types
        .iter()
        .any(|handle_type| handle_type.type_id == type_id && handle_type.target == target)
}

/// M5 WP35: bytecode v7 carries a complete nominal closure, so both every
/// layout and every function ABI must derive before execution.
fn verify_value_layouts(
    module: &Module,
) -> Result<
    (
        nexa_bytecode::layout::LayoutTable,
        nexa_bytecode::layout::ModuleAbi,
    ),
    VerifyError,
> {
    let table =
        nexa_bytecode::layout::LayoutTable::for_module(module).map_err(|error| VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidValueLayout(error),
        })?;
    let module_abi =
        nexa_bytecode::layout::ModuleAbi::for_module(module, &table).map_err(|error| {
            VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidValueLayout(error),
            }
        })?;
    for (function_index, function) in module.functions.iter().enumerate() {
        let abi = module_abi.function(function_index).ok_or(VerifyError {
            function: function_index,
            instruction: None,
            kind: VerifyErrorKind::InvalidPhysicalAbi,
        })?;
        if function.parameter_slots != abi.parameter_slots
            || function.registers < function.parameter_slots
            || function.frame_bytes != u32::from(function.registers).saturating_mul(8)
        {
            return Err(VerifyError {
                function: function_index,
                instruction: None,
                kind: VerifyErrorKind::InvalidPhysicalAbi,
            });
        }
    }
    Ok((table, module_abi))
}

fn verify_host_import_metadata(module: &Module) -> Result<(), VerifyError> {
    for import in &module.host_imports {
        let capabilities_are_canonical = import.capabilities.len()
            <= nexa_bytecode::MAX_HOST_CAPABILITIES
            && import
                .capabilities
                .iter()
                .all(|capability| host_capability_is_valid(capability))
            && import
                .capabilities
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes());
        if !capabilities_are_canonical {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidHostImportMetadata,
            });
        }
        let valid = match (import.mode, import.async_result) {
            (HostCallMode::Immediate, None) => true,
            (HostCallMode::Async, Some(async_result)) => {
                let canonical =
                    nexa_bytecode::result_type(async_result.success, async_result.error);
                import.result == Some(ValueType::Named(async_result.result_type))
                    && async_result.result_type == canonical.type_id
                    && host_policy_error_is_valid(
                        module,
                        async_result.error,
                        async_result.cancel_policy == nexa_bytecode::CancelPolicy::ReturnError,
                        async_result.cancel_error,
                    )
                    && host_policy_error_is_valid(
                        module,
                        async_result.error,
                        async_result.abandon_policy == nexa_bytecode::AbandonPolicy::ReturnError,
                        async_result.abandon_error,
                    )
                    && module
                        .enum_types
                        .iter()
                        .any(|enum_type| enum_type == &canonical)
            }
            _ => false,
        };
        if !valid {
            return Err(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidEnumMetadata,
            });
        }
    }
    Ok(())
}

fn host_capability_is_valid(capability: &str) -> bool {
    !capability.is_empty()
        && capability.len() <= nexa_bytecode::MAX_HOST_CAPABILITY_BYTES
        && !capability.split('.').any(str::is_empty)
        && capability
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn host_policy_error_is_valid(
    module: &Module,
    error: ValueType,
    returns_error: bool,
    code: Option<u32>,
) -> bool {
    match (returns_error, code) {
        (false, None) => true,
        (true, Some(_)) if error == ValueType::I32 => true,
        (true, Some(code)) => match error {
            ValueType::Named(error_type) => module.enum_types.iter().any(|enum_type| {
                enum_type.type_id == error_type
                    && enum_type
                        .variants
                        .iter()
                        .any(|variant| variant.tag == code && variant.payload_type.is_none())
            }),
            _ => false,
        },
        _ => false,
    }
}

fn verify_source_map(module: &Module) -> Result<(), VerifyError> {
    for entry in &module.source_map {
        let Some(function) = module.functions.get(entry.function as usize) else {
            return Err(VerifyError {
                function: entry.function as usize,
                instruction: None,
                kind: VerifyErrorKind::InvalidSourceMap,
            });
        };
        if entry.pc_start >= entry.pc_end
            || entry.pc_end as usize > function.code.len()
            || entry.span.is_empty()
        {
            return Err(VerifyError {
                function: entry.function as usize,
                instruction: Some(entry.pc_start as usize),
                kind: VerifyErrorKind::InvalidSourceMap,
            });
        }
    }
    Ok(())
}

pub fn verify_reload_transition(
    old: &VerifiedModule,
    candidate: &VerifiedModule,
) -> Result<(), VerifyError> {
    let old_fingerprint = old.module().reload_metadata.state_schema_fingerprint;
    let candidate_metadata = candidate.module().reload_metadata;
    if old_fingerprint != candidate_metadata.state_schema_fingerprint
        && candidate_metadata.migration_entry.is_none()
    {
        return Err(VerifyError {
            function: 0,
            instruction: None,
            kind: VerifyErrorKind::InvalidReloadMetadata,
        });
    }
    Ok(())
}

fn verify_reload_metadata(module: &Module) -> Result<(), VerifyError> {
    let invalid = |function| VerifyError {
        function,
        instruction: None,
        kind: VerifyErrorKind::InvalidReloadMetadata,
    };
    let migration_entries = module
        .functions
        .iter()
        .enumerate()
        .filter(|(_, function)| function.effect == FunctionEffect::Migration)
        .map(|(index, _)| u32::try_from(index).expect("module function count exceeds u32"))
        .collect::<Vec<_>>();
    if migration_entries.len() > 1
        || module.reload_metadata.migration_entry != migration_entries.first().copied()
    {
        return Err(invalid(
            usize::try_from(module.reload_metadata.migration_entry.unwrap_or_default())
                .unwrap_or(usize::MAX),
        ));
    }
    if let Some(entry) = module.reload_metadata.activation_entry {
        let entry = usize::try_from(entry).unwrap_or(usize::MAX);
        let function = module.functions.get(entry).ok_or_else(|| invalid(entry))?;
        if function.effect != FunctionEffect::Immediate {
            return Err(invalid(entry));
        }
    }
    let expected_fingerprint = module.state_schema.fingerprint();
    if module.state_schema_fingerprint != expected_fingerprint
        || module.reload_metadata.state_schema_fingerprint != expected_fingerprint
    {
        return Err(invalid(0));
    }
    let required = minimum_migration_limits(module, module.reload_metadata.migration_entry);
    if !module
        .reload_metadata
        .minimum_migration_limits
        .satisfies(required)
    {
        return Err(invalid(
            usize::try_from(module.reload_metadata.migration_entry.unwrap_or_default())
                .unwrap_or(usize::MAX),
        ));
    }
    Ok(())
}

fn standard_intrinsic_metadata_is_complete(module: &Module, intrinsic: StandardIntrinsic) -> bool {
    let has_enum = |expected: nexa_bytecode::EnumType| module.enum_types.contains(&expected);
    let has_array = |element| {
        module
            .array_types
            .contains(&nexa_bytecode::ArrayType::new(element))
    };
    let has_map = |key, value| {
        module
            .map_types
            .contains(&nexa_bytecode::MapType::new(key, value))
    };
    match intrinsic {
        StandardIntrinsic::OptionIsSome { value }
        | StandardIntrinsic::OptionIsNone { value }
        | StandardIntrinsic::OptionUnwrapOr { value } => {
            has_enum(nexa_bytecode::option_type(value))
        }
        StandardIntrinsic::ResultIsOk { success, error }
        | StandardIntrinsic::ResultIsErr { success, error }
        | StandardIntrinsic::ResultUnwrapOr { success, error } => {
            has_enum(nexa_bytecode::result_type(success, error))
        }
        StandardIntrinsic::StringSplit => has_array(ValueType::String),
        StandardIntrinsic::ArrayGet { element } => {
            has_array(element) && has_enum(nexa_bytecode::option_type(element))
        }
        StandardIntrinsic::ArrayLen { element }
        | StandardIntrinsic::ArrayIsEmpty { element }
        | StandardIntrinsic::ArrayPush { element }
        | StandardIntrinsic::ArrayPop { element }
        | StandardIntrinsic::ArrayReserve { element }
        | StandardIntrinsic::ArrayCapacity { element }
        | StandardIntrinsic::ArrayClear { element }
        | StandardIntrinsic::ArrayShrinkToFit { element } => has_array(element),
        StandardIntrinsic::MapGet { key, value } | StandardIntrinsic::MapRemove { key, value } => {
            has_map(key, value) && has_enum(nexa_bytecode::option_type(value))
        }
        StandardIntrinsic::MapLen { key, value }
        | StandardIntrinsic::MapContains { key, value }
        | StandardIntrinsic::MapInsert { key, value } => has_map(key, value),
        _ => true,
    }
}

const fn standard_intrinsic_requires_heap(intrinsic: StandardIntrinsic) -> bool {
    !matches!(
        intrinsic,
        StandardIntrinsic::OptionIsSome { .. }
            | StandardIntrinsic::OptionIsNone { .. }
            | StandardIntrinsic::ResultIsOk { .. }
            | StandardIntrinsic::ResultIsErr { .. }
            | StandardIntrinsic::OptionUnwrapOr { .. }
            | StandardIntrinsic::ResultUnwrapOr { .. }
            | StandardIntrinsic::F32Floor
            | StandardIntrinsic::F64Floor
            | StandardIntrinsic::F32Ceil
            | StandardIntrinsic::F64Ceil
            | StandardIntrinsic::F32Round
            | StandardIntrinsic::F64Round
            | StandardIntrinsic::F32Sqrt
            | StandardIntrinsic::F64Sqrt
            | StandardIntrinsic::F32Sin
            | StandardIntrinsic::F64Sin
            | StandardIntrinsic::F32Cos
            | StandardIntrinsic::F64Cos
            | StandardIntrinsic::DebugAssert
    )
}

const fn string_instruction_requires_heap(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::LoadString { .. }
            | Instruction::StringLen { .. }
            | Instruction::StringByteLen { .. }
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
    )
}

const fn aggregate_instruction_requires_heap(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::StateHandleResolve { .. }
            | Instruction::ClassNew { .. }
            | Instruction::ClassGet { .. }
            | Instruction::ClassSet { .. }
            | Instruction::ClassEqual { .. }
    )
}

const fn collection_instruction_requires_heap(instruction: Instruction) -> bool {
    matches!(
        instruction,
        Instruction::ArrayNew { .. }
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
    )
}

const fn instruction_requires_heap(instruction: Instruction) -> bool {
    if let Instruction::StandardIntrinsic { intrinsic, .. } = instruction {
        standard_intrinsic_requires_heap(intrinsic)
    } else {
        string_instruction_requires_heap(instruction)
            || aggregate_instruction_requires_heap(instruction)
            || collection_instruction_requires_heap(instruction)
    }
}

fn resolve_state_field(
    module: &Module,
    type_id: StableId,
    field_id: StableId,
) -> Option<(usize, usize, usize, ValueType)> {
    let (type_index, state_type) = module
        .state_schema
        .types
        .iter()
        .enumerate()
        .find(|(_, state_type)| state_type.stable_id == type_id)?;
    let (field_index, field) = state_type
        .fields
        .iter()
        .enumerate()
        .find(|(_, field)| field.stable_id == field_id)?;
    let sorted_index = state_type
        .fields
        .iter()
        .filter(|candidate| candidate.stable_id < field_id)
        .count();
    Some((type_index, field_index, sorted_index, field.ty))
}

#[derive(Clone, Copy)]
struct PhysicalVerificationContext<'module> {
    layouts: &'module nexa_bytecode::layout::LayoutTable,
    module_abi: &'module nexa_bytecode::layout::ModuleAbi,
}

fn verify_physical_value_range(
    state: &[Option<ValueType>],
    layouts: &nexa_bytecode::layout::LayoutTable,
    base: u16,
    expected: ValueType,
) -> Result<u16, VerifyErrorKind> {
    let slots = layouts
        .layout_of(expected)
        .map_err(VerifyErrorKind::InvalidValueLayout)?
        .physical_slots;
    if slots == 0 {
        return Err(VerifyErrorKind::InvalidPhysicalAbi);
    }
    let end = base
        .checked_add(slots)
        .filter(|end| usize::from(*end) <= state.len())
        .ok_or(VerifyErrorKind::RegisterOutOfRange(base))?;
    if state.get(usize::from(base)).copied().flatten() != Some(expected) {
        return Err(VerifyErrorKind::TypeMismatch);
    }
    if (base.saturating_add(1)..end).any(|slot| state[usize::from(slot)].is_some()) {
        return Err(VerifyErrorKind::InvalidPhysicalAbi);
    }
    Ok(slots)
}

fn write_physical_value_range(
    state: &mut [Option<ValueType>],
    layouts: &nexa_bytecode::layout::LayoutTable,
    base: u16,
    ty: ValueType,
) -> Result<u16, VerifyErrorKind> {
    let slots = layouts
        .layout_of(ty)
        .map_err(VerifyErrorKind::InvalidValueLayout)?
        .physical_slots;
    if slots == 0 {
        return Err(VerifyErrorKind::InvalidPhysicalAbi);
    }
    let end = base
        .checked_add(slots)
        .filter(|end| usize::from(*end) <= state.len())
        .ok_or(VerifyErrorKind::RegisterOutOfRange(base))?;
    let destination_start = usize::from(base);
    let destination_end = usize::from(end);
    for (existing_base, existing) in state.iter().copied().enumerate() {
        let Some(existing) = existing else {
            continue;
        };
        let existing_slots = usize::from(
            layouts
                .layout_of(existing)
                .map_err(VerifyErrorKind::InvalidValueLayout)?
                .physical_slots,
        );
        let existing_end = existing_base
            .checked_add(existing_slots)
            .ok_or(VerifyErrorKind::InvalidPhysicalAbi)?;
        let overlaps = existing_base < destination_end && destination_start < existing_end;
        if overlaps && existing_base != destination_start {
            return Err(VerifyErrorKind::InvalidPhysicalAbi);
        }
    }
    state[destination_start..destination_end].fill(None);
    state[destination_start] = Some(ty);
    Ok(slots)
}

fn physical_field_offsets(
    fields: &[StructField],
    layouts: &nexa_bytecode::layout::LayoutTable,
) -> Result<(Vec<(u16, u16)>, u16), VerifyErrorKind> {
    let mut cursor = 0_u16;
    let mut offsets = Vec::with_capacity(fields.len());
    for field in fields {
        let slots = layouts
            .layout_of(field.ty)
            .map_err(VerifyErrorKind::InvalidValueLayout)?
            .physical_slots;
        if slots == 0 {
            return Err(VerifyErrorKind::InvalidPhysicalAbi);
        }
        offsets.push((cursor, slots));
        cursor = cursor
            .checked_add(slots)
            .ok_or(VerifyErrorKind::InvalidPhysicalAbi)?;
    }
    Ok((offsets, cursor))
}

fn array_row_slots(
    module: &Module,
    element: ValueType,
    layout: &nexa_bytecode::layout::ValueLayout,
) -> u16 {
    let ValueType::Named(type_id) = element else {
        return 0;
    };
    if layout.physical_slots != 0
        && module
            .struct_types
            .iter()
            .any(|struct_type| struct_type.type_id == type_id)
    {
        layout.physical_slots
    } else {
        0
    }
}

#[allow(clippy::too_many_lines)]
fn verify_function(
    module: &Module,
    physical: PhysicalVerificationContext<'_>,
    function_index: usize,
    function: &Function,
    limits: VerifierLimits,
    immediate_context: bool,
    restricted_context: bool,
) -> Result<Vec<ResolvedNominalOperand>, VerifyError> {
    let error = |instruction, kind| VerifyError {
        function: function_index,
        instruction,
        kind,
    };
    if function.code.is_empty() {
        return Err(error(None, VerifyErrorKind::EmptyFunction));
    }
    if function.frame_bytes > limits.max_frame_bytes {
        return Err(error(None, VerifyErrorKind::FrameLimit));
    }
    if function.root_bitmap.len() != usize::from(function.registers) {
        return Err(error(None, VerifyErrorKind::RootBitmapLength));
    }
    verify_loop_bounds(function_index, function, limits)?;
    let register_count = usize::from(function.registers);
    let function_abi = physical
        .module_abi
        .function(function_index)
        .ok_or_else(|| error(None, VerifyErrorKind::InvalidPhysicalAbi))?;
    if usize::from(function_abi.parameter_slots) > register_count {
        return Err(error(None, VerifyErrorKind::RegisterOutOfRange(u16::MAX)));
    }
    let mut entry = vec![None; register_count];
    for parameter in &function_abi.parameters {
        entry[usize::from(parameter.slot_offset)] = Some(parameter.logical_type);
    }
    let mut states = vec![None; function.code.len()];
    let mut resolved_operands = vec![ResolvedNominalOperand::None; function.code.len()];
    states[0] = Some(entry);
    let mut queue = VecDeque::from([0_usize]);
    while let Some(pc) = queue.pop_front() {
        let mut state = states[pc].clone().expect("queued state exists");
        let instruction = function.code[pc];
        let register = |value: u16| {
            if usize::from(value) < register_count {
                Ok(usize::from(value))
            } else {
                Err(error(Some(pc), VerifyErrorKind::RegisterOutOfRange(value)))
            }
        };
        let require = |state: &[Option<ValueType>], value: u16, ty: ValueType| {
            let index = register(value)?;
            if state[index] == Some(ty) {
                Ok(index)
            } else {
                Err(error(Some(pc), VerifyErrorKind::TypeMismatch))
            }
        };
        let array_layout = |state: &[Option<ValueType>], source: u16| {
            let source = register(source)?;
            let Some(ValueType::Named(type_id)) = state[source] else {
                return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
            };
            let (type_index, array_type) = module
                .array_types
                .iter()
                .enumerate()
                .find(|(_, array_type)| array_type.type_id == type_id)
                .ok_or_else(|| error(Some(pc), VerifyErrorKind::ArrayTypeOutOfRange(type_id.0)))?;
            let layout = physical
                .layouts
                .layout_of(array_type.element)
                .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
            let row_slots = array_row_slots(module, array_type.element, &layout);
            Ok((type_index, array_type.element, layout, row_slots))
        };
        let map_layout = |state: &[Option<ValueType>], source: u16| {
            let source = register(source)?;
            let Some(ValueType::Named(type_id)) = state[source] else {
                return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
            };
            let (type_index, map_type) = module
                .map_types
                .iter()
                .enumerate()
                .find(|(_, map_type)| map_type.type_id == type_id)
                .ok_or_else(|| error(Some(pc), VerifyErrorKind::MapTypeOutOfRange(type_id.0)))?;
            let key_layout = physical
                .layouts
                .layout_of(map_type.key)
                .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
            let value_layout = physical
                .layouts
                .layout_of(map_type.value)
                .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
            Ok((
                type_index,
                map_type.key,
                map_type.value,
                key_layout,
                value_layout,
            ))
        };
        let buffer_element = |state: &[Option<ValueType>], source: u16| {
            let source = register(source)?;
            let Some(ValueType::Named(type_id)) = state[source] else {
                return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
            };
            module
                .buffer_types
                .iter()
                .find(|buffer| buffer.type_id == type_id)
                .map(|buffer| buffer.element)
                .ok_or_else(|| error(Some(pc), VerifyErrorKind::BufferTypeOutOfRange(type_id.0)))
        };
        let mut successors = Vec::with_capacity(2);
        if restricted_context && instruction_requires_heap(instruction) {
            return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
        }
        if matches!(
            instruction,
            Instruction::ArrayNew { .. }
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
        ) && immediate_context
        {
            return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
        }
        if let Instruction::StandardIntrinsic { intrinsic, .. } = instruction
            && (intrinsic.mutates_collection() || intrinsic.fuel_model().is_variable())
            && immediate_context
        {
            return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
        }
        if matches!(
            instruction,
            Instruction::StringLen { .. }
                | Instruction::StringEqual { .. }
                | Instruction::StringConcat { .. }
                | Instruction::StringBuild { .. }
                | Instruction::StringRuneAt { .. }
                | Instruction::EnumEqual { .. }
                | Instruction::StructEqual { .. }
        ) && immediate_context
        {
            // These instructions depend on runtime-sized values. Until the
            // verifier is bound to a Realm resource profile, no finite
            // Immediate-effect WCET can be proven.
            return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
        }
        match instruction {
            Instruction::LoadI32 { dst, .. } => state[register(dst)?] = Some(ValueType::I32),
            Instruction::LoadI64 { dst, .. } => state[register(dst)?] = Some(ValueType::I64),
            Instruction::LoadF32 { dst, .. } => state[register(dst)?] = Some(ValueType::F32),
            Instruction::LoadF64 { dst, .. } => state[register(dst)?] = Some(ValueType::F64),
            Instruction::LoadRune { dst, value } => {
                if char::from_u32(value).is_none() {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidRune(value)));
                }
                state[register(dst)?] = Some(ValueType::Rune);
            }
            Instruction::LoadString { dst, string } => {
                if string as usize >= module.strings.len() {
                    return Err(error(Some(pc), VerifyErrorKind::StringOutOfRange(string)));
                }
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::LoadBool { dst, .. } => state[register(dst)?] = Some(ValueType::Bool),
            Instruction::Move { dst, source } => {
                let source = register(source)?;
                let ty =
                    state[source].ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                state[register(dst)?] = Some(ty);
            }
            Instruction::CopyValue { dst, source, slots } => {
                let source_index = register(source)?;
                let ty = state[source_index]
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                let source_slots =
                    verify_physical_value_range(&state, physical.layouts, source, ty)
                        .map_err(|kind| error(Some(pc), kind))?;
                if slots != source_slots || slots < 2 {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                let destination_slots =
                    write_physical_value_range(&mut state, physical.layouts, dst, ty)
                        .map_err(|kind| error(Some(pc), kind))?;
                if destination_slots != slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::Add { dst, lhs, rhs }
            | Instruction::Sub { dst, lhs, rhs }
            | Instruction::Mul { dst, lhs, rhs }
            | Instruction::Div { dst, lhs, rhs }
            | Instruction::RemI32 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I32)?;
                require(&state, rhs, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::AddI64 { dst, lhs, rhs }
            | Instruction::SubI64 { dst, lhs, rhs }
            | Instruction::MulI64 { dst, lhs, rhs }
            | Instruction::DivI64 { dst, lhs, rhs }
            | Instruction::RemI64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I64)?;
                require(&state, rhs, ValueType::I64)?;
                state[register(dst)?] = Some(ValueType::I64);
            }
            Instruction::AddF32 { dst, lhs, rhs }
            | Instruction::SubF32 { dst, lhs, rhs }
            | Instruction::MulF32 { dst, lhs, rhs }
            | Instruction::DivF32 { dst, lhs, rhs }
            | Instruction::RemF32 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F32)?;
                require(&state, rhs, ValueType::F32)?;
                state[register(dst)?] = Some(ValueType::F32);
            }
            Instruction::AddF64 { dst, lhs, rhs }
            | Instruction::SubF64 { dst, lhs, rhs }
            | Instruction::MulF64 { dst, lhs, rhs }
            | Instruction::DivF64 { dst, lhs, rhs }
            | Instruction::RemF64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F64)?;
                require(&state, rhs, ValueType::F64)?;
                state[register(dst)?] = Some(ValueType::F64);
            }
            Instruction::StringLen { dst, source } | Instruction::StringByteLen { dst, source } => {
                require(&state, source, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::StringEqual { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::String)?;
                require(&state, rhs, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::StringConcat { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::String)?;
                require(&state, rhs, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::StringBuild {
                dst,
                parts_base,
                parts_count,
            } => {
                let parts_end = parts_base.checked_add(parts_count).ok_or_else(|| {
                    error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                })?;
                for source in parts_base..parts_end {
                    let Some(ty) = state[register(source)?] else {
                        return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                    };
                    if !matches!(
                        ty,
                        ValueType::I32
                            | ValueType::I64
                            | ValueType::F32
                            | ValueType::F64
                            | ValueType::Bool
                            | ValueType::Rune
                            | ValueType::String
                    ) {
                        return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                    }
                }
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::StringRuneAt { dst, source, index } => {
                require(&state, source, ValueType::String)?;
                require(&state, index, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::Rune);
            }
            Instruction::StringHash { dst, source } => {
                require(&state, source, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::I64);
            }
            Instruction::I32ToString { dst, source } => {
                require(&state, source, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::I64ToString { dst, source } => {
                require(&state, source, ValueType::I64)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::F32ToString { dst, source } => {
                require(&state, source, ValueType::F32)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::F64ToString { dst, source } => {
                require(&state, source, ValueType::F64)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::BoolToString { dst, source } => {
                require(&state, source, ValueType::Bool)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::RuneToString { dst, source } => {
                require(&state, source, ValueType::Rune)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::StringToString { dst, source } => {
                require(&state, source, ValueType::String)?;
                state[register(dst)?] = Some(ValueType::String);
            }
            Instruction::StandardIntrinsic {
                intrinsic,
                args_base,
                args_count,
                dst,
            } => {
                if !standard_intrinsic_metadata_is_complete(module, intrinsic) {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                let mut argument_slots = [0_u16; 3];
                let mut packed_slots = 0_u16;
                for argument in 0..intrinsic.argument_count() {
                    let ty = intrinsic
                        .argument_type(argument)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                    let register_index = args_base.checked_add(packed_slots).ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                    })?;
                    let slots =
                        verify_physical_value_range(&state, physical.layouts, register_index, ty)
                            .map_err(|kind| error(Some(pc), kind))?;
                    argument_slots[usize::from(argument)] = slots;
                    packed_slots = packed_slots
                        .checked_add(slots)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                }
                if args_count != packed_slots {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }

                let input_payload_offset = match intrinsic {
                    StandardIntrinsic::OptionIsSome { .. }
                    | StandardIntrinsic::OptionIsNone { .. }
                    | StandardIntrinsic::ResultIsOk { .. }
                    | StandardIntrinsic::ResultIsErr { .. }
                    | StandardIntrinsic::OptionUnwrapOr { .. }
                    | StandardIntrinsic::ResultUnwrapOr { .. } => {
                        let input = intrinsic
                            .argument_type(0)
                            .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                        let layout = physical.layouts.layout_of(input).map_err(|kind| {
                            error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind))
                        })?;
                        let enum_layout = layout
                            .enum_layout
                            .as_ref()
                            .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                        if enum_layout.tag_offset != 0 || enum_layout.payload_offset != 1 {
                            return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                        }
                        enum_layout.payload_offset
                    }
                    _ => 0,
                };

                let result_type = intrinsic.result_type();
                let result_layout = physical
                    .layouts
                    .layout_of(result_type)
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
                let result_payload_offset = match intrinsic {
                    StandardIntrinsic::ArrayGet { .. }
                    | StandardIntrinsic::MapGet { .. }
                    | StandardIntrinsic::MapRemove { .. } => {
                        let enum_layout = result_layout
                            .enum_layout
                            .as_ref()
                            .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                        if enum_layout.tag_offset != 0 || enum_layout.payload_offset != 1 {
                            return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                        }
                        enum_layout.payload_offset
                    }
                    _ => 0,
                };
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, result_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != result_layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::StandardIntrinsic {
                    argument_slots,
                    result_slots: result_layout.physical_slots,
                    input_payload_offset,
                    result_payload_offset,
                };
            }
            Instruction::CompareEq { dst, lhs, rhs } => {
                let lhs = register(lhs)?;
                let rhs = register(rhs)?;
                if state[lhs].is_none() || state[lhs] != state[rhs] {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::CompareLtI32 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I32)?;
                require(&state, rhs, ValueType::I32)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::CompareLtI64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::I64)?;
                require(&state, rhs, ValueType::I64)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::CompareLtF32 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F32)?;
                require(&state, rhs, ValueType::F32)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::CompareLtF64 { dst, lhs, rhs } => {
                require(&state, lhs, ValueType::F64)?;
                require(&state, rhs, ValueType::F64)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::Jump { target } => {
                successors.push(target_index(function, function_index, pc, target)?);
            }
            Instruction::JumpIfFalse { condition, target } => {
                require(&state, condition, ValueType::Bool)?;
                successors.push(target_index(function, function_index, pc, target)?);
                if pc + 1 < function.code.len() {
                    successors.push(pc + 1);
                }
            }
            Instruction::Call {
                function: callee,
                args_base,
                args_count,
                dst,
            } => {
                let callee_index = callee as usize;
                let callee = module
                    .functions
                    .get(callee_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::FunctionOutOfRange(callee)))?;
                let callee_abi = physical
                    .module_abi
                    .function(callee_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                if (immediate_context
                    && !matches!(
                        callee.effect,
                        FunctionEffect::Ordinary | FunctionEffect::Immediate
                    ))
                    || (callee.effect == FunctionEffect::Task
                        && function.effect != FunctionEffect::Task)
                    || (function.effect == FunctionEffect::Migration
                        && !matches!(
                            callee.effect,
                            FunctionEffect::Ordinary | FunctionEffect::Migration
                        ))
                    || (callee.effect == FunctionEffect::Migration
                        && function.effect != FunctionEffect::Migration)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                if args_count != callee_abi.parameter_slots {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for parameter in &callee_abi.parameters {
                    let argument =
                        args_base
                            .checked_add(parameter.slot_offset)
                            .ok_or_else(|| {
                                error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                            })?;
                    let slots = verify_physical_value_range(
                        &state,
                        physical.layouts,
                        argument,
                        parameter.logical_type,
                    )
                    .map_err(|kind| error(Some(pc), kind))?;
                    if slots != parameter.slot_count {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
                resolved_operands[pc] = ResolvedNominalOperand::CallFrame {
                    register_count: callee.registers,
                    parameter_slots: callee_abi.parameter_slots,
                    result_slots: callee_abi
                        .result
                        .as_ref()
                        .map_or(0, |result| result.slot_count),
                };
                if let Some(result) = callee.signature.result {
                    let result_slots =
                        callee_abi
                            .result
                            .as_ref()
                            .map(|result| result.slot_count)
                            .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                    let written =
                        write_physical_value_range(&mut state, physical.layouts, dst, result)
                            .map_err(|kind| error(Some(pc), kind))?;
                    if written != result_slots {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
            }
            Instruction::HostCall {
                import,
                args_base,
                args_count,
                dst,
            } => {
                let host = module.host_imports.get(import as usize).ok_or_else(|| {
                    error(Some(pc), VerifyErrorKind::HostImportOutOfRange(import))
                })?;
                if restricted_context
                    || (host.mode == HostCallMode::Async && function.effect != FunctionEffect::Task)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let mut packed_slots = 0_u16;
                for ty in host.parameters.iter().copied() {
                    let argument = args_base.checked_add(packed_slots).ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                    })?;
                    let slots = verify_physical_value_range(&state, physical.layouts, argument, ty)
                        .map_err(|kind| error(Some(pc), kind))?;
                    packed_slots = packed_slots
                        .checked_add(slots)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                }
                if args_count != packed_slots {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                if let Some(result) = host.result {
                    write_physical_value_range(&mut state, physical.layouts, dst, result)
                        .map_err(|kind| error(Some(pc), kind))?;
                }
            }
            Instruction::StateOldGet { ty, dst, .. } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                state[register(dst)?] = Some(ty);
            }
            Instruction::StateCurrentGet { type_id, dst, .. } => {
                if !matches!(
                    function.effect,
                    FunctionEffect::Ordinary | FunctionEffect::Task
                ) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                if !module
                    .state_schema
                    .types
                    .iter()
                    .any(|state_type| state_type.stable_id == type_id)
                    || !module
                        .class_types
                        .iter()
                        .any(|class_type| class_type.type_id == type_id)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::StateOldFieldGet {
                object,
                field_id,
                ty,
                dst,
            } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let object = register(object)?;
                let Some(ValueType::Named(type_id)) = state[object] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (type_index, field_index, sorted_index, field_type) =
                    resolve_state_field(module, type_id, field_id)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                if field_type != ty {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                resolved_operands[pc] = ResolvedNominalOperand::StateField {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    field_index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    sorted_index: u16::try_from(sorted_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                };
                state[register(dst)?] = Some(ty);
            }
            Instruction::StateHandleResolve {
                handle,
                target,
                result_type,
                dst,
            } => {
                if restricted_context || !has_state_handle_type(module, target) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                let expected_result = nexa_bytecode::result_type(
                    target,
                    ValueType::Named(nexa_bytecode::state_handle_error_type().type_id),
                );
                if result_type != expected_result.type_id
                    || !module
                        .enum_types
                        .iter()
                        .any(|enum_type| enum_type == &expected_result)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                state[register(dst)?] = Some(ValueType::Named(result_type));
            }
            Instruction::StateHandleIsAlive {
                handle,
                target,
                dst,
            }
            | Instruction::StateHandleGeneration {
                handle,
                target,
                dst,
            }
            | Instruction::StateHandleHash {
                handle,
                target,
                dst,
            } => {
                if restricted_context || !has_state_handle_type(module, target) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                state[register(dst)?] = Some(
                    if matches!(instruction, Instruction::StateHandleIsAlive { .. }) {
                        ValueType::Bool
                    } else {
                        ValueType::I32
                    },
                );
            }
            Instruction::StateHandleStableId {
                handle,
                target,
                dst,
            } => {
                if restricted_context || !has_state_handle_type(module, target) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                require(
                    &state,
                    handle,
                    ValueType::Named(nexa_bytecode::state_handle_type(target)),
                )?;
                state[register(dst)?] = Some(nexa_bytecode::stable_id_type());
            }
            Instruction::StateHandleEqual {
                lhs,
                rhs,
                target,
                dst,
            } => {
                if restricted_context || !has_state_handle_type(module, target) {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let handle_type = ValueType::Named(nexa_bytecode::state_handle_type(target));
                require(&state, lhs, handle_type)?;
                require(&state, rhs, handle_type)?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::StateNewCreate { type_id, dst, .. } => {
                if function.effect != FunctionEffect::Migration
                    || !module
                        .state_schema
                        .types
                        .iter()
                        .any(|state_type| state_type.stable_id == type_id)
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::StateNewSet {
                object,
                field_id,
                source,
            } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let object = register(object)?;
                let Some(ValueType::Named(type_id)) = state[object] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (type_index, field_index, sorted_index, field_type) =
                    resolve_state_field(module, type_id, field_id)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                resolved_operands[pc] = ResolvedNominalOperand::StateField {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    field_index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    sorted_index: u16::try_from(sorted_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                };
                require(&state, source, field_type)?;
            }
            Instruction::StateReplace { target, .. } => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                let target = register(target)?;
                let Some(ValueType::Named(type_id)) = state[target] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .state_schema
                    .types
                    .iter()
                    .any(|state_type| state_type.stable_id == type_id)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
            }
            Instruction::StateDelete { .. }
            | Instruction::StatePreserve { .. }
            | Instruction::StateFinish => {
                if function.effect != FunctionEffect::Migration {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
            }
            Instruction::EnumNew {
                type_id,
                variant,
                payload,
                dst,
            } => {
                let (type_index, enum_type) = module
                    .enum_types
                    .iter()
                    .enumerate()
                    .find(|(_, enum_type)| enum_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumTypeOutOfRange(type_id.0))
                    })?;
                let (variant_index, variant) = enum_type
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, candidate)| candidate.stable_id == variant)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumVariantOutOfRange(variant.0))
                    })?;
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
                let enum_layout = layout
                    .enum_layout
                    .as_ref()
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let variant_layout = enum_layout
                    .variants
                    .get(variant_index)
                    .filter(|candidate| {
                        candidate.stable_id == variant.stable_id && candidate.tag == variant.tag
                    })
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                match (variant.payload_type, payload) {
                    (Some(expected), Some(payload)) => {
                        let payload_slots = verify_physical_value_range(
                            &state,
                            physical.layouts,
                            payload,
                            expected,
                        )
                        .map_err(|kind| error(Some(pc), kind))?;
                        if payload_slots != variant_layout.payload_slots {
                            return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                        }
                        let payload_end = payload.checked_add(payload_slots).ok_or_else(|| {
                            error(Some(pc), VerifyErrorKind::RegisterOutOfRange(payload))
                        })?;
                        let destination_end =
                            dst.checked_add(layout.physical_slots).ok_or_else(|| {
                                error(Some(pc), VerifyErrorKind::RegisterOutOfRange(dst))
                            })?;
                        if payload < destination_end && dst < payload_end {
                            return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                        }
                    }
                    (None, None) if variant_layout.payload_slots == 0 => {}
                    _ => return Err(error(Some(pc), VerifyErrorKind::TypeMismatch)),
                }
                resolved_operands[pc] = ResolvedNominalOperand::EnumVariant {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    variant_index: u16::try_from(variant_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    tag: variant.tag,
                    payload_offset: enum_layout.payload_offset,
                    payload_slots: variant_layout.payload_slots,
                    owner_slots: layout.physical_slots,
                };
                write_physical_value_range(
                    &mut state,
                    physical.layouts,
                    dst,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
            }
            Instruction::EnumTag { source, dst } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let type_index = module
                    .enum_types
                    .iter()
                    .position(|enum_type| enum_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumTypeOutOfRange(type_id.0))
                    })?;
                let slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    source,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                resolved_operands[pc] = ResolvedNominalOperand::EnumLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?,
                    slots,
                };
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::EnumPayload {
                source,
                variant,
                dst,
            } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (type_index, enum_type) = module
                    .enum_types
                    .iter()
                    .enumerate()
                    .find(|(_, enum_type)| enum_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumTypeOutOfRange(type_id.0))
                    })?;
                let (variant_index, variant) = enum_type
                    .variants
                    .iter()
                    .enumerate()
                    .find(|(_, candidate)| candidate.stable_id == variant)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumVariantOutOfRange(variant.0))
                    })?;
                let payload_type = variant.payload_type.ok_or_else(|| {
                    error(
                        Some(pc),
                        VerifyErrorKind::EnumVariantOutOfRange(variant.stable_id.0),
                    )
                })?;
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
                let enum_layout = layout
                    .enum_layout
                    .as_ref()
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let variant_layout = enum_layout
                    .variants
                    .get(variant_index)
                    .filter(|candidate| candidate.stable_id == variant.stable_id)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                verify_physical_value_range(
                    &state,
                    physical.layouts,
                    source,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, payload_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != variant_layout.payload_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::EnumVariant {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?,
                    variant_index: u16::try_from(variant_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?,
                    tag: variant.tag,
                    payload_offset: enum_layout.payload_offset,
                    payload_slots: variant_layout.payload_slots,
                    owner_slots: layout.physical_slots,
                };
            }
            Instruction::EnumEqual { lhs, rhs, dst } => {
                let lhs_index = register(lhs)?;
                let Some(ValueType::Named(type_id)) = state[lhs_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let type_index = module
                    .enum_types
                    .iter()
                    .position(|enum_type| enum_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::EnumTypeOutOfRange(type_id.0))
                    })?;
                let lhs_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    lhs,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                let rhs_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    rhs,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if lhs_slots != rhs_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::EnumLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?,
                    slots: lhs_slots,
                };
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::StructNew {
                type_id,
                fields_base,
                fields_count,
                dst,
            } => {
                let struct_type = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructTypeOutOfRange(type_id.0))
                    })?;
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                if fields_count != layout.physical_slots
                    || fields_base
                        .checked_add(fields_count)
                        .is_none_or(|end| end > function.registers)
                    || dst
                        .checked_add(layout.physical_slots)
                        .is_none_or(|end| end > function.registers)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (field, placement) in struct_type.fields.iter().zip(&layout.field_offsets) {
                    let source = fields_base.checked_add(placement.offset).ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                    })?;
                    let slots =
                        verify_physical_value_range(&state, physical.layouts, source, field.ty)
                            .map_err(|kind| error(Some(pc), kind))?;
                    if slots != placement.slots {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
                resolved_operands[pc] = ResolvedNominalOperand::StructLayout {
                    type_id,
                    slots: layout.physical_slots,
                };
                let written = write_physical_value_range(
                    &mut state,
                    physical.layouts,
                    dst,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if written != fields_count {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::StructGet { source, field, dst } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (field_index, field_type) = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| {
                        struct_type
                            .fields
                            .iter()
                            .enumerate()
                            .find(|(_, candidate)| candidate.stable_id == field)
                    })
                    .map(|(index, field)| (index, field.ty))
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructFieldOutOfRange(field.0))
                    })?;
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let placement = layout
                    .field_offsets
                    .get(field_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let source_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    source,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if source_slots != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::StructField {
                    type_id,
                    index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    offset: placement.offset,
                    slots: placement.slots,
                    owner_slots: layout.physical_slots,
                };
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, field_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != placement.slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::StructWith {
                source,
                field,
                value,
                dst,
            } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (field_index, field_type) = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| {
                        struct_type
                            .fields
                            .iter()
                            .enumerate()
                            .find(|(_, candidate)| candidate.stable_id == field)
                    })
                    .map(|(index, field)| (index, field.ty))
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::StructFieldOutOfRange(field.0))
                    })?;
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let placement = layout
                    .field_offsets
                    .get(field_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let source_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    source,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                let value_slots =
                    verify_physical_value_range(&state, physical.layouts, value, field_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if source_slots != layout.physical_slots || value_slots != placement.slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                let value_end = value + value_slots;
                let destination_end = dst
                    .checked_add(layout.physical_slots)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::RegisterOutOfRange(dst)))?;
                if value < destination_end && dst < value_end {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::StructField {
                    type_id,
                    index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    offset: placement.offset,
                    slots: placement.slots,
                    owner_slots: layout.physical_slots,
                };
                let written = write_physical_value_range(
                    &mut state,
                    physical.layouts,
                    dst,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if written != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::StructEqual { lhs, rhs, dst } => {
                let lhs_index = register(lhs)?;
                let Some(ValueType::Named(type_id)) = state[lhs_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .struct_types
                    .iter()
                    .any(|struct_type| struct_type.type_id == type_id)
                {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::StructTypeOutOfRange(type_id.0),
                    ));
                }
                let layout = physical
                    .layouts
                    .layout_of(ValueType::Named(type_id))
                    .map_err(|_| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let lhs_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    lhs,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                let rhs_slots = verify_physical_value_range(
                    &state,
                    physical.layouts,
                    rhs,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if lhs_slots != layout.physical_slots || rhs_slots != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::StructLayout {
                    type_id,
                    slots: layout.physical_slots,
                };
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::ClassNew {
                type_id,
                fields_base,
                fields_count,
                dst,
            } => {
                let class_type = module
                    .class_types
                    .iter()
                    .find(|class_type| class_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassTypeOutOfRange(type_id.0))
                    })?;
                let (offsets, owner_slots) =
                    physical_field_offsets(&class_type.fields, physical.layouts)
                        .map_err(|kind| error(Some(pc), kind))?;
                if fields_count != owner_slots
                    || fields_base
                        .checked_add(fields_count)
                        .is_none_or(|end| end > function.registers)
                {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (field, (offset, slots)) in class_type.fields.iter().zip(offsets) {
                    let source = fields_base.checked_add(offset).ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                    })?;
                    let actual =
                        verify_physical_value_range(&state, physical.layouts, source, field.ty)
                            .map_err(|kind| error(Some(pc), kind))?;
                    if actual != slots {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
                write_physical_value_range(
                    &mut state,
                    physical.layouts,
                    dst,
                    ValueType::Named(type_id),
                )
                .map_err(|kind| error(Some(pc), kind))?;
            }
            Instruction::ClassGet { source, field, dst } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (type_index, class_type) = module
                    .class_types
                    .iter()
                    .enumerate()
                    .find(|(_, class_type)| class_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                let field_index = class_type
                    .fields
                    .iter()
                    .position(|candidate| candidate.stable_id == field)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                let field_type = class_type.fields[field_index].ty;
                let (offsets, owner_slots) =
                    physical_field_offsets(&class_type.fields, physical.layouts)
                        .map_err(|kind| error(Some(pc), kind))?;
                let (offset, slots) = offsets[field_index];
                let state_index = resolve_state_field(module, type_id, field)
                    .map(|(_, _, sorted_index, _)| {
                        u16::try_from(sorted_index)
                            .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))
                    })
                    .transpose()?;
                resolved_operands[pc] = ResolvedNominalOperand::ClassField {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    offset,
                    slots,
                    owner_slots,
                    state_index,
                };
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, field_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::ClassSet {
                source,
                field,
                value,
            } => {
                let source_index = register(source)?;
                let Some(ValueType::Named(type_id)) = state[source_index] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (type_index, class_type) = module
                    .class_types
                    .iter()
                    .enumerate()
                    .find(|(_, class_type)| class_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                let field_index = class_type
                    .fields
                    .iter()
                    .position(|candidate| candidate.stable_id == field)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ClassFieldOutOfRange(field.0))
                    })?;
                let field_type = class_type.fields[field_index].ty;
                let (offsets, owner_slots) =
                    physical_field_offsets(&class_type.fields, physical.layouts)
                        .map_err(|kind| error(Some(pc), kind))?;
                let (offset, slots) = offsets[field_index];
                let state_index = resolve_state_field(module, type_id, field)
                    .map(|(_, _, sorted_index, _)| {
                        u16::try_from(sorted_index)
                            .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))
                    })
                    .transpose()?;
                resolved_operands[pc] = ResolvedNominalOperand::ClassField {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    index: u16::try_from(field_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    offset,
                    slots,
                    owner_slots,
                    state_index,
                };
                let actual =
                    verify_physical_value_range(&state, physical.layouts, value, field_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if actual != slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::ClassEqual { lhs, rhs, dst } => {
                let lhs = register(lhs)?;
                let Some(ValueType::Named(type_id)) = state[lhs] else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                if !module
                    .class_types
                    .iter()
                    .any(|class_type| class_type.type_id == type_id)
                {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::ClassTypeOutOfRange(type_id.0),
                    ));
                }
                require(&state, rhs, ValueType::Named(type_id))?;
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::ArrayNew { type_id, dst } => {
                let (type_index, array_type) = module
                    .array_types
                    .iter()
                    .enumerate()
                    .find(|(_, array_type)| array_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::ArrayTypeOutOfRange(type_id.0))
                    })?;
                let layout = physical
                    .layouts
                    .layout_of(array_type.element)
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
                let row_slots = array_row_slots(module, array_type.element, &layout);
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::ArrayLen { source, dst } => {
                array_layout(&state, source)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::ArrayGet { source, index, dst }
            | Instruction::ArrayRemove { source, index, dst } => {
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                require(&state, index, ValueType::I32)?;
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, element)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::ArrayFieldGet {
                source,
                index,
                field,
                dst,
            } => {
                // WP52: one struct-element field projected without
                // materializing the element; the field operand is the
                // positional index inside the declared field order.
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                require(&state, index, ValueType::I32)?;
                let ValueType::Named(type_id) = element else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let (field_type, placement) = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .and_then(|struct_type| struct_type.fields.get(usize::from(field)))
                    .zip(layout.field_offsets.get(usize::from(field)))
                    .map(|(field, placement)| (field.ty, placement))
                    .ok_or_else(|| {
                        error(
                            Some(pc),
                            VerifyErrorKind::StructFieldOutOfRange(u64::from(field)),
                        )
                    })?;
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, field_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != placement.slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayField {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    offset: placement.offset,
                    slots: placement.slots,
                    row_slots,
                };
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
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                require(&state, index, ValueType::I32)?;
                let slots = verify_physical_value_range(&state, physical.layouts, value, element)
                    .map_err(|kind| error(Some(pc), kind))?;
                if slots != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::ArrayPush { source, value } => {
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                let slots = verify_physical_value_range(&state, physical.layouts, value, element)
                    .map_err(|kind| error(Some(pc), kind))?;
                if slots != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::ArrayPushRow {
                source,
                fields_base,
                fields_count,
            } => {
                // WP52 push-side fusion: the register range carries the
                // element struct's flattened physical fields, in order.
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                let ValueType::Named(type_id) = element else {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                };
                let struct_type = module
                    .struct_types
                    .iter()
                    .find(|struct_type| struct_type.type_id == type_id)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                if row_slots == 0 || fields_count != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                for (field, placement) in struct_type.fields.iter().zip(&layout.field_offsets) {
                    let register = fields_base
                        .checked_add(placement.offset)
                        .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                    let slots =
                        verify_physical_value_range(&state, physical.layouts, register, field.ty)
                            .map_err(|kind| error(Some(pc), kind))?;
                    if slots != placement.slots {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::ArrayPop { source, dst } => {
                let (type_index, element, layout, row_slots) = array_layout(&state, source)?;
                let written =
                    write_physical_value_range(&mut state, physical.layouts, dst, element)
                        .map_err(|kind| error(Some(pc), kind))?;
                if written != layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::ArrayClear { source } => {
                let (type_index, _element, layout, row_slots) = array_layout(&state, source)?;
                resolved_operands[pc] = ResolvedNominalOperand::ArrayLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    element_slots: layout.physical_slots,
                    row_slots,
                };
            }
            Instruction::MapNew { type_id, dst } => {
                let (type_index, map_type) = module
                    .map_types
                    .iter()
                    .enumerate()
                    .find(|(_, map_type)| map_type.type_id == type_id)
                    .ok_or_else(|| {
                        error(Some(pc), VerifyErrorKind::MapTypeOutOfRange(type_id.0))
                    })?;
                let key_slots = physical
                    .layouts
                    .layout_of(map_type.key)
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?
                    .physical_slots;
                let value_slots = physical
                    .layouts
                    .layout_of(map_type.value)
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?
                    .physical_slots;
                resolved_operands[pc] = ResolvedNominalOperand::MapLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    key_slots,
                    value_slots,
                    option_slots: 0,
                    option_payload_offset: 0,
                };
                state[register(dst)?] = Some(ValueType::Named(type_id));
            }
            Instruction::MapLen { source, dst } => {
                map_layout(&state, source)?;
                state[register(dst)?] = Some(ValueType::I32);
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
                let (type_index, key_type, value_type, key_layout, value_layout) =
                    map_layout(&state, source)?;
                let key_slots =
                    verify_physical_value_range(&state, physical.layouts, key, key_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if key_slots != key_layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                let option = nexa_bytecode::option_type(value_type);
                if result_type != option.type_id || !module.enum_types.contains(&option) {
                    return Err(error(
                        Some(pc),
                        VerifyErrorKind::EnumTypeOutOfRange(result_type.0),
                    ));
                }
                let option_layout = physical
                    .layouts
                    .layout_of(ValueType::Named(result_type))
                    .map_err(|kind| error(Some(pc), VerifyErrorKind::InvalidValueLayout(kind)))?;
                let enum_layout = option_layout
                    .enum_layout
                    .as_ref()
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let some = enum_layout
                    .variants
                    .get(1)
                    .filter(|variant| variant.payload_slots == value_layout.physical_slots)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let written = write_physical_value_range(
                    &mut state,
                    physical.layouts,
                    dst,
                    ValueType::Named(result_type),
                )
                .map_err(|kind| error(Some(pc), kind))?;
                if written != option_layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::MapLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    key_slots,
                    value_slots: some.payload_slots,
                    option_slots: option_layout.physical_slots,
                    option_payload_offset: enum_layout.payload_offset,
                };
            }
            Instruction::MapSet { source, key, value } => {
                let (type_index, key_type, value_type, key_layout, value_layout) =
                    map_layout(&state, source)?;
                let key_slots =
                    verify_physical_value_range(&state, physical.layouts, key, key_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                let value_slots =
                    verify_physical_value_range(&state, physical.layouts, value, value_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if key_slots != key_layout.physical_slots
                    || value_slots != value_layout.physical_slots
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::MapLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    key_slots,
                    value_slots,
                    option_slots: 0,
                    option_payload_offset: 0,
                };
            }
            Instruction::MapContains { source, key, dst } => {
                let (type_index, key_type, _value_type, key_layout, value_layout) =
                    map_layout(&state, source)?;
                let key_slots =
                    verify_physical_value_range(&state, physical.layouts, key, key_type)
                        .map_err(|kind| error(Some(pc), kind))?;
                if key_slots != key_layout.physical_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
                resolved_operands[pc] = ResolvedNominalOperand::MapLayout {
                    type_index: u16::try_from(type_index)
                        .map_err(|_| error(Some(pc), VerifyErrorKind::TypeMismatch))?,
                    key_slots,
                    value_slots: value_layout.physical_slots,
                    option_slots: 0,
                    option_payload_offset: 0,
                };
                state[register(dst)?] = Some(ValueType::Bool);
            }
            Instruction::MapClear { source } => {
                map_layout(&state, source)?;
            }
            Instruction::BufferLen { source, dst } => {
                buffer_element(&state, source)?;
                state[register(dst)?] = Some(ValueType::I32);
            }
            Instruction::BufferGet { source, index, dst } => {
                let element = buffer_element(&state, source)?;
                require(&state, index, ValueType::I32)?;
                state[register(dst)?] = Some(element);
            }
            Instruction::BufferSet {
                source,
                index,
                value,
            } => {
                let element = buffer_element(&state, source)?;
                require(&state, index, ValueType::I32)?;
                require(&state, value, element)?;
            }
            Instruction::BufferSlice {
                source,
                start,
                length,
                dst,
            } => {
                buffer_element(&state, source)?;
                require(&state, start, ValueType::I32)?;
                require(&state, length, ValueType::I32)?;
                let source = register(source)?;
                state[register(dst)?] = state[source];
            }
            Instruction::BufferCopy {
                destination,
                source,
                source_start,
                destination_start,
                length,
            } => {
                let element = buffer_element(&state, destination)?;
                let source_element = buffer_element(&state, source)?;
                if element != source_element {
                    return Err(error(Some(pc), VerifyErrorKind::TypeMismatch));
                }
                require(&state, source_start, ValueType::I32)?;
                require(&state, destination_start, ValueType::I32)?;
                require(&state, length, ValueType::I32)?;
            }
            Instruction::Return { source } => {
                let result = function
                    .signature
                    .result
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidReturn))?;
                let result_slots = function_abi
                    .result
                    .as_ref()
                    .map(|result| result.slot_count)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi))?;
                let source_slots =
                    verify_physical_value_range(&state, physical.layouts, source, result)
                        .map_err(|kind| error(Some(pc), kind))?;
                if source_slots != result_slots {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                }
            }
            Instruction::ReturnVoid => {
                if function.signature.result.is_some() {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidReturn));
                }
            }
            Instruction::DeferPush {
                function: cleanup,
                args_base,
                args_count,
            } => {
                let cleanup_index = cleanup as usize;
                let cleanup = module
                    .functions
                    .get(cleanup_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::FunctionOutOfRange(cleanup)))?;
                let cleanup_abi = physical
                    .module_abi
                    .function(cleanup_index)
                    .ok_or_else(|| error(Some(pc), VerifyErrorKind::TypeMismatch))?;
                if !matches!(
                    cleanup.effect,
                    FunctionEffect::Ordinary | FunctionEffect::Cleanup
                ) || args_count != cleanup_abi.parameter_slots
                    || args_count > 8
                {
                    return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
                }
                for parameter in &cleanup_abi.parameters {
                    let argument =
                        args_base
                            .checked_add(parameter.slot_offset)
                            .ok_or_else(|| {
                                error(Some(pc), VerifyErrorKind::RegisterOutOfRange(u16::MAX))
                            })?;
                    let slots = verify_physical_value_range(
                        &state,
                        physical.layouts,
                        argument,
                        parameter.logical_type,
                    )
                    .map_err(|kind| error(Some(pc), kind))?;
                    if slots != parameter.slot_count {
                        return Err(error(Some(pc), VerifyErrorKind::InvalidPhysicalAbi));
                    }
                }
            }
            Instruction::Yield if !matches!(function.effect, FunctionEffect::Task) => {
                return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
            }
            Instruction::CleanupReturn if function.effect != FunctionEffect::Cleanup => {
                return Err(error(Some(pc), VerifyErrorKind::InvalidEffect));
            }
            Instruction::Safepoint
            | Instruction::Yield
            | Instruction::Trap
            | Instruction::DeferPop
            | Instruction::CleanupReturn => {}
        }
        if !matches!(
            instruction,
            Instruction::Jump { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::CleanupReturn
                | Instruction::Trap
        ) && successors.is_empty()
            && pc + 1 < function.code.len()
        {
            successors.push(pc + 1);
        }
        for successor in successors {
            match &mut states[successor] {
                None => {
                    states[successor] = Some(state.clone());
                    queue.push_back(successor);
                }
                Some(existing) if *existing == state => {}
                Some(existing) => {
                    let mut changed = false;
                    for (current, incoming) in existing.iter_mut().zip(&state) {
                        match (*current, *incoming) {
                            (Some(lhs), Some(rhs)) if lhs != rhs => {
                                *current = None;
                                changed = true;
                            }
                            (Some(_), None) => {
                                *current = None;
                                changed = true;
                            }
                            (None, _) | (Some(_), Some(_)) => {}
                        }
                    }
                    if changed {
                        queue.push_back(successor);
                    }
                }
            }
        }
    }
    let mut can_hold_refs = vec![false; register_count];
    for state in states.iter().flatten() {
        let roots = physical_state_roots(module, physical.layouts, state, |_| true)
            .map_err(|kind| error(None, kind))?;
        for (can_hold, root) in can_hold_refs.iter_mut().zip(roots) {
            *can_hold |= root;
        }
    }
    for (register, can_hold_ref) in can_hold_refs.into_iter().enumerate() {
        match (function.root_bitmap[register], can_hold_ref) {
            (true, false) => {
                return Err(error(
                    None,
                    VerifyErrorKind::ForgedRoot(u16::try_from(register).unwrap()),
                ));
            }
            (false, true) => {
                return Err(error(
                    None,
                    VerifyErrorKind::MissingRoot(u16::try_from(register).unwrap()),
                ));
            }
            _ => {}
        }
    }
    verify_safepoints(module, physical.layouts, function_index, function, &states)?;
    Ok(resolved_operands)
}

fn verify_loop_bounds(
    function_index: usize,
    function: &Function,
    limits: VerifierLimits,
) -> Result<(), VerifyError> {
    let mut seen = BTreeSet::new();
    for loop_bound in &function.loop_bounds {
        let pc = usize::try_from(loop_bound.back_edge).unwrap_or(usize::MAX);
        let valid_edge = function.code.get(pc).is_some_and(|instruction| {
            matches!(instruction, Instruction::Jump { target } if *target <= loop_bound.back_edge)
                || matches!(
                    instruction,
                    Instruction::JumpIfFalse { target, .. } if *target <= loop_bound.back_edge
                )
        });
        if !valid_edge
            || loop_bound.max_iterations == 0
            || !seen.insert(loop_bound.back_edge)
            || (function.effect == FunctionEffect::Immediate
                && loop_bound.max_iterations > limits.max_immediate_cost)
        {
            return Err(VerifyError {
                function: function_index,
                instruction: (pc < function.code.len()).then_some(pc),
                kind: VerifyErrorKind::InvalidLoopBound(loop_bound.back_edge),
            });
        }
    }
    Ok(())
}

fn physical_state_roots(
    module: &Module,
    layout_table: &nexa_bytecode::layout::LayoutTable,
    state: &[Option<ValueType>],
    active: impl Fn(usize) -> bool,
) -> Result<Vec<bool>, VerifyErrorKind> {
    let mut roots = vec![false; state.len()];
    for (base, ty) in state.iter().copied().enumerate() {
        let Some(ty) = ty.filter(|_| active(base)) else {
            continue;
        };
        let layout = layout_table
            .layout_of(ty)
            .map_err(VerifyErrorKind::InvalidValueLayout)?;
        let physical_aggregate = matches!(ty, ValueType::Named(type_id)
            if module
                .struct_types
                .iter()
                .any(|struct_type| struct_type.type_id == type_id)
                || module
                    .enum_types
                    .iter()
                    .any(|enum_type| enum_type.type_id == type_id));
        if physical_aggregate {
            let end = base
                .checked_add(usize::from(layout.physical_slots))
                .filter(|end| *end <= state.len())
                .ok_or(VerifyErrorKind::InvalidPhysicalAbi)?;
            if state[base + 1..end].iter().any(Option::is_some) {
                return Err(VerifyErrorKind::InvalidPhysicalAbi);
            }
            for (offset, is_root) in layout.gc_bitmap.iter().copied().enumerate() {
                if is_root {
                    roots[base + offset] = true;
                }
            }
        } else if layout.physical_slots == 1 {
            roots[base] = layout.gc_bitmap[0];
        } else {
            // Compact persistent/Host staging uses one carrier slot.
            roots[base] = ty.is_reference();
        }
    }
    Ok(roots)
}

fn verify_safepoints(
    module: &Module,
    layout_table: &nexa_bytecode::layout::LayoutTable,
    function_index: usize,
    function: &Function,
    states: &[Option<Vec<Option<ValueType>>>],
) -> Result<(), VerifyError> {
    let live_before = exact_live_registers(module, function, states);
    let mut previous = None;
    for &safepoint in &function.safepoints {
        let pc = usize::try_from(safepoint).unwrap_or(usize::MAX);
        let required =
            function.code.get(pc).copied().is_some_and(|instruction| {
                instruction_requires_safepoint(function, pc, instruction)
            });
        if previous.is_some_and(|previous| previous >= safepoint) || !required {
            return Err(VerifyError {
                function: function_index,
                instruction: (pc < function.code.len()).then_some(pc),
                kind: VerifyErrorKind::InvalidSafepoint(safepoint),
            });
        }
        previous = Some(safepoint);
    }

    let mut mapped = BTreeSet::new();
    for root_map in &function.root_maps {
        let pc = usize::try_from(root_map.pc).unwrap_or(usize::MAX);
        if pc >= function.code.len()
            || root_map.bitmap.len() != usize::from(function.registers)
            || !mapped.insert(root_map.pc)
        {
            return Err(VerifyError {
                function: function_index,
                instruction: (pc < function.code.len()).then_some(pc),
                kind: VerifyErrorKind::InvalidRootMap(root_map.pc),
            });
        }
    }
    for (pc, instruction) in function.code.iter().copied().enumerate() {
        let pc_u32 = u32::try_from(pc).expect("bytecode position fits u32");
        if matches!(instruction, Instruction::Yield) && pc + 1 == function.code.len() {
            return Err(VerifyError {
                function: function_index,
                instruction: Some(pc),
                kind: VerifyErrorKind::MissingSafepoint(pc_u32.saturating_add(1)),
            });
        }
        let required = instruction_requires_safepoint(function, pc, instruction);
        if required && function.safepoints.binary_search(&pc_u32).is_err() {
            return Err(VerifyError {
                function: function_index,
                instruction: Some(pc),
                kind: VerifyErrorKind::MissingSafepoint(pc_u32),
            });
        }
        if required {
            let Some(root_map) = function.root_maps.iter().find(|map| map.pc == pc_u32) else {
                return Err(VerifyError {
                    function: function_index,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::InvalidRootMap(pc_u32),
                });
            };
            let exact = if let Some(state) = states[pc].as_ref() {
                physical_state_roots(module, layout_table, state, |base| live_before[pc][base])
                    .map_err(|kind| VerifyError {
                        function: function_index,
                        instruction: Some(pc),
                        kind,
                    })?
            } else {
                vec![false; usize::from(function.registers)]
            };
            if root_map.bitmap != exact {
                return Err(VerifyError {
                    function: function_index,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::InvalidRootMap(pc_u32),
                });
            }
        } else if mapped.contains(&pc_u32) {
            return Err(VerifyError {
                function: function_index,
                instruction: Some(pc),
                kind: VerifyErrorKind::InvalidRootMap(pc_u32),
            });
        }
    }
    Ok(())
}

fn exact_live_registers(
    module: &Module,
    function: &Function,
    states: &[Option<Vec<Option<ValueType>>>],
) -> Vec<Vec<bool>> {
    let register_count = usize::from(function.registers);
    let mut live_before = vec![vec![false; register_count]; function.code.len()];
    loop {
        let mut changed = false;
        for pc in (0..function.code.len()).rev() {
            if states[pc].is_none() {
                continue;
            }
            let instruction = function.code[pc];
            let mut live = vec![false; register_count];
            for successor in instruction_successors(function, pc, instruction) {
                if states[successor].is_some() {
                    for (current, incoming) in
                        live.iter_mut().zip(live_before[successor].iter().copied())
                    {
                        *current |= incoming;
                    }
                }
            }
            if let Some(destination) = instruction_destination(module, instruction) {
                live[usize::from(destination)] = false;
            }
            for source in instruction_sources(instruction) {
                live[usize::from(source)] = true;
            }
            if live_before[pc] != live {
                live_before[pc] = live;
                changed = true;
            }
        }
        if !changed {
            return live_before;
        }
    }
}

fn instruction_successors(function: &Function, pc: usize, instruction: Instruction) -> Vec<usize> {
    match instruction {
        Instruction::Jump { target } => vec![
            usize::try_from(target)
                .expect("reachable jump targets were range-checked before root-map validation"),
        ],
        Instruction::JumpIfFalse { target, .. } => {
            let mut successors =
                vec![usize::try_from(target).expect(
                    "reachable jump targets were range-checked before root-map validation",
                )];
            if pc + 1 < function.code.len() {
                successors.push(pc + 1);
            }
            successors
        }
        Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn
        | Instruction::Trap => Vec::new(),
        _ if pc + 1 < function.code.len() => vec![pc + 1],
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn instruction_sources(instruction: Instruction) -> Vec<u16> {
    let range = |base: u16, count: u16| {
        (0..count)
            .map(|offset| base.saturating_add(offset))
            .collect::<Vec<_>>()
    };
    match instruction {
        Instruction::Move { source, .. }
        | Instruction::CopyValue { source, .. }
        | Instruction::StringLen { source, .. }
        | Instruction::StringByteLen { source, .. }
        | Instruction::StringHash { source, .. }
        | Instruction::I32ToString { source, .. }
        | Instruction::I64ToString { source, .. }
        | Instruction::F32ToString { source, .. }
        | Instruction::F64ToString { source, .. }
        | Instruction::BoolToString { source, .. }
        | Instruction::RuneToString { source, .. }
        | Instruction::StringToString { source, .. }
        | Instruction::EnumTag { source, .. }
        | Instruction::EnumPayload { source, .. }
        | Instruction::StructGet { source, .. }
        | Instruction::ClassGet { source, .. }
        | Instruction::ArrayLen { source, .. }
        | Instruction::ArrayPop { source, .. }
        | Instruction::ArrayClear { source }
        | Instruction::MapLen { source, .. }
        | Instruction::MapClear { source }
        | Instruction::BufferLen { source, .. }
        | Instruction::Return { source } => vec![source],
        Instruction::Add { lhs, rhs, .. }
        | Instruction::Sub { lhs, rhs, .. }
        | Instruction::Mul { lhs, rhs, .. }
        | Instruction::Div { lhs, rhs, .. }
        | Instruction::RemI32 { lhs, rhs, .. }
        | Instruction::AddI64 { lhs, rhs, .. }
        | Instruction::SubI64 { lhs, rhs, .. }
        | Instruction::MulI64 { lhs, rhs, .. }
        | Instruction::DivI64 { lhs, rhs, .. }
        | Instruction::RemI64 { lhs, rhs, .. }
        | Instruction::AddF32 { lhs, rhs, .. }
        | Instruction::SubF32 { lhs, rhs, .. }
        | Instruction::MulF32 { lhs, rhs, .. }
        | Instruction::DivF32 { lhs, rhs, .. }
        | Instruction::RemF32 { lhs, rhs, .. }
        | Instruction::AddF64 { lhs, rhs, .. }
        | Instruction::SubF64 { lhs, rhs, .. }
        | Instruction::MulF64 { lhs, rhs, .. }
        | Instruction::DivF64 { lhs, rhs, .. }
        | Instruction::RemF64 { lhs, rhs, .. }
        | Instruction::StringEqual { lhs, rhs, .. }
        | Instruction::StringConcat { lhs, rhs, .. }
        | Instruction::CompareEq { lhs, rhs, .. }
        | Instruction::CompareLtI32 { lhs, rhs, .. }
        | Instruction::CompareLtI64 { lhs, rhs, .. }
        | Instruction::CompareLtF32 { lhs, rhs, .. }
        | Instruction::CompareLtF64 { lhs, rhs, .. }
        | Instruction::EnumEqual { lhs, rhs, .. }
        | Instruction::StructEqual { lhs, rhs, .. }
        | Instruction::ClassEqual { lhs, rhs, .. }
        | Instruction::StateHandleEqual { lhs, rhs, .. } => vec![lhs, rhs],
        Instruction::StringRuneAt { source, index, .. }
        | Instruction::ArrayGet { source, index, .. }
        | Instruction::ArrayFieldGet { source, index, .. }
        | Instruction::ArrayRemove { source, index, .. }
        | Instruction::BufferGet { source, index, .. } => vec![source, index],
        Instruction::StandardIntrinsic {
            args_base,
            args_count,
            ..
        }
        | Instruction::StringBuild {
            parts_base: args_base,
            parts_count: args_count,
            ..
        }
        | Instruction::Call {
            args_base,
            args_count,
            ..
        }
        | Instruction::HostCall {
            args_base,
            args_count,
            ..
        }
        | Instruction::DeferPush {
            args_base,
            args_count,
            ..
        } => range(args_base, args_count),
        Instruction::JumpIfFalse { condition, .. } => vec![condition],
        Instruction::StateNewSet { object, source, .. } => vec![object, source],
        Instruction::StateReplace { target, .. } => vec![target],
        Instruction::EnumNew { payload, .. } => payload.into_iter().collect(),
        Instruction::StructNew {
            fields_base,
            fields_count,
            ..
        }
        | Instruction::ClassNew {
            fields_base,
            fields_count,
            ..
        } => range(fields_base, fields_count),
        Instruction::StructWith { source, value, .. }
        | Instruction::ClassSet { source, value, .. }
        | Instruction::ArrayPush { source, value } => vec![source, value],
        Instruction::ArrayPushRow {
            source,
            fields_base,
            fields_count,
        } => {
            let mut reads = range(fields_base, fields_count);
            reads.push(source);
            reads
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
        }
        | Instruction::BufferSet {
            source,
            index,
            value,
        } => vec![source, index, value],
        Instruction::MapGet { source, key, .. }
        | Instruction::MapRemove { source, key, .. }
        | Instruction::MapContains { source, key, .. } => vec![source, key],
        Instruction::MapSet { source, key, value } => vec![source, key, value],
        Instruction::BufferSlice {
            source,
            start,
            length,
            ..
        } => vec![source, start, length],
        Instruction::BufferCopy {
            destination,
            source,
            source_start,
            destination_start,
            length,
        } => vec![destination, source, source_start, destination_start, length],
        Instruction::StateOldFieldGet { object, .. } => vec![object],
        Instruction::StateHandleResolve { handle, .. }
        | Instruction::StateHandleIsAlive { handle, .. }
        | Instruction::StateHandleStableId { handle, .. }
        | Instruction::StateHandleGeneration { handle, .. }
        | Instruction::StateHandleHash { handle, .. } => vec![handle],
        Instruction::LoadI32 { .. }
        | Instruction::LoadBool { .. }
        | Instruction::LoadI64 { .. }
        | Instruction::LoadF32 { .. }
        | Instruction::LoadF64 { .. }
        | Instruction::LoadRune { .. }
        | Instruction::LoadString { .. }
        | Instruction::Jump { .. }
        | Instruction::StateOldGet { .. }
        | Instruction::StateCurrentGet { .. }
        | Instruction::StateNewCreate { .. }
        | Instruction::StatePreserve { .. }
        | Instruction::StateDelete { .. }
        | Instruction::ArrayNew { .. }
        | Instruction::MapNew { .. }
        | Instruction::StateFinish
        | Instruction::DeferPop
        | Instruction::CleanupReturn
        | Instruction::ReturnVoid
        | Instruction::Safepoint
        | Instruction::Yield
        | Instruction::Trap => Vec::new(),
    }
}

#[allow(clippy::too_many_lines)]
fn instruction_destination(module: &Module, instruction: Instruction) -> Option<u16> {
    match instruction {
        Instruction::Call { function, dst, .. } => module
            .functions
            .get(function as usize)
            .and_then(|callee| callee.signature.result.map(|_| dst)),
        Instruction::HostCall { import, dst, .. } => module
            .host_imports
            .get(import as usize)
            .and_then(|host| host.result.map(|_| dst)),
        Instruction::LoadI32 { dst, .. }
        | Instruction::LoadBool { dst, .. }
        | Instruction::LoadI64 { dst, .. }
        | Instruction::LoadF32 { dst, .. }
        | Instruction::LoadF64 { dst, .. }
        | Instruction::LoadRune { dst, .. }
        | Instruction::LoadString { dst, .. }
        | Instruction::Move { dst, .. }
        | Instruction::CopyValue { dst, .. }
        | Instruction::Add { dst, .. }
        | Instruction::Sub { dst, .. }
        | Instruction::Mul { dst, .. }
        | Instruction::Div { dst, .. }
        | Instruction::RemI32 { dst, .. }
        | Instruction::AddI64 { dst, .. }
        | Instruction::SubI64 { dst, .. }
        | Instruction::MulI64 { dst, .. }
        | Instruction::DivI64 { dst, .. }
        | Instruction::RemI64 { dst, .. }
        | Instruction::AddF32 { dst, .. }
        | Instruction::SubF32 { dst, .. }
        | Instruction::MulF32 { dst, .. }
        | Instruction::DivF32 { dst, .. }
        | Instruction::RemF32 { dst, .. }
        | Instruction::AddF64 { dst, .. }
        | Instruction::SubF64 { dst, .. }
        | Instruction::MulF64 { dst, .. }
        | Instruction::DivF64 { dst, .. }
        | Instruction::RemF64 { dst, .. }
        | Instruction::StringLen { dst, .. }
        | Instruction::StringByteLen { dst, .. }
        | Instruction::StringEqual { dst, .. }
        | Instruction::StringConcat { dst, .. }
        | Instruction::StringBuild { dst, .. }
        | Instruction::StringRuneAt { dst, .. }
        | Instruction::StringHash { dst, .. }
        | Instruction::I32ToString { dst, .. }
        | Instruction::I64ToString { dst, .. }
        | Instruction::F32ToString { dst, .. }
        | Instruction::F64ToString { dst, .. }
        | Instruction::BoolToString { dst, .. }
        | Instruction::RuneToString { dst, .. }
        | Instruction::StringToString { dst, .. }
        | Instruction::StandardIntrinsic { dst, .. }
        | Instruction::CompareEq { dst, .. }
        | Instruction::CompareLtI32 { dst, .. }
        | Instruction::CompareLtI64 { dst, .. }
        | Instruction::CompareLtF32 { dst, .. }
        | Instruction::CompareLtF64 { dst, .. }
        | Instruction::StateOldGet { dst, .. }
        | Instruction::StateCurrentGet { dst, .. }
        | Instruction::StateNewCreate { dst, .. }
        | Instruction::EnumNew { dst, .. }
        | Instruction::EnumTag { dst, .. }
        | Instruction::EnumPayload { dst, .. }
        | Instruction::EnumEqual { dst, .. }
        | Instruction::StructNew { dst, .. }
        | Instruction::StructGet { dst, .. }
        | Instruction::StructWith { dst, .. }
        | Instruction::StructEqual { dst, .. }
        | Instruction::ClassNew { dst, .. }
        | Instruction::ClassGet { dst, .. }
        | Instruction::ClassEqual { dst, .. }
        | Instruction::ArrayNew { dst, .. }
        | Instruction::ArrayLen { dst, .. }
        | Instruction::ArrayGet { dst, .. }
        | Instruction::ArrayFieldGet { dst, .. }
        | Instruction::ArrayPop { dst, .. }
        | Instruction::ArrayRemove { dst, .. }
        | Instruction::MapNew { dst, .. }
        | Instruction::MapLen { dst, .. }
        | Instruction::MapGet { dst, .. }
        | Instruction::MapRemove { dst, .. }
        | Instruction::MapContains { dst, .. }
        | Instruction::BufferLen { dst, .. }
        | Instruction::BufferGet { dst, .. }
        | Instruction::BufferSlice { dst, .. }
        | Instruction::StateOldFieldGet { dst, .. }
        | Instruction::StateHandleResolve { dst, .. }
        | Instruction::StateHandleIsAlive { dst, .. }
        | Instruction::StateHandleStableId { dst, .. }
        | Instruction::StateHandleGeneration { dst, .. }
        | Instruction::StateHandleEqual { dst, .. }
        | Instruction::StateHandleHash { dst, .. } => Some(dst),
        Instruction::Jump { .. }
        | Instruction::JumpIfFalse { .. }
        | Instruction::StateNewSet { .. }
        | Instruction::StateReplace { .. }
        | Instruction::StatePreserve { .. }
        | Instruction::StateDelete { .. }
        | Instruction::ClassSet { .. }
        | Instruction::ArraySet { .. }
        | Instruction::ArrayPush { .. }
        | Instruction::ArrayPushRow { .. }
        | Instruction::ArrayInsert { .. }
        | Instruction::ArrayClear { .. }
        | Instruction::MapSet { .. }
        | Instruction::MapClear { .. }
        | Instruction::BufferSet { .. }
        | Instruction::BufferCopy { .. }
        | Instruction::StateFinish
        | Instruction::DeferPush { .. }
        | Instruction::DeferPop
        | Instruction::CleanupReturn
        | Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::Safepoint
        | Instruction::Yield
        | Instruction::Trap => None,
    }
}

fn instruction_requires_safepoint(
    function: &Function,
    pc: usize,
    instruction: Instruction,
) -> bool {
    let pc_u32 = u32::try_from(pc).expect("bytecode position fits u32");
    pc == 0
        || (pc > 0
            && matches!(
                function.code[pc - 1],
                Instruction::HostCall { .. } | Instruction::Yield
            ))
        || matches!(
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
                | Instruction::ClassNew { .. }
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
                | Instruction::Call { .. }
                | Instruction::HostCall { .. }
                | Instruction::StateCurrentGet { .. }
                | Instruction::StateHandleResolve { .. }
                | Instruction::Return { .. }
                | Instruction::ReturnVoid
                | Instruction::Trap
                | Instruction::CleanupReturn
        )
        || matches!(instruction, Instruction::Jump { target } if target <= pc_u32)
        || matches!(
            instruction,
            Instruction::JumpIfFalse { target, .. } if target <= pc_u32
        )
}

fn target_index(
    function: &Function,
    function_index: usize,
    pc: usize,
    target: u32,
) -> Result<usize, VerifyError> {
    let target_index = usize::try_from(target).unwrap_or(usize::MAX);
    if target_index < function.code.len() {
        Ok(target_index)
    } else {
        Err(VerifyError {
            function: function_index,
            instruction: Some(pc),
            kind: VerifyErrorKind::JumpOutOfRange(target),
        })
    }
}

fn immediate_call_closure(module: &Module) -> Vec<bool> {
    let mut closure = module
        .functions
        .iter()
        .map(|function| function.effect == FunctionEffect::Immediate)
        .collect::<Vec<_>>();
    let mut pending = closure
        .iter()
        .enumerate()
        .filter_map(|(index, reachable)| reachable.then_some(index))
        .collect::<Vec<_>>();
    while let Some(function) = pending.pop() {
        for instruction in &module.functions[function].code {
            let (callee, deferred) = match *instruction {
                Instruction::Call {
                    function: callee, ..
                } => (callee, false),
                Instruction::DeferPush {
                    function: callee, ..
                } => (callee, true),
                _ => continue,
            };
            let callee = usize::try_from(callee).unwrap_or(usize::MAX);
            let Some(callee_function) = module.functions.get(callee) else {
                continue;
            };
            let allowed_effect = if deferred {
                matches!(
                    callee_function.effect,
                    FunctionEffect::Ordinary | FunctionEffect::Cleanup
                )
            } else {
                matches!(
                    callee_function.effect,
                    FunctionEffect::Ordinary | FunctionEffect::Immediate
                )
            };
            if allowed_effect && !closure[callee] {
                closure[callee] = true;
                pending.push(callee);
            }
        }
    }
    closure
}

fn restricted_effect_call_closure(module: &Module) -> Vec<bool> {
    let mut closure = module
        .functions
        .iter()
        .map(|function| {
            matches!(
                function.effect,
                FunctionEffect::Migration | FunctionEffect::Cleanup
            )
        })
        .collect::<Vec<_>>();
    for function in &module.functions {
        for instruction in &function.code {
            let Instruction::DeferPush {
                function: cleanup, ..
            } = instruction
            else {
                continue;
            };
            let cleanup = usize::try_from(*cleanup).unwrap_or(usize::MAX);
            if let Some(restricted) = closure.get_mut(cleanup) {
                *restricted = true;
            }
        }
    }
    let mut pending = closure
        .iter()
        .enumerate()
        .filter_map(|(index, reachable)| reachable.then_some(index))
        .collect::<Vec<_>>();
    while let Some(function) = pending.pop() {
        for instruction in &module.functions[function].code {
            let callee = match instruction {
                Instruction::Call {
                    function: callee, ..
                }
                | Instruction::DeferPush {
                    function: callee, ..
                } => *callee,
                _ => continue,
            };
            let callee = usize::try_from(callee).unwrap_or(usize::MAX);
            if module.functions.get(callee).is_some() && !closure[callee] {
                closure[callee] = true;
                pending.push(callee);
            }
        }
    }
    closure
}

fn static_call_depths(module: &Module) -> Result<Vec<u16>, VerifyError> {
    let mut memo = vec![[None; 2]; module.functions.len()];
    (0..module.functions.len())
        .map(|function| call_depth(module, function, &mut memo))
        .collect()
}

struct CallDepthFrame {
    function: usize,
    next_instruction: usize,
    depth: u16,
    immediate_on_path: bool,
}

fn call_depth(
    module: &Module,
    function: usize,
    memo: &mut [[Option<u16>; 2]],
) -> Result<u16, VerifyError> {
    let immediate_on_path = module.functions[function].effect == FunctionEffect::Immediate;
    let context = usize::from(immediate_on_path);
    if let Some(depth) = memo[function][context] {
        return Ok(depth);
    }

    let mut active = vec![false; module.functions.len()];
    active[function] = true;
    let mut frames = vec![CallDepthFrame {
        function,
        next_instruction: 0,
        depth: 1,
        immediate_on_path,
    }];
    loop {
        let frame = frames
            .last_mut()
            .expect("the root call-depth frame returns instead of emptying the stack");
        let next_call = module.functions[frame.function]
            .code
            .iter()
            .copied()
            .enumerate()
            .skip(frame.next_instruction)
            .find_map(|(pc, instruction)| {
                frame.next_instruction = pc + 1;
                let (Instruction::Call {
                    function: callee, ..
                }
                | Instruction::DeferPush {
                    function: callee, ..
                }) = instruction
                else {
                    return None;
                };
                Some((pc, callee))
            });
        let Some((pc, callee)) = next_call else {
            let completed = frames
                .pop()
                .expect("the call-depth work stack was checked as non-empty");
            active[completed.function] = false;
            let context = usize::from(completed.immediate_on_path);
            memo[completed.function][context] = Some(completed.depth);
            if let Some(parent) = frames.last_mut() {
                parent.depth = parent.depth.max(completed.depth.saturating_add(1));
                continue;
            }
            return Ok(completed.depth);
        };

        let callee = usize::try_from(callee).unwrap_or(usize::MAX);
        if callee >= module.functions.len() {
            return Err(VerifyError {
                function: frame.function,
                instruction: Some(pc),
                kind: VerifyErrorKind::FunctionOutOfRange(
                    u32::try_from(callee).unwrap_or(u32::MAX),
                ),
            });
        }
        if active[callee] {
            if frame.immediate_on_path {
                return Err(VerifyError {
                    function: callee,
                    instruction: None,
                    kind: VerifyErrorKind::ImmediateRecursion,
                });
            }
            frame.depth = u16::MAX;
            continue;
        }
        let callee_immediate = module.functions[callee].effect == FunctionEffect::Immediate;
        let child_immediate_on_path = frame.immediate_on_path || callee_immediate;
        let context = usize::from(child_immediate_on_path);
        if let Some(depth) = memo[callee][context] {
            frame.depth = frame.depth.max(depth.saturating_add(1));
            continue;
        }
        active[callee] = true;
        frames.push(CallDepthFrame {
            function: callee,
            next_instruction: 0,
            depth: 1,
            immediate_on_path: child_immediate_on_path,
        });
    }
}

fn immediate_wcets(
    module: &Module,
    closure: &[bool],
    max_states: u32,
) -> Result<Vec<Option<u32>>, VerifyError> {
    let order = immediate_wcet_order(module, closure)?;
    let mut costs = vec![None; module.functions.len()];
    for function in order {
        let remaining = module.functions[function]
            .loop_bounds
            .iter()
            .map(|loop_bound| (loop_bound.back_edge, loop_bound.max_iterations))
            .collect::<Vec<_>>();
        let mut memo = BTreeMap::new();
        costs[function] = Some(longest_path(
            module, function, remaining, &mut memo, &costs, max_states,
        )?);
    }
    Ok(costs)
}

struct WcetCallFrame {
    function: usize,
    next_instruction: usize,
}

fn immediate_wcet_order(module: &Module, closure: &[bool]) -> Result<Vec<usize>, VerifyError> {
    let mut marks = vec![0_u8; module.functions.len()];
    let mut order = Vec::new();
    for root in 0..module.functions.len() {
        if !closure[root] || marks[root] != 0 {
            continue;
        }
        marks[root] = 1;
        let mut frames = vec![WcetCallFrame {
            function: root,
            next_instruction: 0,
        }];
        loop {
            let frame = frames
                .last_mut()
                .expect("the root WCET call frame returns instead of emptying the stack");
            let next_callee = module.functions[frame.function]
                .code
                .iter()
                .copied()
                .enumerate()
                .skip(frame.next_instruction)
                .find_map(|(pc, instruction)| {
                    frame.next_instruction = pc + 1;
                    let (Instruction::Call {
                        function: callee, ..
                    }
                    | Instruction::DeferPush {
                        function: callee, ..
                    }) = instruction
                    else {
                        return None;
                    };
                    Some(usize::try_from(callee).unwrap_or(usize::MAX))
                });
            let Some(callee) = next_callee else {
                let completed = frames
                    .pop()
                    .expect("the WCET call work stack was checked as non-empty");
                marks[completed.function] = 2;
                order.push(completed.function);
                if frames.is_empty() {
                    break;
                }
                continue;
            };
            if callee >= module.functions.len() || !closure[callee] {
                continue;
            }
            match marks[callee] {
                0 => {
                    marks[callee] = 1;
                    frames.push(WcetCallFrame {
                        function: callee,
                        next_instruction: 0,
                    });
                }
                1 => {
                    return Err(VerifyError {
                        function: callee,
                        instruction: None,
                        kind: VerifyErrorKind::ImmediateRecursion,
                    });
                }
                2 => {}
                _ => unreachable!("WCET call marks have only three states"),
            }
        }
    }
    Ok(order)
}

type WcetMemo = BTreeMap<(usize, Vec<(u32, u32)>), u32>;
type WcetState = (usize, Vec<(u32, u32)>);

struct WcetFrame {
    state: WcetState,
    instruction_cost: u32,
    successors: Vec<WcetState>,
    next_successor: usize,
    suffix_cost: u32,
}

fn longest_path(
    module: &Module,
    function: usize,
    remaining: Vec<(u32, u32)>,
    memo: &mut WcetMemo,
    callee_costs: &[Option<u32>],
    max_states: u32,
) -> Result<u32, VerifyError> {
    let mut explored = 0_u32;
    let mut frames = Vec::<WcetFrame>::new();
    let mut pending = Some((0, remaining));
    loop {
        if let Some(state) = pending.take() {
            if let Some(cost) = memo.get(&state).copied() {
                if let Some(parent) = frames.last_mut() {
                    parent.suffix_cost = parent.suffix_cost.max(cost);
                    continue;
                }
                return Ok(cost);
            }
            explored = explored.saturating_add(1);
            if explored > max_states {
                return Err(VerifyError {
                    function,
                    instruction: Some(state.0),
                    kind: VerifyErrorKind::WcetComplexityLimit,
                });
            }
            frames.push(wcet_frame(module, function, state, callee_costs)?);
        }

        let Some(frame) = frames.last_mut() else {
            unreachable!("the root WCET state returns instead of emptying the work stack");
        };
        if let Some(successor) = frame.successors.get(frame.next_successor).cloned() {
            frame.next_successor += 1;
            pending = Some(successor);
            continue;
        }

        let frame = frames
            .pop()
            .expect("the WCET work stack was checked as non-empty");
        let cost = frame
            .instruction_cost
            .checked_add(frame.suffix_cost)
            .ok_or(VerifyError {
                function,
                instruction: Some(frame.state.0),
                kind: VerifyErrorKind::ImmediateCostLimit,
            })?;
        memo.insert(frame.state, cost);
        if let Some(parent) = frames.last_mut() {
            parent.suffix_cost = parent.suffix_cost.max(cost);
        } else {
            return Ok(cost);
        }
    }
}

fn wcet_frame(
    module: &Module,
    function: usize,
    state: WcetState,
    callee_costs: &[Option<u32>],
) -> Result<WcetFrame, VerifyError> {
    let pc = state.0;
    let instruction = *module.functions[function].code.get(pc).ok_or(VerifyError {
        function,
        instruction: Some(pc),
        kind: VerifyErrorKind::WcetComplexityLimit,
    })?;
    let extra_cost = wcet_instruction_extra_cost(module, function, pc, instruction, callee_costs)?;
    let successors = wcet_successors(module, function, pc, instruction, &state.1)?;
    Ok(WcetFrame {
        state,
        instruction_cost: 1_u32.checked_add(extra_cost).ok_or(VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })?,
        successors,
        next_successor: 0,
        suffix_cost: 0,
    })
}

#[allow(clippy::too_many_lines)]
fn wcet_instruction_extra_cost(
    module: &Module,
    function: usize,
    pc: usize,
    instruction: Instruction,
    callee_costs: &[Option<u32>],
) -> Result<u32, VerifyError> {
    if let Instruction::Call {
        function: callee, ..
    }
    | Instruction::DeferPush {
        function: callee, ..
    } = instruction
    {
        let callee_cost =
            callee_costs
                .get(callee as usize)
                .copied()
                .flatten()
                .ok_or(VerifyError {
                    function,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::ImmediateRecursion,
                })?;
        if matches!(instruction, Instruction::DeferPush { .. }) {
            callee_cost.checked_add(1).ok_or(VerifyError {
                function,
                instruction: Some(pc),
                kind: VerifyErrorKind::ImmediateCostLimit,
            })
        } else {
            Ok(callee_cost)
        }
    } else if let Instruction::HostCall { import, .. } = instruction {
        Ok(module
            .host_imports
            .get(import as usize)
            .ok_or(VerifyError {
                function,
                instruction: Some(pc),
                kind: VerifyErrorKind::HostImportOutOfRange(import),
            })?
            .fuel_cost)
    } else if let Instruction::StandardIntrinsic { intrinsic, .. } = instruction {
        u32::from(intrinsic.base_fuel_cost())
            .checked_sub(1)
            .ok_or(VerifyError {
                function,
                instruction: Some(pc),
                kind: VerifyErrorKind::ImmediateCostLimit,
            })
    } else if let Instruction::LoadString { string, .. } = instruction {
        let bytes = module
            .strings
            .get(string as usize)
            .ok_or(VerifyError {
                function,
                instruction: Some(pc),
                kind: VerifyErrorKind::StringOutOfRange(string),
            })?
            .len();
        let bytes = u64::try_from(bytes).map_err(|_| VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })?;
        let blocks = if bytes == 0 {
            0
        } else {
            (bytes - 1) / STANDARD_STRING_FUEL_BLOCK_BYTES + 1
        };
        u32::try_from(blocks.checked_mul(2).ok_or(VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })?)
        .map_err(|_| VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })
    } else if matches!(
        instruction,
        Instruction::I32ToString { .. }
            | Instruction::I64ToString { .. }
            | Instruction::F32ToString { .. }
            | Instruction::F64ToString { .. }
            | Instruction::BoolToString { .. }
            | Instruction::RuneToString { .. }
    ) {
        let blocks = (SCALAR_TO_STRING_MAX_BYTES - 1) / STANDARD_STRING_FUEL_BLOCK_BYTES + 1;
        u32::try_from(
            blocks
                .checked_mul(SCALAR_TO_STRING_FUEL_PASSES)
                .ok_or(VerifyError {
                    function,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::ImmediateCostLimit,
                })?,
        )
        .map_err(|_| VerifyError {
            function,
            instruction: Some(pc),
            kind: VerifyErrorKind::ImmediateCostLimit,
        })
    } else {
        Ok(0)
    }
}

fn wcet_successors(
    module: &Module,
    function: usize,
    pc: usize,
    instruction: Instruction,
    remaining: &[(u32, u32)],
) -> Result<Vec<WcetState>, VerifyError> {
    let raw_successors = match instruction {
        Instruction::Jump { target } => {
            vec![target as usize]
        }
        Instruction::JumpIfFalse { target, .. } => {
            let mut successors = vec![target as usize];
            if pc + 1 < module.functions[function].code.len() {
                successors.push(pc + 1);
            }
            successors
        }
        Instruction::Return { .. }
        | Instruction::ReturnVoid
        | Instruction::CleanupReturn
        | Instruction::Trap => Vec::new(),
        _ if pc + 1 < module.functions[function].code.len() => vec![pc + 1],
        _ => Vec::new(),
    };
    let mut successors = Vec::with_capacity(raw_successors.len());
    for successor in raw_successors {
        let mut branch_remaining = remaining.to_vec();
        if successor <= pc {
            let back_edge = u32::try_from(pc).expect("bytecode position fits u32");
            let Some((_, budget)) = branch_remaining
                .iter_mut()
                .find(|(edge, _)| *edge == back_edge)
            else {
                return Err(VerifyError {
                    function,
                    instruction: Some(pc),
                    kind: VerifyErrorKind::ImmediateCostLimit,
                });
            };
            if *budget == 0 {
                continue;
            }
            *budget -= 1;
        }
        successors.push((successor, branch_remaining));
    }
    Ok(successors)
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::{
        AbandonPolicy, ArrayType, AsyncResultType, BufferType, CancelPolicy, ClassType, EnumType,
        EnumVariant, Function, FunctionBuilder, FunctionEffect, HostCallMode, HostImport,
        Instruction, MapType, Module, ModuleBuilder, ResourceTokenType, RootMap, ScriptExport,
        Signature, SnapshotType, SourceMapEntry, StandardIntrinsic, StateField, StateHandleType,
        StateSchema, StateType, StructField, StructType, ValueType,
    };
    use nexa_core::{FileId, SourceSpan, StableId};

    use super::{
        ResolvedNominalOperand, VerifierLimits, VerifyErrorKind, verify, verify_reload_transition,
    };

    fn physical_record_module() -> (StableId, Module) {
        let record = StableId::from_name("PhysicalRecord");
        let mut module = ModuleBuilder::new();
        module.struct_type(StructType {
            type_id: record,
            fields: vec![
                StructField {
                    stable_id: StableId::from_name("PhysicalRecord::wide"),
                    ty: ValueType::I64,
                },
                StructField {
                    stable_id: StableId::from_name("PhysicalRecord::flag"),
                    ty: ValueType::Bool,
                },
            ],
        });
        let mut identity = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(record)],
                result: Some(ValueType::Named(record)),
            },
            4,
        );
        identity
            .parameter_slots(2)
            .emit(Instruction::CopyValue {
                dst: 2,
                source: 0,
                slots: 2,
            })
            .emit(Instruction::Return { source: 2 });
        let mut identity = identity.finish().expect("identity function");
        identity.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false; 4],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false; 4],
            },
        ];
        module.function(identity);
        let mut wrapper = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(record)],
                result: Some(ValueType::Named(record)),
            },
            4,
        );
        wrapper
            .parameter_slots(2)
            .emit(Instruction::Call {
                function: 0,
                args_base: 0,
                args_count: 2,
                dst: 2,
            })
            .emit(Instruction::Return { source: 2 });
        let mut wrapper = wrapper.finish().expect("wrapper function");
        wrapper.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false; 4],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false; 4],
            },
        ];
        module.function(wrapper);
        (record, module.finish())
    }

    #[test]
    fn verified_module_owns_the_exact_physical_layout_and_function_abi() {
        let (record, module) = physical_record_module();
        let verified = verify(module, VerifierLimits::default()).expect("verified physical module");
        let layout = verified
            .layout_table()
            .layout_of(ValueType::Named(record))
            .expect("record layout");
        let abi = verified.module_abi().function(0).expect("function ABI");

        assert_eq!(layout.physical_slots, 2);
        assert_eq!(abi.parameter_slots, 2);
        assert_eq!(abi.parameters[0].slot_count, 2);
        assert_eq!(abi.result.as_ref().map(|result| result.slot_count), Some(2));
        assert_eq!(
            verified.resolved_operand(1, 0),
            ResolvedNominalOperand::CallFrame {
                register_count: 4,
                parameter_slots: 2,
                result_slots: 2,
            }
        );
        let mut forged = verified.module().clone();
        forged.functions[0].parameter_slots = 1;
        assert_eq!(
            verify(forged, VerifierLimits::default()).unwrap_err().kind,
            VerifyErrorKind::InvalidPhysicalAbi
        );
        let mut forged = verified.module().clone();
        let Instruction::CopyValue { slots, .. } = &mut forged.functions[0].code[0] else {
            unreachable!("identity starts with CopyValue")
        };
        *slots = 1;
        assert_eq!(
            verify(forged, VerifierLimits::default()).unwrap_err().kind,
            VerifyErrorKind::InvalidPhysicalAbi
        );
    }

    #[test]
    fn host_import_authority_metadata_is_canonical() {
        let import = |capabilities: &[&str]| HostImport {
            stable_id: StableId::from_name("Host::read_profile"),
            declaration_fingerprint: [7; 32],
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            parameters: Vec::new(),
            result: Some(ValueType::I32),
            mode: HostCallMode::Immediate,
            fuel_cost: 3,
            async_result: None,
        };
        let module = |capabilities: &[&str]| {
            let mut module = ModuleBuilder::new();
            module.host_import(import(capabilities));
            module.finish()
        };

        assert!(
            verify(
                module(&["profile.read", "world-state_read"]),
                VerifierLimits::default(),
            )
            .is_ok()
        );
        for invalid in [
            Vec::from(["profile.read", "profile.read"]),
            Vec::from(["world.write", "profile.read"]),
            Vec::from([""]),
            Vec::from(["scope..read"]),
            Vec::from(["scope:read"]),
        ] {
            assert_eq!(
                verify(module(&invalid), VerifierLimits::default())
                    .unwrap_err()
                    .kind,
                VerifyErrorKind::InvalidHostImportMetadata
            );
        }
    }

    #[test]
    fn unused_async_import_cannot_forge_its_result_type_identity() {
        let canonical = nexa_bytecode::result_type(ValueType::I32, ValueType::String);
        let forged = StableId::from_name("forged-result-type");
        let mut module = ModuleBuilder::new();
        module.enum_type(canonical);
        module.host_import(HostImport {
            stable_id: StableId::from_name("Host::unused"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::Named(forged)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(AsyncResultType {
                result_type: forged,
                success: ValueType::I32,
                error: ValueType::String,
                cancel_policy: CancelPolicy::CancelTask,
                abandon_policy: AbandonPolicy::Trap,
                cancel_error: None,
                abandon_error: None,
            }),
        });
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEnumMetadata
        );
    }

    #[test]
    fn async_return_error_policy_requires_a_representable_error_payload() {
        let invalid_scalar = nexa_bytecode::result_type(ValueType::I32, ValueType::Bool);
        let mut scalar_module = ModuleBuilder::new();
        scalar_module.enum_type(invalid_scalar.clone());
        scalar_module.host_import(HostImport {
            stable_id: StableId::from_name("Host::invalid_scalar"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::Named(invalid_scalar.type_id)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(AsyncResultType {
                result_type: invalid_scalar.type_id,
                success: ValueType::I32,
                error: ValueType::Bool,
                cancel_policy: CancelPolicy::ReturnError,
                abandon_policy: AbandonPolicy::Trap,
                cancel_error: Some(0),
                abandon_error: None,
            }),
        });
        assert_eq!(
            verify(scalar_module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEnumMetadata
        );

        let error_type = StableId::from_name("PayloadFailure");
        let error = EnumType {
            type_id: error_type,
            variants: vec![EnumVariant {
                stable_id: StableId::from_name("PayloadFailure::Cancelled"),
                tag: 0,
                payload_type: Some(ValueType::I32),
            }],
        };
        let result = nexa_bytecode::result_type(ValueType::I32, ValueType::Named(error_type));
        let mut payload_module = ModuleBuilder::new();
        payload_module.enum_type(error).enum_type(result.clone());
        payload_module.host_import(HostImport {
            stable_id: StableId::from_name("Host::invalid_payload_variant"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: Some(ValueType::Named(result.type_id)),
            mode: HostCallMode::Async,
            fuel_cost: 1,
            async_result: Some(AsyncResultType {
                result_type: result.type_id,
                success: ValueType::I32,
                error: ValueType::Named(error_type),
                cancel_policy: CancelPolicy::CancelTask,
                abandon_policy: AbandonPolicy::ReturnError,
                cancel_error: None,
                abandon_error: Some(0),
            }),
        });
        assert_eq!(
            verify(payload_module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEnumMetadata
        );
    }

    #[test]
    fn class_metadata_rejects_duplicate_fields_and_named_type_collisions() {
        let type_id = StableId::from_name("Node");
        let field = StructField {
            stable_id: StableId::from_parts(&["Node", "::value"]),
            ty: ValueType::I32,
        };
        let mut duplicate = ModuleBuilder::new();
        duplicate.class_type(ClassType {
            type_id,
            fields: vec![field, field],
        });
        assert_eq!(
            verify(duplicate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidClassMetadata
        );

        let mut collision = ModuleBuilder::new();
        collision
            .struct_type(nexa_bytecode::StructType {
                type_id,
                fields: vec![field],
            })
            .class_type(ClassType {
                type_id,
                fields: vec![field],
            });
        assert_eq!(
            verify(collision.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidClassMetadata
        );
    }

    #[test]
    fn array_metadata_and_instruction_types_are_verified_independently() {
        let array = ArrayType::new(ValueType::I32);
        let mut valid = ModuleBuilder::new();
        valid.array_type(array);
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let mut forged = ModuleBuilder::new();
        forged.array_type(ArrayType {
            type_id: StableId::from_name("forged-array"),
            element: ValueType::I32,
        });
        assert_eq!(
            verify(forged.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidArrayMetadata
        );

        let mut wrong_element = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(array.type_id), ValueType::Bool],
                result: None,
            },
            2,
        );
        wrong_element
            .set_root(0)
            .unwrap()
            .emit(Instruction::ArrayPush {
                source: 0,
                value: 1,
            })
            .emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module
            .array_type(array)
            .function(wrong_element.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );

        let mut immediate = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Named(array.type_id)),
            },
            1,
        );
        immediate
            .effect(FunctionEffect::Immediate)
            .set_root(0)
            .unwrap()
            .emit(Instruction::ArrayNew {
                type_id: array.type_id,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module
            .array_type(array)
            .function(immediate.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEffect
        );
    }

    #[test]
    fn stress_rejects_100_forged_candidates_deterministically() {
        for generation in 0..100 {
            let mut forged = ModuleBuilder::new();
            forged.array_type(ArrayType {
                type_id: StableId::from_parts(&["forged-array", &generation.to_string()]),
                element: ValueType::I32,
            });
            assert_eq!(
                verify(forged.finish(), VerifierLimits::default())
                    .unwrap_err()
                    .kind,
                VerifyErrorKind::InvalidArrayMetadata
            );
        }
    }

    #[test]
    fn buffer_metadata_instruction_types_and_effects_are_verified_independently() {
        let buffer = BufferType::new(ValueType::I32);
        let mut valid = ModuleBuilder::new();
        valid.buffer_type(buffer);
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let mut forged = ModuleBuilder::new();
        forged.buffer_type(BufferType {
            type_id: StableId::from_name("forged-buffer"),
            element: ValueType::I32,
        });
        assert_eq!(
            verify(forged.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidBufferMetadata
        );

        let mut wrong_element = FunctionBuilder::new(
            Signature {
                parameters: vec![
                    ValueType::Named(buffer.type_id),
                    ValueType::I32,
                    ValueType::Bool,
                ],
                result: None,
            },
            3,
        );
        wrong_element
            .set_root(0)
            .unwrap()
            .emit(Instruction::BufferSet {
                source: 0,
                index: 1,
                value: 2,
            })
            .emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module
            .buffer_type(buffer)
            .function(wrong_element.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );

        let mut immediate = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(buffer.type_id)],
                result: Some(ValueType::I32),
            },
            2,
        );
        immediate
            .effect(FunctionEffect::Immediate)
            .set_root(0)
            .unwrap()
            .emit(Instruction::BufferLen { source: 0, dst: 1 })
            .emit(Instruction::Return { source: 1 });
        let mut module = ModuleBuilder::new();
        module
            .buffer_type(buffer)
            .function(immediate.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEffect
        );
    }

    #[test]
    fn typed_snapshot_metadata_requires_an_exact_known_content_type() {
        let content_type = StableId::from_name("EnemyView");
        let snapshot = SnapshotType::new(content_type);
        let content = StructType {
            type_id: content_type,
            fields: Vec::new(),
        };
        let mut identity = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(snapshot.type_id)],
                result: Some(ValueType::Named(snapshot.type_id)),
            },
            1,
        );
        identity.emit(Instruction::Return { source: 0 });
        let mut valid = ModuleBuilder::new();
        valid
            .struct_type(content.clone())
            .snapshot_type(snapshot)
            .function(identity.finish().unwrap());
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let mut forged = ModuleBuilder::new();
        forged.struct_type(content).snapshot_type(SnapshotType {
            type_id: StableId::from_name("forged-snapshot"),
            content_type,
        });
        assert_eq!(
            verify(forged.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidSnapshotMetadata
        );

        let mut unknown_content = ModuleBuilder::new();
        unknown_content.snapshot_type(SnapshotType::new(StableId::from_name("UnknownView")));
        assert_eq!(
            verify(unknown_content.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidSnapshotMetadata
        );
    }

    #[test]
    fn typed_resource_token_metadata_is_canonical_and_content_specific() {
        let action_lock = StableId::from_name("ActionLock");
        let motion_lock = StableId::from_name("MotionLock");
        let action_token = ResourceTokenType::new(action_lock);
        let motion_token = ResourceTokenType::new(motion_lock);
        assert_ne!(action_token.type_id, motion_token.type_id);

        let mut valid = ModuleBuilder::new();
        valid
            .resource_token_type(action_token)
            .resource_token_type(motion_token);
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let mut forged = ModuleBuilder::new();
        forged.resource_token_type(ResourceTokenType {
            type_id: StableId::from_name("ResourceToken"),
            content_type: action_lock,
        });
        assert_eq!(
            verify(forged.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidResourceTokenMetadata
        );

        let mut duplicate = ModuleBuilder::new();
        duplicate
            .resource_token_type(action_token)
            .resource_token_type(action_token);
        assert_eq!(
            verify(duplicate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidResourceTokenMetadata
        );
    }

    #[test]
    fn map_keys_metadata_and_option_results_are_verified_independently() {
        let map = MapType::new(ValueType::I32, ValueType::String);
        let option = nexa_bytecode::option_type(ValueType::String);
        let mut get = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(map.type_id), ValueType::I32],
                result: Some(ValueType::Named(option.type_id)),
            },
            4,
        );
        get.set_root(0)
            .unwrap()
            .set_root(3)
            .unwrap()
            .emit(Instruction::MapGet {
                source: 0,
                key: 1,
                result_type: option.type_id,
                dst: 2,
            })
            .emit(Instruction::Return { source: 2 });
        let mut get = get.finish().unwrap();
        get.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false, false, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, false, false, true],
            },
        ];
        let mut valid = ModuleBuilder::new();
        valid
            .map_type(map)
            .enum_type(option.clone())
            .function(get.clone());
        let verified = verify(valid.finish(), VerifierLimits::default());
        assert!(verified.is_ok(), "{:?}", verified.err());

        let opaque = ValueType::Named(StableId::from_name("EntityHandle"));
        let mut opaque_key = ModuleBuilder::new();
        opaque_key.host_import(HostImport {
            stable_id: StableId::from_name("host.entity"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: vec![opaque],
            result: Some(opaque),
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        });
        opaque_key.map_type(MapType::new(opaque, ValueType::I32));
        assert!(verify(opaque_key.finish(), VerifierLimits::default()).is_ok());

        let mut invalid_key = ModuleBuilder::new();
        invalid_key.map_type(MapType::new(ValueType::Bool, ValueType::I32));
        assert_eq!(
            verify(invalid_key.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidMapMetadata
        );

        let mut forged = ModuleBuilder::new();
        forged.map_type(MapType {
            type_id: StableId::from_name("forged-map"),
            key: ValueType::I32,
            value: ValueType::String,
        });
        assert_eq!(
            verify(forged.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidMapMetadata
        );

        get.code[0] = Instruction::MapGet {
            source: 0,
            key: 1,
            result_type: StableId::from_name("wrong-option"),
            dst: 2,
        };
        let mut wrong_result = ModuleBuilder::new();
        wrong_result.map_type(map).enum_type(option).function(get);
        assert!(matches!(
            verify(wrong_result.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::EnumTypeOutOfRange(_)
        ));
    }

    #[test]
    fn state_metadata_allows_only_verified_persistent_value_closures() {
        let store = StableId::from_name("Store");
        let wrapper = StableId::from_name("Wrapper");
        let node = StableId::from_name("Node");
        let handle = StateHandleType::new(ValueType::Named(store));
        let mut valid = ModuleBuilder::new();
        valid
            .struct_type(StructType {
                type_id: wrapper,
                fields: vec![StructField {
                    stable_id: StableId::from_parts(&["Wrapper", "::value"]),
                    ty: ValueType::String,
                }],
            })
            .state_handle_type(handle)
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: store,
                    version: 1,
                    fields: vec![
                        StateField {
                            stable_id: StableId::from_parts(&["Store", "::wrapper"]),
                            ty: ValueType::Named(wrapper),
                        },
                        StateField {
                            stable_id: StableId::from_parts(&["Store", "::next"]),
                            ty: ValueType::Named(handle.type_id),
                        },
                    ],
                }],
            });
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let mut nested_class = ModuleBuilder::new();
        nested_class
            .class_type(ClassType {
                type_id: node,
                fields: Vec::new(),
            })
            .struct_type(StructType {
                type_id: wrapper,
                fields: vec![StructField {
                    stable_id: StableId::from_parts(&["Wrapper", "::node"]),
                    ty: ValueType::Named(node),
                }],
            })
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: store,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: StableId::from_parts(&["Store", "::wrapper"]),
                        ty: ValueType::Named(wrapper),
                    }],
                }],
            });
        assert_eq!(
            verify(nested_class.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidStateMetadata
        );

        let mut forged_handle = ModuleBuilder::new();
        forged_handle
            .state_handle_type(StateHandleType {
                type_id: StableId::from_name("forged"),
                target: ValueType::Named(store),
            })
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: store,
                    version: 1,
                    fields: Vec::new(),
                }],
            });
        assert_eq!(
            verify(forged_handle.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidStateMetadata
        );
    }

    #[test]
    fn rejects_source_map_ranges_outside_their_function() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        function.emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        module.source_map([SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 2,
            span: SourceSpan::new(FileId(1), 2, 3),
        }]);

        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidSourceMap
        );
    }

    #[test]
    fn rejects_non_scalar_rune_constants() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Rune),
            },
            1,
        );
        function
            .emit(Instruction::LoadRune {
                dst: 0,
                value: 0xD800,
            })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidRune(0xD800)
        );
    }

    #[test]
    fn rejects_bad_jump_type_and_forged_root_bitmap() {
        let signature = Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        };
        let mut function = FunctionBuilder::new(signature.clone(), 1);
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 1 })
            .emit(Instruction::Jump { target: 99 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::JumpOutOfRange(99)
        ));

        let mut function = FunctionBuilder::new(signature, 1);
        function
            .set_root(0)
            .unwrap()
            .emit(Instruction::LoadI32 { dst: 0, value: 1 })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ForgedRoot(0)
        ));

        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            1,
        );
        function.emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(function.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::MissingRoot(0)
        ));

        let eight_i32 = vec![ValueType::I32; 8];
        let mut target_function = FunctionBuilder::new(
            Signature {
                parameters: eight_i32.clone(),
                result: Some(ValueType::I32),
            },
            8,
        );
        target_function.emit(Instruction::Return { source: 0 });
        let mut out_of_range_call = FunctionBuilder::new(
            Signature {
                parameters: eight_i32,
                result: Some(ValueType::I32),
            },
            8,
        );
        out_of_range_call
            .emit(Instruction::Call {
                function: 0,
                args_base: 1,
                args_count: 8,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(target_function.finish().unwrap());
        module.function(out_of_range_call.finish().unwrap());
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::RegisterOutOfRange(8)
        ));

        let mut missing_callee = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        missing_callee
            .emit(Instruction::Call {
                function: 99,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module.function(missing_callee.finish().unwrap());
        let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.kind, VerifyErrorKind::FunctionOutOfRange(99));
    }

    #[test]
    fn immediate_wcet_requires_and_consumes_static_loop_bounds() {
        fn immediate_loop(bound: Option<u32>) -> nexa_bytecode::Module {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: Some(ValueType::I32),
                },
                2,
            );
            function
                .effect(FunctionEffect::Immediate)
                .emit(Instruction::LoadI32 { dst: 0, value: 1 })
                .emit(Instruction::LoadBool {
                    dst: 1,
                    value: false,
                })
                .emit(Instruction::JumpIfFalse {
                    condition: 1,
                    target: 5,
                })
                .emit(Instruction::Safepoint)
                .emit(Instruction::Jump { target: 2 })
                .emit(Instruction::Return { source: 0 });
            if let Some(bound) = bound {
                function.loop_bound(4, bound);
            }
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            module.finish()
        }

        assert!(matches!(
            verify(immediate_loop(None), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ImmediateCostLimit
        ));
        assert!(verify(immediate_loop(Some(3)), VerifierLimits::default()).is_ok());

        let large_loop = immediate_loop(Some(1_025));
        assert_eq!(
            verify(large_loop.clone(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidLoopBound(4)
        );
        assert!(
            verify(
                large_loop,
                VerifierLimits {
                    max_immediate_cost: 10_000,
                    ..VerifierLimits::default()
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn immediate_wcet_uses_a_bounded_work_stack_for_deep_control_flow() {
        fn deep_immediate(instruction_count: u32) -> nexa_bytecode::Module {
            assert!(instruction_count >= 2);
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: Some(ValueType::I32),
                },
                1,
            );
            function.effect(FunctionEffect::Immediate);
            for value in 0..instruction_count - 1 {
                function.emit(Instruction::LoadI32 {
                    dst: 0,
                    value: i32::try_from(value).unwrap(),
                });
            }
            function.emit(Instruction::Return { source: 0 });
            let mut module = ModuleBuilder::new();
            module.function(function.finish().unwrap());
            module.finish()
        }

        const DEEP_INSTRUCTIONS: u32 = 16_384;
        const STATE_LIMIT: u32 = 4_096;
        let module = deep_immediate(DEEP_INSTRUCTIONS);
        assert_eq!(
            verify(module.clone(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ImmediateCostLimit
        );
        assert!(
            verify(
                module.clone(),
                VerifierLimits {
                    max_immediate_cost: DEEP_INSTRUCTIONS,
                    max_wcet_states: DEEP_INSTRUCTIONS,
                    ..VerifierLimits::default()
                },
            )
            .is_ok()
        );

        let error = verify(
            module,
            VerifierLimits {
                max_immediate_cost: u32::MAX,
                max_wcet_states: STATE_LIMIT,
                ..VerifierLimits::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.instruction, Some(STATE_LIMIT as usize));
        assert_eq!(error.kind, VerifyErrorKind::WcetComplexityLimit);
    }

    fn deep_call_chain(
        function_count: usize,
        root_effect: FunctionEffect,
        cycle: bool,
    ) -> nexa_bytecode::Module {
        assert!(function_count >= 2);
        let mut module = ModuleBuilder::new();
        for index in 0..function_count {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: Some(ValueType::I32),
                },
                1,
            );
            if index == 0 {
                function.effect(root_effect);
            }
            let callee = if index + 1 < function_count {
                Some(u32::try_from(index + 1).unwrap())
            } else {
                cycle.then_some(0)
            };
            if let Some(callee) = callee {
                function.emit(Instruction::Call {
                    function: callee,
                    args_base: 0,
                    args_count: 0,
                    dst: 0,
                });
            } else {
                function.emit(Instruction::LoadI32 { dst: 0, value: 7 });
            }
            function.emit(Instruction::Return { source: 0 });
            module.function(function.finish().unwrap());
        }
        module.finish()
    }

    fn nested_defer_module(root_effect: FunctionEffect) -> nexa_bytecode::Module {
        let mut root = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        root.effect(root_effect).emit(Instruction::DeferPush {
            function: 1,
            args_base: 0,
            args_count: 0,
        });
        if root_effect == FunctionEffect::Migration {
            root.emit(Instruction::StateFinish);
        }
        root.emit(Instruction::ReturnVoid);

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
        nested_cleanup
            .effect(FunctionEffect::Cleanup)
            .emit(Instruction::CleanupReturn);

        let mut module = ModuleBuilder::new();
        module.function(root.finish().unwrap());
        module.function(direct_cleanup.finish().unwrap());
        module.function(nested_cleanup.finish().unwrap());
        module.finish()
    }

    #[test]
    fn direct_and_nested_defer_edges_contribute_to_depth_and_immediate_wcet() {
        let module = nested_defer_module(FunctionEffect::Immediate);
        assert_eq!(
            verify(
                module.clone(),
                VerifierLimits {
                    max_immediate_cost: 6,
                    ..VerifierLimits::default()
                },
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::ImmediateCostLimit
        );

        let verified = verify(
            module,
            VerifierLimits {
                max_immediate_cost: 7,
                ..VerifierLimits::default()
            },
        )
        .expect("the exact nested-defer cost and depth must verify");
        assert_eq!(verified.module().functions[0].max_static_call_depth, 3);
        assert_eq!(verified.module().functions[1].max_static_call_depth, 2);
        assert_eq!(verified.module().functions[2].max_static_call_depth, 1);
    }

    #[test]
    fn deep_immediate_call_chain_uses_iterative_depth_and_wcet_evaluation() {
        const FUNCTION_COUNT: usize = 10_000;
        let module = deep_call_chain(FUNCTION_COUNT, FunctionEffect::Immediate, false);
        assert_eq!(
            verify(module.clone(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::ImmediateCostLimit
        );
        let verified = verify(
            module,
            VerifierLimits {
                max_immediate_cost: 25_000,
                ..VerifierLimits::default()
            },
        )
        .expect("a relaxed limit verifies the deep acyclic call chain");
        assert_eq!(
            verified.module().functions[0].max_static_call_depth,
            u16::try_from(FUNCTION_COUNT).unwrap()
        );
        assert_eq!(
            verified
                .module()
                .functions
                .last()
                .unwrap()
                .max_static_call_depth,
            1
        );
    }

    #[test]
    fn deep_call_cycles_preserve_ordinary_and_immediate_recursion_semantics() {
        const FUNCTION_COUNT: usize = 10_000;
        let ordinary = deep_call_chain(FUNCTION_COUNT, FunctionEffect::Ordinary, true);
        let verified = verify(ordinary, VerifierLimits::default())
            .expect("pure Ordinary recursion retains its saturated depth contract");
        assert!(
            verified
                .module()
                .functions
                .iter()
                .all(|function| function.max_static_call_depth == u16::MAX)
        );

        let immediate = deep_call_chain(FUNCTION_COUNT, FunctionEffect::Immediate, true);
        let error = verify(immediate, VerifierLimits::default()).unwrap_err();
        assert_eq!(error.function, 0);
        assert_eq!(error.instruction, None);
        assert_eq!(error.kind, VerifyErrorKind::ImmediateRecursion);
    }

    #[test]
    fn immediate_functions_accept_only_immediate_safe_ordinary_call_closures() {
        let mut immediate = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        immediate
            .effect(FunctionEffect::Immediate)
            .emit(Instruction::Call {
                function: 1,
                args_base: 1,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::Return { source: 0 });
        let mut pure_ordinary = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        pure_ordinary
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::Return { source: 0 });
        let mut module = ModuleBuilder::new();
        module.function(immediate.finish().unwrap());
        module.function(pure_ordinary.finish().unwrap());
        assert!(verify(module.finish(), VerifierLimits::default()).is_ok());

        let mut immediate = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::I32),
            },
            2,
        );
        immediate
            .effect(FunctionEffect::Immediate)
            .set_root(0)
            .unwrap()
            .emit(Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 1,
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });
        let mut runtime_sized_ordinary = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::I32),
            },
            2,
        );
        runtime_sized_ordinary
            .set_root(0)
            .unwrap()
            .emit(Instruction::StringLen { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 });
        let mut immediate = immediate.finish().unwrap();
        immediate.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, false],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(immediate);
        module.function(runtime_sized_ordinary.finish().unwrap());
        let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
        assert_eq!(error.function, 1);
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.kind, VerifyErrorKind::InvalidEffect);
    }

    #[test]
    fn migration_call_closure_cannot_reach_host_calls_indirectly() {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        migration
            .effect(FunctionEffect::Migration)
            .emit(Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);

        let mut ordinary_helper = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        ordinary_helper
            .emit(Instruction::HostCall {
                import: 0,
                args_base: 0,
                args_count: 0,
                dst: 0,
            })
            .emit(Instruction::ReturnVoid);

        let mut module = ModuleBuilder::new();
        module.function(
            migration
                .finish()
                .expect("migration fixture is well formed"),
        );
        module.function(
            ordinary_helper
                .finish()
                .expect("ordinary helper fixture is well formed"),
        );
        module.host_import(HostImport {
            stable_id: StableId::from_name("host.indirect"),
            declaration_fingerprint: [0; 32],
            capabilities: Vec::new(),
            parameters: Vec::new(),
            result: None,
            mode: HostCallMode::Immediate,
            fuel_cost: 1,
            async_result: None,
        });

        let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
        assert_eq!(error.function, 1);
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.kind, VerifyErrorKind::InvalidEffect);
    }

    #[test]
    fn migration_functions_reject_direct_heap_dependent_instructions() {
        let mut migration = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        migration
            .effect(FunctionEffect::Migration)
            .set_root(0)
            .unwrap()
            .emit(Instruction::LoadString { dst: 0, string: 0 })
            .emit(Instruction::StateFinish)
            .emit(Instruction::ReturnVoid);
        let mut migration = migration.finish().unwrap();
        migration.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 2,
                bitmap: vec![true],
            },
        ];

        let mut module = ModuleBuilder::new();
        module.string("migration cannot allocate");
        module.function(migration);

        let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
        assert_eq!(error.function, 0);
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.kind, VerifyErrorKind::InvalidEffect);
    }

    fn heap_dependent_class_helper(type_id: StableId) -> Function {
        let mut helper = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        helper
            .set_root(0)
            .unwrap()
            .emit(Instruction::ClassNew {
                type_id,
                fields_base: 0,
                fields_count: 0,
                dst: 0,
            })
            .emit(Instruction::ReturnVoid);
        let mut helper = helper.finish().unwrap();
        helper.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false],
            },
        ];
        helper
    }

    #[test]
    fn migration_call_and_defer_closures_reject_heap_dependent_ordinary_helpers() {
        let type_id = StableId::from_name("HeapDependentHelperValue");
        let helper = heap_dependent_class_helper(type_id);
        let class_type = ClassType {
            type_id,
            fields: Vec::new(),
        };

        let mut ordinary = ModuleBuilder::new();
        ordinary
            .class_type(class_type.clone())
            .function(helper.clone());
        verify(ordinary.finish(), VerifierLimits::default())
            .expect("the helper is valid in an ordinary heap-backed execution");

        for instruction in [
            Instruction::Call {
                function: 1,
                args_base: 0,
                args_count: 0,
                dst: 0,
            },
            Instruction::DeferPush {
                function: 1,
                args_base: 0,
                args_count: 0,
            },
        ] {
            let mut migration = FunctionBuilder::new(
                Signature {
                    parameters: Vec::new(),
                    result: None,
                },
                0,
            );
            migration
                .effect(FunctionEffect::Migration)
                .emit(instruction)
                .emit(Instruction::StateFinish)
                .emit(Instruction::ReturnVoid);

            let mut module = ModuleBuilder::new();
            module
                .class_type(class_type.clone())
                .function(migration.finish().unwrap());
            module.function(helper.clone());

            let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
            assert_eq!(error.function, 1);
            assert_eq!(error.instruction, Some(0));
            assert_eq!(error.kind, VerifyErrorKind::InvalidEffect);
        }

        let mut task = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        task.effect(FunctionEffect::Task)
            .emit(Instruction::DeferPush {
                function: 1,
                args_base: 0,
                args_count: 0,
            })
            .emit(Instruction::ReturnVoid);
        let mut module = ModuleBuilder::new();
        module
            .class_type(class_type)
            .function(task.finish().unwrap());
        module.function(helper);

        let error = verify(module.finish(), VerifierLimits::default()).unwrap_err();
        assert_eq!(error.function, 1);
        assert_eq!(error.instruction, Some(0));
        assert_eq!(error.kind, VerifyErrorKind::InvalidEffect);
    }

    #[test]
    fn reload_metadata_is_verified_independently_of_the_compiler() {
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
        let migration = migration.finish().unwrap();

        let mut forged = ModuleBuilder::new();
        forged.function(migration.clone());
        let mut forged = forged.finish();
        forged.reload_metadata.migration_entry = None;
        assert!(matches!(
            verify(forged, VerifierLimits::default()).unwrap_err().kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut underreported = ModuleBuilder::new();
        underreported.function(migration.clone());
        let mut underreported = underreported.finish();
        underreported
            .reload_metadata
            .minimum_migration_limits
            .max_fuel = 0;
        assert!(matches!(
            verify(underreported, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut duplicate = ModuleBuilder::new();
        duplicate.function(migration.clone());
        duplicate.function(migration);
        assert!(matches!(
            verify(duplicate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let mut ordinary = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            0,
        );
        ordinary.emit(Instruction::ReturnVoid);
        let ordinary = ordinary.finish().unwrap();
        let mut invalid_activation = ModuleBuilder::new();
        invalid_activation.function(ordinary.clone());
        let mut invalid_activation = invalid_activation.finish();
        invalid_activation.reload_metadata.activation_entry = Some(0);
        assert!(matches!(
            verify(invalid_activation, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));

        let old_schema = nexa_bytecode::StateSchema::default();
        let old_fingerprint = old_schema.fingerprint();
        let host_hash = StableId::from_name("reload-test-host");
        let mut old = ModuleBuilder::new();
        old.metadata(host_hash, old_fingerprint)
            .function(ordinary.clone());
        let old = verify(old.finish(), VerifierLimits::default()).unwrap();
        let mut candidate = ModuleBuilder::new();
        let candidate_schema = StateSchema {
            types: vec![StateType {
                stable_id: StableId::from_name("reload-test-state"),
                version: 1,
                fields: Vec::new(),
            }],
        };
        let candidate_fingerprint = candidate_schema.fingerprint();
        candidate
            .metadata(host_hash, candidate_fingerprint)
            .state_schema(candidate_schema)
            .function(ordinary);
        let candidate = verify(candidate.finish(), VerifierLimits::default()).unwrap();
        assert!(matches!(
            verify_reload_transition(&old, &candidate).unwrap_err().kind,
            VerifyErrorKind::InvalidReloadMetadata
        ));
    }

    #[test]
    fn reload_metadata_rejects_underreported_nested_defer_depth() {
        let mut module = nested_defer_module(FunctionEffect::Migration);
        assert_eq!(
            module
                .reload_metadata
                .minimum_migration_limits
                .max_call_depth,
            3
        );
        module
            .reload_metadata
            .minimum_migration_limits
            .max_call_depth = 2;
        assert_eq!(
            verify(module, VerifierLimits::default()).unwrap_err().kind,
            VerifyErrorKind::InvalidReloadMetadata
        );
    }

    #[test]
    fn root_maps_are_exact_for_each_safepoint_program_counter() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Ref],
                result: Some(ValueType::Ref),
            },
            2,
        );
        function
            .set_root(0)
            .unwrap()
            .set_root(1)
            .unwrap()
            .emit(Instruction::Move { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 });
        let mut function = function.finish().unwrap();
        function.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![true, false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![false, true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.function(function.clone());
        assert!(verify(module.finish(), VerifierLimits::default()).is_ok());

        function.root_maps[0].bitmap[1] = true;
        let mut module = ModuleBuilder::new();
        module.function(function);
        assert!(matches!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidRootMap(0)
        ));
    }

    #[test]
    fn safepoints_are_strictly_increasing_and_exactly_required() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: None,
            },
            1,
        );
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::Move { dst: 0, source: 0 })
            .emit(Instruction::ReturnVoid);
        let function = function.finish().unwrap();
        assert_eq!(function.safepoints, vec![0, 2]);

        let verify_with = |safepoints| {
            let mut candidate = function.clone();
            candidate.safepoints = safepoints;
            let mut module = ModuleBuilder::new();
            module.function(candidate);
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind
        };

        assert_eq!(
            verify_with(vec![0, 1, 2]),
            VerifyErrorKind::InvalidSafepoint(1)
        );
        assert_eq!(
            verify_with(vec![0, 0, 2]),
            VerifyErrorKind::InvalidSafepoint(0)
        );
        assert_eq!(
            verify_with(vec![2, 0]),
            VerifyErrorKind::InvalidSafepoint(0)
        );
        assert_eq!(verify_with(vec![0]), VerifyErrorKind::MissingSafepoint(2));
    }

    #[test]
    fn scalar_to_string_requires_the_exact_scalar_type_and_produces_a_gc_root() {
        fn conversion_function(
            source_type: ValueType,
            instruction: Instruction,
        ) -> nexa_bytecode::Function {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type],
                    result: Some(ValueType::String),
                },
                2,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 1 });
            let mut function = function.finish().unwrap();
            let source_is_root = source_type.is_reference();
            function.root_bitmap[0] = source_is_root;
            function.root_bitmap[1] = true;
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: vec![source_is_root, false],
                },
                RootMap {
                    pc: 1,
                    bitmap: vec![false, true],
                },
            ];
            function
        }

        let cases = [
            (
                ValueType::I32,
                Instruction::I32ToString { dst: 1, source: 0 },
            ),
            (
                ValueType::I64,
                Instruction::I64ToString { dst: 1, source: 0 },
            ),
            (
                ValueType::F32,
                Instruction::F32ToString { dst: 1, source: 0 },
            ),
            (
                ValueType::F64,
                Instruction::F64ToString { dst: 1, source: 0 },
            ),
            (
                ValueType::Bool,
                Instruction::BoolToString { dst: 1, source: 0 },
            ),
            (
                ValueType::Rune,
                Instruction::RuneToString { dst: 1, source: 0 },
            ),
            (
                ValueType::String,
                Instruction::StringToString { dst: 1, source: 0 },
            ),
        ];
        for (source_type, instruction) in cases {
            let mut module = ModuleBuilder::new();
            module.function(conversion_function(source_type, instruction));
            assert!(verify(module.finish(), VerifierLimits::default()).is_ok());
        }

        let mut invalid = ModuleBuilder::new();
        invalid.function(conversion_function(
            ValueType::I32,
            Instruction::I64ToString { dst: 1, source: 0 },
        ));
        assert_eq!(
            verify(invalid.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );
    }

    #[test]
    fn string_build_accepts_only_initialized_scalar_windows() {
        fn build_function(
            parameters: Vec<ValueType>,
            parts_base: u16,
            parts_count: u16,
        ) -> nexa_bytecode::Function {
            let dst = u16::try_from(parameters.len()).unwrap();
            let registers = dst + 1;
            let entry_roots = parameters
                .iter()
                .map(|ty| ty.is_reference())
                .chain(std::iter::once(false))
                .collect::<Vec<_>>();
            let mut return_roots = vec![false; usize::from(registers)];
            return_roots[usize::from(dst)] = true;
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters,
                    result: Some(ValueType::String),
                },
                registers,
            );
            for (register, root) in entry_roots.iter().copied().enumerate() {
                if root {
                    function.set_root(u16::try_from(register).unwrap()).unwrap();
                }
            }
            function.set_root(dst).unwrap();
            function
                .emit(Instruction::StringBuild {
                    dst,
                    parts_base,
                    parts_count,
                })
                .emit(Instruction::Return { source: dst });
            let mut function = function.finish().unwrap();
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: entry_roots,
                },
                RootMap {
                    pc: 1,
                    bitmap: return_roots,
                },
            ];
            function
        }

        let mut valid = ModuleBuilder::new();
        valid.function(build_function(
            vec![
                ValueType::String,
                ValueType::I32,
                ValueType::F64,
                ValueType::Bool,
                ValueType::Rune,
            ],
            0,
            5,
        ));
        assert!(verify(valid.finish(), VerifierLimits::default()).is_ok());

        let array = nexa_bytecode::array_type(ValueType::I32);
        let mut aggregate = ModuleBuilder::new();
        aggregate
            .array_type(ArrayType::new(ValueType::I32))
            .function(build_function(vec![ValueType::Named(array)], 0, 1));
        assert_eq!(
            verify(aggregate.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );

        let mut out_of_range = ModuleBuilder::new();
        out_of_range.function(build_function(vec![ValueType::I32], u16::MAX, 1));
        assert_eq!(
            verify(out_of_range.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::RegisterOutOfRange(u16::MAX)
        );
    }

    #[test]
    fn scalar_ordering_requires_matching_numeric_operands() {
        fn ordering_function(
            source_type: ValueType,
            instruction: Instruction,
        ) -> nexa_bytecode::Function {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type, source_type],
                    result: Some(ValueType::Bool),
                },
                3,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 2 });
            function.finish().unwrap()
        }

        let cases = [
            (
                ValueType::I32,
                Instruction::CompareLtI32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::I64,
                Instruction::CompareLtI64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::F32,
                Instruction::CompareLtF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::F64,
                Instruction::CompareLtF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
        ];
        for (source_type, instruction) in cases {
            let mut module = ModuleBuilder::new();
            module.function(ordering_function(source_type, instruction));
            assert!(verify(module.finish(), VerifierLimits::default()).is_ok());
        }

        let mut invalid = ModuleBuilder::new();
        invalid.function(ordering_function(
            ValueType::I32,
            Instruction::CompareLtI64 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
        ));
        assert_eq!(
            verify(invalid.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );
    }

    #[test]
    fn scalar_remainder_requires_matching_numeric_operands() {
        fn remainder_function(
            source_type: ValueType,
            instruction: Instruction,
        ) -> nexa_bytecode::Function {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![source_type, source_type],
                    result: Some(source_type),
                },
                3,
            );
            function
                .emit(instruction)
                .emit(Instruction::Return { source: 2 });
            function.finish().unwrap()
        }

        let cases = [
            (
                ValueType::I32,
                Instruction::RemI32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::I64,
                Instruction::RemI64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::F32,
                Instruction::RemF32 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
            (
                ValueType::F64,
                Instruction::RemF64 {
                    dst: 2,
                    lhs: 0,
                    rhs: 1,
                },
            ),
        ];
        for (source_type, instruction) in cases {
            let mut module = ModuleBuilder::new();
            module.function(remainder_function(source_type, instruction));
            assert!(verify(module.finish(), VerifierLimits::default()).is_ok());
        }

        let mut invalid = ModuleBuilder::new();
        invalid.function(remainder_function(
            ValueType::I32,
            Instruction::RemI64 {
                dst: 2,
                lhs: 0,
                rhs: 1,
            },
        ));
        assert_eq!(
            verify(invalid.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FrozenIntrinsicKind {
        OptionIsSome,
        OptionIsNone,
        ResultIsOk,
        ResultIsErr,
        OptionUnwrapOr,
        ResultUnwrapOr,
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
        ArrayLen,
        ArrayIsEmpty,
        ArrayGet,
        ArrayPush,
        ArrayPop,
        ArrayReserve,
        ArrayCapacity,
        ArrayClear,
        ArrayShrinkToFit,
        MapLen,
        MapContains,
        MapGet,
        MapInsert,
        MapRemove,
        DebugAssert,
        DebugTrap,
    }

    impl FrozenIntrinsicKind {
        const ALL: [Self; 42] = [
            Self::OptionIsSome,
            Self::OptionIsNone,
            Self::ResultIsOk,
            Self::ResultIsErr,
            Self::OptionUnwrapOr,
            Self::ResultUnwrapOr,
            Self::F32Floor,
            Self::F64Floor,
            Self::F32Ceil,
            Self::F64Ceil,
            Self::F32Round,
            Self::F64Round,
            Self::F32Sqrt,
            Self::F64Sqrt,
            Self::F32Sin,
            Self::F64Sin,
            Self::F32Cos,
            Self::F64Cos,
            Self::StringContains,
            Self::StringStartsWith,
            Self::StringEndsWith,
            Self::StringLen,
            Self::StringByteLen,
            Self::StringSubstring,
            Self::StringTrim,
            Self::StringSplit,
            Self::ArrayLen,
            Self::ArrayIsEmpty,
            Self::ArrayGet,
            Self::ArrayPush,
            Self::ArrayPop,
            Self::ArrayReserve,
            Self::ArrayCapacity,
            Self::ArrayClear,
            Self::ArrayShrinkToFit,
            Self::MapLen,
            Self::MapContains,
            Self::MapGet,
            Self::MapInsert,
            Self::MapRemove,
            Self::DebugAssert,
            Self::DebugTrap,
        ];
    }

    fn frozen_intrinsic_kind(intrinsic: StandardIntrinsic) -> FrozenIntrinsicKind {
        match intrinsic {
            StandardIntrinsic::OptionIsSome { .. } => FrozenIntrinsicKind::OptionIsSome,
            StandardIntrinsic::OptionIsNone { .. } => FrozenIntrinsicKind::OptionIsNone,
            StandardIntrinsic::ResultIsOk { .. } => FrozenIntrinsicKind::ResultIsOk,
            StandardIntrinsic::ResultIsErr { .. } => FrozenIntrinsicKind::ResultIsErr,
            StandardIntrinsic::OptionUnwrapOr { .. } => FrozenIntrinsicKind::OptionUnwrapOr,
            StandardIntrinsic::ResultUnwrapOr { .. } => FrozenIntrinsicKind::ResultUnwrapOr,
            StandardIntrinsic::F32Floor => FrozenIntrinsicKind::F32Floor,
            StandardIntrinsic::F64Floor => FrozenIntrinsicKind::F64Floor,
            StandardIntrinsic::F32Ceil => FrozenIntrinsicKind::F32Ceil,
            StandardIntrinsic::F64Ceil => FrozenIntrinsicKind::F64Ceil,
            StandardIntrinsic::F32Round => FrozenIntrinsicKind::F32Round,
            StandardIntrinsic::F64Round => FrozenIntrinsicKind::F64Round,
            StandardIntrinsic::F32Sqrt => FrozenIntrinsicKind::F32Sqrt,
            StandardIntrinsic::F64Sqrt => FrozenIntrinsicKind::F64Sqrt,
            StandardIntrinsic::F32Sin => FrozenIntrinsicKind::F32Sin,
            StandardIntrinsic::F64Sin => FrozenIntrinsicKind::F64Sin,
            StandardIntrinsic::F32Cos => FrozenIntrinsicKind::F32Cos,
            StandardIntrinsic::F64Cos => FrozenIntrinsicKind::F64Cos,
            StandardIntrinsic::StringContains => FrozenIntrinsicKind::StringContains,
            StandardIntrinsic::StringStartsWith => FrozenIntrinsicKind::StringStartsWith,
            StandardIntrinsic::StringEndsWith => FrozenIntrinsicKind::StringEndsWith,
            StandardIntrinsic::StringLen => FrozenIntrinsicKind::StringLen,
            StandardIntrinsic::StringByteLen => FrozenIntrinsicKind::StringByteLen,
            StandardIntrinsic::StringSubstring => FrozenIntrinsicKind::StringSubstring,
            StandardIntrinsic::StringTrim => FrozenIntrinsicKind::StringTrim,
            StandardIntrinsic::StringSplit => FrozenIntrinsicKind::StringSplit,
            StandardIntrinsic::ArrayLen { .. } => FrozenIntrinsicKind::ArrayLen,
            StandardIntrinsic::ArrayIsEmpty { .. } => FrozenIntrinsicKind::ArrayIsEmpty,
            StandardIntrinsic::ArrayGet { .. } => FrozenIntrinsicKind::ArrayGet,
            StandardIntrinsic::ArrayPush { .. } => FrozenIntrinsicKind::ArrayPush,
            StandardIntrinsic::ArrayPop { .. } => FrozenIntrinsicKind::ArrayPop,
            StandardIntrinsic::ArrayReserve { .. } => FrozenIntrinsicKind::ArrayReserve,
            StandardIntrinsic::ArrayCapacity { .. } => FrozenIntrinsicKind::ArrayCapacity,
            StandardIntrinsic::ArrayClear { .. } => FrozenIntrinsicKind::ArrayClear,
            StandardIntrinsic::ArrayShrinkToFit { .. } => FrozenIntrinsicKind::ArrayShrinkToFit,
            StandardIntrinsic::MapLen { .. } => FrozenIntrinsicKind::MapLen,
            StandardIntrinsic::MapContains { .. } => FrozenIntrinsicKind::MapContains,
            StandardIntrinsic::MapGet { .. } => FrozenIntrinsicKind::MapGet,
            StandardIntrinsic::MapInsert { .. } => FrozenIntrinsicKind::MapInsert,
            StandardIntrinsic::MapRemove { .. } => FrozenIntrinsicKind::MapRemove,
            StandardIntrinsic::DebugAssert => FrozenIntrinsicKind::DebugAssert,
            StandardIntrinsic::DebugTrap => FrozenIntrinsicKind::DebugTrap,
        }
    }

    struct FrozenIntrinsicSpec {
        intrinsic: StandardIntrinsic,
        arguments: Vec<ValueType>,
        result: ValueType,
    }

    #[allow(clippy::too_many_lines)]
    fn frozen_intrinsic_specs() -> Vec<FrozenIntrinsicSpec> {
        let value = ValueType::I32;
        let key = ValueType::String;
        let option = ValueType::Named(nexa_bytecode::option_type(value).type_id);
        let result = ValueType::Named(nexa_bytecode::result_type(value, key).type_id);
        let array = ValueType::Named(nexa_bytecode::array_type(value));
        let string_array = ValueType::Named(nexa_bytecode::array_type(ValueType::String));
        let map = ValueType::Named(nexa_bytecode::map_type(key, value));
        let spec = |intrinsic, arguments, result| FrozenIntrinsicSpec {
            intrinsic,
            arguments,
            result,
        };
        vec![
            spec(
                StandardIntrinsic::OptionIsSome { value },
                vec![option],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::OptionIsNone { value },
                vec![option],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ResultIsOk {
                    success: value,
                    error: key,
                },
                vec![result],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ResultIsErr {
                    success: value,
                    error: key,
                },
                vec![result],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::OptionUnwrapOr { value },
                vec![option, value],
                value,
            ),
            spec(
                StandardIntrinsic::ResultUnwrapOr {
                    success: value,
                    error: key,
                },
                vec![result, value],
                value,
            ),
            spec(
                StandardIntrinsic::F32Floor,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Floor,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::F32Ceil,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Ceil,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::F32Round,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Round,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::F32Sqrt,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Sqrt,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::F32Sin,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Sin,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::F32Cos,
                vec![ValueType::F32],
                ValueType::F32,
            ),
            spec(
                StandardIntrinsic::F64Cos,
                vec![ValueType::F64],
                ValueType::F64,
            ),
            spec(
                StandardIntrinsic::StringContains,
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::StringStartsWith,
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::StringEndsWith,
                vec![ValueType::String, ValueType::String],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::StringLen,
                vec![ValueType::String],
                ValueType::I32,
            ),
            spec(
                StandardIntrinsic::StringByteLen,
                vec![ValueType::String],
                ValueType::I32,
            ),
            spec(
                StandardIntrinsic::StringSubstring,
                vec![ValueType::String, ValueType::I32, ValueType::I32],
                ValueType::String,
            ),
            spec(
                StandardIntrinsic::StringTrim,
                vec![ValueType::String],
                ValueType::String,
            ),
            spec(
                StandardIntrinsic::StringSplit,
                vec![ValueType::String, ValueType::String],
                string_array,
            ),
            spec(
                StandardIntrinsic::ArrayLen { element: value },
                vec![array],
                ValueType::I32,
            ),
            spec(
                StandardIntrinsic::ArrayIsEmpty { element: value },
                vec![array],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ArrayGet { element: value },
                vec![array, ValueType::I32],
                option,
            ),
            spec(
                StandardIntrinsic::ArrayPush { element: value },
                vec![array, value],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ArrayPop { element: value },
                vec![array],
                value,
            ),
            spec(
                StandardIntrinsic::ArrayReserve { element: value },
                vec![array, ValueType::I32],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ArrayCapacity { element: value },
                vec![array],
                ValueType::I32,
            ),
            spec(
                StandardIntrinsic::ArrayClear { element: value },
                vec![array],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::ArrayShrinkToFit { element: value },
                vec![array],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::MapLen { key, value },
                vec![map],
                ValueType::I32,
            ),
            spec(
                StandardIntrinsic::MapContains { key, value },
                vec![map, key],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::MapGet { key, value },
                vec![map, key],
                option,
            ),
            spec(
                StandardIntrinsic::MapInsert { key, value },
                vec![map, key, value],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::MapRemove { key, value },
                vec![map, key],
                option,
            ),
            spec(
                StandardIntrinsic::DebugAssert,
                vec![ValueType::Bool],
                ValueType::Bool,
            ),
            spec(
                StandardIntrinsic::DebugTrap,
                vec![ValueType::String],
                ValueType::Bool,
            ),
        ]
    }

    fn frozen_intrinsic_spec(intrinsic: StandardIntrinsic) -> FrozenIntrinsicSpec {
        frozen_intrinsic_specs()
            .into_iter()
            .find(|spec| spec.intrinsic == intrinsic)
            .unwrap_or_else(|| panic!("missing frozen spec for {}", intrinsic.canonical_name()))
    }

    #[allow(clippy::too_many_lines)]
    fn intrinsic_module(spec: &FrozenIntrinsicSpec, effect: FunctionEffect) -> Module {
        let parameters = spec.arguments.clone();
        let result = spec.result;
        let mut module = ModuleBuilder::new();
        module
            .enum_type(nexa_bytecode::option_type(ValueType::I32))
            .enum_type(nexa_bytecode::result_type(
                ValueType::I32,
                ValueType::String,
            ))
            .array_type(ArrayType::new(ValueType::I32))
            .array_type(ArrayType::new(ValueType::String))
            .map_type(MapType::new(ValueType::String, ValueType::I32));
        let mut module = module.finish();
        let table = nexa_bytecode::layout::LayoutTable::for_module(&module).unwrap();
        let signature = Signature {
            parameters,
            result: Some(result),
        };
        let abi = nexa_bytecode::layout::FunctionAbi::for_signature(&table, &signature).unwrap();
        let args_base = abi.parameter_slots;
        let argument_count = abi
            .parameters
            .iter()
            .try_fold(0_u16, |total, parameter| {
                total.checked_add(parameter.slot_count)
            })
            .expect("frozen intrinsic physical arguments fit in u16");
        let dst = args_base + argument_count;
        let result_slots = abi.result.as_ref().unwrap().slot_count;
        let registers = dst + result_slots;
        let mut argument_offset = 0_u16;
        let mut code = Vec::with_capacity(abi.parameters.len() + 2);
        for parameter in &abi.parameters {
            let dst = args_base + argument_offset;
            code.push(if parameter.slot_count == 1 {
                Instruction::Move {
                    dst,
                    source: parameter.slot_offset,
                }
            } else {
                Instruction::CopyValue {
                    dst,
                    source: parameter.slot_offset,
                    slots: parameter.slot_count,
                }
            });
            argument_offset += parameter.slot_count;
        }
        let intrinsic_pc = u32::try_from(code.len()).unwrap();
        code.push(Instruction::StandardIntrinsic {
            intrinsic: spec.intrinsic,
            args_base,
            args_count: argument_count,
            dst,
        });
        let return_pc = u32::try_from(code.len()).unwrap();
        code.push(Instruction::Return { source: dst });
        let mut roots_at_entry = vec![false; usize::from(registers)];
        for parameter in &abi.parameters {
            let layout = table.layout_of(parameter.logical_type).unwrap();
            for (offset, root) in layout.gc_bitmap.iter().copied().enumerate() {
                roots_at_entry[usize::from(parameter.slot_offset) + offset] = root;
            }
        }
        let mut roots_at_intrinsic = vec![false; usize::from(registers)];
        let mut argument_offset = 0_u16;
        for parameter in &abi.parameters {
            let layout = table.layout_of(parameter.logical_type).unwrap();
            for (offset, root) in layout.gc_bitmap.iter().copied().enumerate() {
                roots_at_intrinsic[usize::from(args_base + argument_offset) + offset] = root;
            }
            argument_offset += parameter.slot_count;
        }
        let mut roots_at_return = vec![false; usize::from(registers)];
        let result_layout = table.layout_of(result).unwrap();
        for (offset, root) in result_layout.gc_bitmap.iter().copied().enumerate() {
            roots_at_return[usize::from(dst) + offset] = root;
        }
        let root_bitmap = roots_at_entry
            .iter()
            .zip(&roots_at_intrinsic)
            .zip(&roots_at_return)
            .map(|((entry, intrinsic), returned)| *entry || *intrinsic || *returned)
            .collect::<Vec<_>>();
        let mut root_maps = Vec::new();
        if argument_count != 0 {
            root_maps.push(RootMap {
                pc: 0,
                bitmap: roots_at_entry,
            });
        }
        root_maps.push(RootMap {
            pc: intrinsic_pc,
            bitmap: roots_at_intrinsic,
        });
        root_maps.push(RootMap {
            pc: return_pc,
            bitmap: roots_at_return,
        });
        let function = Function {
            signature,
            parameter_slots: abi.parameter_slots,
            registers,
            frame_bytes: u32::from(registers) * 8,
            root_bitmap,
            root_maps,
            safepoints: if argument_count == 0 {
                vec![intrinsic_pc, return_pc]
            } else {
                vec![0, intrinsic_pc, return_pc]
            },
            loop_bounds: Vec::new(),
            effect,
            max_static_call_depth: 1,
            code,
        };
        module.functions.push(function);
        module
    }

    #[test]
    fn every_standard_intrinsic_has_verified_types_metadata_roots_and_fuel() {
        let specs = frozen_intrinsic_specs();
        assert_eq!(
            specs
                .iter()
                .map(|spec| frozen_intrinsic_kind(spec.intrinsic))
                .collect::<Vec<_>>(),
            FrozenIntrinsicKind::ALL,
            "the frozen verifier specs must cover each intrinsic variant exactly once"
        );
        for spec in &specs {
            let intrinsic = spec.intrinsic;
            assert_eq!(
                usize::from(intrinsic.argument_count()),
                spec.arguments.len(),
                "{} arity changed from its independently frozen signature",
                intrinsic.canonical_name()
            );
            for (index, expected) in spec.arguments.iter().copied().enumerate() {
                assert_eq!(
                    intrinsic.argument_type(u16::try_from(index).unwrap()),
                    Some(expected),
                    "{} argument {index} changed from its independently frozen signature",
                    intrinsic.canonical_name()
                );
            }
            assert_eq!(
                intrinsic.argument_type(intrinsic.argument_count()),
                None,
                "{} exposes an argument beyond its frozen arity",
                intrinsic.canonical_name()
            );
            assert_eq!(
                intrinsic.result_type(),
                spec.result,
                "{} result changed from its independently frozen signature",
                intrinsic.canonical_name()
            );
            assert_ne!(intrinsic.base_fuel_cost(), 0);
            verify(
                intrinsic_module(spec, FunctionEffect::Ordinary),
                VerifierLimits::default(),
            )
            .unwrap_or_else(|error| panic!("{}: {error}", intrinsic.canonical_name()));
        }
    }

    #[test]
    fn standard_intrinsic_arity_effect_and_cost_guards_are_enforced() {
        let string_contains = frozen_intrinsic_spec(StandardIntrinsic::StringContains);
        let mut wrong_count = intrinsic_module(&string_contains, FunctionEffect::Ordinary);
        let intrinsic_pc = wrong_count.functions[0]
            .code
            .iter()
            .position(|instruction| matches!(instruction, Instruction::StandardIntrinsic { .. }))
            .unwrap();
        let Instruction::StandardIntrinsic { args_count, .. } =
            &mut wrong_count.functions[0].code[intrinsic_pc]
        else {
            unreachable!()
        };
        *args_count = 1;
        assert_eq!(
            verify(wrong_count, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );

        assert_eq!(
            verify(
                intrinsic_module(
                    &frozen_intrinsic_spec(StandardIntrinsic::ArrayPush {
                        element: ValueType::I32,
                    }),
                    FunctionEffect::Immediate,
                ),
                VerifierLimits::default(),
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::InvalidEffect
        );

        assert_eq!(
            verify(
                intrinsic_module(
                    &frozen_intrinsic_spec(StandardIntrinsic::StringContains),
                    FunctionEffect::Immediate,
                ),
                VerifierLimits::default(),
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::InvalidEffect
        );

        assert_eq!(
            verify(
                intrinsic_module(
                    &frozen_intrinsic_spec(StandardIntrinsic::F64Sin),
                    FunctionEffect::Immediate,
                ),
                VerifierLimits {
                    max_immediate_cost: 15,
                    ..VerifierLimits::default()
                },
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::ImmediateCostLimit
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn state_current_get_is_typed_rooted_and_effect_scoped() {
        let stable_id = StableId::from_name("repl::environment");
        let type_id = StableId::from_name("repl::Environment");
        let schema = StateSchema {
            types: vec![StateType {
                stable_id: type_id,
                version: 1,
                fields: Vec::new(),
            }],
        };
        let module = |effect: FunctionEffect, requested_type: StableId, forged_gc_root: bool| {
            let signature = Signature {
                parameters: Vec::new(),
                result: Some(ValueType::Named(requested_type)),
            };
            let mut function = FunctionBuilder::new(signature.clone(), 1);
            function
                .effect(effect)
                .emit(Instruction::StateCurrentGet {
                    stable_id,
                    type_id: requested_type,
                    dst: 0,
                })
                .emit(Instruction::Return { source: 0 });
            if forged_gc_root {
                function.set_root(0).unwrap();
            }
            let mut function = function.finish().unwrap();
            if forged_gc_root {
                function.root_maps[0].bitmap[0] = false;
            }
            let mut module = ModuleBuilder::new();
            module.state_schema(schema.clone()).class_type(ClassType {
                type_id,
                fields: Vec::new(),
            });
            let function = module.function(function);
            module.script_export(ScriptExport {
                stable_id: StableId::from_name("repl::cell_0"),
                function,
                signature,
                effect,
            });
            module.finish()
        };

        verify(
            module(FunctionEffect::Ordinary, type_id, false),
            VerifierLimits::default(),
        )
        .unwrap();
        verify(
            module(FunctionEffect::Task, type_id, false),
            VerifierLimits::default(),
        )
        .unwrap();
        for effect in [
            FunctionEffect::Immediate,
            FunctionEffect::Migration,
            FunctionEffect::Cleanup,
        ] {
            assert_eq!(
                verify(module(effect, type_id, false), VerifierLimits::default())
                    .unwrap_err()
                    .kind,
                VerifyErrorKind::InvalidEffect
            );
        }
        let unknown = StableId::from_name("repl::UnknownEnvironment");
        assert_eq!(
            verify(
                module(FunctionEffect::Ordinary, unknown, false),
                VerifierLimits::default(),
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::InvalidValueLayout(nexa_bytecode::layout::LayoutError::UnknownType(
                unknown
            ))
        );
        assert_eq!(
            verify(
                module(FunctionEffect::Ordinary, type_id, true),
                VerifierLimits::default(),
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::ForgedRoot(0)
        );
        let mut forged_export = module(FunctionEffect::Ordinary, type_id, false);
        forged_export.exports[0].effect = FunctionEffect::Task;
        assert_eq!(
            verify(forged_export, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidExportSignature
        );
        let mut mismatched_state_class = module(FunctionEffect::Ordinary, type_id, false);
        mismatched_state_class.class_types[0]
            .fields
            .push(StructField {
                stable_id: StableId::from_name("repl::Environment::unexpected"),
                ty: ValueType::Bool,
            });
        assert_eq!(
            verify(mismatched_state_class, VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidStateMetadata
        );
    }

    #[test]
    fn current_state_class_fields_carry_their_sorted_dense_slot() {
        let type_id = StableId::from_name("DenseState");
        let high_field = StableId(20);
        let low_field = StableId(10);
        let fields = vec![
            StructField {
                stable_id: high_field,
                ty: ValueType::I32,
            },
            StructField {
                stable_id: low_field,
                ty: ValueType::Bool,
            },
        ];
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::Named(type_id)],
                result: Some(ValueType::I32),
            },
            2,
        );
        function
            .emit(Instruction::ClassGet {
                source: 0,
                field: high_field,
                dst: 1,
            })
            .emit(Instruction::Return { source: 1 });
        let function = function.finish().unwrap();
        let mut module = ModuleBuilder::new();
        module
            .state_schema(StateSchema {
                types: vec![StateType {
                    stable_id: type_id,
                    version: 1,
                    fields: fields
                        .iter()
                        .map(|field| StateField {
                            stable_id: field.stable_id,
                            ty: field.ty,
                        })
                        .collect(),
                }],
            })
            .class_type(ClassType { type_id, fields })
            .function(function);
        let verified = verify(module.finish(), VerifierLimits::default()).unwrap();
        assert!(matches!(
            verified.resolved_operand(0, 0),
            ResolvedNominalOperand::ClassField {
                type_index: 0,
                index: 0,
                state_index: Some(1),
                ..
            }
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn migration_fields_and_replace_targets_require_exact_state_schema_nominals() {
        let owner = StableId::from_name("Owner");
        let other_owner = StableId::from_name("OtherOwner");
        let field = StableId::from_parts(&["Owner", "::value"]);
        let schema = StateSchema {
            types: vec![
                StateType {
                    stable_id: owner,
                    version: 1,
                    fields: vec![StateField {
                        stable_id: field,
                        ty: ValueType::I32,
                    }],
                },
                StateType {
                    stable_id: other_owner,
                    version: 1,
                    fields: Vec::new(),
                },
            ],
        };

        let old_field_module =
            |object_type: StableId, value_type: ValueType| -> nexa_bytecode::Module {
                let mut function = FunctionBuilder::new(
                    Signature {
                        parameters: vec![ValueType::Named(object_type)],
                        result: None,
                    },
                    2,
                );
                function
                    .effect(FunctionEffect::Migration)
                    .emit(Instruction::StateOldFieldGet {
                        object: 0,
                        field_id: field,
                        ty: value_type,
                        dst: 1,
                    })
                    .emit(Instruction::ReturnVoid);
                let mut function = function.finish().unwrap();
                function.root_maps = vec![
                    RootMap {
                        pc: 0,
                        bitmap: vec![false, false],
                    },
                    RootMap {
                        pc: 1,
                        bitmap: vec![false, false],
                    },
                ];
                let mut module = ModuleBuilder::new();
                module.state_schema(schema.clone()).function(function);
                module.reload_entries(Some(0), None);
                module.finish()
            };

        let verified = verify(
            old_field_module(owner, ValueType::I32),
            VerifierLimits::default(),
        )
        .unwrap();
        assert!(matches!(
            verified.resolved_operand(0, 0),
            ResolvedNominalOperand::StateField {
                type_index: 0,
                field_index: 0,
                sorted_index: 0,
            }
        ));
        for forged in [
            old_field_module(other_owner, ValueType::I32),
            old_field_module(owner, ValueType::Bool),
        ] {
            assert_eq!(
                verify(forged, VerifierLimits::default()).unwrap_err().kind,
                VerifyErrorKind::TypeMismatch
            );
        }

        let replace_module = |target_type: StableId, declare_struct: bool| {
            let mut function = FunctionBuilder::new(
                Signature {
                    parameters: vec![ValueType::Named(target_type)],
                    result: None,
                },
                1,
            );
            function
                .effect(FunctionEffect::Migration)
                .emit(Instruction::StateReplace {
                    old_id: StableId::from_name("old"),
                    target: 0,
                })
                .emit(Instruction::ReturnVoid);
            let mut function = function.finish().unwrap();
            function.root_maps = vec![
                RootMap {
                    pc: 0,
                    bitmap: vec![false],
                },
                RootMap {
                    pc: 1,
                    bitmap: vec![false],
                },
            ];
            let mut module = ModuleBuilder::new();
            module.state_schema(schema.clone()).function(function);
            if declare_struct {
                module.struct_type(StructType {
                    type_id: target_type,
                    fields: vec![StructField {
                        stable_id: StableId::from_name("PlainStruct::value"),
                        ty: ValueType::I32,
                    }],
                });
            }
            module.reload_entries(Some(0), None);
            module.finish()
        };

        verify(replace_module(owner, false), VerifierLimits::default()).unwrap();
        let non_state = StableId::from_name("PlainStruct");
        assert_eq!(
            verify(replace_module(non_state, true), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::TypeMismatch
        );
    }

    #[test]
    fn immediate_string_work_is_static_or_rejected_without_a_resource_profile() {
        let mut runtime_sized = FunctionBuilder::new(
            Signature {
                parameters: vec![ValueType::String],
                result: Some(ValueType::I32),
            },
            2,
        );
        runtime_sized
            .set_root(0)
            .unwrap()
            .emit(Instruction::StringLen { dst: 1, source: 0 })
            .emit(Instruction::Return { source: 1 })
            .effect(FunctionEffect::Immediate);
        let mut module = ModuleBuilder::new();
        module.function(runtime_sized.finish().unwrap());
        assert_eq!(
            verify(module.finish(), VerifierLimits::default())
                .unwrap_err()
                .kind,
            VerifyErrorKind::InvalidEffect
        );

        let mut bounded = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::String),
            },
            1,
        );
        bounded
            .set_root(0)
            .unwrap()
            .emit(Instruction::LoadString { dst: 0, string: 0 })
            .emit(Instruction::Return { source: 0 })
            .effect(FunctionEffect::Immediate);
        let mut bounded = bounded.finish().unwrap();
        bounded.root_maps = vec![
            RootMap {
                pc: 0,
                bitmap: vec![false],
            },
            RootMap {
                pc: 1,
                bitmap: vec![true],
            },
        ];
        let mut module = ModuleBuilder::new();
        module.string("123456789012345678901234567890123");
        module.function(bounded);
        let module = module.finish();
        assert_eq!(
            verify(
                module.clone(),
                VerifierLimits {
                    max_immediate_cost: 5,
                    ..VerifierLimits::default()
                },
            )
            .unwrap_err()
            .kind,
            VerifyErrorKind::ImmediateCostLimit
        );
        assert!(
            verify(
                module,
                VerifierLimits {
                    max_immediate_cost: 6,
                    ..VerifierLimits::default()
                },
            )
            .is_ok()
        );
    }
}
