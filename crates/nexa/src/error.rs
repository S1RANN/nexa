use std::fmt;

use nexa_bytecode::DecodeError;
use nexa_compiler::{
    AnalysisDiagnostic as CompilerAnalysisDiagnostic, AnalysisDiagnosticSource, CompileError,
};
use nexa_core::{FileId, ModuleId, RawHandle, SourceSpan};
use nexa_diagnostics::{
    ByteRange, Diagnostic as LeafDiagnostic, Label as LeafLabel, SourceIdentity,
};
pub use nexa_diagnostics::{ERROR_CODE_TABLE, ErrorCode, ErrorCodeDefinition, Severity};
use nexa_runtime::{
    HostCompletionProtocolError, HostRequestError, HostTrap, InterpreterError, MigrationLimitError,
    RealmError, ReloadError, RuntimeError, RuntimeHostCloseError, RuntimeMessage, ScopeError,
    StatefulError, TaskError, Trap,
};
use nexa_verifier::{VerifyError, VerifyErrorKind};
use serde::Serialize;

pub type DiagnosticCode = ErrorCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorEmissionDefinition {
    pub code: ErrorCode,
    pub module: &'static str,
    pub variant: &'static str,
    pub test: &'static str,
    pub fixture: &'static str,
}

impl ErrorEmissionDefinition {
    const fn new(
        code: ErrorCode,
        module: &'static str,
        variant: &'static str,
        fixture: &'static str,
    ) -> Self {
        Self {
            code,
            module,
            variant,
            test: "cli::diagnostic_corpus_check",
            fixture,
        }
    }
}

macro_rules! emission {
    ($code:ident, $module:literal, $variant:literal, $ext:literal) => {
        ErrorEmissionDefinition::new(
            ErrorCode::$code,
            $module,
            $variant,
            concat!("fixtures/diagnostics/cases/", stringify!($code), ".json"),
        )
    };
}

pub static ERROR_EMISSION_TABLE: &[ErrorEmissionDefinition] = &[
    emission!(
        NX1001,
        "nexa-syntax::lexer",
        "SyntaxErrorKind::UnexpectedCharacter",
        ".nexa"
    ),
    emission!(
        NX1002,
        "nexa-syntax::tree",
        "SyntaxErrorKind::UnmatchedDelimiter",
        ".nexa"
    ),
    emission!(
        NX2001,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2001",
        ".nexa"
    ),
    emission!(
        NX2002,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2002",
        ".nexa"
    ),
    emission!(
        NX2101,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2101",
        ".nexa"
    ),
    emission!(
        NX2201,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2201",
        ".nexa"
    ),
    emission!(
        NX2202,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2202",
        ".nexa"
    ),
    emission!(
        NX2210,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2210",
        ".nexa"
    ),
    emission!(
        NX2220,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2220",
        ".nexa"
    ),
    emission!(
        NX2221,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2221",
        ".nexa"
    ),
    emission!(
        NX2301,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2301",
        ".nexa"
    ),
    emission!(
        NX2302,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2302",
        ".nexa"
    ),
    emission!(
        NX2401,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2401",
        ".nexa"
    ),
    emission!(
        NX2501,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2501",
        ".nexa"
    ),
    emission!(
        NX2601,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2601",
        ".nexa"
    ),
    emission!(
        NX2602,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2602",
        ".nexa"
    ),
    emission!(
        NX2603,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2603",
        ".nexa"
    ),
    emission!(
        NX2604,
        "nexa-analysis::analyzer",
        "ErrorCode::NX2604",
        ".nexa"
    ),
    emission!(
        NX2701,
        "nexa-analysis::module_graph",
        "ModulePathMismatch",
        ".analysis"
    ),
    emission!(
        NX2702,
        "nexa-analysis::module_graph",
        "ModuleCycle",
        ".analysis"
    ),
    emission!(
        NX2703,
        "nexa-analysis::resolver",
        "UnknownUsePath",
        ".analysis"
    ),
    emission!(
        NX2704,
        "nexa-analysis::resolver",
        "DuplicateOrAmbiguousNamespace",
        ".analysis"
    ),
    emission!(
        NX2705,
        "nexa-analysis::visibility",
        "PrivateAccess",
        ".analysis"
    ),
    emission!(
        NX2706,
        "nexa-analysis::visibility",
        "InvalidPublicApiExposure",
        ".analysis"
    ),
    emission!(
        NX2710,
        "nexa-analysis::stable_identity",
        "InvalidStableAttribute",
        ".analysis"
    ),
    emission!(
        NX2711,
        "nexa-analysis::stable_identity",
        "StableIdentityConflict",
        ".analysis"
    ),
    emission!(
        NX2720,
        "nexa-analysis::const_eval",
        "InvalidConstExpression",
        ".analysis"
    ),
    emission!(
        NX2730,
        "nexa-analysis::package_test",
        "InvalidPackageTest",
        ".analysis"
    ),
    emission!(
        NX2740,
        "nexa-analysis::lifecycle",
        "InvalidLifecycleEntrypointLocation",
        ".analysis"
    ),
    emission!(NX3001, "nexa-bytecode::decode", "InvalidMagic", ".bin"),
    emission!(NX3002, "nexa-verifier", "RegisterOutOfRange", ".bin"),
    emission!(NX3003, "nexa-verifier", "InvalidRootMap", ".bin"),
    emission!(NX3004, "nexa-verifier", "InvalidSourceMap", ".bin"),
    emission!(
        NX4001,
        "nexa-runtime::realm",
        "HostContractIdMismatch",
        ".runtime"
    ),
    emission!(
        NX4002,
        "nexa-runtime::realm",
        "HostCapabilitiesUnavailable",
        ".runtime"
    ),
    emission!(NX4003, "nexa-runtime::host", "Arity", ".runtime"),
    emission!(NX5001, "nexa-runtime::host", "Panicked", ".runtime"),
    emission!(NX5002, "nexa-runtime::host", "Abandoned", ".runtime"),
    emission!(
        NX5003,
        "nexa-runtime::host",
        "UnknownHostErrorCode",
        ".runtime"
    ),
    emission!(NX5004, "nexa-runtime::kernel", "ResourceLimit", ".runtime"),
    emission!(
        NX6001,
        "nexa-runtime::migration",
        "MigrationLimit",
        ".runtime"
    ),
    emission!(NX6002, "nexa-runtime::stateful", "GraphFailure", ".runtime"),
    emission!(
        NX6003,
        "nexa-runtime::realm",
        "ActivationFailure",
        ".runtime"
    ),
    emission!(NX6005, "nexa-verifier", "InvalidReloadMetadata", ".bin"),
    emission!(
        NX7001,
        "nexa-embed::source",
        "PackageSourceFailure",
        ".engine"
    ),
    emission!(NX7002, "nexa-embed::manifest", "InvalidManifest", ".engine"),
    emission!(NX7003, "nexa-embed::policy", "PolicyRejected", ".engine"),
    emission!(NX7004, "nexa-embed::entitlement", "Unavailable", ".engine"),
    emission!(
        NX7010,
        "nexa-embed::entrypoint",
        "MissingRequired",
        ".engine"
    ),
    emission!(
        NX7011,
        "nexa-embed::entrypoint",
        "SignatureMismatch",
        ".engine"
    ),
    emission!(NX7101, "nexa-embed::handler", "Yielded", ".engine"),
    emission!(NX7102, "nexa-embed::handler", "Waited", ".engine"),
    emission!(NX7103, "nexa-embed::handler", "Trapped", ".engine"),
    emission!(NX7201, "nexa-embed::reload", "RolledBack", ".engine"),
    emission!(NX7202, "nexa-embed::reload", "ActivationFaulted", ".engine"),
    emission!(
        NX7302,
        "nexa-embed::persistence",
        "PersistenceFailed",
        ".engine"
    ),
    emission!(
        NX7303,
        "nexa-embed::shutdown",
        "ShutdownIncomplete",
        ".engine"
    ),
];

