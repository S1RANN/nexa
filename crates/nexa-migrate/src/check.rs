use std::fmt;

use nexa_bytecode::{DecodeLimits, ValueType};
use nexa_core::StableId;
use nexa_runtime::{
    MigrationLimits, OfflineStateField, OfflineStateObject, OfflineStateValue, StateHandle,
    StatefulDomainId, run_offline_migration,
};
use serde::Serialize;

use crate::{
    StateFixture, StateFixtureError, StateFixtureField, StateFixtureFieldSchema,
    StateFixtureLimits, StateFixtureObject, StateFixtureSchema, StateFixtureTypeSchema,
    StateFixtureValue, StateFixtureValueKind, parse_state_fixture,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MigrateCheckConfig {
    pub decode_limits: DecodeLimits,
    pub verifier_limits: nexa_verifier::VerifierLimits,
    pub fixture_limits: StateFixtureLimits,
    pub migration_limits: MigrationLimits,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MigrateCheckResult {
    pub old_schema_hash: u64,
    pub new_schema_hash: u64,
    pub migration_entry: u32,
    pub migration_hash: u64,
    pub usage: MigrateCheckUsage,
    pub output_state: StateFixture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MigrateCheckUsage {
    pub object_peak: usize,
    pub field_peak: usize,
    pub forwarding_peak: usize,
    pub payload_byte_peak: usize,
    pub gc_root_peak: usize,
    pub fuel_used: u64,
    pub max_call_depth_used: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrateCheckError {
    DecodeOld(String),
    DecodeNew(String),
    VerifyOld(String),
    VerifyNew(String),
    Fixture(StateFixtureError),
    UnsupportedFixtureValue {
        object: u64,
        field: u64,
        kind: StateFixtureValueKind,
    },
    Runtime(String),
}

impl fmt::Display for MigrateCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for MigrateCheckError {}

pub fn run_migrate_check(
    old_module_bytes: &[u8],
    new_module_bytes: &[u8],
    state_bytes: &[u8],
    config: MigrateCheckConfig,
) -> Result<MigrateCheckResult, MigrateCheckError> {
    let old_module =
        nexa_bytecode::Module::decode_with_limits(old_module_bytes, config.decode_limits)
            .map_err(|error| MigrateCheckError::DecodeOld(error.to_string()))?;
    let new_module =
        nexa_bytecode::Module::decode_with_limits(new_module_bytes, config.decode_limits)
            .map_err(|error| MigrateCheckError::DecodeNew(error.to_string()))?;
    let old_module = nexa_verifier::verify(old_module, config.verifier_limits)
        .map_err(|error| MigrateCheckError::VerifyOld(error.to_string()))?;
    let new_module = nexa_verifier::verify(new_module, config.verifier_limits)
        .map_err(|error| MigrateCheckError::VerifyNew(error.to_string()))?;
    let fixture = parse_state_fixture(state_bytes, config.fixture_limits)
        .map_err(MigrateCheckError::Fixture)?;
    fixture
        .validate(
            &fixture_schema(&old_module.module().state_schema),
            config.fixture_limits,
        )
        .map_err(MigrateCheckError::Fixture)?;
    let input = offline_objects(&fixture)?;
    let output = run_offline_migration(
        StatefulDomainId::new(fixture.stateful_domain),
        input,
        &old_module,
        &new_module,
        config.migration_limits,
    )
    .map_err(|error| MigrateCheckError::Runtime(error.to_string()))?;
    let migration_entry = new_module
        .module()
        .reload_metadata
        .migration_entry
        .ok_or_else(|| MigrateCheckError::Runtime("missing migration entry".into()))?;
    Ok(MigrateCheckResult {
        old_schema_hash: old_module.module().state_schema.stable_hash().0,
        new_schema_hash: new_module.module().state_schema.stable_hash().0,
        migration_entry,
        migration_hash: output.migration_hash.0,
        usage: MigrateCheckUsage {
            object_peak: output.usage.object_peak,
            field_peak: output.usage.field_peak,
            forwarding_peak: output.usage.forwarding_peak,
            payload_byte_peak: output.usage.payload_byte_peak,
            gc_root_peak: output.usage.gc_root_peak,
            fuel_used: output.usage.fuel_used,
            max_call_depth_used: output.usage.max_call_depth_used,
        },
        output_state: output_fixture(fixture.stateful_domain, output.objects),
    })
}

fn fixture_schema(schema: &nexa_bytecode::StateSchema) -> StateFixtureSchema {
    StateFixtureSchema {
        types: schema
            .types
            .iter()
            .map(|ty| StateFixtureTypeSchema {
                stable_id: ty.stable_id.0,
                fields: ty
                    .fields
                    .iter()
                    .map(|field| StateFixtureFieldSchema {
                        stable_id: field.stable_id.0,
                        kind: match field.ty {
                            ValueType::I32 => StateFixtureValueKind::I32,
                            ValueType::Bool => StateFixtureValueKind::Bool,
                            ValueType::Ref => StateFixtureValueKind::ObjectReference,
                            ValueType::Named(_) => StateFixtureValueKind::StateHandle,
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn offline_objects(fixture: &StateFixture) -> Result<Vec<OfflineStateObject>, MigrateCheckError> {
    fixture
        .objects
        .iter()
        .map(|object| {
            Ok(OfflineStateObject {
                stable_id: StableId(object.stable_id),
                type_id: StableId(object.type_id),
                generation: u32::try_from(object.generation)
                    .expect("fixture validation rejects generation overflow"),
                fields: object
                    .fields
                    .iter()
                    .map(|field| offline_field(fixture, object, field))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        })
        .collect()
}

fn offline_field(
    fixture: &StateFixture,
    object: &StateFixtureObject,
    field: &StateFixtureField,
) -> Result<OfflineStateField, MigrateCheckError> {
    let value = match &field.value {
        StateFixtureValue::I32 { value } => OfflineStateValue::I32(*value),
        StateFixtureValue::Bool { value } => OfflineStateValue::Bool(*value),
        StateFixtureValue::StateHandle {
            domain,
            stable_id,
            generation,
        } => OfflineStateValue::Handle(StateHandle {
            domain: StatefulDomainId::new(*domain),
            stable_id: StableId(*stable_id),
            generation: u32::try_from(*generation)
                .expect("fixture validation rejects generation overflow"),
        }),
        value => {
            return Err(MigrateCheckError::UnsupportedFixtureValue {
                object: object.stable_id,
                field: field.stable_id,
                kind: value.kind(),
            });
        }
    };
    debug_assert_eq!(
        fixture.stateful_domain,
        match value {
            OfflineStateValue::Handle(handle) => handle.domain.get(),
            OfflineStateValue::I32(_) | OfflineStateValue::Bool(_) => fixture.stateful_domain,
        }
    );
    Ok(OfflineStateField {
        stable_id: StableId(field.stable_id),
        value,
    })
}

fn output_fixture(domain: u64, objects: Vec<OfflineStateObject>) -> StateFixture {
    StateFixture {
        format_version: crate::STATE_FIXTURE_FORMAT_VERSION,
        stateful_domain: domain,
        objects: objects
            .into_iter()
            .map(|object| StateFixtureObject {
                stable_id: object.stable_id.0,
                type_id: object.type_id.0,
                generation: u64::from(object.generation),
                fields: object
                    .fields
                    .into_iter()
                    .map(|field| StateFixtureField {
                        stable_id: field.stable_id.0,
                        value: match field.value {
                            OfflineStateValue::I32(value) => StateFixtureValue::I32 { value },
                            OfflineStateValue::Bool(value) => StateFixtureValue::Bool { value },
                            OfflineStateValue::Handle(handle) => StateFixtureValue::StateHandle {
                                domain: handle.domain.get(),
                                stable_id: handle.stable_id.0,
                                generation: u64::from(handle.generation),
                            },
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use nexa_core::StableId;
    use serde_json::json;

    use super::{MigrateCheckConfig, run_migrate_check};

    #[test]
    fn migrate_check_uses_decoder_verifier_interpreter_context_and_registry() {
        let old = nexa_compiler::compile(
            "@stateful class Store { value: i32; }
             fn read(value: i32) -> i32 { return value; }",
        )
        .unwrap();
        let new = nexa_compiler::compile(
            "@stateful class Store { value: i32; }
             migration fn migrate() -> bool {
                 finish_migration();
                 return true;
             }",
        )
        .unwrap();
        let fixture = serde_json::to_vec(&json!({
            "format_version": 1,
            "stateful_domain": 7,
            "objects": [{
                "stable_id": StableId::from_name("store").0,
                "type_id": StableId::from_name("Store").0,
                "generation": 0,
                "fields": [{
                    "stable_id": StableId::from_parts(&["Store", "::", "value"]).0,
                    "value": {"type": "i32", "value": 41}
                }]
            }]
        }))
        .unwrap();
        let result = run_migrate_check(
            &old.module().encode(),
            &new.module().encode(),
            &fixture,
            MigrateCheckConfig::default(),
        )
        .unwrap();

        assert_eq!(result.old_schema_hash, result.new_schema_hash);
        assert_eq!(result.migration_entry, 0);
        assert_eq!(result.output_state.objects.len(), 1);
        assert_ne!(result.migration_hash, 0);
        assert!(result.usage.fuel_used > 0);
        assert!(result.usage.max_call_depth_used > 0);
    }
}
