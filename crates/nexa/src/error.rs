use std::fmt;

use nexa_bytecode::DecodeError;
use nexa_compiler::CompileError;
use nexa_core::{FileId, ModuleId, RawHandle, SourceSpan};
use nexa_runtime::{
    HostRequestError, HostTrap, MigrationLimitError, RealmError, ReloadError, RuntimeError,
    RuntimeHostCloseError, RuntimeMessage, StatefulError,
};
use nexa_verifier::{VerifyError, VerifyErrorKind};
use serde::Serialize;

/// A stable, machine-readable public error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(&'static str);

pub type DiagnosticCode = ErrorCode;

impl ErrorCode {
    pub const NX1001: Self = Self::new("NX1001");
    pub const NX1002: Self = Self::new("NX1002");
    pub const NX2001: Self = Self::new("NX2001");
    pub const NX2002: Self = Self::new("NX2002");
    pub const NX2101: Self = Self::new("NX2101");
    pub const NX2201: Self = Self::new("NX2201");
    pub const NX2202: Self = Self::new("NX2202");
    pub const NX2210: Self = Self::new("NX2210");
    pub const NX2220: Self = Self::new("NX2220");
    pub const NX2221: Self = Self::new("NX2221");
    pub const NX2301: Self = Self::new("NX2301");
    pub const NX2302: Self = Self::new("NX2302");
    pub const NX2401: Self = Self::new("NX2401");
    pub const NX2501: Self = Self::new("NX2501");
    pub const NX2601: Self = Self::new("NX2601");
    pub const NX2602: Self = Self::new("NX2602");
    pub const NX2603: Self = Self::new("NX2603");
    pub const NX2604: Self = Self::new("NX2604");
    pub const NX3001: Self = Self::new("NX3001");
    pub const NX3002: Self = Self::new("NX3002");
    pub const NX3003: Self = Self::new("NX3003");
    pub const NX3004: Self = Self::new("NX3004");
    pub const NX4001: Self = Self::new("NX4001");
    pub const NX4002: Self = Self::new("NX4002");
    pub const NX4003: Self = Self::new("NX4003");
    pub const NX5001: Self = Self::new("NX5001");
    pub const NX5002: Self = Self::new("NX5002");
    pub const NX5003: Self = Self::new("NX5003");
    pub const NX5004: Self = Self::new("NX5004");
    pub const NX6001: Self = Self::new("NX6001");
    pub const NX6002: Self = Self::new("NX6002");
    pub const NX6003: Self = Self::new("NX6003");
    pub const NX6004: Self = Self::new("NX6004");
    pub const NX6005: Self = Self::new("NX6005");

    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    #[must_use]
    pub fn definition(self) -> Option<&'static ErrorCodeDefinition> {
        ERROR_CODE_TABLE
            .iter()
            .find(|definition| definition.code == self)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// One immutable entry in Nexa's public diagnostic-code registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCodeDefinition {
    pub code: ErrorCode,
    pub summary: &'static str,
}

impl ErrorCodeDefinition {
    const fn new(code: ErrorCode, summary: &'static str) -> Self {
        Self { code, summary }
    }
}

