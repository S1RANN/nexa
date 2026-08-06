//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

mod build_profile;
mod diagnostic_corpus;
mod error;
mod package_build;
mod package_environment;
mod package_inspection;
mod package_test;
mod repl_session;
mod runtime_diagnostics;

pub use build_profile::BuildProfile;
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
    PackageMainInfo, PackageReplCellInfo, PackageReplStateFieldInfo, STANDALONE_MAIN_STABLE_ID,
    standalone_main_stable_id,
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
pub use nexa_contract::{
    ABI_DESCRIPTOR_VERSION, AbiDescriptor, AbiFingerprint, BindingModel, CodegenError,
    EffectiveContractDescriptor, EffectiveContractSelection, EffectiveDescriptorError,
    CONTRACT_SYNTAX_VERSION, ContractAst, ContractError, ContractErrorKind, ValidatedContract, ValidatedFunction,
    abi_descriptor, contract_fingerprint, contract_runtime_id, effective_contract_descriptor,
    effective_contract_fingerprint, entrypoint_signature, entrypoint_stable_id,
    generate_rust as generate_rust_bindings, host_function_signature, parse as parse_contract,
    parse_ast as parse_contract_ast,
};
pub use nexa_runtime::profiler;
pub use nexa_runtime::{
    AllocationKind, AllocationProfile, AllocationSiteId, CheckedInterpreter, DroppedProfile,
    FunctionIdentity, FunctionProfile, GcProfile, HOST_CONTRACT_SCHEMA_VERSION, HostCallProfile,
    HostContract, HostFunctionAuthority, HostFunctionAuthorityField, MustCompletePolicy,
    OpcodeProfile, PerformanceProfile, PreparedScriptExport, RuntimeHostArgs, RuntimeLimits,
    RuntimeMessage, ScriptArgumentRequirements, ScriptArguments, ScriptCallError, ScriptCallStack,
    ScriptCallWriter, ScriptExport, ScriptFrame, ScriptOutputReader, ScriptSignature, StateObject,
    TaskProfile, Trap,
};
pub use nexa_test_runner::{
    StackFrame, TestError, TestResult, TestRun, TestRunSummary, TestStatus,
};
pub use nexa_verifier::{VerifiedModule, VerifierLimits};
pub use package_build::{
    COMPILATION_OPTIONS_SCHEMA_VERSION, CompiledPackageArtifact, CompiledPackageTests,
    CompiledReplCellArtifact, CompiledSource, CompiledStandaloneArtifact, HostContractInput,
    HostContractSource, HostContractSourceError, NEXA_COMPILER_VERSION, NEXA_LANGUAGE_VERSION,
    PackageArtifactIntegrityError, PackageBuildDurations, PackageBuildError,
    PackageBuildObservation, PackageBuildSession, PackageCheckReport, PackageCompilationEvidence,
    PackagePipelineStats, PackageSourceSnapshot, canonical_compilation_options,
    canonical_compilation_options_for_profile, canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input_with_contract,
    canonical_package_build_fingerprint_input_with_contract_for_profile,
    check_package_with_contract, compile_package_tests_with_contract,
    compile_package_with_contract, compile_standalone_package_with_contract,
    compile_standalone_with_contract,
};
pub use package_inspection::{
    PackageDebugInspection, PackageFunctionInspection, PackageHostImportInspection,
    PackageModuleInspection, PackageSymbolVisibility,
};
pub use package_test::{
    PackageTestBackendSetupError, PackageTestDeclarationError, PackageTestDeclarationErrorReason,
    PackageTestEligibilityReason, PackageTestEligibilityViolation, PackageTestFunctionLocation,
    PackageTestOptions, PackageTestRunError,
};
pub use repl_session::{
    CONSOLE_HOST_CONTRACT, CONSOLE_HOST_SOURCE_IDENTITY, ReplCellOutcome, ReplConsoleEmission,
    ReplConsoleHost, ReplConsoleHostError, ReplConsoleStream, ReplGcReport, ReplMemoryReport,
    ReplResolvedCellInput, ReplSession, ReplSessionError, ReplSessionLimits,
};
pub use runtime_diagnostics::{
    MultiFileRuntimeDiagnosticEvidence, RuntimeDiagnosticCaseEvidence,
    RuntimeDiagnosticEndToEndReport, RuntimeDiagnosticHarness, run_runtime_diagnostic_end_to_end,
};

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
    pub use nexa_core::{FileId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_contract::generate_rust as generate_rust_bindings;
    pub use nexa_runtime::{
        CancelReason, CompletionAccounting, HostCallOutcome, HostCompletionResult,
        HostCompletionTicket, HostErrorPayload, HostFunctionAuthority, HostFunctionAuthorityField,
        HostFunctionSlot, HostPayload, HostRegistry, HostRequestHandle, HostTrap,
        MigrationCapacityReport, MigrationLimitError, MigrationLimits, MigrationUsageReport,
        ModuleHandle, PendingHostRequest, RealmConfig, RealmError, RealmRuntime,
        ResolvedHostFunction, ResourceContext, ResourceTokenHandle, RestartReloadMetrics,
        RestartReloadOutcome, RestartReloadPolicy, RestartReloadResult, RuntimeFailureConfigError,
        RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint, RuntimeHost,
        RuntimeHostCloseError, RuntimeHostCloseStatus, RuntimeHostDomain, RuntimeHostState,
        RuntimeResourceLedger, RuntimeValue, ScopeHandle, ScopeSnapshot, SnapshotHandle,
        StateHandle, StateHandleError, StateValue, StatefulDomainId, StepConfig, TaskHandle,
        TaskLimits, TaskPoll, TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
        YieldReason,
    };
    pub use nexa_verifier::VerifierLimits;

    pub use crate::{
        BuildProfile, CheckedInterpreter, ClassifiedError, CompiledPackageArtifact,
        CompiledPackageTests, CompiledReplCellArtifact, CompiledSource, CompiledStandaloneArtifact,
        Diagnostic, DiagnosticCode, ERROR_CODE_TABLE, ErrorCategory, ErrorCode,
        ErrorCodeDefinition, ErrorContext, ErrorMetadata, ErrorModuleEpoch,
        HOST_CONTRACT_SCHEMA_VERSION, HostContract, HostContractInput, HostContractSource,
        HostContractSourceError, HostError, Label, MigrationError, MustCompletePolicy, NexaError,
        PackageArtifactIntegrityError, PackageBuildDurations, PackageBuildError,
        PackageBuildObservation, PackageBuildSession, PackageDebugInspection,
        PackageFunctionInspection, PackageHostImportInspection, PackageMainInfo,
        PackageModuleInspection, PackageSourceSnapshot, PackageSymbolVisibility,
        PackageTestEligibilityReason, PackageTestEligibilityViolation, PackageTestFunctionLocation,
        PackageTestOptions, PackageTestRunError, PreparedScriptExport, QueryDatabase, QueryStats,
        ReplCellOutcome, ReplConsoleEmission, ReplConsoleHost, ReplConsoleHostError,
        ReplConsoleStream, ReplGcReport, ReplMemoryReport, ReplResolvedCellInput, ReplSession,
        ReplSessionError, ReplSessionLimits, ResolvedBuildInput, ResolvedTestInput,
        RuntimeHostArgs, RuntimeLimits, RuntimeMessage, ScriptArgumentRequirements,
        ScriptArguments, ScriptCallError, ScriptCallStack, ScriptCallWriter, ScriptExport,
        ScriptFrame, ScriptOutputReader, ScriptSignature, Severity, StableId, StackFrame,
        StateObject, TestError, TestResult, TestRun, TestRunSummary, TestStatus, Trap,
        ValidatedContract, VerifiedModule, abi_descriptor, canonical_compilation_options,
        canonical_package_build_fingerprint_input_with_contract, check_package_with_contract,
        compile_package_tests_with_contract, compile_package_with_contract,
        compile_standalone_package_with_contract, compile_standalone_with_contract,
        contract_fingerprint, contract_runtime_id, decode_module, effective_contract_descriptor,
        entrypoint_signature, entrypoint_stable_id, parse_contract, parse_contract_ast,
        standalone_main_stable_id, verify_module,
    };
    pub use crate::{
        ByteRange, DiagnosticBatch, DiagnosticBatchLimits, DiagnosticPhase, LeafDiagnostic,
        LeafDiagnosticRenderer, LeafLabel, LeafRelatedLocation, SourceIdentity, SourceSnapshot,
        SourceSnapshotRegistry, TextEditSuggestion,
    };
}

#[cfg(test)]
mod tests {
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
