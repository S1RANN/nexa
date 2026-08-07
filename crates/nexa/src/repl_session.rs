//! Transactional Language v2 REPL façade.
//!
//! Every submitted cell crosses the canonical resolved-build, cumulative analysis, Typed IR,
//! bytecode, verifier, and Realm transaction boundaries. Analysis metadata is committed only
//! after the runtime candidate has produced a value and the runtime reload commits.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nexa_analysis::{
    CandidateIdentity, IrType, ReplAnalysisSession, ReplCellInput, ResolvedBuildInput, SourceKey,
    TypedDeclarationBody, TypedPackageIr, TypedTypeLayoutIr, display_ir_type,
};
use nexa_diagnostics::{DiagnosticBatch, DiagnosticRenderer, SourceIdentity};
use nexa_runtime::{
    CancelReason, HostCallMode, HostCallOutcome, HostFunctionAuthority, HostFunctionSlot,
    HostRegistry, HostTrap, ModuleHandle, RealmConfig, RealmRuntime, ResolvedHostFunction,
    ResourceContext, RestartReloadPolicy, RuntimeHost, RuntimeHostArgs, RuntimeMessage,
    RuntimeValue, ScopeHandle, StepConfig, TaskLimits, TransactionalCellEntrypoint,
    TransactionalCellFailure, TransactionalCellFailureCause, TransactionalCellPoll, ValueType,
};

use crate::{CompiledReplCellArtifact, HostContractInput, PackageBuildError, PackageBuildSession};

/// Built-in Host contract used by standalone scripts and REPL sessions.
pub const CONSOLE_HOST_CONTRACT: &str = "contract Console;\n\
    host {\n\
        fn write(value: string);\n\
        fn write_line(value: string);\n\
        fn write_error(value: string);\n\
        fn write_error_line(value: string);\n\
    }\n";

/// Reader-facing identity of the built-in Console contract.
pub const CONSOLE_HOST_SOURCE_IDENTITY: &str = "contract://builtin/console.contract.nexa";

const CONSOLE_HOST_PARAMETERS: &[ValueType] = &[ValueType::String];
const CONSOLE_HOST_CAPABILITIES: &[&str] = &[];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplConsoleStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplConsoleHostError {
    pub message: String,
}

impl fmt::Display for ReplConsoleHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReplConsoleHostError {}

/// One immutable Console emission staged by a REPL Cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplConsoleEmission {
    stream: ReplConsoleStream,
    text: String,
    line_terminated: bool,
}

impl ReplConsoleEmission {
    #[must_use]
    pub const fn stream(&self) -> ReplConsoleStream {
        self.stream
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn line_terminated(&self) -> bool {
        self.line_terminated
    }
}

/// Transactional Console capability supplied by the embedding tool.
///
/// `prepare_cell` must not publish externally visible output. It may reserve or copy the complete
/// batch and fail before the Runtime transaction commits. Once preparation succeeds,
/// `commit_prepared_cell` must be infallible; it is called only after Runtime and analysis state
/// are committed. `discard_prepared_cell` must drop the prepared batch without publishing it.
pub trait ReplConsoleHost: Send {
    fn prepare_cell(&mut self, output: &[ReplConsoleEmission]) -> Result<(), ReplConsoleHostError>;

    fn commit_prepared_cell(&mut self);

    fn discard_prepared_cell(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplSessionLimits {
    pub max_heap_objects: u32,
    pub fuel_per_cell: u64,
    pub max_committed_cells: usize,
    pub max_diagnostic_history: usize,
    pub max_output_bytes_per_cell: usize,
}

impl Default for ReplSessionLimits {
    fn default() -> Self {
        Self {
            max_heap_objects: 4_096,
            fuel_per_cell: 20_000,
            max_committed_cells: 1_024,
            max_diagnostic_history: 256,
            max_output_bytes_per_cell: 1024 * 1024,
        }
    }
}

impl ReplSessionLimits {
    fn validate(self) -> Result<Self, ReplSessionError> {
        if self.max_heap_objects == 0 {
            return Err(ReplSessionError::InvalidLimits(
                "max_heap_objects must be greater than zero",
            ));
        }
        if self.fuel_per_cell == 0 {
            return Err(ReplSessionError::InvalidLimits(
                "fuel_per_cell must be greater than zero",
            ));
        }
        if self.max_committed_cells == 0 {
            return Err(ReplSessionError::InvalidLimits(
                "max_committed_cells must be greater than zero",
            ));
        }
        if self.max_diagnostic_history == 0 {
            return Err(ReplSessionError::InvalidLimits(
                "max_diagnostic_history must be greater than zero",
            ));
        }
        if self.max_output_bytes_per_cell == 0 {
            return Err(ReplSessionError::InvalidLimits(
                "max_output_bytes_per_cell must be greater than zero",
            ));
        }
        Ok(self)
    }
}

/// One fully resolved cell authority supplied by CLI, Engine, or another embedding.
///
/// The exact cell source must be the source retained by `build_input`; the contract source URI,
/// effective descriptor, compilation profile, and candidate fingerprint are validated again by
/// the façade before analysis.
pub struct ReplResolvedCellInput<'a, 'contract> {
    pub build_input: &'a ResolvedBuildInput,
    pub contract: &'a HostContractInput<'contract>,
    pub identity: CandidateIdentity,
    pub source_key: &'a SourceKey,
    pub cell: ReplCellInput,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplCellOutcome {
    pub ordinal: u64,
    pub rendered_value: Option<String>,
    pub rendered_type: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplGcReport {
    pub collections: u64,
    pub reclaimed_objects: u64,
    pub live_objects: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplMemoryReport {
    pub max_heap_objects: u32,
    pub live_heap_objects: u64,
    pub committed_cells: usize,
    pub retained_diagnostic_batches: usize,
}

impl fmt::Display for ReplGcReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} collection(s), {} object(s) reclaimed, {} live object(s)",
            self.collections, self.reclaimed_objects, self.live_objects
        )
    }
}

impl fmt::Display for ReplMemoryReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} committed cell(s), {} retained diagnostic batch(es), {}/{} live/maximum heap objects",
            self.committed_cells,
            self.retained_diagnostic_batches,
            self.live_heap_objects,
            self.max_heap_objects
        )
    }
}

#[derive(Debug)]
pub enum ReplSessionError {
    InvalidLimits(&'static str),
    CellLimitReached {
        limit: usize,
    },
    InvalidCellSource {
        source: SourceIdentity,
        reason: &'static str,
    },
    Cancelled {
        ordinal: u64,
    },
    OutputLimitExceeded {
        limit: usize,
        attempted: usize,
    },
    Console(ReplConsoleHostError),
    Analysis {
        source: SourceIdentity,
        diagnostics: DiagnosticBatch,
    },
    AnalysisSession(Box<nexa_analysis::ReplSessionError>),
    Build(PackageBuildError),
    Runtime(nexa_runtime::RealmError),
    Transaction(nexa_runtime::TransactionalCellFailure),
    Internal(String),
}

impl fmt::Display for ReplSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(message) => write!(formatter, "invalid REPL limits: {message}"),
            Self::CellLimitReached { limit } => {
                write!(
                    formatter,
                    "REPL committed Cell limit {limit} reached; use `:reset`"
                )
            }
            Self::InvalidCellSource { source, reason } => {
                write!(formatter, "invalid resolved REPL cell `{source}`: {reason}")
            }
            Self::Cancelled { ordinal } => write!(formatter, "REPL Cell {ordinal} was cancelled"),
            Self::OutputLimitExceeded { limit, attempted } => write!(
                formatter,
                "REPL Cell output requires {attempted} bytes, exceeding the {limit}-byte limit"
            ),
            Self::Console(error) => write!(formatter, "REPL Console failed: {error}"),
            Self::Analysis {
                source,
                diagnostics,
            } => write!(
                formatter,
                "REPL analysis failed for {source}:\n{}",
                DiagnosticRenderer::human(diagnostics)
            ),
            Self::AnalysisSession(error) => error.fmt(formatter),
            Self::Build(error) => error.fmt(formatter),
            Self::Runtime(error) => error.fmt(formatter),
            Self::Transaction(error) => render_transaction_failure(formatter, error),
            Self::Internal(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ReplSessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Console(error) => Some(error),
            Self::AnalysisSession(error) => Some(error.as_ref()),
            Self::Build(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::InvalidLimits(_)
            | Self::CellLimitReached { .. }
            | Self::InvalidCellSource { .. }
            | Self::Cancelled { .. }
            | Self::OutputLimitExceeded { .. }
            | Self::Analysis { .. }
            | Self::Transaction(_)
            | Self::Internal(_) => None,
        }
    }
}

impl From<PackageBuildError> for ReplSessionError {
    fn from(value: PackageBuildError) -> Self {
        Self::Build(value)
    }
}

impl From<nexa_analysis::ReplSessionError> for ReplSessionError {
    fn from(value: nexa_analysis::ReplSessionError) -> Self {
        Self::AnalysisSession(Box::new(value))
    }
}

impl From<nexa_runtime::RealmError> for ReplSessionError {
    fn from(value: nexa_runtime::RealmError) -> Self {
        Self::Runtime(value)
    }
}

