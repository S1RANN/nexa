//! Model-driven Nexa runtime primitives.

mod execution;
mod frame;
mod heap;
mod host;
mod interpreter;
mod kernel;
mod metrics;
mod reload;
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
pub use execution::{VerifiedTaskContinuation, VerifiedTaskError};
pub use frame::{DeferAction, Frame, FrameArena, FrameError, FrameLimits, RuntimeValue};
pub use heap::{CollectionStats, GcRef, GcRoots, Heap, HeapError, Object};
pub use host::{
    CopyBuffer, HostRequestError, HostRequestHandle, HostRequestManager, HostRequestState,
    ImmutableSnapshot, ReleaseKind, ReleaseQueue, ReleaseQueueError, ReleaseQueueState,
    ResourceTokenHandle, ResourceTokenManager, RuntimeHostDomain,
};
pub use interpreter::{
    CheckedContinuation, CheckedInterpreter, InterpreterError, InterpreterOutcome,
};
pub use metrics::ExecutionMetrics;
pub use reload::{
    ModuleEpochRoot, ReloadError, ReloadManager, ReloadState, StateHandle, StateValue,
    StatefulError, StatefulRegistry,
};
pub use scope::{ScopeError, ScopeHandle, ScopeSnapshot, ScopeState};
pub use slot_pool::{HandleError, SlotAllocError, SlotPool};
pub use task::{TaskError, TaskHandle, TaskSnapshot, TaskState};
pub use trace::RuntimeTrace;
