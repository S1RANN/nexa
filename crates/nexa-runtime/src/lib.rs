//! Model-driven Nexa runtime primitives.

mod allocation;
mod frame;
mod heap;
mod host;
mod interpreter;
mod kernel;
mod metrics;
mod realm;
mod reload;
mod scheduler;
mod scope;
mod slot_pool;
mod task;
mod trace;

#[allow(dead_code, clippy::all, clippy::pedantic)]
#[path = "generated/machines.rs"]
mod machines;

pub use allocation::{AllocationSnapshot, allocation_snapshot};
pub use kernel::{FailurePoint, RuntimeError, RuntimeLimits, StepConfig, TaskLimits, TaskRuntime};
pub use nexa_core::StableId;

#[cfg(test)]
mod micro;
pub use frame::{
    ContinuationReservation, DeferAction, Frame, FrameArena, FrameError, FrameLimits, RuntimeValue,
};
pub use heap::{CollectionStats, GcRef, GcRoots, Heap, HeapError, Object};
pub use host::{
    CopyBuffer, HostArgs, HostCallOutcome, HostCompletion, HostCompletionSender, HostPayload,
    HostRegistry, HostRequestError, HostRequestHandle, HostRequestState, HostTrap, HostValue,
    ReleaseKind, ReleaseQueue, ReleaseQueueError, ReleaseQueueState, ResourceContext,
    ResourceTokenHandle, RuntimeHost, RuntimeHostDomain, RuntimeResourceSnapshot, RuntimeResources,
    ScriptFunction, SnapshotHandle, TaskResourceSet,
};
pub use interpreter::{
    CheckedInterpreter, ExecutionCharge, FuelState, InterpreterContinuation, InterpreterError,
    InterpreterHost, InterpreterHostOutcome, InterpreterOutcome, OpcodeCostTable, SuspendReason,
    Trap, TrapKind,
};
pub use metrics::ExecutionMetrics;
pub use realm::{
    CancelReason, ModuleHandle, PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime,
    RuntimeCapacityReport, TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
};
pub use reload::{ReloadError, StateHandle, StateObject, StateValue, StatefulError};
pub use scope::{ScopeError, ScopeHandle, ScopeSnapshot, ScopeState};
pub use slot_pool::{HandleError, SlotAllocError, SlotPool};
pub use task::{TaskError, TaskHandle, TaskSnapshot, TaskState};
pub use trace::{RuntimeTrace, TraceRecords};