fn render_transaction_failure(
    formatter: &mut fmt::Formatter<'_>,
    failure: &TransactionalCellFailure,
) -> fmt::Result {
    formatter.write_str("REPL transaction failed: ")?;
    match &failure.cause {
        TransactionalCellFailureCause::Cancelled(reason) => {
            write!(formatter, "cancelled ({reason:?})")?;
        }
        TransactionalCellFailureCause::Trapped(error) => write!(formatter, "trapped: {error}")?,
        TransactionalCellFailureCause::Runtime(error) => {
            write!(formatter, "runtime error: {error}")?;
        }
        TransactionalCellFailureCause::Activation(error) => {
            write!(formatter, "activation failed: {error}")?;
        }
        TransactionalCellFailureCause::NotReady => {
            formatter.write_str("cell was committed before reaching a terminal value")?;
        }
        TransactionalCellFailureCause::AlreadyFinished => {
            formatter.write_str("cell transaction was already finished")?;
        }
    }
    if let Some(rollback) = &failure.rollback_error {
        write!(formatter, "; rollback also failed: {rollback}")?;
    }
    Ok(())
}

struct ReplConsoleState {
    host: Box<dyn ReplConsoleHost>,
    byte_limit: usize,
    charged_bytes: usize,
    limit_error: Option<(usize, usize)>,
    pending: Vec<ReplConsoleEmission>,
}

#[derive(Clone)]
struct ReplSourceOrigin {
    display: SourceIdentity,
    text: Arc<str>,
    ordinal: u64,
}

impl ReplConsoleState {
    fn begin_cell(&mut self) {
        self.host.discard_prepared_cell();
        self.charged_bytes = 0;
        self.limit_error = None;
        self.pending.clear();
    }

    fn charge(&mut self, bytes: usize) -> Result<(), (usize, usize)> {
        let attempted = self.charged_bytes.saturating_add(bytes);
        if attempted > self.byte_limit {
            let error = (self.byte_limit, attempted);
            self.limit_error = Some(error);
            return Err(error);
        }
        self.charged_bytes = attempted;
        Ok(())
    }

    fn prepare_cell(&mut self) -> Result<(), ReplConsoleHostError> {
        self.host.prepare_cell(&self.pending)
    }

    fn commit_prepared_cell(&mut self) {
        self.host.commit_prepared_cell();
        self.pending.clear();
    }

    fn discard_cell(&mut self) {
        self.host.discard_prepared_cell();
        self.pending.clear();
    }
}

#[derive(Clone, Copy)]
struct ConsoleFunctionSlots {
    write: HostFunctionSlot,
    write_line: HostFunctionSlot,
    write_error: HostFunctionSlot,
    write_error_line: HostFunctionSlot,
}

struct ConsoleRegistry {
    contract_runtime_id: nexa_core::StableId,
    functions: ConsoleFunctionSlots,
    authorities: Vec<HostFunctionAuthority>,
    state: Arc<Mutex<ReplConsoleState>>,
}

impl ConsoleRegistry {
    fn new(
        contract: &nexa_contract::ValidatedContract,
        state: Arc<Mutex<ReplConsoleState>>,
    ) -> Result<Self, ReplSessionError> {
        let function_slot = |name: &str| {
            contract
                .host_functions
                .iter()
                .position(|function| function.name == name)
                .and_then(|index| u32::try_from(index).ok())
                .map(HostFunctionSlot::new)
                .ok_or_else(|| {
                    ReplSessionError::Internal(format!(
                        "built-in Console contract is missing Host function `{name}`"
                    ))
                })
        };
        let authorities = contract
            .host_functions
            .iter()
            .map(console_function_authority)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            contract_runtime_id: nexa_contract::contract_runtime_id(contract),
            functions: ConsoleFunctionSlots {
                write: function_slot("write")?,
                write_line: function_slot("write_line")?,
                write_error: function_slot("write_error")?,
                write_error_line: function_slot("write_error_line")?,
            },
            authorities,
            state,
        })
    }
}

fn console_function_authority(
    function: &nexa_contract::ValidatedFunction,
) -> Result<HostFunctionAuthority, ReplSessionError> {
    let signature = nexa_contract::host_function_signature(function);
    if function.is_async
        || signature.parameters.as_slice() != CONSOLE_HOST_PARAMETERS
        || signature.result.is_some()
        || !function.capabilities.is_empty()
    {
        return Err(ReplSessionError::Internal(format!(
            "built-in Console Host function `{}` has an invalid runtime contract",
            function.name
        )));
    }
    Ok(HostFunctionAuthority::new(
        function.stable_id,
        function.declaration_fingerprint.into_bytes(),
        CONSOLE_HOST_PARAMETERS,
        signature.result,
        HostCallMode::Immediate,
        function.fuel_cost,
        None,
        CONSOLE_HOST_CAPABILITIES,
    ))
}

impl HostRegistry for ConsoleRegistry {
    fn contract_runtime_id(&self) -> Option<nexa_core::StableId> {
        Some(self.contract_runtime_id)
    }

    fn resolve_function(&self, id: nexa_core::StableId) -> Option<ResolvedHostFunction<'_>> {
        self.authorities
            .iter()
            .enumerate()
            .find(|(_, authority)| authority.stable_id() == id)
            .and_then(|(index, authority)| {
                u32::try_from(index)
                    .ok()
                    .map(|index| ResolvedHostFunction::new(HostFunctionSlot::new(index), authority))
            })
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        let (stream, line_terminated) = if slot == self.functions.write {
            (ReplConsoleStream::Stdout, false)
        } else if slot == self.functions.write_line {
            (ReplConsoleStream::Stdout, true)
        } else if slot == self.functions.write_error {
            (ReplConsoleStream::Stderr, false)
        } else if slot == self.functions.write_error_line {
            (ReplConsoleStream::Stderr, true)
        } else {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        };
        if args.len() != 1 {
            return Err(HostTrap::Arity);
        }
        let text = args.str_ref(0)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| HostTrap::Host(RuntimeMessage::inline("REPL Console lock is poisoned")))?;
        let charged = text.len().saturating_add(usize::from(line_terminated));
        if let Err((limit, attempted)) = state.charge(charged) {
            return Err(HostTrap::Host(RuntimeMessage::inline(&format!(
                "REPL Cell output requires {attempted} bytes, exceeding the {limit}-byte limit"
            ))));
        }
        state.pending.push(ReplConsoleEmission {
            stream,
            text: text.as_str().to_owned(),
            line_terminated,
        });
        Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::Unit))
    }
}

/// Owning cumulative REPL session.
pub struct ReplSession {
    limits: ReplSessionLimits,
    console: Arc<Mutex<ReplConsoleState>>,
    analysis: ReplAnalysisSession,
    builds: PackageBuildSession,
    realm: Option<RealmRuntime>,
    runtime_host: RuntimeHost,
    active_module: Option<ModuleHandle>,
    owner: Option<ScopeHandle>,
    contract_runtime_id: nexa_core::StableId,
    contract_fingerprint: nexa_contract::AbiFingerprint,
    committed_contract_selection: nexa_contract::EffectiveContractSelection,
    latest_artifact: Option<CompiledReplCellArtifact>,
    source_origins: BTreeMap<SourceIdentity, ReplSourceOrigin>,
    diagnostics: VecDeque<DiagnosticBatch>,
    committed_cells: usize,
    collections: u64,
}

impl ReplSession {
    pub fn new(
        limits: ReplSessionLimits,
        console: Box<dyn ReplConsoleHost>,
    ) -> Result<Self, ReplSessionError> {
        let limits = limits.validate()?;
        let console = Arc::new(Mutex::new(ReplConsoleState {
            host: console,
            byte_limit: limits.max_output_bytes_per_cell,
            charged_bytes: 0,
            limit_error: None,
            pending: Vec::new(),
        }));
        let mut builds = PackageBuildSession::new();
        let runtime = initialize_runtime(limits, Arc::clone(&console), &mut builds)?;
        Ok(Self {
            limits,
            console,
            analysis: ReplAnalysisSession::new(),
            builds,
            realm: Some(runtime.realm),
            runtime_host: runtime.runtime_host,
            active_module: Some(runtime.module),
            owner: Some(runtime.owner),
            contract_runtime_id: runtime.contract_runtime_id,
            contract_fingerprint: runtime.contract_fingerprint,
            committed_contract_selection: nexa_contract::EffectiveContractSelection::default(),
            latest_artifact: None,
            source_origins: BTreeMap::new(),
            diagnostics: VecDeque::new(),
            committed_cells: 0,
            collections: 0,
        })
    }

    #[must_use]
    pub const fn limits(&self) -> ReplSessionLimits {
        self.limits
    }

    #[must_use]
    pub fn snapshot(&self) -> nexa_analysis::ReplSessionSnapshot {
        self.analysis.snapshot()
    }

    #[allow(clippy::unused_self)]
    pub fn is_complete(
        &self,
        identity: &SourceIdentity,
        source: &str,
    ) -> Result<bool, ReplSessionError> {
        nexa_syntax::classify_cell_completeness(source)
            .map(|completeness| completeness == nexa_syntax::CellCompleteness::Complete)
            .map_err(|error| {
                ReplSessionError::Internal(format!(
                    "could not classify REPL cell `{identity}`: {error}"
                ))
            })
    }

