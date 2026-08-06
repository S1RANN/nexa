use std::fmt;
use std::sync::Arc;

/// A byte offset in UTF-8 source text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextSize(u32);

impl TextSize {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn to_usize(self) -> usize {
        self.0 as usize
    }
}

impl From<u32> for TextSize {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

/// A half-open UTF-8 byte range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TextRange {
    pub start: TextSize,
    pub end: TextSize,
}

impl TextRange {
    #[must_use]
    pub const fn new(start: TextSize, end: TextSize) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn at(start: TextSize, len: u32) -> Self {
        Self {
            start,
            end: TextSize::new(start.get().saturating_add(len)),
        }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.get().saturating_sub(self.start.get())
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start.get() >= self.end.get()
    }

    #[must_use]
    pub const fn contains(self, offset: TextSize) -> bool {
        self.start.get() <= offset.get() && offset.get() < self.end.get()
    }
}

/// Owned immutable source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceText(Arc<str>);

impl SourceText {
    pub fn new(text: impl Into<Arc<str>>) -> Result<Self, SourceTooLarge> {
        let text = text.into();
        u32::try_from(text.len()).map_err(|_| SourceTooLarge { bytes: text.len() })?;
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> TextSize {
        TextSize::new(u32::try_from(self.0.len()).expect("source length checked at construction"))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn slice(&self, range: TextRange) -> Option<&str> {
        self.0.get(range.start.to_usize()..range.end.to_usize())
    }

    #[must_use]
    pub fn line_index(&self) -> LineIndex {
        LineIndex::new(self)
    }
}

impl AsRef<str> for SourceText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceTooLarge {
    pub bytes: usize,
}

impl fmt::Display for SourceTooLarge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "source is {} bytes, exceeding the 32-bit syntax range",
            self.bytes
        )
    }
}

impl std::error::Error for SourceTooLarge {}

/// A zero-based line and column pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct LineColumn {
    pub line: u32,
    pub column: u32,
}

/// The unit used for a reported source column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextEncoding {
    Utf8,
    UnicodeScalar,
    Utf16,
}

/// Immutable line starts for UTF-8, Unicode scalar and LSP UTF-16 conversion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineIndex {
    source: SourceText,
    line_starts: Vec<TextSize>,
}

