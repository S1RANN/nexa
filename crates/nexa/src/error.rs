use std::fmt;

use nexa_bytecode::DecodeError;
use nexa_compiler::CompileError;
use nexa_core::{FileId, ModuleId, RawHandle, SourceSpan};
use nexa_runtime::{
    HostRequestError, HostTrap, MigrationLimitError, RealmError, ReloadError, RuntimeError,
    RuntimeHostCloseError, StatefulError,
};
use nexa_verifier::{VerifyError, VerifyErrorKind};

/// A stable, machine-readable public error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(&'static str);

impl ErrorCode {
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

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

/// A compiler diagnostic with a source location and stable classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    error: CompileError,
    context: ErrorContext,
}

impl Diagnostic {
    #[must_use]
    pub fn new(error: CompileError, file: FileId) -> Self {
        let span = compile_error_span(&error, file);
        Self {
            error,
            context: ErrorContext {
                span,
                ..ErrorContext::default()
            },
        }
    }

    #[must_use]
    pub const fn error(&self) -> &CompileError {
        &self.error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_compile_error(&self.error, formatter)
    }
}

impl std::error::Error for Diagnostic {}

impl ClassifiedError for Diagnostic {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: compile_error_code(&self.error),
            category: ErrorCategory::Diagnostic,
            context: self.context,
        }
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
                Self::Lifecycle(_) => ErrorCode::new("NX5004"),
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
                Self::State(_) => ErrorCode::new("NX6002"),
                Self::Limit(_) => ErrorCode::new("NX6001"),
            },
            category: ErrorCategory::Migration,
            context: ErrorContext::default(),
        }
    }
}

/// The only error boundary exposed by high-level Nexa facade operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NexaError {
    Diagnostic(Diagnostic),
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
        Self::Diagnostic(Diagnostic::new(error, FileId::default()))
    }
}