    #[allow(clippy::too_many_lines)]
    pub fn submit_cell(
        &mut self,
        input: ReplResolvedCellInput<'_, '_>,
        cancelled: &AtomicBool,
    ) -> Result<ReplCellOutcome, ReplSessionError> {
        if self.committed_cells == self.limits.max_committed_cells {
            return Err(ReplSessionError::CellLimitReached {
                limit: self.limits.max_committed_cells,
            });
        }
        self.validate_resolved_cell(&input)?;
        if Self::is_cancelled(cancelled) {
            return Err(ReplSessionError::Cancelled {
                ordinal: input.cell.ordinal,
            });
        }
        self.begin_console_cell()?;
        let (staged_contract, staged_contract_selection) = stage_effective_contract(
            &self.committed_contract_selection,
            input.contract,
            input.build_input,
        )?;

        let source = input.cell.source.clone();
        let source_text = Arc::clone(&input.cell.text);
        let internal_source = SourceIdentity::package(
            input.source_key.package_id.as_str(),
            input.source_key.path.as_str(),
        );
        let ordinal = input.cell.ordinal;
        let compilation = self.builds.compile_repl_cell_with_contract(
            input.build_input,
            &staged_contract,
            input.identity,
            &self.analysis.snapshot(),
            input.cell,
        );
        let (artifact, staged) = match compilation {
            Ok(compiled) => compiled,
            Err(error) => {
                return Err(self.build_error(
                    source,
                    &internal_source,
                    source_text,
                    ordinal,
                    error,
                ));
            }
        };
        let candidate_ir = staged.candidate_ir().cloned().ok_or_else(|| {
            ReplSessionError::Internal("compiled REPL stage lost Typed IR".into())
        })?;
        let result_type = staged.result_type().clone();
        let rendered_type = display_ir_type(&result_type, candidate_ir.definitions());
        let mut next_analysis = self.analysis.clone();
        next_analysis.commit(staged)?;
        let extends_environment = !artifact.cell().new_state_fields.is_empty();
        let entry = if extends_environment {
            TransactionalCellEntrypoint::new(
                artifact.cell().stable_id,
                artifact.cell().signature.clone(),
            )
            .with_state_extension(artifact.cell().environment)
        } else {
            TransactionalCellEntrypoint::new(
                artifact.cell().stable_id,
                artifact.cell().signature.clone(),
            )
        };
        let old_module = self
            .active_module
            .ok_or_else(|| ReplSessionError::Internal("REPL seed module is absent".into()))?;
        let owner = self
            .owner
            .ok_or_else(|| ReplSessionError::Internal("REPL owner scope is absent".into()))?;
        let console = Arc::clone(&self.console);
        let realm = self
            .realm
            .as_mut()
            .ok_or_else(|| ReplSessionError::Internal("REPL Realm is absent".into()))?;
        let mut transaction = realm.stage_cell_transaction(
            old_module,
            artifact.package().verified.clone(),
            &entry,
            &[],
            RestartReloadPolicy::default(),
            StepConfig {
                owner,
                priority: 1,
                fuel_slice: self.limits.fuel_per_cell.min(4_096),
                cumulative_budget: self.limits.fuel_per_cell,
                limits: TaskLimits::default(),
            },
        )?;

        let terminal_value = loop {
            if Self::is_cancelled(cancelled) {
                transaction
                    .cancel(CancelReason::HostCancelled)
                    .map_err(|failure| transaction_error(&console, failure))?;
                discard_console_cell(&console)?;
                return Err(ReplSessionError::Cancelled { ordinal });
            }
            match transaction.poll() {
                Ok(TransactionalCellPoll::ReadyToCommit { value, .. }) => break value,
                Ok(TransactionalCellPoll::Yielded(_)) => {}
                Ok(TransactionalCellPoll::Waiting(_)) => {
                    transaction
                        .cancel(CancelReason::HostCancelled)
                        .map_err(|failure| transaction_error(&console, failure))?;
                    discard_console_cell(&console)?;
                    return Err(ReplSessionError::Internal(
                        "built-in Console Host unexpectedly left a pending request".into(),
                    ));
                }
                Err(failure) => return Err(transaction_error(&console, failure)),
            }
        };
        if Self::is_cancelled(cancelled) {
            transaction
                .cancel(CancelReason::HostCancelled)
                .map_err(|failure| transaction_error(&console, failure))?;
            discard_console_cell(&console)?;
            return Err(ReplSessionError::Cancelled { ordinal });
        }
        let value_result = {
            let reader = transaction.output_reader();
            render_repl_value(
                reader,
                terminal_value,
                &result_type,
                &candidate_ir,
                &console,
                cancelled,
                ordinal,
            )
        };
        let rendered_value = match value_result {
            Ok(value) => value,
            Err(error) => {
                discard_console_cell(&console)?;
                return Err(error);
            }
        };
        if Self::is_cancelled(cancelled) {
            transaction
                .cancel(CancelReason::HostCancelled)
                .map_err(|failure| transaction_error(&console, failure))?;
            discard_console_cell(&console)?;
            return Err(ReplSessionError::Cancelled { ordinal });
        }
        if let Err(error) = prepare_console_cell(&console) {
            discard_console_cell(&console)?;
            return Err(error);
        }
        if Self::is_cancelled(cancelled) {
            transaction
                .cancel(CancelReason::HostCancelled)
                .map_err(|failure| transaction_error(&console, failure))?;
            discard_console_cell(&console)?;
            return Err(ReplSessionError::Cancelled { ordinal });
        }
        let committed = transaction
            .commit()
            .map_err(|failure| transaction_error(&console, failure))?;
        self.analysis = next_analysis;
        self.active_module = Some(committed.module);
        self.latest_artifact = Some(artifact);
        self.committed_contract_selection = staged_contract_selection;
        self.source_origins.insert(
            internal_source,
            ReplSourceOrigin {
                display: source,
                text: source_text,
                ordinal,
            },
        );
        self.committed_cells = self.committed_cells.saturating_add(1);
        commit_console_cell(&console);
        Ok(ReplCellOutcome {
            ordinal,
            rendered_value,
            rendered_type,
        })
    }

    pub fn inspect_cell_type(
        &mut self,
        input: ReplResolvedCellInput<'_, '_>,
    ) -> Result<String, ReplSessionError> {
        self.validate_resolved_cell(&input)?;
        let (staged_contract, _) = stage_effective_contract(
            &self.committed_contract_selection,
            input.contract,
            input.build_input,
        )?;
        let source = input.cell.source.clone();
        let source_text = Arc::clone(&input.cell.text);
        let ordinal = input.cell.ordinal;
        let internal_source = SourceIdentity::package(
            input.source_key.package_id.as_str(),
            input.source_key.path.as_str(),
        );
        let compilation = self.builds.compile_repl_cell_with_contract(
            input.build_input,
            &staged_contract,
            input.identity,
            &self.analysis.snapshot(),
            input.cell,
        );
        let (_, staged) = match compilation {
            Ok(compiled) => compiled,
            Err(error) => {
                return Err(self.build_error(
                    source,
                    &internal_source,
                    source_text,
                    ordinal,
                    error,
                ));
            }
        };
        let ir = staged.candidate_ir().ok_or_else(|| {
            ReplSessionError::Internal("inspected REPL stage lost Typed IR".into())
        })?;
        Ok(display_ir_type(staged.result_type(), ir.definitions()))
    }

    #[allow(clippy::unused_self)]
    pub fn ast(&self, identity: &SourceIdentity, source: &str) -> Result<String, ReplSessionError> {
        let tree = nexa_syntax::parse_nexa(source).map_err(|error| {
            ReplSessionError::Internal(format!("could not parse REPL cell `{identity}`: {error}"))
        })?;
        Ok(format!("source: {identity}\n{tree:#?}"))
    }