impl LineIndex {
    #[must_use]
    pub fn new(source: &SourceText) -> Self {
        // CRLF-aware line breaking: `\n`, `\r\n`, and a lone `\r` all terminate a
        // line, matching nexa-diagnostics' LineIndex and the CLI's LSP conversion.
        let mut line_starts = vec![TextSize::ZERO];
        let bytes = source.as_str().as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\n' => {
                    let next = u32::try_from(cursor + 1).expect("source length is checked");
                    line_starts.push(TextSize::new(next));
                    cursor += 1;
                }
                b'\r' => {
                    let end = if bytes.get(cursor + 1) == Some(&b'\n') {
                        cursor + 2
                    } else {
                        cursor + 1
                    };
                    let next = u32::try_from(end).expect("source length is checked");
                    line_starts.push(TextSize::new(next));
                    cursor = end;
                }
                _ => cursor += 1,
            }
        }
        Self {
            source: source.clone(),
            line_starts,
        }
    }

    #[must_use]
    pub fn line_count(&self) -> u32 {
        u32::try_from(self.line_starts.len()).expect("line count fits source byte length")
    }

    #[must_use]
    pub fn line_start(&self, line: u32) -> Option<TextSize> {
        self.line_starts.get(line as usize).copied()
    }

    #[must_use]
    pub fn line_end(&self, line: u32) -> Option<TextSize> {
        let start = self.line_start(line)?;
        let mut end = self
            .line_starts
            .get(line as usize + 1)
            .copied()
            .unwrap_or_else(|| self.source.len());
        let source = self.source.as_str().as_bytes();
        // Strip `\n`, then a preceding `\r` (`\r\n`), or a lone `\r`; all are line
        // terminators, so the content end excludes the full terminator.
        if end > start && source.get(end.to_usize() - 1) == Some(&b'\n') {
            end = TextSize::new(end.get() - 1);
        }
        if end > start && source.get(end.to_usize() - 1) == Some(&b'\r') {
            end = TextSize::new(end.get() - 1);
        }
        Some(end)
    }

    #[must_use]
    pub fn line_column(&self, offset: TextSize, encoding: TextEncoding) -> Option<LineColumn> {
        // Floor to a char boundary and clamp to the source, matching the authority's
        // `floor_char_boundary` semantics for offsets inside astral characters.
        let mut offset = offset.min(self.source.len());
        let source = self.source.as_str();
        while !source.is_char_boundary(offset.to_usize()) {
            offset = TextSize::new(offset.get().saturating_sub(1));
        }
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line];
        // Clamp to the line's content end so offsets at `\n`, `\r\n`, or a lone `\r`
        // report the end of the previous line, matching the authority exactly.
        let logical_offset = offset.min(self.line_end(u32::try_from(line).ok()?)?);
        let prefix = self
            .source
            .as_str()
            .get(line_start.to_usize()..logical_offset.to_usize())?;
        let column = match encoding {
            TextEncoding::Utf8 => u32::try_from(prefix.len()).ok()?,
            TextEncoding::UnicodeScalar => u32::try_from(prefix.chars().count()).ok()?,
            TextEncoding::Utf16 => u32::try_from(prefix.encode_utf16().count()).ok()?,
        };
        Some(LineColumn {
            line: u32::try_from(line).ok()?,
            column,
        })
    }

    #[must_use]
    pub fn offset(&self, position: LineColumn, encoding: TextEncoding) -> Option<TextSize> {
        let start = self.line_start(position.line)?;
        let end = self.line_end(position.line)?;
        let text = self.source.as_str().get(start.to_usize()..end.to_usize())?;
        let relative = match encoding {
            TextEncoding::Utf8 => {
                let column = usize::try_from(position.column).ok()?;
                text.is_char_boundary(column).then_some(column)?
            }
            TextEncoding::UnicodeScalar => offset_for_scalar_column(text, position.column)?,
            TextEncoding::Utf16 => offset_for_utf16_column(text, position.column)?,
        };
        let absolute = start.get().checked_add(u32::try_from(relative).ok()?)?;
        Some(TextSize::new(absolute))
    }
}

fn offset_for_scalar_column(text: &str, column: u32) -> Option<usize> {
    if column == 0 {
        return Some(0);
    }
    let mut scalars = 0_u32;
    for (offset, _) in text.char_indices() {
        if scalars == column {
            return Some(offset);
        }
        scalars += 1;
    }
    (scalars == column).then_some(text.len())
}

