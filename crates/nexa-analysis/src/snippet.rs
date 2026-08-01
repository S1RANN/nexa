use std::fmt;

use nexa_diagnostics::ByteRange;

use crate::ModulePath;

pub const DEFAULT_SNIPPET_MODULE: &str = "main";

/// The structured reason virtual-snippet module inference failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnippetModuleInferenceErrorKind {
    SourceTooLarge { bytes: usize, limit: usize },
}

/// A source-backed virtual-snippet module inference failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetModuleInferenceError {
    pub kind: SnippetModuleInferenceErrorKind,
    pub range: ByteRange,
    pub message: String,
}

impl fmt::Display for SnippetModuleInferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SnippetModuleInferenceError {}

/// Returns the compiler-owned module identity for one source-preserving virtual snippet.
///
/// Language v2 has no source-level module declaration. The source is never rewritten and always
/// maps to `main`; the same byte limit later used by [`crate::SourceSetBuilder`] is accepted
/// explicitly so callers cannot scan a snippet which the package source set would reject.
pub fn infer_snippet_module(
    source: &str,
    source_file_bytes: usize,
) -> Result<ModulePath, SnippetModuleInferenceError> {
    if source.len() > source_file_bytes {
        return Err(SnippetModuleInferenceError {
            kind: SnippetModuleInferenceErrorKind::SourceTooLarge {
                bytes: source.len(),
                limit: source_file_bytes,
            },
            range: whole_source_range(source),
            message: format!(
                "source is {} bytes, exceeding the virtual-snippet limit of {source_file_bytes} bytes",
                source.len()
            ),
        });
    }
    ModulePath::new(DEFAULT_SNIPPET_MODULE).map_err(|error| SnippetModuleInferenceError {
        kind: SnippetModuleInferenceErrorKind::SourceTooLarge {
            bytes: source.len(),
            limit: source_file_bytes,
        },
        range: whole_source_range(source),
        message: error.to_string(),
    })
}

fn whole_source_range(source: &str) -> ByteRange {
    ByteRange::new(0, u32::try_from(source.len()).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use nexa_diagnostics::ByteRange;

    use super::{SnippetModuleInferenceErrorKind, infer_snippet_module};

    #[test]
    fn omitted_module_uses_main_without_rewriting_source() {
        let source = "fn main() -> i32 { return 0; }\r\n";
        let module = infer_snippet_module(source, source.len()).unwrap();
        assert_eq!(module.as_str(), "main");
    }

    #[test]
    fn source_limit_accepts_the_boundary_and_rejects_the_next_byte() {
        let source = "//x\n";
        assert_eq!(
            infer_snippet_module(source, source.len()).unwrap().as_str(),
            "main"
        );

        let error = infer_snippet_module(source, source.len() - 1).unwrap_err();
        assert_eq!(
            error.kind,
            SnippetModuleInferenceErrorKind::SourceTooLarge {
                bytes: source.len(),
                limit: source.len() - 1,
            }
        );
        assert_eq!(
            error.range,
            ByteRange::new(0, u32::try_from(source.len()).unwrap())
        );
        assert!(
            error
                .message
                .contains("exceeding the virtual-snippet limit")
        );
    }
}
