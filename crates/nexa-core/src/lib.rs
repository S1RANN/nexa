//! Stable primitive identities and trace records shared by Nexa crates.

pub mod deterministic_math;

use std::collections::BTreeMap;
use std::fmt;

/// Version of the compiler implementation included in every build fingerprint.
///
/// All workspace compiler-facing crates share one release version. Keeping the authority here
/// prevents façade, compiler-adapter, and analysis-only producers from using their own package
/// version as independent identities.
pub const NEXA_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Current bytecode wire-format version included in every build fingerprint and module header.
pub const BYTECODE_VERSION: u16 = 7;
/// Version of observable VM execution semantics included in every build fingerprint.
pub const RUNTIME_SEMANTICS_VERSION: u16 = 1;
/// Version of the fixed instruction fuel schedule included in every build fingerprint.
pub const OPCODE_COST_TABLE_VERSION: u32 = 7;
/// Exact `libm` release used by the deterministic scalar-math implementation.
pub const RUNTIME_LIBM_VERSION: &str = "0.2.16";
/// Canonical quiet-NaN encoding used for every observable `f32` NaN result.
pub const CANONICAL_NAN_F32_BITS: u32 = 0x7fc0_0000;
/// Canonical quiet-NaN encoding used for every observable `f64` NaN result.
pub const CANONICAL_NAN_F64_BITS: u64 = 0x7ff8_0000_0000_0000;
/// Version of the canonical NaN policy encoded in the math backend identity.
pub const CANONICAL_NAN_POLICY_VERSION: u16 = 1;
/// Deterministic scalar-math implementation included in every build fingerprint.
pub const RUNTIME_MATH_BACKEND_ID: &str = "pure-rust-libm-0.2.16-canonical-nan-v1";

/// Identifies a source file inside one compilation session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

/// Identifies a module in a verified bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(pub u32);

/// Identifies a function in a verified module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// Identifies a concrete runtime or bytecode type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeId(pub u32);

/// Identifies an isolated runtime realm.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RealmId(pub u32);

/// A generation-protected runtime identity.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RawHandle {
    pub realm_id: u32,
    pub index: u32,
    pub generation: u32,
}

impl RawHandle {
    #[must_use]
    pub const fn new(realm_id: u32, index: u32, generation: u32) -> Self {
        Self {
            realm_id,
            index,
            generation,
        }
    }
}

/// A half-open byte range in a source file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl SourceSpan {
    #[must_use]
    pub const fn new(file: FileId, start: u32, end: u32) -> Self {
        Self { file, start, end }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Stable identifier derived from a normative symbolic name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableId(pub u64);

impl StableId {
    /// Uses fixed FNV-1a instead of a process-randomized hash so generated IDs are reproducible.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        Self::from_parts(&[name])
    }

    #[must_use]
    pub fn from_parts(parts: &[&str]) -> Self {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for part in parts {
            for byte in part.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Self(hash)
    }
}

/// Versioned canonical identity for a source-level symbol.
///
/// The canonical text is retained by the analysis layer so two definitions
/// that truncate to the same 64-bit runtime identifier can be rejected before
/// bytecode is emitted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalSymbolIdentity {
    package_id: String,
    module_path: String,
    kind: SymbolKind,
    name: String,
    explicit_stable_name: Option<String>,
}

impl CanonicalSymbolIdentity {
    #[must_use]
    pub fn automatic(
        package_id: impl Into<String>,
        module_path: impl Into<String>,
        kind: SymbolKind,
        name: impl Into<String>,
    ) -> Self {
        Self {
            package_id: package_id.into(),
            module_path: module_path.into(),
            kind,
            name: name.into(),
            explicit_stable_name: None,
        }
    }

    #[must_use]
    pub fn explicit(
        package_id: impl Into<String>,
        kind: SymbolKind,
        stable_name: impl Into<String>,
    ) -> Self {
        let stable_name = stable_name.into();
        Self {
            package_id: package_id.into(),
            module_path: String::new(),
            kind,
            name: stable_name.clone(),
            explicit_stable_name: Some(stable_name),
        }
    }

