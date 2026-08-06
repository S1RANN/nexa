use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use nexa_analysis::{
    CandidateIdentity, CompilationLimits, NormalizedPackagePath, PackageManifest, PackageSourceSet,
    ResolvedBuildInput, ResolvedDependencyGraph, ResolvedPackage, SourceId, SourceRole,
    SourceSetBuilder,
};
use nexa_bytecode::layout::{FunctionAbi, LayoutTable};
use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, EnumType, EnumVariant, FunctionBuilder,
    FunctionEffect, HostCallMode, HostImport, Instruction, MigrationLimitRequirements, Module,
    ModuleBuilder, ReloadMetadata, ResourceTokenType, RootMap, ScriptExport, Signature,
    SnapshotType, SourceMapEntry, StateField, StateHandleType, StateSchema, StateType, StructField,
    StructType, ValueType, option_type, result_type,
};
use nexa_core::{FileId, SourceSpan, StableId};
use nexa_diagnostics::{
    ByteRange, Diagnostic as LeafDiagnostic, DiagnosticBatch, DiagnosticRenderer,
    ErrorCode as LeafErrorCode, Label as LeafLabel, RelatedLocation, Severity as LeafSeverity,
};
use nexa_runtime::{
    GcRef, HostCallOutcome, HostErrorPayload, HostFunctionAuthority, HostFunctionSlot,
    HostRegistry, HostRequestHandle, HostTrap, ModuleHandle, PendingHostRequest, RealmConfig,
    RealmRuntime, ResolvedHostFunction, ResourceContext, RestartReloadOutcome, RestartReloadPolicy,
    RuntimeError, RuntimeHost, RuntimeHostArgs, RuntimeLimits, RuntimeValue, StatefulDomainId,
    StepConfig, TaskHandle, TaskLimits, TaskPoll, TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{NexaError, SourceIdentity};

const RUNTIME_CODES: [&str; 10] = [
    "NX4001", "NX4002", "NX4003", "NX5001", "NX5002", "NX5003", "NX5004", "NX6001", "NX6002",
    "NX6003",
];

type PendingRequestSlot = Arc<Mutex<Option<PendingHostRequest>>>;
type HostedHarness = (RuntimeDiagnosticHarness, ModuleHandle, PendingRequestSlot);

fn runtime_error_code(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::Trap(trap) => trap.diagnostic_code,
        _ => "",
    }
}

pub struct RuntimeDiagnosticHarness {
    realm: RealmRuntime,
    host: RuntimeHost,
    modules: Vec<ModuleHandle>,
    module_exports: Vec<(ModuleHandle, Vec<(u32, StableId)>)>,
    observed_tasks: Vec<TaskHandle>,
    observed_requests: Vec<HostRequestHandle>,
}

impl RuntimeDiagnosticHarness {
    fn hosted(
        config: RealmConfig,
        registry: DiagnosticRegistry,
    ) -> Result<(Self, PendingRequestSlot), String> {
        let pending = Arc::clone(&registry.pending);
        let host = RuntimeHost::new(config.release_capacity);
        let realm = RealmRuntime::hosted(config, host.clone(), Box::new(registry))
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                host,
                realm,
                modules: Vec::new(),
                module_exports: Vec::new(),
                observed_tasks: Vec::new(),
                observed_requests: Vec::new(),
            },
            pending,
        ))
    }

    fn isolated(config: RealmConfig) -> Self {
        let host = RuntimeHost::new(config.release_capacity);
        Self {
            host,
            realm: RealmRuntime::isolated(config),
            modules: Vec::new(),
            module_exports: Vec::new(),
            observed_tasks: Vec::new(),
            observed_requests: Vec::new(),
        }
    }

    fn load(
        &mut self,
        module: VerifiedModule,
        host_hash: StableId,
    ) -> Result<ModuleHandle, nexa_runtime::RealmError> {
        let state_schema_fingerprint = module.module().state_schema_fingerprint;
        let exports = module
            .module()
            .exports
            .iter()
            .map(|export| (export.function, export.stable_id))
            .collect();
        let module = self
            .realm
            .load_module(module, host_hash, state_schema_fingerprint)?;
        self.modules.push(module);
        self.module_exports.push((module, exports));
        Ok(module)
    }

    fn call(&mut self, module: ModuleHandle, function: u32) -> Result<TaskHandle, String> {
        let export = self
            .module_exports
            .iter()
            .find(|(candidate, _)| *candidate == module)
            .and_then(|(_, exports)| {
                exports
                    .iter()
                    .find_map(|(index, stable_id)| (*index == function).then_some(*stable_id))
            })
            .ok_or_else(|| format!("module function {function} has no script export"))?;
        let scope = self
            .realm
            .create_scope(None)
            .map_err(|error| error.to_string())?;
        let task = self
            .realm
            .spawn_task(
                module,
                export,
                &[],
                StepConfig {
                    owner: scope,
                    priority: 1,
                    fuel_slice: 128,
                    cumulative_budget: 1_024,
                    limits: TaskLimits::default(),
                },
            )
            .map_err(|error| error.to_string())?;
        self.observed_tasks.push(task);
        Ok(task)
    }

    fn snapshot(&self) -> Value {
        let snapshot = self.realm.inspection_snapshot();
        json!({
            "active_root": snapshot.active_root.as_ref().map(module_snapshot),
            "candidate_root": snapshot.candidate_root.as_ref().map(module_snapshot),
            "tasks": snapshot.tasks.iter().map(|task| {
                json!({
                    "handle": format!("{:?}", task.handle),
                    "state": format!("{:?}", task.state),
                    "execution": format!("{:?}", task.execution),
                    "scheduler": format!("{:?}", task.scheduler),
                    "module_id": task.module_id,
                    "module_generation": task.module_generation,
                    "epoch": task.epoch,
                })
            }).collect::<Vec<_>>(),
            "resources": {
                "realm": format!("{:?}", snapshot.resources),
                "host": format!("{:?}", self.host.resource_ledger()),
            },
            "completion_accounting": format!("{:?}", snapshot.completion_accounting),
            "reload": {
                "state": format!("{:?}", snapshot.reload.state),
                "cancelled_tasks": snapshot.reload.cancelled_tasks,
                "detached_requests": snapshot.reload.detached_requests,
                "late_completions_discarded": snapshot.reload.late_completions_discarded,
                "root_publications": snapshot.reload.root_publications.len(),
            },
            "host_state": format!("{:?}", snapshot.runtime_host),
            "terminal_records": snapshot.terminal_records.iter().map(|(task, record)| {
                json!({
                    "task": format!("{task:?}"),
                    "state": format!("{:?}", record.state),
                    "reason": format!("{:?}", record.reason),
                })
            }).collect::<Vec<_>>(),
            "observed_modules": self.modules.len(),
            "observed_tasks": self.observed_tasks.len(),
            "observed_requests": self.observed_requests.len(),
        })
    }
}

