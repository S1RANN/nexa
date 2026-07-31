//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

mod diagnostic_corpus;
mod error;
mod package_build;
mod package_environment;
mod package_test;
mod runtime_diagnostics;

pub use diagnostic_corpus::{
    AnalysisDiagnosticReport, BinaryDiagnosticReport, CaseFormatReport, CompilerDiagnosticReport,
    DiagnosticCorpusReport, EngineDiagnosticReport, ObservedDiagnosticCase, PipelineReport,
    RuntimeDiagnosticReport, run_analysis_diagnostic_cases, run_binary_diagnostic_cases,
    run_compiler_diagnostic_cases, run_diagnostic_corpus, run_runtime_diagnostic_cases,
};
pub use error::{
    ClassifiedError, Diagnostic, DiagnosticCode, DiagnosticPhase, ERROR_CODE_TABLE,
    ERROR_EMISSION_TABLE, ErrorCategory, ErrorCode, ErrorCodeDefinition, ErrorContext,
    ErrorEmissionDefinition, ErrorMetadata, ErrorModuleEpoch, HostError, Label, MigrationError,
    NexaError, Severity,
};
pub use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprint, BuildFingerprintInput,
    CandidateIdentity, CompilationLimits, LinkedStateFingerprint, PackageId, PackageManifest,
    PackageSourceSet, PublicApiFingerprint, QueryDatabase, QueryStats, ResolvedBuildInput,
    ResolvedBuildInputError, ResolvedTestInput, SourceKey, SourceSetFingerprint,
    StateSchemaFingerprint,
};
pub use nexa_bytecode::{BYTECODE_VERSION, DecodeLimits, Module, SectionKind};
pub use nexa_compiler::{
    PackageDebugInfo, PackageFunctionDebugInfo, PackageHostImportDebugInfo, PackageModuleDebugInfo,
    PackageTestCallGraphNode, PackageTestInfo,
};
pub use nexa_core::{FileId, StableId};
pub use nexa_diagnostics::{
    ByteRange, Diagnostic as LeafDiagnostic, DiagnosticBatch, DiagnosticBatchLimits,
    DiagnosticRenderer as LeafDiagnosticRenderer, DroppedCounts, HUMAN_POSITION_ENCODING,
    Label as LeafLabel, LabelStyle, MACHINE_POSITION_ENCODING, RENDER_SCHEMA_VERSION,
    RelatedLocation as LeafRelatedLocation, RenderError as DiagnosticRenderError, SourceIdentity,
    SourceSnapshot, SourceSnapshotRegistry, SourceSnapshotRegistryBuilder,
    SourceSnapshotRegistryError, TextEditSuggestion,
};
pub use nexa_idl::{
    Idl, canonical, canonical as canonical_idl, exact_hash, exact_hash as exact_idl_hash,
    export_signature, export_stable_id, parse, parse as parse_idl,
};
pub use nexa_runtime::{
    CheckedInterpreter, HOST_CONTRACT_SCHEMA_VERSION, HostContract, MustCompletePolicy,
    RuntimeHostArgs, RuntimeLimits, RuntimeMessage, ScriptArgumentRequirements, ScriptCallError,
    ScriptCallStack, ScriptCallWriter, ScriptExport, ScriptFrame, ScriptFunction,
    ScriptOutputReader, StateObject, Trap,
};
pub use nexa_test_runner::{
    StackFrame, TestError, TestResult, TestRun, TestRunSummary, TestStatus,
};
pub use nexa_verifier::{VerifiedModule, VerifierLimits};
pub use package_build::{
    COMPILATION_OPTIONS_SCHEMA_VERSION, CompiledPackageArtifact, CompiledPackageTests,
    CompiledSource, HostContractInput, HostContractSource, HostContractSourceError,
    NEXA_COMPILER_VERSION, NEXA_LANGUAGE_VERSION, PackageArtifactIntegrityError,
    PackageBuildDurations, PackageBuildError, PackageBuildObservation, PackageBuildSession,
    PackageCheckReport, PackageCompilationEvidence, PackagePipelineStats, PackageSourceSnapshot,
    canonical_compilation_options, canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input,
    canonical_package_build_fingerprint_input_with_contract, check_package,
    check_package_with_contract, compile_package, compile_package_tests,
    compile_package_tests_with_contract, compile_package_with_contract, linked_state_fingerprint,
    verify_package_artifact_integrity,
};
pub use package_test::{
    PackageTestBackendSetupError, PackageTestDeclarationError, PackageTestDeclarationErrorReason,
    PackageTestEligibilityReason, PackageTestEligibilityViolation, PackageTestFunctionLocation,
    PackageTestOptions, PackageTestRunError,
};
pub use runtime_diagnostics::{
    MultiFileRuntimeDiagnosticEvidence, RuntimeDiagnosticCaseEvidence,
    RuntimeDiagnosticEndToEndReport, RuntimeDiagnosticHarness, run_runtime_diagnostic_end_to_end,
};

