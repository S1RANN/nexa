//! Authoritative physical value layouts (M5 WP19-WP22).
//!
//! A `LayoutTable` is derived deterministically from a module's type
//! sections; two derivations over the same module are identical. Layouts are
//! expressed in physical-slot units: a slot carries exactly one scalar, GC
//! reference, or host handle (WP22). Struct and Enum layouts flatten into
//! contiguous slot ranges instead of single heap-reference slots; Class and
//! collection types remain one GC-reference slot by design.
//!
//! Byte-level packing and alignment belong to the `ExecutableModule` stage;
//! at this layer `alignment` is always one slot, kept as an explicit field
//! so the wire schema does not change when packed layouts land.

use std::collections::BTreeMap;

use crate::{EnumType, Module, Signature, StructType, ValueType};
use nexa_core::StableId;

/// The only value categories a physical slot may carry (WP22).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalSlotKind {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Rune,
    /// String, Class, Array, Map, and Buffer references traced by the GC.
    GcReference,
    /// Snapshot, resource-token, state-handle, and host-request handles;
    /// rooted through their own registries, never through frame bitmaps.
    HostHandle,
    /// Host-defined, non-GC scalar whose interpretation stays behind the
    /// generated ABI boundary.
    Opaque,
}

/// How a value crosses assignment and call boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyStrategy {
    /// Single-slot scalar copy.
    Scalar,
    /// Multi-slot contiguous copy without heap traffic.
    SlotMemcpy,
    /// One-slot reference share; the referent is not duplicated.
    ReferenceShare,
}

/// How equality over the layout is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqualityStrategy {
    Bits,
    FloatAware,
    StringContent,
    StructFieldwise,
    EnumTagPayload,
    ReferenceIdentity,
}

/// How hashing over the layout is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashStrategy {
    Bits,
    StringContent,
    StructFieldwise,
    EnumTagPayload,
    ReferenceIdentity,
}

/// Slot placement of one struct field inside its parent range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldOffset {
    pub stable_id: StableId,
    pub logical_type: ValueType,
    pub offset: u16,
    pub slots: u16,
}

/// Per-variant payload layout; inactive payload slots never enter equality,
/// hashing, or root scanning (WP28).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariantLayout {
    pub stable_id: StableId,
    pub tag: u32,
    pub payload_type: Option<ValueType>,
    pub payload_slots: u16,
    pub payload_slot_kinds: Vec<PhysicalSlotKind>,
    pub payload_gc_bitmap: Vec<bool>,
}

/// Tag slot plus maximum payload range (WP28).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumLayout {
    pub tag_offset: u16,
    pub payload_offset: u16,
    pub payload_slots: u16,
    pub variants: Vec<EnumVariantLayout>,
}

/// Authoritative physical representation of one logical type (WP19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueLayout {
    pub logical_type: ValueType,
    pub physical_slots: u16,
    pub alignment: u16,
    pub slot_kinds: Vec<PhysicalSlotKind>,
    pub gc_bitmap: Vec<bool>,
    pub field_offsets: Vec<FieldOffset>,
    pub enum_layout: Option<EnumLayout>,
    pub copy_strategy: CopyStrategy,
    pub equality_strategy: EqualityStrategy,
    pub hash_strategy: HashStrategy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    UnknownType(StableId),
    RecursiveValueType(StableId),
    SlotOverflow(StableId),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownType(id) => write!(formatter, "unknown layout type {:016x}", id.0),
            Self::RecursiveValueType(id) => {
                write!(formatter, "recursive value type {:016x}", id.0)
            }
            Self::SlotOverflow(id) => write!(formatter, "slot overflow in type {:016x}", id.0),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Deterministic per-module layout table ordered by stable type ID (WP20).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutTable {
    named: BTreeMap<u64, ValueLayout>,
}

