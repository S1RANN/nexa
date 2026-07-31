use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// A stable source identity. The package and normalized package-relative path can be converted to
/// an analysis-layer `SourceKey` without relying on artifact-local numeric file identifiers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceIdentity {
    package_id: Option<Arc<str>>,
    path: Arc<str>,
}

impl SourceIdentity {
    #[must_use]
    pub fn package(package_id: impl Into<Arc<str>>, path: impl Into<Arc<str>>) -> Self {
        Self {
            package_id: Some(package_id.into()),
            path: path.into(),
        }
    }

    #[must_use]
    pub fn standalone(path: impl Into<Arc<str>>) -> Self {
        Self {
            package_id: None,
            path: path.into(),
        }
    }

    #[must_use]
    pub fn package_id(&self) -> Option<&str> {
        self.package_id.as_deref()
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(package_id) = &self.package_id {
            write!(formatter, "{package_id}:{}", self.path)
        } else {
            formatter.write_str(&self.path)
        }
    }
}

/// A half-open UTF-8 byte range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    pub start: u32,
    pub end: u32,
}

impl ByteRange {
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// A one-based line and Unicode-scalar column used by the human renderer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HumanPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HumanRange {
    pub start: HumanPosition,
    pub end: HumanPosition,
}

/// A zero-based line and UTF-16 code-unit column used by JSON, NDJSON, and LSP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Utf16Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Utf16Range {
    pub start: Utf16Position,
    pub end: Utf16Position,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Line {
    start: usize,
    content_end: usize,
    end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineIndex {
    lines: Vec<Line>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut lines = Vec::new();
        let mut start = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            match bytes[cursor] {
                b'\n' => {
                    lines.push(Line {
                        start,
                        content_end: cursor,
                        end: cursor + 1,
                    });
                    cursor += 1;
                    start = cursor;
                }
                b'\r' => {
                    let end = if bytes.get(cursor + 1) == Some(&b'\n') {
                        cursor + 2
                    } else {
                        cursor + 1
                    };
                    lines.push(Line {
                        start,
                        content_end: cursor,
                        end,
                    });
                    cursor = end;
                    start = cursor;
                }
                _ => cursor += 1,
            }
        }
        lines.push(Line {
            start,
            content_end: text.len(),
            end: text.len(),
        });
        Self { lines }
    }

    fn position_data<'a>(&self, text: &'a str, byte_offset: usize) -> (usize, &'a str) {
        let offset = floor_char_boundary(text, byte_offset.min(text.len()));
        let line_index = self
            .lines
            .partition_point(|line| line.start <= offset)
            .saturating_sub(1);
        let line = self.lines[line_index];
        let logical_offset = offset.min(line.content_end);
        (line_index, &text[line.start..logical_offset])
    }

    fn human_position(&self, text: &str, byte_offset: usize) -> HumanPosition {
        let (line, prefix) = self.position_data(text, byte_offset);
        HumanPosition {
            line: saturating_u32(line.saturating_add(1)),
            column: saturating_u32(prefix.chars().count().saturating_add(1)),
        }
    }

    fn utf16_position(&self, text: &str, byte_offset: usize) -> Utf16Position {
        let (line, prefix) = self.position_data(text, byte_offset);
        Utf16Position {
            line: saturating_u32(line),
            character: saturating_u32(prefix.encode_utf16().count()),
        }
    }

    fn line_text<'a>(&self, text: &'a str, one_based_line: u32) -> Option<&'a str> {
        let line = self
            .lines
            .get(usize::try_from(one_based_line).ok()?.checked_sub(1)?)?;
        Some(&text[line.start..line.content_end])
    }
}

fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    while !text.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    offset
}

fn saturating_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// One immutable source captured for an analysis revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceSnapshot {
    identity: SourceIdentity,
    text: Arc<str>,
    line_index: LineIndex,
}

impl SourceSnapshot {
    fn new(identity: SourceIdentity, text: Arc<str>) -> Self {
        let line_index = LineIndex::new(&text);
        Self {
            identity,
            text,
            line_index,
        }
    }

    #[must_use]
    pub const fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn human_position(&self, byte_offset: usize) -> HumanPosition {
        self.line_index.human_position(&self.text, byte_offset)
    }

    #[must_use]
    pub fn utf16_position(&self, byte_offset: usize) -> Utf16Position {
        self.line_index.utf16_position(&self.text, byte_offset)
    }