    pub fn bytecode(&self, function: Option<&str>) -> Result<String, ReplSessionError> {
        let artifact = self.latest_artifact.as_ref().ok_or_else(|| {
            ReplSessionError::Internal("no committed REPL Cell has bytecode yet".into())
        })?;
        let package = artifact.package();
        let Some(function) = function else {
            let mut output = String::new();
            for function in &package.debug_info.functions {
                writeln!(
                    output,
                    "fn {}::{}::{} [stable={:?}]",
                    function.package_id, function.module_path, function.name, function.stable_id
                )
                .expect("writing REPL bytecode text to String cannot fail");
            }
            return Ok(output);
        };
        let mut matches = package.debug_info.functions.iter().filter(|candidate| {
            candidate.name == function
                || format!(
                    "{}::{}::{}",
                    candidate.package_id, candidate.module_path, candidate.name
                ) == function
        });
        let Some(debug) = matches.next() else {
            return Err(ReplSessionError::Internal(format!(
                "committed REPL bytecode has no function `{function}`"
            )));
        };
        if matches.next().is_some() {
            return Err(ReplSessionError::Internal(format!(
                "committed REPL bytecode name `{function}` is ambiguous; use package::module::name"
            )));
        }
        let compiled = package
            .module()
            .functions
            .get(usize::try_from(debug.function_index).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                ReplSessionError::Internal(format!(
                    "debug entry for `{function}` has an invalid function index"
                ))
            })?;
        Ok(format!(
            "fn {}::{}::{} [stable={:?}]\n{compiled:#?}",
            debug.package_id, debug.module_path, debug.name, debug.stable_id
        ))
    }

    #[must_use]
    pub fn diagnostic_history(&self) -> &VecDeque<DiagnosticBatch> {
        &self.diagnostics
    }

    #[must_use]
    pub fn memory(&self) -> ReplMemoryReport {
        let live_heap_objects = self
            .realm
            .as_ref()
            .map_or(0, |realm| realm.resource_ledger().heap_objects);
        ReplMemoryReport {
            max_heap_objects: self.limits.max_heap_objects,
            live_heap_objects,
            committed_cells: self.committed_cells,
            retained_diagnostic_batches: self.diagnostics.len(),
        }
    }

    pub fn collect_garbage(&mut self) -> Result<ReplGcReport, ReplSessionError> {
        let collection = if let Some(realm) = &mut self.realm {
            Some(realm.collect_garbage()?)
        } else {
            None
        };
        self.collections = self.collections.saturating_add(1);
        Ok(ReplGcReport {
            collections: self.collections,
            reclaimed_objects: collection.map_or(0, |stats| {
                u64::try_from(stats.reclaimed).unwrap_or(u64::MAX)
            }),
            live_objects: collection
                .map_or(0, |stats| u64::try_from(stats.live).unwrap_or(u64::MAX)),
        })
    }

    pub fn reset(&mut self) -> Result<(), ReplSessionError> {
        let mut builds = PackageBuildSession::new();
        let runtime = initialize_runtime(self.limits, Arc::clone(&self.console), &mut builds)?;
        let old_realm = self.realm.take();
        drop(old_realm);
        let _ = self.runtime_host.begin_close();
        self.runtime_host.try_finish_close().map_err(|error| {
            ReplSessionError::Internal(format!(
                "could not close the previous REPL Runtime Host: {error}"
            ))
        })?;
        self.analysis = ReplAnalysisSession::new();
        self.builds = builds;
        self.realm = Some(runtime.realm);
        self.runtime_host = runtime.runtime_host;
        self.active_module = Some(runtime.module);
        self.owner = Some(runtime.owner);
        self.contract_runtime_id = runtime.contract_runtime_id;
        self.contract_fingerprint = runtime.contract_fingerprint;
        self.committed_contract_selection = nexa_contract::EffectiveContractSelection::default();
        self.latest_artifact = None;
        self.source_origins.clear();
        self.diagnostics.clear();
        self.committed_cells = 0;
        self.collections = 0;
        let mut console = self
            .console
            .lock()
            .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?;
        console.begin_cell();
        Ok(())
    }

    #[must_use]
    pub fn latest_artifact(&self) -> Option<&CompiledReplCellArtifact> {
        self.latest_artifact.as_ref()
    }

    fn validate_resolved_cell(
        &self,
        input: &ReplResolvedCellInput<'_, '_>,
    ) -> Result<(), ReplSessionError> {
        if input.contract.runtime_id() != self.contract_runtime_id
            || nexa_contract::abi_descriptor(input.contract.contract()).fingerprint
                != self.contract_fingerprint
        {
            return Err(ReplSessionError::Build(
                PackageBuildError::HostContractIdMismatch,
            ));
        }
        let expected_contract_source = SourceIdentity::standalone(CONSOLE_HOST_SOURCE_IDENTITY);
        if input.contract.source().identity() != &expected_contract_source
            || input.contract.source().text().as_ref() != CONSOLE_HOST_CONTRACT
        {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "Host contract is not the built-in Console source authority",
            });
        }
        if input.build_input.root_manifest.id.as_str() != nexa_analysis::REPL_PACKAGE_ID {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "resolved root is not the reserved synthetic REPL package",
            });
        }
        if input.source_key.package_id != input.build_input.root_manifest.id {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "source key does not belong to the resolved root package",
            });
        }
        if input.cell.source.package_id() != Some(input.build_input.root_manifest.id.as_str()) {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "reader-facing source does not belong to the resolved root package",
            });
        }
        let mut production = input.build_input.root_source_set.production_units();
        if production
            .next()
            .is_none_or(|unit| &unit.key != input.source_key)
            || production.next().is_some()
        {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "resolved REPL input must contain exactly the selected production cell",
            });
        }
        let Some(unit) = input.build_input.root_source_set.get(input.source_key) else {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "source key is absent from the resolved root source set",
            });
        };
        if unit.role != nexa_analysis::SourceRole::Production {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "source key does not identify a production REPL cell",
            });
        }
        if !unit
            .expected_module_path()
            .is_ok_and(|module| module.as_str() == nexa_analysis::REPL_MODULE_PATH)
        {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "resolved cell does not belong to the synthetic REPL module",
            });
        }
        if unit.text.as_ref() != input.cell.text.as_ref() {
            return Err(ReplSessionError::InvalidCellSource {
                source: input.cell.source.clone(),
                reason: "resolved source bytes do not match the submitted cell",
            });
        }
        Ok(())
    }

    fn begin_console_cell(&self) -> Result<(), ReplSessionError> {
        self.console
            .lock()
            .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?
            .begin_cell();
        Ok(())
    }

    fn build_error(
        &mut self,
        source: SourceIdentity,
        internal_source: &SourceIdentity,
        source_text: Arc<str>,
        ordinal: u64,
        error: PackageBuildError,
    ) -> ReplSessionError {
        match error {
            PackageBuildError::AnalysisFailed(diagnostics) => {
                let current = ReplSourceOrigin {
                    display: source.clone(),
                    text: source_text,
                    ordinal,
                };
                let diagnostics = match remap_cell_diagnostics(
                    &diagnostics,
                    &self.source_origins,
                    internal_source,
                    &current,
                ) {
                    Ok(diagnostics) => diagnostics,
                    Err(error) => return error,
                };
                if self.diagnostics.len() == self.limits.max_diagnostic_history {
                    self.diagnostics.pop_front();
                }
                self.diagnostics.push_back(diagnostics.clone());
                ReplSessionError::Analysis {
                    source,
                    diagnostics,
                }
            }
            error => ReplSessionError::Build(error),
        }
    }

    #[must_use]
    pub fn is_cancelled(cancelled: &AtomicBool) -> bool {
        cancelled.load(Ordering::Acquire)
    }
}

impl Drop for ReplSession {
    fn drop(&mut self) {
        let realm = self.realm.take();
        drop(realm);
        let _ = self.runtime_host.begin_close();
        let _ = self.runtime_host.try_finish_close();
    }
}

fn transaction_error(
    console: &Arc<Mutex<ReplConsoleState>>,
    failure: TransactionalCellFailure,
) -> ReplSessionError {
    if let Ok(mut state) = console.lock() {
        state.discard_cell();
        if let Some((limit, attempted)) = state.limit_error.take() {
            return ReplSessionError::OutputLimitExceeded { limit, attempted };
        }
    }
    ReplSessionError::Transaction(failure)
}

fn discard_console_cell(console: &Arc<Mutex<ReplConsoleState>>) -> Result<(), ReplSessionError> {
    console
        .lock()
        .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?
        .discard_cell();
    Ok(())
}

fn prepare_console_cell(console: &Arc<Mutex<ReplConsoleState>>) -> Result<(), ReplSessionError> {
    console
        .lock()
        .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?
        .prepare_cell()
        .map_err(ReplSessionError::Console)
}

fn commit_console_cell(console: &Arc<Mutex<ReplConsoleState>>) {
    console
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .commit_prepared_cell();
}

fn charge_rendered_value(
    console: &Arc<Mutex<ReplConsoleState>>,
    value: &str,
) -> Result<(), ReplSessionError> {
    let bytes = value.len().saturating_add(1);
    let mut console = console
        .lock()
        .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?;
    if let Err((limit, attempted)) = console.charge(bytes) {
        console.discard_cell();
        return Err(ReplSessionError::OutputLimitExceeded { limit, attempted });
    }
    Ok(())
}

fn stage_effective_contract<'contract>(
    committed: &nexa_contract::EffectiveContractSelection,
    contract: &HostContractInput<'contract>,
    input: &ResolvedBuildInput,
) -> Result<
    (
        HostContractInput<'contract>,
        nexa_contract::EffectiveContractSelection,
    ),
    ReplSessionError,
> {
    let current = contract
        .selecting_effective_package_contract(
            &input.root_manifest,
            &input.root_source_set,
            &input.dependency_source_sets,
        )
        .map_err(|error| ReplSessionError::Build(PackageBuildError::HostContractSource(error)))?;
    let cumulative = crate::package_build::union_effective_contract_selection(
        committed,
        current.effective_selection(),
    );
    let staged = contract
        .selecting_effective_contract(cumulative.clone())
        .map_err(|error| ReplSessionError::Build(PackageBuildError::HostContractSource(error)))?;
    Ok((staged, cumulative))
}