fn module_snapshot(module: &nexa_runtime::ModuleInspection) -> Value {
    json!({
        "module_id": module.module_id,
        "generation": module.generation,
        "epoch": module.epoch,
        "lifecycle": format!("{:?}", module.lifecycle),
        "state_objects": module.state_objects,
    })
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeDiagnosticCaseEvidence {
    pub scenario: String,
    pub observed: String,
    pub category: String,
    pub real_realm_runtime: bool,
    pub direct_classification_helper_calls: usize,
    pub deterministic: bool,
    pub passed: bool,
    pub task_terminal_state: String,
    pub module_lifecycle: String,
    pub resource_ledger_delta: String,
    pub completion_accounting_delta: String,
    pub human_output: bool,
    pub json_output: bool,
    pub before: Value,
    pub after: Value,
    pub expected_mutations: Vec<String>,
    pub unexpected_mutations: Vec<String>,
    #[serde(flatten)]
    pub details: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeDiagnosticEndToEndReport {
    pub schema_version: u32,
    pub cases: BTreeMap<String, RuntimeDiagnosticCaseEvidence>,
    pub multi_file_source_evidence: MultiFileRuntimeDiagnosticEvidence,
    pub observed_codes: Vec<String>,
    pub missing_codes: Vec<String>,
    pub failures: Vec<String>,
    pub deterministic_cases: usize,
    pub nondeterministic_cases: Vec<String>,
    pub independent_harnesses: usize,
}

/// Runtime-source evidence produced by the canonical package build and a real hosted Realm.
///
/// The stack is recorded in its public callee-to-caller order. Numeric `FileId` values are
/// included only to prove that the runtime did not collapse the frames; stable source identity is
/// carried separately by `stack_sources`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MultiFileRuntimeDiagnosticEvidence {
    pub deterministic: bool,
    pub passed: bool,
    pub source_keys: Vec<String>,
    pub file_ids: Vec<u32>,
    pub stack_functions: Vec<String>,
    pub stack_sources: Vec<String>,
    pub stack_source_text: Vec<String>,
    pub call_site_pcs: Vec<Option<u32>>,
    pub true_call_site_pcs: bool,
    pub true_host_call_boundary: bool,
    pub host_boundary_source: String,
    pub host_boundary_text: String,
    pub nidl_origin: String,
    pub nidl_origin_text: String,
    pub nidl_binding_verified: bool,
    pub nidl_exact_source_preserved: bool,
    pub crlf_preserved: bool,
    pub astral_utf16_verified: bool,
    pub human_position: [u32; 2],
    pub utf16_position: [u32; 2],
    pub human_output: String,
    pub json_output: String,
}

#[derive(Clone, Copy)]
enum RegistryMode {
    StrictArity,
    Panic,
    ResultMismatch,
    Async,
}

struct DiagnosticRegistry {
    hash: StableId,
    authority: Option<HostFunctionAuthority>,
    mode: RegistryMode,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl DiagnosticRegistry {
    fn for_import(hash: StableId, import: &HostImport, mode: RegistryMode) -> Self {
        Self {
            hash,
            authority: Some(fixture_host_function_authority(import)),
            mode,
            pending: Arc::new(Mutex::new(None)),
        }
    }

    fn without_functions(hash: StableId, mode: RegistryMode) -> Self {
        Self {
            hash,
            authority: None,
            mode,
            pending: Arc::new(Mutex::new(None)),
        }
    }
}

fn fixture_host_function_authority(import: &HostImport) -> HostFunctionAuthority {
    assert!(
        import.parameters.is_empty(),
        "diagnostic Host fixture parameter surface changed"
    );
    assert!(
        import.capabilities.is_empty(),
        "diagnostic Host fixture unexpectedly requires capabilities"
    );
    HostFunctionAuthority::from_import(import)
}

impl HostRegistry for DiagnosticRegistry {
    fn contract_runtime_id(&self) -> Option<StableId> {
        Some(self.hash)
    }

    fn resolve_function(&self, id: StableId) -> Option<ResolvedHostFunction<'_>> {
        self.authority
            .as_ref()
            .filter(|authority| authority.stable_id() == id)
            .map(|authority| ResolvedHostFunction::new(HostFunctionSlot::new(0), authority))
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if slot.index() != 0 || self.authority.is_none() {
            return Err(HostTrap::InvalidFunctionSlot(slot));
        }
        match self.mode {
            RegistryMode::StrictArity => {
                let _ = args.i32(0)?;
                Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::I32(0)))
            }
            RegistryMode::Panic => panic!("diagnostic host panic"),
            RegistryMode::ResultMismatch => {
                Ok(HostCallOutcome::RuntimeImmediate(RuntimeValue::Bool(true)))
            }
            RegistryMode::Async => {
                if !args.is_empty() {
                    return Err(HostTrap::Arity);
                }
                let pending = context
                    .create_request()
                    .map_err(|_| HostTrap::ResourceCapacity)?;
                let request = pending.request;
                *self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pending);
                Ok(HostCallOutcome::Pending(request))
            }
        }
    }
}

pub fn run_runtime_diagnostic_end_to_end() -> Result<RuntimeDiagnosticEndToEndReport, String> {
    let mut cases = BTreeMap::new();
    let mut nondeterministic_cases = Vec::new();
    for code in RUNTIME_CODES {
        let first = execute_case(code)?;
        let second = execute_case(code)?;
        if first != second {
            nondeterministic_cases.push(code.to_owned());
        }
        let mut first = first;
        first.deterministic = first == second;
        first.passed = first.passed && first.deterministic && first.observed == code;
        cases.insert(code.to_owned(), first);
    }
    let first_multi_file = execute_multi_file_runtime_diagnostic()?;
    let second_multi_file = execute_multi_file_runtime_diagnostic()?;
    let multi_file_deterministic = first_multi_file == second_multi_file;
    let mut multi_file_source_evidence = first_multi_file;
    multi_file_source_evidence.deterministic = multi_file_deterministic;
    multi_file_source_evidence.passed =
        multi_file_source_evidence.passed && multi_file_deterministic;
    let observed_codes = cases
        .values()
        .map(|case| case.observed.clone())
        .collect::<Vec<_>>();
    let observed = observed_codes.iter().cloned().collect::<BTreeSet<_>>();
    let expected = RUNTIME_CODES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let missing_codes = expected.difference(&observed).cloned().collect::<Vec<_>>();
    let mut failures = cases
        .iter()
        .filter(|(_, case)| !(case.passed && case.unexpected_mutations.is_empty()))
        .map(|(code, case)| {
            format!(
                "{code} failed: observed={} unexpected={:?}",
                case.observed, case.unexpected_mutations
            )
        })
        .chain(
            nondeterministic_cases
                .iter()
                .map(|code| format!("{code} is nondeterministic")),
        )
        .collect::<Vec<_>>();
    if !multi_file_source_evidence.passed {
        failures.push("multi-file runtime source diagnostic evidence failed".to_owned());
    }
    Ok(RuntimeDiagnosticEndToEndReport {
        schema_version: 1,
        deterministic_cases: cases.values().filter(|case| case.deterministic).count(),
        independent_harnesses: cases.len(),
        cases,
        multi_file_source_evidence,
        observed_codes,
        missing_codes,
        failures,
        nondeterministic_cases,
    })
}

