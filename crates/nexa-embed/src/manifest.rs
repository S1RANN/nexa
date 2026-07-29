use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::capability::CapabilitySet;
use crate::policy::{ActivationPolicy, PackagePolicy};

macro_rules! validated_id {
    ($name:ident, $validator:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ManifestError> {
                let value = value.into();
                if !($validator)(&value) {
                    return Err(ManifestError::InvalidIdentifier(value));
                }
                Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

fn valid_dotted(value: &str) -> bool {
    !value.is_empty()
        && !value.contains("..")
        && !value.chars().any(char::is_whitespace)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && value.contains('.')
}

fn valid_source(value: &str) -> bool {
    !value.is_empty()
        && !value.contains("..")
        && !value.chars().any(char::is_whitespace)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

validated_id!(SourceId, valid_source);
validated_id!(PackageId, valid_dotted);
validated_id!(CapabilityId, valid_dotted);
validated_id!(EntitlementId, valid_dotted);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageVersion(String);

impl PackageVersion {
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            });
        if !valid {
            return Err(ManifestError::InvalidVersion(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePath(PathBuf);

impl PackagePath {
    pub fn new(value: impl Into<PathBuf>) -> Result<Self, ManifestError> {
        let value = value.into();
        if value.as_os_str().is_empty()
            || value.is_absolute()
            || value.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ManifestError::InvalidPath(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct RawManifest {
    schema: u32,
    id: String,
    name: String,
    version: String,
    entry: String,
    #[serde(default)]
    priority: i32,
    activation: ActivationPolicy,
    #[serde(default = "default_state_schema")]
    state_schema: String,
    handler_fuel: Option<u64>,
    cumulative_budget: Option<u64>,
    heap_objects: Option<u32>,
    host_resources: Option<u32>,
    tasks: Option<u32>,
    release_records: Option<usize>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    entitlement: String,
}

fn default_state_schema() -> String {
    "v1".into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    pub schema: u32,
    pub id: PackageId,
    pub name: String,
    pub version: PackageVersion,
    pub entry: PackagePath,
    pub priority: i32,
    pub activation: ActivationPolicy,
    pub state_schema: String,
    pub handler_fuel: u64,
    pub cumulative_budget: u64,
    pub heap_objects: u32,
    pub host_resources: u32,
    pub tasks: u32,
    pub release_records: usize,
    pub capabilities: CapabilitySet,
    pub entitlement: Option<EntitlementId>,
}

impl PackageManifest {
    pub fn parse(source: &str, policy: &PackagePolicy) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            toml::from_str(source).map_err(|error| ManifestError::Toml(error.to_string()))?;
        if raw.schema != 1 {
            return Err(ManifestError::UnknownSchema(raw.schema));
        }
        if raw.name.trim().is_empty() {
            return Err(ManifestError::EmptyName);
        }
        if !policy.allowed_activation.contains(raw.activation) {
            return Err(ManifestError::ActivationNotAllowed);
        }
        let capability_ids = raw
            .capabilities
            .iter()
            .map(|value| CapabilityId::new(value.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let capabilities = CapabilitySet::new(capability_ids.clone());
        if capabilities.iter().count() != capability_ids.len() {
            return Err(ManifestError::DuplicateCapability);
        }
        if !capabilities.is_subset(&policy.capability_ceiling) {
            return Err(ManifestError::CapabilityCeiling);
        }
        let limits = policy.runtime_limits;
        let handler_fuel = raw.handler_fuel.unwrap_or(limits.handler_fuel);
        let cumulative_budget = raw.cumulative_budget.unwrap_or(limits.cumulative_budget);
        let heap_objects = raw.heap_objects.unwrap_or(limits.heap_objects);
        let host_resources = raw.host_resources.unwrap_or(limits.host_resources);
        let tasks = raw.tasks.unwrap_or(limits.tasks);
        let release_records = raw.release_records.unwrap_or(limits.release_records);
        if handler_fuel > limits.handler_fuel
            || cumulative_budget > limits.cumulative_budget
            || heap_objects > limits.heap_objects
            || host_resources > limits.host_resources
            || tasks > limits.tasks
            || release_records > limits.release_records
        {
            return Err(ManifestError::RuntimeLimit);
        }
        let entitlement = if raw.entitlement.is_empty() {
            None
        } else if !policy.allow_entitlement {
            return Err(ManifestError::EntitlementNotAllowed);
        } else {
            Some(EntitlementId::new(raw.entitlement)?)
        };
        Ok(Self {
            schema: raw.schema,
            id: PackageId::new(raw.id)?,
            name: raw.name,
            version: PackageVersion::new(raw.version)?,
            entry: PackagePath::new(raw.entry)?,
            priority: raw.priority,
            activation: raw.activation,
            state_schema: raw.state_schema,
            handler_fuel,
            cumulative_budget,
            heap_objects,
            host_resources,
            tasks,
            release_records,
            capabilities,
            entitlement,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Toml(String),
    UnknownSchema(u32),
    InvalidIdentifier(String),
    InvalidVersion(String),
    InvalidPath(PathBuf),
    EmptyName,
    DuplicateCapability,
    ActivationNotAllowed,
    CapabilityCeiling,
    RuntimeLimit,
    EntitlementNotAllowed,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ManifestError {}
