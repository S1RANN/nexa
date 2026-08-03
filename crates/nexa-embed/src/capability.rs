use std::collections::BTreeSet;
use std::sync::Arc;

use crate::manifest::CapabilityId;

// M5 WP92: capability sets are immutable once granted - every operation
// builds a new set - so the storage is shared. Cloning one (the
// steady-state dispatch path attaches the effective set to every output)
// is a reference-count bump, never an allocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySet(Arc<BTreeSet<CapabilityId>>);

impl CapabilitySet {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = CapabilityId>) -> Self {
        Self(Arc::new(values.into_iter().collect()))
    }

    #[must_use]
    pub fn contains(&self, value: &CapabilityId) -> bool {
        self.0.contains(value)
    }

    #[must_use]
    pub fn is_subset(&self, other: &Self) -> bool {
        self.0.is_subset(&other.0)
    }

    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        Self(Arc::new(self.0.union(&other.0).cloned().collect()))
    }

    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self(Arc::new(self.0.intersection(&other.0).cloned().collect()))
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CapabilityId> {
        self.0.iter()
    }
}
