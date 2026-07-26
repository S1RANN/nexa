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
    RuntimeFailureConfigError, RuntimeFailureInjector, RuntimeFailureMode, RuntimeFailurePoint,
};
pub use kernel::{RuntimeError, RuntimeLimits, StepConfig, TaskLimits, TaskRuntime};
pub use ledger::RuntimeResourceLedger;
pub use message::{DiagnosticCode, InlineMessage, RuntimeMessage};
pub use nexa_core::StableId;

#[cfg(test)]
mod micro;
pub use frame::{
    ContinuationReservation, DeferAction, Frame, FrameArena, FrameError, FrameLimits, RuntimeValue,
};
pub use heap::{CollectionStats, GcRef, GcRoots, Heap, HeapError, Object};
pub use host::{
    CompletionAccounting, CopyBuffer, HostArgs, HostCallOutcome, HostCompletionDelivery,
    HostCompletionResult, HostCompletionTicket, HostErrorPayload, HostPayload, HostRegistry,
    HostRequestError, HostRequestHandle, HostRequestState, HostTrap, HostValue, PendingHostRequest,
    RELEASE_DOMAIN_COUNT, ReleaseKind, ReleaseQueue, ReleaseQueueError, ReleaseQueueState,
    ReleaseRecord, RequestTerminalRecord, ResourceContext, ResourceTokenHandle, RuntimeHost,
    RuntimeHostCloseError, RuntimeHostCloseStatus, RuntimeHostDomain, RuntimeHostState,
    RuntimeResourceSnapshot, RuntimeResources, ScriptFunction, SnapshotHandle, TaskResourceSet,
};
#[cfg(feature = "fuzzing")]
pub use host::{fuzz_completion_ticket_terminal_race, fuzz_release_intrusive_list};
pub use interpreter::{
    CheckedInterpreter, ExecutionCharge, FuelState, InterpreterContinuation, InterpreterError,
    InterpreterHost, InterpreterHostOutcome, InterpreterOutcome, InterpreterState, OpcodeCostTable,
    SuspendReason, Trap, TrapKind,
};
pub use metrics::ExecutionMetrics;
pub use realm::{
    CancelReason, CompletionRoute, ModuleEpochKey, ModuleEpochRoot, ModuleHandle, ModuleLifecycle,
    PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime, ReloadCompletionStats,
    RetiredEpochSnapshot, RetiredEpochState, RootPublicationRecord, RuntimeCapacityReport,
    TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
};
pub use reload::ReloadError;
pub use scope::{ScopeError, ScopeHandle, ScopeSnapshot, ScopeState};
pub use slot_pool::{HandleError, SlotAllocError, SlotPool};
pub use stateful::{
    MigrationCapacityReport, MigrationLimitError, MigrationLimits, MigrationUsageReport,
    StateHandle, StateHandleError, StateObject, StateValue, StatefulDomainId, StatefulError,
};
#[cfg(feature = "fuzzing")]
pub use stateful::{fuzz_migration_arena, fuzz_stateful_registry};
pub use task::{TaskError, TaskHandle, TaskSnapshot, TaskState};
pub use trace::{RuntimeTrace, TraceRecords};
