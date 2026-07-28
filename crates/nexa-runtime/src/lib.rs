//! Model-driven Nexa runtime primitives.

mod allocation;
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
mod realm;
mod reload;
mod scheduler;
mod scope;
mod slot_pool;
mod stateful;
mod task;
mod trace;

#[allow(dead_code, clippy::all, clippy::pedantic)]
#[path = "generated/machines.rs"]
mod machines;

pub use allocation::{
    AllocationBoundary, AllocationSnapshot, MigrationAllocationPhase, allocation_snapshot,
    set_migration_allocation_observer,
};
pub use failure::{
    FailureObservation, FailurePointStats, FailureProbe, FailureProbeState,
    RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint,
};
pub use kernel::{RuntimeError, RuntimeLimits, RuntimeTrap, StepConfig, TaskLimits, TaskRuntime};
pub use ledger::RuntimeResourceLedger;
pub use message::{DiagnosticCode, InlineMessage, RuntimeMessage};
pub use nexa_bytecode::ValueType;
pub use nexa_core::StableId;

#[cfg(test)]
mod micro;
pub use frame::{
    ContinuationReservation, DeferAction, Frame, FrameArena, FrameError, FrameLimits, RuntimeValue,
};
pub use heap::{
    CollectionArena, CollectionArenaInspection, CollectionRange, CollectionReservation,
    CollectionStats, GcRef, GcRoots, Heap, HeapError, MapSetOutcome, Object,
};
pub use host::{
    CompletionAccounting, CopyBuffer, DecodeTypedSnapshot, EncodeHostReturn, EncodedSnapshot,
    HostArrayRef, HostBufferRef, HostCallOutcome, HostCollectionBuilder, HostCompletionDelivery,
    HostCompletionProtocolError, HostCompletionResult, HostCompletionTicket, HostEnumRef,
    HostErrorPayload, HostOptionRef, HostPayload, HostRegistry, HostRequestError,
    HostRequestHandle, HostRequestState, HostResultRef, HostReturnRequirements,
    HostReturnTransaction, HostStr, HostStructRef, HostTrap, HostValueRef, MAX_HOST_RETURN_FIELDS,
    PendingHostRequest, RELEASE_DOMAIN_COUNT, ReleaseKind, ReleaseQueue, ReleaseQueueError,
    ReleaseQueueState, ReleaseRecord, RequestTerminalRecord, ResourceContext, ResourceTokenHandle,
    RuntimeHost, RuntimeHostArgs, RuntimeHostCloseError, RuntimeHostCloseStatus, RuntimeHostDomain,
    RuntimeHostState, RuntimeResourceSnapshot, RuntimeResources, ScriptFunction, SnapshotHandle,
    SnapshotLayout, TaskResourceSet, TypedSnapshotRef, invoke_host_boundary,
    validate_host_completion,
};
#[cfg(feature = "fuzzing")]
pub use host::{fuzz_completion_ticket_terminal_race, fuzz_release_intrusive_list};
pub use interpreter::{
    CheckedInterpreter, ExecutionCharge, FuelState, HostCallBoundary, InterpreterContinuation,
    InterpreterError, InterpreterHost, InterpreterHostOutcome, InterpreterOutcome,
    InterpreterState, MAX_SCRIPT_CALL_STACK_DEPTH, OpcodeCostTable, ScriptCallStack, ScriptFrame,
    SuspendReason, Trap, TrapKind,
};
pub use metrics::ExecutionMetrics;
pub use realm::{
    CancelReason, CompletionDisposition, HostResult, ModuleHandle, ModuleLifecycle, NexaValue,
    PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime, RestartReloadOutcome,
    RestartReloadPolicy, RuntimeCapacityReport, TaskPoll, TaskTerminalReason, TaskTerminalRecord,
    TickBudget, TickReport, YieldReason,
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
