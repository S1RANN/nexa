use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use nexa_core::StableId;

pub use crate::model::AbiFingerprint;
use crate::model::{
    AbandonPolicy, CancelPolicy, ResolvedNamedType, ResolvedTypeKind, ResolvedTypeRef,
    ValidatedContract, ValidatedEnum, ValidatedField, ValidatedFunction, ValidatedHandle,
    ValidatedParameter, ValidatedStruct, ValidatedVariant,
};

/// The only accepted Contract source syntax version.
pub const CONTRACT_SYNTAX_VERSION: u16 = 3;
/// The canonical binary ABI descriptor version.
pub const ABI_DESCRIPTOR_VERSION: u16 = 2;

const DESCRIPTOR_PREFIX: &[u8] = b"nexa.contract-descriptor";
const TYPE_LAYOUT_DOMAIN: &str = "type-layout";
const HOST_FUNCTION_DOMAIN: &str = "host-function";
const NEXA_ENTRYPOINT_DOMAIN: &str = "nexa-entrypoint";
const CONTRACT_DOMAIN: &str = "full-contract";
const EFFECTIVE_CONTRACT_DOMAIN: &str = "effective-contract";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FunctionFingerprintKind {
    Host,
    Nexa,
}

pub(crate) struct FunctionFingerprintInput<'a> {
    pub(crate) kind: FunctionFingerprintKind,
    pub(crate) stable_id: StableId,
    pub(crate) name: &'a str,
    pub(crate) is_async: bool,
    pub(crate) parameters: &'a [ValidatedParameter],
    pub(crate) result: Option<&'a ResolvedTypeRef>,
    pub(crate) fuel_cost: u32,
    pub(crate) cancel_policy: CancelPolicy,
    pub(crate) abandon_policy: AbandonPolicy,
    pub(crate) capabilities: &'a [String],
}

/// Fingerprint for one independently addressable ABI declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarationFingerprint {
    pub name: String,
    pub stable_id: StableId,
    pub fingerprint: AbiFingerprint,
}

/// Complete canonical ABI descriptor for one validated Contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbiDescriptor {
    pub bytes: Vec<u8>,
    pub fingerprint: AbiFingerprint,
    pub type_layouts: Vec<DeclarationFingerprint>,
    pub host_functions: Vec<DeclarationFingerprint>,
    pub nexa_entrypoints: Vec<DeclarationFingerprint>,
}

impl AbiDescriptor {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// The Package-specific portions of a Contract which take part in its effective identity.
///
/// Names are resolved against a [`ValidatedContract`]. The resulting descriptor stores stable
/// identities, never these lookup strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveContractSelection {
    /// Shared types used directly by the Package, in addition to types reached from functions.
    pub referenced_types: BTreeSet<String>,
    /// Host functions which the Package can actually call.
    pub host_functions: BTreeSet<String>,
    /// Nexa entrypoints which the embedding Host requires this Package to implement.
    pub required_nexa_entrypoints: BTreeSet<String>,
    /// Declared optional Nexa entrypoints which this Package actually implements.
    pub present_optional_nexa_entrypoints: BTreeSet<String>,
}

impl EffectiveContractSelection {
    /// Selects the complete Contract. This is useful for tools which do not yet have a narrower
    /// per-Package usage view.
    #[must_use]
    pub fn complete(contract: &ValidatedContract) -> Self {
        Self {
            referenced_types: contract
                .handles
                .iter()
                .map(|declaration| declaration.name.clone())
                .chain(
                    contract
                        .structs
                        .iter()
                        .map(|declaration| declaration.name.clone()),
                )
                .chain(
                    contract
                        .enums
                        .iter()
                        .map(|declaration| declaration.name.clone()),
                )
                .collect(),
            host_functions: contract
                .host_functions
                .iter()
                .map(|function| function.name.clone())
                .collect(),
            required_nexa_entrypoints: contract
                .nexa_functions
                .iter()
                .map(|function| function.name.clone())
                .collect(),
            present_optional_nexa_entrypoints: BTreeSet::new(),
        }
    }
}

/// Canonical ABI identity for one Package's effective Contract view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveContractDescriptor {
    pub bytes: Vec<u8>,
    pub fingerprint: AbiFingerprint,
    pub shared_types: Vec<DeclarationFingerprint>,
    pub host_functions: Vec<DeclarationFingerprint>,
    pub required_nexa_entrypoints: Vec<DeclarationFingerprint>,
    pub present_optional_nexa_entrypoints: Vec<DeclarationFingerprint>,
}

impl EffectiveContractDescriptor {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectiveDescriptorError {
    UnknownType(String),
    UnknownHostFunction(String),
    UnknownNexaEntrypoint(String),
}

impl fmt::Display for EffectiveDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownType(name) => write!(formatter, "unknown Contract type `{name}`"),
            Self::UnknownHostFunction(name) => {
                write!(formatter, "unknown Contract Host function `{name}`")
            }
            Self::UnknownNexaEntrypoint(name) => {
                write!(formatter, "unknown Contract Nexa entrypoint `{name}`")
            }
        }
    }
}

impl std::error::Error for EffectiveDescriptorError {}

