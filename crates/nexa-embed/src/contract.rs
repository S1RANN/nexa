pub use nexa_runtime::HostContract;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportRequirement {
    pub name: String,
    pub stable_id: nexa_core::StableId,
    pub signature: nexa_runtime::Signature,
}

impl ExportRequirement {
    #[must_use]
    pub fn of<E: nexa_runtime::ScriptExport>() -> Self {
        Self {
            name: E::NAME.to_owned(),
            stable_id: E::STABLE_ID,
            signature: E::signature(),
        }
    }
}
