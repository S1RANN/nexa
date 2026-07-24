//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

pub mod prelude {
    pub use nexa_bytecode::{FunctionBuilder, Instruction, ModuleBuilder, Signature, ValueType};
    pub use nexa_compiler::compile;
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::{exact_hash as exact_idl_hash, generate_rust as generate_rust_bindings};
    pub use nexa_runtime::{
        CheckedInterpreter, FrameArena, FrameLimits, GcRef, Heap, HostRequestManager,
        ModuleEpochRoot, ReloadManager, RuntimeError, RuntimeLimits, RuntimeTrace, ScopeHandle,
        ScopeSnapshot, StateHandle, StatefulRegistry, StepConfig, StepResult, TaskError,
        TaskHandle, TaskLimits, TaskRuntime, TaskSnapshot, TaskState, VerifiedTaskContinuation,
    };
    pub use nexa_verifier::{VerifierLimits, verify};
}