/// The stable top-level class of an error crossing the Nexa facade.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    Diagnostic,
    Decode,
    Verify,
    Runtime,
    Host,
    Reload,
    Migration,
}

impl ErrorCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostic => "diagnostic",
            Self::Decode => "decode",
            Self::Verify => "verify",
            Self::Runtime => "runtime",
            Self::Host => "host",
            Self::Reload => "reload",
            Self::Migration => "migration",
        }
    }
}

/// Module identity associated with an error when execution has entered a loaded epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorModuleEpoch {
    pub module: ModuleId,
    pub epoch: u64,
}

/// Structured context shared by every public error class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ErrorContext {
    pub span: Option<SourceSpan>,
    pub module_epoch: Option<ErrorModuleEpoch>,
    pub task: Option<RawHandle>,
}

/// Stable metadata available without formatting or parsing an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorMetadata {
    pub code: ErrorCode,
    pub category: ErrorCategory,
    pub context: ErrorContext,
}

/// Implemented for all error classes admitted by [`NexaError`].
pub trait ClassifiedError {
    fn metadata(&self) -> ErrorMetadata;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: SourceSpan,
    pub message: RuntimeMessage,
}

/// The compiler stage which produced a facade diagnostic.
///
/// This is carried separately from the stable error code so hosts never need to infer a phase
/// from the textual shape of a code such as `NX2xxx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DiagnosticPhase {
    Lex,
    Parse,
    Resolve,
    TypeCheck,
    Lower,
    Verify,
}

/// One source-backed diagnostic representation shared by human and JSON renderers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub message: RuntimeMessage,
    pub primary: Option<Label>,
    pub secondary: Vec<Label>,
    pub notes: Vec<RuntimeMessage>,
    primary_source: Option<SourceIdentity>,
    secondary_sources: Vec<Option<SourceIdentity>>,
    phase: Option<DiagnosticPhase>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(error: &CompileError, _file: FileId) -> Self {
        if let CompileError::AnalysisDiagnostic(diagnostic) = error {
            return Self::from_analysis(diagnostic);
        }
        let message = RuntimeMessage::inline(&CompileErrorMessage(error).to_string());
        let mut diagnostic = Self {
            code: compile_error_code(error),
            severity: Severity::Error,
            message,
            primary: error.source_span().map(|span| Label {
                span,
                message: RuntimeMessage::Static("primary source location"),
            }),
            secondary: Vec::new(),
            notes: Vec::new(),
            primary_source: None,
            secondary_sources: Vec::new(),
            phase: Some(compile_error_phase(error)),
        };
        match error {
            CompileError::DuplicateName { first, .. } => {
                diagnostic.secondary.push(Label {
                    span: *first,
                    message: RuntimeMessage::Static("first declaration"),
                });
                diagnostic.secondary_sources.push(None);
            }
            CompileError::TypeMismatch {
                expected, actual, ..
            } => {
                if let Some(expected) = expected {
                    diagnostic.notes.push(RuntimeMessage::inline(&format!(
                        "expected type: `{expected}`"
                    )));
                }
                if let Some(actual) = actual {
                    diagnostic
                        .notes
                        .push(RuntimeMessage::inline(&format!("actual type: `{actual}`")));
                }
            }
            _ => {}
        }
        diagnostic
    }

    #[must_use]
    fn from_analysis(diagnostic: &CompilerAnalysisDiagnostic) -> Self {
        let message = diagnostic.code.definition().map_or_else(
            || diagnostic.message.clone(),
            |definition| format!("{}: {}", definition.summary, diagnostic.message),
        );
        let secondary = diagnostic
            .secondary
            .iter()
            .chain(&diagnostic.related)
            .map(|label| Label {
                span: label.span,
                message: RuntimeMessage::inline(&label.message),
            })
            .collect::<Vec<_>>();
        let secondary_sources = diagnostic
            .secondary
            .iter()
            .chain(&diagnostic.related)
            .map(|label| canonical_analysis_source(&label.source))
            .collect();
        Self {
            code: diagnostic.code,
            severity: Severity::Error,
            message: RuntimeMessage::inline(&message),
            primary: Some(Label {
                span: diagnostic.primary.span,
                message: RuntimeMessage::inline(&diagnostic.primary.message),
            }),
            secondary,
            notes: diagnostic
                .notes
                .iter()
                .map(|note| RuntimeMessage::inline(note))
                .collect(),
            primary_source: canonical_analysis_source(&diagnostic.primary.source),
            secondary_sources,
            phase: Some(analysis_diagnostic_phase(diagnostic.code)),
        }
    }

    #[must_use]
    pub fn from_parts(
        code: DiagnosticCode,
        severity: Severity,
        message: RuntimeMessage,
        primary: Label,
    ) -> Self {
        Self {
            code,
            severity,
            message,
            primary: Some(primary),
            secondary: Vec::new(),
            notes: Vec::new(),
            primary_source: None,
            secondary_sources: Vec::new(),
            phase: None,
        }
    }

    #[must_use]
    pub fn without_source(
        code: DiagnosticCode,
        severity: Severity,
        message: RuntimeMessage,
    ) -> Self {
        Self {
            code,
            severity,
            message,
            primary: None,
            secondary: Vec::new(),
            notes: Vec::new(),
            primary_source: None,
            secondary_sources: Vec::new(),
            phase: None,
        }
    }

    /// Returns the compiler phase attached by [`Diagnostic::new`].
    #[must_use]
    pub const fn phase(&self) -> Option<DiagnosticPhase> {
        self.phase
    }

    /// Returns the canonical source identity carried by an external/compiler-provided primary
    /// label. Caller-owned virtual-snippet labels deliberately return `None` and retain the
    /// caller's numeric [`FileId`] instead.
    #[must_use]
    pub const fn primary_source_identity(&self) -> Option<&SourceIdentity> {
        self.primary_source.as_ref()
    }

    /// Returns the canonical source identity carried by one external/compiler-provided secondary
    /// label.
    #[must_use]
    pub fn secondary_source_identity(&self, index: usize) -> Option<&SourceIdentity> {
        self.secondary_sources.get(index).and_then(Option::as_ref)
    }

    /// Converts the compiler facade into the shared leaf diagnostic representation.
    ///
    /// Numeric `FileId`s are revision-local, so callers with a source registry should prefer
    /// [`Diagnostic::to_leaf_with_source_identities`].
    #[must_use]
    pub fn to_leaf(&self) -> LeafDiagnostic {
        self.to_leaf_with_source_identities(|file| {
            SourceIdentity::standalone(format!("<file:{}>", file.0))
        })
    }

    /// Converts the compiler facade into a leaf diagnostic while preserving the independent
    /// source identity of every primary and secondary label.
    #[must_use]
    pub fn to_leaf_with_source_identities(
        &self,
        mut source_identity: impl FnMut(FileId) -> SourceIdentity,
    ) -> LeafDiagnostic {
        let mut leaf = LeafDiagnostic::new(self.code, self.severity, self.message.to_string());
        if let Some(primary) = &self.primary {
            leaf.labels.push(LeafLabel::primary(
                self.primary_source
                    .clone()
                    .unwrap_or_else(|| source_identity(primary.span.file)),
                ByteRange::new(primary.span.start, primary.span.end),
                primary.message.to_string(),
            ));
        }
        leaf.labels
            .extend(self.secondary.iter().enumerate().map(|(index, secondary)| {
                LeafLabel::secondary(
                    self.secondary_sources
                        .get(index)
                        .and_then(Clone::clone)
                        .unwrap_or_else(|| source_identity(secondary.span.file)),
                    ByteRange::new(secondary.span.start, secondary.span.end),
                    secondary.message.to_string(),
                )
            }));
        leaf.notes
            .extend(self.notes.iter().map(|note| note.to_string().into()));
        leaf
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&DiagnosticOutput::from(self))
    }
}

