//! Nexa Language v2 typed-IR compiler and single-source virtual-package adapters.
//!
//! Parsing, name resolution, and type checking live in `nexa-syntax` and
//! `nexa-analysis`. This crate only lowers typed package IR to verified bytecode.

mod package;
mod snippet;
mod typed;

use std::fmt;

use nexa_bytecode::{Module, ValueType};
use nexa_core::{FileId, SourceSpan, StableId};
use nexa_diagnostics::{ErrorCode, SourceIdentity};
use nexa_idl::ValidatedContract;
use nexa_verifier::VerifiedModule;

pub use package::{
    PackageCompileOutput, PackageCompiledSource, PackageDebugInfo, PackageFunctionDebugInfo,
    PackageHostImportDebugInfo, PackageMainInfo, PackageModuleDebugInfo, PackagePublicSymbol,
    PackageReplCellInfo, PackageReplStateFieldInfo, PackageStandardLibraryInfo,
    PackageStateFieldInfo, PackageStateTypeInfo, PackageTestCallGraphNode,
    PackageTestForbiddenEffect, PackageTestInfo, PackageTestRejection, PackageVisibility,
    ReplCellCompileOutput, ReplSeedCompileOutput, StandaloneCompileOutput,
};
pub use typed::{
    STANDALONE_MAIN_STABLE_ID, compile_typed_package, compile_typed_repl_cell,
    compile_typed_repl_seed, compile_typed_standalone_package, standalone_main_stable_id,
};

/// Source ownership for one diagnostic label emitted by a virtual snippet.
///
/// The virtual root is represented by [`Self::Caller`] because its package/path identity is an
/// adapter implementation detail; its [`SourceSpan`] uses the caller-provided [`FileId`].
/// Compiler-provided and external sources retain their exact canonical identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisDiagnosticSource {
    Caller,
    Canonical(SourceIdentity),
}

/// One source label preserved from the canonical analyzer when adapting a virtual snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisDiagnosticLabel {
    pub source: AnalysisDiagnosticSource,
    pub span: SourceSpan,
    pub message: String,
}

/// One canonical analyzer diagnostic preserved across the virtual-snippet adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisDiagnostic {
    pub code: ErrorCode,
    pub message: String,
    pub primary: AnalysisDiagnosticLabel,
    pub secondary: Vec<AnalysisDiagnosticLabel>,
    pub related: Vec<AnalysisDiagnosticLabel>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// A canonical analyzer diagnostic carried through the single-source façade.
    AnalysisDiagnostic(Box<AnalysisDiagnostic>),
    DuplicateName {
        name: String,
        first: SourceSpan,
        duplicate: SourceSpan,
    },
    UnknownName {
        name: String,
        span: SourceSpan,
    },
    UnknownType {
        name: String,
        span: SourceSpan,
    },
    TypeMismatch {
        expected: Option<ValueType>,
        actual: Option<ValueType>,
        span: SourceSpan,
    },
    MissingReturn {
        function_span: SourceSpan,
    },
    DeferCaptureLimit {
        span: SourceSpan,
    },
    InvalidEffect {
        span: SourceSpan,
    },
    MissingMain {
        entry_module: String,
        span: SourceSpan,
    },
    InvalidMainSignature {
        message: &'static str,
        span: SourceSpan,
    },
    MissingReplEntrypoint {
        span: SourceSpan,
    },
    InvalidReplEntrypoint {
        message: &'static str,
        span: SourceSpan,
    },
    InvalidReloadMetadata {
        message: &'static str,
        function_span: SourceSpan,
    },
    TooManyRegisters {
        function_span: SourceSpan,
    },
    Verify {
        message: String,
        function_span: SourceSpan,
    },
}

impl CompileError {
    pub(crate) fn duplicate_name(name: String, first: SourceSpan, duplicate: SourceSpan) -> Self {
        Self::DuplicateName {
            name,
            first,
            duplicate,
        }
    }

    pub(crate) fn unknown_name(name: String, span: SourceSpan) -> Self {
        Self::UnknownName { name, span }
    }

    pub(crate) fn unknown_type(name: String, span: SourceSpan) -> Self {
        Self::UnknownType { name, span }
    }

    pub(crate) fn type_mismatch(
        expected: Option<ValueType>,
        actual: Option<ValueType>,
        span: SourceSpan,
    ) -> Self {
        Self::TypeMismatch {
            expected,
            actual,
            span,
        }
    }

    pub(crate) fn missing_return(function_span: SourceSpan) -> Self {
        Self::MissingReturn { function_span }
    }

