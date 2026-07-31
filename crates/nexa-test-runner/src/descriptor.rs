use std::cmp::Ordering;
use std::fmt;

/// A half-open byte range in a package source.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceSpan {
    /// Stable package-relative source path or source URI.
    pub source: String,
    /// Inclusive byte offset.
    pub start: u32,
    /// Exclusive byte offset.
    pub end: u32,
}

impl SourceSpan {
    #[must_use]
    pub fn new(source: impl Into<String>, start: u32, end: u32) -> Self {
        Self {
            source: source.into(),
            start,
            end,
        }
    }

    #[must_use]
    pub const fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

impl fmt::Display for SourceSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}..{}", self.source, self.start, self.end)
    }
}

/// A compiled function discovered as a package test.
///
/// The function identity is generic so the compiler or facade can retain its
/// native compiled-function handle without this crate depending on it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestDescriptor<F> {
    pub package: String,
    pub module: String,
    pub name: String,
    pub span: SourceSpan,
    pub function: F,
}

impl<F> TestDescriptor<F> {
    #[must_use]
    pub fn new(
        package: impl Into<String>,
        module: impl Into<String>,
        name: impl Into<String>,
        span: SourceSpan,
        function: F,
    ) -> Self {
        Self {
            package: package.into(),
            module: module.into(),
            name: name.into(),
            span,
            function,
        }
    }

    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}::{}::{}", self.package, self.module, self.name)
    }
}

pub(crate) fn compare_descriptors<A, B>(
    left: &TestDescriptor<A>,
    right: &TestDescriptor<B>,
) -> Ordering {
    (
        left.package.as_str(),
        left.module.as_str(),
        left.name.as_str(),
        &left.span,
    )
        .cmp(&(
            right.package.as_str(),
            right.module.as_str(),
            right.name.as_str(),
            &right.span,
        ))
}
