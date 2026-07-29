use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum BuildError {
    MissingFileStem(PathBuf),
    MissingOutDir,
    Io(std::io::Error),
    Idl(super::IdlError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for BuildError {}

impl From<std::io::Error> for BuildError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<super::IdlError> for BuildError {
    fn from(error: super::IdlError) -> Self {
        Self::Idl(error)
    }
}

pub fn generate(path: impl AsRef<Path>) -> Result<PathBuf, BuildError> {
    let path = path.as_ref();
    let source = std::fs::read_to_string(path)?;
    let idl = super::parse(&source)?;
    let stem = path
        .file_stem()
        .ok_or_else(|| BuildError::MissingFileStem(path.to_path_buf()))?;
    let out_dir = std::env::var_os("OUT_DIR").ok_or(BuildError::MissingOutDir)?;
    let output = PathBuf::from(out_dir).join(stem).with_extension("rs");
    std::fs::write(&output, super::generate_rust(&idl))?;
    println!("cargo:rerun-if-changed={}", path.display());
    Ok(output)
}