impl LayoutTable {
    /// Derives the table for every layoutable named type in the module.
    ///
    /// Recursive value types, dangling named types, and slot overflows are
    /// hard errors. Bytecode v7 carries the complete nominal closure,
    /// including explicit Host opaque scalar identities.
    pub fn for_module(module: &Module) -> Result<Self, LayoutError> {
        let mut table = Self::default();
        let context = LayoutContext { module };
        for struct_type in &module.struct_types {
            let layout = context.struct_layout(struct_type, &mut Vec::new())?;
            table.named.insert(struct_type.type_id.0, layout);
        }
        for enum_type in &module.enum_types {
            let layout = context.enum_value_layout(enum_type, &mut Vec::new())?;
            table.named.insert(enum_type.type_id.0, layout);
        }
        for class_type in &module.class_types {
            table.named.insert(
                class_type.type_id.0,
                reference_layout(ValueType::Named(class_type.type_id)),
            );
        }
        for array_type in &module.array_types {
            table.named.insert(
                array_type.type_id.0,
                reference_layout(ValueType::Named(array_type.type_id)),
            );
        }
        for map_type in &module.map_types {
            table.named.insert(
                map_type.type_id.0,
                reference_layout(ValueType::Named(map_type.type_id)),
            );
        }
        for buffer_type in &module.buffer_types {
            table.named.insert(
                buffer_type.type_id.0,
                reference_layout(ValueType::Named(buffer_type.type_id)),
            );
        }
        for snapshot_type in &module.snapshot_types {
            table.named.insert(
                snapshot_type.type_id.0,
                handle_layout(ValueType::Named(snapshot_type.type_id)),
            );
        }
        for token_type in &module.resource_token_types {
            table.named.insert(
                token_type.type_id.0,
                handle_layout(ValueType::Named(token_type.type_id)),
            );
        }
        for handle_type in &module.state_handle_types {
            table.named.insert(
                handle_type.type_id.0,
                handle_layout(ValueType::Named(handle_type.type_id)),
            );
        }
        for state_type in &module.state_schema.types {
            // State objects are rooted by the stateful registry, not by
            // frame bitmaps; in registers they travel as one handle slot.
            table.named.insert(
                state_type.stable_id.0,
                handle_layout(ValueType::Named(state_type.stable_id)),
            );
        }
        for type_id in &module.opaque_types {
            table
                .named
                .insert(type_id.0, opaque_layout(ValueType::Named(*type_id)));
        }
        Ok(table)
    }

    /// Layout of any logical type against this table.
    pub fn layout_of(&self, ty: ValueType) -> Result<ValueLayout, LayoutError> {
        match ty {
            ValueType::Named(id) => {
                if let Some(layout) = self.named.get(&id.0) {
                    return Ok(layout.clone());
                }
                builtin_named_layout(id).ok_or(LayoutError::UnknownType(id))
            }
            other => Ok(scalar_or_reference_layout(other)),
        }
    }

    /// Borrows a module-owned named layout without cloning its field,
    /// variant, or bitmap metadata. Runtime Host views use this at the ABI
    /// boundary so flattening an aggregate never allocates merely to inspect
    /// its already-verified layout.
    #[must_use]
    pub fn named_layout(&self, id: StableId) -> Option<&ValueLayout> {
        self.named.get(&id.0)
    }

    /// Returns only the physical width of a logical value. Unlike
    /// [`Self::layout_of`], this hot-boundary query never clones layout
    /// metadata.
    pub fn physical_slots(&self, ty: ValueType) -> Result<u16, LayoutError> {
        match ty {
            ValueType::Named(id) => self
                .named
                .get(&id.0)
                .map(|layout| layout.physical_slots)
                .or_else(|| builtin_named_layout(id).map(|layout| layout.physical_slots))
                .ok_or(LayoutError::UnknownType(id)),
            ValueType::I32
            | ValueType::I64
            | ValueType::F32
            | ValueType::F64
            | ValueType::Bool
            | ValueType::Rune
            | ValueType::String
            | ValueType::Ref => Ok(1),
        }
    }

    pub fn named_layouts(&self) -> impl Iterator<Item = (&u64, &ValueLayout)> {
        self.named.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.named.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.named.is_empty()
    }
}

struct LayoutContext<'module> {
    module: &'module Module,
}

#[derive(Clone, Debug)]
struct SlotExpansion {
    slot_kinds: Vec<PhysicalSlotKind>,
    gc_bitmap: Vec<bool>,
}

