use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::manifest::PackageId;

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
struct PersistedSelections {
    enabled: BTreeMap<String, bool>,
}

pub(crate) fn load(directory: &Path) -> Result<BTreeMap<PackageId, bool>, std::io::Error> {
    let path = directory.join("package-selections.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let source = std::fs::read_to_string(path)?;
    let persisted: PersistedSelections = serde_json::from_str(&source)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    persisted
        .enabled
        .into_iter()
        .map(|(id, enabled)| {
            PackageId::new(id)
                .map(|id| (id, enabled))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })
        .collect()
}

pub(crate) fn save(
    directory: &Path,
    values: &BTreeMap<PackageId, bool>,
) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join("package-selections.json");
    let temporary = temporary_path(&path);
    let persisted = PersistedSelections {
        enabled: values
            .iter()
            .map(|(id, enabled)| (id.as_str().to_owned(), *enabled))
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&persisted)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}
