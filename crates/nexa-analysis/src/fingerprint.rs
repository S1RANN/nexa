use std::collections::BTreeMap;

pub use nexa_core::{
    BuildFingerprint, FingerprintBuilder, LinkedStateFingerprint, PublicApiFingerprint,
    SourceSetFingerprint, StateSchemaFingerprint,
};

use crate::{Definition, IrType, PackageId, PackageSourceSet, StateTypeIr, TypedIrError};

const SOURCE_SET_SCHEMA: u16 = 2;
const PUBLIC_API_SCHEMA: u16 = 1;
const STATE_SCHEMA_SCHEMA: u16 = 1;
const BUILD_SCHEMA: u16 = 1;

/// One canonical, already type-checked semantic record.
///
/// The caller is responsible for producing the normative payload encoding for the semantic kind.
/// The fingerprint layer supplies deterministic sorting, a kind marker, and unambiguous framing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticFingerprintRecord {
    pub canonical_identity: String,
    pub kind: String,
    pub payload: Vec<u8>,
}

#[must_use]
pub fn source_set_fingerprint(source_set: &PackageSourceSet) -> SourceSetFingerprint {
    let mut builder = FingerprintBuilder::new(SourceSetFingerprint::DOMAIN, SOURCE_SET_SCHEMA);
    let production = source_set.production_units().collect::<Vec<_>>();
    builder.field_u64(
        "source-count",
        u64::try_from(production.len()).unwrap_or(u64::MAX),
    );
    for unit in production {
        builder.field_str("package-id", unit.key.package_id.as_str());
        builder.field_str("path", unit.key.path.as_str());
        match unit.virtual_module_path() {
            Some(module) => {
                builder.field_str("module-identity", "virtual");
                builder.field_str("virtual-module", module.as_str());
            }
            None => builder.field_str("module-identity", "path"),
        }
        builder.field_bytes("source", unit.text.as_bytes());
    }
    SourceSetFingerprint::from_bytes(builder.finish_bytes())
}

#[must_use]
pub fn public_api_fingerprint(
    records: impl IntoIterator<Item = SemanticFingerprintRecord>,
) -> PublicApiFingerprint {
    let bytes =
        semantic_records_fingerprint(PublicApiFingerprint::DOMAIN, PUBLIC_API_SCHEMA, records);
    PublicApiFingerprint::from_bytes(bytes)
}

#[must_use]
pub fn state_schema_fingerprint(
    records: impl IntoIterator<Item = SemanticFingerprintRecord>,
) -> StateSchemaFingerprint {
    let bytes =
        semantic_records_fingerprint(StateSchemaFingerprint::DOMAIN, STATE_SCHEMA_SCHEMA, records);
    StateSchemaFingerprint::from_bytes(bytes)
}

/// Lowers analyzed state metadata through the same runtime-neutral ABI model used by bytecode and
/// the verifier. This is the only valid state fingerprint for a compiled package.
pub fn canonical_state_schema(
    state_types: &[StateTypeIr],
    definitions: &[Definition],
) -> Result<nexa_core::CanonicalStateSchema, TypedIrError> {
    let mut types = Vec::with_capacity(state_types.len());
    for state in state_types {
        let mut fields = Vec::with_capacity(state.fields.len());
        for field in &state.fields {
            fields.push(nexa_core::CanonicalStateField {
                stable_id: field.stable_id.0,
                ty: canonical_value_type(&field.ty, definitions)?,
            });
        }
        types.push(nexa_core::CanonicalStateType {
            stable_id: state.stable_id.0,
            version: state.version,
            fields,
        });
    }
    Ok(nexa_core::CanonicalStateSchema { types })
}