/// Builds the canonical descriptor and every declaration-level fingerprint.
#[must_use]
pub fn abi_descriptor(contract: &ValidatedContract) -> AbiDescriptor {
    let records = ContractRecords::new(contract);
    let bytes = encode_top_level(CONTRACT_DOMAIN, |encoder| {
        encoder.stable_id(contract.stable_id);
        encoder.string(&contract.name);
        encode_record_fingerprints(encoder, &records.types);
        encode_record_fingerprints(encoder, &records.host_functions);
        encode_record_fingerprints(encoder, &records.nexa_entrypoints);
    });

    AbiDescriptor {
        fingerprint: fingerprint(&bytes),
        bytes,
        type_layouts: declaration_fingerprints(&records.types),
        host_functions: declaration_fingerprints(&records.host_functions),
        nexa_entrypoints: declaration_fingerprints(&records.nexa_entrypoints),
    }
}

/// Alias for the full ABI Descriptor v2 Contract identity.
#[must_use]
pub fn contract_fingerprint(contract: &ValidatedContract) -> AbiFingerprint {
    abi_descriptor(contract).fingerprint
}

/// Returns one stable Type Layout Fingerprint for every handle, struct and enum.
#[must_use]
pub fn type_layout_fingerprints(contract: &ValidatedContract) -> Vec<DeclarationFingerprint> {
    declaration_fingerprints(&ContractRecords::new(contract).types)
}

/// Returns one stable Host Function Fingerprint for every declared Host function.
#[must_use]
pub fn host_function_fingerprints(contract: &ValidatedContract) -> Vec<DeclarationFingerprint> {
    declaration_fingerprints(&ContractRecords::new(contract).host_functions)
}

/// Returns one stable Nexa Entrypoint Fingerprint for every legal Nexa entrypoint.
#[must_use]
pub fn nexa_entrypoint_fingerprints(contract: &ValidatedContract) -> Vec<DeclarationFingerprint> {
    declaration_fingerprints(&ContractRecords::new(contract).nexa_entrypoints)
}

