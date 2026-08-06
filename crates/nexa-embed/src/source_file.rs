use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path};

use nexa::prelude::{FileId, SourceSpan};

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
        // CRLF-aware line breaking: `\n`, `\r\n`, and a lone `\r` all terminate a
        // line, matching nexa-diagnostics' LineIndex and the CLI's LSP conversion.
        let mut line_starts = vec![0];
        let bytes = text.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\n' => {
                    line_starts.push(cursor + 1);
                    cursor += 1;
                }
                b'\r' => {
                    let end = if bytes.get(cursor + 1) == Some(&b'\n') {
                        cursor + 2
                    } else {
                        cursor + 1
                    };
                    line_starts.push(end);
                    cursor = end;
                }
                _ => cursor += 1,
            }
        }
        Self {
            id,
            path,
            text,
            line_starts,
        }
    }

    /// Exclusive end of a line's content: the byte offset of the line terminator
    /// (`\n`, `\r\n`, or a lone `\r`), or the end of text for the final line.
    fn line_content_end(&self, line_index: usize) -> usize {
        let line_start = self.line_starts[line_index];
        let next_start = self
            .line_starts
            .get(line_index + 1)
            .copied()
            .unwrap_or(self.text.len());
        let bytes = self.text.as_bytes();
        if next_start <= line_start {
            return next_start;
        }
        match bytes[next_start - 1] {
            b'\n' if next_start - line_start >= 2 && bytes[next_start - 2] == b'\r' => {
                next_start - 2
            }
            b'\n' | b'\r' => next_start - 1,
            _ => next_start,
        }
    }

    /// Converts a byte offset to one-based line and column values. The byte offset is
    /// floored to a char boundary and clamped to the end of the line's content, so an
    /// offset at a line terminator still reports the end of the previous line.
    #[must_use]
    pub fn line_column(&self, byte_offset: usize) -> (usize, usize) {
        let offset = floor_char_boundary(&self.text, byte_offset.min(self.text.len()));
        let line = self
            .line_starts
            .partition_point(|line_start| *line_start <= offset)
            .saturating_sub(1);
        let logical_offset = offset.min(self.line_content_end(line));
        let column = self.text[self.line_starts[line]..logical_offset]
            .chars()
            .count();
        (line + 1, column + 1)
    }

    /// Converts a byte offset to a zero-based LSP position with a UTF-16 column. The
    /// byte offset is floored to a char boundary and clamped to the end of the line's
    /// content, matching the CLI's `byte_offset_to_lsp_position` exactly.
    #[must_use]
    pub fn lsp_position(&self, byte_offset: usize) -> SourcePosition {
        let offset = floor_char_boundary(&self.text, byte_offset.min(self.text.len()));
        let line = self
            .line_starts
            .partition_point(|line_start| *line_start <= offset)
            .saturating_sub(1);
        let logical_offset = offset.min(self.line_content_end(line));
        let utf16_column = self.text[self.line_starts[line]..logical_offset]
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

#[cfg(test)]
mod tests {
    use nexa::prelude::FileId;

    use super::{SourceFile, SourcePosition};

    /// The authority semantics from `nexa-cli/src/lsp.rs::byte_offset_to_lsp_position`
    /// (and `nexa-diagnostics/src/source.rs`): floor to a char boundary, split on
    /// `\n`, `\r\n`, and lone `\r`, then clamp to the line's content end.
    fn reference_lsp_position(source: &str, byte_offset: usize) -> SourcePosition {
        let mut offset = byte_offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }

        let bytes = source.as_bytes();
        let mut lines = Vec::<(usize, usize)>::new();
        let mut start = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\n' => {
                    lines.push((start, cursor));
                    cursor += 1;
                    start = cursor;
                }
                b'\r' => {
                    lines.push((start, cursor));
                    cursor += usize::from(bytes.get(cursor + 1) == Some(&b'\n')) + 1;
                    start = cursor;
                }
                _ => cursor += 1,
            }
        }
        lines.push((start, source.len()));

        let line_index = lines
            .partition_point(|(line_start, _)| *line_start <= offset)
            .saturating_sub(1);
        let (line_start, content_end) = lines[line_index];
        let logical_offset = offset.min(content_end);
        let character = source[line_start..logical_offset].encode_utf16().count();
        SourcePosition {
            line: u32::try_from(line_index).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        }
    }

    const MIXED_SOURCES: &[&str] = &[
        "",
        "\n",
        "\r",
        "\r\n",
        "plain",
        "a\nb",
        "a\rb",
        "a\r\nb",
        "a\r\n\r\nb",
        "a\r\rb",
        "a😀b\r\n中x\n",
        "a\rb\rc\r",
        "\ta\tb\r\n\t中😀",
        "😀x\r\ny😀",
        "crlf\r\nonly\r",
    ];

    fn sample_file(source: &str) -> SourceFile {
        SourceFile::new(FileId(1), "main.nexa".to_owned(), source.to_owned())
    }

    #[test]
    fn lsp_positions_match_authority_for_every_byte_offset() {
        for source in MIXED_SOURCES {
            let file = sample_file(source);
            for byte_offset in 0..=source.len() {
                let expected = reference_lsp_position(source, byte_offset);
                assert_eq!(
                    file.lsp_position(byte_offset),
                    expected,
                    "source={source:?} byte_offset={byte_offset}"
                );
            }
        }
    }

    #[test]
    fn lsp_positions_match_the_cli_exact_crlf_astral_expectations() {
        let source = "a😀\r\n界b\rz";
        let file = sample_file(source);
        let position = |offset| file.lsp_position(offset);
        assert_eq!(position(0), SourcePosition::default());
        assert_eq!(
            position(1),
            SourcePosition {
                line: 0,
                character: 1
            }
        );
        assert_eq!(position(3), position(1), "floors an astral byte offset");
        assert_eq!(
            position(5),
            SourcePosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(position(6), position(5), "CRLF has no UTF-16 column");
        assert_eq!(
            position(7),
            SourcePosition {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            position(10),
            SourcePosition {
                line: 1,
                character: 1
            }
        );
        assert_eq!(
            position(11),
            SourcePosition {
                line: 1,
                character: 2
            },
            "lone CR has no UTF-16 column; clamps to the end of \"界b\""
        );
        assert_eq!(
            position(12),
            SourcePosition {
                line: 2,
                character: 0
            }
        );
    }

    #[test]
    fn tabs_count_as_single_utf16_units() {
        let file = sample_file("\ta\t中😀");
        assert_eq!(
            file.lsp_position(1),
            SourcePosition {
                line: 0,
                character: 1
            }
        );
        assert_eq!(
            file.lsp_position(2),
            SourcePosition {
                line: 0,
                character: 2
            }
        );
        assert_eq!(
            file.lsp_position(3),
            SourcePosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            file.lsp_position(5),
            file.lsp_position(3),
            "mid-astral offsets floor to the character start"
        );
        assert_eq!(
            file.lsp_position(6),
            SourcePosition {
                line: 0,
                character: 4
            },
            "tab counts as one UTF-16 unit"
        );
        assert_eq!(
            file.lsp_position(9),
            file.lsp_position(6),
            "mid-astral offsets floor to the character start"
        );
        assert_eq!(
            file.lsp_position(10),
            SourcePosition {
                line: 0,
                character: 6
            },
            "astral character counts as two UTF-16 units"
        );
    }

    /// Authority semantics for the one-based human renderer (`source.rs::human_position`).
    fn reference_human_column(source: &str, byte_offset: usize) -> (usize, usize) {
        let mut offset = byte_offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let bytes = source.as_bytes();
        let mut lines = Vec::<(usize, usize)>::new();
        let mut start = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\n' => {
                    lines.push((start, cursor));
                    cursor += 1;
                    start = cursor;
                }
                b'\r' => {
                    lines.push((start, cursor));
                    cursor += usize::from(bytes.get(cursor + 1) == Some(&b'\n')) + 1;
                    start = cursor;
                }
                _ => cursor += 1,
            }
        }
        lines.push((start, source.len()));
        let line_index = lines
            .partition_point(|(line_start, _)| *line_start <= offset)
            .saturating_sub(1);
        let (line_start, content_end) = lines[line_index];
        let logical_offset = offset.min(content_end);
        (
            line_index + 1,
            source[line_start..logical_offset].chars().count() + 1,
        )
    }

    #[test]
    fn human_line_column_matches_authority_for_every_byte_offset() {
        for source in MIXED_SOURCES {
            let file = sample_file(source);
            for byte_offset in 0..=source.len() {
                let expected = reference_human_column(source, byte_offset);
                assert_eq!(
                    file.line_column(byte_offset),
                    expected,
                    "source={source:?} byte_offset={byte_offset}"
                );
            }
        }
    }

    #[test]
    fn line_text_excludes_crlf_and_lone_cr_terminators() {
        let file = sample_file("a\r\nb\rc\n");
        assert_eq!(file.line_text(1), Some("a"));
        assert_eq!(file.line_text(2), Some("b"));
        assert_eq!(file.line_text(3), Some("c"));
        assert_eq!(file.line_text(4), Some(""));
    }
}