    #[must_use]
    pub fn runtime_id(&self) -> StableSymbolId {
        let mut builder = FingerprintBuilder::new("nexa.stable-symbol-id", 1);
        builder.field_str("package", &self.package_id);
        builder.field_u8("kind", self.kind as u8);
        if let Some(stable_name) = &self.explicit_stable_name {
            builder.field_str("stable", stable_name);
        } else {
            builder.field_str("module", &self.module_path);
            builder.field_str("name", &self.name);
        }
        let digest = builder.finish_bytes();
        StableSymbolId(StableId(u64::from_le_bytes(
            digest[..8].try_into().expect("BLAKE3 digest has 32 bytes"),
        )))
    }

    #[must_use]
    pub fn package_id(&self) -> &str {
        &self.package_id
    }

    #[must_use]
    pub fn module_path(&self) -> &str {
        &self.module_path
    }

    #[must_use]
    pub fn kind(&self) -> SymbolKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn explicit_stable_name(&self) -> Option<&str> {
        self.explicit_stable_name.as_deref()
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SymbolKind {
    Function = 1,
    Type = 2,
    Field = 3,
    Variant = 4,
    Constant = 5,
    Task = 6,
    Test = 7,
}

/// Compact runtime symbol identity. Analysis must pair it with the canonical
/// identity and reject truncation collisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableSymbolId(pub StableId);

impl fmt::Display for StableSymbolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Package-wide registry that makes 64-bit truncation collisions explicit.
#[derive(Clone, Debug, Default)]
pub struct StableSymbolRegistry {
    identities: BTreeMap<StableSymbolId, CanonicalSymbolIdentity>,
}

impl StableSymbolRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        identity: CanonicalSymbolIdentity,
    ) -> Result<StableSymbolId, StableSymbolCollision> {
        let runtime_id = identity.runtime_id();
        if let Some(existing) = self.identities.get(&runtime_id) {
            if existing != &identity {
                return Err(StableSymbolCollision {
                    runtime_id,
                    first: Box::new(existing.clone()),
                    second: Box::new(identity),
                });
            }
            return Ok(runtime_id);
        }
        self.identities.insert(runtime_id, identity);
        Ok(runtime_id)
    }

    #[must_use]
    pub fn identity(&self, id: StableSymbolId) -> Option<&CanonicalSymbolIdentity> {
        self.identities.get(&id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.identities.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StableSymbolCollision {
    pub runtime_id: StableSymbolId,
    pub first: Box<CanonicalSymbolIdentity>,
    pub second: Box<CanonicalSymbolIdentity>,
}

impl fmt::Display for StableSymbolCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "stable symbol collision for {}: {:?} and {:?}",
            self.runtime_id, self.first, self.second
        )
    }
}

impl std::error::Error for StableSymbolCollision {}

/// Domain-separated and length-prefixed BLAKE3 input builder.
///
/// Callers must use a stable domain and schema version. Field names and values
/// are framed independently, preventing concatenation ambiguities.
#[derive(Clone)]
pub struct FingerprintBuilder {
    hasher: blake3::Hasher,
}

impl FingerprintBuilder {
    #[must_use]
    pub fn new(domain: &str, schema: u16) -> Self {
        let mut hasher = blake3::Hasher::new();
        update_framed(&mut hasher, b"nexa.fingerprint");
        update_framed(&mut hasher, domain.as_bytes());
        update_framed(&mut hasher, &schema.to_le_bytes());
        Self { hasher }
    }

    pub fn field_bytes(&mut self, name: &str, value: &[u8]) {
        update_framed(&mut self.hasher, name.as_bytes());
        update_framed(&mut self.hasher, value);
    }

    pub fn field_str(&mut self, name: &str, value: &str) {
        self.field_bytes(name, value.as_bytes());
    }

    pub fn field_u8(&mut self, name: &str, value: u8) {
        self.field_bytes(name, &[value]);
    }

    pub fn field_u32(&mut self, name: &str, value: u32) {
        self.field_bytes(name, &value.to_le_bytes());
    }

    pub fn field_u64(&mut self, name: &str, value: u64) {
        self.field_bytes(name, &value.to_le_bytes());
    }

    #[must_use]
    pub fn finish_bytes(self) -> [u8; 32] {
        *self.hasher.finalize().as_bytes()
    }
}

fn update_framed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.update(&length.to_le_bytes());
    hasher.update(bytes);
}