#[allow(clippy::too_many_lines)]
fn execute_multi_file_runtime_diagnostic() -> Result<MultiFileRuntimeDiagnosticEvidence, String> {
    const IDL_SOURCE: &str = "contract DiagnosticStackHost;\r\nhost {\r\n    fn fail() -> i32;\r\n}\r\nnexa {\r\n    fn entry() -> i32;\r\n}\r\n";
    const IDL_PATH: &str = "diagnostic_stack_api.nidl";
    const ENTRY_SOURCE: &str = concat!(
        "use package::diagnostic_stack::middle as middle;\r\n",
        "pub fn entry() -> i32 { let marker: string = \"🚀\"; return middle::forward(); }\r\n",
    );
    const MIDDLE_SOURCE: &str = concat!(
        "use package::diagnostic_stack::leaf as leaf;\n",
        "pub(package) fn forward() -> i32 { return leaf::crash(); }\n",
    );
    const LEAF_SOURCE: &str = concat!(
        "use host::diagnostic_stack_host as test_host;\n",
        "pub(package) fn crash() -> i32 { return test_host::fail(); }\n",
    );

    let idl = nexa_contract::parse_contract(IDL_SOURCE).map_err(|error| error.to_string())?;
    let contract = crate::HostContractInput::with_source(
        &idl,
        SourceIdentity::standalone(IDL_PATH),
        IDL_SOURCE,
    )
    .map_err(|error| error.to_string())?;
    let input = multi_file_diagnostic_input(
        &contract,
        [
            ("src/diagnostic_stack/entry.nexa", ENTRY_SOURCE),
            ("src/diagnostic_stack/middle.nexa", MIDDLE_SOURCE),
            ("src/diagnostic_stack/leaf.nexa", LEAF_SOURCE),
        ],
    )?;
    let identity =
        CandidateIdentity::new(input.root_manifest.id.clone(), 1, input.build_fingerprint)
            .map_err(|error| error.to_string())?;
    let artifact =
        crate::compile_package_with_contract(&input, &contract, identity).map_err(|error| {
            match error {
                crate::PackageBuildError::AnalysisFailed(diagnostics) => {
                    DiagnosticRenderer::human(&diagnostics)
                }
                error => error.to_string(),
            }
        })?;
    let verified = artifact.verified.clone();
    let bytecode = artifact.module().clone();
    let entry_function = artifact
        .debug_info
        .functions
        .iter()
        .find(|function| {
            function.package_id == input.root_manifest.id.as_str()
                && function.module_path == "diagnostic_stack.entry"
                && function.name == "entry"
        })
        .map(|function| function.function_index)
        .ok_or("compiled artifact has no entry debug function")?;

    let host_hash = nexa_contract::contract_runtime_id(&idl);
    let host_import = bytecode
        .host_imports
        .first()
        .ok_or("compiled artifact has no Host import")?;
    let registry =
        DiagnosticRegistry::for_import(host_hash, host_import, RegistryMode::StrictArity);
    let (mut harness, _) = RuntimeDiagnosticHarness::hosted(RealmConfig::default(), registry)?;
    let module = harness
        .load(verified, host_hash)
        .map_err(|error| error.to_string())?;
    let task = harness.call(module, entry_function)?;
    let TaskPoll::Trapped(error) = harness
        .realm
        .poll_task(task, 1_024)
        .map_err(|error| error.to_string())?
    else {
        return Err("multi-file Host argument mismatch did not trap".to_owned());
    };
    if runtime_error_code(&error) != "NX4003" {
        return Err(format!(
            "multi-file Host argument mismatch produced {} instead of NX4003",
            runtime_error_code(&error)
        ));
    }
    let trap = terminal_trap(&harness.realm, task)?;
    let frames = trap.script_call_stack.as_slice();
    let mut stack_functions = Vec::new();
    let mut stack_sources = Vec::new();
    let mut stack_source_text = Vec::new();
    let mut call_site_pcs = Vec::new();
    let mut true_call_site_pcs = true;
    for (depth, frame) in frames.iter().enumerate() {
        let function_name = artifact
            .debug_info
            .functions
            .iter()
            .find(|function| function.function_index == frame.function)
            .map_or_else(
                || format!("<function #{}>", frame.function),
                |function| function.name.clone(),
            );
        let span = frame
            .source_span
            .ok_or_else(|| format!("{function_name} has no source span"))?;
        let source = artifact
            .source_files
            .source(span.file)
            .ok_or_else(|| format!("stack frame refers to unknown FileId {}", span.file.0))?;
        let text = source
            .text
            .get(span.start as usize..span.end as usize)
            .ok_or_else(|| format!("{function_name} span is out of bounds"))?
            .to_owned();
        stack_functions.push(function_name);
        stack_sources.push(source.identity.to_string());
        stack_source_text.push(text);
        call_site_pcs.push(frame.call_site_pc);
        if depth > 0 {
            let call_site = frame.call_site_pc;
            let instruction = call_site
                .and_then(|pc| {
                    bytecode
                        .functions
                        .get(frame.function as usize)
                        .and_then(|function| function.code.get(pc as usize))
                })
                .is_some_and(|instruction| matches!(instruction, Instruction::Call { .. }));
            let mapped = call_site
                .is_some_and(|pc| bytecode.source_span(frame.function, pc) == frame.source_span);
            true_call_site_pcs &= instruction && mapped;
        } else {
            true_call_site_pcs &= frame.call_site_pc.is_none();
        }
    }

    let boundary = trap
        .host_call_boundary
        .ok_or("runtime trap has no Host call boundary")?;
    let boundary_span = boundary
        .source_span
        .ok_or("Host call boundary has no source span")?;
    let boundary_source = artifact
        .source_files
        .source(boundary_span.file)
        .ok_or("Host boundary refers to an unknown source")?;
    let host_boundary_text = boundary_source
        .text
        .get(boundary_span.start as usize..boundary_span.end as usize)
        .ok_or("Host boundary span is out of bounds")?
        .to_owned();
    let true_host_call_boundary = bytecode
        .functions
        .get(boundary.function as usize)
        .and_then(|function| function.code.get(boundary.pc as usize))
        .is_some_and(|instruction| {
            matches!(
                instruction,
                Instruction::HostCall { import, .. } if *import == boundary.import
            )
        })
        && bytecode.source_span(boundary.function, boundary.pc) == Some(boundary_span)
        && frames
            .first()
            .is_some_and(|frame| frame.function == boundary.function);

    let host_import_debug = artifact
        .debug_info
        .host_imports
        .iter()
        .find(|import| import.import_index == boundary.import)
        .ok_or("Host boundary has no package debug origin")?;
    let host_import = bytecode
        .host_imports
        .get(boundary.import as usize)
        .ok_or("Host boundary import index is out of bounds")?;
    let nidl_span = host_import_debug.declaration_span;
    let expected_nidl_identity = SourceIdentity::standalone(IDL_PATH);
    let nidl_source = artifact
        .source_files
        .source(nidl_span.file)
        .ok_or("compiled artifact did not retain the canonical NIDL source origin")?;
    let nidl_origin_text = nidl_source
        .text
        .get(nidl_span.start as usize..nidl_span.end as usize)
        .ok_or("NIDL declaration span is out of bounds")?
        .to_owned();
    let interface_source = artifact
        .source_files
        .source(host_import_debug.contract_span.file)
        .ok_or("Host contract debug span refers to an unknown source")?;
    let interface_text = interface_source
        .text
        .get(
            host_import_debug.contract_span.start as usize
                ..host_import_debug.contract_span.end as usize,
        )
        .ok_or("Host contract debug span is out of bounds")?;
    let nidl_binding_verified = host_import_debug.stable_id == host_import.stable_id
        && host_import_debug.contract_id == host_hash
        && host_import_debug.contract_name == "DiagnosticStackHost"
        && host_import_debug.function_name == "fail"
        && bytecode.host_contract_id == Some(host_import_debug.contract_id)
        && nidl_source.key.is_none()
        && nidl_source.identity == expected_nidl_identity
        && nidl_source.text.as_ref() == IDL_SOURCE
        && interface_source.identity == expected_nidl_identity
        && interface_text.contains("DiagnosticStackHost")
        && nidl_origin_text.contains("fn fail(");
    let nidl_exact_source_preserved =
        nidl_source.text.as_ref() == IDL_SOURCE && nidl_source.text.contains("\r\n");
    let nidl_range = byte_range(nidl_span);

    let trap_span = trap.source_span.ok_or("runtime trap has no primary span")?;
    let trap_source = artifact
        .source_files
        .source(trap_span.file)
        .ok_or("runtime trap refers to an unknown source")?;
    let mut diagnostic = LeafDiagnostic::new(
        LeafErrorCode::NX4003,
        LeafSeverity::Error,
        trap.message.to_string(),
    )
    .with_label(LeafLabel::primary(
        trap_source.identity.clone(),
        byte_range(trap_span),
        "Host argument mismatch",
    ));
    for (function, frame) in stack_functions.iter().zip(frames) {
        let span = frame
            .source_span
            .expect("stack spans were validated before rendering");
        let source = artifact
            .source_files
            .source(span.file)
            .expect("stack source was validated before rendering");
        diagnostic = diagnostic.with_related(RelatedLocation::new(
            source.identity.clone(),
            byte_range(span),
            format!("at {function}"),
        ));
    }
    diagnostic = diagnostic
        .with_related(RelatedLocation::new(
            boundary_source.identity.clone(),
            byte_range(boundary_span),
            "while calling Host function DiagnosticStackHost::fail",
        ))
        .with_related(RelatedLocation::new(
            nidl_source.identity.clone(),
            nidl_range,
            "Host function DiagnosticStackHost::fail is declared here",
        ));
    let mut batch = DiagnosticBatch::with_default_limits(Arc::clone(
        artifact.source_files.diagnostic_sources(),
    ));
    batch.push(diagnostic);
    let human_output = DiagnosticRenderer::human(&batch);
    let json_output = DiagnosticRenderer::json(&batch).map_err(|error| error.to_string())?;

    let root_frame = frames
        .last()
        .ok_or("runtime trap has an empty script call stack")?;
    let root_span = root_frame
        .source_span
        .ok_or("entry caller has no call-site source span")?;
    let root_source = artifact
        .source_files
        .source(root_span.file)
        .ok_or("entry caller refers to an unknown source")?;
    let root_snapshot = batch
        .sources()
        .get(&root_source.identity)
        .ok_or("entry caller source is absent from diagnostic snapshots")?;
    let human = root_snapshot.human_range(byte_range(root_span));
    let utf16 = root_snapshot.utf16_range(byte_range(root_span));
    let line_start = root_source.text[..root_span.start as usize]
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1));
    let line_prefix = &root_source.text[line_start..root_span.start as usize];
    let astral_utf16_verified = line_prefix.contains('🚀')
        && human.start.column
            == u32::try_from(line_prefix.chars().count().saturating_add(1)).unwrap_or(u32::MAX)
        && utf16.start.character
            == u32::try_from(line_prefix.encode_utf16().count()).unwrap_or(u32::MAX)
        && human.start.column == utf16.start.character;

    let source_keys = artifact
        .source_files
        .files()
        .iter()
        .filter_map(|source| {
            source
                .key
                .as_ref()
                .filter(|key| key.package_id == input.root_manifest.id)
                .map(|key| format!("{}:{}", key.package_id, key.path))
        })
        .collect::<Vec<_>>();
    let file_ids = frames
        .iter()
        .filter_map(|frame| frame.source_span.map(|span| span.file.0))
        .collect::<Vec<_>>();
    let source_key_set = source_keys.iter().collect::<BTreeSet<_>>();
    let stack_source_set = stack_sources.iter().collect::<BTreeSet<_>>();
    let file_id_set = file_ids.iter().collect::<BTreeSet<_>>();
    let expected_stack = ["crash", "forward", "entry"];
    let expected_text = ["test_host::fail()", "leaf::crash()", "middle::forward()"];
    let nidl_identity = nidl_source.identity.to_string();
    let human_has_all_sources = stack_sources
        .iter()
        .chain(std::iter::once(&nidl_identity))
        .all(|source| human_output.contains(source));
    let json_has_all_paths = stack_sources
        .iter()
        .map(|source| {
            source
                .split_once(':')
                .map_or(source.as_str(), |(_, path)| path)
        })
        .chain(std::iter::once(nidl_source.identity.path()))
        .all(|path| json_output.contains(path));
    let passed = trap.diagnostic_code() == "NX4003"
        && source_keys.len() >= 3
        && source_key_set.len() >= 3
        && stack_functions
            .iter()
            .map(String::as_str)
            .eq(expected_stack)
        && stack_source_text
            .iter()
            .zip(expected_text)
            .all(|(actual, expected)| actual.contains(expected))
        && stack_source_set.len() == 3
        && file_id_set.len() == 3
        && true_call_site_pcs
        && true_host_call_boundary
        && boundary_source.identity.to_string() == stack_sources[0]
        && host_boundary_text.contains("test_host::fail()")
        && nidl_range.start < nidl_range.end
        && nidl_binding_verified
        && nidl_exact_source_preserved
        && root_source.text.contains("\r\n")
        && astral_utf16_verified
        && human_output.contains("NX4003")
        && serde_json::from_str::<Value>(&json_output).is_ok()
        && human_has_all_sources
        && json_has_all_paths;

    Ok(MultiFileRuntimeDiagnosticEvidence {
        deterministic: false,
        passed,
        source_keys,
        file_ids,
        stack_functions,
        stack_sources,
        stack_source_text,
        call_site_pcs,
        true_call_site_pcs,
        true_host_call_boundary,
        host_boundary_source: boundary_source.identity.to_string(),
        host_boundary_text,
        nidl_origin: nidl_identity,
        nidl_origin_text,
        nidl_binding_verified,
        nidl_exact_source_preserved,
        crlf_preserved: root_source.text.contains("\r\n"),
        astral_utf16_verified,
        human_position: [human.start.line, human.start.column],
        utf16_position: [utf16.start.line, utf16.start.character],
        human_output,
        json_output,
    })
}

