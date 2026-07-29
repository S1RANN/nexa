use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct SnakeSettings {
    pub enabled_packages: BTreeSet<String>,
    pub selected_skin: Option<String>,
    pub selected_spawn_policy: Option<String>,
    pub entitlements: BTreeSet<String>,
    pub total_plays: i64,
}