/// Compiles source through the stable facade error boundary.
pub fn compile(source: &str) -> Result<VerifiedModule, NexaError> {
    compile_file(source, FileId::default())
}

/// Compiles source and attaches `file` to diagnostics that have a source location.
pub fn compile_file(source: &str, file: FileId) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile_file(source, file)
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, file))))
}

/// Compiles source against an exact IDL interface through the stable facade error boundary.
pub fn compile_with_interface(source: &str, interface: &Idl) -> Result<VerifiedModule, NexaError> {
    compile_with_interface_file(source, FileId::default(), interface)
}

/// Compiles source against an exact IDL interface while preserving its real file identity.
pub fn compile_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile_with_interface_file(source, file, interface)
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, file))))
}

/// Compiles through lowering but deliberately leaves structural verification to the caller.
pub fn compile_module_with_interface_file(
    source: &str,
    file: FileId,
    interface: &Idl,
) -> Result<Module, NexaError> {
    nexa_compiler::compile_module_with_interface_file(source, file, interface)
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
        AbandonPolicy, ArrayType, AsyncResultType, BYTECODE_VERSION, BufferType, CancelPolicy,
        ClassType, DecodeLimits, EnumType, EnumVariant, FunctionBuilder, FunctionEffect,
        HostCallMode, HostImport, Instruction, MapType, MigrationLimitRequirements, ModuleBuilder,
        ReloadMetadata, SectionKind, Signature, SnapshotType, SourceMapEntry, StateField,
        StateHandleType, StateSchema, StateType, StructField, StructType, ValueType, option_type,
        result_type,
    };
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::generate_rust as generate_rust_bindings;
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
        ByteRange, DiagnosticBatch, DiagnosticBatchLimits, DiagnosticPhase, LeafDiagnostic,
        LeafDiagnosticRenderer, LeafLabel, LeafRelatedLocation, SourceIdentity, SourceSnapshot,
        SourceSnapshotRegistry, TextEditSuggestion,
    };
    pub use crate::{
        CheckedInterpreter, ClassifiedError, CompiledPackageArtifact, CompiledPackageTests,
        CompiledSource, Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ErrorCategory, ErrorCode,
        ErrorCodeDefinition, ErrorContext, ErrorMetadata, ErrorModuleEpoch,
        HOST_CONTRACT_SCHEMA_VERSION, HostContract, HostContractInput, HostContractSource,
        HostContractSourceError, HostError, Idl, Label, MigrationError, MustCompletePolicy,
        NexaError, PackageArtifactIntegrityError, PackageBuildDurations, PackageBuildError,
        PackageBuildObservation, PackageBuildSession, PackageDebugInfo, PackageFunctionDebugInfo,
        PackageHostImportDebugInfo, PackageModuleDebugInfo, PackageSourceSnapshot,
        PackageTestEligibilityReason, PackageTestEligibilityViolation, PackageTestFunctionLocation,
        PackageTestOptions, PackageTestRunError, QueryDatabase, QueryStats, ResolvedBuildInput,
        ResolvedTestInput, RuntimeHostArgs, RuntimeLimits, RuntimeMessage,
        ScriptArgumentRequirements, ScriptCallError, ScriptCallStack, ScriptCallWriter,
        ScriptExport, ScriptFrame, ScriptOutputReader, Severity, StableId, StackFrame, StateObject,
        TestError, TestResult, TestRun, TestRunSummary, TestStatus, Trap, VerifiedModule,
        canonical, canonical_compilation_options, canonical_idl,
        canonical_package_build_fingerprint_input, check_package, check_package_with_contract,
        compile, compile_file, compile_module_with_interface_file, compile_package,
        compile_package_tests, compile_package_tests_with_contract, compile_package_with_contract,
        compile_with_interface, compile_with_interface_file, decode_module, exact_hash,
        exact_idl_hash, export_signature, export_stable_id, parse, parse_idl, verify_module,
        verify_package_artifact_integrity,
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn compile_file_preserves_origin_for_non_lex_diagnostics() {
        let file = crate::FileId(41);
        let error = crate::compile_file("fn main() -> i32 { return missing; }", file).unwrap_err();
        let crate::NexaError::Diagnostic(diagnostic) = error else {
            panic!("unknown name must remain a source diagnostic");
        };

        assert_eq!(diagnostic.code, crate::ErrorCode::NX2001);
        assert_eq!(diagnostic.phase(), Some(crate::DiagnosticPhase::Resolve));
        assert_eq!(
            diagnostic.primary.as_ref().map(|label| label.span.file),
            Some(file)
        );
    }

    #[test]
    fn compile_file_preserves_lexical_class_and_origin() {
        let file = crate::FileId(42);
        let error = crate::compile_file("#", file).unwrap_err();
        let crate::NexaError::Diagnostic(diagnostic) = error else {
            panic!("unexpected character must remain a source diagnostic");
        };

        assert_eq!(diagnostic.code, crate::ErrorCode::NX1001);
        assert_eq!(diagnostic.phase(), Some(crate::DiagnosticPhase::Lex));
        assert_eq!(
            diagnostic.primary.as_ref().map(|label| label.span),
            Some(nexa_core::SourceSpan::new(file, 0, 1))
        );
    }

    #[test]
    fn compiler_diagnostic_corpus_uses_real_sources_and_exact_spans() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_compiler_diagnostic_cases(&root).unwrap();
        assert!(report.case_count > 0);
        assert_eq!(report.deterministic_cases, report.case_count);
        assert_eq!(report.source_backed_zero_zero_spans, 0);
        assert_eq!(
            report.source_backed_inexact_spans, 0,
            "inexact diagnostics: {:#?}",
            report.cases
        );
        assert!(report.cases.iter().all(|case| {
            case.passed
                && case.human_output
                && case.json_output
                && case.primary_start < case.primary_end
        }));
    }

    #[test]
    fn analysis_diagnostic_corpus_uses_real_packages_and_exact_spans() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_analysis_diagnostic_cases(&root).unwrap();
        assert!(report.case_count > 0);
        assert_eq!(report.deterministic_cases, report.case_count);
        assert_eq!(report.source_backed_zero_zero_spans, 0);
        assert_eq!(
            report.source_backed_inexact_spans, 0,
            "inexact diagnostics: {:#?}",
            report.cases
        );
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
        assert!(report.case_count > 0);
        assert_eq!(report.passed, report.case_count);
        assert_eq!(report.deterministic_cases, report.case_count);
        assert!(
            report
                .cases
                .iter()
                .all(|case| case.human_output && case.json_output)
        );
    }

    #[test]
    fn runtime_diagnostic_corpus_calls_real_runtime_host_and_reload_apis() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = crate::run_runtime_diagnostic_cases(&root).unwrap();
        assert!(report.case_count > 0);
        assert_eq!(report.passed, report.case_count);
        assert_eq!(report.deterministic_cases, report.case_count);
        assert!(!report.direct_nexa_error_construction);
        let evidence = &report.multi_file_source_evidence;
        assert!(evidence.passed && evidence.deterministic);
        assert_eq!(evidence.stack_functions, ["crash", "forward", "entry"]);
        assert_eq!(evidence.stack_sources.len(), 3);
        assert_eq!(
            evidence
                .file_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert!(evidence.true_call_site_pcs);
        assert!(evidence.true_host_call_boundary);
        assert!(evidence.nidl_binding_verified);
        assert!(evidence.nidl_exact_source_preserved);
        assert!(evidence.crlf_preserved && evidence.astral_utf16_verified);
        assert!(
            std::path::Path::new(&evidence.nidl_origin)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("nidl"))
        );
    }
}