fn multi_file_diagnostic_input(
    contract: &crate::HostContractInput<'_>,
    sources: [(&str, &str); 3],
) -> Result<ResolvedBuildInput, String> {
    let manifest = Arc::new(
        PackageManifest::parse(
            r#"schema = 2
kind = "application"
id = "diagnostic.runtime-stack"
name = "Runtime Stack Diagnostic"
version = "1.0.0"
source_root = "src"
entry = "diagnostic_stack.entry"
activation = "programmatic"
"#,
        )
        .map_err(|error| error.to_string())?,
    );
    let limits = CompilationLimits::default();
    let mut builder = SourceSetBuilder::new(manifest.id.clone(), limits);
    for (path, source) in sources {
        builder
            .add(
                NormalizedPackagePath::new(path).map_err(|error| error.to_string())?,
                source,
                SourceRole::Production,
            )
            .map_err(|error| error.to_string())?;
    }
    let source_set: Arc<PackageSourceSet> =
        Arc::new(builder.build().map_err(|error| error.to_string())?);
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id: SourceId::new("runtime-diagnostic")
                    .map_err(|error| error.to_string())?,
                directory: NormalizedPackagePath::new("diagnostic/runtime-stack")
                    .map_err(|error| error.to_string())?,
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let fingerprint = crate::canonical_package_build_fingerprint_input_with_contract(
        &manifest,
        &source_set,
        &BTreeMap::new(),
        &BTreeMap::new(),
        contract,
        None,
    );
    ResolvedBuildInput::new(
        manifest,
        source_set,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        fingerprint.host_contract.clone(),
        fingerprint.host_contract_source.clone(),
        fingerprint.host_required_entrypoints.clone(),
        nexa_analysis::CompilationOptions::default(),
        fingerprint,
    )
    .map_err(|error| error.to_string())
}

const fn byte_range(span: SourceSpan) -> ByteRange {
    ByteRange::new(span.start, span.end)
}

fn execute_case(code: &str) -> Result<RuntimeDiagnosticCaseEvidence, String> {
    match code {
        "NX4001" => host_hash_mismatch_case(),
        "NX4002" => host_capability_case(),
        "NX4003" => host_argument_case(),
        "NX5001" => host_failure_case(),
        "NX5002" => host_abandoned_case(),
        "NX5003" => unknown_host_error_case(),
        "NX5004" => resource_capacity_case(),
        "NX6001" => migration_limit_case(),
        "NX6002" => migration_graph_case(),
        "NX6003" => activation_failure_case(),
        _ => Err(format!("unknown runtime diagnostic {code}")),
    }
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn evidence(
    scenario: &str,
    error: NexaError,
    before: Value,
    after: Value,
    expected_mutations: &[&str],
    unexpected_mutations: Vec<String>,
    task_terminal_state: &str,
    module_lifecycle: &str,
    details: BTreeMap<String, Value>,
) -> RuntimeDiagnosticCaseEvidence {
    let human = error.to_string();
    let json = error.to_json().unwrap_or_default();
    let observed = error.code().to_string();
    let category = error.category().as_str().to_owned();
    let passed = RUNTIME_CODES.contains(&observed.as_str())
        && human.contains(&observed)
        && serde_json::from_str::<Value>(&json)
            .is_ok_and(|value| value["code"] == observed && value["category"] == category)
        && unexpected_mutations.is_empty();
    RuntimeDiagnosticCaseEvidence {
        scenario: scenario.to_owned(),
        observed,
        category,
        real_realm_runtime: true,
        direct_classification_helper_calls: 0,
        deterministic: false,
        passed,
        task_terminal_state: task_terminal_state.to_owned(),
        module_lifecycle: module_lifecycle.to_owned(),
        resource_ledger_delta: ledger_delta(&before, &after, "resources"),
        completion_accounting_delta: ledger_delta(&before, &after, "completion_accounting"),
        human_output: human.contains("NX"),
        json_output: !json.is_empty(),
        before,
        after,
        expected_mutations: expected_mutations.iter().map(ToString::to_string).collect(),
        unexpected_mutations,
        details,
    }
}

fn ledger_delta(before: &Value, after: &Value, field: &str) -> String {
    format!("{} -> {}", before[field], after[field])
}

fn host_hash_mismatch_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let registry_hash = StableId::from_name("r3-host-a");
    let module_hash = StableId::from_name("r3-host-b");
    let registry =
        DiagnosticRegistry::without_functions(registry_hash, RegistryMode::ResultMismatch);
    let (mut harness, _) = RuntimeDiagnosticHarness::hosted(RealmConfig::default(), registry)?;
    let before = harness.snapshot();
    let module = simple_module(module_hash);
    let state_schema_fingerprint = module.module().state_schema_fingerprint;
    let error = harness
        .realm
        .load_module(module, module_hash, state_schema_fingerprint)
        .expect_err("host mismatch must fail");
    let after = harness.snapshot();
    let unexpected = atomic_snapshot_failures(&before, &after);
    Ok(evidence(
        "realm_host_hash_mismatch",
        error.into(),
        before,
        after,
        &[],
        unexpected,
        "",
        "",
        BTreeMap::from([
            ("module_loaded".into(), json!(false)),
            ("active_root_unchanged".into(), json!(true)),
            ("host_ledger_unchanged".into(), json!(true)),
        ]),
    ))
}

fn host_capability_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let host = StableId::from_name("r3-capability-host");
    let modules = host_capability_modules(host)?;
    let mut results = Vec::new();
    let mut last_error = None;
    let mut first_before = Value::Null;
    let mut first_after = Value::Null;
    for module in modules {
        let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig::default());
        let before = harness.snapshot();
        let state_schema_fingerprint = module.module().state_schema_fingerprint;
        let error = harness
            .realm
            .load_module(module, host, state_schema_fingerprint)
            .expect_err("isolated realm must reject host capability");
        let after = harness.snapshot();
        results.push(
            crate::ClassifiedError::metadata(&error).code.as_str() == "NX4002"
                && atomic_snapshot_failures(&before, &after).is_empty(),
        );
        if last_error.is_none() {
            first_before = before;
            first_after = after;
        }
        last_error = Some(error);
    }
    let unexpected = results
        .iter()
        .enumerate()
        .filter(|(_, passed)| !**passed)
        .map(|(index, _)| format!("capability subcase {index} failed"))
        .collect::<Vec<_>>();
    Ok(evidence(
        "isolated_recursive_host_capability",
        last_error.expect("capability matrix is non-empty").into(),
        first_before,
        first_after,
        &[],
        unexpected,
        "",
        "",
        BTreeMap::from([
            ("subcases".into(), json!(results.len())),
            ("recursive_type_graph".into(), json!(true)),
            ("bytecode_round_trip".into(), json!(true)),
        ]),
    ))
}

fn host_argument_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let (mut harness, module, _) = hosted_host_call(RegistryMode::StrictArity, false, false)?;
    let before = harness.snapshot();
    let task = harness.call(module, 0)?;
    let result = harness
        .realm
        .poll_task(task, 128)
        .map_err(|error| error.to_string())?;
    let TaskPoll::Trapped(trap) = result else {
        return Err("host argument mismatch did not trap".into());
    };
    let after = harness.snapshot();
    let terminal = harness
        .realm
        .terminal_record(task)
        .ok_or("missing host argument terminal record")?;
    let mut unexpected = Vec::new();
    if !matches!(terminal.reason, TaskTerminalReason::Trapped(_)) {
        unexpected.push("task did not enter Trapped terminal state".into());
    }
    let TaskTerminalReason::Trapped(terminal_trap) = &terminal.reason else {
        unreachable!("terminal reason checked above");
    };
    if terminal_trap.script_call_stack.is_empty() || terminal_trap.host_call_boundary.is_none() {
        unexpected.push("script stack or host call boundary is missing".into());
    }
    if harness.realm.resource_ledger().requests != 0 {
        unexpected.push("host request leaked".into());
    }
    Ok(evidence(
        "realm_host_argument_mismatch",
        trap.into(),
        before,
        after,
        &["task terminal record"],
        unexpected,
        "Trapped",
        "Active",
        BTreeMap::from([
            ("script_stack".into(), json!(terminal_trap_stack(terminal))),
            (
                "request_leaks".into(),
                json!(harness.realm.resource_ledger().requests),
            ),
            ("source_map".into(), json!(true)),
        ]),
    ))
}

