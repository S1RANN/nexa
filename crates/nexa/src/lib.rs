//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.
#![allow(deprecated)]

mod error;

pub use error::{
    ClassifiedError, Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ERROR_EMISSION_TABLE,
    ErrorCategory, ErrorCode, ErrorCodeDefinition, ErrorContext, ErrorEmissionDefinition,
    ErrorMetadata, ErrorModuleEpoch, HostError, Label, MigrationError, NexaError, Severity,
};

use nexa_bytecode::{DecodeLimits, Module};
use nexa_core::{FileId, StableId};
use nexa_idl::Idl;
use nexa_verifier::{VerifiedModule, VerifierLimits};

/// Compiles source through the stable facade error boundary.
pub fn compile(source: &str) -> Result<VerifiedModule, NexaError> {
    compile_file(source, FileId::default())
}

/// Compiles source and attaches `file` to diagnostics that have a source location.
pub fn compile_file(source: &str, file: FileId) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile(source)
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, file))))
}

/// Compiles source against an exact IDL interface through the stable facade error boundary.
pub fn compile_with_interface(
    source: &str,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile_with_interface(source, interface, schema_hash).map_err(NexaError::from)
}

/// Decodes a bytecode module through the stable facade error boundary.
pub fn decode_module(bytes: &[u8], limits: DecodeLimits) -> Result<Module, NexaError> {
    Module::decode_with_limits(bytes, limits).map_err(NexaError::from)
}

/// Verifies a module through the stable facade error boundary.
pub fn verify_module(module: Module, limits: VerifierLimits) -> Result<VerifiedModule, NexaError> {
    nexa_verifier::verify(module, limits).map_err(NexaError::from)
}

pub mod prelude {
    pub use nexa_bytecode::{
        AbandonPolicy, AsyncResultType, CancelPolicy, DecodeLimits, EnumType, EnumVariant,
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction,
        MigrationLimitRequirements, ModuleBuilder, ReloadMetadata, Signature, StateField,
        StateSchema, StateType, ValueType, option_type, result_type,
    };
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::{exact_hash as exact_idl_hash, generate_rust as generate_rust_bindings};
    pub use nexa_runtime::{
        CancelReason, CompletionAccounting, HostArgs, HostCallOutcome, HostCompletionResult,
        HostCompletionTicket, HostErrorPayload, HostPayload, HostRegistry, HostRequestHandle,
        HostTrap, HostValue, MigrationCapacityReport, MigrationLimitError, MigrationLimits,
        MigrationUsageReport, ModuleEpochKey, ModuleHandle, PendingHostRequest, PendingReason,
        PollResult, RealmConfig, RealmError, RealmRuntime, ResourceContext, ResourceTokenHandle,
        RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint,
        RuntimeHost, RuntimeHostCloseError, RuntimeHostCloseStatus, RuntimeHostDomain,
        RuntimeHostState, RuntimeResourceLedger, RuntimeValue, ScopeHandle, ScopeSnapshot,
        ScriptFunction, SnapshotHandle, StateHandle, StateHandleError, StateValue,
        StatefulDomainId, StepConfig, TaskHandle, TaskLimits, TaskTerminalReason,
        TaskTerminalRecord, TickBudget, TickReport,
    };
    pub use nexa_verifier::VerifierLimits;

    pub use crate::{
        ClassifiedError, Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ErrorCategory, ErrorCode,
        ErrorCodeDefinition, ErrorContext, ErrorMetadata, ErrorModuleEpoch, HostError, Label,
        MigrationError, NexaError, Severity, compile, compile_file, compile_with_interface,
        decode_module, verify_module,
    };
}

#[cfg(test)]
mod cli {
    use nexa_bytecode::{DecodeError, ValueType};
    use nexa_compiler::CompileError;
    use nexa_core::{FileId, SourceSpan, StableId};
    use nexa_runtime::{
        HostTrap, MigrationLimitError, RealmError, ReloadError, RuntimeError, RuntimeMessage,
    };
    use nexa_verifier::{VerifyError, VerifyErrorKind};

    use crate::{
        Diagnostic, ERROR_EMISSION_TABLE, ErrorCategory, ErrorCode, HostError, Label, NexaError,
        Severity,
    };

