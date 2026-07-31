use std::sync::Arc;

use crate::{Diagnostic, SourceSnapshotRegistry};

/// Hard limits for one analysis revision's diagnostic batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticBatchLimits {
    pub max_diagnostics: usize,
    pub max_total_text_bytes: usize,
    pub max_field_bytes: usize,
}

impl Default for DiagnosticBatchLimits {
    fn default() -> Self {
        Self {
            max_diagnostics: 256,
            max_total_text_bytes: 4 * 1024 * 1024,
            max_field_bytes: 64 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DroppedCounts {
    /// Number of offered diagnostics not retained after deterministic limit selection.
    pub diagnostics: u64,
    /// Text bytes belonging to offered diagnostics not retained by the batch.
    pub text_bytes: u64,
    /// Individual text fields truncated to `max_field_bytes`.
    pub truncated_fields: u64,
}

/// A bounded diagnostic collection with deterministic ordering.
///
/// The retained set is the canonical sorted prefix of all diagnostics offered to the batch. This
/// makes limit behavior independent of worker completion or filesystem enumeration order. Labels
/// and related locations inside each diagnostic retain their semantic order.
#[derive(Clone, Debug)]
pub struct DiagnosticBatch {
    sources: Arc<SourceSnapshotRegistry>,
    limits: DiagnosticBatchLimits,
    diagnostics: Vec<Diagnostic>,
    offered_diagnostics: u64,
    offered_text_bytes: u64,
    retained_text_bytes: usize,
    truncated_fields: u64,
}

impl DiagnosticBatch {
    #[must_use]
    pub fn new(sources: Arc<SourceSnapshotRegistry>, limits: DiagnosticBatchLimits) -> Self {
        Self {
            sources,
            limits,
            diagnostics: Vec::new(),
            offered_diagnostics: 0,
            offered_text_bytes: 0,
            retained_text_bytes: 0,
            truncated_fields: 0,
        }
    }

    #[must_use]
    pub fn with_default_limits(sources: Arc<SourceSnapshotRegistry>) -> Self {
        Self::new(sources, DiagnosticBatchLimits::default())
    }

    pub fn push(&mut self, mut diagnostic: Diagnostic) {
        self.truncated_fields = self
            .truncated_fields
            .saturating_add(diagnostic.truncate_fields(self.limits.max_field_bytes));
        let text_bytes = diagnostic.estimated_text_bytes();
        self.offered_diagnostics = self.offered_diagnostics.saturating_add(1);
        self.offered_text_bytes = self
            .offered_text_bytes
            .saturating_add(u64::try_from(text_bytes).unwrap_or(u64::MAX));
        self.retained_text_bytes = self.retained_text_bytes.saturating_add(text_bytes);
        self.diagnostics.push(diagnostic);
        self.diagnostics.sort_by(Diagnostic::canonical_cmp);
        self.enforce_limits();
    }

    pub fn extend(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        for diagnostic in diagnostics {
            self.push(diagnostic);
        }
    }

    #[must_use]
    pub fn sources(&self) -> &Arc<SourceSnapshotRegistry> {
        &self.sources
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.diagnostics.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    #[must_use]
    pub fn retained_text_bytes(&self) -> usize {
        self.retained_text_bytes
    }

    #[must_use]
    pub fn dropped(&self) -> DroppedCounts {
        DroppedCounts {
            diagnostics: self
                .offered_diagnostics
                .saturating_sub(u64::try_from(self.diagnostics.len()).unwrap_or(u64::MAX)),
            text_bytes: self
                .offered_text_bytes
                .saturating_sub(u64::try_from(self.retained_text_bytes).unwrap_or(u64::MAX)),
            truncated_fields: self.truncated_fields,
        }
    }

    fn enforce_limits(&mut self) {
        while self.diagnostics.len() > self.limits.max_diagnostics
            || self.retained_text_bytes > self.limits.max_total_text_bytes
        {
            let Some(removed) = self.diagnostics.pop() else {
                break;
            };
            self.retained_text_bytes = self
                .retained_text_bytes
                .saturating_sub(removed.estimated_text_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        ByteRange, Diagnostic, ErrorCode, Label, RelatedLocation, Severity, SourceIdentity,
        SourceSnapshotRegistry,
    };

    use super::{DiagnosticBatch, DiagnosticBatchLimits};

    fn diagnostic(code: ErrorCode, path: &str, related: &[&str]) -> Diagnostic {
        let source = SourceIdentity::package("p", Arc::<str>::from(path));
        let mut diagnostic = Diagnostic::new(code, Severity::Error, code.as_str()).with_label(
            Label::primary(source.clone(), ByteRange::new(0, 1), "primary"),
        );
        for message in related {
            diagnostic = diagnostic.with_related(RelatedLocation::new(
                source.clone(),
                ByteRange::new(0, 1),
                Arc::<str>::from(*message),
            ));
        }
        diagnostic
    }

    #[test]
    fn retained_prefix_is_independent_of_insertion_order() {
        let sources = Arc::new(SourceSnapshotRegistry::default());
        let limits = DiagnosticBatchLimits {
            max_diagnostics: 2,
            max_total_text_bytes: usize::MAX,
            max_field_bytes: 1024,
        };
        let values = [
            diagnostic(ErrorCode::NX2101, "c.nexa", &["callee", "caller"]),
            diagnostic(ErrorCode::NX1001, "a.nexa", &["first", "second"]),
            diagnostic(ErrorCode::NX2001, "b.nexa", &["use", "declaration"]),
        ];
        let mut forward = DiagnosticBatch::new(Arc::clone(&sources), limits);
        forward.extend(values.clone());
        let mut reverse = DiagnosticBatch::new(sources, limits);
        reverse.extend(values.into_iter().rev());

        assert_eq!(forward.diagnostics(), reverse.diagnostics());
        assert_eq!(forward.dropped().diagnostics, 1);
        assert_eq!(
            forward.diagnostics()[0]
                .related
                .iter()
                .map(|related| related.message.as_ref())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn field_and_total_text_limits_are_bounded() {
        let limits = DiagnosticBatchLimits {
            max_diagnostics: 10,
            max_total_text_bytes: 12,
            max_field_bytes: 8,
        };
        let mut batch = DiagnosticBatch::new(Arc::new(SourceSnapshotRegistry::default()), limits);
        batch.push(Diagnostic::new(
            ErrorCode::NX1001,
            Severity::Error,
            "0123456789abcdef",
        ));
        batch.push(Diagnostic::new(
            ErrorCode::NX1002,
            Severity::Error,
            "abcdefgh",
        ));

        assert!(batch.retained_text_bytes() <= limits.max_total_text_bytes);
        assert_eq!(batch.dropped().truncated_fields, 1);
        assert!(batch.dropped().diagnostics >= 1);
        assert!(
            batch
                .diagnostics()
                .iter()
                .all(|diagnostic| diagnostic.message.len() <= limits.max_field_bytes)
        );
    }
}