fn remap_cell_diagnostics(
    diagnostics: &DiagnosticBatch,
    committed: &BTreeMap<SourceIdentity, ReplSourceOrigin>,
    current_internal: &SourceIdentity,
    current: &ReplSourceOrigin,
) -> Result<DiagnosticBatch, ReplSessionError> {
    let mut builder = nexa_diagnostics::SourceSnapshotRegistry::builder();
    let mut inserted = BTreeMap::new();
    insert_remapped_source(
        &mut builder,
        &mut inserted,
        current.display.clone(),
        Arc::clone(&current.text),
    )?;
    for (identity, snapshot) in diagnostics.sources().iter() {
        let origin = repl_source_origin(committed, current_internal, current, identity);
        let remapped_identity =
            remapped_repl_identity(committed, current_internal, current, identity)
                .unwrap_or_else(|| identity.clone());
        let snapshot_text = Arc::<str>::from(snapshot.text());
        let text = if let Some(origin) = origin {
            if origin.text.as_ref() != snapshot.text() {
                return Err(ReplSessionError::Internal(format!(
                    "REPL diagnostic source `{identity}` disagrees with its committed source bytes"
                )));
            }
            Arc::clone(&origin.text)
        } else {
            snapshot_text
        };
        insert_remapped_source(&mut builder, &mut inserted, remapped_identity, text)?;
    }
    for identity in diagnostics.diagnostics().iter().flat_map(|diagnostic| {
        diagnostic
            .labels
            .iter()
            .map(|label| &label.source)
            .chain(diagnostic.related.iter().map(|related| &related.source))
            .chain(
                diagnostic
                    .fixes
                    .iter()
                    .filter_map(|fix| fix.source.as_ref()),
            )
    }) {
        let Some(origin) = repl_source_origin(committed, current_internal, current, identity)
        else {
            continue;
        };
        let remapped_identity =
            remapped_repl_identity(committed, current_internal, current, identity)
                .expect("the source origin was already resolved");
        insert_remapped_source(
            &mut builder,
            &mut inserted,
            remapped_identity,
            Arc::clone(&origin.text),
        )?;
    }
    let mut remapped = DiagnosticBatch::with_default_limits(builder.build());
    remapped.extend(
        diagnostics
            .diagnostics()
            .iter()
            .cloned()
            .map(|mut diagnostic| {
                for label in &mut diagnostic.labels {
                    if let Some(identity) =
                        remapped_repl_identity(committed, current_internal, current, &label.source)
                    {
                        label.source = identity;
                    }
                }
                for related in &mut diagnostic.related {
                    if let Some(identity) = remapped_repl_identity(
                        committed,
                        current_internal,
                        current,
                        &related.source,
                    ) {
                        related.source = identity;
                    }
                }
                for fix in &mut diagnostic.fixes {
                    if let Some(identity) = fix.source.as_ref().and_then(|source| {
                        remapped_repl_identity(committed, current_internal, current, source)
                    }) {
                        fix.source = Some(identity);
                    }
                }
                diagnostic
            }),
    );
    Ok(remapped)
}

fn insert_remapped_source(
    builder: &mut nexa_diagnostics::SourceSnapshotRegistryBuilder,
    inserted: &mut BTreeMap<SourceIdentity, Arc<str>>,
    identity: SourceIdentity,
    text: Arc<str>,
) -> Result<(), ReplSessionError> {
    if let Some(existing) = inserted.get(&identity) {
        if existing.as_ref() != text.as_ref() {
            return Err(ReplSessionError::Internal(format!(
                "REPL diagnostic identity `{identity}` resolves to conflicting source bytes"
            )));
        }
        return Ok(());
    }
    builder
        .insert(identity.clone(), Arc::clone(&text))
        .map_err(|error| {
            ReplSessionError::Internal(format!(
                "could not retain REPL diagnostic source `{identity}`: {error}"
            ))
        })?;
    inserted.insert(identity, text);
    Ok(())
}

fn repl_source_origin<'a>(
    committed: &'a BTreeMap<SourceIdentity, ReplSourceOrigin>,
    current_internal: &SourceIdentity,
    current: &'a ReplSourceOrigin,
    identity: &SourceIdentity,
) -> Option<&'a ReplSourceOrigin> {
    if identity == current_internal {
        Some(current)
    } else {
        committed.get(identity)
    }
}

fn remapped_repl_identity(
    committed: &BTreeMap<SourceIdentity, ReplSourceOrigin>,
    current_internal: &SourceIdentity,
    current: &ReplSourceOrigin,
    identity: &SourceIdentity,
) -> Option<SourceIdentity> {
    let origin = repl_source_origin(committed, current_internal, current, identity)?;
    if identity == current_internal {
        return Some(origin.display.clone());
    }
    let has_distinct_revision = (origin.display == current.display && origin.text != current.text)
        || committed.iter().any(|(other_identity, other)| {
            other_identity != identity
                && other.display == origin.display
                && other.text != origin.text
        });
    if !has_distinct_revision {
        return Some(origin.display.clone());
    }
    let revision_path = format!(
        "{}#nexa-repl-cell-{}",
        origin.display.path(),
        origin.ordinal
    );
    Some(match origin.display.package_id() {
        Some(package_id) => SourceIdentity::package(package_id, revision_path),
        None => SourceIdentity::standalone(revision_path),
    })
}

struct BoundedValueRenderer<'a> {
    output: String,
    byte_limit: usize,
    previously_charged: usize,
    cancelled: &'a AtomicBool,
    ordinal: u64,
}

impl<'a> BoundedValueRenderer<'a> {
    fn new(
        console: &Arc<Mutex<ReplConsoleState>>,
        cancelled: &'a AtomicBool,
        ordinal: u64,
    ) -> Result<Self, ReplSessionError> {
        let console = console
            .lock()
            .map_err(|_| ReplSessionError::Internal("REPL Console lock is poisoned".into()))?;
        Ok(Self {
            output: String::new(),
            byte_limit: console.byte_limit,
            previously_charged: console.charged_bytes,
            cancelled,
            ordinal,
        })
    }

    fn push_str(&mut self, value: &str) -> Result<(), ReplSessionError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(ReplSessionError::Cancelled {
                ordinal: self.ordinal,
            });
        }
        // The CLI terminates a rendered value with one newline. Reserve that byte before every
        // append so no value allocation can cross the combined Console/value output limit.
        let attempted = self
            .previously_charged
            .saturating_add(self.output.len())
            .saturating_add(value.len())
            .saturating_add(1);
        if attempted > self.byte_limit {
            return Err(ReplSessionError::OutputLimitExceeded {
                limit: self.byte_limit,
                attempted,
            });
        }
        self.output.push_str(value);
        Ok(())
    }

    fn push_char(&mut self, value: char) -> Result<(), ReplSessionError> {
        let mut encoded = [0_u8; 4];
        self.push_str(value.encode_utf8(&mut encoded))
    }

    fn write_quoted(&mut self, value: &str) -> Result<(), ReplSessionError> {
        self.push_char('"')?;
        for value in value.chars() {
            for escaped in value.escape_debug() {
                self.push_char(escaped)?;
            }
        }
        self.push_char('"')
    }

    fn finish(self) -> String {
        self.output
    }
}

fn render_repl_value(
    reader: nexa_runtime::ScriptOutputReader<'_>,
    value: RuntimeValue,
    ty: &IrType,
    ir: &TypedPackageIr,
    console: &Arc<Mutex<ReplConsoleState>>,
    cancelled: &AtomicBool,
    ordinal: u64,
) -> Result<Option<String>, ReplSessionError> {
    if matches!(ty, IrType::Unit) {
        validate_repl_runtime_value(value, ty, ir)?;
        return Ok(None);
    }
    let mut formatter = BoundedValueRenderer::new(console, cancelled, ordinal)?;
    render_repl_value_inner(&mut formatter, reader, value, ty, ir, 0)?;
    let output = formatter.finish();
    charge_rendered_value(console, &output)?;
    Ok(Some(output))
}