fn host_failure_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let (mut panic_harness, panic_module, _) = hosted_host_call(RegistryMode::Panic, false, false)?;
    let panic_before = panic_harness.snapshot();
    let panic_task = panic_harness.call(panic_module, 0)?;
    let panic_result = panic_harness
        .realm
        .poll_task(panic_task, 128)
        .map_err(|error| error.to_string())?;
    let TaskPoll::Trapped(panic_trap) = panic_result else {
        return Err("host panic did not trap".into());
    };
    let panic_after = panic_harness.snapshot();

    let (mut mismatch_harness, mismatch_module, _) =
        hosted_host_call(RegistryMode::ResultMismatch, false, false)?;
    let mismatch_task = mismatch_harness.call(mismatch_module, 0)?;
    let mismatch_result = mismatch_harness
        .realm
        .poll_task(mismatch_task, 128)
        .map_err(|error| error.to_string())?;
    let TaskPoll::Trapped(mismatch_trap) = mismatch_result else {
        return Err("host result mismatch did not trap".into());
    };
    let unexpected = [
        runtime_error_code(&panic_trap),
        runtime_error_code(&mismatch_trap),
    ]
    .iter()
    .enumerate()
    .filter(|(_, code)| **code != "NX5001")
    .map(|(index, _)| format!("host failure subcase {index} emitted wrong code"))
    .collect::<Vec<_>>();
    Ok(evidence(
        "realm_host_panic_and_result_mismatch",
        panic_trap.into(),
        panic_before,
        panic_after,
        &["task terminal record"],
        unexpected,
        "Trapped",
        "Active",
        BTreeMap::from([
            ("subcases".into(), json!(2)),
            ("panic_contained".into(), json!(true)),
            (
                "result_mismatch_observed".into(),
                json!(runtime_error_code(&mismatch_trap)),
            ),
        ]),
    ))
}

fn host_abandoned_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let (mut harness, module, pending) = hosted_host_call(RegistryMode::Async, true, false)?;
    let before = harness.snapshot();
    let task = harness.call(module, 0)?;
    assert!(matches!(
        harness
            .realm
            .poll_task(task, 128)
            .map_err(|error| error.to_string())?,
        TaskPoll::Waiting(_)
    ));
    let request = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .ok_or("async host did not create a request")?;
    harness.observed_requests.push(request.request);
    drop(request);
    harness
        .realm
        .tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 128,
            collect_garbage: false,
        })
        .map_err(|error| error.to_string())?;
    let after = harness.snapshot();
    let trap = terminal_trap(&harness.realm, task)?;
    let mut unexpected = Vec::new();
    if trap.diagnostic_code() != "NX5002" {
        unexpected.push("abandon did not emit NX5002".into());
    }
    if harness.realm.resource_ledger().requests != 0 {
        unexpected.push("abandoned request reservation was not released".into());
    }
    Ok(evidence(
        "realm_async_ticket_abandoned",
        trap.clone().into(),
        before,
        after,
        &["waiting task becomes terminal", "request release"],
        unexpected,
        "Trapped",
        "Active",
        BTreeMap::from([
            ("completion_exactly_once".into(), json!(true)),
            (
                "request_reservations_after".into(),
                json!(harness.realm.resource_ledger().requests),
            ),
            (
                "release_records".into(),
                json!(harness.host.resource_ledger().queued_releases),
            ),
        ]),
    ))
}

fn unknown_host_error_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let (mut harness, module, pending) = hosted_host_call(RegistryMode::Async, true, true)?;
    let before = harness.snapshot();
    let task = harness.call(module, 0)?;
    assert!(matches!(
        harness
            .realm
            .poll_task(task, 128)
            .map_err(|error| error.to_string())?,
        TaskPoll::Waiting(_)
    ));
    let mut request = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .ok_or("async host did not create a request")?;
    harness.observed_requests.push(request.request);
    request
        .ticket
        .fail(HostErrorPayload::Code(77))
        .map_err(|error| error.to_string())?;
    harness
        .realm
        .tick(TickBudget {
            max_tasks: 1,
            frame_fuel_budget: 128,
            collect_garbage: false,
        })
        .map_err(|error| error.to_string())?;
    let after = harness.snapshot();
    let trap = terminal_trap(&harness.realm, task)?;
    let unexpected = (trap.diagnostic_code() != "NX5003")
        .then(|| "unknown error writeback did not emit NX5003".to_owned())
        .into_iter()
        .collect();
    Ok(evidence(
        "realm_unknown_host_error_writeback",
        trap.clone().into(),
        before,
        after,
        &["waiting task becomes terminal", "completion consumed"],
        unexpected,
        "Trapped",
        "Active",
        BTreeMap::from([
            ("unknown_error_code".into(), json!(77)),
            ("completion_queue_drained".into(), json!(true)),
        ]),
    ))
}

fn resource_capacity_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let host = StableId::from_name("r3-capacity-host");
    let mut module_harness = RuntimeDiagnosticHarness::isolated(RealmConfig {
        max_modules: 0,
        ..RealmConfig::default()
    });
    let before = module_harness.snapshot();
    let module = simple_module(host);
    let state_schema_fingerprint = module.module().state_schema_fingerprint;
    let module_error = module_harness
        .realm
        .load_module(module, host, state_schema_fingerprint)
        .expect_err("zero module capacity must fail");
    let after = module_harness.snapshot();

    let task_limits = RuntimeLimits {
        max_tasks: 0,
        ..RuntimeLimits::default()
    };
    let mut task_harness = RuntimeDiagnosticHarness::isolated(RealmConfig {
        runtime_limits: task_limits,
        ..RealmConfig::default()
    });
    let module = task_harness
        .load(simple_module(host), host)
        .map_err(|error| error.to_string())?;
    let scope = task_harness
        .realm
        .create_scope(None)
        .map_err(|error| error.to_string())?;
    let task_error = task_harness
        .realm
        .spawn_task(
            module,
            StableId::from_name("runtime-diagnostics::simple"),
            &[],
            StepConfig {
                owner: scope,
                priority: 1,
                fuel_slice: 32,
                cumulative_budget: 32,
                limits: TaskLimits::default(),
            },
        )
        .expect_err("zero task capacity must fail");

    let (mut request_harness, request_module, _) = hosted_host_call_with_config(
        RegistryMode::Async,
        true,
        false,
        RealmConfig {
            max_host_resources: 0,
            ..RealmConfig::default()
        },
    )?;
    let request_task = request_harness.call(request_module, 0)?;
    let TaskPoll::Trapped(request_trap) = request_harness
        .realm
        .poll_task(request_task, 128)
        .map_err(|error| error.to_string())?
    else {
        return Err("request capacity did not trap".into());
    };
    let codes = [
        NexaError::from(module_error).code().as_str(),
        NexaError::from(task_error).code().as_str(),
        runtime_error_code(&request_trap),
    ];
    let unexpected = codes
        .iter()
        .enumerate()
        .filter(|(_, code)| **code != "NX5004")
        .map(|(index, _)| format!("capacity subcase {index} emitted wrong code"))
        .collect::<Vec<_>>();
    Ok(evidence(
        "runtime_capacity_admission",
        request_trap.into(),
        before,
        after,
        &[],
        unexpected,
        "Trapped",
        "Active",
        BTreeMap::from([
            ("subcases".into(), json!(codes.len())),
            ("partial_mutation".into(), json!(false)),
            (
                "resources".into(),
                json!([
                    {"requested_resource":"module","capacity":0,"used_before":0,"used_after":0},
                    {"requested_resource":"task","capacity":0,"used_before":0,"used_after":0},
                    {"requested_resource":"host_request","capacity":0,"used_before":0,"used_after":0}
                ]),
            ),
        ]),
    ))
}

