use std::collections::BTreeSet;

use crate::capability::CapabilitySet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustLevel {
    FirstParty,
    Trusted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationPolicy {
    Required,
    DefaultEnabled,
    UserControlled,
    Programmatic,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationSet(BTreeSet<ActivationPolicy>);

impl ActivationSet {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = ActivationPolicy>) -> Self {
        Self(values.into_iter().collect())
    }

    #[must_use]
    pub fn contains(&self, value: ActivationPolicy) -> bool {
        self.0.contains(&value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageRuntimeLimits {
    pub handler_fuel: u64,
    pub cumulative_budget: u64,
    pub heap_objects: u32,
    pub host_resources: u32,
    pub tasks: u32,
    pub release_records: usize,
}

impl Default for PackageRuntimeLimits {
    fn default() -> Self {
        Self {
            handler_fuel: 20_000,
            cumulative_budget: 20_000,
            heap_objects: 4_096,
            host_resources: 1_024,
            tasks: 128,
            release_records: 2_048,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePolicy {
    pub trust: TrustLevel,
    pub capability_ceiling: CapabilitySet,
    pub allowed_activation: ActivationSet,
    pub max_packages: usize,
    pub runtime_limits: PackageRuntimeLimits,
    pub allow_entitlement: bool,
}
