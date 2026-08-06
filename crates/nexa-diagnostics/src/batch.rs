use std::sync::Arc;

use crate::{
    Diagnostic, ErrorCode, Label, RelatedLocation, Severity, SourceSnapshotRegistry,
    TextEditSuggestion,
};

/// Borrowed identity of a diagnostic for deduplication.
///
/// Two diagnostics are duplicates only when every user-visible field matches: code, severity,
/// message, all labels (primary and secondary), related locations, notes, and fixes. The key
/// borrows from the diagnostic, so deduplication remains a linear scan of cheap slice comparisons;
/// slices compare element-wise and only after the cheaper scalar fields (code, severity, message)
/// already match.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct DiagnosticDedupKey<'a> {
    code: ErrorCode,
    severity: Severity,
    message: &'a str,
    labels: &'a [Label],
    related: &'a [RelatedLocation],
    notes: &'a [Arc<str>],
    fixes: &'a [TextEditSuggestion],
}

fn diagnostic_dedup_key(diagnostic: &Diagnostic) -> DiagnosticDedupKey<'_> {
    DiagnosticDedupKey {
        code: diagnostic.code,
        severity: diagnostic.severity,
        message: diagnostic.message.as_ref(),
        labels: &diagnostic.labels,
        related: &diagnostic.related,
        notes: &diagnostic.notes,
        fixes: &diagnostic.fixes,
    }
}

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
    /// Individual text fields truncated to `max_field_bytes`, counted only for diagnostics
    /// retained in the batch.
    pub truncated_fields: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SuppressedCounts {
    /// Diagnostics not emitted because a prior error already explains them (cascades, duplicates).
    pub diagnostics: u64,
    /// Cause recorded for the first suppressed diagnostic.
    pub first_cause: Option<Arc<str>>,
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
    /// Parallel to `diagnostics`: how many fields of each retained diagnostic were truncated by
    /// `max_field_bytes`. Kept aligned on sort and pop so that dropping a diagnostic also removes
    /// its truncation count from `truncated_fields`.
    truncated_field_counts: Vec<u64>,
    offered_diagnostics: u64,
    offered_text_bytes: u64,
    retained_text_bytes: usize,
    truncated_fields: u64,
    suppressed: SuppressedCounts,
}

impl DiagnosticBatch {
    #[must_use]
    pub fn new(sources: Arc<SourceSnapshotRegistry>, limits: DiagnosticBatchLimits) -> Self {
        Self {
            sources,
            limits,
            diagnostics: Vec::new(),
            truncated_field_counts: Vec::new(),
            offered_diagnostics: 0,
            offered_text_bytes: 0,
            retained_text_bytes: 0,
            truncated_fields: 0,
            suppressed: SuppressedCounts::default(),
        }
    }

    #[must_use]
    pub fn with_default_limits(sources: Arc<SourceSnapshotRegistry>) -> Self {
        Self::new(sources, DiagnosticBatchLimits::default())
    }

    pub fn push(&mut self, mut diagnostic: Diagnostic) {
        if self.is_duplicate(&diagnostic) {
            self.record_suppressed("duplicate diagnostic");
            return;
        }
        let truncated = diagnostic.truncate_fields(self.limits.max_field_bytes);
        self.truncated_fields = self.truncated_fields.saturating_add(truncated);
        let text_bytes = diagnostic.estimated_text_bytes();
        self.offered_diagnostics = self.offered_diagnostics.saturating_add(1);
        self.offered_text_bytes = self
            .offered_text_bytes
            .saturating_add(u64::try_from(text_bytes).unwrap_or(u64::MAX));
        self.retained_text_bytes = self.retained_text_bytes.saturating_add(text_bytes);
        self.diagnostics.push(diagnostic);
        self.truncated_field_counts.push(truncated);
        self.sort();
        self.enforce_limits();
    }

    /// Records one diagnostic suppressed by cascade containment or deduplication.
    pub fn record_suppressed(&mut self, cause: impl Into<Arc<str>>) {
        self.suppressed.diagnostics = self.suppressed.diagnostics.saturating_add(1);
        if self.suppressed.first_cause.is_none() {
            self.suppressed.first_cause = Some(cause.into());
        }
    }

    #[must_use]
    pub fn suppressed(&self) -> &SuppressedCounts {
        &self.suppressed
    }

    /// Copies another batch's suppressed counts into this one, keeping the first cause.
    pub fn inherit_suppressed(&mut self, other: &Self) {
        self.suppressed.diagnostics = self
            .suppressed
            .diagnostics
            .saturating_add(other.suppressed.diagnostics);
        if self.suppressed.first_cause.is_none() {
            self.suppressed
                .first_cause
                .clone_from(&other.suppressed.first_cause);
        }
    }

