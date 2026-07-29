use std::io::Write;
use std::path::{Path, PathBuf};

use crate::settings::SnakeSettings;

#[must_use]
pub fn default_data_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/nexa-snake-user-data")
}

pub fn load(path: &Path) -> Result<SnakeSettings, std::io::Error> {
    if !path.exists() {
        return Ok(SnakeSettings::default());
    }
    let source = std::fs::read_to_string(path)?;
    serde_json::from_str(&source)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub fn save(path: &Path, settings: &SnakeSettings) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}