impl LayoutContext<'_> {
    /// Slot expansion of one logical type, flattening nested value types.
    fn expand(
        &self,
        ty: ValueType,
        visiting: &mut Vec<StableId>,
    ) -> Result<SlotExpansion, LayoutError> {
        match ty {
            ValueType::I32 => Ok(scalar_expansion(PhysicalSlotKind::I32)),
            ValueType::I64 => Ok(scalar_expansion(PhysicalSlotKind::I64)),
            ValueType::F32 => Ok(scalar_expansion(PhysicalSlotKind::F32)),
            ValueType::F64 => Ok(scalar_expansion(PhysicalSlotKind::F64)),
            ValueType::Bool => Ok(scalar_expansion(PhysicalSlotKind::Bool)),
            ValueType::Rune => Ok(scalar_expansion(PhysicalSlotKind::Rune)),
            ValueType::String | ValueType::Ref => {
                Ok(scalar_expansion(PhysicalSlotKind::GcReference))
            }
            ValueType::Named(id) => self.expand_named(id, visiting),
        }
    }

    fn expand_named(
        &self,
        id: StableId,
        visiting: &mut Vec<StableId>,
    ) -> Result<SlotExpansion, LayoutError> {
        if visiting.contains(&id) {
            return Err(LayoutError::RecursiveValueType(id));
        }
        if let Some(struct_type) = self
            .module
            .struct_types
            .iter()
            .find(|candidate| candidate.type_id == id)
        {
            visiting.push(id);
            let mut slot_kinds = Vec::new();
            let mut gc_bitmap = Vec::new();
            for field in &struct_type.fields {
                let field = self.expand(field.ty, visiting)?;
                slot_kinds.extend(field.slot_kinds);
                gc_bitmap.extend(field.gc_bitmap);
            }
            visiting.pop();
            return Ok(SlotExpansion {
                slot_kinds,
                gc_bitmap,
            });
        }
        if let Some(enum_type) = self
            .module
            .enum_types
            .iter()
            .find(|candidate| candidate.type_id == id)
        {
            visiting.push(id);
            let layout = self.enum_slot_expansion(enum_type, visiting)?;
            visiting.pop();
            return Ok(layout);
        }
        if self.is_reference_type(id) {
            return Ok(scalar_expansion(PhysicalSlotKind::GcReference));
        }
        if self.is_handle_type(id) {
            return Ok(scalar_expansion(PhysicalSlotKind::HostHandle));
        }
        if self.module.opaque_types.contains(&id) {
            return Ok(scalar_expansion(PhysicalSlotKind::Opaque));
        }
        if let Some(layout) = builtin_named_layout(id) {
            return Ok(SlotExpansion {
                slot_kinds: layout.slot_kinds,
                gc_bitmap: layout.gc_bitmap,
            });
        }
        Err(LayoutError::UnknownType(id))
    }

    fn enum_slot_expansion(
        &self,
        enum_type: &EnumType,
        visiting: &mut Vec<StableId>,
    ) -> Result<SlotExpansion, LayoutError> {
        // Tag slot followed by the widest payload range (WP28). A physical
        // position used by different scalar kinds is explicitly Opaque;
        // the active variant retains the exact kind and GC bitmap below.
        let mut payloads = Vec::with_capacity(enum_type.variants.len());
        for variant in &enum_type.variants {
            payloads.push(
                variant
                    .payload_type
                    .map(|payload_type| self.expand(payload_type, visiting))
                    .transpose()?
                    .unwrap_or_else(empty_expansion),
            );
        }
        let payload_slots = payloads
            .iter()
            .map(|payload| payload.slot_kinds.len())
            .max()
            .unwrap_or(0);
        let mut merged_kinds = Vec::with_capacity(payload_slots);
        let mut possible_gc = Vec::with_capacity(payload_slots);
        for offset in 0..payload_slots {
            let mut merged = None;
            let mut may_hold_gc = false;
            for payload in &payloads {
                let Some(kind) = payload.slot_kinds.get(offset).copied() else {
                    continue;
                };
                merged = Some(match merged {
                    None => kind,
                    Some(existing) if existing == kind => existing,
                    Some(_) => PhysicalSlotKind::Opaque,
                });
                may_hold_gc |= payload.gc_bitmap[offset];
            }
            merged_kinds.push(merged.expect("widest payload owns every merged offset"));
            possible_gc.push(may_hold_gc);
        }
        let mut slot_kinds = vec![PhysicalSlotKind::I32];
        slot_kinds.extend(merged_kinds);
        let mut gc_bitmap = vec![false];
        gc_bitmap.extend(possible_gc);
        Ok(SlotExpansion {
            slot_kinds,
            gc_bitmap,
        })
    }

    fn is_reference_type(&self, id: StableId) -> bool {
        self.module
            .class_types
            .iter()
            .any(|class_type| class_type.type_id == id)
            || self
                .module
                .array_types
                .iter()
                .any(|array_type| array_type.type_id == id)
            || self
                .module
                .map_types
                .iter()
                .any(|map_type| map_type.type_id == id)
            || self
                .module
                .buffer_types
                .iter()
                .any(|buffer_type| buffer_type.type_id == id)
    }

    fn is_handle_type(&self, id: StableId) -> bool {
        self.module
            .snapshot_types
            .iter()
            .any(|snapshot_type| snapshot_type.type_id == id)
            || self
                .module
                .resource_token_types
                .iter()
                .any(|token_type| token_type.type_id == id)
            || self
                .module
                .state_handle_types
                .iter()
                .any(|handle_type| handle_type.type_id == id)
            || self
                .module
                .state_schema
                .types
                .iter()
                .any(|state_type| state_type.stable_id == id)
    }

    fn struct_layout(
        &self,
        struct_type: &StructType,
        visiting: &mut Vec<StableId>,
    ) -> Result<ValueLayout, LayoutError> {
        visiting.push(struct_type.type_id);
        let mut slot_kinds = Vec::new();
        let mut gc_bitmap = Vec::new();
        let mut field_offsets = Vec::with_capacity(struct_type.fields.len());
        for field in &struct_type.fields {
            let field_slots = self.expand(field.ty, visiting)?;
            let offset = to_u16(slot_kinds.len(), struct_type.type_id)?;
            let slots = to_u16(field_slots.slot_kinds.len(), struct_type.type_id)?;
            field_offsets.push(FieldOffset {
                stable_id: field.stable_id,
                logical_type: field.ty,
                offset,
                slots,
            });
            slot_kinds.extend(field_slots.slot_kinds);
            gc_bitmap.extend(field_slots.gc_bitmap);
        }
        visiting.pop();
        let physical_slots = to_u16(slot_kinds.len(), struct_type.type_id)?;
        Ok(ValueLayout {
            logical_type: ValueType::Named(struct_type.type_id),
            physical_slots,
            alignment: 1,
            gc_bitmap,
            slot_kinds,
            field_offsets,
            enum_layout: None,
            copy_strategy: CopyStrategy::SlotMemcpy,
            equality_strategy: EqualityStrategy::StructFieldwise,
            hash_strategy: HashStrategy::StructFieldwise,
        })
    }

    fn enum_value_layout(
        &self,
        enum_type: &EnumType,
        visiting: &mut Vec<StableId>,
    ) -> Result<ValueLayout, LayoutError> {
        visiting.push(enum_type.type_id);
        let expansion = self.enum_slot_expansion(enum_type, visiting)?;
        let mut variants = Vec::with_capacity(enum_type.variants.len());
        for variant in &enum_type.variants {
            let payload = match variant.payload_type {
                Some(payload_type) => self.expand(payload_type, visiting)?,
                None => empty_expansion(),
            };
            variants.push(EnumVariantLayout {
                stable_id: variant.stable_id,
                tag: variant.tag,
                payload_type: variant.payload_type,
                payload_slots: to_u16(payload.slot_kinds.len(), enum_type.type_id)?,
                payload_slot_kinds: payload.slot_kinds,
                payload_gc_bitmap: payload.gc_bitmap,
            });
        }
        visiting.pop();
        let physical_slots = to_u16(expansion.slot_kinds.len(), enum_type.type_id)?;
        let payload_slots = physical_slots - 1;
        Ok(ValueLayout {
            logical_type: ValueType::Named(enum_type.type_id),
            physical_slots,
            alignment: 1,
            gc_bitmap: expansion.gc_bitmap,
            slot_kinds: expansion.slot_kinds,
            field_offsets: Vec::new(),
            enum_layout: Some(EnumLayout {
                tag_offset: 0,
                payload_offset: 1,
                payload_slots,
                variants,
            }),
            copy_strategy: CopyStrategy::SlotMemcpy,
            equality_strategy: EqualityStrategy::EnumTagPayload,
            hash_strategy: HashStrategy::EnumTagPayload,
        })
    }
}

