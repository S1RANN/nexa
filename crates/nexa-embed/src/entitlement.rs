use std::collections::BTreeSet;

use crate::manifest::EntitlementId;

pub trait EntitlementResolver {
    fn contains(&self, id: &EntitlementId) -> bool;
}

#[derive(Default)]
pub struct NoEntitlements;

impl EntitlementResolver for NoEntitlements {
    fn contains(&self, _: &EntitlementId) -> bool {
        false
    }
}

#[derive(Clone, Debug, Default)]
pub struct StaticEntitlements {
    values: BTreeSet<EntitlementId>,
}

impl StaticEntitlements {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = EntitlementId>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl EntitlementResolver for StaticEntitlements {
    fn contains(&self, id: &EntitlementId) -> bool {
        self.values.contains(id)
    }
}
