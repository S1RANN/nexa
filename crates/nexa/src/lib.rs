//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

pub mod prelude {
    pub use nexa_bytecode::{
        AbandonPolicy, AsyncResultType, CancelPolicy, EnumType, EnumVariant, FunctionBuilder,
        FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder, Signature,
        StateField, StateSchema, StateType, ValueType, option_type, result_type,
    };
    pub use nexa_compiler::{compile, compile_with_interface};
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::{exact_hash as exact_idl_hash, generate_rust as generate_rust_bindings};
    pub use nexa_runtime::{
        CancelReason, HostArgs, HostCallOutcome, HostCompletionResult, HostCompletionTicket,
        HostErrorPayload, HostPayload, HostRegistry, HostRequestHandle, HostTrap, HostValue,
        ModuleHandle, PendingHostRequest, PendingReason, PollResult, RealmConfig, RealmError,
        RealmRuntime, ResourceContext, ResourceTokenHandle, RuntimeHost, RuntimeHostDomain,
        RuntimeValue, ScopeHandle, ScopeSnapshot, ScriptFunction, SnapshotHandle, StateHandle,
        StateValue, StatefulDomainId, StepConfig, TaskHandle, TaskLimits, TaskTerminalReason,
        TaskTerminalRecord, TickBudget, TickReport,
    };
    pub use nexa_verifier::{VerifierLimits, verify};
}