pub fn canonical_value_type(
    ty: &IrType,
    definitions: &[Definition],
) -> Result<nexa_core::CanonicalValueType, TypedIrError> {
    use nexa_core::CanonicalValueType;
    let named = |stable_id| Ok(CanonicalValueType::Named(stable_id));
    match ty {
        IrType::I32 => Ok(CanonicalValueType::I32),
        IrType::I64 => Ok(CanonicalValueType::I64),
        IrType::F32 => Ok(CanonicalValueType::F32),
        IrType::F64 => Ok(CanonicalValueType::F64),
        IrType::Bool => Ok(CanonicalValueType::Bool),
        IrType::Rune => Ok(CanonicalValueType::Rune),
        IrType::String => Ok(CanonicalValueType::String),
        IrType::Named(definition) => {
            let stable = definitions
                .get(definition.0 as usize)
                .and_then(|definition| definition.stable_symbol.as_ref())
                .ok_or(TypedIrError::MissingStableSymbol(*definition))?;
            named(stable.runtime_id.0)
        }
        IrType::Option(inner) => named(nexa_core::canonical_option_type_id(canonical_value_type(
            inner,
            definitions,
        )?)),
        IrType::Result(ok, error) => named(nexa_core::canonical_result_type_id(
            canonical_value_type(ok, definitions)?,
            canonical_value_type(error, definitions)?,
        )),
        IrType::Array(inner) => named(nexa_core::canonical_array_type_id(canonical_value_type(
            inner,
            definitions,
        )?)),
        IrType::Map(key, value) => named(nexa_core::canonical_map_type_id(
            canonical_value_type(key, definitions)?,
            canonical_value_type(value, definitions)?,
        )),
        IrType::Tuple(items) => {
            let items = items
                .iter()
                .map(|item| canonical_value_type(item, definitions))
                .collect::<Result<Vec<_>, _>>()?;
            named(nexa_core::canonical_tuple_type_id(&items))
        }
        IrType::HostRequest(_) => named(nexa_core::StableId::from_name("HostRequest")),
        IrType::ResourceToken(_) => named(nexa_core::StableId::from_name("ResourceToken")),
        IrType::Snapshot(content) => {
            let CanonicalValueType::Named(content) = canonical_value_type(content, definitions)?
            else {
                return Err(TypedIrError::InvalidSnapshotContentType);
            };
            named(nexa_core::canonical_snapshot_type_id(content))
        }
        IrType::Buffer(inner) => named(nexa_core::canonical_buffer_type_id(canonical_value_type(
            inner,
            definitions,
        )?)),
        IrType::StateHandle(inner) => named(nexa_core::canonical_state_handle_type_id(
            canonical_value_type(inner, definitions)?,
        )),
        IrType::Unit | IrType::TypeParameter(_) => Err(TypedIrError::NonRuntimeStateType),
    }
}

fn semantic_records_fingerprint(
    domain: &str,
    schema: u16,
    records: impl IntoIterator<Item = SemanticFingerprintRecord>,
) -> [u8; 32] {
    let mut records = records.into_iter().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        (
            &left.canonical_identity,
            &left.kind,
            left.payload.as_slice(),
        )
            .cmp(&(
                &right.canonical_identity,
                &right.kind,
                right.payload.as_slice(),
            ))
    });
    let mut builder = FingerprintBuilder::new(domain, schema);
    builder.field_u64(
        "record-count",
        u64::try_from(records.len()).unwrap_or(u64::MAX),
    );
    for record in records {
        builder.field_str("identity", &record.canonical_identity);
        builder.field_str("kind", &record.kind);
        builder.field_bytes("payload", &record.payload);
    }
    builder.finish_bytes()
}

/// Every input that changes a statically linked root package artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildFingerprintInput {
    pub root_package: PackageId,
    /// Canonical schema-2 root manifest bytes.
    pub root_manifest: Vec<u8>,
    pub root_source_set: SourceSetFingerprint,
    /// Canonical schema-2 dependency manifests keyed by resolved package identity.
    pub dependency_manifests: BTreeMap<PackageId, Vec<u8>>,
    pub dependency_source_sets: BTreeMap<PackageId, SourceSetFingerprint>,
    /// Canonical semantic Host ABI bytes.
    pub host_contract: Vec<u8>,
    /// Exact source/debug identity for the Host contract, including standalone URI and raw text.
    pub host_contract_source: Vec<u8>,
    /// Canonical subset of Host-declared exports which this build must implement.
    ///
    /// The complete Host ABI remains in `host_contract`; this field changes only the effective
    /// export-requirement view selected by the build owner.
    pub host_required_exports: Vec<u8>,
    pub language_version: String,
    pub standard_library_version: String,
    /// Canonical descriptor bytes for every compiler-provided static module and intrinsic.
    ///
    /// The human version is retained for diagnostics, but cannot by itself protect against an
    /// implementation/catalog change whose version was accidentally not bumped.
    pub standard_library_descriptor: Vec<u8>,
    pub compiler_version: String,
    pub bytecode_version: u32,
    pub runtime_semantics_version: u32,
    pub opcode_cost_table_version: u32,
    pub deterministic_math_backend: String,
    pub compiler_options: Vec<u8>,
    pub canonical_lock_graph: Vec<u8>,
}

