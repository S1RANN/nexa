use std::fmt;

pub use nexa_analysis::{
    ActivationPolicy, ManifestError as AnalysisManifestError, PackageId, PackageManifest, SourceId,
};
pub use semver::Version as PackageVersion;

use crate::capability::CapabilitySet;
use crate::policy::{PackagePolicy, PackageRuntimeLimits};

macro_rules! validated_string {
    ($name:ident, $validator:expr, $label:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ManifestError> {
                let value = value.into();
                if !($validator)(&value) {
                    return Err(ManifestError::InvalidIdentifier {
                        kind: $label,
                        value,
                    });
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

fn valid_capability(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn valid_entitlement(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_whitespace)
}

validated_string!(CapabilityId, valid_capability, "capability");
validated_string!(EntitlementId, valid_entitlement, "entitlement");

/// Host-policy values after schema-2 application defaults and ceilings have been applied.
///
/// This lives beside, rather than inside, the immutable analysis manifest: a single canonical
/// candidate can be evaluated under different host-owned package sources without changing its
/// source identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveApplicationSettings {
    pub activation: ActivationPolicy,
    pub priority: i32,
    pub runtime_limits: PackageRuntimeLimits,
    pub capabilities: CapabilitySet,
    pub entitlement: Option<EntitlementId>,
}

pub(crate) fn apply_package_policy(
    manifest: &PackageManifest,
    policy: &PackagePolicy,
) -> Result<EffectiveApplicationSettings, ManifestError> {
    let application = manifest
        .application
        .as_ref()
        .ok_or(ManifestError::LibraryCannotExecute)?;
    if !policy.allowed_activation.contains(application.activation) {
        return Err(ManifestError::ActivationNotAllowed);
    }

    let requested_capabilities = application
        .capabilities
        .iter()
        .cloned()
        .map(CapabilityId::new)
        .collect::<Result<Vec<_>, _>>()?;
    let capabilities = CapabilitySet::new(requested_capabilities);
    if !capabilities.is_subset(&policy.capability_ceiling) {
        return Err(ManifestError::CapabilityCeiling);
    }

    let entitlement = application
        .entitlement
        .as_ref()
        .map(|value| EntitlementId::new(value.clone()))
        .transpose()?;
    if entitlement.is_some() && !policy.allow_entitlement {
        return Err(ManifestError::EntitlementNotAllowed);
    }

    let ceiling = policy.runtime_limits;
    let runtime_limits = PackageRuntimeLimits {
        handler_fuel: application.handler_fuel.unwrap_or(ceiling.handler_fuel),
        cumulative_budget: application
            .cumulative_budget
            .unwrap_or(ceiling.cumulative_budget),
        heap_objects: application.heap_objects.unwrap_or(ceiling.heap_objects),
        host_resources: application.host_resources.unwrap_or(ceiling.host_resources),
        tasks: application.tasks.unwrap_or(ceiling.tasks),
        release_records: application
            .release_records
            .unwrap_or(ceiling.release_records),
    };
    if !runtime_limits.within(ceiling) {
        return Err(ManifestError::RuntimeLimit);
    }

    Ok(EffectiveApplicationSettings {
        activation: application.activation,
        priority: application.priority,
        runtime_limits,
        capabilities,
        entitlement,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    Analysis(String),
    InvalidIdentifier { kind: &'static str, value: String },
    LibraryCannotExecute,
    ActivationNotAllowed,
    CapabilityCeiling,
    RuntimeLimit,
    EntitlementNotAllowed,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Analysis(error) => formatter.write_str(error),
            Self::InvalidIdentifier { kind, value } => {
                write!(formatter, "invalid {kind} identifier: {value:?}")
            }
            Self::LibraryCannotExecute => {
                formatter.write_str("library packages cannot create an execution realm")
            }
            Self::ActivationNotAllowed => {
                formatter.write_str("application activation is not allowed by package policy")
            }
            Self::CapabilityCeiling => {
                formatter.write_str("application capability request exceeds package policy")
            }
            Self::RuntimeLimit => {
                formatter.write_str("application runtime limit exceeds package policy")
            }
            Self::EntitlementNotAllowed => {
                formatter.write_str("application entitlement is not allowed by package policy")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

impl From<AnalysisManifestError> for ManifestError {
    fn from(error: AnalysisManifestError) -> Self {
        Self::Analysis(error.to_string())
    }
}