/// Builds the Package-specific descriptor. Unselected optional Nexa entrypoints and unrelated
/// shared types are deliberately absent from both the descriptor and its fingerprint.
#[allow(clippy::too_many_lines)]
pub fn effective_contract_descriptor(
    contract: &ValidatedContract,
    selection: &EffectiveContractSelection,
) -> Result<EffectiveContractDescriptor, EffectiveDescriptorError> {
    let records = ContractRecords::new(contract);
    let type_by_name = records
        .types
        .iter()
        .map(|record| (record.name.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let type_by_id = records
        .types
        .iter()
        .map(|record| (record.stable_id, record))
        .collect::<BTreeMap<_, _>>();
    let host_by_name = records
        .host_functions
        .iter()
        .map(|record| (record.name.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let nexa_by_name = records
        .nexa_entrypoints
        .iter()
        .map(|record| (record.name.as_str(), record))
        .collect::<BTreeMap<_, _>>();

    let host_functions = select_records(
        &selection.host_functions,
        &host_by_name,
        EffectiveDescriptorError::UnknownHostFunction,
    )?;
    let required_nexa_entrypoints = select_records(
        &selection.required_nexa_entrypoints,
        &nexa_by_name,
        EffectiveDescriptorError::UnknownNexaEntrypoint,
    )?;

    // Required status dominates optional status when a caller supplies the same entrypoint in
    // both sets, leaving one canonical representation for the same effective surface.
    let optional_names = selection
        .present_optional_nexa_entrypoints
        .difference(&selection.required_nexa_entrypoints)
        .cloned()
        .collect::<BTreeSet<_>>();
    let present_optional_nexa_entrypoints = select_records(
        &optional_names,
        &nexa_by_name,
        EffectiveDescriptorError::UnknownNexaEntrypoint,
    )?;

    let mut pending = VecDeque::new();
    for name in &selection.referenced_types {
        let Some(record) = type_by_name.get(name.as_str()) else {
            return Err(EffectiveDescriptorError::UnknownType(name.clone()));
        };
        pending.push_back((record.stable_id, record.name.clone()));
    }
    for record in host_functions
        .iter()
        .chain(&required_nexa_entrypoints)
        .chain(&present_optional_nexa_entrypoints)
    {
        pending.extend(
            record
                .references
                .iter()
                .map(|(stable_id, name)| (*stable_id, name.clone())),
        );
    }

    let mut included_ids = BTreeSet::new();
    while let Some((stable_id, source_name)) = pending.pop_front() {
        if !included_ids.insert(stable_id) {
            continue;
        }
        let Some(record) = type_by_id.get(&stable_id) else {
            return Err(EffectiveDescriptorError::UnknownType(source_name));
        };
        pending.extend(
            record
                .references
                .iter()
                .map(|(stable_id, name)| (*stable_id, name.clone())),
        );
    }
    let mut shared_types = included_ids
        .iter()
        .filter_map(|stable_id| type_by_id.get(stable_id).copied())
        .collect::<Vec<_>>();

    sort_records(&mut shared_types);
    let mut host_functions = host_functions;
    let mut required_nexa_entrypoints = required_nexa_entrypoints;
    let mut present_optional_nexa_entrypoints = present_optional_nexa_entrypoints;
    sort_records(&mut host_functions);
    sort_records(&mut required_nexa_entrypoints);
    sort_records(&mut present_optional_nexa_entrypoints);

    let bytes = encode_top_level(EFFECTIVE_CONTRACT_DOMAIN, |encoder| {
        encoder.stable_id(contract.stable_id);
        encoder.string(&contract.name);
        encode_record_fingerprint_refs(encoder, &shared_types);
        encode_record_fingerprint_refs(encoder, &host_functions);
        encode_record_fingerprint_refs(encoder, &required_nexa_entrypoints);
        encode_record_fingerprint_refs(encoder, &present_optional_nexa_entrypoints);
    });

    Ok(EffectiveContractDescriptor {
        fingerprint: fingerprint(&bytes),
        bytes,
        shared_types: declaration_fingerprint_refs(&shared_types),
        host_functions: declaration_fingerprint_refs(&host_functions),
        required_nexa_entrypoints: declaration_fingerprint_refs(&required_nexa_entrypoints),
        present_optional_nexa_entrypoints: declaration_fingerprint_refs(
            &present_optional_nexa_entrypoints,
        ),
    })
}

pub fn effective_contract_fingerprint(
    contract: &ValidatedContract,
    selection: &EffectiveContractSelection,
) -> Result<AbiFingerprint, EffectiveDescriptorError> {
    effective_contract_descriptor(contract, selection).map(|descriptor| descriptor.fingerprint)
}

#[derive(Clone, Debug)]
struct ContractRecords {
    types: Vec<EncodedDeclaration>,
    host_functions: Vec<EncodedDeclaration>,
    nexa_entrypoints: Vec<EncodedDeclaration>,
}

impl ContractRecords {
    fn new(contract: &ValidatedContract) -> Self {
        let mut types = contract
            .handles
            .iter()
            .map(record_handle)
            .chain(contract.structs.iter().map(record_struct))
            .chain(contract.enums.iter().map(record_enum))
            .collect::<Vec<_>>();
        let mut host_functions = contract
            .host_functions
            .iter()
            .map(|function| record_function(FunctionFingerprintKind::Host, function))
            .collect::<Vec<_>>();
        let mut nexa_entrypoints = contract
            .nexa_functions
            .iter()
            .map(|function| record_function(FunctionFingerprintKind::Nexa, function))
            .collect::<Vec<_>>();
        sort_records(&mut types);
        sort_records(&mut host_functions);
        sort_records(&mut nexa_entrypoints);
        Self {
            types,
            host_functions,
            nexa_entrypoints,
        }
    }
}

#[derive(Clone, Debug)]
struct EncodedDeclaration {
    name: String,
    stable_id: StableId,
    kind_tag: u8,
    references: BTreeMap<StableId, String>,
    fingerprint: AbiFingerprint,
    #[cfg(test)]
    canonical_bytes: Vec<u8>,
}

pub(crate) fn handle_declaration_fingerprint(stable_id: StableId, name: &str) -> AbiFingerprint {
    fingerprint(&handle_declaration_bytes(stable_id, name))
}

pub(crate) fn struct_declaration_fingerprint(
    stable_id: StableId,
    name: &str,
    fields: &[ValidatedField],
) -> AbiFingerprint {
    fingerprint(&struct_declaration_bytes(stable_id, name, fields))
}

pub(crate) fn enum_declaration_fingerprint(
    stable_id: StableId,
    name: &str,
    variants: &[ValidatedVariant],
) -> AbiFingerprint {
    fingerprint(&enum_declaration_bytes(stable_id, name, variants))
}

pub(crate) fn function_declaration_fingerprint(
    input: &FunctionFingerprintInput<'_>,
) -> AbiFingerprint {
    fingerprint(&function_declaration_bytes(input))
}

fn handle_declaration_bytes(stable_id: StableId, name: &str) -> Vec<u8> {
    encode_top_level(TYPE_LAYOUT_DOMAIN, |encoder| {
        encoder.tag(Tag::Handle);
        encoder.stable_id(stable_id);
        encoder.string(name);
    })
}

fn struct_declaration_bytes(stable_id: StableId, name: &str, fields: &[ValidatedField]) -> Vec<u8> {
    encode_top_level(TYPE_LAYOUT_DOMAIN, |encoder| {
        encoder.tag(Tag::Struct);
        encoder.stable_id(stable_id);
        encoder.string(name);
        encoder.length(fields.len());
        for field in fields {
            encoder.stable_id(field.stable_id);
            encoder.string(&field.name);
            encode_type_ref(encoder, &field.ty);
        }
    })
}

fn enum_declaration_bytes(
    stable_id: StableId,
    name: &str,
    variants: &[ValidatedVariant],
) -> Vec<u8> {
    encode_top_level(TYPE_LAYOUT_DOMAIN, |encoder| {
        encoder.tag(Tag::Enum);
        encoder.stable_id(stable_id);
        encoder.string(name);
        encoder.length(variants.len());
        for variant in variants {
            encoder.stable_id(variant.stable_id);
            encoder.string(&variant.name);
            match &variant.payload {
                Some(payload) => {
                    encoder.u8(ENUM_TUPLE_PAYLOAD);
                    encode_type_ref(encoder, payload);
                }
                None => encoder.u8(ENUM_NO_PAYLOAD),
            }
        }
    })
}

fn function_declaration_bytes(input: &FunctionFingerprintInput<'_>) -> Vec<u8> {
    match input.kind {
        FunctionFingerprintKind::Host => {
            let function_tag = host_function_tag(input.is_async);
            encode_top_level(HOST_FUNCTION_DOMAIN, |encoder| {
                encoder.tag(function_tag);
                encoder.stable_id(input.stable_id);
                encoder.string(input.name);
                encode_host_attributes(
                    encoder,
                    input.is_async,
                    input.fuel_cost,
                    input.cancel_policy,
                    input.abandon_policy,
                    input.capabilities,
                );
                encode_function_signature(encoder, input.parameters, input.result);
            })
        }
        FunctionFingerprintKind::Nexa => encode_top_level(NEXA_ENTRYPOINT_DOMAIN, |encoder| {
            encoder.tag(Tag::NexaEntrypoint);
            encoder.boolean(input.is_async);
            encoder.stable_id(input.stable_id);
            encoder.string(input.name);
            encode_function_signature(encoder, input.parameters, input.result);
        }),
    }
}

fn encode_host_attributes(
    encoder: &mut Encoder,
    is_async: bool,
    fuel_cost: u32,
    cancel_policy: CancelPolicy,
    abandon_policy: AbandonPolicy,
    capabilities: &[String],
) {
    let mut capabilities = capabilities.iter().collect::<Vec<_>>();
    capabilities.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    capabilities.dedup();

    encoder.length(if is_async { 4 } else { 2 });
    encoder.tag(Tag::Fuel);
    encoder.u32(fuel_cost);
    if is_async {
        encoder.tag(Tag::Cancel);
        encoder.u8(match cancel_policy {
            CancelPolicy::ReturnError => CANCEL_RETURN_ERROR,
            CancelPolicy::CancelTask => CANCEL_TASK,
        });
        encoder.tag(Tag::Abandon);
        encoder.u8(match abandon_policy {
            AbandonPolicy::ReturnError => ABANDON_RETURN_ERROR,
            AbandonPolicy::Trap => ABANDON_TRAP,
        });
    }
    encoder.tag(Tag::Capability);
    encoder.length(capabilities.len());
    for capability in capabilities {
        encoder.string(capability);
    }
}

fn encode_function_signature(
    encoder: &mut Encoder,
    parameters: &[ValidatedParameter],
    result: Option<&ResolvedTypeRef>,
) {
    encoder.length(parameters.len());
    for parameter in parameters {
        encoder.stable_id(parameter.stable_id);
        encoder.string(&parameter.name);
        encode_type_ref(encoder, &parameter.ty);
    }
    match result {
        Some(result) => {
            encoder.boolean(true);
            encode_type_ref(encoder, result);
        }
        None => encoder.boolean(false),
    }
}

fn record_handle(handle: &ValidatedHandle) -> EncodedDeclaration {
    EncodedDeclaration {
        name: handle.name.clone(),
        stable_id: handle.stable_id,
        kind_tag: Tag::Handle as u8,
        references: BTreeMap::new(),
        fingerprint: handle.declaration_fingerprint,
        #[cfg(test)]
        canonical_bytes: handle_declaration_bytes(handle.stable_id, &handle.name),
    }
}

fn record_struct(structure: &ValidatedStruct) -> EncodedDeclaration {
    let mut references = BTreeMap::new();
    for field in &structure.fields {
        collect_named_types(&field.ty, &mut references);
    }
    EncodedDeclaration {
        name: structure.name.clone(),
        stable_id: structure.stable_id,
        kind_tag: Tag::Struct as u8,
        references,
        fingerprint: structure.declaration_fingerprint,
        #[cfg(test)]
        canonical_bytes: struct_declaration_bytes(
            structure.stable_id,
            &structure.name,
            &structure.fields,
        ),
    }
}

fn record_enum(enumeration: &ValidatedEnum) -> EncodedDeclaration {
    let mut references = BTreeMap::new();
    for variant in &enumeration.variants {
        if let Some(payload) = &variant.payload {
            collect_named_types(payload, &mut references);
        }
    }
    EncodedDeclaration {
        name: enumeration.name.clone(),
        stable_id: enumeration.stable_id,
        kind_tag: Tag::Enum as u8,
        references,
        fingerprint: enumeration.declaration_fingerprint,
        #[cfg(test)]
        canonical_bytes: enum_declaration_bytes(
            enumeration.stable_id,
            &enumeration.name,
            &enumeration.variants,
        ),
    }
}

fn record_function(
    kind: FunctionFingerprintKind,
    function: &ValidatedFunction,
) -> EncodedDeclaration {
    let mut references = BTreeMap::new();
    for parameter in &function.parameters {
        collect_named_types(&parameter.ty, &mut references);
    }
    if let Some(result) = &function.result {
        collect_named_types(result, &mut references);
    }
    EncodedDeclaration {
        name: function.name.clone(),
        stable_id: function.stable_id,
        kind_tag: match kind {
            FunctionFingerprintKind::Host => host_function_tag(function.is_async) as u8,
            FunctionFingerprintKind::Nexa => Tag::NexaEntrypoint as u8,
        },
        references,
        fingerprint: function.declaration_fingerprint,
        #[cfg(test)]
        canonical_bytes: function_declaration_bytes(&function_fingerprint_input(kind, function)),
    }
}

#[cfg(test)]
fn function_fingerprint_input(
    kind: FunctionFingerprintKind,
    function: &ValidatedFunction,
) -> FunctionFingerprintInput<'_> {
    FunctionFingerprintInput {
        kind,
        stable_id: function.stable_id,
        name: &function.name,
        is_async: function.is_async,
        parameters: &function.parameters,
        result: function.result.as_ref(),
        fuel_cost: function.fuel_cost,
        cancel_policy: function.cancel_policy,
        abandon_policy: function.abandon_policy,
        capabilities: &function.capabilities,
    }
}

fn host_function_tag(is_async: bool) -> Tag {
    if is_async {
        Tag::HostAsyncFunction
    } else {
        Tag::HostFunction
    }
}

fn encode_type_ref(encoder: &mut Encoder, ty: &ResolvedTypeRef) {
    match &ty.kind {
        ResolvedTypeKind::I32 => encoder.tag(Tag::I32),
        ResolvedTypeKind::I64 => encoder.tag(Tag::I64),
        ResolvedTypeKind::F32 => encoder.tag(Tag::F32),
        ResolvedTypeKind::F64 => encoder.tag(Tag::F64),
        ResolvedTypeKind::Bool => encoder.tag(Tag::Bool),
        ResolvedTypeKind::Rune => encoder.tag(Tag::Rune),
        ResolvedTypeKind::String => encoder.tag(Tag::String),
        ResolvedTypeKind::Array(inner) => {
            encoder.tag(Tag::Array);
            encode_type_ref(encoder, inner);
        }
        ResolvedTypeKind::Buffer(inner) => {
            encoder.tag(Tag::Buffer);
            encode_type_ref(encoder, inner);
        }
        ResolvedTypeKind::Option(inner) => {
            encoder.tag(Tag::Option);
            encode_type_ref(encoder, inner);
        }
        ResolvedTypeKind::Result(ok, error) => {
            encoder.tag(Tag::Result);
            encode_type_ref(encoder, ok);
            encode_type_ref(encoder, error);
        }
        ResolvedTypeKind::Token(named) => {
            encoder.tag(Tag::Token);
            encode_named_type(encoder, named);
        }
        ResolvedTypeKind::Snapshot(named) => {
            encoder.tag(Tag::Snapshot);
            encode_named_type(encoder, named);
        }
        ResolvedTypeKind::Named(named) => encode_named_type(encoder, named),
    }
}

fn encode_named_type(encoder: &mut Encoder, named: &ResolvedNamedType) {
    encoder.tag(Tag::Named);
    encoder.stable_id(named.stable_id);
    encoder.string(&named.source_name);
}

fn collect_named_types(ty: &ResolvedTypeRef, output: &mut BTreeMap<StableId, String>) {
    match &ty.kind {
        ResolvedTypeKind::Array(inner)
        | ResolvedTypeKind::Buffer(inner)
        | ResolvedTypeKind::Option(inner) => collect_named_types(inner, output),
        ResolvedTypeKind::Result(ok, error) => {
            collect_named_types(ok, output);
            collect_named_types(error, output);
        }
        ResolvedTypeKind::Token(named)
        | ResolvedTypeKind::Snapshot(named)
        | ResolvedTypeKind::Named(named) => {
            output.insert(named.stable_id, named.source_name.clone());
        }
        ResolvedTypeKind::I32
        | ResolvedTypeKind::I64
        | ResolvedTypeKind::F32
        | ResolvedTypeKind::F64
        | ResolvedTypeKind::Bool
        | ResolvedTypeKind::Rune
        | ResolvedTypeKind::String => {}
    }
}

fn encode_top_level(domain: &str, payload: impl FnOnce(&mut Encoder)) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.bytes(DESCRIPTOR_PREFIX);
    encoder.u32(u32::from(ABI_DESCRIPTOR_VERSION));
    encoder.string(domain);
    payload(&mut encoder);
    encoder.finish()
}

fn fingerprint(descriptor: &[u8]) -> AbiFingerprint {
    AbiFingerprint::from_bytes(*blake3::hash(descriptor).as_bytes())
}

fn encode_record_fingerprints(encoder: &mut Encoder, records: &[EncodedDeclaration]) {
    encoder.length(records.len());
    for record in records {
        encoder.abi_fingerprint(record.fingerprint);
    }
}

fn encode_record_fingerprint_refs(encoder: &mut Encoder, records: &[&EncodedDeclaration]) {
    encoder.length(records.len());
    for record in records {
        encoder.abi_fingerprint(record.fingerprint);
    }
}

fn declaration_fingerprints(records: &[EncodedDeclaration]) -> Vec<DeclarationFingerprint> {
    records.iter().map(declaration_fingerprint).collect()
}

fn declaration_fingerprint_refs(records: &[&EncodedDeclaration]) -> Vec<DeclarationFingerprint> {
    records
        .iter()
        .map(|record| declaration_fingerprint(record))
        .collect()
}

fn declaration_fingerprint(record: &EncodedDeclaration) -> DeclarationFingerprint {
    DeclarationFingerprint {
        name: record.name.clone(),
        stable_id: record.stable_id,
        fingerprint: record.fingerprint,
    }
}

fn select_records<'a>(
    names: &BTreeSet<String>,
    records: &BTreeMap<&str, &'a EncodedDeclaration>,
    unknown: impl Fn(String) -> EffectiveDescriptorError,
) -> Result<Vec<&'a EncodedDeclaration>, EffectiveDescriptorError> {
    names
        .iter()
        .map(|name| {
            records
                .get(name.as_str())
                .copied()
                .ok_or_else(|| unknown(name.clone()))
        })
        .collect()
}

