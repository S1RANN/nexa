use std::collections::BTreeMap;

use nexa_embed::PackageId;

use crate::commands::FoodDefinition;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedEntry<T> {
    pub owner: PackageId,
    pub local_id: String,
    pub value: T,
}

#[derive(Clone, Debug, Default)]
pub struct UiRegistry {
    entries: BTreeMap<String, OwnedEntry<String>>,
}

impl UiRegistry {
    pub fn register(&mut self, owner: &PackageId, local_id: String) {
        self.entries.insert(
            namespaced(owner, &local_id),
            OwnedEntry {
                owner: owner.clone(),
                local_id,
                value: String::new(),
            },
        );
    }

    pub fn set_text(&mut self, owner: &PackageId, local_id: &str, text: String) -> bool {
        self.entries
            .get_mut(&namespaced(owner, local_id))
            .is_some_and(|entry| {
                entry.value = text;
                true
            })
    }

    #[must_use]
    pub fn owns(&self, owner: &PackageId, local_id: &str) -> bool {
        self.entries
            .get(&namespaced(owner, local_id))
            .is_some_and(|entry| entry.owner == *owner)
    }

    pub fn remove_owner(&mut self, owner: &PackageId) {
        self.entries.retain(|_, entry| entry.owner != *owner);
    }

    pub fn values(&self) -> impl Iterator<Item = &OwnedEntry<String>> {
        self.entries.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

macro_rules! string_registry {
    ($name:ident) => {
        #[derive(Clone, Debug, Default)]
        pub struct $name {
            entries: BTreeMap<String, OwnedEntry<String>>,
        }

        impl $name {
            pub fn register(&mut self, owner: &PackageId, local_id: String) {
                self.entries.insert(
                    namespaced(owner, &local_id),
                    OwnedEntry {
                        owner: owner.clone(),
                        value: local_id.clone(),
                        local_id,
                    },
                );
            }

            pub fn remove_owner(&mut self, owner: &PackageId) {
                self.entries.retain(|_, entry| entry.owner != *owner);
            }

            pub fn contains(&self, id: &str) -> bool {
                self.entries.contains_key(id)
            }

            #[must_use]
            pub fn owner(&self, id: &str) -> Option<&PackageId> {
                self.entries.get(id).map(|entry| &entry.owner)
            }

            pub fn first_id(&self) -> Option<String> {
                self.entries.keys().next().cloned()
            }

            pub fn ids(&self) -> Vec<String> {
                self.entries.keys().cloned().collect()
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.entries.is_empty()
            }

            #[must_use]
            pub fn len(&self) -> usize {
                self.entries.len()
            }
        }
    };
}

string_registry!(SkinRegistry);
string_registry!(SpawnPolicyRegistry);

#[derive(Clone, Debug, Default)]
pub struct FoodRegistry {
    entries: BTreeMap<String, OwnedEntry<FoodDefinition>>,
}

impl FoodRegistry {
    pub fn register(&mut self, owner: &PackageId, definition: FoodDefinition) {
        let local_id = definition.local_id.clone();
        self.entries.insert(
            namespaced(owner, &local_id),
            OwnedEntry {
                owner: owner.clone(),
                local_id,
                value: definition,
            },
        );
    }

    pub fn remove_owner(&mut self, owner: &PackageId) {
        self.entries.retain(|_, entry| entry.owner != *owner);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn select(&self, index: usize) -> Option<(&str, &FoodDefinition)> {
        if self.entries.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .nth(index % self.entries.len())
            .map(|(id, entry)| (id.as_str(), &entry.value))
    }

    #[must_use]
    pub fn owner(&self, id: &str) -> Option<&PackageId> {
        self.entries.get(id).map(|entry| &entry.owner)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExtensionRegistries {
    pub ui: UiRegistry,
    pub skins: SkinRegistry,
    pub foods: FoodRegistry,
    pub spawn_policies: SpawnPolicyRegistry,
}

impl ExtensionRegistries {
    pub fn remove_owner(&mut self, owner: &PackageId) {
        self.ui.remove_owner(owner);
        self.skins.remove_owner(owner);
        self.foods.remove_owner(owner);
        self.spawn_policies.remove_owner(owner);
    }
}

#[must_use]
pub fn namespaced(owner: &PackageId, local_id: &str) -> String {
    format!("{}:{local_id}", owner.as_str())
}