    #[must_use]
    pub fn human_range(&self, range: ByteRange) -> HumanRange {
        HumanRange {
            start: self.human_position(range.start as usize),
            end: self.human_position(range.end as usize),
        }
    }

    #[must_use]
    pub fn utf16_range(&self, range: ByteRange) -> Utf16Range {
        Utf16Range {
            start: self.utf16_position(range.start as usize),
            end: self.utf16_position(range.end as usize),
        }
    }

    #[must_use]
    pub fn line_text(&self, one_based_line: u32) -> Option<&str> {
        self.line_index.line_text(&self.text, one_based_line)
    }
}

/// Immutable, deterministically ordered source snapshots shared by a diagnostic batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceSnapshotRegistry {
    sources: BTreeMap<SourceIdentity, Arc<SourceSnapshot>>,
}

impl SourceSnapshotRegistry {
    #[must_use]
    pub fn builder() -> SourceSnapshotRegistryBuilder {
        SourceSnapshotRegistryBuilder::default()
    }

    #[must_use]
    pub fn get(&self, identity: &SourceIdentity) -> Option<&Arc<SourceSnapshot>> {
        self.sources.get(identity)
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&SourceIdentity, &Arc<SourceSnapshot>)> {
        self.sources.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SourceSnapshotRegistryBuilder {
    sources: BTreeMap<SourceIdentity, Arc<SourceSnapshot>>,
}

impl SourceSnapshotRegistryBuilder {
    pub fn insert(
        &mut self,
        identity: SourceIdentity,
        text: impl Into<Arc<str>>,
    ) -> Result<&mut Self, SourceSnapshotRegistryError> {
        if self.sources.contains_key(&identity) {
            return Err(SourceSnapshotRegistryError::Duplicate(identity));
        }
        let snapshot = Arc::new(SourceSnapshot::new(identity.clone(), text.into()));
        self.sources.insert(identity, snapshot);
        Ok(self)
    }

    #[must_use]
    pub fn build(self) -> Arc<SourceSnapshotRegistry> {
        Arc::new(SourceSnapshotRegistry {
            sources: self.sources,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSnapshotRegistryError {
    Duplicate(SourceIdentity),
}

impl fmt::Display for SourceSnapshotRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(identity) => write!(formatter, "duplicate source: {identity}"),
        }
    }
}

impl std::error::Error for SourceSnapshotRegistryError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{ByteRange, HumanPosition, SourceIdentity, SourceSnapshotRegistry, Utf16Position};

    #[test]
    fn positions_are_exact_for_crlf_and_astral_characters() {
        let source = "a😀b\r\n中x\n";
        let identity = SourceIdentity::package("example", "src/main.nexa");
        let mut builder = SourceSnapshotRegistry::builder();
        builder.insert(identity.clone(), source).unwrap();
        let registry = builder.build();
        let snapshot = registry.get(&identity).unwrap();

        assert_eq!(
            snapshot.human_position("a😀".len()),
            HumanPosition { line: 1, column: 3 }
        );
        assert_eq!(
            snapshot.utf16_position("a😀".len()),
            Utf16Position {
                line: 0,
                character: 3
            }
        );
        let cr = source.find('\r').unwrap();
        let lf = cr + 1;
        assert_eq!(
            snapshot.utf16_position(cr),
            Utf16Position {
                line: 0,
                character: 4
            }
        );
        assert_eq!(snapshot.utf16_position(lf), snapshot.utf16_position(cr));
        assert_eq!(
            snapshot.utf16_position(lf + 1),
            Utf16Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(snapshot.line_text(1), Some("a😀b"));
        assert_eq!(
            snapshot.utf16_range(ByteRange::new(1, 5)).end,
            Utf16Position {
                line: 0,
                character: 3
            }
        );
    }

    #[test]
    fn registry_shares_source_snapshots() {
        let identity = SourceIdentity::standalone("main.nexa");
        let mut builder = SourceSnapshotRegistry::builder();
        builder.insert(identity.clone(), "fn main() {}").unwrap();
        let registry = builder.build();
        let first = Arc::clone(registry.get(&identity).unwrap());
        let second = Arc::clone(registry.get(&identity).unwrap());
        assert!(Arc::ptr_eq(&first, &second));
    }
}