impl From<Diagnostic> for NexaError {
    fn from(error: Diagnostic) -> Self {
        Self::Diagnostic(error)
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
            code: ErrorCode::new("NX3001"),
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
            | VerifyErrorKind::ExportOutOfRange(_) => ErrorCode::new("NX3002"),
            VerifyErrorKind::RootBitmapLength
            | VerifyErrorKind::ForgedRoot(_)
            | VerifyErrorKind::MissingRoot(_)
            | VerifyErrorKind::InvalidRootMap(_) => ErrorCode::new("NX3003"),
            _ => ErrorCode::new("NX3001"),
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
                Self::ResourceLimit(_) => ErrorCode::new("NX5004"),
                Self::Scope(_) | Self::Task(_) | Self::InjectedFailure(_) => {
                    ErrorCode::new("NX5001")
                }
            },
            category: ErrorCategory::Runtime,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for ReloadError {
    fn metadata(&self) -> ErrorMetadata {
        let code = match self {
            Self::CompletionBufferCapacity => ErrorCode::new("NX6004"),
            Self::MigrationLimit(_) => ErrorCode::new("NX6001"),
            Self::GraphCheck | Self::MissingForwarding | Self::DuplicateForwarding => {
                ErrorCode::new("NX6002")
            }
            Self::Activation(_) => ErrorCode::new("NX6003"),
            Self::HostHashMismatch => ErrorCode::new("NX4001"),
            Self::InvalidState
            | Self::EpochNotNewer
            | Self::StagingCapacity
            | Self::MigrationNoOutput
            | Self::MigrationNotFinished
            | Self::InvalidStateHandle
            | Self::Migration(_)
            | Self::QuiesceTimeout => ErrorCode::new("NX6005"),
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
            code: ErrorCode::new("NX5004"),
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
            code: ErrorCode::new("NX6002"),
            category: ErrorCategory::Migration,
            context: ErrorContext::default(),
        }
    }
}

impl ClassifiedError for MigrationLimitError {
    fn metadata(&self) -> ErrorMetadata {
        ErrorMetadata {
            code: ErrorCode::new("NX6001"),
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
        CompileError::UnexpectedCharacter { .. } => ErrorCode::new("NX1001"),
        CompileError::UnexpectedToken { .. } | CompileError::UnexpectedEnd => {
            ErrorCode::new("NX1002")
        }
        CompileError::UnknownName(_) | CompileError::DuplicateName(_) => ErrorCode::new("NX2001"),
        CompileError::UnknownType(_)
        | CompileError::MissingReturn
        | CompileError::SuspendingDefer
        | CompileError::DeferCaptureLimit
        | CompileError::InvalidEffect
        | CompileError::TooManyRegisters
        | CompileError::Verify(_) => ErrorCode::new("NX2002"),
        CompileError::TypeMismatch => ErrorCode::new("NX2101"),
        CompileError::CannotInferType => ErrorCode::new("NX2210"),
        CompileError::NonExhaustiveMatch => ErrorCode::new("NX2201"),
        CompileError::DuplicateMatchVariant => ErrorCode::new("NX2202"),
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
        CompileError::CannotInferType => formatter.write_str("cannot infer type"),
        CompileError::NonExhaustiveMatch => formatter.write_str("non-exhaustive match"),
        CompileError::DuplicateMatchVariant => formatter.write_str("duplicate match variant"),
        CompileError::MissingReturn => formatter.write_str("missing return"),
        CompileError::SuspendingDefer => formatter.write_str("defer body may suspend"),
        CompileError::DeferCaptureLimit => formatter.write_str("defer capture limit exceeded"),
        CompileError::InvalidEffect => formatter.write_str("invalid function effect"),
        CompileError::TooManyRegisters => formatter.write_str("register limit exceeded"),
        CompileError::Verify(message) => write!(formatter, "verification failed: {message}"),
    }
}

fn host_trap_code(error: &HostTrap) -> ErrorCode {
    match error {
        HostTrap::UnknownFunction(_) => ErrorCode::new("NX4001"),
        HostTrap::Arity | HostTrap::Type => ErrorCode::new("NX4003"),
        HostTrap::Panicked | HostTrap::Host(_) => ErrorCode::new("NX5001"),
    }
}

fn host_request_code(error: &HostRequestError) -> ErrorCode {
    match error {
        HostRequestError::CompletionQueueFull | HostRequestError::Allocation(_) => {
            ErrorCode::new("NX5004")
        }
        HostRequestError::UnknownCustomDomain(_) => ErrorCode::new("NX4002"),
        HostRequestError::Handle(_)
        | HostRequestError::ReleaseQueue(_)
        | HostRequestError::CompletionQueueClosed
        | HostRequestError::AlreadyCompleted
        | HostRequestError::InvalidState => ErrorCode::new("NX5001"),
    }
}

fn realm_error_code(error: &RealmError) -> ErrorCode {
    match error {
        RealmError::HostHashMismatch | RealmError::MissingHostInterfaceHash => {
            ErrorCode::new("NX4001")
        }
        RealmError::HostCapabilitiesUnavailable => ErrorCode::new("NX4002"),
        RealmError::RuntimeHostClosed
        | RealmError::ModuleAllocation(_)
        | RealmError::EpochExhausted => ErrorCode::new("NX5004"),
        RealmError::Reload(error) => error.metadata().code,
        RealmError::State(_) | RealmError::SchemaHashMismatch => ErrorCode::new("NX6002"),
        RealmError::Runtime(_)
        | RealmError::Interpreter(_)
        | RealmError::Host(_)
        | RealmError::Heap(_)
        | RealmError::ModuleHandle(_)
        | RealmError::MissingModule(_)
        | RealmError::ModuleNotCallable
        | RealmError::TerminalTask
        | RealmError::TaskWaiting => ErrorCode::new("NX5001"),
    }
}

#[cfg(test)]
mod tests {
    use nexa_bytecode::DecodeError;
    use nexa_compiler::CompileError;
    use nexa_core::{FileId, SourceSpan};
    use nexa_runtime::{
        HostRequestError, HostTrap, MigrationLimitError, ReloadError, RuntimeError,
        RuntimeHostCloseError,
    };
    use nexa_verifier::{VerifyError, VerifyErrorKind};

    use super::{ClassifiedError, Diagnostic, ErrorCategory, HostError, MigrationError, NexaError};

    #[test]
    fn every_public_error_class_has_structured_metadata() {
        let errors = [
            NexaError::Diagnostic(Diagnostic::new(
                CompileError::UnexpectedCharacter {
                    offset: 4,
                    character: '#',
                },
                FileId(7),
            )),
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
            CompileError::UnexpectedCharacter {
                offset: 4,
                character: '界',
            },
            FileId(9),
        ));
        assert_eq!(error.context().span, Some(SourceSpan::new(FileId(9), 4, 7)));
        assert_eq!(error.to_string(), "unexpected character `界` at byte 4");
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
