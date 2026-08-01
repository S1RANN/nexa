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

use crate::{EnumType, Module, StructType, ValueType};
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
    pub offset: u16,
    pub slots: u16,
}

/// Per-variant payload layout; inactive payload slots never enter equality,
/// hashing, or root scanning (WP28).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnumVariantLayout {
    pub stable_id: StableId,
    pub tag: u32,
    pub payload_slots: u16,
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
    /// Derives the complete table for every named type in the module.
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
        Ok(table)
    }

    /// Layout of any logical type against this table.
    pub fn layout_of(&self, ty: ValueType) -> Result<ValueLayout, LayoutError> {
        match ty {
            ValueType::Named(id) => self
                .named
                .get(&id.0)
                .cloned()
                .ok_or(LayoutError::UnknownType(id)),
            other => Ok(scalar_or_reference_layout(other)),
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

impl LayoutContext<'_> {
    /// Slot expansion of one logical type, flattening nested value types.
    fn expand(
        &self,
        ty: ValueType,
        visiting: &mut Vec<StableId>,
    ) -> Result<Vec<PhysicalSlotKind>, LayoutError> {
        match ty {
            ValueType::I32 => Ok(vec![PhysicalSlotKind::I32]),
            ValueType::I64 => Ok(vec![PhysicalSlotKind::I64]),
            ValueType::F32 => Ok(vec![PhysicalSlotKind::F32]),
            ValueType::F64 => Ok(vec![PhysicalSlotKind::F64]),
            ValueType::Bool => Ok(vec![PhysicalSlotKind::Bool]),
            ValueType::Rune => Ok(vec![PhysicalSlotKind::Rune]),
            ValueType::String | ValueType::Ref => Ok(vec![PhysicalSlotKind::GcReference]),
            ValueType::Named(id) => self.expand_named(id, visiting),
        }
    }

    fn expand_named(
        &self,
        id: StableId,
        visiting: &mut Vec<StableId>,
    ) -> Result<Vec<PhysicalSlotKind>, LayoutError> {
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
            let mut slots = Vec::new();
            for field in &struct_type.fields {
                slots.extend(self.expand(field.ty, visiting)?);
            }
            visiting.pop();
            return Ok(slots);
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
            return Ok(vec![PhysicalSlotKind::GcReference]);
        }
        if self.is_handle_type(id) {
            return Ok(vec![PhysicalSlotKind::HostHandle]);
        }
        Err(LayoutError::UnknownType(id))
    }

    fn enum_slot_expansion(
        &self,
        enum_type: &EnumType,
        visiting: &mut Vec<StableId>,
    ) -> Result<Vec<PhysicalSlotKind>, LayoutError> {
        // Tag slot followed by the widest payload range (WP28).
        let mut payload = Vec::new();
        for variant in &enum_type.variants {
            if let Some(payload_type) = variant.payload_type {
                let slots = self.expand(payload_type, visiting)?;
                if slots.len() > payload.len() {
                    payload = slots;
                }
            }
        }
        let mut slots = vec![PhysicalSlotKind::I32];
        slots.extend(payload);
        Ok(slots)
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
        let mut field_offsets = Vec::with_capacity(struct_type.fields.len());
        for field in &struct_type.fields {
            let field_slots = self.expand(field.ty, visiting)?;
            let offset = to_u16(slot_kinds.len(), struct_type.type_id)?;
            let slots = to_u16(field_slots.len(), struct_type.type_id)?;
            field_offsets.push(FieldOffset {
                stable_id: field.stable_id,
                offset,
                slots,
            });
            slot_kinds.extend(field_slots);
        }
        visiting.pop();
        let physical_slots = to_u16(slot_kinds.len(), struct_type.type_id)?;
        let gc_bitmap = gc_bitmap(&slot_kinds);
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
        let slot_kinds = self.enum_slot_expansion(enum_type, visiting)?;
        let mut variants = Vec::with_capacity(enum_type.variants.len());
        for variant in &enum_type.variants {
            let payload_kinds = match variant.payload_type {
                Some(payload_type) => self.expand(payload_type, visiting)?,
                None => Vec::new(),
            };
            variants.push(EnumVariantLayout {
                stable_id: variant.stable_id,
                tag: variant.tag,
                payload_slots: to_u16(payload_kinds.len(), enum_type.type_id)?,
                payload_gc_bitmap: gc_bitmap(&payload_kinds),
            });
        }
        visiting.pop();
        let physical_slots = to_u16(slot_kinds.len(), enum_type.type_id)?;
        let payload_slots = physical_slots - 1;
        let gc_bitmap = gc_bitmap(&slot_kinds);
        Ok(ValueLayout {
            logical_type: ValueType::Named(enum_type.type_id),
            physical_slots,
            alignment: 1,
            gc_bitmap,
            slot_kinds,
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

fn gc_bitmap(slot_kinds: &[PhysicalSlotKind]) -> Vec<bool> {
    slot_kinds
        .iter()
        .map(|kind| matches!(kind, PhysicalSlotKind::GcReference))
        .collect()
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
