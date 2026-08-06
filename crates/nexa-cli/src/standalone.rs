use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Stable tool exit used when a compiled standalone program traps.
pub(crate) const TRAP_EXIT_CODE: i32 = 4;

pub(crate) const DEFAULT_FUEL: u64 = 20_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunInput {
    Path(PathBuf),
    Project {
        configuration: PathBuf,
        package_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunOptions {
    pub input: RunInput,
    pub program_arguments: Vec<String>,
    pub fuel: u64,
    pub limits_file: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecOptions {
    pub module: PathBuf,
    pub function: u32,
    pub runtime_arguments: Vec<i32>,
    pub fuel: u64,
    pub limits_file: Option<PathBuf>,
    pub trace_output: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StandaloneRuntimeError {
    Trap(String),
    Internal(String),
}

impl std::fmt::Display for StandaloneRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trap(message) => write!(formatter, "standalone program trapped: {message}"),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

struct StandaloneMain;

impl nexa::ScriptExport for StandaloneMain {
    type Args = Vec<String>;
    type Output = i32;

    const STABLE_ID: nexa::StableId = nexa::STANDALONE_MAIN_STABLE_ID;
    const NAME: &'static str = "main";
    const CONTRACT_SLOT: usize = 0;
    const SIGNATURE: nexa::ScriptSignature = nexa::ScriptSignature::new(
        &[nexa::prelude::ValueType::Named(
            nexa::prelude::ArrayType::new(nexa::prelude::ValueType::String).type_id,
        )],
        Some(nexa::prelude::ValueType::I32),
    );
    const EFFECT: nexa::prelude::FunctionEffect = nexa::prelude::FunctionEffect::Task;

    fn argument_requirements(
        arguments: &Self::Args,
    ) -> Result<nexa::ScriptArgumentRequirements, nexa::ScriptCallError> {
        let mut requirements = nexa::ScriptArgumentRequirements {
            object_slots: 1,
            collection_elements: arguments.len(),
            ..nexa::ScriptArgumentRequirements::ZERO
        };
        for argument in arguments {
            requirements = requirements.checked_add(nexa::ScriptArgumentRequirements {
                object_slots: 1,
                string_bytes: argument.len(),
                ..nexa::ScriptArgumentRequirements::ZERO
            })?;
        }
        Ok(requirements)
    }

    fn encode_args(
        writer: &mut nexa::ScriptCallWriter<'_>,
        arguments: &Self::Args,
    ) -> Result<nexa::ScriptArguments, nexa::ScriptCallError> {
        let array = nexa::prelude::ArrayType::new(nexa::prelude::ValueType::String);
        let mut builder = writer.begin_array(array.type_id, array.element, arguments.len())?;
        for argument in arguments {
            let value = writer.write_string(argument.clone())?;
            writer.push_array_value(&mut builder, value)?;
        }
        nexa::ScriptArguments::try_from_array([writer.finish_array(builder)?])
    }

    fn decode_output(
        _reader: &nexa::ScriptOutputReader<'_>,
        value: nexa::prelude::RuntimeValue,
    ) -> Result<Self::Output, nexa::ScriptCallError> {
        match value {
            nexa::prelude::RuntimeValue::I32(value) => Ok(value),
            _ => Err(nexa::ScriptCallError::OutputDecoding),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsoleOperation {
    Write,
    WriteLine,
    WriteError,
    WriteErrorLine,
}

struct ConsoleRegistry {
    contract_runtime_id: nexa::StableId,
    operations: Vec<ConsoleBinding>,
}

struct ConsoleBinding {
    authority: nexa::prelude::HostFunctionAuthority,
    operation: ConsoleOperation,
}

impl ConsoleRegistry {
    fn from_artifact(
        artifact: &nexa::CompiledStandaloneArtifact,
    ) -> Result<Self, StandaloneRuntimeError> {
        let package = artifact.package();
        let contract_runtime_id = package.module().host_contract_id.ok_or_else(|| {
            StandaloneRuntimeError::Internal(
                "standalone artifact is missing its Host contract ID".into(),
            )
        })?;
        let inspection = package.debug_inspection();
        let imports = inspection.host_imports.iter().collect::<Vec<_>>();
        if imports.len() != package.module().host_imports.len() {
            return Err(StandaloneRuntimeError::Internal(
                "standalone Host import debug metadata is incomplete".into(),
            ));
        }
        let contract = nexa::parse_contract(nexa::CONSOLE_HOST_NIDL).map_err(|error| {
            StandaloneRuntimeError::Internal(format!(
                "invalid built-in Console Host contract: {error}"
            ))
        })?;
        if nexa::contract_runtime_id(&contract) != contract_runtime_id {
            return Err(StandaloneRuntimeError::Internal(
                "standalone artifact does not use the built-in Console Host contract".into(),
            ));
        }
        let mut operations = imports
            .iter()
            .map(|import| {
                if !package
                    .module()
                    .host_imports
                    .iter()
                    .any(|compiled| compiled.stable_id == import.stable_id)
                {
                    return Err(StandaloneRuntimeError::Internal(
                        "standalone Host import debug identity is not present in bytecode".into(),
                    ));
                }
                let function = contract
                    .host_functions
                    .iter()
                    .find(|function| {
                        function.stable_id == import.stable_id
                            && function.name == import.function_name
                    })
                    .ok_or_else(|| {
                        StandaloneRuntimeError::Internal(format!(
                            "standalone profile cannot authorize Host function `{}`",
                            import.function_name
                        ))
                    })?;
                let signature = nexa::host_function_signature(function);
                if function.is_async
                    || signature.parameters.as_slice() != [nexa::prelude::ValueType::String]
                    || signature.result.is_some()
                    || !function.capabilities.is_empty()
                {
                    return Err(StandaloneRuntimeError::Internal(format!(
                        "built-in Console function `{}` violates its synchronous string ABI",
                        function.name
                    )));
                }
                let operation = match import.function_name.as_str() {
                    "write" => ConsoleOperation::Write,
                    "write_line" => ConsoleOperation::WriteLine,
                    "write_error" => ConsoleOperation::WriteError,
                    "write_error_line" => ConsoleOperation::WriteErrorLine,
                    name => Err(StandaloneRuntimeError::Internal(format!(
                        "standalone profile cannot provide Host function `{name}`"
                    )))?,
                };
                Ok(ConsoleBinding {
                    authority: nexa::prelude::HostFunctionAuthority::new(
                        function.stable_id,
                        function.declaration_fingerprint.0,
                        &[nexa::prelude::ValueType::String],
                        None,
                        nexa::prelude::HostCallMode::Immediate,
                        function.fuel_cost,
                        None,
                        &[],
                    ),
                    operation,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        operations.sort_by_key(|binding| binding.authority.stable_id().0);
        if operations
            .windows(2)
            .any(|pair| pair[0].authority.stable_id() == pair[1].authority.stable_id())
        {
            return Err(StandaloneRuntimeError::Internal(
                "standalone Host import debug metadata contains duplicate stable identities".into(),
            ));
        }
        Ok(Self {
            contract_runtime_id,
            operations,
        })
    }
}

impl nexa::prelude::HostRegistry for ConsoleRegistry {
    fn contract_runtime_id(&self) -> Option<nexa::StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(
        &self,
        id: nexa::StableId,
    ) -> Option<nexa::prelude::ResolvedHostFunction<'_>> {
        self.operations
            .iter()
            .enumerate()
            .find(|(_, binding)| binding.authority.stable_id() == id)
            .and_then(|(index, binding)| {
                u32::try_from(index).ok().map(|index| {
                    nexa::prelude::ResolvedHostFunction::new(
                        nexa::prelude::HostFunctionSlot::new(index),
                        &binding.authority,
                    )
                })
            })
    }

    fn call_runtime(
        &mut self,
        slot: nexa::prelude::HostFunctionSlot,
        _context: &mut nexa::prelude::ResourceContext<'_>,
        arguments: nexa::RuntimeHostArgs<'_>,
    ) -> Result<nexa::prelude::HostCallOutcome, nexa::prelude::HostTrap> {
        use std::io::Write as _;

        let operation = self
            .operations
            .get(slot.index() as usize)
            .map(|binding| &binding.operation)
            .ok_or(nexa::prelude::HostTrap::InvalidFunctionSlot(slot))?;
        let value = arguments.string(0)?;
        let result = match operation {
            ConsoleOperation::Write => {
                print!("{value}");
                std::io::stdout().flush()
            }
            ConsoleOperation::WriteLine => {
                println!("{value}");
                std::io::stdout().flush()
            }
            ConsoleOperation::WriteError => {
                eprint!("{value}");
                std::io::stderr().flush()
            }
            ConsoleOperation::WriteErrorLine => {
                eprintln!("{value}");
                std::io::stderr().flush()
            }
        };
        result.map_err(|error| {
            nexa::prelude::HostTrap::Host(nexa::RuntimeMessage::inline(&error.to_string()))
        })?;
        Ok(nexa::prelude::HostCallOutcome::RuntimeImmediate(
            nexa::prelude::RuntimeValue::Unit,
        ))
    }
}

pub(crate) fn run_compiled(
    artifact: &nexa::CompiledStandaloneArtifact,
    arguments: &[String],
    fuel: u64,
    max_heap_objects: u32,
    cancelled: &AtomicBool,
) -> Result<i32, StandaloneRuntimeError> {
    if artifact.main().stable_id != <StandaloneMain as nexa::ScriptExport>::STABLE_ID
        || artifact.main().effect != nexa::prelude::FunctionEffect::Task
        || artifact.main_stable_id() != artifact.main().stable_id
    {
        return Err(StandaloneRuntimeError::Internal(
            "standalone main descriptor disagrees with the fixed Task ABI".into(),
        ));
    }
    let registry = ConsoleRegistry::from_artifact(artifact)?;
    let package = artifact.package();
    let host_contract_id = package.module().host_contract_id.ok_or_else(|| {
        StandaloneRuntimeError::Internal(
            "standalone artifact is missing its Host contract ID".into(),
        )
    })?;
    let state_schema = package.state_schema_fingerprint;
    let verified = package.verified.clone();
    let runtime_host = nexa::prelude::RuntimeHost::new(2_048);
    let mut realm = nexa::prelude::RealmRuntime::hosted(
        nexa::prelude::RealmConfig {
            max_heap_objects,
            ..nexa::prelude::RealmConfig::default()
        },
        runtime_host.clone(),
        Box::new(registry),
    )
    .map_err(|error| {
        StandaloneRuntimeError::Internal(format!("could not create standalone Realm: {error}"))
    })?;
    let module = realm
        .load_module(verified, host_contract_id, state_schema)
        .map_err(|error| {
            StandaloneRuntimeError::Internal(format!("could not load standalone artifact: {error}"))
        })?;
    let owner = realm.create_scope(None).map_err(|error| {
        StandaloneRuntimeError::Internal(format!("could not create standalone scope: {error}"))
    })?;
    let fuel_slice = fuel.clamp(1, 4_096);
    let task = realm
        .spawn_export::<StandaloneMain>(
            module,
            &arguments.to_vec(),
            nexa::prelude::StepConfig {
                owner,
                priority: 1,
                fuel_slice,
                cumulative_budget: fuel,
                limits: nexa::prelude::TaskLimits::default(),
            },
        )
        .map_err(|error| {
            StandaloneRuntimeError::Internal(format!(
                "could not invoke validated standalone main: {error}"
            ))
        })?;
    let exit_code = loop {
        if cancelled.swap(false, Ordering::AcqRel) {
            let _ = realm.cancel_task(task, nexa::prelude::CancelReason::HostCancelled);
        }
        match realm.poll_task(task, fuel_slice) {
            Ok(nexa::prelude::TaskPoll::Completed(nexa::prelude::RuntimeValue::I32(value))) => {
                break Ok(value);
            }
            Ok(nexa::prelude::TaskPoll::Completed(value)) => {
                break Err(StandaloneRuntimeError::Internal(format!(
                    "validated standalone main returned an invalid runtime value: {value:?}"
                )));
            }
            Ok(nexa::prelude::TaskPoll::Yielded(_)) => {}
            Ok(nexa::prelude::TaskPoll::Waiting(_)) => {
                break Err(StandaloneRuntimeError::Trap(
                    "standalone Console Host cannot leave a request pending".into(),
                ));
            }
            Ok(nexa::prelude::TaskPoll::Cancelled(reason)) => {
                break Err(StandaloneRuntimeError::Trap(format!(
                    "main was cancelled: {reason:?}"
                )));
            }
            Ok(nexa::prelude::TaskPoll::Trapped(error)) => {
                break Err(StandaloneRuntimeError::Trap(standalone_trap_message(
                    &realm,
                    task,
                    error.to_string(),
                )));
            }
            Err(error) => {
                break Err(StandaloneRuntimeError::Trap(error.to_string()));
            }
        }
    };
    drop(realm);
    let _ = runtime_host.begin_close();
    runtime_host.try_finish_close().map_err(|error| {
        StandaloneRuntimeError::Internal(format!("standalone Host did not close cleanly: {error}"))
    })?;
    exit_code
}

fn standalone_trap_message(
    realm: &nexa::prelude::RealmRuntime,
    task: nexa::prelude::TaskHandle,
    fallback: String,
) -> String {
    realm
        .terminal_record(task)
        .and_then(|record| match &record.reason {
            nexa::prelude::TaskTerminalReason::Trapped(trap) => Some(trap.message.to_string()),
            nexa::prelude::TaskTerminalReason::Completed(_)
            | nexa::prelude::TaskTerminalReason::Cancelled(_) => None,
        })
        .unwrap_or(fallback)
}