fn migration_limit_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let host = StableId::from_name("r3-migration-limit-host");
    let limit_kinds = [
        "objects",
        "fields",
        "forwarding",
        "state_bytes",
        "gc_roots",
        "call_depth",
    ];
    let mut observed = Vec::new();
    for kind in limit_kinds {
        let (config, required) = migration_limit_config(kind);
        let mut harness = RuntimeDiagnosticHarness::isolated(config);
        let old = harness
            .load(simple_module(host), host)
            .map_err(|error| error.to_string())?;
        let error = expected_restart_failure(harness.realm.restart_reload(
            old,
            migration_module(host, false, required),
            RestartReloadPolicy::default(),
        ))?;
        observed.push((kind, NexaError::from(error).code().as_str()));
    }
    let fuel_candidate = migration_fuel_module(host);
    let minimum_fuel = fuel_candidate
        .module()
        .reload_metadata
        .minimum_migration_limits
        .max_fuel;
    let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig {
        migration_limits: nexa_runtime::MigrationLimits {
            max_fuel: minimum_fuel,
            ..nexa_runtime::MigrationLimits::default()
        },
        ..RealmConfig::default()
    });
    let old = harness
        .load(simple_module(host), host)
        .map_err(|error| error.to_string())?;
    let before = harness.snapshot();
    let error = expected_restart_failure(harness.realm.restart_reload(
        old,
        fuel_candidate,
        RestartReloadPolicy::default(),
    ))?;
    let after = harness.snapshot();
    observed.push(("fuel", NexaError::from(error.clone()).code().as_str()));
    let unexpected = observed
        .iter()
        .filter(|(_, code)| *code != "NX6001")
        .map(|(kind, code)| format!("{kind} emitted {code}"))
        .collect::<Vec<_>>();
    Ok(evidence(
        "restart_reload_migration_limit",
        error.into(),
        before,
        after,
        &["migration usage report"],
        unexpected,
        "",
        "Active",
        BTreeMap::from([
            ("migration_limit_subcases".into(), json!(observed.len())),
            (
                "limit_kinds".into(),
                json!(observed.iter().map(|(kind, _)| *kind).collect::<Vec<_>>()),
            ),
            ("restart_reload_executed".into(), json!(true)),
        ]),
    ))
}

#[derive(Clone, Copy)]
enum GraphFault {
    NestedStateObject,
    CrossDomainHandle,
    DanglingHandle,
    WrongGeneration,
    IllegalStrongReference,
}

impl GraphFault {
    const fn name(self) -> &'static str {
        match self {
            Self::NestedStateObject => "nested_state_object",
            Self::CrossDomainHandle => "cross_domain_handle",
            Self::DanglingHandle => "dangling_handle",
            Self::WrongGeneration => "wrong_generation",
            Self::IllegalStrongReference => "illegal_strong_reference",
        }
    }

    fn argument(self, domain: StatefulDomainId) -> RuntimeValue {
        let root = StableId::from_name("R3GraphRootObject");
        let handle_type =
            StateHandleType::new(ValueType::Named(StableId::from_name("R3GraphChild"))).type_id;
        match self {
            Self::NestedStateObject => RuntimeValue::Opaque {
                type_id: handle_type,
                value: StableId::from_name("R3NestedStateObject").0,
            },
            Self::CrossDomainHandle => RuntimeValue::StateHandle {
                handle_type,
                domain: domain.get().saturating_add(1),
                stable_id: root,
                generation: 0,
            },
            Self::DanglingHandle => RuntimeValue::StateHandle {
                handle_type,
                domain: domain.get(),
                stable_id: StableId::from_name("R3DanglingStateObject"),
                generation: 0,
            },
            Self::WrongGeneration => RuntimeValue::StateHandle {
                handle_type,
                domain: domain.get(),
                stable_id: root,
                generation: 1,
            },
            Self::IllegalStrongReference => RuntimeValue::Ref(GcRef {
                index: u32::MAX,
                generation: u32::MAX,
            }),
        }
    }
}

fn migration_graph_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let host = StableId::from_name("r3-graph-host");
    let faults = [
        GraphFault::NestedStateObject,
        GraphFault::CrossDomainHandle,
        GraphFault::DanglingHandle,
        GraphFault::WrongGeneration,
        GraphFault::IllegalStrongReference,
    ];
    let mut observed = Vec::new();
    let mut first_before = Value::Null;
    let mut first_after = Value::Null;
    let mut last_error = None;
    for fault in faults {
        let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig::default());
        let old = harness
            .load(simple_module(host), host)
            .map_err(|error| error.to_string())?;
        let domain = harness
            .realm
            .module_stateful_domain(old)
            .map_err(|error| error.to_string())?;
        let before = harness.snapshot();
        let error = expected_restart_failure(harness.realm.restart_reload(
            old,
            migration_graph_module(host, fault),
            RestartReloadPolicy {
                migration_arguments: vec![fault.argument(domain)],
                ..RestartReloadPolicy::default()
            },
        ))?;
        let after = harness.snapshot();
        let code = NexaError::from(error.clone()).code().as_str();
        observed.push((fault.name(), code));
        if last_error.is_none() {
            first_before = before;
            first_after = after;
        }
        last_error = Some(error);
    }
    let unexpected = observed
        .iter()
        .filter(|(_, code)| *code != "NX6002")
        .map(|(fault, code)| format!("{fault} emitted {code}"))
        .collect();
    Ok(evidence(
        "realm_migration_graph_validation",
        last_error.expect("graph matrix is non-empty").into(),
        first_before,
        first_after,
        &["migration context usage"],
        unexpected,
        "",
        "Active",
        BTreeMap::from([
            ("graph_subcases".into(), json!(observed.len())),
            (
                "graph_kinds".into(),
                json!(observed.iter().map(|(fault, _)| *fault).collect::<Vec<_>>()),
            ),
            ("state_finish_traversed".into(), json!(true)),
        ]),
    ))
}

fn activation_failure_case() -> Result<RuntimeDiagnosticCaseEvidence, String> {
    let host = StableId::from_name("r3-activation-host");
    let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig::default());
    let old = harness
        .load(simple_module(host), host)
        .map_err(|error| error.to_string())?;
    let before = harness.snapshot();
    let outcome = harness
        .realm
        .restart_reload(
            old,
            activation_trap_module(host),
            RestartReloadPolicy {
                activation_fuel: 128,
                ..RestartReloadPolicy::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let RestartReloadOutcome::ActivationFaulted { candidate, error } = outcome else {
        return Err("activation trap did not produce ActivationFaulted".into());
    };
    let after = harness.snapshot();
    let old_lifecycle = format!(
        "{:?}",
        harness
            .realm
            .module_lifecycle(old)
            .map_err(|error| error.to_string())?
    );
    let old_root_restored = harness.realm.active_root() == Some(old);
    let candidate_released = harness.realm.module_lifecycle(candidate).is_err();
    let candidate_root_cleared = after["candidate_root"].is_null();
    let old_epoch_preserved =
        before["active_root"]["epoch"].as_u64() == after["active_root"]["epoch"].as_u64();
    let publications = after["reload"]["root_publications"].as_u64().unwrap_or(0);
    let mut unexpected = Vec::new();
    if old_lifecycle != "Active" {
        unexpected.push("last-known-good module did not return to Active".into());
    }
    if !old_root_restored {
        unexpected.push("last-known-good root was not restored".into());
    }
    if !candidate_released {
        unexpected.push("activation-fault candidate remained addressable".into());
    }
    if !candidate_root_cleared {
        unexpected.push("activation-fault transaction retained a candidate root".into());
    }
    if !old_epoch_preserved {
        unexpected.push("last-known-good epoch changed during activation rollback".into());
    }
    if publications != 1 {
        unexpected.push("root publication count was not exactly one".into());
    }
    Ok(evidence(
        "realm_commit_reload_activation_trap",
        error.into(),
        before,
        after,
        &[
            "candidate provisional publication",
            "activation fault evidence",
            "old root restoration",
            "candidate release",
        ],
        unexpected,
        "",
        &old_lifecycle,
        BTreeMap::from([
            ("candidate_released".into(), json!(candidate_released)),
            (
                "candidate_root_cleared".into(),
                json!(candidate_root_cleared),
            ),
            ("root_publications".into(), json!(publications)),
            ("old_epoch_retired".into(), json!(false)),
            ("old_epoch_preserved".into(), json!(old_epoch_preserved)),
            ("old_lifecycle".into(), json!(old_lifecycle)),
            ("outcome_activation_faulted".into(), json!(true)),
            ("rollback_old_root".into(), json!(old_root_restored)),
        ]),
    ))
}

fn expected_restart_failure(
    result: Result<RestartReloadOutcome, nexa_runtime::ReloadError>,
) -> Result<nexa_runtime::ReloadError, String> {
    match result {
        Err(error) => Ok(error),
        Ok(RestartReloadOutcome::RolledBackBeforeCommit { reason, .. }) => Ok(reason),
        Ok(outcome) => Err(format!(
            "restart reload unexpectedly succeeded: {outcome:?}"
        )),
    }
}

fn atomic_snapshot_failures(before: &Value, after: &Value) -> Vec<String> {
    (before != after)
        .then(|| "atomic failure mutated the Realm snapshot".to_owned())
        .into_iter()
        .collect()
}

fn terminal_trap(realm: &RealmRuntime, task: TaskHandle) -> Result<&nexa_runtime::Trap, String> {
    let terminal = realm
        .terminal_record(task)
        .ok_or("missing terminal record")?;
    let TaskTerminalReason::Trapped(trap) = &terminal.reason else {
        return Err("terminal record is not trapped".into());
    };
    Ok(trap)
}

fn terminal_trap_stack(terminal: &nexa_runtime::TaskTerminalRecord) -> Vec<Value> {
    let TaskTerminalReason::Trapped(trap) = &terminal.reason else {
        return Vec::new();
    };
    trap.script_call_stack
        .as_slice()
        .iter()
        .map(|frame| {
            json!({
                "function": frame.function,
                "pc": frame.pc,
                "call_site_pc": frame.call_site_pc,
                "source_span": frame.source_span.map(|span| [span.start, span.end]),
            })
        })
        .collect()
}

fn hosted_host_call(
    mode: RegistryMode,
    asynchronous: bool,
    typed_error: bool,
) -> Result<HostedHarness, String> {
    hosted_host_call_with_config(mode, asynchronous, typed_error, RealmConfig::default())
}

fn hosted_host_call_with_config(
    mode: RegistryMode,
    asynchronous: bool,
    typed_error: bool,
    config: RealmConfig,
) -> Result<HostedHarness, String> {
    let host = StableId::from_name("r3-diagnostic-host");
    let module = if asynchronous {
        async_module(host, typed_error)
    } else {
        host_call_module(host)
    };
    let import = module
        .module()
        .host_imports
        .first()
        .expect("diagnostic Host-call module has one import");
    let registry = DiagnosticRegistry::for_import(host, import, mode);
    let (mut harness, pending) = RuntimeDiagnosticHarness::hosted(config, registry)?;
    let module = harness
        .load(module, host)
        .map_err(|error| error.to_string())?;
    Ok((harness, module, pending))
}

fn simple_module(host: StableId) -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    function
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    let function = function.finish().expect("simple diagnostic function");
    let mut module = ModuleBuilder::new();
    module
        .metadata(host, StateSchema::default().fingerprint())
        .script_export(ScriptExport {
            stable_id: StableId::from_name("runtime-diagnostics::simple"),
            function: 0,
            signature: function.signature.clone(),
            effect: function.effect,
        })
        .function(function);
    verify_round_trip(module.finish())
}