fn canonical_analysis_source(source: &AnalysisDiagnosticSource) -> Option<SourceIdentity> {
    match source {
        AnalysisDiagnosticSource::Caller => None,
        AnalysisDiagnosticSource::Canonical(identity) => Some(identity.clone()),
    }
}

impl From<&Diagnostic> for LeafDiagnostic {
    fn from(diagnostic: &Diagnostic) -> Self {
        diagnostic.to_leaf()
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "{}[{}]: {}",
            self.severity.as_str(),
            self.code,
            self.message
        )?;
        if let Some(primary) = &self.primary {
            write_label(formatter, "primary", primary)?;
        }
        for label in &self.secondary {
            write_label(formatter, "secondary", label)?;
        }
        for note in &self.notes {
            write!(formatter, "\nnote: {note}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

impl ClassifiedError for Diagnostic {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: self.code,
            category: ErrorCategory::Diagnostic,
            context: ErrorContext {
                span: self.primary.as_ref().map(|label| label.span),
                ..ErrorContext::default()
            },
        }
    }
}

#[derive(Serialize)]
struct DiagnosticOutput {
    code: &'static str,
    severity: &'static str,
    message: String,
    primary: Option<LabelOutput>,
    secondary: Vec<LabelOutput>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct LabelOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    file: u32,
    start: u32,
    end: u32,
    message: String,
}

impl From<&Diagnostic> for DiagnosticOutput {
    fn from(diagnostic: &Diagnostic) -> Self {
        Self {
            code: diagnostic.code.as_str(),
            severity: diagnostic.severity.as_str(),
            message: diagnostic.message.to_string(),
            primary: diagnostic.primary.as_ref().map(|label| LabelOutput {
                source: diagnostic.primary_source.as_ref().map(ToString::to_string),
                file: label.span.file.0,
                start: label.span.start,
                end: label.span.end,
                message: label.message.to_string(),
            }),
            secondary: diagnostic
                .secondary
                .iter()
                .enumerate()
                .map(|(index, label)| LabelOutput {
                    source: diagnostic
                        .secondary_sources
                        .get(index)
                        .and_then(Option::as_ref)
                        .map(ToString::to_string),
                    file: label.span.file.0,
                    start: label.span.start,
                    end: label.span.end,
                    message: label.message.to_string(),
                })
                .collect(),
            notes: diagnostic.notes.iter().map(ToString::to_string).collect(),
        }
    }
}

fn write_label(formatter: &mut fmt::Formatter<'_>, kind: &str, label: &Label) -> fmt::Result {
    write!(
        formatter,
        "\n  {kind} {}:{}..{}: {}",
        label.span.file.0, label.span.start, label.span.end, label.message
    )
}

struct CompileErrorMessage<'a>(&'a CompileError);

impl fmt::Display for CompileErrorMessage<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_compile_error(self.0, formatter)
    }
}

/// Errors produced while crossing a host boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    Trap(HostTrap),
    Request(HostRequestError),
    Lifecycle(RuntimeHostCloseError),
    Realm(RealmError),
    Protocol(HostCompletionProtocolError),
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metadata = self.metadata();
        write!(
            formatter,
            "{} error {}",
            metadata.category.as_str(),
            metadata.code
        )
    }
}

impl std::error::Error for HostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Trap(_) => None,
            Self::Request(error) => Some(error),
            Self::Lifecycle(error) => Some(error),
            Self::Realm(error) => Some(error),
            Self::Protocol(error) => Some(error),
        }
    }
}

impl ClassifiedError for HostError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: match self {
                Self::Trap(trap) => host_trap_code(trap),
                Self::Request(error) => host_request_code(error),
                Self::Lifecycle(_) => ErrorCode::NX5004,
                Self::Realm(error) => realm_error_code(error),
                Self::Protocol(HostCompletionProtocolError::Abandoned) => ErrorCode::NX5002,
                Self::Protocol(HostCompletionProtocolError::UnknownErrorCode(_)) => {
                    ErrorCode::NX5003
                }
            },
            category: ErrorCategory::Host,
            context: ErrorContext::default(),
        }
    }
}

/// Stateful-data and migration-limit failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationError {
    State(StatefulError),
    Limit(MigrationLimitError),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metadata = self.metadata();
        write!(
            formatter,
            "{} error {}",
            metadata.category.as_str(),
            metadata.code
        )
    }
}

impl std::error::Error for MigrationError {}

impl ClassifiedError for MigrationError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: match self {
                Self::State(_) => ErrorCode::NX6002,
                Self::Limit(_) => ErrorCode::NX6001,
            },
            category: ErrorCategory::Migration,
            context: ErrorContext::default(),
        }
    }
}

/// The only error boundary exposed by high-level Nexa facade operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NexaError {
    Diagnostic(Box<Diagnostic>),
    Decode(DecodeError),
    Verify(VerifyError),
    Runtime(RuntimeError),
    Trap(Box<Trap>),
    Host(HostError),
    Reload(ReloadError),
    Migration(MigrationError),
}

