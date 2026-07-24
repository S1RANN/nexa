//! Model-driven Nexa runtime primitives.

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

pub use kernel::{
    FailurePoint, RuntimeError, RuntimeLimits, StepConfig, StepResult, TaskLimits, TaskRuntime,
};

#[cfg(test)]
mod micro;
pub use frame::{
    ContinuationReservation, DeferAction, Frame, FrameArena, FrameError, FrameLimits, RuntimeValue,
};
pub use heap::{CollectionStats, GcRef, GcRoots, Heap, HeapError, Object};
pub use host::{
    CopyBuffer, HostArgs, HostCallOutcome, HostCompletion, HostCompletionSender, HostPayload,
    HostRegistry, HostRequestError, HostRequestHandle, HostRequestManager, HostRequestState,
    HostTrap, HostValue, ImmutableSnapshot, ReleaseKind, ReleaseQueue, ReleaseQueueError,
    ReleaseQueueState, ResourceContext, ResourceTokenHandle, ResourceTokenManager,
    RuntimeHostDomain, RuntimeResources, ScriptFunction, SnapshotHandle, SnapshotManager,
    TaskResourceSet,
};
pub use interpreter::{
    CheckedInterpreter, ExecutionCharge, FuelState, InterpreterContinuation, InterpreterError,
    InterpreterOutcome, OpcodeCostTable, SuspendReason, Trap, TrapKind,
};
pub use metrics::ExecutionMetrics;
pub use realm::{
    CancelReason, ModuleHandle, PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime,
    TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
};
pub use reload::{
    ModuleEpochRoot, ReloadCoordinator, ReloadError, ReloadManager, ReloadState, ReloadTransaction,
    StateHandle, StateValue, StatefulError, StatefulRegistry,
};
pub use scheduler::Scheduler;
pub use scope::{ScopeError, ScopeHandle, ScopeSnapshot, ScopeState};
pub use slot_pool::{HandleError, SlotAllocError, SlotPool};
pub use task::{TaskError, TaskHandle, TaskSnapshot, TaskState};
pub use trace::RuntimeTrace;