fn scalar_expansion(kind: PhysicalSlotKind) -> SlotExpansion {
    SlotExpansion {
        slot_kinds: vec![kind],
        gc_bitmap: vec![matches!(kind, PhysicalSlotKind::GcReference)],
    }
}

fn empty_expansion() -> SlotExpansion {
    SlotExpansion {
        slot_kinds: Vec::new(),
        gc_bitmap: Vec::new(),
    }
}

fn to_u16(value: usize, owner: StableId) -> Result<u16, LayoutError> {
    u16::try_from(value).map_err(|_| LayoutError::SlotOverflow(owner))
}

fn scalar_or_reference_layout(ty: ValueType) -> ValueLayout {
    let (kind, copy, equality, hash) = match ty {
        ValueType::I32 => (
            PhysicalSlotKind::I32,
            CopyStrategy::Scalar,
            EqualityStrategy::Bits,
            HashStrategy::Bits,
        ),
        ValueType::I64 => (
            PhysicalSlotKind::I64,
            CopyStrategy::Scalar,
            EqualityStrategy::Bits,
            HashStrategy::Bits,
        ),
        ValueType::F32 => (
            PhysicalSlotKind::F32,
            CopyStrategy::Scalar,
            EqualityStrategy::FloatAware,
            HashStrategy::Bits,
        ),
        ValueType::F64 => (
            PhysicalSlotKind::F64,
            CopyStrategy::Scalar,
            EqualityStrategy::FloatAware,
            HashStrategy::Bits,
        ),
        ValueType::Bool => (
            PhysicalSlotKind::Bool,
            CopyStrategy::Scalar,
            EqualityStrategy::Bits,
            HashStrategy::Bits,
        ),
        ValueType::Rune => (
            PhysicalSlotKind::Rune,
            CopyStrategy::Scalar,
            EqualityStrategy::Bits,
            HashStrategy::Bits,
        ),
        ValueType::String => (
            PhysicalSlotKind::GcReference,
            CopyStrategy::ReferenceShare,
            EqualityStrategy::StringContent,
            HashStrategy::StringContent,
        ),
        ValueType::Ref | ValueType::Named(_) => (
            PhysicalSlotKind::GcReference,
            CopyStrategy::ReferenceShare,
            EqualityStrategy::ReferenceIdentity,
            HashStrategy::ReferenceIdentity,
        ),
    };
    ValueLayout {
        logical_type: ty,
        physical_slots: 1,
        alignment: 1,
        slot_kinds: vec![kind],
        gc_bitmap: vec![matches!(kind, PhysicalSlotKind::GcReference)],
        field_offsets: Vec::new(),
        enum_layout: None,
        copy_strategy: copy,
        equality_strategy: equality,
        hash_strategy: hash,
    }
}

