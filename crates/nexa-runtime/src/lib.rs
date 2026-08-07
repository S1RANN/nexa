//! Model-driven Nexa runtime primitives.

mod allocation;
mod executable;
mod failure;
mod frame;
mod heap;
mod host;
mod interpreter;
mod kernel;
mod ledger;
mod message;
mod metrics;
#[cfg(any(test, feature = "model-adapter"))]
pub mod model_adapter;
pub mod profiler;
mod realm;
mod reload;
mod scheduler;
mod scope;
mod slot_pool;
mod stateful;
mod task;
mod trace;
mod trusted;

#[allow(dead_code, clippy::all, clippy::pedantic)]
#[path = "generated/machines.rs"]
mod machines;

pub use allocation::{
    AllocationBoundary, AllocationSnapshot, MigrationAllocationPhase, allocation_snapshot,
    set_migration_allocation_observer,
};
pub use executable::{
    ExecutableBuildError, ExecutableFunction, ExecutableInstruction, ExecutableModule,
};
pub use failure::{
    FailureObservation, FailurePointStats, FailureProbe, FailureProbeState,
    RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint,
};
pub use kernel::{RuntimeError, RuntimeLimits, RuntimeTrap, StepConfig, TaskLimits, TaskRuntime};
pub use ledger::RuntimeResourceLedger;
pub use message::{DiagnosticCode, InlineMessage, RuntimeMessage};
pub use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, FunctionEffect, HostCallMode, Signature,
    ValueType,
};
pub use nexa_core::{
    CANONICAL_NAN_F32_BITS, CANONICAL_NAN_F64_BITS, CANONICAL_NAN_POLICY_VERSION,
    OPCODE_COST_TABLE_VERSION, RUNTIME_LIBM_VERSION, RUNTIME_MATH_BACKEND_ID,
    RUNTIME_SEMANTICS_VERSION, StableId,
};

#[cfg(test)]
mod micro;
pub use frame::{
    ContinuationReservation, DeferAction, Frame, FrameArena, FrameError, FrameLimits,
    MigrationOldObjectHandle, MigrationStagingObjectHandle, ReturnRange, RuntimeValue,
};
pub use heap::{
    CollectionArena, CollectionArenaInspection, CollectionRange, CollectionReservation,
    CollectionStats, CollectionStorage, CollectionView, GcBudget, GcCycleTelemetry, GcPhase, GcRef,
    GcRoots, Heap, HeapByteInspection, HeapError, IncrementalGcReport, MapSetOutcome, Object,
    SetInsertOutcome, VmAllocationCounters,
};
pub use host::{
    CompletionAccounting, CopyBuffer, DecodeTypedSnapshot, EncodeHostReturn, EncodedSnapshot,
    HOST_CONTRACT_SCHEMA_VERSION, HostArrayRef, HostBufferRef, HostCallOutcome, HostClassRef,
    HostCollectionBuilder, HostCompletionDelivery, HostCompletionProtocolError,
    HostCompletionResult, HostCompletionTicket, HostContract, HostEnumRef, HostErrorPayload,
    HostFunctionAuthority, HostFunctionSlot, HostMapEntryRef, HostMapRef, HostOptionRef,
    HostPayload, HostRegistry, HostRequestError, HostRequestHandle, HostRequestState,
    HostResultRef, HostReturnRequirements, HostReturnTransaction, HostStr, HostStructRef, HostTrap,
    HostValueRef, MAX_HOST_RETURN_FIELDS, MAX_SCRIPT_ARGUMENTS, MustCompletePolicy,
    PendingHostRequest, RELEASE_DOMAIN_COUNT, ReleaseKind, ReleaseQueue, ReleaseQueueError,
    ReleaseQueueState, ReleaseRecord, RequestTerminalRecord, ResolvedHostFunction, ResourceContext,
    ResourceTokenHandle, RuntimeHost, RuntimeHostArgs, RuntimeHostCloseError,
    RuntimeHostCloseStatus, RuntimeHostDomain, RuntimeHostState, RuntimeResourceSnapshot,
    RuntimeResources, ScriptArgumentRequirements, ScriptArguments, ScriptCallError,
    ScriptCallWriter, ScriptExport, ScriptOutputReader, ScriptSignature, SnapshotHandle,
    SnapshotLayout, TaskResourceSet, TypedSnapshotRef, contract_runtime_id_from_fingerprint,
    invoke_host_boundary, validate_host_completion,
};
#[cfg(feature = "fuzzing")]
pub use host::{fuzz_completion_ticket_terminal_race, fuzz_release_intrusive_list};
pub use interpreter::{
    CheckedInterpreter, ExecutionCharge, FuelState, HostCallBoundary, InterpreterContinuation,
    InterpreterError, InterpreterHost, InterpreterHostArguments, InterpreterHostOutcome,
    InterpreterOutcome, InterpreterState, MAX_SCRIPT_CALL_STACK_DEPTH, OpcodeCostTable,
    ScriptCallStack, ScriptFrame, StaticLeafOutcome, SuspendReason, Trap, TrapKind,
};
pub use metrics::ExecutionMetrics;
pub use profiler::{
    AllocationKind, AllocationProfile, AllocationSiteId, DroppedProfile, FunctionIdentity,
    FunctionProfile, GcProfile, HostCallProfile, OpcodeProfile, PROFILER_FUNCTION_CAPACITY,
    PROFILER_HOST_CALL_CAPACITY, PROFILER_MODULE_CAPACITY, PROFILER_SCHEMA_VERSION,
    PROFILER_SITE_CAPACITY, PerformanceProfile, TaskProfile,
};
pub use realm::{
    CancelReason, CompletionDisposition, ExecutionImageCacheInspection, GcTriggerInspection,
    GcTriggerReasons, HostFunctionAuthorityField, HostImportPlanCacheInspection, HostResult,
    ModuleHandle, ModuleLifecycle, NexaValue, PreparedScriptExport, RealmConfig, RealmError,
    RealmRuntime, ReloadAccounting, RestartReloadMetrics, RestartReloadOutcome,
    RestartReloadPolicy, RestartReloadResult, RuntimeCapacityReport, SchedulerFastPathInspection,
    StagedCellTransaction, TaskPoll, TaskTerminalReason, TaskTerminalRecord, TickBudget,
    TickReport, TransactionalCellCommit, TransactionalCellEntrypoint, TransactionalCellFailure,
    TransactionalCellFailureCause, TransactionalCellPoll, TransactionalCellRollback, YieldReason,
};
#[cfg(any(test, feature = "model-adapter"))]
pub use realm::{
    HeapInspection, ModuleInspection, RealmInspectionSnapshot, ReloadInspection,
    ReloadInspectionState, RootInspection, SchedulerInspection, TaskExecutionInspection,
    TaskInspection, TerminalTaskInspection,
};
pub use reload::{ReloadError, invoke_reload_activation};
pub use scope::{ScopeError, ScopeHandle, ScopeSnapshot, ScopeState};
pub use slot_pool::{HandleError, SlotAllocError, SlotPool};
pub use stateful::{
    MigrationCapacityReport, MigrationLimitError, MigrationLimits, MigrationUsageReport,
    OfflineMigrationError, OfflineMigrationResult, OfflineStateField, OfflineStateObject,
    OfflineStateValue, StateHandle, StateHandleError, StateObject, StateValue, StatefulDomainId,
    StatefulError, StatefulRegistry, run_offline_migration,
};
#[cfg(feature = "fuzzing")]
pub use stateful::{fuzz_migration_arena, fuzz_stateful_registry};
pub use task::{TaskError, TaskHandle, TaskSnapshot, TaskState};
pub use trace::{RuntimeTrace, TraceRecords};

