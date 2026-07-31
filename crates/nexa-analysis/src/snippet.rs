use std::fmt;

use nexa_diagnostics::ByteRange;

use crate::ModulePath;

pub const DEFAULT_SNIPPET_MODULE: &str = "main";

/// The structured reason virtual-snippet module inference failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnippetModuleInferenceErrorKind {
    SourceTooLarge { bytes: usize, limit: usize },
    InvalidModulePath { path: String },
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

/// Infers the semantic module identity for one source-preserving virtual snippet.
///
/// The source is never rewritten. An omitted declaration maps to `main`; an explicit declaration
/// is validated as a canonical [`ModulePath`]. The same byte limit later used by
/// [`crate::SourceSetBuilder`] is accepted explicitly so callers cannot parse a snippet which the
/// package source set would subsequently reject.
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
    let syntax = nexa_syntax::parse_nexa(source).map_err(|error| SnippetModuleInferenceError {
        kind: SnippetModuleInferenceErrorKind::SourceTooLarge {
            bytes: error.bytes,
            limit: u32::MAX as usize,
        },
        range: whole_source_range(source),
        message: error.to_string(),
    })?;
    let ast = nexa_syntax::ast::parse_nexa_ast(&syntax);
    let Some(declaration) = ast.module else {
        return ModulePath::new(DEFAULT_SNIPPET_MODULE).map_err(|error| {
            SnippetModuleInferenceError {
                kind: SnippetModuleInferenceErrorKind::InvalidModulePath {
                    path: DEFAULT_SNIPPET_MODULE.to_owned(),
                },
                range: ByteRange::default(),
                message: error.to_string(),
            }
        });
    };
    let path = declaration.path.text();
    ModulePath::new(path.clone()).map_err(|_| SnippetModuleInferenceError {
        kind: SnippetModuleInferenceErrorKind::InvalidModulePath { path: path.clone() },
        range: ByteRange::new(
            declaration.path.range.start.get(),
            declaration.path.range.end.get(),
        ),
        message: format!("invalid module path `{path}`"),
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
    fn explicit_module_preserves_the_declared_identity() {
        let source = "module game.combat;\nfn main() -> i32 { return 0; }\n";
        let module = infer_snippet_module(source, source.len()).unwrap();
        assert_eq!(module.as_str(), "game.combat");
    }

    #[test]
    fn invalid_module_reports_the_exact_path_bytes_and_text() {
        let source = "module Game.Combat;\nfn main() -> i32 { return 0; }\n";
        let error = infer_snippet_module(source, source.len()).unwrap_err();
        assert_eq!(
            error.kind,
            SnippetModuleInferenceErrorKind::InvalidModulePath {
                path: "Game.Combat".into()
            }
        );
        assert_eq!(error.range, ByteRange::new(7, 18));
        assert_eq!(
            &source[error.range.start as usize..error.range.end as usize],
            "Game.Combat"
        );
        assert_eq!(error.message, "invalid module path `Game.Combat`");
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