impl BuildFingerprintInput {
    #[must_use]
    pub fn fingerprint(&self) -> BuildFingerprint {
        let mut builder = FingerprintBuilder::new(BuildFingerprint::DOMAIN, BUILD_SCHEMA);
        builder.field_str("root-package", self.root_package.as_str());
        builder.field_bytes("root-manifest", &self.root_manifest);
        builder.field_bytes("root-source-set", self.root_source_set.as_bytes());
        builder.field_u64(
            "dependency-count",
            u64::try_from(self.dependency_source_sets.len()).unwrap_or(u64::MAX),
        );
        for (package, source_set) in &self.dependency_source_sets {
            builder.field_str("dependency-package", package.as_str());
            if let Some(manifest) = self.dependency_manifests.get(package) {
                builder.field_bytes("dependency-manifest", manifest);
            } else {
                // Missing is framed explicitly and cannot alias an empty manifest.
                builder.field_u8("dependency-manifest-missing", 1);
            }
            builder.field_bytes("dependency-source-set", source_set.as_bytes());
        }
        for (package, manifest) in &self.dependency_manifests {
            if !self.dependency_source_sets.contains_key(package) {
                builder.field_str("manifest-only-dependency-package", package.as_str());
                builder.field_bytes("manifest-only-dependency", manifest);
            }
        }
        builder.field_bytes("host-contract", &self.host_contract);
        builder.field_bytes("host-contract-source", &self.host_contract_source);
        builder.field_bytes("host-required-exports", &self.host_required_exports);
        builder.field_str("language-version", &self.language_version);
        builder.field_str("standard-library-version", &self.standard_library_version);
        builder.field_bytes(
            "standard-library-descriptor",
            &self.standard_library_descriptor,
        );
        builder.field_str("compiler-version", &self.compiler_version);
        builder.field_u32("bytecode-version", self.bytecode_version);
        builder.field_u32("runtime-semantics-version", self.runtime_semantics_version);
        builder.field_u32("opcode-cost-table-version", self.opcode_cost_table_version);
        builder.field_str(
            "deterministic-math-backend",
            &self.deterministic_math_backend,
        );
        builder.field_bytes("compiler-options", &self.compiler_options);
        builder.field_bytes("lock-graph", &self.canonical_lock_graph);
        BuildFingerprint::from_bytes(builder.finish_bytes())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        CompilationLimits, ModulePath, NormalizedPackagePath, PackageId, SourceRole,
        SourceSetBuilder,
    };

    use super::*;

    #[test]
    fn source_enumeration_order_does_not_change_fingerprint() {
        let package = PackageId::new("example.package").unwrap();
        let paths = ["src/a.nexa", "src/z.nexa"];
        let build = |paths: &[&str]| {
            let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
            for path in paths {
                builder
                    .add(
                        NormalizedPackagePath::new(path).unwrap(),
                        format!("module {};", path.as_bytes()[4] as char),
                        SourceRole::Production,
                    )
                    .unwrap();
            }
            source_set_fingerprint(&builder.build().unwrap())
        };
        assert_eq!(build(&paths), build(&["src/z.nexa", "src/a.nexa"]));
    }

    #[test]
    fn test_sources_do_not_change_product_source_fingerprint() {
        let package = PackageId::new("example.package").unwrap();
        let mut base = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
        base.add(
            NormalizedPackagePath::new("src/main.nexa").unwrap(),
            "module main;",
            SourceRole::Production,
        )
        .unwrap();
        let base = source_set_fingerprint(&base.build().unwrap());

        let mut with_test = SourceSetBuilder::new(package, CompilationLimits::default());
        with_test
            .add(
                NormalizedPackagePath::new("src/main.nexa").unwrap(),
                "module main;",
                SourceRole::Production,
            )
            .unwrap()
            .add(
                NormalizedPackagePath::new("tests/check.nexa").unwrap(),
                "module test.check;",
                SourceRole::Test,
            )
            .unwrap();
        assert_eq!(base, source_set_fingerprint(&with_test.build().unwrap()));
    }

    #[test]
    fn virtual_module_identity_is_part_of_source_fingerprint() {
        let package = PackageId::new("example.package").unwrap();
        let path = NormalizedPackagePath::new("src/main.nexa").unwrap();
        let source = "fn main() -> i32 { return 0; }\r\n";

        let fingerprint_for = |module: &str| {
            let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
            builder
                .add_virtual_snippet(path.clone(), source, ModulePath::new(module).unwrap())
                .unwrap();
            source_set_fingerprint(&builder.build().unwrap())
        };

        assert_ne!(fingerprint_for("main"), fingerprint_for("alternate"));
    }

    #[test]
    fn public_api_records_are_canonicalized() {
        let a = SemanticFingerprintRecord {
            canonical_identity: "pkg::a".into(),
            kind: "function".into(),
            payload: b"() -> i32".to_vec(),
        };
        let b = SemanticFingerprintRecord {
            canonical_identity: "pkg::b".into(),
            kind: "const".into(),
            payload: b"i32=1".to_vec(),
        };
        assert_eq!(
            public_api_fingerprint([a.clone(), b.clone()]),
            public_api_fingerprint([b, a])
        );
    }
}