fn offset_for_utf16_column(text: &str, column: u32) -> Option<usize> {
    if column == 0 {
        return Some(0);
    }
    let mut units = 0_u32;
    for (offset, character) in text.char_indices() {
        if units == column {
            return Some(offset);
        }
        units = units.checked_add(u32::try_from(character.len_utf16()).ok()?)?;
        if units > column {
            return None;
        }
    }
    (units == column).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::{LineColumn, SourceText, TextEncoding, TextSize};

    /// The authority semantics from `nexa-cli/src/lsp.rs::byte_offset_to_lsp_position`
    /// (and `nexa-diagnostics/src/source.rs`): floor to a char boundary, split on
    /// `\n`, `\r\n`, and lone `\r`, then clamp to the line's content end.
    fn reference_lines(source: &str) -> Vec<(usize, usize)> {
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
        lines
    }

    fn reference_utf16_column(source: &str, byte_offset: usize) -> Option<(u32, u32)> {
        let mut offset = byte_offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let lines = reference_lines(source);
        let line_index = lines
            .partition_point(|(line_start, _)| *line_start <= offset)
            .saturating_sub(1);
        let (line_start, content_end) = lines[line_index];
        let logical_offset = offset.min(content_end);
        Some((
            u32::try_from(line_index).ok()?,
            u32::try_from(source[line_start..logical_offset].encode_utf16().count()).ok()?,
        ))
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

    #[test]
    fn utf16_line_column_matches_authority_for_every_byte_offset() {
        for source in MIXED_SOURCES {
            let index = SourceText::new(*source).expect("small source").line_index();
            for byte_offset in 0..=source.len() {
                let offset = TextSize::new(u32::try_from(byte_offset).expect("short test"));
                let actual = index
                    .line_column(offset, TextEncoding::Utf16)
                    .expect("in-range offset");
                let expected = reference_utf16_column(source, byte_offset).expect("reference");
                assert_eq!(
                    (actual.line, actual.column),
                    expected,
                    "source={source:?} byte_offset={byte_offset}"
                );
            }
        }
    }

    #[test]
    fn utf16_column_floors_offsets_inside_astral_characters() {
        let index = SourceText::new("a😀b").expect("small source").line_index();
        assert_eq!(
            index.line_column(TextSize::new(2), TextEncoding::Utf16),
            Some(LineColumn { line: 0, column: 1 })
        );
        assert_eq!(
            index.line_column(TextSize::new(3), TextEncoding::Utf16),
            Some(LineColumn { line: 0, column: 1 })
        );
        assert_eq!(
            index.line_column(TextSize::new(4), TextEncoding::Utf16),
            Some(LineColumn { line: 0, column: 1 }),
            "inside the astral character, floors to its start"
        );
        assert_eq!(
            index.line_column(TextSize::new(5), TextEncoding::Utf16),
            Some(LineColumn { line: 0, column: 3 })
        );
    }

    #[test]
    fn line_ends_exclude_crlf_and_lone_cr_terminators() {
        for source in MIXED_SOURCES {
            let index = SourceText::new(*source).expect("small source").line_index();
            let lines = reference_lines(source);
            for (line, &(_, content_end)) in lines.iter().enumerate() {
                assert_eq!(
                    index.line_end(u32::try_from(line).expect("short test")),
                    Some(TextSize::new(
                        u32::try_from(content_end).expect("short test")
                    )),
                    "source={source:?} line={line}"
                );
            }
        }
    }

    #[test]
    fn crlf_and_lone_cr_are_recognized_as_line_breaks() {
        let source = SourceText::new("a\r\nb\rc\n")
            .expect("small source")
            .line_index();
        assert_eq!(source.line_count(), 4);
        assert_eq!(
            source.line_start(1),
            Some(TextSize::new(u32::try_from("a\r\n".len()).expect("short")))
        );
        assert_eq!(
            source.line_start(2),
            Some(TextSize::new(
                u32::try_from("a\r\nb\r".len()).expect("short")
            ))
        );
        assert_eq!(
            source.line_start(3),
            Some(TextSize::new(
                u32::try_from("a\r\nb\rc\n".len()).expect("short")
            ))
        );
        assert_eq!(
            source.line_column(TextSize::new(2), TextEncoding::Utf16),
            Some(LineColumn { line: 0, column: 1 })
        );
        assert_eq!(
            source.line_column(TextSize::new(3), TextEncoding::Utf16),
            Some(LineColumn { line: 1, column: 0 })
        );
        assert_eq!(
            source.line_column(TextSize::new(4), TextEncoding::Utf16),
            Some(LineColumn { line: 1, column: 1 }),
            "lone CR has no UTF-16 column; clamps to the end of \"b\""
        );
        assert_eq!(
            source.line_column(TextSize::new(5), TextEncoding::Utf16),
            Some(LineColumn { line: 2, column: 0 })
        );
    }

    #[test]
    fn reverse_offset_still_rejects_mid_utf16_columns() {
        let index = SourceText::new("a😀b\r\n下一行")
            .expect("small source")
            .line_index();
        assert_eq!(
            index.offset(LineColumn { line: 0, column: 2 }, TextEncoding::Utf16),
            None,
            "column 2 lands inside the astral character"
        );
        assert_eq!(
            index.offset(LineColumn { line: 0, column: 3 }, TextEncoding::Utf16),
            Some(TextSize::new(u32::try_from("a😀".len()).expect("short")))
        );
        assert_eq!(
            index.offset(LineColumn { line: 1, column: 1 }, TextEncoding::Utf16),
            Some(TextSize::new(
                u32::try_from("a😀b\r\n下".len()).expect("short")
            ))
        );
    }
}