fn reference_layout(ty: ValueType) -> ValueLayout {
    ValueLayout {
        logical_type: ty,
        physical_slots: 1,
        alignment: 1,
        slot_kinds: vec![PhysicalSlotKind::GcReference],
        gc_bitmap: vec![true],
        field_offsets: Vec::new(),
        enum_layout: None,
        copy_strategy: CopyStrategy::ReferenceShare,
        equality_strategy: EqualityStrategy::ReferenceIdentity,
        hash_strategy: HashStrategy::ReferenceIdentity,
    }
}

fn handle_layout(ty: ValueType) -> ValueLayout {
    ValueLayout {
        logical_type: ty,
        physical_slots: 1,
        alignment: 1,
        slot_kinds: vec![PhysicalSlotKind::HostHandle],
        gc_bitmap: vec![false],
        field_offsets: Vec::new(),
        enum_layout: None,
        copy_strategy: CopyStrategy::ReferenceShare,
        equality_strategy: EqualityStrategy::Bits,
        hash_strategy: HashStrategy::Bits,
    }
}

fn opaque_layout(ty: ValueType) -> ValueLayout {
    ValueLayout {
        logical_type: ty,
        physical_slots: 1,
        alignment: 1,
        slot_kinds: vec![PhysicalSlotKind::Opaque],
        gc_bitmap: vec![false],
        field_offsets: Vec::new(),
        enum_layout: None,
        copy_strategy: CopyStrategy::Scalar,
        equality_strategy: EqualityStrategy::Bits,
        hash_strategy: HashStrategy::Bits,
    }
}