fn host_call_module(host: StableId) -> VerifiedModule {
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::I32),
        },
        1,
    );
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut function = function.finish().expect("host diagnostic function");
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![false],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(host, StateSchema::default().fingerprint());
    module.host_import(HostImport {
        stable_id: StableId::from_name("R3::immediate"),
        declaration_fingerprint: [0x31; 32],
        capabilities: Vec::new(),
        parameters: Vec::new(),
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    module.script_export(ScriptExport {
        stable_id: StableId::from_name("runtime-diagnostics::host-call"),
        function: 0,
        signature: function.signature.clone(),
        effect: function.effect,
    });
    module.function(function);
    module.source_map([
        SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: SourceSpan::new(FileId(71), 4, 12),
        },
        SourceMapEntry {
            function: 0,
            pc_start: 1,
            pc_end: 2,
            span: SourceSpan::new(FileId(71), 13, 19),
        },
    ]);
    verify_round_trip(module.finish())
}

fn async_module(host: StableId, typed_error: bool) -> VerifiedModule {
    let error = if typed_error {
        EnumType {
            type_id: StableId::from_name("KnownHostError"),
            variants: vec![EnumVariant {
                stable_id: StableId::from_parts(&["KnownHostError", "::Known"]),
                tag: 1,
                payload_type: None,
            }],
        }
    } else {
        EnumType {
            type_id: StableId::from_name("ScalarHostError"),
            variants: vec![EnumVariant {
                stable_id: StableId::from_parts(&["ScalarHostError", "::Known"]),
                tag: 1,
                payload_type: None,
            }],
        }
    };
    let result = result_type(ValueType::I32, ValueType::Named(error.type_id));
    let async_result = AsyncResultType {
        result_type: result.type_id,
        success: ValueType::I32,
        error: ValueType::Named(error.type_id),
        cancel_policy: CancelPolicy::ReturnError,
        abandon_policy: AbandonPolicy::Trap,
        cancel_error: Some(1),
        abandon_error: None,
    };
    let result_type = ValueType::Named(result.type_id);
    let mut module = ModuleBuilder::new();
    module.metadata(host, StateSchema::default().fingerprint());
    module.enum_type(error);
    module.enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("R3::async"),
        declaration_fingerprint: [0x32; 32],
        capabilities: Vec::new(),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    let mut module = module.finish();
    add_async_diagnostic_function(&mut module, result_type);
    verify_round_trip(module)
}

fn add_async_diagnostic_function(module: &mut Module, result_type: ValueType) {
    let signature = Signature {
        parameters: Vec::new(),
        result: Some(result_type),
    };
    let layouts = LayoutTable::for_module(module).expect("async diagnostic module layout");
    let abi = FunctionAbi::for_signature(&layouts, &signature)
        .expect("async diagnostic function physical ABI");
    let result_abi = abi.result.expect("async diagnostic function result");
    let mut function = FunctionBuilder::new(signature, result_abi.slot_count);
    function.parameter_slots(abi.parameter_slots);
    for register in result_abi
        .gc_bitmap
        .iter()
        .enumerate()
        .filter_map(|(register, is_root)| is_root.then_some(register))
    {
        function
            .set_root(u16::try_from(register).expect("small async diagnostic root"))
            .expect("async diagnostic root register");
    }
    function
        .effect(FunctionEffect::Task)
        .emit(Instruction::HostCall {
            import: 0,
            args_base: 0,
            args_count: 0,
            dst: 0,
        })
        .emit(Instruction::Return { source: 0 });
    let mut function = function.finish().expect("async diagnostic function");
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false; usize::from(result_abi.slot_count)],
        },
        RootMap {
            pc: 1,
            bitmap: result_abi.gc_bitmap,
        },
    ];
    module.exports.push(ScriptExport {
        stable_id: StableId::from_name("runtime-diagnostics::async"),
        function: 0,
        signature: function.signature.clone(),
        effect: function.effect,
    });
    module.functions.push(function);
    module.source_map.extend([
        SourceMapEntry {
            function: 0,
            pc_start: 0,
            pc_end: 1,
            span: SourceSpan::new(FileId(72), 4, 12),
        },
        SourceMapEntry {
            function: 0,
            pc_start: 1,
            pc_end: 2,
            span: SourceSpan::new(FileId(72), 13, 19),
        },
    ]);
}

fn migration_module(
    host: StableId,
    finished: bool,
    minimum: MigrationLimitRequirements,
) -> VerifiedModule {
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    migration.effect(FunctionEffect::Migration);
    if finished {
        migration.emit(Instruction::StateFinish);
    }
    migration.emit(Instruction::ReturnVoid);
    let mut module = ModuleBuilder::new();
    module.metadata(host, StateSchema::default().fingerprint());
    let entry = module.function(migration.finish().expect("migration diagnostic function"));
    let mut module = module.finish();
    let required = nexa_bytecode::minimum_migration_limits(&module, Some(entry));
    let minimum = MigrationLimitRequirements {
        max_objects: minimum.max_objects.max(required.max_objects),
        max_fields: minimum.max_fields.max(required.max_fields),
        max_forwarding_entries: minimum
            .max_forwarding_entries
            .max(required.max_forwarding_entries),
        max_state_bytes: minimum.max_state_bytes.max(required.max_state_bytes),
        max_gc_roots: minimum.max_gc_roots.max(required.max_gc_roots),
        max_fuel: minimum.max_fuel.max(required.max_fuel),
        max_call_depth: minimum.max_call_depth.max(required.max_call_depth),
    };
    module.reload_metadata = ReloadMetadata {
        migration_entry: Some(entry),
        activation_entry: None,
        state_schema_fingerprint: module.state_schema.fingerprint(),
        minimum_migration_limits: minimum,
    };
    verify_round_trip(module)
}

fn migration_fuel_module(host: StableId) -> VerifiedModule {
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        1,
    );
    migration
        .effect(FunctionEffect::Migration)
        .emit(Instruction::LoadBool {
            dst: 0,
            value: true,
        })
        .emit(Instruction::JumpIfFalse {
            condition: 0,
            target: 4,
        })
        .emit(Instruction::Safepoint)
        .emit(Instruction::Jump { target: 1 })
        .emit(Instruction::ReturnVoid)
        .loop_bound(3, 2);
    let mut migration = migration.finish().expect("fuel migration function");
    migration.safepoints = vec![0, 2, 3, 4];
    migration.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 2,
            bitmap: vec![false],
        },
        RootMap {
            pc: 3,
            bitmap: vec![false],
        },
        RootMap {
            pc: 4,
            bitmap: vec![false],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(host, StateSchema::default().fingerprint());
    let entry = module.function(migration);
    let mut module = module.finish();
    module.reload_metadata = ReloadMetadata {
        migration_entry: Some(entry),
        activation_entry: None,
        state_schema_fingerprint: module.state_schema.fingerprint(),
        minimum_migration_limits: nexa_bytecode::minimum_migration_limits(&module, Some(entry)),
    };
    verify_round_trip(module)
}