impl NexaError {
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        self.metadata().code
    }

    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        self.metadata().category
    }

    #[must_use]
    pub fn context(&self) -> ErrorContext {
        self.metadata().context
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let metadata = self.metadata();
        serde_json::to_string_pretty(&ClassifiedErrorOutput {
            code: metadata.code.as_str(),
            category: metadata.category.as_str(),
            context: ClassifiedErrorContextOutput {
                span: metadata
                    .context
                    .span
                    .map(|span| (span.file.0, span.start, span.end)),
                module: metadata.context.module_epoch.map(|module| module.module.0),
                module_epoch: metadata.context.module_epoch.map(|module| module.epoch),
                task: metadata
                    .context
                    .task
                    .map(|task| format!("{}:{}:{}", task.realm_id, task.index, task.generation)),
            },
        })
    }
}

#[derive(Serialize)]
struct ClassifiedErrorOutput {
    code: &'static str,
    category: &'static str,
    context: ClassifiedErrorContextOutput,
}

#[derive(Serialize)]
struct ClassifiedErrorContextOutput {
    span: Option<(u32, u32, u32)>,
    module: Option<u32>,
    module_epoch: Option<u64>,
    task: Option<String>,
}

impl ClassifiedError for NexaError {
    fn metadata(&self) -> ErrorMetadata {
        match self {
            Self::Diagnostic(error) => error.metadata(),
            Self::Decode(error) => error.metadata(),
            Self::Verify(error) => error.metadata(),
            Self::Runtime(error) => error.metadata(),
            Self::Trap(error) => error.metadata(),
            Self::Host(error) => error.metadata(),
            Self::Reload(error) => error.metadata(),
            Self::Migration(error) => error.metadata(),
        }
    }
}

impl fmt::Display for NexaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Diagnostic(error) => error.fmt(formatter),
            error => {
                let metadata = error.metadata();
                write!(
                    formatter,
                    "{} error {}",
                    metadata.category.as_str(),
                    metadata.code
                )
            }
        }
    }
}

impl std::error::Error for NexaError {}

impl From<CompileError> for NexaError {
    fn from(error: CompileError) -> Self {
        Self::Diagnostic(Box::new(Diagnostic::new(&error, FileId::default())))
    }
}

impl From<Diagnostic> for NexaError {
    fn from(error: Diagnostic) -> Self {
        Self::Diagnostic(Box::new(error))
    }
}

impl From<DecodeError> for NexaError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

impl From<VerifyError> for NexaError {
    fn from(error: VerifyError) -> Self {
        Self::Verify(error)
    }
}

impl From<RuntimeError> for NexaError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<Trap> for NexaError {
    fn from(error: Trap) -> Self {
        Self::Trap(Box::new(error))
    }
}

impl From<HostError> for NexaError {
    fn from(error: HostError) -> Self {
        Self::Host(error)
    }
}

impl From<HostTrap> for NexaError {
    fn from(error: HostTrap) -> Self {
        Self::Host(HostError::Trap(error))
    }
}

impl From<HostRequestError> for NexaError {
    fn from(error: HostRequestError) -> Self {
        Self::Host(HostError::Request(error))
    }
}

impl From<RuntimeHostCloseError> for NexaError {
    fn from(error: RuntimeHostCloseError) -> Self {
        Self::Host(HostError::Lifecycle(error))
    }
}

impl From<HostCompletionProtocolError> for NexaError {
    fn from(error: HostCompletionProtocolError) -> Self {
        Self::Host(HostError::Protocol(error))
    }
}

impl From<RealmError> for NexaError {
    fn from(error: RealmError) -> Self {
        match error {
            RealmError::Runtime(error) => Self::Runtime(error),
            RealmError::Host(error) => Self::Host(HostError::Request(error)),
            RealmError::Reload(error) => Self::Reload(error),
            RealmError::State(error) => Self::Migration(MigrationError::State(error)),
            RealmError::Interpreter(InterpreterError::Migration(message)) => {
                Self::Reload(ReloadError::Migration(message))
            }
            error => Self::Host(HostError::Realm(error)),
        }
    }
}

impl From<ReloadError> for NexaError {
    fn from(error: ReloadError) -> Self {
        Self::Reload(error)
    }
}

impl From<StatefulError> for NexaError {
    fn from(error: StatefulError) -> Self {
        Self::Migration(MigrationError::State(error))
    }
}

impl From<MigrationLimitError> for NexaError {
    fn from(error: MigrationLimitError) -> Self {
        Self::Migration(MigrationError::Limit(error))
    }
}