#[allow(clippy::too_many_lines)]
fn render_repl_value_inner(
    renderer: &mut BoundedValueRenderer<'_>,
    reader: nexa_runtime::ScriptOutputReader<'_>,
    value: RuntimeValue,
    ty: &IrType,
    ir: &TypedPackageIr,
    depth: usize,
) -> Result<(), ReplSessionError> {
    if depth >= 32 {
        return Err(ReplSessionError::Internal(
            "REPL value exceeds the renderer nesting limit".into(),
        ));
    }
    validate_repl_runtime_value(value, ty, ir)?;
    let source = reader.value(value);
    let decode = |error: HostTrap| {
        ReplSessionError::Internal(format!(
            "runtime value disagrees with the analyzed REPL type: {error:?}"
        ))
    };
    match ty {
        IrType::Error => renderer.push_str("<error>"),
        IrType::Unit => renderer.push_str("()"),
        IrType::Bool => renderer.push_str(&source.bool().map_err(decode)?.to_string()),
        IrType::I32 => renderer.push_str(&source.i32().map_err(decode)?.to_string()),
        IrType::I64 => renderer.push_str(&source.i64().map_err(decode)?.to_string()),
        IrType::F32 => renderer.push_str(&source.f32().map_err(decode)?.to_string()),
        IrType::F64 => renderer.push_str(&source.f64().map_err(decode)?.to_string()),
        IrType::String => {
            let value = source.str_ref().map_err(decode)?;
            renderer.write_quoted(value.as_str())
        }
        IrType::Rune => renderer.push_str(&format!("{:?}", source.rune().map_err(decode)?)),
        IrType::Option(payload) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let enumeration = source.enum_ref(type_id).map_err(decode)?;
            let option = nexa_bytecode::option_type(repl_bytecode_value_type(payload, ir)?);
            match enumeration.tag() {
                0 => {
                    validate_runtime_variant(
                        enumeration.variant(),
                        option.variants[0].stable_id,
                        "Option::None",
                    )?;
                    if enumeration.payload().is_some() {
                        return Err(ReplSessionError::Internal(
                            "Option::None runtime value unexpectedly has a payload".into(),
                        ));
                    }
                    renderer.push_str("None")
                }
                1 => {
                    validate_runtime_variant(
                        enumeration.variant(),
                        option.variants[1].stable_id,
                        "Option::Some",
                    )?;
                    let payload_value = enumeration.payload().ok_or_else(|| {
                        ReplSessionError::Internal(
                            "Option::Some runtime value has no payload".into(),
                        )
                    })?;
                    renderer.push_str("Some(")?;
                    render_repl_value_inner(
                        renderer,
                        reader,
                        payload_value.runtime_value(),
                        payload,
                        ir,
                        depth + 1,
                    )?;
                    renderer.push_char(')')
                }
                tag => Err(ReplSessionError::Internal(format!(
                    "Option runtime value has invalid tag {tag}"
                ))),
            }
        }
        IrType::Result(success, error) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let enumeration = source.enum_ref(type_id).map_err(decode)?;
            let payload = enumeration.payload().ok_or_else(|| {
                ReplSessionError::Internal("Result runtime value has no payload".into())
            })?;
            let result = nexa_bytecode::result_type(
                repl_bytecode_value_type(success, ir)?,
                repl_bytecode_value_type(error, ir)?,
            );
            match enumeration.tag() {
                0 => {
                    validate_runtime_variant(
                        enumeration.variant(),
                        result.variants[0].stable_id,
                        "Result::Ok",
                    )?;
                    renderer.push_str("Ok(")?;
                    render_repl_value_inner(
                        renderer,
                        reader,
                        payload.runtime_value(),
                        success,
                        ir,
                        depth + 1,
                    )?;
                    renderer.push_char(')')
                }
                1 => {
                    validate_runtime_variant(
                        enumeration.variant(),
                        result.variants[1].stable_id,
                        "Result::Err",
                    )?;
                    renderer.push_str("Err(")?;
                    render_repl_value_inner(
                        renderer,
                        reader,
                        payload.runtime_value(),
                        error,
                        ir,
                        depth + 1,
                    )?;
                    renderer.push_char(')')
                }
                tag => Err(ReplSessionError::Internal(format!(
                    "Result runtime value has invalid tag {tag}"
                ))),
            }
        }
        IrType::Array(element) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let array = source.array_ref(type_id).map_err(decode)?;
            renderer.push_char('[')?;
            for (index, value) in array.iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                render_repl_value_inner(
                    renderer,
                    reader,
                    value.runtime_value(),
                    element,
                    ir,
                    depth + 1,
                )?;
            }
            renderer.push_char(']')
        }
        IrType::Buffer(element) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let buffer = source.buffer_ref(type_id).map_err(decode)?;
            renderer.push_str("buffer[")?;
            for (index, value) in buffer.iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                render_repl_value_inner(
                    renderer,
                    reader,
                    value.runtime_value(),
                    element,
                    ir,
                    depth + 1,
                )?;
            }
            renderer.push_char(']')
        }
        IrType::Named(definition) => {
            render_named_value(renderer, reader, value, *definition, ir, depth)
        }
        IrType::Tuple(elements) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let tuple = source.struct_ref(type_id).map_err(decode)?;
            if tuple.len() != elements.len() {
                return Err(ReplSessionError::Internal(
                    "Tuple runtime field count disagrees with Typed IR".into(),
                ));
            }
            renderer.push_char('(')?;
            for (index, element) in elements.iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                let field = tuple.field(index).map_err(decode)?;
                render_repl_value_inner(
                    renderer,
                    reader,
                    field.runtime_value(),
                    element,
                    ir,
                    depth + 1,
                )?;
            }
            if elements.len() == 1 {
                renderer.push_char(',')?;
            }
            renderer.push_char(')')
        }
        IrType::Map(key, value_type) => {
            let type_id = expected_named_runtime_type_id(ty, ir)?;
            let map = source.map_ref(type_id).map_err(decode)?;
            renderer.push_char('{')?;
            for (index, entry) in map.iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                render_repl_value_inner(
                    renderer,
                    reader,
                    entry.key().runtime_value(),
                    key,
                    ir,
                    depth + 1,
                )?;
                renderer.push_str(": ")?;
                render_repl_value_inner(
                    renderer,
                    reader,
                    entry.value().runtime_value(),
                    value_type,
                    ir,
                    depth + 1,
                )?;
            }
            renderer.push_char('}')
        }
        IrType::HostRequest(_) => Err(ReplSessionError::Internal(
            "a HostRequest escaped the typed await boundary into the REPL reader".into(),
        )),
        IrType::ResourceToken(Some(_)) => renderer.push_str("<Token>"),
        IrType::ResourceToken(None) => Err(ReplSessionError::Internal(
            "an untyped Token escaped typed IR validation into the REPL reader".into(),
        )),
        IrType::Snapshot(_) => renderer.push_str("<Snapshot>"),
        IrType::StateHandle(_) => renderer.push_str("<StateHandle>"),
        IrType::TypeParameter(index) => Err(ReplSessionError::Internal(format!(
            "REPL result retained unresolved type parameter T{index}"
        ))),
    }
}

#[allow(clippy::too_many_lines)]
fn render_named_value(
    renderer: &mut BoundedValueRenderer<'_>,
    reader: nexa_runtime::ScriptOutputReader<'_>,
    value: RuntimeValue,
    definition: nexa_analysis::DefinitionId,
    ir: &TypedPackageIr,
    depth: usize,
) -> Result<(), ReplSessionError> {
    let name = ir
        .definition(definition)
        .map_or("<unknown>", |definition| definition.name.as_str());
    let Some(layout) = ir.modules().iter().find_map(|module| {
        module.declarations.iter().find_map(|declaration| {
            (declaration.definition == definition).then_some(&declaration.body)
        })
    }) else {
        renderer.push_char('<')?;
        renderer.push_str(name)?;
        return renderer.push_char('>');
    };
    match layout {
        TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Struct { fields }) => {
            let RuntimeValue::Struct { .. } = value else {
                return Err(ReplSessionError::Internal(format!(
                    "`{name}` runtime value is not a Struct"
                )));
            };
            let type_id = expected_named_runtime_type_id(&IrType::Named(definition), ir)?;
            let structure = reader.value(value).struct_ref(type_id).map_err(|error| {
                ReplSessionError::Internal(format!("could not decode `{name}`: {error:?}"))
            })?;
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by_key(|field| field.order);
            if structure.len() != fields.len() {
                return Err(ReplSessionError::Internal(format!(
                    "`{name}` runtime field count disagrees with Typed IR"
                )));
            }
            renderer.push_str(name)?;
            renderer.push_str(" { ")?;
            for (index, field) in fields.into_iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                let field_name = ir
                    .definition(field.definition)
                    .map_or("?", |definition| definition.name.as_str());
                let field_value = structure.field(index).map_err(|error| {
                    ReplSessionError::Internal(format!(
                        "could not decode `{name}.{field_name}`: {error:?}"
                    ))
                })?;
                renderer.push_str(field_name)?;
                renderer.push_str(": ")?;
                render_repl_value_inner(
                    renderer,
                    reader,
                    field_value.runtime_value(),
                    &field.ty,
                    ir,
                    depth + 1,
                )?;
            }
            renderer.push_str(" }")
        }
        TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Class { fields, .. }) => {
            let type_id = expected_named_runtime_type_id(&IrType::Named(definition), ir)?;
            let class = reader.value(value).class_ref(type_id).map_err(|error| {
                ReplSessionError::Internal(format!("could not decode `{name}`: {error:?}"))
            })?;
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_by_key(|field| field.order);
            if class.len() != fields.len() {
                return Err(ReplSessionError::Internal(format!(
                    "`{name}` runtime field count disagrees with Typed IR"
                )));
            }
            renderer.push_str(name)?;
            renderer.push_str(" { ")?;
            for (index, field) in fields.into_iter().enumerate() {
                if index != 0 {
                    renderer.push_str(", ")?;
                }
                let field_name = ir
                    .definition(field.definition)
                    .map_or("?", |definition| definition.name.as_str());
                let field_value = class.field(index).map_err(|error| {
                    ReplSessionError::Internal(format!(
                        "could not decode `{name}.{field_name}`: {error:?}"
                    ))
                })?;
                renderer.push_str(field_name)?;
                renderer.push_str(": ")?;
                render_repl_value_inner(
                    renderer,
                    reader,
                    field_value.runtime_value(),
                    &field.ty,
                    ir,
                    depth + 1,
                )?;
            }
            renderer.push_str(" }")
        }
        TypedDeclarationBody::TypeLayout(TypedTypeLayoutIr::Enum { variants }) => {
            let type_id = expected_named_runtime_type_id(&IrType::Named(definition), ir)?;
            let enumeration = reader.value(value).enum_ref(type_id).map_err(|error| {
                ReplSessionError::Internal(format!("could not decode `{name}`: {error:?}"))
            })?;
            let variant = variants
                .iter()
                .find(|variant| variant.tag == enumeration.tag())
                .ok_or_else(|| {
                    ReplSessionError::Internal(format!(
                        "`{name}` runtime value has unknown tag {}",
                        enumeration.tag()
                    ))
                })?;
            let variant_name = ir
                .definition(variant.definition)
                .map_or("?", |definition| definition.name.as_str());
            let expected_variant = source_definition_runtime_id(variant.definition, ir)?;
            validate_runtime_variant(
                enumeration.variant(),
                expected_variant,
                &format!("{name}::{variant_name}"),
            )?;
            match (&variant.payload, enumeration.payload()) {
                (None, None) => {
                    renderer.push_str(name)?;
                    renderer.push_str("::")?;
                    renderer.push_str(variant_name)
                }
                (Some(ty), Some(payload)) => {
                    renderer.push_str(name)?;
                    renderer.push_str("::")?;
                    renderer.push_str(variant_name)?;
                    renderer.push_char('(')?;
                    render_repl_value_inner(
                        renderer,
                        reader,
                        payload.runtime_value(),
                        ty,
                        ir,
                        depth + 1,
                    )?;
                    renderer.push_char(')')
                }
                _ => Err(ReplSessionError::Internal(format!(
                    "`{name}::{variant_name}` runtime payload disagrees with Typed IR"
                ))),
            }
        }
        _ => {
            renderer.push_char('<')?;
            renderer.push_str(name)?;
            renderer.push_char('>')
        }
    }
}

