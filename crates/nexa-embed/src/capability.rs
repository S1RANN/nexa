use std::collections::BTreeSet;

use crate::manifest::CapabilityId;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySet(BTreeSet<CapabilityId>);

impl CapabilitySet {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = CapabilityId>) -> Self {
        Self(values.into_iter().collect())
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
        Self(self.0.union(&other.0).cloned().collect())
    }

    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).cloned().collect())
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &CapabilityId> {
        self.0.iter()
    }
}