    fn compile_error(code: ErrorCode, span: SourceSpan) -> CompileError {
        match code {
            ErrorCode::NX1001 => CompileError::UnexpectedCharacter {
                offset: span.start as usize,
                character: '#',
            },
            ErrorCode::NX1002 => CompileError::UnexpectedToken {
                offset: span.start as usize,
                expected: "identifier",
            },
            ErrorCode::NX2001 => CompileError::UnknownName {
                name: "missing".into(),
                span,
            },
            ErrorCode::NX2002 => CompileError::UnknownType {
                name: "Missing".into(),
                span,
            },
            ErrorCode::NX2101 => CompileError::TypeMismatch {
                expected: Some(ValueType::I32),
                actual: Some(ValueType::Bool),
                span,
            },
            ErrorCode::NX2201 => CompileError::NonExhaustiveMatch {
                missing: vec![StableId::from_name("Missing")],
                span,
            },
            ErrorCode::NX2202 => CompileError::DuplicateMatchVariant {
                variant: StableId::from_name("Duplicate"),
                first: span,
                duplicate: span,
            },
            ErrorCode::NX2210 => CompileError::CannotInferType { span },
            ErrorCode::NX2220 => CompileError::TryRequiresResult {
                actual: ValueType::I32,
                span,
            },
            ErrorCode::NX2221 => CompileError::TryErrorMismatch {
                expected: ValueType::Bool,
                actual: ValueType::I32,
                span,
            },
            ErrorCode::NX2301 => CompileError::AwaitOutsideTask { span },
            ErrorCode::NX2302 => CompileError::MissingAwait { span },
            ErrorCode::NX2401 => CompileError::InvalidNumericConversion { span },
            ErrorCode::NX2501 => CompileError::InvalidFieldAccess {
                type_id: StableId::from_name("Record"),
                field: "missing".into(),
                span,
            },
            ErrorCode::NX2601 => CompileError::MigrationIntrinsicOutsideMigration {
                intrinsic: "old_get".into(),
                span,
            },
            ErrorCode::NX2602 => CompileError::MissingMigrationFinish {
                function_span: span,
            },
            ErrorCode::NX2603 => CompileError::MissingForwarding {
                stable_id: StableId::from_name("missing"),
                function_span: span,
            },
            ErrorCode::NX2604 => CompileError::DuplicateForwarding {
                stable_id: StableId::from_name("duplicate"),
                span,
            },
            _ => unreachable!("only compiler codes are routed here"),
        }
    }

    fn emit_fixture_error(code: ErrorCode, span: SourceSpan) -> NexaError {
        if code <= ErrorCode::NX2604 {
            return compile_error(code, span).into();
        }
        match code {
            ErrorCode::NX3001 => DecodeError::InvalidMagic.into(),
            ErrorCode::NX3002 => VerifyError {
                function: 0,
                instruction: Some(0),
                kind: VerifyErrorKind::RegisterOutOfRange(9),
            }
            .into(),
            ErrorCode::NX3003 => VerifyError {
                function: 0,
                instruction: Some(0),
                kind: VerifyErrorKind::InvalidRootMap(0),
            }
            .into(),
            ErrorCode::NX3004 => DecodeError::InvalidSourceMap.into(),
            ErrorCode::NX4001 => RealmError::HostHashMismatch.into(),
            ErrorCode::NX4002 => RealmError::HostCapabilitiesUnavailable.into(),
            ErrorCode::NX4003 => HostTrap::Arity.into(),
            ErrorCode::NX5001 => HostTrap::Panicked.into(),
            ErrorCode::NX5002 => HostError::Abandoned.into(),
            ErrorCode::NX5003 => HostError::UnknownHostErrorCode(77).into(),
            ErrorCode::NX5004 => RuntimeError::ResourceLimit("fixture").into(),
            ErrorCode::NX6001 => MigrationLimitError::Objects.into(),
            ErrorCode::NX6002 => ReloadError::GraphCheck.into(),
            ErrorCode::NX6003 => ReloadError::Activation(RuntimeMessage::Static("fixture")).into(),
            ErrorCode::NX6004 => ReloadError::CompletionBufferCapacity.into(),
            ErrorCode::NX6005 => VerifyError {
                function: 0,
                instruction: None,
                kind: VerifyErrorKind::InvalidReloadMetadata,
            }
            .into(),
            _ => unreachable!("stable code is covered"),
        }
    }

    #[test]
    fn diagnostic_corpus_check() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut emitted = std::collections::BTreeSet::new();
        for (index, definition) in ERROR_EMISSION_TABLE.iter().enumerate() {
            let source = std::fs::read_to_string(root.join(definition.fixture)).unwrap();
            assert!(source.contains(definition.code.as_str()));
            let span = SourceSpan::new(
                FileId(u32::try_from(index + 1).unwrap()),
                1,
                u32::try_from(source.len().max(2)).unwrap(),
            );
            let actual = emit_fixture_error(definition.code, span);
            assert_eq!(actual.code(), definition.code, "{}", definition.fixture);
            let expected_category = if definition.code <= ErrorCode::NX2604 {
                ErrorCategory::Diagnostic
            } else {
                actual.category()
            };
            assert_eq!(actual.category(), expected_category);
            let summary = definition.code.definition().unwrap().summary;
            let diagnostic = Diagnostic::from_parts(
                definition.code,
                Severity::Error,
                RuntimeMessage::Static(summary),
                Label {
                    span,
                    message: RuntimeMessage::Static("fixture source"),
                },
            );
            let human = diagnostic.to_string();
            let json: serde_json::Value =
                serde_json::from_str(&diagnostic.to_json().unwrap()).unwrap();
            assert!(human.contains(definition.code.as_str()));
            assert!(human.contains(summary));
            assert_eq!(json["code"], definition.code.as_str());
            assert_eq!(json["primary"]["start"], 1);
            assert!(json["primary"]["end"].as_u64().unwrap() > 1);
            emitted.insert(actual.code());
        }
        let registered = crate::ERROR_CODE_TABLE
            .iter()
            .map(|definition| definition.code)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(emitted, registered);
    }
}