macro_rules! fingerprint_type {
    ($name:ident, $domain:literal) => {
        #[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            pub const DOMAIN: &'static str = $domain;

            #[must_use]
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl std::str::FromStr for $name {
            type Err = FingerprintParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_fingerprint(value).map(Self)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

fingerprint_type!(SourceSetFingerprint, "nexa.source-set");
fingerprint_type!(PublicApiFingerprint, "nexa.public-api");
fingerprint_type!(StateSchemaFingerprint, "nexa.state-schema");
fingerprint_type!(BuildFingerprint, "nexa.build");
fingerprint_type!(LinkedStateFingerprint, "nexa.linked-state");

/// Runtime-compatible value identity used by the canonical state-layout
/// fingerprint. This neutral model keeps analysis and bytecode from copying
/// type tags or depending on one another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonicalValueType {
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

/// Derives the compact runtime ABI identity of a structural type.
///
/// This is intentionally distinct from a 256-bit content fingerprint. The
/// framing is centralized here so analysis, compiler, bytecode metadata, and
/// state-layout fingerprinting cannot disagree about nested structural types.
#[must_use]
pub fn canonical_parameterized_type_id(name: &str, arguments: &[CanonicalValueType]) -> StableId {
    canonical_parameterized_type_id_iter(name, arguments.iter().copied())
}

/// Derives a structural runtime ABI identity without materializing an
/// intermediate argument collection.
#[must_use]
pub fn canonical_parameterized_type_id_iter(
    name: &str,
    arguments: impl IntoIterator<Item = CanonicalValueType>,
) -> StableId {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    fn append(hash: &mut u64, bytes: impl IntoIterator<Item = u8>) {
        for byte in bytes {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(PRIME);
        }
    }

    let mut hash = OFFSET;
    append(&mut hash, name.bytes());
    append(&mut hash, *b"<");
    for (index, argument) in arguments.into_iter().enumerate() {
        if index != 0 {
            append(&mut hash, *b",");
        }
        match argument {
            CanonicalValueType::I32 => append(&mut hash, b"i32".iter().copied()),
            CanonicalValueType::I64 => append(&mut hash, b"i64".iter().copied()),
            CanonicalValueType::F32 => append(&mut hash, b"f32".iter().copied()),
            CanonicalValueType::F64 => append(&mut hash, b"f64".iter().copied()),
            CanonicalValueType::Bool => append(&mut hash, b"bool".iter().copied()),
            CanonicalValueType::Rune => append(&mut hash, b"rune".iter().copied()),
            CanonicalValueType::String => append(&mut hash, b"string".iter().copied()),
            CanonicalValueType::Ref => append(&mut hash, b"ref".iter().copied()),
            CanonicalValueType::Named(id) => {
                append(&mut hash, b"named:".iter().copied());
                for shift in (0..16).rev().map(|index| index * 4) {
                    let nibble = ((id.0 >> shift) & 0xf) as u8;
                    append(
                        &mut hash,
                        [if nibble < 10 {
                            b'0' + nibble
                        } else {
                            b'a' + nibble - 10
                        }],
                    );
                }
            }
        }
    }
    append(&mut hash, *b">");
    StableId(hash)
}

#[must_use]
pub fn canonical_option_type_id(payload: CanonicalValueType) -> StableId {
    canonical_parameterized_type_id("Option", &[payload])
}

#[must_use]
pub fn canonical_result_type_id(
    success: CanonicalValueType,
    error: CanonicalValueType,
) -> StableId {
    canonical_parameterized_type_id("Result", &[success, error])
}

#[must_use]
pub fn canonical_array_type_id(element: CanonicalValueType) -> StableId {
    canonical_parameterized_type_id("Array", &[element])
}

#[must_use]
pub fn canonical_map_type_id(key: CanonicalValueType, value: CanonicalValueType) -> StableId {
    canonical_parameterized_type_id("Map", &[key, value])
}

#[must_use]
pub fn canonical_tuple_type_id(elements: &[CanonicalValueType]) -> StableId {
    canonical_parameterized_type_id("Tuple", elements)
}

#[must_use]
pub fn canonical_buffer_type_id(element: CanonicalValueType) -> StableId {
    canonical_parameterized_type_id("Buffer", &[element])
}

#[must_use]
pub fn canonical_snapshot_type_id(content_type: StableId) -> StableId {
    canonical_parameterized_type_id("Snapshot", &[CanonicalValueType::Named(content_type)])
}

#[must_use]
pub fn canonical_resource_token_type_id(content_type: StableId) -> StableId {
    canonical_parameterized_type_id("Token", &[CanonicalValueType::Named(content_type)])
}

#[must_use]
pub fn canonical_state_handle_type_id(target: CanonicalValueType) -> StableId {
    canonical_parameterized_type_id("StateHandle", &[target])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalStateField {
    pub stable_id: StableId,
    pub ty: CanonicalValueType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalStateType {
    pub stable_id: StableId,
    pub version: u32,
    pub fields: Vec<CanonicalStateField>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanonicalStateSchema {
    pub types: Vec<CanonicalStateType>,
}

impl CanonicalStateSchema {
    /// Computes the one normative state-layout content identity.
    ///
    /// Type enumeration order is canonicalized, while field declaration order
    /// remains semantic. The compact `StableId`s remain ABI identities; the
    /// complete framed BLAKE3 digest is the content identity used by build,
    /// verifier, and reload.
    #[must_use]
    pub fn fingerprint(&self) -> StateSchemaFingerprint {
        let mut types = self.types.iter().collect::<Vec<_>>();
        types.sort_by_key(|state_type| state_type.stable_id);
        let mut builder = FingerprintBuilder::new(StateSchemaFingerprint::DOMAIN, 2);
        builder.field_u64("type-count", u64::try_from(types.len()).unwrap_or(u64::MAX));
        for state_type in types {
            builder.field_u64("type-id", state_type.stable_id.0);
            builder.field_u32("type-version", state_type.version);
            builder.field_u64(
                "field-count",
                u64::try_from(state_type.fields.len()).unwrap_or(u64::MAX),
            );
            for field in &state_type.fields {
                builder.field_u64("field-id", field.stable_id.0);
                match field.ty {
                    CanonicalValueType::I32 => builder.field_u8("field-type", 0),
                    CanonicalValueType::I64 => builder.field_u8("field-type", 1),
                    CanonicalValueType::F32 => builder.field_u8("field-type", 2),
                    CanonicalValueType::F64 => builder.field_u8("field-type", 3),
                    CanonicalValueType::Bool => builder.field_u8("field-type", 4),
                    CanonicalValueType::Rune => builder.field_u8("field-type", 5),
                    CanonicalValueType::String => builder.field_u8("field-type", 6),
                    CanonicalValueType::Ref => builder.field_u8("field-type", 7),
                    CanonicalValueType::Named(stable_id) => {
                        builder.field_u8("field-type", 8);
                        builder.field_u64("field-named-id", stable_id.0);
                    }
                }
            }
        }
        StateSchemaFingerprint::from_bytes(builder.finish_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintParseError;

impl fmt::Display for FingerprintParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fingerprint must contain exactly 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for FingerprintParseError {}

fn parse_fingerprint(value: &str) -> Result<[u8; 32], FingerprintParseError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(FingerprintParseError);
    }
    let mut bytes = [0_u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let offset = index * 2;
        let high = hex_value(value.as_bytes()[offset]).ok_or(FingerprintParseError)?;
        let low = hex_value(value.as_bytes()[offset + 1]).ok_or(FingerprintParseError)?;
        *byte = (high << 4) | low;
    }
    Ok(bytes)
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[must_use]
pub fn machine_state_id(machine: &str, state: &str) -> StableId {
    StableId::from_parts(&[machine, "::State::", state])
}

#[must_use]
pub fn machine_event_id(machine: &str, event: &str) -> StableId {
    StableId::from_parts(&[machine, "::Event::", event])
}

#[must_use]
pub const fn machine_instance_id(handle: RawHandle) -> u64 {
    (handle.generation as u64) << 32 | handle.index as u64
}

/// Hashes the invariant-visible state shared by the runtime trace and reference model.
#[must_use]
pub fn machine_invariant_hash(
    machine: &str,
    state: &str,
    owner_scope: Option<RawHandle>,
    resources: &[(&str, i64)],
) -> u64 {
    let resource_ids = resources
        .iter()
        .map(|(resource, amount)| (StableId::from_name(resource), *amount));
    machine_invariant_hash_ids(
        StableId::from_name(machine),
        machine_state_id(machine, state),
        owner_scope,
        resource_ids,
    )
}

/// Allocation-free invariant hashing for runtime hot paths.
#[must_use]
pub fn machine_invariant_hash_ids(
    machine: StableId,
    state: StableId,
    owner_scope: Option<RawHandle>,
    resources: impl IntoIterator<Item = (StableId, i64)>,
) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    update(&machine.0.to_le_bytes());
    update(&state.0.to_le_bytes());
    match owner_scope {
        Some(owner) => {
            update(&[1]);
            update(&owner.realm_id.to_le_bytes());
            update(&owner.index.to_le_bytes());
            update(&owner.generation.to_le_bytes());
        }
        None => update(&[0]),
    }
    for (resource, amount) in resources {
        update(&resource.0.to_le_bytes());
        update(&amount.to_le_bytes());
    }
    hash
}

impl fmt::Display for StableId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

/// Runtime state-machine category used by versioned traces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MachineKind {
    Task,
    Scope,
    Module,
    Reload,
    HostRequest,
    ResourceToken,
    ReleaseQueue,
    Custom(StableId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransitionDisposition {
    Applied,
    GuardRejected,
    Undefined,
}

/// Resource accounting changes caused by one transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceDelta {
    pub resource: StableId,
    pub amount: i64,
}

pub const MAX_INLINE_RESOURCE_DELTAS: usize = 4;

/// Fixed-capacity transition deltas, sized for every generated Nexa machine transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineDeltas {
    values: [ResourceDelta; MAX_INLINE_RESOURCE_DELTAS],
    len: u8,
}

impl Default for InlineDeltas {
    fn default() -> Self {
        Self::new()
    }
}

impl InlineDeltas {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: [ResourceDelta {
                resource: StableId(0),
                amount: 0,
            }; MAX_INLINE_RESOURCE_DELTAS],
            len: 0,
        }
    }

    pub fn try_push(&mut self, delta: ResourceDelta) -> Result<(), ResourceDelta> {
        let index = usize::from(self.len);
        let Some(slot) = self.values.get_mut(index) else {
            return Err(delta);
        };
        *slot = delta;
        self.len += 1;
        Ok(())
    }

    #[must_use]
    pub const fn as_slice(&self) -> &[ResourceDelta] {
        self.values.split_at(self.len as usize).0
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResourceDelta> {
        self.as_slice().iter()
    }
}

/// Versioned trace record emitted by generated state-machine transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    pub schema_version: u16,
    pub sequence: u64,
    pub machine_kind: MachineKind,
    pub machine_id: u64,
    pub transition_id: StableId,
    pub disposition: TransitionDisposition,
    pub old_state: StableId,
    pub event: StableId,
    pub new_state: StableId,
    pub realm_id: u32,
    pub module_epoch: u64,
    pub owner_scope: Option<RawHandle>,
    pub resource_deltas: InlineDeltas,
    pub error_code: Option<u32>,
    pub invariant_hash: u64,
}

pub const TRACE_SCHEMA_VERSION: u16 = 1;

#[cfg(test)]
mod tests {
    use super::{
        BuildFingerprint, CanonicalStateField, CanonicalStateSchema, CanonicalStateType,
        CanonicalSymbolIdentity, CanonicalValueType, FileId, FingerprintBuilder, RawHandle,
        SourceSpan, StableId, StableSymbolRegistry, SymbolKind, machine_event_id,
        machine_invariant_hash, machine_state_id,
    };

    #[test]
    fn stable_ids_are_reproducible_and_name_sensitive() {
        assert_eq!(
            StableId::from_name("TASK_CREATED_START_READY"),
            StableId::from_name("TASK_CREATED_START_READY")
        );
        assert_ne!(
            StableId::from_name("TASK_CREATED_START_READY"),
            StableId::from_name("TASK_READY_POLL_RUNNING")
        );
    }

    #[test]
    fn source_spans_are_half_open() {
        let span = SourceSpan::new(FileId(3), 4, 11);
        assert_eq!(span.len(), 7);
        assert!(!span.is_empty());
    }

    #[test]
    fn machine_trace_ids_and_invariants_are_canonical() {
        assert_eq!(
            machine_state_id("Task", "Ready"),
            StableId::from_name("Task::State::Ready")
        );
        assert_eq!(
            machine_event_id("Task", "Poll"),
            StableId::from_name("Task::Event::Poll")
        );
        let owner = RawHandle::new(1, 2, 3);
        assert_eq!(
            machine_invariant_hash("Task", "Ready", Some(owner), &[("task_slot", 1)]),
            machine_invariant_hash("Task", "Ready", Some(owner), &[("task_slot", 1)])
        );
    }

    #[test]
    fn fingerprint_fields_are_length_framed() {
        let mut first = FingerprintBuilder::new("test", 1);
        first.field_str("a", "ab");
        first.field_str("b", "c");
        let mut second = FingerprintBuilder::new("test", 1);
        second.field_str("a", "a");
        second.field_str("b", "bc");
        assert_ne!(first.finish_bytes(), second.finish_bytes());
    }

    #[test]
    fn explicit_stable_identity_survives_move_and_rename() {
        let automatic_before = CanonicalSymbolIdentity::automatic(
            "example.app",
            "score.old",
            SymbolKind::Function,
            "calculate",
        );
        let automatic_after = CanonicalSymbolIdentity::automatic(
            "example.app",
            "score.new",
            SymbolKind::Function,
            "compute",
        );
        assert_ne!(automatic_before.runtime_id(), automatic_after.runtime_id());

        let explicit_before = CanonicalSymbolIdentity::explicit(
            "example.app",
            SymbolKind::Function,
            "classic-score-policy",
        );
        let explicit_after = CanonicalSymbolIdentity::explicit(
            "example.app",
            SymbolKind::Function,
            "classic-score-policy",
        );
        assert_eq!(explicit_before.runtime_id(), explicit_after.runtime_id());
    }

    #[test]
    fn package_registry_retains_canonical_identity() {
        let identity = CanonicalSymbolIdentity::automatic(
            "example.app",
            "score",
            SymbolKind::Function,
            "calculate",
        );
        let mut registry = StableSymbolRegistry::new();
        let id = registry.insert(identity.clone()).expect("unique identity");
        assert_eq!(registry.insert(identity.clone()), Ok(id));
        assert_eq!(registry.identity(id), Some(&identity));
    }

    #[test]
    fn package_registry_rejects_forced_runtime_id_truncation_collision() {
        let first = CanonicalSymbolIdentity::automatic(
            "example.app",
            "score.first",
            SymbolKind::Function,
            "calculate",
        );
        let second = CanonicalSymbolIdentity::automatic(
            "example.app",
            "score.second",
            SymbolKind::Function,
            "calculate",
        );
        assert_ne!(first, second);

        let forced_runtime_id = second.runtime_id();
        let mut registry = StableSymbolRegistry::new();
        registry.identities.insert(forced_runtime_id, first.clone());

        let collision = registry
            .insert(second.clone())
            .expect_err("different canonical identities sharing one runtime id must be rejected");
        assert_eq!(collision.runtime_id, forced_runtime_id);
        assert_eq!(*collision.first, first);
        assert_eq!(*collision.second, second);
    }

    #[test]
    fn fingerprints_serialize_as_canonical_hex() {
        let fingerprint = BuildFingerprint::from_bytes([0xab; 32]);
        let json = serde_json::to_string(&fingerprint).expect("serialize fingerprint");
        assert_eq!(json, format!("\"{}\"", "ab".repeat(32)));
        assert_eq!(
            serde_json::from_str::<BuildFingerprint>(&json).expect("deserialize fingerprint"),
            fingerprint
        );
    }

    #[test]
    fn canonical_state_fingerprint_preserves_field_order_and_type() {
        let field_a = CanonicalStateField {
            stable_id: StableId::from_name("State::a"),
            ty: CanonicalValueType::I32,
        };
        let field_b = CanonicalStateField {
            stable_id: StableId::from_name("State::b"),
            ty: CanonicalValueType::String,
        };
        let build = |fields| {
            CanonicalStateSchema {
                types: vec![CanonicalStateType {
                    stable_id: StableId::from_name("State"),
                    version: 1,
                    fields,
                }],
            }
            .fingerprint()
        };
        assert_ne!(build(vec![field_a, field_b]), build(vec![field_b, field_a]));
        assert_ne!(
            build(vec![field_a]),
            build(vec![CanonicalStateField {
                ty: CanonicalValueType::I64,
                ..field_a
            }])
        );
    }
}