    /// Appends a note to the first diagnostic matching the predicate. Returns whether one matched.
    pub fn push_note_to_first(
        &mut self,
        predicate: impl Fn(&Diagnostic) -> bool,
        note: impl Into<Arc<str>>,
    ) -> bool {
        let Some(diagnostic) = self
            .diagnostics
            .iter_mut()
            .find(|diagnostic| predicate(diagnostic))
        else {
            return false;
        };
        diagnostic.notes.push(note.into());
        true
    }

    fn is_duplicate(&self, diagnostic: &Diagnostic) -> bool {
        let key = diagnostic_dedup_key(diagnostic);
        self.diagnostics
            .iter()
            .any(|existing| diagnostic_dedup_key(existing) == key)
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

    /// Deterministic canonical ordering that keeps `truncated_field_counts` aligned with
    /// `diagnostics`: both vectors receive the same permutation.
    fn sort(&mut self) {
        // Compute the permutation that orders diagnostics canonically, then apply it to the
        // parallel count vector. The diagnostics themselves are sorted in place, which avoids
        // cloning them to apply the permutation.
        let mut order: Vec<usize> = (0..self.diagnostics.len()).collect();
        order.sort_by(|&a, &b| self.diagnostics[a].canonical_cmp(&self.diagnostics[b]));
        if order
            .iter()
            .enumerate()
            .any(|(index, &position)| index != position)
        {
            self.truncated_field_counts = order
                .iter()
                .map(|&position| self.truncated_field_counts[position])
                .collect();
        }
        self.diagnostics.sort_by(Diagnostic::canonical_cmp);
    }

    fn enforce_limits(&mut self) {
        while self.diagnostics.len() > self.limits.max_diagnostics
            || self.retained_text_bytes > self.limits.max_total_text_bytes
        {
            let Some(removed) = self.diagnostics.pop() else {
                break;
            };
            let removed_truncations = self.truncated_field_counts.pop().unwrap_or(0);
            self.retained_text_bytes = self
                .retained_text_bytes
                .saturating_sub(removed.estimated_text_bytes());
            // A dropped diagnostic's truncations no longer describe the retained batch.
            self.truncated_fields = self.truncated_fields.saturating_sub(removed_truncations);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        ByteRange, Diagnostic, ErrorCode, Label, RelatedLocation, Severity, SourceIdentity,
        SourceSnapshotRegistry, TextEditSuggestion,
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

    #[test]
    fn push_deduplicates_identical_code_primary_range_and_message() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]));
        batch.push(Diagnostic::new(ErrorCode::NX2101, Severity::Error, "other"));

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 1);
        assert_eq!(
            batch.suppressed().first_cause.as_deref(),
            Some("duplicate diagnostic")
        );
    }

    #[test]
    fn dedup_keeps_diagnostics_differing_in_secondary_labels() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]));
        let with_secondary =
            diagnostic(ErrorCode::NX2101, "a.nexa", &[]).with_label(Label::secondary(
                SourceIdentity::package("p", Arc::<str>::from("b.nexa")),
                ByteRange::new(4, 8),
                "unused variable",
            ));
        batch.push(with_secondary);

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 0);
    }

    #[test]
    fn dedup_keeps_diagnostics_differing_in_notes() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]).with_note("first use at 1:1"));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]).with_note("second use at 2:3"));

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 0);
    }

    #[test]
    fn dedup_keeps_diagnostics_differing_in_related_locations() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &["callee"]));
        batch.push(diagnostic(
            ErrorCode::NX2101,
            "a.nexa",
            &["callee", "caller"],
        ));

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 0);
    }

    #[test]
    fn dedup_keeps_diagnostics_differing_in_fixes() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(
            diagnostic(ErrorCode::NX2101, "a.nexa", &[])
                .with_fix(TextEditSuggestion::message("use `let`")),
        );
        batch.push(
            diagnostic(ErrorCode::NX2101, "a.nexa", &[])
                .with_fix(TextEditSuggestion::message("add `mut`")),
        );

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 0);
    }

    #[test]
    fn dedup_suppresses_identical_full_identity_including_secondary_fields() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        let build = || {
            let source_a = SourceIdentity::package("p", Arc::<str>::from("a.nexa"));
            let source_b = SourceIdentity::package("p", Arc::<str>::from("b.nexa"));
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "mismatched types")
                .with_label(Label::primary(
                    source_a.clone(),
                    ByteRange::new(0, 1),
                    "primary",
                ))
                .with_label(Label::secondary(
                    source_b,
                    ByteRange::new(4, 8),
                    "unused variable",
                ))
                .with_related(RelatedLocation::new(
                    source_a.clone(),
                    ByteRange::new(2, 3),
                    "defined here",
                ))
                .with_note("note text")
                .with_fix(TextEditSuggestion::message("apply fix"))
        };
        batch.push(build());
        batch.push(build());

        assert_eq!(batch.diagnostics().len(), 1);
        assert_eq!(batch.suppressed().diagnostics, 1);
    }

    #[test]
    fn dedup_keeps_diagnostics_differing_in_severity() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2101, "a.nexa", &[]));
        let warning = Diagnostic::new(ErrorCode::NX2101, Severity::Warning, "NX2101").with_label(
            Label::primary(
                SourceIdentity::package("p", Arc::<str>::from("a.nexa")),
                ByteRange::new(0, 1),
                "primary",
            ),
        );
        batch.push(warning);

        assert_eq!(batch.diagnostics().len(), 2);
        assert_eq!(batch.suppressed().diagnostics, 0);
    }

    #[test]
    fn truncated_fields_exclude_diagnostics_dropped_by_total_text_limit() {
        let limits = DiagnosticBatchLimits {
            max_diagnostics: 10,
            max_total_text_bytes: 8,
            max_field_bytes: 8,
        };
        let mut batch = DiagnosticBatch::new(Arc::new(SourceSnapshotRegistry::default()), limits);
        // NX2001 sorts after NX1001, so it is dropped when the text budget overflows.
        batch.push(Diagnostic::new(
            ErrorCode::NX2001,
            Severity::Error,
            "0123456789abcdef",
        ));
        batch.push(Diagnostic::new(ErrorCode::NX1001, Severity::Error, "x"));

        assert_eq!(batch.diagnostics().len(), 1);
        assert_eq!(batch.diagnostics()[0].code, ErrorCode::NX1001);
        assert_eq!(batch.dropped().diagnostics, 1);
        // The truncated diagnostic was dropped; its truncation must not be counted.
        assert_eq!(batch.dropped().truncated_fields, 0);
    }

    #[test]
    fn truncated_fields_exclude_diagnostics_dropped_by_diagnostic_limit() {
        let limits = DiagnosticBatchLimits {
            max_diagnostics: 1,
            max_total_text_bytes: usize::MAX,
            max_field_bytes: 8,
        };
        let mut batch = DiagnosticBatch::new(Arc::new(SourceSnapshotRegistry::default()), limits);
        batch.push(Diagnostic::new(
            ErrorCode::NX2001,
            Severity::Error,
            "0123456789abcdef",
        ));
        batch.push(Diagnostic::new(ErrorCode::NX1001, Severity::Error, "x"));

        assert_eq!(batch.diagnostics().len(), 1);
        assert_eq!(batch.diagnostics()[0].code, ErrorCode::NX1001);
        assert_eq!(batch.dropped().diagnostics, 1);
        assert_eq!(batch.dropped().truncated_fields, 0);
    }

    #[test]
    fn record_suppressed_keeps_the_first_cause() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.record_suppressed("caused by unknown type `u32`");
        batch.record_suppressed("duplicate diagnostic");

        assert_eq!(batch.suppressed().diagnostics, 2);
        assert_eq!(
            batch.suppressed().first_cause.as_deref(),
            Some("caused by unknown type `u32`")
        );
    }

    #[test]
    fn push_note_to_first_appends_only_to_the_matching_diagnostic() {
        let mut batch =
            DiagnosticBatch::with_default_limits(Arc::new(SourceSnapshotRegistry::default()));
        batch.push(diagnostic(ErrorCode::NX2002, "a.nexa", &[]));
        batch.push(Diagnostic::new(ErrorCode::NX2101, Severity::Error, "other"));

        assert!(!batch.push_note_to_first(
            |diagnostic| diagnostic.code == ErrorCode::NX1001,
            "no match",
        ));
        assert!(batch.push_note_to_first(
            |diagnostic| diagnostic.code == ErrorCode::NX2002,
            "1 more use at 3:4",
        ));
        let diagnostic = batch
            .diagnostics()
            .iter()
            .find(|diagnostic| diagnostic.code == ErrorCode::NX2002)
            .expect("NX2002 retained");
        assert_eq!(diagnostic.notes.len(), 1);
        assert_eq!(diagnostic.notes[0].as_ref(), "1 more use at 3:4");
    }
}