fn sort_records<T>(records: &mut [T])
where
    T: RecordIdentity,
{
    records.sort_unstable_by(|left, right| {
        (left.kind_tag(), left.name().as_bytes(), left.stable_id().0).cmp(&(
            right.kind_tag(),
            right.name().as_bytes(),
            right.stable_id().0,
        ))
    });
}

trait RecordIdentity {
    fn kind_tag(&self) -> u8;
    fn stable_id(&self) -> StableId;
    fn name(&self) -> &str;
}

impl RecordIdentity for EncodedDeclaration {
    fn kind_tag(&self) -> u8 {
        self.kind_tag
    }

    fn stable_id(&self) -> StableId {
        self.stable_id
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl RecordIdentity for &EncodedDeclaration {
    fn kind_tag(&self) -> u8 {
        self.kind_tag
    }

    fn stable_id(&self) -> StableId {
        self.stable_id
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
enum Tag {
    I32 = 0x01,
    I64 = 0x02,
    F32 = 0x03,
    F64 = 0x04,
    Bool = 0x05,
    Rune = 0x06,
    String = 0x07,
    Array = 0x10,
    Buffer = 0x11,
    Option = 0x12,
    Result = 0x13,
    Token = 0x14,
    Snapshot = 0x15,
    Named = 0x20,
    Struct = 0x30,
    Enum = 0x31,
    Handle = 0x32,
    HostFunction = 0x40,
    HostAsyncFunction = 0x41,
    NexaEntrypoint = 0x50,
    Fuel = 0x60,
    Cancel = 0x61,
    Abandon = 0x62,
    Capability = 0x63,
}

const ENUM_NO_PAYLOAD: u8 = 0x00;
const ENUM_TUPLE_PAYLOAD: u8 = 0x01;
// `0x02` is reserved by Descriptor v2 for record enum payloads. The current
// validated Contract model has no record-payload form.
const CANCEL_RETURN_ERROR: u8 = 0x01;
const CANCEL_TASK: u8 = 0x02;
const ABANDON_RETURN_ERROR: u8 = 0x01;
const ABANDON_TRAP: u8 = 0x02;

struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn tag(&mut self, tag: Tag) {
        self.u8(tag as u8);
    }

    fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn stable_id(&mut self, value: StableId) {
        self.bytes.extend_from_slice(&value.0.to_le_bytes());
    }

    fn abi_fingerprint(&mut self, value: AbiFingerprint) {
        self.bytes.extend_from_slice(value.as_bytes());
    }

    fn length(&mut self, value: usize) {
        self.u32(
            u32::try_from(value)
                .expect("ValidatedContract descriptor lengths and counts must fit in u32"),
        );
    }

    fn bytes(&mut self, value: &[u8]) {
        self.length(value.len());
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ABI_DESCRIPTOR_VERSION, CONTRACT_DOMAIN, ContractRecords, DESCRIPTOR_PREFIX,
        EFFECTIVE_CONTRACT_DOMAIN, EffectiveContractSelection, Tag, abi_descriptor,
        effective_contract_descriptor,
    };

    fn contract(source: &str) -> crate::ValidatedContract {
        crate::parse(source).expect("test Contract must validate")
    }

    fn take_u32(bytes: &[u8], cursor: &mut usize) -> u32 {
        let encoded = bytes
            .get(*cursor..*cursor + 4)
            .expect("test descriptor must contain a u32");
        *cursor += 4;
        u32::from_le_bytes(encoded.try_into().expect("slice has the exact u32 width"))
    }

    fn take_bytes<'a>(bytes: &'a [u8], cursor: &mut usize) -> &'a [u8] {
        let length = usize::try_from(take_u32(bytes, cursor)).expect("u32 fits usize");
        let encoded = bytes
            .get(*cursor..*cursor + length)
            .expect("test descriptor must contain its framed bytes");
        *cursor += length;
        encoded
    }

    fn assert_top_level_header<'a>(bytes: &'a [u8], domain: &str) -> &'a [u8] {
        let mut cursor = 0;
        assert_eq!(take_bytes(bytes, &mut cursor), DESCRIPTOR_PREFIX);
        assert_eq!(
            take_u32(bytes, &mut cursor),
            u32::from(ABI_DESCRIPTOR_VERSION)
        );
        assert_eq!(take_bytes(bytes, &mut cursor), domain.as_bytes());
        &bytes[cursor..]
    }

    #[test]
    fn top_level_wire_is_self_describing_and_hashes_exact_bytes() {
        let contract = contract("contract Example;");
        let descriptor = abi_descriptor(&contract);

        let payload = assert_top_level_header(descriptor.as_bytes(), CONTRACT_DOMAIN);
        assert!(!payload.is_empty());
        assert_eq!(
            descriptor.fingerprint.as_bytes(),
            blake3::hash(descriptor.as_bytes()).as_bytes()
        );

        let effective =
            effective_contract_descriptor(&contract, &EffectiveContractSelection::default())
                .expect("empty selection is valid");
        let payload = assert_top_level_header(effective.as_bytes(), EFFECTIVE_CONTRACT_DOMAIN);
        assert!(!payload.is_empty());
        assert_eq!(
            effective.fingerprint.as_bytes(),
            blake3::hash(effective.as_bytes()).as_bytes()
        );
    }

    #[test]
    fn full_sequences_embed_unframed_raw_declaration_fingerprints() {
        let contract = contract(
            r"
                contract Example;
                    struct Payload {
                        value: i32,
                    }
            ",
        );
        let descriptor = abi_descriptor(&contract);
        let payload = assert_top_level_header(descriptor.as_bytes(), CONTRACT_DOMAIN);
        let mut cursor = 8;
        assert_eq!(take_bytes(payload, &mut cursor), b"Example");

        assert_eq!(take_u32(payload, &mut cursor), 1);
        let type_fingerprint = payload
            .get(cursor..cursor + 32)
            .expect("one raw Type Layout Fingerprint");
        assert_eq!(
            type_fingerprint,
            descriptor.type_layouts[0].fingerprint.as_bytes()
        );
        cursor += 32;
        assert_eq!(take_u32(payload, &mut cursor), 0);
        assert_eq!(take_u32(payload, &mut cursor), 0);
        assert_eq!(cursor, payload.len());
    }

    #[test]
    fn declaration_tags_and_nexa_effect_are_fixed_wire_data() {
        let contract = contract(
            r"
                contract Example;
                    handle Identity;
                    enum Failure {
                        Rejected,
                    }
                    struct Payload {
                        value: i32,
                    }
                    host {
                        fn inspect(payload: Payload) -> i32;
                        @cancel(cancel_task)
                        @abandon(trap)
                        async fn load() -> Result<i32, Failure>;
                    }
                    nexa {
                        fn update();
                        async fn on_event();
                    }
            ",
        );
        let records = ContractRecords::new(&contract);

        assert_eq!(
            records
                .types
                .iter()
                .map(|record| record.kind_tag)
                .collect::<Vec<_>>(),
            vec![Tag::Struct as u8, Tag::Enum as u8, Tag::Handle as u8]
        );
        assert_eq!(
            records
                .host_functions
                .iter()
                .map(|record| record.kind_tag)
                .collect::<Vec<_>>(),
            vec![Tag::HostFunction as u8, Tag::HostAsyncFunction as u8]
        );

        let ordinary = records
            .nexa_entrypoints
            .iter()
            .find(|record| record.name == "update")
            .expect("ordinary entrypoint exists");
        let task = records
            .nexa_entrypoints
            .iter()
            .find(|record| record.name == "on_event")
            .expect("Task entrypoint exists");
        let ordinary_payload =
            assert_top_level_header(&ordinary.canonical_bytes, super::NEXA_ENTRYPOINT_DOMAIN);
        let task_payload =
            assert_top_level_header(&task.canonical_bytes, super::NEXA_ENTRYPOINT_DOMAIN);
        assert_eq!(
            ordinary_payload.get(..2),
            Some(&[Tag::NexaEntrypoint as u8, 0x00][..])
        );
        assert_eq!(
            task_payload.get(..2),
            Some(&[Tag::NexaEntrypoint as u8, 0x01][..])
        );
    }

    #[test]
    fn host_attributes_are_normalized_before_fingerprinting() {
        let first = contract(
            r#"
                contract Example;
                    host {
                        @fuel(7)
                        @capability("zeta.read")
                        @cancel(cancel_task)
                        @capability("alpha.read")
                        @abandon(trap)
                        async fn load() -> Result<i32, i32>;
                    }
            "#,
        );
        let reordered = contract(
            r#"
                contract Example;
                    host {
                        @abandon(trap)
                        @capability("alpha.read")
                        @fuel(7)
                        @capability("zeta.read")
                        @cancel(cancel_task)
                        async fn load() -> Result<i32, i32>;
                    }
            "#,
        );

        assert_eq!(
            abi_descriptor(&first).host_functions,
            abi_descriptor(&reordered).host_functions
        );
    }

    #[test]
    fn top_level_and_block_order_are_not_semantic() {
        let first = contract(
            r"
                /// Documentation is not ABI.
                contract Example;
                    struct Payload {
                        value: i32,
                    }
                    enum Failure {
                        Rejected,
                    }
                    host {
                        fn inspect(payload: Payload) -> i32;
                    }
                    nexa {
                        fn on_event(payload: Payload) -> Result<i32, Failure>;
                    }
            ",
        );
        let reordered = contract(
            r"
                contract Example;
                    nexa {
                        fn on_event(payload: Payload) -> Result<i32, Failure>;
                    }
                    enum Failure {
                        Rejected,
                    }
                    // Ordinary comments and formatting are not ABI.
                    host {
                        fn inspect(payload: Payload) -> i32;
                    }
                    struct Payload {
                        value: i32,
                    }
            ",
        );

        assert_eq!(abi_descriptor(&first), abi_descriptor(&reordered));
    }

    #[test]
    fn ordered_members_are_semantic() {
        let first = contract(
            r"
                contract Example;
                    struct Pair {
                        left: i32,
                        right: i64,
                    }
                    enum Choice {
                        First,
                        Second,
                    }
                    host {
                        fn combine(left: i32, right: i64) -> Pair;
                    }
            ",
        );
        let reordered_fields = contract(
            r"
                contract Example;
                    struct Pair {
                        right: i64,
                        left: i32,
                    }
                    enum Choice {
                        First,
                        Second,
                    }
                    host {
                        fn combine(left: i32, right: i64) -> Pair;
                    }
            ",
        );
        let reordered_variants = contract(
            r"
                contract Example;
                    struct Pair {
                        left: i32,
                        right: i64,
                    }
                    enum Choice {
                        Second,
                        First,
                    }
                    host {
                        fn combine(left: i32, right: i64) -> Pair;
                    }
            ",
        );
        let reordered_parameters = contract(
            r"
                contract Example;
                    struct Pair {
                        left: i32,
                        right: i64,
                    }
                    enum Choice {
                        First,
                        Second,
                    }
                    host {
                        fn combine(right: i64, left: i32) -> Pair;
                    }
            ",
        );

        let fingerprint = abi_descriptor(&first).fingerprint;
        assert_ne!(fingerprint, abi_descriptor(&reordered_fields).fingerprint);
        assert_ne!(fingerprint, abi_descriptor(&reordered_variants).fingerprint);
        assert_ne!(
            fingerprint,
            abi_descriptor(&reordered_parameters).fingerprint
        );
    }

    #[test]
    fn effective_descriptor_ignores_unrelated_optional_surface() {
        let first = contract(
            r"
                contract Example;
                    struct Payload {
                        value: i32,
                    }
                    struct Unused {
                        value: i32,
                    }
                    host {
                        fn inspect(payload: Payload) -> i32;
                    }
                    nexa {
                        fn required(payload: Payload);
                        fn optional_a() -> i32;
                    }
            ",
        );
        let extended = contract(
            r"
                contract Example;
                    struct Payload {
                        value: i32,
                    }
                    struct Unused {
                        value: string,
                        other: i64,
                    }
                    host {
                        fn inspect(payload: Payload) -> i32;
                        fn unrelated_host(value: Unused);
                    }
                    nexa {
                        fn required(payload: Payload);
                        fn optional_a() -> i32;
                        fn optional_b(value: Unused);
                    }
            ",
        );
        let selection = EffectiveContractSelection {
            referenced_types: std::collections::BTreeSet::default(),
            host_functions: ["inspect".to_owned()].into_iter().collect(),
            required_nexa_entrypoints: ["required".to_owned()].into_iter().collect(),
            present_optional_nexa_entrypoints: ["optional_a".to_owned()].into_iter().collect(),
        };

        let first =
            effective_contract_descriptor(&first, &selection).expect("selected declarations exist");
        let extended = effective_contract_descriptor(&extended, &selection)
            .expect("selected declarations exist");
        assert_eq!(first, extended);
        assert_eq!(
            first
                .shared_types
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Payload"]
        );
        assert_eq!(
            first
                .required_nexa_entrypoints
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["required"]
        );
        assert_eq!(
            first
                .present_optional_nexa_entrypoints
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["optional_a"]
        );
    }

    #[test]
    fn required_entrypoint_is_not_repeated_as_present_optional() {
        let contract = contract(
            r"
                contract Example;
                    nexa {
                        fn required();
                        fn optional();
                    }
            ",
        );
        let selection = EffectiveContractSelection {
            required_nexa_entrypoints: ["required".to_owned()].into_iter().collect(),
            present_optional_nexa_entrypoints: ["required".to_owned(), "optional".to_owned()]
                .into_iter()
                .collect(),
            ..EffectiveContractSelection::default()
        };

        let descriptor =
            effective_contract_descriptor(&contract, &selection).expect("selection is valid");
        assert_eq!(
            descriptor
                .required_nexa_entrypoints
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["required"]
        );
        assert_eq!(
            descriptor
                .present_optional_nexa_entrypoints
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>(),
            vec!["optional"]
        );
    }

    #[test]
    fn effective_descriptor_includes_transitive_shared_type_layouts() {
        let first = contract(
            r"
                contract Example;
                    struct Inner {
                        value: i32,
                    }
                    struct Outer {
                        inner: Inner,
                    }
                    nexa {
                        fn on_event(value: Outer);
                    }
            ",
        );
        let changed = contract(
            r"
                contract Example;
                    struct Inner {
                        value: i64,
                    }
                    struct Outer {
                        inner: Inner,
                    }
                    nexa {
                        fn on_event(value: Outer);
                    }
            ",
        );
        let selection = EffectiveContractSelection {
            required_nexa_entrypoints: ["on_event".to_owned()].into_iter().collect(),
            ..EffectiveContractSelection::default()
        };

        let first =
            effective_contract_descriptor(&first, &selection).expect("selected declarations exist");
        let changed = effective_contract_descriptor(&changed, &selection)
            .expect("selected declarations exist");
        assert_ne!(first.fingerprint, changed.fingerprint);
        assert_eq!(first.shared_types.len(), 2);
    }
}
