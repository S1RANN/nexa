//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

mod diagnostic_corpus;
mod error;
mod runtime_diagnostics;

pub use diagnostic_corpus::{
    BinaryDiagnosticReport, CaseFormatReport, CompilerDiagnosticReport, DiagnosticCorpusReport,
    EngineDiagnosticReport, ObservedDiagnosticCase, PipelineReport, RuntimeDiagnosticReport,
    run_binary_diagnostic_cases, run_compiler_diagnostic_cases, run_diagnostic_corpus,
    run_runtime_diagnostic_cases,
};
pub use error::{
    ClassifiedError, Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ERROR_EMISSION_TABLE,
    ErrorCategory, ErrorCode, ErrorCodeDefinition, ErrorContext, ErrorEmissionDefinition,
    ErrorMetadata, ErrorModuleEpoch, HostError, Label, MigrationError, NexaError, Severity,
};
pub use runtime_diagnostics::{
    RuntimeDiagnosticCaseEvidence, RuntimeDiagnosticEndToEndReport, RuntimeDiagnosticHarness,
    run_runtime_diagnostic_end_to_end,
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
    compile_with_interface_file(source, FileId::default(), interface, schema_hash)
}

/// Compiles source against an exact IDL interface while preserving its real file identity.
pub fn compile_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile_with_interface_file(source, file, interface, schema_hash)
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, file))))
}

/// Compiles through lowering but deliberately leaves structural verification to the caller.
pub fn compile_module_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<Module, NexaError> {
    nexa_compiler::compile_module_with_interface_file(source, file, interface, schema_hash)
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, file))))
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
        CancelReason, CompletionAccounting, HostCallOutcome, HostCompletionResult,
        HostCompletionTicket, HostErrorPayload, HostPayload, HostRegistry, HostRequestHandle,
        HostTrap, MigrationCapacityReport, MigrationLimitError, MigrationLimits,
        MigrationUsageReport, ModuleHandle, PendingHostRequest, RealmConfig, RealmError,
        RealmRuntime, ResourceContext, ResourceTokenHandle, RestartReloadMetrics,
        RestartReloadOutcome, RestartReloadPolicy, RestartReloadResult, RuntimeFailureConfigError,
        RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint, RuntimeHost,
        RuntimeHostCloseError, RuntimeHostCloseStatus, RuntimeHostDomain, RuntimeHostState,
        RuntimeResourceLedger, RuntimeValue, ScopeHandle, ScopeSnapshot, ScriptFunction,
        SnapshotHandle, StateHandle, StateHandleError, StateValue, StatefulDomainId, StepConfig,
        TaskHandle, TaskLimits, TaskPoll, TaskTerminalReason, TaskTerminalRecord, TickBudget,
        TickReport, YieldReason,
    };
    pub use nexa_verifier::VerifierLimits;

    pub use crate::{
        ClassifiedError, Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ErrorCategory, ErrorCode,
        ErrorCodeDefinition, ErrorContext, ErrorMetadata, ErrorModuleEpoch, HostError, Label,
        MigrationError, NexaError, Severity, compile, compile_file,
        compile_module_with_interface_file, compile_with_interface, compile_with_interface_file,
        decode_module, verify_module,
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn compiler_diagnostic_corpus_uses_real_sources_and_exact_spans() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_compiler_diagnostic_cases(&root).unwrap();
        assert_eq!(report.case_count, 18);
        assert_eq!(report.deterministic_cases, 18);
        assert_eq!(report.source_backed_zero_zero_spans, 0);
        assert_eq!(report.source_backed_inexact_spans, 0);
        assert!(report.cases.iter().all(|case| {
            case.passed
                && case.human_output
                && case.json_output
                && case.primary_start < case.primary_end
        }));
    }

    #[test]
    fn binary_diagnostic_corpus_decodes_and_verifies_real_fixtures() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_binary_diagnostic_cases(&root).unwrap();
        assert_eq!(report.case_count, 5);
        assert_eq!(report.passed, 5);
        assert_eq!(report.deterministic_cases, 5);
    }

    #[test]
    fn runtime_diagnostic_corpus_calls_real_runtime_host_and_reload_apis() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_runtime_diagnostic_cases(&root).unwrap();
        assert_eq!(report.case_count, 10);
        assert_eq!(report.passed, 10);
        assert_eq!(report.deterministic_cases, 10);
        assert!(!report.direct_nexa_error_construction);
    }
}