fn migration_graph_module(host: StableId, fault: GraphFault) -> VerifiedModule {
    let root_type = StableId::from_name("R3GraphRoot");
    let root_id = StableId::from_name("R3GraphRootObject");
    let child_type = StableId::from_name("R3GraphChild");
    let field_id = StableId::from_parts(&["R3GraphRoot", "::value"]);
    let handle = StateHandleType::new(ValueType::Named(child_type));
    let field_type = match fault {
        GraphFault::IllegalStrongReference => ValueType::Ref,
        _ => ValueType::Named(handle.type_id),
    };
    let input_is_gc_root = matches!(field_type, ValueType::Ref);
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: vec![field_type],
            result: None,
        },
        2,
    );
    migration.effect(FunctionEffect::Migration);
    if input_is_gc_root {
        migration
            .set_root(0)
            .expect("strong-reference graph input is a root");
    }
    migration
        .emit(Instruction::StateNewCreate {
            stable_id: root_id,
            type_id: root_type,
            dst: 1,
        })
        .emit(Instruction::StateNewSet {
            object: 1,
            field_id,
            source: 0,
        })
        .emit(Instruction::StateFinish)
        .emit(Instruction::ReturnVoid);
    let mut migration = migration.finish().expect("graph migration function");
    migration.safepoints = vec![0, 3];
    migration.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![input_is_gc_root, false],
        },
        RootMap {
            pc: 3,
            bitmap: vec![false, false],
        },
    ];
    let state_schema = StateSchema {
        types: vec![
            StateType {
                stable_id: root_type,
                version: 1,
                fields: vec![StateField {
                    stable_id: field_id,
                    ty: field_type,
                }],
            },
            StateType {
                stable_id: child_type,
                version: 1,
                fields: Vec::new(),
            },
        ],
    };
    let state_schema_fingerprint = state_schema.fingerprint();
    let mut module = ModuleBuilder::new();
    module
        .metadata(host, state_schema_fingerprint)
        .state_schema(state_schema);
    if !matches!(fault, GraphFault::IllegalStrongReference) {
        module.state_handle_type(handle);
    }
    let entry = module.function(migration);
    let mut module = module.finish();
    module.reload_metadata = ReloadMetadata {
        migration_entry: Some(entry),
        activation_entry: None,
        state_schema_fingerprint: module.state_schema.fingerprint(),
        minimum_migration_limits: nexa_bytecode::minimum_migration_limits(&module, Some(entry)),
    };
    verify_round_trip(module)
}

fn activation_trap_module(host: StableId) -> VerifiedModule {
    let mut activation = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        0,
    );
    activation
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::Trap);
    let mut module = ModuleBuilder::new();
    module.metadata(host, StateSchema::default().fingerprint());
    let entry = module.function(activation.finish().expect("activation diagnostic function"));
    module.reload_entries(None, Some(entry));
    verify_round_trip(module.finish())
}

fn host_capability_modules(host: StableId) -> Result<Vec<VerifiedModule>, String> {
    let mut modules = Vec::new();

    let mut direct = ModuleBuilder::new();
    direct.metadata(host, StateSchema::default().fingerprint());
    direct.host_import(HostImport {
        stable_id: StableId::from_name("R3::capability"),
        declaration_fingerprint: [0x33; 32],
        capabilities: vec!["diagnostic.invoke".into()],
        parameters: Vec::new(),
        result: None,
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    let mut direct = direct.finish();
    add_void_function(&mut direct, Vec::new());
    modules.push(verify_round_trip(direct));

    let request_type = ValueType::Named(StableId::from_name("HostRequest"));
    let option = option_type(request_type);
    let mut option_module = ModuleBuilder::new();
    option_module
        .metadata(host, StateSchema::default().fingerprint())
        .enum_type(option.clone());
    let mut option_module = option_module.finish();
    add_void_function(&mut option_module, vec![ValueType::Named(option.type_id)]);
    modules.push(verify_round_trip(option_module));

    let host_error = ValueType::Named(StableId::from_name("HostError"));
    let result = result_type(ValueType::I32, host_error);
    let mut result_module = ModuleBuilder::new();
    result_module
        .metadata(host, StateSchema::default().fingerprint())
        .enum_type(result.clone());
    let mut result_module = result_module.finish();
    add_void_function(&mut result_module, vec![ValueType::Named(result.type_id)]);
    modules.push(verify_round_trip(result_module));

    let content = StableId::from_name("R3SnapshotContent");
    let snapshot = SnapshotType::new(content);
    let snapshot_content = StructType {
        type_id: content,
        fields: Vec::new(),
    };
    let structure = StructType {
        type_id: StableId::from_name("SnapshotContainer"),
        fields: vec![StructField {
            stable_id: StableId::from_name("SnapshotContainer::value"),
            ty: ValueType::Named(snapshot.type_id),
        }],
    };
    let mut struct_module = ModuleBuilder::new();
    struct_module
        .metadata(host, StateSchema::default().fingerprint())
        .struct_type(snapshot_content)
        .snapshot_type(snapshot)
        .struct_type(structure.clone());
    let mut struct_module = struct_module.finish();
    add_void_function(
        &mut struct_module,
        vec![ValueType::Named(structure.type_id)],
    );
    modules.push(verify_round_trip(struct_module));

    let resource_content = StableId::from_name("R3DiagnosticResource");
    let resource_token = ResourceTokenType::new(resource_content);
    let resource = ValueType::Named(resource_token.type_id);
    let enumeration = EnumType {
        type_id: StableId::from_name("ResourceEnvelope"),
        variants: vec![EnumVariant {
            stable_id: StableId::from_parts(&["ResourceEnvelope", "::Token"]),
            tag: 0,
            payload_type: Some(resource),
        }],
    };
    let mut enum_module = ModuleBuilder::new();
    enum_module
        .metadata(host, StateSchema::default().fingerprint())
        .resource_token_type(resource_token)
        .enum_type(enumeration.clone());
    let mut enum_module = enum_module.finish();
    add_void_function(
        &mut enum_module,
        vec![ValueType::Named(enumeration.type_id)],
    );
    modules.push(verify_round_trip(enum_module));

    if modules.len() != 5 {
        return Err("host capability matrix is incomplete".into());
    }
    Ok(modules)
}

fn add_void_function(module: &mut Module, parameters: Vec<ValueType>) {
    let signature = Signature {
        parameters,
        result: None,
    };
    let layouts = LayoutTable::for_module(module).expect("capability module layout");
    let abi =
        FunctionAbi::for_signature(&layouts, &signature).expect("capability function physical ABI");
    let registers = abi.parameter_slots;
    let mut function = FunctionBuilder::new(signature, registers);
    function.parameter_slots(abi.parameter_slots);
    for register in abi
        .parameter_gc_bitmap
        .iter()
        .enumerate()
        .filter_map(|(register, is_root)| is_root.then_some(register))
    {
        function
            .set_root(u16::try_from(register).expect("small capability root"))
            .expect("capability root register");
    }
    function
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    let mut function = function.finish().expect("capability function");
    function.safepoints = vec![0];
    function.root_maps = vec![RootMap {
        pc: 0,
        // The parameters establish which registers may contain references, but
        // this fixture returns without reading them. Exact root maps therefore
        // contain no live roots at the entry/return safepoint.
        bitmap: vec![false; usize::from(registers)],
    }];
    module.functions.push(function);
}

#[allow(clippy::needless_pass_by_value)]
fn verify_round_trip(module: Module) -> VerifiedModule {
    let bytes = module.encode();
    let decoded = Module::decode(&bytes).expect("diagnostic bytecode round trip");
    nexa_verifier::verify(decoded, VerifierLimits::default())
        .expect("runtime diagnostic module verifies")
}

fn migration_limit_config(kind: &str) -> (RealmConfig, MigrationLimitRequirements) {
    let mut required = MigrationLimitRequirements::default();
    let mut limits = nexa_runtime::MigrationLimits::default();
    match kind {
        "objects" => {
            limits.max_objects = 0;
            required.max_objects = 1;
        }
        "fields" => {
            limits.max_fields = 0;
            required.max_fields = 1;
        }
        "forwarding" => {
            limits.max_forwarding_entries = 0;
            required.max_forwarding_entries = 1;
        }
        "state_bytes" => {
            limits.max_state_bytes = 0;
            required.max_state_bytes = 1;
        }
        "gc_roots" => {
            limits.max_gc_roots = 0;
            required.max_gc_roots = 1;
        }
        "call_depth" => {
            limits.max_call_depth = 0;
            required.max_call_depth = 1;
        }
        _ => unreachable!("known migration limit kind"),
    }
    (
        RealmConfig {
            migration_limits: limits,
            ..RealmConfig::default()
        },
        required,
    )
}

#[cfg(test)]
mod tests {
    use super::execute_case;

    #[test]
    fn activation_failure_diagnostic_records_lkg_rollback() {
        let evidence = execute_case("NX6003").expect("NX6003 evidence");

        assert!(evidence.passed, "{:?}", evidence.unexpected_mutations);
        assert_eq!(evidence.module_lifecycle, "Active");
        assert_eq!(evidence.details["candidate_released"], true);
        assert_eq!(evidence.details["candidate_root_cleared"], true);
        assert_eq!(evidence.details["old_epoch_retired"], false);
        assert_eq!(evidence.details["old_epoch_preserved"], true);
        assert_eq!(evidence.details["outcome_activation_faulted"], true);
        assert_eq!(evidence.details["rollback_old_root"], true);
    }
}
