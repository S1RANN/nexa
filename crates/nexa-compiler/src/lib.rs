//! Nexa M4 typed-IR compiler and single-source virtual-package adapters.
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
use nexa_idl::Idl;
use nexa_verifier::VerifiedModule;

pub use package::{
    PackageCompileOutput, PackageCompiledSource, PackageDebugInfo, PackageFunctionDebugInfo,
    PackageHostImportDebugInfo, PackageModuleDebugInfo, PackagePublicSymbol,
    PackageStandardLibraryInfo, PackageStateFieldInfo, PackageStateTypeInfo,
    PackageTestCallGraphNode, PackageTestForbiddenEffect, PackageTestInfo, PackageTestRejection,
    PackageVisibility,
};
pub use typed::compile_typed_package;

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
            | Self::InvalidEffect { span } => Some(*span),
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
    snippet::compile_verified(source, file, None, None)
}

/// Compiles one source string and pins its expected host-interface fingerprint.
pub fn compile_with_metadata(
    source: &str,
    host_hash: StableId,
) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, FileId::default(), None, Some(host_hash))
}

/// Compiles one source string against a concrete host contract.
pub fn compile_with_interface(
    source: &str,
    interface: &Idl,
) -> Result<VerifiedModule, CompileError> {
    compile_with_interface_file(source, FileId::default(), interface)
}

/// Compiles one source string against a host contract and preserves its source identity.
pub fn compile_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
) -> Result<VerifiedModule, CompileError> {
    snippet::compile_verified(source, file, Some(interface), None)
}

/// Lowers one source string against a host contract without running the verifier.
pub fn compile_module_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
) -> Result<Module, CompileError> {
    snippet::compile_module(source, file, Some(interface))
}