/// Well-known runtime-builtin type names that never appear in module type
/// sections; the runtime's own value taxonomy treats them the same way.
fn builtin_named_layout(id: StableId) -> Option<ValueLayout> {
    if id == StableId::from_name("HostRequest") || id == StableId::from_name("HostError") {
        return Some(handle_layout(ValueType::Named(id)));
    }
    if id == StableId::from_name("StableId") {
        return Some(opaque_layout(ValueType::Named(id)));
    }
    if id == StableId::from_name("Buffer") {
        return Some(reference_layout(ValueType::Named(id)));
    }
    None
}

/// Physical placement of one parameter inside the callee's argument range
/// (WP23).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterAbi {
    pub logical_type: ValueType,
    pub slot_offset: u16,
    pub slot_count: u16,
    pub gc_bitmap: Vec<bool>,
}

/// Caller-allocated result range of one function (WP24).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultAbi {
    pub logical_type: ValueType,
    pub slot_count: u16,
    pub gc_bitmap: Vec<bool>,
}

/// Derived calling convention of one function: logical signature in,
/// contiguous physical slot ranges out. Calls copy arguments directly into
/// the target range and never construct temporary heap aggregates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionAbi {
    pub parameters: Vec<ParameterAbi>,
    pub parameter_slots: u16,
    pub parameter_gc_bitmap: Vec<bool>,
    pub result: Option<ResultAbi>,
}

impl FunctionAbi {
    /// Derives the physical ABI for one logical signature.
    pub fn for_signature(table: &LayoutTable, signature: &Signature) -> Result<Self, LayoutError> {
        let mut parameters = Vec::with_capacity(signature.parameters.len());
        let mut parameter_gc_bitmap = Vec::new();
        let mut cursor: usize = 0;
        for parameter in &signature.parameters {
            let layout = table.layout_of(*parameter)?;
            let slot_offset = u16::try_from(cursor).map_err(|_| overflow_owner(*parameter))?;
            parameters.push(ParameterAbi {
                logical_type: *parameter,
                slot_offset,
                slot_count: layout.physical_slots,
                gc_bitmap: layout.gc_bitmap.clone(),
            });
            parameter_gc_bitmap.extend_from_slice(&layout.gc_bitmap);
            cursor += usize::from(layout.physical_slots);
        }
        let parameter_slots = u16::try_from(cursor).map_err(|_| overflow_owner(ValueType::I32))?;
        let result = match signature.result {
            Some(result_type) => {
                let layout = table.layout_of(result_type)?;
                Some(ResultAbi {
                    logical_type: result_type,
                    slot_count: layout.physical_slots,
                    gc_bitmap: layout.gc_bitmap,
                })
            }
            None => None,
        };
        Ok(Self {
            parameters,
            parameter_slots,
            parameter_gc_bitmap,
            result,
        })
    }
}

const fn overflow_owner(ty: ValueType) -> LayoutError {
    match ty {
        ValueType::Named(id) => LayoutError::SlotOverflow(id),
        _ => LayoutError::SlotOverflow(StableId(0)),
    }
}

/// Deterministic per-module ABI table indexed by function position (WP23).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModuleAbi {
    functions: Vec<FunctionAbi>,
}

impl ModuleAbi {
    /// Derives every function ABI against the module's layout table.
    pub fn for_module(module: &Module, table: &LayoutTable) -> Result<Self, LayoutError> {
        let mut functions = Vec::with_capacity(module.functions.len());
        for function in &module.functions {
            functions.push(FunctionAbi::for_signature(table, &function.signature)?);
        }
        Ok(Self { functions })
    }

    #[must_use]
    pub fn function(&self, index: usize) -> Option<&FunctionAbi> {
        self.functions.get(index)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}
