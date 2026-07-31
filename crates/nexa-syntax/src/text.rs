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
        let mut line_starts = vec![TextSize::ZERO];
        for (offset, byte) in source.as_str().bytes().enumerate() {
            if byte == b'\n' {
                let next = u32::try_from(offset + 1).expect("source length is checked");
                line_starts.push(TextSize::new(next));
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
        if end > start && source.get(end.to_usize().saturating_sub(1)) == Some(&b'\n') {
            end = TextSize::new(end.get() - 1);
            if end > start && source.get(end.to_usize().saturating_sub(1)) == Some(&b'\r') {
                end = TextSize::new(end.get() - 1);
            }
        }
        Some(end)
    }

    #[must_use]
    pub fn line_column(&self, offset: TextSize, encoding: TextEncoding) -> Option<LineColumn> {
        if offset > self.source.len() || !self.source.as_str().is_char_boundary(offset.to_usize()) {
            return None;
        }
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line];
        let prefix = self
            .source
            .as_str()
            .get(line_start.to_usize()..offset.to_usize())?;
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
