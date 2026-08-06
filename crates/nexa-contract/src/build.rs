use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum BuildError {
    MissingFileStem(PathBuf),
    MissingOutDir,
    Io(std::io::Error),
    Nidl(super::ContractError),
    Codegen(super::CodegenError),
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

impl From<super::ContractError> for BuildError {
    fn from(error: super::ContractError) -> Self {
        Self::Nidl(error)
    }
}

impl From<super::CodegenError> for BuildError {
    fn from(error: super::CodegenError) -> Self {
        Self::Codegen(error)
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
    let generated = super::generate_rust(&idl)?;
    write_atomically(&output, generated.as_bytes())?;
    println!("cargo:rerun-if-changed={}", path.display());
    Ok(output)
}

fn write_atomically(output: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let (temporary, mut file) = create_temporary_file(output)?;

    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);

    if let Err(error) = std::fs::rename(&temporary, output) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    Ok(())
}

fn create_temporary_file(output: &Path) -> std::io::Result<(PathBuf, File)> {
    let directory = output.parent().unwrap_or_else(|| Path::new("."));
    let file_name = output.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "generated binding output has no file name",
        )
    })?;

    loop {
        let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = file_name.to_os_string();
        temporary_name.push(format!(".tmp.{}.{id}", std::process::id()));
        let temporary = directory.join(temporary_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}