impl ClassifiedError for DecodeError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: if matches!(self, DecodeError::InvalidSourceMap) {
                ErrorCode::NX3004
            } else {
                ErrorCode::NX3001
            },
            category: ErrorCategory::Decode,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for VerifyError {
    fn metadata(&self) -> ErrorMetadata {
        let code = match self.kind {
            VerifyErrorKind::RegisterOutOfRange(_)
            | VerifyErrorKind::FunctionOutOfRange(_)
            | VerifyErrorKind::HostImportOutOfRange(_)
            | VerifyErrorKind::ExportOutOfRange(_) => ErrorCode::NX3002,
            VerifyErrorKind::RootBitmapLength
            | VerifyErrorKind::ForgedRoot(_)
            | VerifyErrorKind::MissingRoot(_)
            | VerifyErrorKind::InvalidRootMap(_) => ErrorCode::NX3003,
            VerifyErrorKind::InvalidSourceMap => ErrorCode::NX3004,
            VerifyErrorKind::InvalidReloadMetadata => ErrorCode::NX6005,
            _ => ErrorCode::NX3001,
        };
        ErrorMetadata {
            code,
            category: ErrorCategory::Verify,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for RuntimeError {
    fn metadata(&self) -> ErrorMetadata {
        if let Self::Trap(trap) = self {
            return ErrorMetadata {
                code: runtime_trap_code(trap.diagnostic_code),
                category: ErrorCategory::Host,
                context: ErrorContext {
                    span: trap.source_span,
                    module_epoch: trap.module.zip(trap.epoch).map(|(module, epoch)| {
                        ErrorModuleEpoch {
                            module: ModuleId(module.index),
                            epoch,
                        }
                    }),
                    task: trap.task,
                },
            };
        }
        if let Self::Realm(error) = self {
            return error.metadata();
        }
        ErrorMetadata {
            code: match self {
                Self::ResourceLimit(_)
                | Self::Scope(ScopeError::Allocation(_))
                | Self::Task(TaskError::Allocation(_)) => ErrorCode::NX5004,
                Self::Scope(_)
                | Self::Task(_)
                | Self::TerminalTask
                | Self::StaleTaskHandle
                | Self::CrossRealmTaskHandle
                | Self::InjectedFailure(_) => ErrorCode::NX5001,
                Self::Trap(_) | Self::Realm(_) => unreachable!(),
            },
            category: ErrorCategory::Runtime,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for Trap {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: runtime_trap_code(self.diagnostic_code()),
            category: ErrorCategory::Host,
            context: ErrorContext {
                span: self.source_span,
                module_epoch: self
                    .module
                    .zip(self.epoch)
                    .map(|(module, epoch)| ErrorModuleEpoch {
                        module: ModuleId(module.index),
                        epoch,
                    }),
                task: self.task,
            },
        }
    }
}

impl ClassifiedError for ReloadError {
    fn metadata(&self) -> ErrorMetadata {
        let code = match self {
            Self::MigrationLimit(_) => ErrorCode::NX6001,
            Self::GraphCheck
            | Self::MissingForwarding
            | Self::DuplicateForwarding
            | Self::MigrationNoOutput
            | Self::MigrationNotFinished
            | Self::InvalidStateHandle
            | Self::Migration(_) => ErrorCode::NX6002,
            Self::Activation(_) => ErrorCode::NX6003,
            Self::HostContractIdMismatch => ErrorCode::NX4001,
            Self::InvalidState
            | Self::EpochNotNewer
            | Self::StagingCapacity
            | Self::QuiesceTimeout => ErrorCode::NX6005,
        };
        ErrorMetadata {
            code,
            category: match self {
                Self::MigrationLimit(_)
                | Self::GraphCheck
                | Self::MissingForwarding
                | Self::DuplicateForwarding
                | Self::MigrationNoOutput
                | Self::MigrationNotFinished
                | Self::InvalidStateHandle
                | Self::Migration(_) => ErrorCategory::Migration,
                _ => ErrorCategory::Reload,
            },
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for HostTrap {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: host_trap_code(self),
            category: ErrorCategory::Host,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for HostRequestError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: host_request_code(self),
            category: ErrorCategory::Host,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for RuntimeHostCloseError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: ErrorCode::NX5004,
            category: ErrorCategory::Host,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for RealmError {
    fn metadata(&self) -> ErrorMetadata {
        match self {
            Self::Runtime(error) => error.metadata(),
            Self::Host(error) => error.metadata(),
            Self::Reload(error) => error.metadata(),
            Self::State(error) => error.metadata(),
            error => ErrorMetadata {
                code: realm_error_code(error),
                category: ErrorCategory::Runtime,
                context: ErrorContext::default(),
            },
        }
    }
}

impl ClassifiedError for StatefulError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: ErrorCode::NX6002,
            category: ErrorCategory::Migration,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for MigrationLimitError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: ErrorCode::NX6001,
            category: ErrorCategory::Migration,
            context: ErrorContext::default(),
        }
    }
}

fn compile_error_code(error: &CompileError) -> ErrorCode {
    match error {
        CompileError::AnalysisDiagnostic(diagnostic) => diagnostic.code,
        CompileError::UnknownName { .. }
        | CompileError::DuplicateName { .. }
        | CompileError::MissingMain { .. } => ErrorCode::NX2001,
        CompileError::MissingReplEntrypoint { .. } => ErrorCode::NX7010,
        CompileError::InvalidReplEntrypoint { .. } => ErrorCode::NX7011,
        CompileError::UnknownType { .. }
        | CompileError::MissingReturn { .. }
        | CompileError::DeferCaptureLimit { .. }
        | CompileError::InvalidEffect { .. }
        | CompileError::TooManyRegisters { .. }
        | CompileError::Verify { .. } => ErrorCode::NX2002,
        CompileError::InvalidReloadMetadata { .. } => ErrorCode::NX6005,
        CompileError::TypeMismatch { .. } | CompileError::InvalidMainSignature { .. } => {
            ErrorCode::NX2101
        }
    }
}

fn compile_error_phase(error: &CompileError) -> DiagnosticPhase {
    match error {
        CompileError::AnalysisDiagnostic(diagnostic) => analysis_diagnostic_phase(diagnostic.code),
        CompileError::UnknownName { .. }
        | CompileError::DuplicateName { .. }
        | CompileError::UnknownType { .. }
        | CompileError::MissingMain { .. } => DiagnosticPhase::Resolve,
        CompileError::InvalidReloadMetadata { .. }
        | CompileError::TooManyRegisters { .. }
        | CompileError::MissingReplEntrypoint { .. }
        | CompileError::InvalidReplEntrypoint { .. } => DiagnosticPhase::Lower,
        CompileError::Verify { .. } => DiagnosticPhase::Verify,
        CompileError::TypeMismatch { .. }
        | CompileError::MissingReturn { .. }
        | CompileError::DeferCaptureLimit { .. }
        | CompileError::InvalidEffect { .. }
        | CompileError::InvalidMainSignature { .. } => DiagnosticPhase::TypeCheck,
    }
}

fn analysis_diagnostic_phase(code: ErrorCode) -> DiagnosticPhase {
    match code.as_str() {
        "NX1001" => DiagnosticPhase::Lex,
        "NX1002" => DiagnosticPhase::Parse,
        "NX2001" | "NX2002" | "NX2701" | "NX2702" | "NX2703" | "NX2704" | "NX2705" | "NX2706"
        | "NX2710" | "NX2711" => DiagnosticPhase::Resolve,
        "NX3001" | "NX3002" | "NX3003" | "NX3004" => DiagnosticPhase::Verify,
        "NX6005" => DiagnosticPhase::Lower,
        _ => DiagnosticPhase::TypeCheck,
    }
}

fn write_compile_error(error: &CompileError, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        CompileError::AnalysisDiagnostic(diagnostic) => formatter.write_str(&diagnostic.message),
        CompileError::DuplicateName { name, .. } => write!(formatter, "duplicate name `{name}`"),
        CompileError::UnknownName { name, .. } => write!(formatter, "unknown name `{name}`"),
        CompileError::UnknownType { name, .. } => write!(formatter, "unknown type `{name}`"),
        CompileError::TypeMismatch { .. } => formatter.write_str("type mismatch"),
        CompileError::MissingReturn { .. } => formatter.write_str("missing return"),
        CompileError::DeferCaptureLimit { .. } => {
            formatter.write_str("defer capture limit exceeded")
        }
        CompileError::InvalidEffect { .. } => formatter.write_str("invalid function effect"),
        CompileError::MissingMain { entry_module, .. } => {
            write!(
                formatter,
                "standalone entry module `{entry_module}` has no main"
            )
        }
        CompileError::InvalidMainSignature { message, .. }
        | CompileError::InvalidReplEntrypoint { message, .. } => formatter.write_str(message),
        CompileError::MissingReplEntrypoint { .. } => {
            formatter.write_str("compiled REPL cell is missing its transactional entrypoint")
        }
        CompileError::InvalidReloadMetadata { message, .. } => {
            write!(formatter, "invalid reload metadata: {message}")
        }
        CompileError::TooManyRegisters { .. } => formatter.write_str("register limit exceeded"),
        CompileError::Verify { message, .. } => {
            write!(formatter, "verification failed: {message}")
        }
    }
}

fn host_trap_code(error: &HostTrap) -> ErrorCode {
    match error {
        HostTrap::UnknownFunction(_) | HostTrap::InvalidFunctionSlot(_) => ErrorCode::NX4001,
        HostTrap::Arity | HostTrap::Type => ErrorCode::NX4003,
        HostTrap::ResourceCapacity => ErrorCode::NX5004,
        HostTrap::Panicked | HostTrap::Host(_) => ErrorCode::NX5001,
    }
}

fn runtime_trap_code(code: &str) -> ErrorCode {
    match code {
        "NX4001" => ErrorCode::NX4001,
        "NX4003" => ErrorCode::NX4003,
        "NX5002" => ErrorCode::NX5002,
        "NX5003" => ErrorCode::NX5003,
        "NX5004" => ErrorCode::NX5004,
        _ => ErrorCode::NX5001,
    }
}

fn host_request_code(error: &HostRequestError) -> ErrorCode {
    match error {
        HostRequestError::CompletionQueueFull | HostRequestError::Allocation(_) => {
            ErrorCode::NX5004
        }
        HostRequestError::UnknownCustomDomain(_) => ErrorCode::NX4002,
        HostRequestError::Handle(_)
        | HostRequestError::ReleaseQueue(_)
        | HostRequestError::HostClosing
        | HostRequestError::HostClosed
        | HostRequestError::CompletionQueueClosed
        | HostRequestError::StaleHostRequestHandle
        | HostRequestError::CrossRealmHostRequestHandle
        | HostRequestError::AlreadyCompleted
        | HostRequestError::DetachedByReload
        | HostRequestError::InvalidState
        | HostRequestError::InjectedFailure(_) => ErrorCode::NX5001,
    }
}

fn realm_error_code(error: &RealmError) -> ErrorCode {
    match error {
        RealmError::HostContractIdMismatch
        | RealmError::MissingHostContractRuntimeId
        | RealmError::MissingHostFunctionAuthority(_)
        | RealmError::HostFunctionAuthorityMismatch { .. }
        | RealmError::MissingScriptExport(_)
        | RealmError::ScriptExportMetadataMismatch(_) => ErrorCode::NX4001,
        RealmError::HostCapabilitiesUnavailable | RealmError::ScriptExportNotCallable(_) => {
            ErrorCode::NX4002
        }
        RealmError::MissingTransactionalCellExport(_) => ErrorCode::NX7010,
        RealmError::TransactionalCellSignatureMismatch(_)
        | RealmError::TransactionalCellEffectMismatch { .. } => ErrorCode::NX7011,
        RealmError::RuntimeHostClosing
        | RealmError::RuntimeHostClosed
        | RealmError::ModuleAllocation(_)
        | RealmError::EpochExhausted => ErrorCode::NX5004,
        RealmError::Reload(error) => error.metadata().code,
        RealmError::State(_)
        | RealmError::SchemaHashMismatch
        | RealmError::InvalidTransactionalStateExtension
        | RealmError::InvalidTransactionalStateSeed => ErrorCode::NX6002,
        RealmError::Runtime(_)
        | RealmError::Interpreter(_)
        | RealmError::Host(_)
        | RealmError::Heap(_)
        | RealmError::ModuleHandle(_)
        // A verified module that fails predecoding is an internal
        // inconsistency, not a user-facing admission class.
        | RealmError::ExecutableBuild(_)
        | RealmError::MissingModule(_)
        | RealmError::ModuleNotCallable
        | RealmError::TerminalTask
        | RealmError::StaleTaskHandle
        | RealmError::CrossRealmTaskHandle
        | RealmError::TaskWaiting
        | RealmError::TransactionalCellTerminalRecordMissing
        | RealmError::TransactionalCellSetupRollbackFailed { .. }
        | RealmError::InjectedFailure(_) => ErrorCode::NX5001,
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::DecodeError;
    use nexa_compiler::{
        AnalysisDiagnostic, AnalysisDiagnosticLabel, AnalysisDiagnosticSource, CompileError,
    };
    use nexa_core::{FileId, SourceSpan};
    use nexa_runtime::{
        HostRequestError, HostTrap, MigrationLimitError, ReloadError, RuntimeError,
        RuntimeHostCloseError, RuntimeMessage,
    };
    use nexa_verifier::{VerifyError, VerifyErrorKind};

    use super::{
        ClassifiedError, Diagnostic, ERROR_CODE_TABLE, ERROR_EMISSION_TABLE, ErrorCategory,
        ErrorCode, HostError, Label, MigrationError, NexaError, Severity,
    };

    fn analysis_compile_error(code: ErrorCode, message: &str, span: SourceSpan) -> CompileError {
        CompileError::AnalysisDiagnostic(Box::new(AnalysisDiagnostic {
            code,
            message: message.into(),
            primary: AnalysisDiagnosticLabel {
                source: AnalysisDiagnosticSource::Caller,
                span,
                message: "primary source location".into(),
            },
            secondary: Vec::new(),
            related: Vec::new(),
            notes: Vec::new(),
        }))
    }

    #[test]
    fn stable_error_code_table_is_complete_ordered_and_unique() {
        let expected = [
            ("NX1001", "Unexpected character"),
            ("NX1002", "Unexpected token"),
            ("NX2001", "Unknown name"),
            ("NX2002", "Unknown type"),
            ("NX2101", "Type mismatch"),
            ("NX2201", "Non-exhaustive match"),
            ("NX2202", "Duplicate match variant"),
            ("NX2210", "Cannot infer constructor type"),
            ("NX2220", "? requires Result"),
            ("NX2221", "? error mismatch"),
            ("NX2301", "Await outside async function"),
            ("NX2302", "Missing await"),
            ("NX2401", "Invalid numeric conversion"),
            ("NX2501", "Invalid field access"),
            ("NX2601", "Migration intrinsic outside @migration"),
            ("NX2602", "Missing finish_migration"),
            ("NX2603", "Missing forwarding"),
            ("NX2604", "Duplicate forwarding"),
            ("NX2701", "Module path mismatch"),
            ("NX2702", "Module cycle"),
            ("NX2703", "Unknown use path"),
            ("NX2704", "Duplicate/ambiguous namespace"),
            ("NX2705", "Private access"),
            ("NX2706", "Invalid public API exposure"),
            ("NX2710", "Invalid @stable"),
            ("NX2711", "Duplicate/colliding stable identity"),
            ("NX2720", "Invalid const expression"),
            ("NX2730", "Invalid package test"),
            ("NX2740", "Invalid lifecycle/entrypoint location"),
            ("NX3001", "Invalid bytecode section"),
            ("NX3002", "Invalid register range"),
            ("NX3003", "Invalid root map"),
            ("NX3004", "Invalid SourceMap"),
            ("NX4001", "Host contract mismatch"),
            ("NX4002", "Host capability unavailable"),
            ("NX4003", "Host argument mismatch"),
            ("NX5001", "Host result mismatch"),
            ("NX5002", "Host abandoned"),
            ("NX5003", "Unknown host error code"),
            ("NX5004", "Runtime resource capacity"),
            ("NX6001", "Migration limit"),
            ("NX6002", "Migration graph failure"),
            ("NX6003", "Activation failure"),
            ("NX6005", "Invalid ReloadMetadata"),
            ("NX7001", "Package source failure"),
            ("NX7002", "Invalid package manifest"),
            ("NX7003", "Package policy rejection"),
            ("NX7004", "Entitlement unavailable"),
            ("NX7010", "Missing required entrypoint"),
            ("NX7011", "Entrypoint signature mismatch"),
            ("NX7101", "Handler yielded under MustComplete"),
            ("NX7102", "Handler waited under MustComplete"),
            ("NX7103", "Handler trapped"),
            ("NX7201", "Reload rolled back before commit"),
            ("NX7202", "Activation faulted after commit"),
            ("NX7302", "Persistence failed"),
            ("NX7303", "Engine shutdown incomplete"),
        ];

        let actual = ERROR_CODE_TABLE
            .iter()
            .map(|definition| (definition.code.as_str(), definition.summary))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(
            ERROR_CODE_TABLE
                .windows(2)
                .all(|pair| pair[0].code < pair[1].code)
        );
        assert_eq!(
            ErrorCode::NX6005.definition().map(|entry| entry.summary),
            Some("Invalid ReloadMetadata")
        );
        assert_eq!(ErrorCode::new("NX9999").definition(), None);
    }

    #[test]
    fn diagnostic_code_emission_table_is_complete() {
        let registered = ERROR_CODE_TABLE
            .iter()
            .map(|definition| definition.code)
            .collect::<std::collections::BTreeSet<_>>();
        let emitted = ERROR_EMISSION_TABLE
            .iter()
            .map(|definition| definition.code)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(registered, emitted);
        assert_eq!(ERROR_EMISSION_TABLE.len(), ERROR_CODE_TABLE.len());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for definition in ERROR_EMISSION_TABLE {
            assert!(!definition.module.is_empty());
            assert!(!definition.variant.is_empty());
            assert!(!definition.test.is_empty());
            if matches!(
                definition.code,
                ErrorCode::NX1001
                    | ErrorCode::NX1002
                    | ErrorCode::NX2001
                    | ErrorCode::NX2002
                    | ErrorCode::NX2101
                    | ErrorCode::NX2201
                    | ErrorCode::NX2202
                    | ErrorCode::NX2210
                    | ErrorCode::NX2220
                    | ErrorCode::NX2221
                    | ErrorCode::NX2301
                    | ErrorCode::NX2302
                    | ErrorCode::NX2401
                    | ErrorCode::NX2501
                    | ErrorCode::NX2601
                    | ErrorCode::NX2602
                    | ErrorCode::NX2603
                    | ErrorCode::NX2604
            ) {
                assert!(
                    definition.module.starts_with("nexa-syntax::")
                        || definition.module == "nexa-analysis::analyzer",
                    "{} still names a removed source frontend: {}",
                    definition.code,
                    definition.module
                );
                assert!(
                    !definition.module.starts_with("nexa-compiler::"),
                    "{} must be emitted by the canonical syntax/analysis frontend",
                    definition.code
                );
            }
            assert!(
                root.join(definition.fixture).is_file(),
                "{}",
                definition.fixture
            );
        }
    }

    #[test]
    fn every_public_error_class_has_structured_metadata() {
        let errors = [
            NexaError::Diagnostic(Box::new(Diagnostic::new(
                &analysis_compile_error(
                    ErrorCode::NX1001,
                    "unexpected character `#`",
                    SourceSpan::new(FileId(7), 4, 5),
                ),
                FileId(7),
            ))),
            NexaError::Decode(DecodeError::InvalidMagic),
            NexaError::Verify(VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::RegisterOutOfRange(3),
            }),
            NexaError::Runtime(RuntimeError::ResourceLimit("task")),
            NexaError::Host(HostError::Request(HostRequestError::CompletionQueueFull)),
            NexaError::Reload(ReloadError::Activation(
                nexa_runtime::RuntimeMessage::inline("activation"),
            )),
            NexaError::Migration(MigrationError::Limit(MigrationLimitError::Objects)),
        ];
        let expected = [
            (ErrorCategory::Diagnostic, "NX1001"),
            (ErrorCategory::Decode, "NX3001"),
            (ErrorCategory::Verify, "NX3002"),
            (ErrorCategory::Runtime, "NX5004"),
            (ErrorCategory::Host, "NX5004"),
            (ErrorCategory::Reload, "NX6003"),
            (ErrorCategory::Migration, "NX6001"),
        ];

        for (error, (category, code)) in errors.iter().zip(expected) {
            assert_eq!(error.category(), category);
            assert_eq!(error.code().as_str(), code);
            assert_eq!(error.metadata().category, category);
        }
    }

    #[test]
    fn diagnostic_carries_source_span_without_parsing_display_output() {
        let error = NexaError::from(Diagnostic::new(
            &analysis_compile_error(
                ErrorCode::NX1001,
                "unexpected character `界`",
                SourceSpan::new(FileId(9), 4, 7),
            ),
            FileId(9),
        ));
        assert_eq!(error.context().span, Some(SourceSpan::new(FileId(9), 4, 7)));
        assert!(error.to_string().contains("error[NX1001]"));
        assert!(error.to_string().contains("unexpected character `界`"));
        assert!(error.to_string().contains("primary 9:4..7"));

        let numeric = NexaError::from(Diagnostic::new(
            &analysis_compile_error(
                ErrorCode::NX2401,
                "invalid implicit numeric conversion",
                SourceSpan::new(FileId(9), 12, 17),
            ),
            FileId(9),
        ));
        assert_eq!(numeric.code(), ErrorCode::NX2401);
        assert_eq!(
            numeric.context().span,
            Some(SourceSpan::new(FileId(9), 12, 17))
        );
    }

    #[test]
    fn analyzer_diagnostic_survives_the_compiler_facade_losslessly() {
        let error = CompileError::AnalysisDiagnostic(Box::new(AnalysisDiagnostic {
            code: ErrorCode::NX2202,
            message: "variant `A` is matched more than once".into(),
            primary: AnalysisDiagnosticLabel {
                source: AnalysisDiagnosticSource::Caller,
                span: SourceSpan::new(FileId(2), 20, 26),
                message: "duplicate arm".into(),
            },
            secondary: vec![AnalysisDiagnosticLabel {
                source: AnalysisDiagnosticSource::Canonical(
                    nexa_diagnostics::SourceIdentity::package(
                        "nexa.stdlib",
                        "stdlib/std/core.nexa",
                    ),
                ),
                span: SourceSpan::new(FileId(3), 4, 10),
                message: "first arm".into(),
            }],
            related: vec![AnalysisDiagnosticLabel {
                source: AnalysisDiagnosticSource::Canonical(
                    nexa_diagnostics::SourceIdentity::standalone("contracts/game.nidl"),
                ),
                span: SourceSpan::new(FileId(4), 12, 18),
                message: "variant declaration".into(),
            }],
            notes: vec!["duplicate variant: A".into()],
        }));

        let diagnostic = Diagnostic::new(&error, FileId(99));
        assert_eq!(diagnostic.code, ErrorCode::NX2202);
        assert_eq!(
            diagnostic.message.to_string(),
            "Duplicate match variant: variant `A` is matched more than once"
        );
        assert_eq!(
            diagnostic.primary,
            Some(Label {
                span: SourceSpan::new(FileId(2), 20, 26),
                message: nexa_runtime::RuntimeMessage::inline("duplicate arm"),
            })
        );
        assert_eq!(
            diagnostic
                .secondary
                .iter()
                .map(|label| (label.span, label.message.to_string()))
                .collect::<Vec<_>>(),
            vec![
                (SourceSpan::new(FileId(3), 4, 10), "first arm".to_owned()),
                (
                    SourceSpan::new(FileId(4), 12, 18),
                    "variant declaration".to_owned()
                ),
            ]
        );
        assert_eq!(
            diagnostic
                .notes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["duplicate variant: A"]
        );
        assert_eq!(diagnostic.primary_source_identity(), None);
        assert_eq!(
            diagnostic
                .secondary_source_identity(0)
                .map(ToString::to_string),
            Some("nexa.stdlib:stdlib/std/core.nexa".into())
        );
        assert_eq!(
            diagnostic
                .secondary_source_identity(1)
                .map(ToString::to_string),
            Some("contracts/game.nidl".into())
        );
        let leaf = diagnostic.to_leaf_with_source_identities(|file| {
            nexa_diagnostics::SourceIdentity::standalone(format!("caller/{}.nexa", file.0))
        });
        assert_eq!(leaf.labels[0].source.path(), "caller/2.nexa");
        assert_eq!(
            leaf.labels[1].source.to_string(),
            "nexa.stdlib:stdlib/std/core.nexa"
        );
        assert_eq!(leaf.labels[2].source.path(), "contracts/game.nidl");
        let json: serde_json::Value = serde_json::from_str(&diagnostic.to_json().unwrap()).unwrap();
        assert_eq!(
            json["secondary"][0]["source"],
            "nexa.stdlib:stdlib/std/core.nexa"
        );
        assert_eq!(json["secondary"][1]["source"], "contracts/game.nidl");
        assert_eq!(diagnostic.phase(), Some(super::DiagnosticPhase::TypeCheck));
    }

    #[test]
    fn compile_diagnostic_preserves_cross_file_labels_and_leaf_identities() {
        let diagnostic = Diagnostic::new(
            &CompileError::DuplicateName {
                name: "shared".into(),
                first: SourceSpan::new(FileId(31), 4, 10),
                duplicate: SourceSpan::new(FileId(47), 12, 18),
            },
            FileId(99),
        );

        assert_eq!(
            diagnostic.primary.as_ref().map(|label| label.span),
            Some(SourceSpan::new(FileId(47), 12, 18))
        );
        assert_eq!(
            diagnostic.secondary[0].span,
            SourceSpan::new(FileId(31), 4, 10)
        );
        assert_eq!(diagnostic.phase(), Some(super::DiagnosticPhase::Resolve));

        let leaf = diagnostic.to_leaf_with_source_identities(|file| {
            nexa_diagnostics::SourceIdentity::standalone(format!("src/{}.nexa", file.0))
        });
        assert_eq!(leaf.labels[0].source.path(), "src/47.nexa");
        assert_eq!(
            leaf.labels[0].range,
            nexa_diagnostics::ByteRange::new(12, 18)
        );
        assert_eq!(leaf.labels[1].source.path(), "src/31.nexa");
        assert_eq!(
            leaf.labels[1].range,
            nexa_diagnostics::ByteRange::new(4, 10)
        );
    }

    #[test]
    fn source_backed_diagnostics_have_no_zero_zero_spans() {
        let cases = [
            analysis_compile_error(
                ErrorCode::NX1001,
                "unexpected character",
                SourceSpan::new(FileId(2), 4, 5),
            ),
            analysis_compile_error(
                ErrorCode::NX1002,
                "unexpected token",
                SourceSpan::new(FileId(2), 8, 9),
            ),
            analysis_compile_error(
                ErrorCode::NX1002,
                "unexpected end",
                SourceSpan::new(FileId(2), 11, 12),
            ),
            CompileError::TypeMismatch {
                expected: Some(nexa_bytecode::ValueType::I32),
                actual: Some(nexa_bytecode::ValueType::Bool),
                span: SourceSpan::new(FileId(2), 15, 19),
            },
            analysis_compile_error(
                ErrorCode::NX2220,
                "`?` requires Result",
                SourceSpan::new(FileId(2), 21, 23),
            ),
        ];
        for error in cases {
            let diagnostic = Diagnostic::new(&error, FileId(2));
            let primary = diagnostic
                .primary
                .expect("compiler diagnostics are located");
            assert!(primary.span.start < primary.span.end, "{error:?}");
            assert_ne!((primary.span.start, primary.span.end), (0, 0));
        }
        let unlocated = Diagnostic::without_source(
            ErrorCode::NX3001,
            Severity::Error,
            RuntimeMessage::Static("invalid bytecode"),
        );
        assert!(unlocated.primary.is_none());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&unlocated.to_json().unwrap()).unwrap()["primary"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn every_stable_code_renders_from_one_structure_in_human_and_json_formats() {
        for definition in ERROR_CODE_TABLE {
            let diagnostic = Diagnostic::from_parts(
                definition.code,
                Severity::Error,
                RuntimeMessage::Static(definition.summary),
                Label {
                    span: SourceSpan::new(FileId(3), 5, 8),
                    message: RuntimeMessage::Static("selected source"),
                },
            );
            let human = diagnostic.to_string();
            let json: serde_json::Value =
                serde_json::from_str(&diagnostic.to_json().unwrap()).unwrap();

            assert!(human.contains(definition.code.as_str()));
            assert!(human.contains(definition.summary));
            assert_eq!(json["code"], definition.code.as_str());
            assert_eq!(json["severity"], "error");
            assert_eq!(json["message"], definition.summary);
            assert_eq!(json["primary"]["file"], 3);
            assert_eq!(json["primary"]["start"], 5);
            assert_eq!(json["primary"]["end"], 8);
            assert_eq!(json["secondary"], serde_json::json!([]));
            assert_eq!(json["notes"], serde_json::json!([]));
        }
    }

    #[test]
    fn host_variants_have_codes_without_debug_formatting() {
        let errors = [
            HostError::Trap(HostTrap::Arity),
            HostError::Lifecycle(RuntimeHostCloseError::LiveRealms),
        ];
        assert_eq!(errors[0].metadata().code.as_str(), "NX4003");
        assert_eq!(errors[1].metadata().code.as_str(), "NX5004");
        assert_eq!(errors[0].to_string(), "host error NX4003");
    }
}