fn validate_runtime_variant(
    actual: nexa_core::StableId,
    expected: nexa_core::StableId,
    name: &str,
) -> Result<(), ReplSessionError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ReplSessionError::Internal(format!(
            "`{name}` runtime variant identity disagrees with Typed IR"
        )))
    }
}

fn validate_repl_runtime_value(
    value: RuntimeValue,
    ty: &IrType,
    ir: &TypedPackageIr,
) -> Result<(), ReplSessionError> {
    if matches!(ty, IrType::Unit) {
        return if matches!(value, RuntimeValue::Unit) {
            Ok(())
        } else {
            Err(ReplSessionError::Internal(
                "runtime value disagrees with the analyzed REPL Unit type".into(),
            ))
        };
    }
    let expected = repl_bytecode_value_type(ty, ir)?;
    let actual = runtime_value_type(value);
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(ReplSessionError::Internal(format!(
            "runtime value type {actual:?} disagrees with analyzed REPL type {expected:?}"
        )))
    }
}

fn runtime_value_type(value: RuntimeValue) -> Option<ValueType> {
    match value {
        RuntimeValue::I32(_) => Some(ValueType::I32),
        RuntimeValue::I64(_) => Some(ValueType::I64),
        RuntimeValue::F32(_) => Some(ValueType::F32),
        RuntimeValue::F64(_) => Some(ValueType::F64),
        RuntimeValue::Bool(_) => Some(ValueType::Bool),
        RuntimeValue::Rune(_) => Some(ValueType::Rune),
        RuntimeValue::String { .. } => Some(ValueType::String),
        RuntimeValue::Struct { type_id, .. } => Some(ValueType::Named(type_id)),
        RuntimeValue::Ref(_) => Some(ValueType::Ref),
        RuntimeValue::NamedRef { type_id, .. } | RuntimeValue::Opaque { type_id, .. } => {
            Some(ValueType::Named(type_id))
        }
        RuntimeValue::StateHandle { handle_type, .. } => Some(ValueType::Named(handle_type)),
        RuntimeValue::HostRequest(_) => Some(ValueType::Named(nexa_core::StableId::from_name(
            "HostRequest",
        ))),
        RuntimeValue::ResourceToken(token) => Some(ValueType::Named(token.token_type())),
        RuntimeValue::Snapshot(snapshot) => Some(ValueType::Named(snapshot.type_id())),
        RuntimeValue::MigrationOldObject(_)
        | RuntimeValue::MigrationStagingObject(_)
        | RuntimeValue::Unit => None,
    }
}

fn expected_named_runtime_type_id(
    ty: &IrType,
    ir: &TypedPackageIr,
) -> Result<nexa_core::StableId, ReplSessionError> {
    match repl_bytecode_value_type(ty, ir)? {
        ValueType::Named(type_id) => Ok(type_id),
        actual => Err(ReplSessionError::Internal(format!(
            "aggregate REPL type lowered to non-nominal bytecode type {actual:?}"
        ))),
    }
}

#[allow(clippy::too_many_lines)]
fn repl_bytecode_value_type(
    ty: &IrType,
    ir: &TypedPackageIr,
) -> Result<ValueType, ReplSessionError> {
    match ty {
        IrType::Unit | IrType::I32 => Ok(ValueType::I32),
        IrType::I64 => Ok(ValueType::I64),
        IrType::F32 => Ok(ValueType::F32),
        IrType::F64 => Ok(ValueType::F64),
        IrType::Bool => Ok(ValueType::Bool),
        IrType::Rune => Ok(ValueType::Rune),
        IrType::String => Ok(ValueType::String),
        IrType::Named(definition) => named_definition_value_type(*definition, ir),
        IrType::Option(payload) => Ok(ValueType::Named(
            nexa_bytecode::option_type(repl_bytecode_value_type(payload, ir)?).type_id,
        )),
        IrType::Result(success, error) => Ok(ValueType::Named(
            nexa_bytecode::result_type(
                repl_bytecode_value_type(success, ir)?,
                repl_bytecode_value_type(error, ir)?,
            )
            .type_id,
        )),
        IrType::Array(element) => Ok(ValueType::Named(nexa_bytecode::array_type(
            repl_bytecode_value_type(element, ir)?,
        ))),
        IrType::Map(key, value) => Ok(ValueType::Named(nexa_bytecode::map_type(
            repl_bytecode_value_type(key, ir)?,
            repl_bytecode_value_type(value, ir)?,
        ))),
        IrType::Tuple(elements) => {
            let elements = elements
                .iter()
                .map(|element| repl_bytecode_value_type(element, ir))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ValueType::Named(nexa_bytecode::parameterized_type_id(
                "Tuple", &elements,
            )))
        }
        IrType::HostRequest(_) => Err(ReplSessionError::Internal(
            "a HostRequest escaped the typed await boundary into the REPL reader".into(),
        )),
        IrType::ResourceToken(Some(content)) => {
            let IrType::Named(definition) = content.as_ref() else {
                return Err(ReplSessionError::Internal(
                    "a Token with non-nominal content escaped Typed IR validation".into(),
                ));
            };
            let ValueType::Named(content_type) = named_definition_value_type(*definition, ir)?
            else {
                return Err(ReplSessionError::Internal(
                    "a Token content type lowered to a non-nominal runtime type".into(),
                ));
            };
            Ok(ValueType::Named(nexa_bytecode::resource_token_type(
                content_type,
            )))
        }
        IrType::ResourceToken(None) => Err(ReplSessionError::Internal(
            "an untyped Token escaped Typed IR validation into the REPL reader".into(),
        )),
        IrType::Snapshot(content) => {
            let content = repl_bytecode_value_type(content, ir)?;
            let content_type = match content {
                ValueType::Named(type_id) => type_id,
                scalar => nexa_bytecode::parameterized_type_id("SnapshotContent", &[scalar]),
            };
            Ok(ValueType::Named(nexa_bytecode::snapshot_type(content_type)))
        }
        IrType::Buffer(element) => Ok(ValueType::Named(nexa_bytecode::buffer_type(
            repl_bytecode_value_type(element, ir)?,
        ))),
        IrType::StateHandle(target) => Ok(ValueType::Named(nexa_bytecode::state_handle_type(
            repl_bytecode_value_type(target, ir)?,
        ))),
        IrType::TypeParameter(index) => Err(ReplSessionError::Internal(format!(
            "REPL result retained unresolved type parameter T{index}"
        ))),
        IrType::Error => Err(ReplSessionError::Internal(
            "an error type escaped into the REPL reader".into(),
        )),
    }
}

