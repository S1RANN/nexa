use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use nexa_core::{FileId, SourceSpan};

/// One immutable source file captured for a package candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub id: FileId,
    pub path: String,
    pub text: String,
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn new(id: FileId, path: String, text: String) -> Self {
        let mut line_starts = vec![0];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset + 1);
            }
        }
        Self {
            id,
            path,
            text,
            line_starts,
        }
    }

    /// Converts a byte offset to one-based line and column values.
    #[must_use]
    pub fn line_column(&self, byte_offset: usize) -> (usize, usize) {
        let offset = floor_char_boundary(&self.text, byte_offset.min(self.text.len()));
        let line = self
            .line_starts
            .partition_point(|line_start| *line_start <= offset)
            .saturating_sub(1);
        let column = self.text[self.line_starts[line]..offset].chars().count();
        (line + 1, column + 1)
    }

    /// Converts a byte offset to a zero-based LSP position with a UTF-16 column.
    #[must_use]
    pub fn lsp_position(&self, byte_offset: usize) -> SourcePosition {
        let offset = floor_char_boundary(&self.text, byte_offset.min(self.text.len()));
        let line = self
            .line_starts
            .partition_point(|line_start| *line_start <= offset)
            .saturating_sub(1);
        let utf16_column = self.text[self.line_starts[line]..offset]
            .encode_utf16()
            .count();
        SourcePosition {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(utf16_column).unwrap_or(u32::MAX),
        }
    }

    #[must_use]
    pub fn line_text(&self, one_based_line: usize) -> Option<&str> {
        let start = *self.line_starts.get(one_based_line.checked_sub(1)?)?;
        let end = self
            .line_starts
            .get(one_based_line)
            .copied()
            .unwrap_or(self.text.len());
        Some(self.text[start..end].trim_end_matches(['\r', '\n']))
    }
}

fn floor_char_boundary(source: &str, mut offset: usize) -> usize {
    while !source.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    offset
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourcePosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceRange {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceFileRegistry {
    files: BTreeMap<FileId, SourceFile>,
    ids_by_path: BTreeMap<String, FileId>,
}

impl SourceFileRegistry {
    pub fn from_files<I, P, S>(files: I) -> Result<Self, SourceFileRegistryError>
    where
        I: IntoIterator<Item = (P, S)>,
        P: AsRef<Path>,
        S: Into<String>,
    {
        let mut normalized = BTreeMap::<String, String>::new();
        for (path, text) in files {
            let path = normalize_package_path(path.as_ref())?;
            if normalized.insert(path.clone(), text.into()).is_some() {
                return Err(SourceFileRegistryError::DuplicatePath(path));
            }
        }
        let mut registry = Self::default();
        for (index, (path, text)) in normalized.into_iter().enumerate() {
            let raw_id =
                u32::try_from(index + 1).map_err(|_| SourceFileRegistryError::TooManyFiles)?;
            let id = FileId(raw_id);
            registry
                .files
                .insert(id, SourceFile::new(id, path.clone(), text));
            registry.ids_by_path.insert(path, id);
        }
        Ok(registry)
    }

    #[must_use]
    pub fn file(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(&id)
    }

    #[must_use]
    pub fn file_by_path(&self, path: &str) -> Option<&SourceFile> {
        self.ids_by_path.get(path).and_then(|id| self.files.get(id))
    }

    #[must_use]
    pub fn file_id(&self, path: &str) -> Option<FileId> {
        self.ids_by_path.get(path).copied()
    }

    #[must_use]
    pub fn files(&self) -> impl ExactSizeIterator<Item = &SourceFile> {
        self.files.values()
    }

    #[must_use]
    pub fn source_range(&self, span: SourceSpan) -> Option<SourceRange> {
        let file = self.file(span.file)?;
        Some(SourceRange {
            start: file.lsp_position(span.start as usize),
            end: file.lsp_position(span.end as usize),
        })
    }
}

fn normalize_package_path(path: &Path) -> Result<String, SourceFileRegistryError> {
    if path.is_absolute() {
        return Err(SourceFileRegistryError::EscapedPackage(
            path.display().to_string(),
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SourceFileRegistryError::EscapedPackage(
                    path.display().to_string(),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(SourceFileRegistryError::EmptyPath);
    }
    Ok(parts.join("/"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceFileRegistryError {
    EmptyPath,
    EscapedPackage(String),
    DuplicatePath(String),
    TooManyFiles,
}

impl fmt::Display for SourceFileRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => formatter.write_str("source path is empty"),
            Self::EscapedPackage(path) => {
                write!(formatter, "source path escapes the package: {path}")
            }
            Self::DuplicatePath(path) => write!(formatter, "duplicate source path: {path}"),
            Self::TooManyFiles => formatter.write_str("too many source files"),
        }
    }
}

impl std::error::Error for SourceFileRegistryError {}
