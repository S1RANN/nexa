//! Public Nexa facade.
//!
//! Stable facade over Nexa's model-checked runtime, compiler, bytecode and IDL APIs.

mod error;

pub use error::{
    ClassifiedError, Diagnostic, ErrorCategory, ErrorCode, ErrorContext, ErrorMetadata,
    ErrorModuleEpoch, HostError, MigrationError, NexaError,
};

use nexa_bytecode::{DecodeLimits, Module};
use nexa_core::{FileId, StableId};
use nexa_idl::Idl;
use nexa_verifier::{VerifiedModule, VerifierLimits};

/// Compiles source through the stable facade error boundary.
pub fn compile(source: &str) -> Result<VerifiedModule, NexaError> {
    compile_file(source, FileId::default())
}

/// Compiles source and attaches `file` to diagnostics that have a source location.
pub fn compile_file(source: &str, file: FileId) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile(source)
        .map_err(|error| NexaError::Diagnostic(Diagnostic::new(error, file)))
}

/// Compiles source against an exact IDL interface through the stable facade error boundary.
pub fn compile_with_interface(
    source: &str,
    interface: &Idl,
    schema_hash: StableId,
) -> Result<VerifiedModule, NexaError> {
    nexa_compiler::compile_with_interface(source, interface, schema_hash).map_err(NexaError::from)
}

/// Decodes a bytecode module through the stable facade error boundary.
pub fn decode_module(bytes: &[u8], limits: DecodeLimits) -> Result<Module, NexaError> {
    Module::decode_with_limits(bytes, limits).map_err(NexaError::from)
}

/// Verifies a module through the stable facade error boundary.
pub fn verify_module(module: Module, limits: VerifierLimits) -> Result<VerifiedModule, NexaError> {
    nexa_verifier::verify(module, limits).map_err(NexaError::from)
}

pub mod prelude {
    pub use nexa_bytecode::{
        AbandonPolicy, AsyncResultType, CancelPolicy, DecodeLimits, EnumType, EnumVariant,
        FunctionBuilder, FunctionEffect, HostCallMode, HostImport, Instruction, ModuleBuilder,
        Signature, StateField, StateSchema, StateType, ValueType, option_type, result_type,
    };
    pub use nexa_core::{FileId, FunctionId, ModuleId, RawHandle, RealmId, SourceSpan, TypeId};
    pub use nexa_idl::{exact_hash as exact_idl_hash, generate_rust as generate_rust_bindings};
    pub use nexa_runtime::{
        CancelReason, HostArgs, HostCallOutcome, HostCompletionResult, HostCompletionTicket,
        HostErrorPayload, HostPayload, HostRegistry, HostRequestHandle, HostTrap, HostValue,
        MigrationCapacityReport, MigrationLimitError, MigrationLimits, ModuleHandle,
        PendingHostRequest, PendingReason, PollResult, RealmConfig, RealmError, RealmRuntime,
        ResourceContext, ResourceTokenHandle, RuntimeHost, RuntimeHostCloseError,
        RuntimeHostDomain, RuntimeValue, ScopeHandle, ScopeSnapshot, ScriptFunction,
        SnapshotHandle, StateHandle, StateValue, StatefulDomainId, StepConfig, TaskHandle,
        TaskLimits, TaskTerminalReason, TaskTerminalRecord, TickBudget, TickReport,
    };
    pub use nexa_verifier::VerifierLimits;

    pub use crate::{
        ClassifiedError, Diagnostic, ErrorCategory, ErrorCode, ErrorContext, ErrorMetadata,
        ErrorModuleEpoch, HostError, MigrationError, NexaError, compile, compile_file,
        compile_with_interface, decode_module, verify_module,
    };
}
