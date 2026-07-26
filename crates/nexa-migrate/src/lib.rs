//! Versioned state fixtures and offline migration support.

mod check;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub use check::{
    MigrateCheckConfig, MigrateCheckError, MigrateCheckResult, MigrateCheckUsage,
    MigrateObjectDiff, MigrateStateDiff, run_migrate_check,
};

pub const STATE_FIXTURE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFixtureLimits {
    pub max_bytes: usize,
    pub max_objects: usize,
    pub max_fields: usize,
    pub max_string_bytes: usize,
}

impl Default for StateFixtureLimits {
    fn default() -> Self {
        Self {
            max_bytes: 16 << 20,
            max_objects: 65_536,
            max_fields: 262_144,
            max_string_bytes: 16 << 20,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFixture {
    pub format_version: u32,
    pub stateful_domain: u64,
    pub objects: Vec<StateFixtureObject>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFixtureObject {
    pub stable_id: u64,
    pub type_id: u64,
    pub generation: u64,
    pub fields: Vec<StateFixtureField>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFixtureField {
    pub stable_id: u64,
    pub value: StateFixtureValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateFixtureValue {
    I32 {
        value: i32,
    },
    I64 {
        value: i64,
    },
    Bool {
        value: bool,
    },
    F32 {
        value: f32,
    },
    F64 {
        value: f64,
    },
    Rune {
        value: char,
    },
    String {
        value: String,
    },
    StateHandle {
        domain: u64,
        stable_id: u64,
        generation: u64,
    },
    ObjectReference {
        stable_id: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateFixtureValueKind {
    I32,
    I64,
    Bool,
    F32,
    F64,
    Rune,
    String,
    StateHandle,
    ObjectReference,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateFixtureSchema {
    pub types: Vec<StateFixtureTypeSchema>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateFixtureTypeSchema {
    pub stable_id: u64,
    pub fields: Vec<StateFixtureFieldSchema>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateFixtureFieldSchema {
    pub stable_id: u64,
    pub kind: StateFixtureValueKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateFixtureError {
    InputTooLarge {
        actual: usize,
        limit: usize,
    },
    Json(String),
    UnsupportedFormatVersion(u32),
    InvalidDomain,
    ObjectLimit {
        actual: usize,
        limit: usize,
    },
    FieldLimit {
        actual: usize,
        limit: usize,
    },
    StringLimit {
        actual: usize,
        limit: usize,
    },
    DuplicateObjectStableId(u64),
    DuplicateFieldStableId {
        object: u64,
        field: u64,
    },
    UnknownObjectType {
        object: u64,
        type_id: u64,
    },
    UnknownField {
        object: u64,
        field: u64,
    },
    MissingField {
        object: u64,
        field: u64,
    },
    WrongFieldType {
        object: u64,
        field: u64,
        expected: StateFixtureValueKind,
        actual: StateFixtureValueKind,
    },
    DanglingHandle {
        object: u64,
        field: u64,
        target: u64,
    },
    CrossDomainHandle {
        object: u64,
        field: u64,
        expected: u64,
        actual: u64,
    },
    StaleHandle {
        object: u64,
        field: u64,
        target: u64,
        expected: u32,
        actual: u32,
    },
    DanglingObjectReference {
        object: u64,
        field: u64,
        target: u64,
    },
    GenerationOverflow {
        stable_id: u64,
        generation: u64,
    },
    DuplicateSchemaType(u64),
    DuplicateSchemaField {
        type_id: u64,
        field: u64,
    },
}

impl fmt::Display for StateFixtureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for StateFixtureError {}

pub fn parse_state_fixture(
    bytes: &[u8],
    limits: StateFixtureLimits,
) -> Result<StateFixture, StateFixtureError> {
    if bytes.len() > limits.max_bytes {
        return Err(StateFixtureError::InputTooLarge {
            actual: bytes.len(),
            limit: limits.max_bytes,
        });
    }
    let fixture: StateFixture = serde_json::from_slice(bytes)
        .map_err(|error| StateFixtureError::Json(error.to_string()))?;
    fixture.validate_limits(limits)?;
    Ok(fixture)
}

impl StateFixture {
    pub fn validate(
        &self,
        schema: &StateFixtureSchema,
        limits: StateFixtureLimits,
    ) -> Result<(), StateFixtureError> {
        self.validate_limits(limits)?;
        let schema = schema.index()?;
        let objects = self.object_index()?;
        for object in &self.objects {
            let expected_fields =
                schema
                    .get(&object.type_id)
                    .ok_or(StateFixtureError::UnknownObjectType {
                        object: object.stable_id,
                        type_id: object.type_id,
                    })?;
            let mut fields = BTreeSet::new();
            for field in &object.fields {
                if !fields.insert(field.stable_id) {
                    return Err(StateFixtureError::DuplicateFieldStableId {
                        object: object.stable_id,
                        field: field.stable_id,
                    });
                }
                let expected = expected_fields.get(&field.stable_id).copied().ok_or(
                    StateFixtureError::UnknownField {
                        object: object.stable_id,
                        field: field.stable_id,
                    },
                )?;
                let actual = field.value.kind();
                if actual != expected {
                    return Err(StateFixtureError::WrongFieldType {
                        object: object.stable_id,
                        field: field.stable_id,
                        expected,
                        actual,
                    });
                }
                self.validate_reference(object.stable_id, field, &objects)?;
            }
            if let Some(field) = expected_fields
                .keys()
                .find(|field| !fields.contains(field))
                .copied()
            {
                return Err(StateFixtureError::MissingField {
                    object: object.stable_id,
                    field,
                });
            }
        }
        Ok(())
    }

    fn validate_limits(&self, limits: StateFixtureLimits) -> Result<(), StateFixtureError> {
        if self.format_version != STATE_FIXTURE_FORMAT_VERSION {
            return Err(StateFixtureError::UnsupportedFormatVersion(
                self.format_version,
            ));
        }
        if self.stateful_domain == 0 {
            return Err(StateFixtureError::InvalidDomain);
        }
        if self.objects.len() > limits.max_objects {
            return Err(StateFixtureError::ObjectLimit {
                actual: self.objects.len(),
                limit: limits.max_objects,
            });
        }
        let mut fields = 0_usize;
        let mut string_bytes = 0_usize;
        for object in &self.objects {
            fields =
                fields
                    .checked_add(object.fields.len())
                    .ok_or(StateFixtureError::FieldLimit {
                        actual: usize::MAX,
                        limit: limits.max_fields,
                    })?;
            if object.generation > u64::from(u32::MAX) {
                return Err(StateFixtureError::GenerationOverflow {
                    stable_id: object.stable_id,
                    generation: object.generation,
                });
            }
            for field in &object.fields {
                if let StateFixtureValue::String { value } = &field.value {
                    string_bytes = string_bytes.checked_add(value.len()).ok_or(
                        StateFixtureError::StringLimit {
                            actual: usize::MAX,
                            limit: limits.max_string_bytes,
                        },
                    )?;
                }
                if let StateFixtureValue::StateHandle {
                    stable_id,
                    generation,
                    ..
                } = field.value
                    && generation > u64::from(u32::MAX)
                {
                    return Err(StateFixtureError::GenerationOverflow {
                        stable_id,
                        generation,
                    });
                }
            }
        }
        if fields > limits.max_fields {
            return Err(StateFixtureError::FieldLimit {
                actual: fields,
                limit: limits.max_fields,
            });
        }
        if string_bytes > limits.max_string_bytes {
            return Err(StateFixtureError::StringLimit {
                actual: string_bytes,
                limit: limits.max_string_bytes,
            });
        }
        let _ = self.object_index()?;
        Ok(())
    }

    fn object_index(&self) -> Result<BTreeMap<u64, u32>, StateFixtureError> {
        let mut objects = BTreeMap::new();
        for object in &self.objects {
            let generation = u32::try_from(object.generation).map_err(|_| {
                StateFixtureError::GenerationOverflow {
                    stable_id: object.stable_id,
                    generation: object.generation,
                }
            })?;
            if objects.insert(object.stable_id, generation).is_some() {
                return Err(StateFixtureError::DuplicateObjectStableId(object.stable_id));
            }
        }
        Ok(objects)
    }

    fn validate_reference(
        &self,
        object: u64,
        field: &StateFixtureField,
        objects: &BTreeMap<u64, u32>,
    ) -> Result<(), StateFixtureError> {
        match field.value {
            StateFixtureValue::StateHandle {
                domain,
                stable_id,
                generation,
            } => {
                if domain != self.stateful_domain {
                    return Err(StateFixtureError::CrossDomainHandle {
                        object,
                        field: field.stable_id,
                        expected: self.stateful_domain,
                        actual: domain,
                    });
                }
                let expected =
                    objects
                        .get(&stable_id)
                        .copied()
                        .ok_or(StateFixtureError::DanglingHandle {
                            object,
                            field: field.stable_id,
                            target: stable_id,
                        })?;
                let actual = u32::try_from(generation).map_err(|_| {
                    StateFixtureError::GenerationOverflow {
                        stable_id,
                        generation,
                    }
                })?;
                if actual != expected {
                    return Err(StateFixtureError::StaleHandle {
                        object,
                        field: field.stable_id,
                        target: stable_id,
                        expected,
                        actual,
                    });
                }
            }
            StateFixtureValue::ObjectReference { stable_id } => {
                if !objects.contains_key(&stable_id) {
                    return Err(StateFixtureError::DanglingObjectReference {
                        object,
                        field: field.stable_id,
                        target: stable_id,
                    });
                }
            }
            StateFixtureValue::I32 { .. }
            | StateFixtureValue::I64 { .. }
            | StateFixtureValue::Bool { .. }
            | StateFixtureValue::F32 { .. }
            | StateFixtureValue::F64 { .. }
            | StateFixtureValue::Rune { .. }
            | StateFixtureValue::String { .. } => {}
        }
        Ok(())
    }
}

impl StateFixtureValue {
    #[must_use]
    pub const fn kind(&self) -> StateFixtureValueKind {
        match self {
            Self::I32 { .. } => StateFixtureValueKind::I32,
            Self::I64 { .. } => StateFixtureValueKind::I64,
            Self::Bool { .. } => StateFixtureValueKind::Bool,
            Self::F32 { .. } => StateFixtureValueKind::F32,
            Self::F64 { .. } => StateFixtureValueKind::F64,
            Self::Rune { .. } => StateFixtureValueKind::Rune,
            Self::String { .. } => StateFixtureValueKind::String,
            Self::StateHandle { .. } => StateFixtureValueKind::StateHandle,
            Self::ObjectReference { .. } => StateFixtureValueKind::ObjectReference,
        }
    }
}

impl StateFixtureSchema {
    fn index(
        &self,
    ) -> Result<BTreeMap<u64, BTreeMap<u64, StateFixtureValueKind>>, StateFixtureError> {
        let mut types = BTreeMap::new();
        for ty in &self.types {
            let mut fields = BTreeMap::new();
            for field in &ty.fields {
                if fields.insert(field.stable_id, field.kind).is_some() {
                    return Err(StateFixtureError::DuplicateSchemaField {
                        type_id: ty.stable_id,
                        field: field.stable_id,
                    });
                }
            }
            if types.insert(ty.stable_id, fields).is_some() {
                return Err(StateFixtureError::DuplicateSchemaType(ty.stable_id));
            }
        }
        Ok(types)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        STATE_FIXTURE_FORMAT_VERSION, StateFixtureError, StateFixtureFieldSchema,
        StateFixtureLimits, StateFixtureSchema, StateFixtureTypeSchema, StateFixtureValueKind,
        parse_state_fixture,
    };

    fn schema() -> StateFixtureSchema {
        StateFixtureSchema {
            types: vec![StateFixtureTypeSchema {
                stable_id: 10,
                fields: vec![
                    StateFixtureFieldSchema {
                        stable_id: 100,
                        kind: StateFixtureValueKind::I32,
                    },
                    StateFixtureFieldSchema {
                        stable_id: 101,
                        kind: StateFixtureValueKind::StateHandle,
                    },
                    StateFixtureFieldSchema {
                        stable_id: 102,
                        kind: StateFixtureValueKind::ObjectReference,
                    },
                ],
            }],
        }
    }

    fn fixture(handle_domain: u64, handle_target: u64, generation: u64) -> String {
        format!(
            r#"{{
                "format_version": {STATE_FIXTURE_FORMAT_VERSION},
                "stateful_domain": 7,
                "objects": [
                    {{
                        "stable_id": 1,
                        "type_id": 10,
                        "generation": 3,
                        "fields": [
                            {{"stable_id": 100, "value": {{"type": "i32", "value": 9}}}},
                            {{"stable_id": 101, "value": {{
                                "type": "state_handle",
                                "domain": {handle_domain},
                                "stable_id": {handle_target},
                                "generation": {generation}
                            }}}},
                            {{"stable_id": 102, "value": {{
                                "type": "object_reference",
                                "stable_id": 1
                            }}}}
                        ]
                    }}
                ]
            }}"#
        )
    }

    #[test]
    fn every_supported_value_type_round_trips() {
        let source = r#"{
            "format_version":1,
            "stateful_domain":7,
            "objects":[{
                "stable_id":1,
                "type_id":10,
                "generation":0,
                "fields":[
                    {"stable_id":1,"value":{"type":"i32","value":-1}},
                    {"stable_id":2,"value":{"type":"i64","value":-2}},
                    {"stable_id":3,"value":{"type":"bool","value":true}},
                    {"stable_id":4,"value":{"type":"f32","value":1.5}},
                    {"stable_id":5,"value":{"type":"f64","value":2.5}},
                    {"stable_id":6,"value":{"type":"rune","value":"界"}},
                    {"stable_id":7,"value":{"type":"string","value":"nexa"}},
                    {"stable_id":8,"value":{"type":"state_handle","domain":7,"stable_id":1,"generation":0}},
                    {"stable_id":9,"value":{"type":"object_reference","stable_id":1}}
                ]
            }]
        }"#;
        let fixture =
            parse_state_fixture(source.as_bytes(), StateFixtureLimits::default()).unwrap();
        let encoded = serde_json::to_vec(&fixture).unwrap();
        assert_eq!(
            parse_state_fixture(&encoded, StateFixtureLimits::default()).unwrap(),
            fixture
        );
    }

    #[test]
    fn schema_and_graph_validation_reject_every_required_error_class() {
        let limits = StateFixtureLimits::default();
        let parsed = parse_state_fixture(fixture(7, 1, 3).as_bytes(), limits).unwrap();
        parsed.validate(&schema(), limits).unwrap();

        let duplicate = fixture(7, 1, 3).replace(
            r#""objects": ["#,
            r#""objects": [{"stable_id":1,"type_id":10,"generation":3,"fields":[]},"#,
        );
        assert!(matches!(
            parse_state_fixture(duplicate.as_bytes(), limits),
            Err(StateFixtureError::DuplicateObjectStableId(1))
        ));

        let wrong_type = fixture(7, 1, 3).replace(
            r#""type": "i32", "value": 9"#,
            r#""type": "bool", "value": true"#,
        );
        assert!(matches!(
            parse_state_fixture(wrong_type.as_bytes(), limits)
                .unwrap()
                .validate(&schema(), limits),
            Err(StateFixtureError::WrongFieldType { .. })
        ));
        assert!(matches!(
            parse_state_fixture(fixture(8, 1, 3).as_bytes(), limits)
                .unwrap()
                .validate(&schema(), limits),
            Err(StateFixtureError::CrossDomainHandle { .. })
        ));
        assert!(matches!(
            parse_state_fixture(fixture(7, 99, 3).as_bytes(), limits)
                .unwrap()
                .validate(&schema(), limits),
            Err(StateFixtureError::DanglingHandle { .. })
        ));
        assert!(matches!(
            parse_state_fixture(fixture(7, 1, u64::from(u32::MAX) + 1).as_bytes(), limits),
            Err(StateFixtureError::GenerationOverflow { .. })
        ));
    }

    #[test]
    fn serde_rejects_unknown_json_fields_and_value_tags() {
        let unknown_field = br#"{
            "format_version":1,
            "stateful_domain":7,
            "objects":[],
            "surprise":true
        }"#;
        assert!(matches!(
            parse_state_fixture(unknown_field, StateFixtureLimits::default()),
            Err(StateFixtureError::Json(_))
        ));
        let unknown_type = br#"{
            "format_version":1,
            "stateful_domain":7,
            "objects":[{
                "stable_id":1,
                "type_id":10,
                "generation":0,
                "fields":[{"stable_id":1,"value":{"type":"bytes","value":[]}}]
            }]
        }"#;
        assert!(matches!(
            parse_state_fixture(unknown_type, StateFixtureLimits::default()),
            Err(StateFixtureError::Json(_))
        ));
    }
}