/// The complete Milestone 4.0 stable error-code registry, ordered by code.
pub static ERROR_CODE_TABLE: &[ErrorCodeDefinition] = &[
    ErrorCodeDefinition::new(ErrorCode::NX1001, "Unexpected character"),
    ErrorCodeDefinition::new(ErrorCode::NX1002, "Unexpected token"),
    ErrorCodeDefinition::new(ErrorCode::NX2001, "Unknown name"),
    ErrorCodeDefinition::new(ErrorCode::NX2002, "Unknown type"),
    ErrorCodeDefinition::new(ErrorCode::NX2101, "Type mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX2201, "Non-exhaustive match"),
    ErrorCodeDefinition::new(ErrorCode::NX2202, "Duplicate match variant"),
    ErrorCodeDefinition::new(ErrorCode::NX2210, "Cannot infer constructor type"),
    ErrorCodeDefinition::new(ErrorCode::NX2220, "? requires Result"),
    ErrorCodeDefinition::new(ErrorCode::NX2221, "? error mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX2301, "Await outside Task"),
    ErrorCodeDefinition::new(ErrorCode::NX2302, "Missing await"),
    ErrorCodeDefinition::new(ErrorCode::NX2401, "Invalid numeric conversion"),
    ErrorCodeDefinition::new(ErrorCode::NX2501, "Invalid field access"),
    ErrorCodeDefinition::new(ErrorCode::NX2601, "Migration intrinsic outside Migration"),
    ErrorCodeDefinition::new(ErrorCode::NX2602, "Missing finish_migration"),
    ErrorCodeDefinition::new(ErrorCode::NX2603, "Missing forwarding"),
    ErrorCodeDefinition::new(ErrorCode::NX2604, "Duplicate forwarding"),
    ErrorCodeDefinition::new(ErrorCode::NX3001, "Invalid bytecode section"),
    ErrorCodeDefinition::new(ErrorCode::NX3002, "Invalid register range"),
    ErrorCodeDefinition::new(ErrorCode::NX3003, "Invalid root map"),
    ErrorCodeDefinition::new(ErrorCode::NX3004, "Invalid SourceMap"),
    ErrorCodeDefinition::new(ErrorCode::NX4001, "Host interface mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX4002, "Host capability unavailable"),
    ErrorCodeDefinition::new(ErrorCode::NX4003, "Host argument mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX5001, "Host result mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX5002, "Host abandoned"),
    ErrorCodeDefinition::new(ErrorCode::NX5003, "Unknown host error code"),
    ErrorCodeDefinition::new(ErrorCode::NX5004, "Runtime resource capacity"),
    ErrorCodeDefinition::new(ErrorCode::NX6001, "Migration limit"),
    ErrorCodeDefinition::new(ErrorCode::NX6002, "Migration graph failure"),
    ErrorCodeDefinition::new(ErrorCode::NX6003, "Activation failure"),
    ErrorCodeDefinition::new(ErrorCode::NX6004, "Reload completion capacity"),
    ErrorCodeDefinition::new(ErrorCode::NX6005, "Invalid ReloadMetadata"),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label {
    pub span: SourceSpan,
    pub message: RuntimeMessage,
}

/// One source-backed diagnostic representation shared by human and JSON renderers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub message: RuntimeMessage,
    pub primary: Label,
    pub secondary: Vec<Label>,
    pub notes: Vec<RuntimeMessage>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(error: &CompileError, file: FileId) -> Self {
        let span = compile_error_span(error, file).unwrap_or_else(|| SourceSpan::new(file, 0, 0));
        let message = RuntimeMessage::inline(&CompileErrorMessage(error).to_string());
        Self {
            code: compile_error_code(error),
            severity: Severity::Error,
            message,
            primary: Label {
                span,
                message: RuntimeMessage::Static("primary source location"),
            },
            secondary: Vec::new(),
            notes: Vec::new(),
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
            primary,
            secondary: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&DiagnosticOutput::from(self))
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
        write_label(formatter, "primary", &self.primary)?;
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
                span: Some(self.primary.span),
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
    primary: LabelOutput,
    secondary: Vec<LabelOutput>,
    notes: Vec<String>,
}

#[derive(Serialize)]
struct LabelOutput {
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
            primary: LabelOutput::from(&diagnostic.primary),
            secondary: diagnostic.secondary.iter().map(LabelOutput::from).collect(),
            notes: diagnostic.notes.iter().map(ToString::to_string).collect(),
        }
    }
}

impl From<&Label> for LabelOutput {
    fn from(label: &Label) -> Self {
        Self {
            file: label.span.file.0,
            start: label.span.start,
            end: label.span.end,
            message: label.message.to_string(),
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

impl std::error::Error for HostError {}

impl ClassifiedError for HostError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: match self {
                Self::Trap(trap) => host_trap_code(trap),
                Self::Request(error) => host_request_code(error),
                Self::Lifecycle(_) => ErrorCode::NX5004,
                Self::Realm(error) => realm_error_code(error),
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
}

impl ClassifiedError for NexaError {
    fn metadata(&self) -> ErrorMetadata {
        match self {
            Self::Diagnostic(error) => error.metadata(),
            Self::Decode(error) => error.metadata(),
            Self::Verify(error) => error.metadata(),
            Self::Runtime(error) => error.metadata(),
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

impl From<RealmError> for NexaError {
    fn from(error: RealmError) -> Self {
        match error {
            RealmError::Runtime(error) => Self::Runtime(error),
            RealmError::Host(error) => Self::Host(HostError::Request(error)),
            RealmError::Reload(error) => Self::Reload(error),
            RealmError::State(error) => Self::Migration(MigrationError::State(error)),
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
        ErrorMetadata {
            code: match self {
                Self::ResourceLimit(_) => ErrorCode::NX5004,
                Self::Scope(_) | Self::Task(_) | Self::InjectedFailure(_) => ErrorCode::NX5001,
            },
            category: ErrorCategory::Runtime,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for ReloadError {
    fn metadata(&self) -> ErrorMetadata {
        let code = match self {
            Self::CompletionBufferCapacity => ErrorCode::NX6004,
            Self::MigrationLimit(_) => ErrorCode::NX6001,
            Self::GraphCheck | Self::MissingForwarding | Self::DuplicateForwarding => {
                ErrorCode::NX6002
            }
            Self::Activation(_) => ErrorCode::NX6003,
            Self::HostHashMismatch => ErrorCode::NX4001,
            Self::InvalidState
            | Self::EpochNotNewer
            | Self::StagingCapacity
            | Self::MigrationNoOutput
            | Self::MigrationNotFinished
            | Self::InvalidStateHandle
            | Self::Migration(_)
            | Self::QuiesceTimeout => ErrorCode::NX6005,
        };
        ErrorMetadata {
            code,
            category: ErrorCategory::Reload,
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

fn compile_error_span(error: &CompileError, file: FileId) -> Option<SourceSpan> {
    let (start, end) = match error {
        CompileError::UnexpectedCharacter { offset, character } => {
            (*offset, offset.saturating_add(character.len_utf8()))
        }
        CompileError::UnexpectedToken { offset, .. } => (*offset, offset.saturating_add(1)),
        CompileError::UnexpectedEnd => return Some(SourceSpan::new(file, 0, 0)),
        CompileError::InvalidNumericConversion { span } => return Some(*span),
        _ => return None,
    };
    Some(SourceSpan::new(
        file,
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(end).unwrap_or(u32::MAX),
    ))
}

fn compile_error_code(error: &CompileError) -> ErrorCode {
    match error {
        CompileError::UnexpectedCharacter { .. } => ErrorCode::NX1001,
        CompileError::UnexpectedToken { .. } | CompileError::UnexpectedEnd => ErrorCode::NX1002,
        CompileError::UnknownName(_) | CompileError::DuplicateName(_) => ErrorCode::NX2001,
        CompileError::UnknownType(_)
        | CompileError::MissingReturn
        | CompileError::SuspendingDefer
        | CompileError::DeferCaptureLimit
        | CompileError::InvalidEffect
        | CompileError::TooManyRegisters
        | CompileError::Verify(_) => ErrorCode::NX2002,
        CompileError::InvalidReloadMetadata(_) => ErrorCode::NX6005,
        CompileError::TypeMismatch => ErrorCode::NX2101,
        CompileError::InvalidNumericConversion { .. } => ErrorCode::NX2401,
        CompileError::CannotInferType => ErrorCode::NX2210,
        CompileError::NonExhaustiveMatch => ErrorCode::NX2201,
        CompileError::DuplicateMatchVariant => ErrorCode::NX2202,
    }
}

fn write_compile_error(error: &CompileError, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        CompileError::UnexpectedCharacter { offset, character } => {
            write!(
                formatter,
                "unexpected character `{character}` at byte {offset}"
            )
        }
        CompileError::UnexpectedToken { offset, expected } => {
            write!(
                formatter,
                "unexpected token at byte {offset}; expected {expected}"
            )
        }
        CompileError::UnexpectedEnd => formatter.write_str("unexpected end of input"),
        CompileError::DuplicateName(name) => write!(formatter, "duplicate name `{name}`"),
        CompileError::UnknownName(name) => write!(formatter, "unknown name `{name}`"),
        CompileError::UnknownType(name) => write!(formatter, "unknown type `{name}`"),
        CompileError::TypeMismatch => formatter.write_str("type mismatch"),
        CompileError::InvalidNumericConversion { .. } => {
            formatter.write_str("invalid implicit numeric conversion")
        }
        CompileError::CannotInferType => formatter.write_str("cannot infer type"),
        CompileError::NonExhaustiveMatch => formatter.write_str("non-exhaustive match"),
        CompileError::DuplicateMatchVariant => formatter.write_str("duplicate match variant"),
        CompileError::MissingReturn => formatter.write_str("missing return"),
        CompileError::SuspendingDefer => formatter.write_str("defer body may suspend"),
        CompileError::DeferCaptureLimit => formatter.write_str("defer capture limit exceeded"),
        CompileError::InvalidEffect => formatter.write_str("invalid function effect"),
        CompileError::InvalidReloadMetadata(message) => {
            write!(formatter, "invalid reload metadata: {message}")
        }
        CompileError::TooManyRegisters => formatter.write_str("register limit exceeded"),
        CompileError::Verify(message) => write!(formatter, "verification failed: {message}"),
    }
}

fn host_trap_code(error: &HostTrap) -> ErrorCode {
    match error {
        HostTrap::UnknownFunction(_) => ErrorCode::NX4001,
        HostTrap::Arity | HostTrap::Type => ErrorCode::NX4003,
        HostTrap::Panicked | HostTrap::Host(_) => ErrorCode::NX5001,
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
        | HostRequestError::AlreadyCompleted
        | HostRequestError::InvalidState => ErrorCode::NX5001,
    }
}

fn realm_error_code(error: &RealmError) -> ErrorCode {
    match error {
        RealmError::HostHashMismatch | RealmError::MissingHostInterfaceHash => ErrorCode::NX4001,
        RealmError::HostCapabilitiesUnavailable => ErrorCode::NX4002,
        RealmError::RuntimeHostClosing
        | RealmError::RuntimeHostClosed
        | RealmError::ModuleAllocation(_)
        | RealmError::EpochExhausted => ErrorCode::NX5004,
        RealmError::Reload(error) => error.metadata().code,
        RealmError::State(_) | RealmError::SchemaHashMismatch => ErrorCode::NX6002,
        RealmError::Runtime(_)
        | RealmError::Interpreter(_)
        | RealmError::Host(_)
        | RealmError::Heap(_)
        | RealmError::ModuleHandle(_)
        | RealmError::MissingModule(_)
        | RealmError::ModuleNotCallable
        | RealmError::TerminalTask
        | RealmError::TaskWaiting => ErrorCode::NX5001,
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::DecodeError;
    use nexa_compiler::CompileError;
    use nexa_core::{FileId, SourceSpan};
    use nexa_runtime::{
        HostRequestError, HostTrap, MigrationLimitError, ReloadError, RuntimeError,
        RuntimeHostCloseError, RuntimeMessage,
    };
    use nexa_verifier::{VerifyError, VerifyErrorKind};

    use super::{
        ClassifiedError, Diagnostic, ERROR_CODE_TABLE, ErrorCategory, ErrorCode, HostError, Label,
        MigrationError, NexaError, Severity,
    };

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
            ("NX2301", "Await outside Task"),
            ("NX2302", "Missing await"),
            ("NX2401", "Invalid numeric conversion"),
            ("NX2501", "Invalid field access"),
            ("NX2601", "Migration intrinsic outside Migration"),
            ("NX2602", "Missing finish_migration"),
            ("NX2603", "Missing forwarding"),
            ("NX2604", "Duplicate forwarding"),
            ("NX3001", "Invalid bytecode section"),
            ("NX3002", "Invalid register range"),
            ("NX3003", "Invalid root map"),
            ("NX3004", "Invalid SourceMap"),
            ("NX4001", "Host interface mismatch"),
            ("NX4002", "Host capability unavailable"),
            ("NX4003", "Host argument mismatch"),
            ("NX5001", "Host result mismatch"),
            ("NX5002", "Host abandoned"),
            ("NX5003", "Unknown host error code"),
            ("NX5004", "Runtime resource capacity"),
            ("NX6001", "Migration limit"),
            ("NX6002", "Migration graph failure"),
            ("NX6003", "Activation failure"),
            ("NX6004", "Reload completion capacity"),
            ("NX6005", "Invalid ReloadMetadata"),
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
    fn every_public_error_class_has_structured_metadata() {
        let errors = [
            NexaError::Diagnostic(Box::new(Diagnostic::new(
                &CompileError::UnexpectedCharacter {
                    offset: 4,
                    character: '#',
                },
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
            NexaError::Reload(ReloadError::CompletionBufferCapacity),
            NexaError::Migration(MigrationError::Limit(MigrationLimitError::Objects)),
        ];
        let expected = [
            (ErrorCategory::Diagnostic, "NX1001"),
            (ErrorCategory::Decode, "NX3001"),
            (ErrorCategory::Verify, "NX3002"),
            (ErrorCategory::Runtime, "NX5004"),
            (ErrorCategory::Host, "NX5004"),
            (ErrorCategory::Reload, "NX6004"),
            (ErrorCategory::Migration, "NX6001"),
        ];

        for (error, (category, code)) in errors.iter().zip(expected) {
            assert_eq!(error.category(), category);
            assert_eq!(error.code().as_str(), code);
            assert_eq!(error.metadata().category, category);
            assert!(!error.to_string().contains("UnexpectedCharacter"));
        }
    }

    #[test]
    fn diagnostic_carries_source_span_without_parsing_display_output() {
        let error = NexaError::from(Diagnostic::new(
            &CompileError::UnexpectedCharacter {
                offset: 4,
                character: '界',
            },
            FileId(9),
        ));
        assert_eq!(error.context().span, Some(SourceSpan::new(FileId(9), 4, 7)));
        assert!(error.to_string().contains("error[NX1001]"));
        assert!(error.to_string().contains("unexpected character `界`"));
        assert!(error.to_string().contains("primary 9:4..7"));

        let numeric = NexaError::from(Diagnostic::new(
            &CompileError::InvalidNumericConversion {
                span: SourceSpan::new(FileId(9), 12, 17),
            },
            FileId(9),
        ));
        assert_eq!(numeric.code(), ErrorCode::NX2401);
        assert_eq!(
            numeric.context().span,
            Some(SourceSpan::new(FileId(9), 12, 17))
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
