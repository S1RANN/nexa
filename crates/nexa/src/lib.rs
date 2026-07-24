//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

pub mod prelude {
    pub use nexa_bytecode::{
        FunctionBuilder, FunctionEffect, Instruction, ModuleBuilder, Signature, ValueType,
    };
    pub use nexa_compiler::compile;
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::{exact_hash as exact_idl_hash, generate_rust as generate_rust_bindings};
    pub use nexa_runtime::{
        CancelReason, HostArgs, HostCallOutcome, HostCompletion, HostCompletionSender, HostPayload,
        HostRegistry, HostRequestHandle, HostTrap, HostValue, ModuleHandle, PendingReason,
        PollResult, RealmConfig, RealmError, RealmRuntime, ResourceContext, ResourceTokenHandle,
        RuntimeHostDomain, RuntimeValue, ScopeHandle, ScopeSnapshot, ScriptFunction,
        SnapshotHandle, StateHandle, StateValue, StepConfig, TaskHandle, TaskLimits,
        TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
    };
    pub use nexa_verifier::{VerifierLimits, verify};
}