    pub(crate) fn defer_capture_limit(span: SourceSpan) -> Self {
        Self::DeferCaptureLimit { span }
    }

    pub(crate) fn invalid_effect(span: SourceSpan) -> Self {
        Self::InvalidEffect { span }
    }

    pub(crate) fn missing_main(entry_module: String, span: SourceSpan) -> Self {
        Self::MissingMain { entry_module, span }
    }

    pub(crate) fn invalid_main_signature(message: &'static str, span: SourceSpan) -> Self {
        Self::InvalidMainSignature { message, span }
    }

    pub(crate) fn missing_repl_entrypoint(span: SourceSpan) -> Self {
        Self::MissingReplEntrypoint { span }
    }

    pub(crate) fn invalid_repl_entrypoint(message: &'static str, span: SourceSpan) -> Self {
        Self::InvalidReplEntrypoint { message, span }
    }

    pub(crate) fn invalid_reload_metadata(
        message: &'static str,
        function_span: SourceSpan,
    ) -> Self {
        Self::InvalidReloadMetadata {
            message,
            function_span,
        }
    }

    pub(crate) fn too_many_registers(function_span: SourceSpan) -> Self {
        Self::TooManyRegisters { function_span }
    }

    pub(crate) fn verify(message: String, function_span: SourceSpan) -> Self {
        Self::Verify {
            message,
            function_span,
        }
    }

    #[must_use]
    pub fn source_span(&self) -> Option<SourceSpan> {
        match self {
            Self::AnalysisDiagnostic(diagnostic) => Some(diagnostic.primary.span),
            Self::UnknownName { span, .. }
            | Self::UnknownType { span, .. }
            | Self::TypeMismatch { span, .. }
            | Self::DeferCaptureLimit { span }
            | Self::InvalidEffect { span }
            | Self::MissingMain { span, .. }
            | Self::InvalidMainSignature { span, .. }
            | Self::MissingReplEntrypoint { span }
            | Self::InvalidReplEntrypoint { span, .. } => Some(*span),
            Self::DuplicateName { duplicate, .. } => Some(*duplicate),
            Self::MissingReturn { function_span }
            | Self::InvalidReloadMetadata { function_span, .. }
            | Self::TooManyRegisters { function_span }
            | Self::Verify { function_span, .. } => Some(*function_span),
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnalysisDiagnostic(diagnostic) => {
                write!(formatter, "{}: {}", diagnostic.code, diagnostic.message)
            }
            _ => write!(formatter, "{self:?}"),
        }
    }
}

impl std::error::Error for CompileError {}

/// Compiles one source string through the canonical M4 virtual-package analyzer.
pub fn compile(source: &str) -> Result<VerifiedModule, CompileError> {
    compile_file(source, FileId::default())
}

/// Compiles one source string while preserving the caller's source identity.
pub fn compile_file(source: &str, file: FileId) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, file, None, None, true)
}

/// Compiles one source string and pins its compact Host Contract runtime identity.
pub fn compile_with_contract_id(
    source: &str,
    host_contract_id: StableId,
) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(
        source,
        FileId::default(),
        None,
        Some(host_contract_id),
        true,
    )
}

/// Compiles one source string against a concrete NIDL v2 Contract.
pub fn compile_with_contract(
    source: &str,
    contract: &ValidatedContract,
) -> Result<VerifiedModule, CompileError> {
    compile_with_contract_file(source, FileId::default(), contract)
}

/// Compiles one source string against a host contract and preserves its source identity.
pub fn compile_with_contract_file(
    source: &str,
    file: FileId,
    contract: &ValidatedContract,
) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, file, Some(contract), None, true)
}

/// Compiles one source string through the M5 WP36 reference pipeline: the
/// identical front end, analyzer, and lowering with every emission
/// optimization disabled (Typed IR passes, physical struct inlining).
///
/// The differential gate runs the same program through both pipelines and
/// requires identical results, traps, and task lifecycles; fuel totals are
/// exempt per the cross-pipeline ruling in `BENCHMARK_PROTOCOL_V1.md`.
pub fn compile_reference(source: &str) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, FileId::default(), None, None, false)
}

/// Reference-pipeline variant of [`compile_with_contract`] (M5 WP36).
pub fn compile_reference_with_contract(
    source: &str,
    contract: &ValidatedContract,
) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, FileId::default(), Some(contract), None, false)
}

/// Lowers one source string against a host contract without running the verifier.
pub fn compile_module_with_contract_file(
    source: &str,
    file: FileId,
    contract: &ValidatedContract,
) -> Result<Module, CompileError> {
    snippet::compile_module(source, file, Some(contract), true)
}
