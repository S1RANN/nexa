use std::sync::Arc;

use crate::{ByteRange, ErrorCode, SourceIdentity};

const ELLIPSIS: &str = "…";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Note => "note",
            Self::Help => "help",
        }
    }

    pub(crate) const fn sort_rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
            Self::Note => 2,
            Self::Help => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LabelStyle {
    Primary,
    Secondary,
}

/// A source label. Every label carries its own stable source identity, allowing one diagnostic to
/// describe locations across any number of files or packages.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Label {
    pub style: LabelStyle,
    pub source: SourceIdentity,
    pub range: ByteRange,
    pub message: Arc<str>,
}

impl Label {
    #[must_use]
    pub fn primary(source: SourceIdentity, range: ByteRange, message: impl Into<Arc<str>>) -> Self {
        Self {
            style: LabelStyle::Primary,
            source,
            range,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn secondary(
        source: SourceIdentity,
        range: ByteRange,
        message: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            style: LabelStyle::Secondary,
            source,
            range,
            message: message.into(),
        }
    }
}

/// One semantically ordered related location, such as a call chain or a declaration/use chain.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelatedLocation {
    pub source: SourceIdentity,
    pub range: ByteRange,
    pub message: Arc<str>,
}

impl RelatedLocation {
    #[must_use]
    pub fn new(source: SourceIdentity, range: ByteRange, message: impl Into<Arc<str>>) -> Self {
        Self {
            source,
            range,
            message: message.into(),
        }
    }
}

/// A textual fix with an optional source replacement. M4 renderers expose fixes but never apply
/// them automatically.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextEditSuggestion {
    pub message: Arc<str>,
    pub source: Option<SourceIdentity>,
    pub range: Option<ByteRange>,
    pub replacement: Option<Arc<str>>,
}

impl TextEditSuggestion {
    #[must_use]
    pub fn message(message: impl Into<Arc<str>>) -> Self {
        Self {
            message: message.into(),
            source: None,
            range: None,
            replacement: None,
        }
    }

    #[must_use]
    pub fn replacement(
        message: impl Into<Arc<str>>,
        source: SourceIdentity,
        range: ByteRange,
        replacement: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(source),
            range: Some(range),
            replacement: Some(replacement.into()),
        }
    }
}

/// A source-backed diagnostic independent of compiler, runtime, or editor-specific error types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: ErrorCode,
    pub severity: Severity,
    pub message: Arc<str>,
    pub labels: Vec<Label>,
    /// Order is semantic and is never changed by [`crate::DiagnosticBatch`].
    pub related: Vec<RelatedLocation>,
    pub notes: Vec<Arc<str>>,
    pub fixes: Vec<TextEditSuggestion>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(code: ErrorCode, severity: Severity, message: impl Into<Arc<str>>) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            labels: Vec::new(),
            related: Vec::new(),
            notes: Vec::new(),
            fixes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_label(mut self, label: Label) -> Self {
        self.labels.push(label);
        self
    }

    #[must_use]
    pub fn with_related(mut self, related: RelatedLocation) -> Self {
        self.related.push(related);
        self
    }

    #[must_use]
    pub fn with_note(mut self, note: impl Into<Arc<str>>) -> Self {
        self.notes.push(note.into());
        self
    }

    #[must_use]
    pub fn with_fix(mut self, fix: TextEditSuggestion) -> Self {
        self.fixes.push(fix);
        self
    }

    #[must_use]
    pub fn primary_label(&self) -> Option<&Label> {
        self.labels
            .iter()
            .find(|label| label.style == LabelStyle::Primary)
    }

    pub(crate) fn estimated_text_bytes(&self) -> usize {
        let label_bytes = self
            .labels
            .iter()
            .map(|label| label.message.len())
            .sum::<usize>();
        let related_bytes = self
            .related
            .iter()
            .map(|related| related.message.len())
            .sum::<usize>();
        let notes_bytes = self.notes.iter().map(|note| note.len()).sum::<usize>();
        let fix_bytes = self
            .fixes
            .iter()
            .map(|fix| {
                fix.message.len()
                    + fix
                        .replacement
                        .as_ref()
                        .map_or(0, |replacement| replacement.len())
            })
            .sum::<usize>();
        self.message
            .len()
            .saturating_add(label_bytes)
            .saturating_add(related_bytes)
            .saturating_add(notes_bytes)
            .saturating_add(fix_bytes)
    }

    pub(crate) fn canonical_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.canonical_key().cmp(&other.canonical_key())
    }

    fn canonical_key(&self) -> DiagnosticCanonicalKey<'_> {
        let primary = self.primary_label();
        DiagnosticCanonicalKey {
            severity: self.severity.sort_rank(),
            source: primary.map(|label| &label.source),
            start: primary.map_or(u32::MAX, |label| label.range.start),
            end: primary.map_or(u32::MAX, |label| label.range.end),
            code: self.code,
            message: &self.message,
            labels: &self.labels,
            related: &self.related,
            notes: &self.notes,
            fixes: &self.fixes,
        }
    }

    pub(crate) fn truncate_fields(&mut self, max_bytes: usize) -> u64 {
        let mut truncated = 0;
        truncate_arc(&mut self.message, max_bytes, &mut truncated);
        for label in &mut self.labels {
            truncate_arc(&mut label.message, max_bytes, &mut truncated);
        }
        for related in &mut self.related {
            truncate_arc(&mut related.message, max_bytes, &mut truncated);
        }
        for note in &mut self.notes {
            truncate_arc(note, max_bytes, &mut truncated);
        }
        for fix in &mut self.fixes {
            truncate_arc(&mut fix.message, max_bytes, &mut truncated);
            if let Some(replacement) = &mut fix.replacement {
                truncate_arc(replacement, max_bytes, &mut truncated);
            }
        }
        truncated
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DiagnosticCanonicalKey<'a> {
    severity: u8,
    source: Option<&'a SourceIdentity>,
    start: u32,
    end: u32,
    code: ErrorCode,
    message: &'a str,
    labels: &'a [Label],
    related: &'a [RelatedLocation],
    notes: &'a [Arc<str>],
    fixes: &'a [TextEditSuggestion],
}

fn truncate_arc(value: &mut Arc<str>, max_bytes: usize, truncated: &mut u64) {
    if value.len() <= max_bytes {
        return;
    }
    let append_ellipsis = max_bytes >= ELLIPSIS.len();
    let mut end = max_bytes.saturating_sub(if append_ellipsis { ELLIPSIS.len() } else { 0 });
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if append_ellipsis { ELLIPSIS } else { "" };
    *value = Arc::from(format!("{}{suffix}", &value[..end]));
    *truncated = truncated.saturating_add(1);
}
