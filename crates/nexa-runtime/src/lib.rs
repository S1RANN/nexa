//! Model-driven Nexa runtime primitives.

mod scope;
mod slot_pool;
mod task;
mod trace;

#[allow(dead_code, clippy::all, clippy::pedantic)]
mod machines {
    include!(concat!(env!("OUT_DIR"), "/machines.rs"));
}

pub use scope::{ScopeError, ScopeHandle, ScopeManager, ScopeSnapshot, ScopeTransitionError};
pub use slot_pool::{HandleError, SlotPool};
pub use task::{
    TaskError, TaskEvent, TaskHandle, TaskManager, TaskSnapshot, TaskState, TaskTransitionError,
};
pub use trace::TraceRecorder;