fn named_definition_value_type(
    definition: nexa_analysis::DefinitionId,
    ir: &TypedPackageIr,
) -> Result<ValueType, ReplSessionError> {
    if let Some(host_type) = ir
        .metadata()
        .host_bindings
        .iter()
        .flat_map(|host| host.types.iter())
        .find(|ty| ty.definition == definition)
    {
        return Ok(ValueType::Named(host_type.stable_id));
    }
    let definition_record = ir.definition(definition).ok_or_else(|| {
        ReplSessionError::Internal(format!(
            "REPL type refers to missing definition#{}",
            definition.0
        ))
    })?;
    if definition_record.kind == nexa_analysis::DefinitionKind::StandardLibrary
        && definition_record.module.as_str() == "nexa.builtin"
    {
        match definition_record.name.as_str() {
            "StableId" => return Ok(nexa_bytecode::stable_id_type()),
            "StateHandleError" => {
                return Ok(ValueType::Named(
                    nexa_bytecode::state_handle_error_type().type_id,
                ));
            }
            _ => {}
        }
    }
    Ok(ValueType::Named(source_definition_runtime_id(
        definition, ir,
    )?))
}

fn source_definition_runtime_id(
    definition: nexa_analysis::DefinitionId,
    ir: &TypedPackageIr,
) -> Result<nexa_core::StableId, ReplSessionError> {
    ir.definition(definition)
        .and_then(|definition| definition.stable_symbol.as_ref())
        .map(|stable| stable.runtime_id.0)
        .ok_or_else(|| {
            ReplSessionError::Internal(format!(
                "REPL definition#{} has no stable runtime identity",
                definition.0
            ))
        })
}

struct InitializedRuntime {
    realm: RealmRuntime,
    runtime_host: RuntimeHost,
    module: ModuleHandle,
    owner: ScopeHandle,
    contract_runtime_id: nexa_core::StableId,
    contract_fingerprint: nexa_contract::AbiFingerprint,
}

fn initialize_runtime(
    limits: ReplSessionLimits,
    console: Arc<Mutex<ReplConsoleState>>,
    builds: &mut PackageBuildSession,
) -> Result<InitializedRuntime, ReplSessionError> {
    let parsed_contract = nexa_contract::parse_contract(CONSOLE_HOST_CONTRACT).map_err(|error| {
        ReplSessionError::Internal(format!("invalid built-in Console NIDL: {error}"))
    })?;
    let contract = HostContractInput::with_source(
        &parsed_contract,
        SourceIdentity::standalone(CONSOLE_HOST_SOURCE_IDENTITY),
        CONSOLE_HOST_CONTRACT,
    )
    .map_err(|error| ReplSessionError::Build(PackageBuildError::HostContractSource(error)))?;
    let seed = builds
        .compile_repl_seed_with_contract(&contract, nexa_verifier::VerifierLimits::default())?;
    let registry = ConsoleRegistry::new(&parsed_contract, console)?;
    let contract_runtime_id = contract.runtime_id();
    let contract_fingerprint = nexa_contract::abi_descriptor(&parsed_contract).fingerprint;
    let runtime_host = RuntimeHost::new(32);
    let mut realm = RealmRuntime::hosted(
        RealmConfig {
            max_heap_objects: limits.max_heap_objects,
            ..RealmConfig::default()
        },
        runtime_host.clone(),
        Box::new(registry),
    )?;
    let module = realm.load_module(
        seed.verified,
        contract_runtime_id,
        seed.state_schema_fingerprint,
    )?;
    realm.initialize_transactional_state_seed(module, seed.environment)?;
    let owner = realm.create_scope(None)?;
    Ok(InitializedRuntime {
        realm,
        runtime_host,
        module,
        owner,
        contract_runtime_id,
        contract_fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use nexa_diagnostics::{
        ByteRange, Diagnostic, DiagnosticBatch, ErrorCode, Label, RelatedLocation, Severity,
        SourceIdentity, SourceSnapshotRegistry,
    };

    use super::{
        ReplConsoleEmission, ReplConsoleHost, ReplConsoleHostError, ReplConsoleState,
        ReplConsoleStream, ReplSessionError, ReplSourceOrigin, commit_console_cell,
        discard_console_cell, prepare_console_cell, remap_cell_diagnostics,
    };

    #[derive(Default)]
    struct FakeConsoleObservation {
        prepare_count: usize,
        commit_count: usize,
        discard_count: usize,
        published: Vec<Vec<ReplConsoleEmission>>,
    }

    struct FakeConsoleHost {
        observation: Arc<Mutex<FakeConsoleObservation>>,
        prepared: Vec<ReplConsoleEmission>,
        fail_prepare: bool,
    }

    impl ReplConsoleHost for FakeConsoleHost {
        fn prepare_cell(
            &mut self,
            output: &[ReplConsoleEmission],
        ) -> Result<(), ReplConsoleHostError> {
            self.observation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .prepare_count += 1;
            self.prepared.clear();
            self.prepared.extend_from_slice(output);
            if self.fail_prepare {
                Err(ReplConsoleHostError {
                    message: "prepare failed".into(),
                })
            } else {
                Ok(())
            }
        }

        fn commit_prepared_cell(&mut self) {
            let published = std::mem::take(&mut self.prepared);
            let mut observation = self
                .observation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            observation.commit_count += 1;
            observation.published.push(published);
        }

        fn discard_prepared_cell(&mut self) {
            self.prepared.clear();
            self.observation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .discard_count += 1;
        }
    }

    fn fake_console(
        fail_prepare: bool,
    ) -> (
        Arc<Mutex<ReplConsoleState>>,
        Arc<Mutex<FakeConsoleObservation>>,
        ReplConsoleEmission,
    ) {
        let observation = Arc::new(Mutex::new(FakeConsoleObservation::default()));
        let emission = ReplConsoleEmission {
            stream: ReplConsoleStream::Stdout,
            text: "ready".into(),
            line_terminated: true,
        };
        let console = Arc::new(Mutex::new(ReplConsoleState {
            host: Box::new(FakeConsoleHost {
                observation: Arc::clone(&observation),
                prepared: Vec::new(),
                fail_prepare,
            }),
            byte_limit: 1024,
            charged_bytes: 6,
            limit_error: None,
            pending: vec![emission.clone()],
        }));
        (console, observation, emission)
    }

    #[test]
    fn repeated_reader_uri_retains_exact_current_and_distinct_historical_snapshots() {
        let reader = SourceIdentity::package("nexa.repl", "load/shared.nexa");
        let historical_internal = SourceIdentity::package("nexa.repl", "repl/session/cell_1.nexa");
        let current_internal = SourceIdentity::package("nexa.repl", "repl/session/cell_2.nexa");
        let historical_text = Arc::<str>::from("let value = 1;");
        let current_text = Arc::<str>::from("let value = 2;");
        let mut committed = std::collections::BTreeMap::new();
        committed.insert(
            historical_internal.clone(),
            ReplSourceOrigin {
                display: reader.clone(),
                text: Arc::clone(&historical_text),
                ordinal: 1,
            },
        );
        let current = ReplSourceOrigin {
            display: reader.clone(),
            text: Arc::clone(&current_text),
            ordinal: 2,
        };

        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(historical_internal.clone(), Arc::clone(&historical_text))
            .expect("historical internal identity is unique");
        sources
            .insert(current_internal.clone(), Arc::clone(&current_text))
            .expect("current internal identity is unique");
        let mut diagnostics = DiagnosticBatch::with_default_limits(sources.build());
        diagnostics.push(
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "current failure")
                .with_label(Label::primary(
                    current_internal.clone(),
                    ByteRange::new(0, 3),
                    "current",
                ))
                .with_related(RelatedLocation::new(
                    historical_internal,
                    ByteRange::new(0, 3),
                    "historical",
                )),
        );

        let remapped =
            remap_cell_diagnostics(&diagnostics, &committed, &current_internal, &current)
                .expect("different revisions of one reader URI must remain representable");
        let historical_revision =
            SourceIdentity::package("nexa.repl", "load/shared.nexa#nexa-repl-cell-1");
        assert_eq!(
            remapped
                .sources()
                .get(&reader)
                .expect("current reader identity is retained")
                .text(),
            current_text.as_ref()
        );
        assert_eq!(
            remapped
                .sources()
                .get(&historical_revision)
                .expect("historical reader revision is retained")
                .text(),
            historical_text.as_ref()
        );
        assert_eq!(remapped.sources().len(), 2);
        let diagnostic = &remapped.diagnostics()[0];
        assert_eq!(diagnostic.labels[0].source, reader);
        assert_eq!(diagnostic.related[0].source, historical_revision);
    }

    #[test]
    fn failed_console_prepare_is_discarded_without_publication() {
        let (console, observation, _) = fake_console(true);
        assert!(matches!(
            prepare_console_cell(&console),
            Err(ReplSessionError::Console(_))
        ));
        discard_console_cell(&console).expect("failed preparation can be discarded");

        let observation = observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(observation.prepare_count, 1);
        assert_eq!(observation.discard_count, 1);
        assert_eq!(observation.commit_count, 0);
        assert!(observation.published.is_empty());
    }

    #[test]
    fn successful_console_prepare_only_publishes_once_committed() {
        let (console, observation, emission) = fake_console(false);
        prepare_console_cell(&console).expect("preparation succeeds");
        {
            let observation = observation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(observation.prepare_count, 1);
            assert_eq!(observation.commit_count, 0);
            assert!(observation.published.is_empty());
        }

        commit_console_cell(&console);
        let observation = observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(observation.commit_count, 1);
        assert_eq!(observation.discard_count, 0);
        assert_eq!(observation.published, vec![vec![emission]]);
    }
}
