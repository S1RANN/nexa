pub use nexa::HostContract;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportRequirement {
    pub name: String,
    pub stable_id: nexa::StableId,
    pub signature: nexa::prelude::Signature,
}

impl ExportRequirement {
    #[must_use]
    pub fn of<E: nexa::ScriptExport>() -> Self {
        Self {
            name: E::NAME.to_owned(),
            stable_id: E::STABLE_ID,
            signature: E::signature(),
        }
    }
}