#[cfg(test)]
mod fingerprint_authority_tests {
    use nexa_core::{CANONICAL_NAN_POLICY_VERSION, RUNTIME_LIBM_VERSION};

    #[test]
    fn public_constants_reexport_core_and_drive_the_opcode_cost_table() {
        assert_eq!(
            super::RUNTIME_SEMANTICS_VERSION,
            nexa_core::RUNTIME_SEMANTICS_VERSION
        );
        assert_eq!(
            super::OPCODE_COST_TABLE_VERSION,
            nexa_core::OPCODE_COST_TABLE_VERSION
        );
        assert_eq!(
            super::RUNTIME_MATH_BACKEND_ID,
            nexa_core::RUNTIME_MATH_BACKEND_ID
        );
        assert_eq!(
            super::OpcodeCostTable::default().version,
            nexa_core::OPCODE_COST_TABLE_VERSION
        );
    }

    #[test]
    fn math_backend_identity_matches_the_exact_workspace_libm_pin() {
        let expected_identity = format!(
            "pure-rust-libm-{RUNTIME_LIBM_VERSION}-canonical-nan-v{CANONICAL_NAN_POLICY_VERSION}"
        );
        assert_eq!(super::RUNTIME_MATH_BACKEND_ID, expected_identity);

        let workspace_manifest = include_str!("../../../Cargo.toml");
        let exact_pin = format!(r#"libm = "={RUNTIME_LIBM_VERSION}""#);
        assert!(
            workspace_manifest
                .lines()
                .any(|line| line.trim() == exact_pin),
            "workspace libm dependency must stay exactly pinned to the version in the runtime math identity"
        );
        assert!(
            include_str!("../../nexa-core/Cargo.toml")
                .lines()
                .any(|line| line.trim() == "libm.workspace = true"),
            "nexa-core must consume the workspace libm authority"
        );
        assert!(
            include_str!("../Cargo.toml")
                .lines()
                .any(|line| line.trim() == r#"nexa-core = { path = "../nexa-core" }"#),
            "nexa-runtime must consume deterministic math through nexa-core"
        );
    }
}
