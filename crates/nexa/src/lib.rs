//! Public Nexa facade.
//!
//! The first milestone intentionally exports only stable primitive identities. Runtime,
//! compiler, and binding APIs will be added only after their model-checked implementations exist.

pub mod prelude {
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_runtime::{
        ScopeHandle, ScopeManager, ScopeSnapshot, TaskError, TaskEvent, TaskHandle, TaskManager,
        TaskSnapshot, TaskState, TraceRecorder,
    };
}
