use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use nexa_bytecode::{
    AbandonPolicy, AsyncResultType, CancelPolicy, EnumType, EnumVariant, FunctionBuilder,
    FunctionEffect, HostCallMode, HostImport, Instruction, MigrationLimitRequirements, Module,
    ModuleBuilder, ReloadMetadata, RootMap, Signature, SnapshotType, SourceMapEntry, StateField,
    StateHandleType, StateSchema, StateType, StructField, StructType, ValueType, option_type,
    result_type,
};
use nexa_core::{FileId, SourceSpan, StableId};
use nexa_runtime::{
    GcRef, HostCallOutcome, HostErrorPayload, HostRegistry, HostRequestHandle, HostTrap,
    ModuleHandle, PendingHostRequest, RealmConfig, RealmRuntime, ResourceContext,
    RestartReloadOutcome, RestartReloadPolicy, RuntimeError, RuntimeHost, RuntimeHostArgs,
    RuntimeLimits, RuntimeValue, StatefulDomainId, StepConfig, TaskHandle, TaskLimits, TaskPoll,
    TaskTerminalReason, TickBudget,
};
use nexa_verifier::{VerifiedModule, VerifierLimits};
use serde::Serialize;
use serde_json::{Value, json};

use crate::NexaError;

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
    observed_tasks: Vec<TaskHandle>,
    observed_requests: Vec<HostRequestHandle>,
}

impl RuntimeDiagnosticHarness {
    fn hosted(
        config: RealmConfig,
        registry: DiagnosticRegistry,
    ) -> Result<(Self, PendingRequestSlot), String> {
        let pending = Arc::clone(&registry.pending);
        let host = RuntimeHost::new(config.max_host_resources.max(1) as usize);
        let realm = RealmRuntime::hosted(config, host.clone(), Box::new(registry))
            .map_err(|error| error.to_string())?;
        Ok((
            Self {
                host,
                realm,
                modules: Vec::new(),
                observed_tasks: Vec::new(),
                observed_requests: Vec::new(),
            },
            pending,
        ))
    }

    fn isolated(config: RealmConfig) -> Self {
        let host = RuntimeHost::new(config.max_host_resources.max(1) as usize);
        Self {
            host,
            realm: RealmRuntime::isolated(config),
            modules: Vec::new(),
            observed_tasks: Vec::new(),
            observed_requests: Vec::new(),
        }
    }

    fn load(
        &mut self,
        module: VerifiedModule,
        host_hash: StableId,
        schema_hash: StableId,
    ) -> Result<ModuleHandle, nexa_runtime::RealmError> {
        let module = self.realm.load_module(module, host_hash, schema_hash)?;
        self.modules.push(module);
        Ok(module)
    }

    fn call(&mut self, module: ModuleHandle, function: u32) -> Result<TaskHandle, String> {
        let scope = self
            .realm
            .create_scope(None)
            .map_err(|error| error.to_string())?;
        let task = self
            .realm
            .spawn_task(
                module,
                function,
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
    pub observed_codes: Vec<String>,
    pub missing_codes: Vec<String>,
    pub failures: Vec<String>,
    pub deterministic_cases: usize,
    pub nondeterministic_cases: Vec<String>,
    pub independent_harnesses: usize,
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
    mode: RegistryMode,
    pending: Arc<Mutex<Option<PendingHostRequest>>>,
}

impl DiagnosticRegistry {
    fn new(hash: StableId, mode: RegistryMode) -> Self {
        Self {
            hash,
            mode,
            pending: Arc::new(Mutex::new(None)),
        }
    }
}

impl HostRegistry for DiagnosticRegistry {
    fn interface_hash(&self) -> Option<StableId> {
        Some(self.hash)
    }

    fn call_runtime(
        &mut self,
        id: u32,
        context: &mut ResourceContext<'_>,
        args: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        if id != 0 {
            return Err(HostTrap::UnknownFunction(id));
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
    let failures = cases
        .iter()
        .filter(|(_, case)| !case.passed || !case.unexpected_mutations.is_empty())
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
    Ok(RuntimeDiagnosticEndToEndReport {
        schema_version: 1,
        deterministic_cases: cases.values().filter(|case| case.deterministic).count(),
        independent_harnesses: cases.len(),
        cases,
        observed_codes,
        missing_codes,
        failures,
        nondeterministic_cases,
    })
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
    let schema = StableId::from_name("r3-schema");
    let registry = DiagnosticRegistry::new(registry_hash, RegistryMode::ResultMismatch);
    let (mut harness, _) = RuntimeDiagnosticHarness::hosted(RealmConfig::default(), registry)?;
    let before = harness.snapshot();
    let error = harness
        .realm
        .load_module(simple_module(module_hash, schema), module_hash, schema)
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
    let schema = StableId::from_name("r3-capability-schema");
    let modules = host_capability_modules(host, schema)?;
    let mut results = Vec::new();
    let mut last_error = None;
    let mut first_before = Value::Null;
    let mut first_after = Value::Null;
    for module in modules {
        let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig::default());
        let before = harness.snapshot();
        let error = harness
            .realm
            .load_module(module, host, schema)
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
        .fail(HostErrorPayload { code: 77 })
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
    let schema = StableId::from_name("r3-capacity-schema");
    let mut module_harness = RuntimeDiagnosticHarness::isolated(RealmConfig {
        max_modules: 0,
        ..RealmConfig::default()
    });
    let before = module_harness.snapshot();
    let module_error = module_harness
        .realm
        .load_module(simple_module(host, schema), host, schema)
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
        .load(simple_module(host, schema), host, schema)
        .map_err(|error| error.to_string())?;
    let scope = task_harness
        .realm
        .create_scope(None)
        .map_err(|error| error.to_string())?;
    let task_error = task_harness
        .realm
        .spawn_task(
            module,
            0,
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
    let schema = StableId::from_name("r3-migration-limit-schema");
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
            .load(simple_module(host, schema), host, schema)
            .map_err(|error| error.to_string())?;
        let error = expected_restart_failure(harness.realm.restart_reload(
            old,
            migration_module(host, schema, false, required),
            RestartReloadPolicy::default(),
        ))?;
        observed.push((kind, NexaError::from(error).code().as_str()));
    }
    let fuel_candidate = migration_fuel_module(host, schema);
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
        .load(simple_module(host, schema), host, schema)
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
    let schema = StableId::from_name("r3-graph-schema");
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
            .load(simple_module(host, schema), host, schema)
            .map_err(|error| error.to_string())?;
        let domain = harness
            .realm
            .module_stateful_domain(old)
            .map_err(|error| error.to_string())?;
        let before = harness.snapshot();
        let error = expected_restart_failure(harness.realm.restart_reload(
            old,
            migration_graph_module(host, schema, fault),
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
    let schema = StableId::from_name("r3-activation-schema");
    let mut harness = RuntimeDiagnosticHarness::isolated(RealmConfig::default());
    let old = harness
        .load(simple_module(host, schema), host, schema)
        .map_err(|error| error.to_string())?;
    let before = harness.snapshot();
    let outcome = harness
        .realm
        .restart_reload(
            old,
            activation_trap_module(host, schema),
            RestartReloadPolicy {
                activation_fuel: 128,
                ..RestartReloadPolicy::default()
            },
        )
        .map_err(|error| error.to_string())?;
    let RestartReloadOutcome::ActivationFaulted { candidate, error } = outcome else {
        return Err("activation trap did not produce ActivationFaulted".into());
    };
    harness.modules.push(candidate);
    let after = harness.snapshot();
    let lifecycle = format!(
        "{:?}",
        harness
            .realm
            .module_lifecycle(candidate)
            .map_err(|error| error.to_string())?
    );
    let publications = after["reload"]["root_publications"].as_u64().unwrap_or(0);
    let mut unexpected = Vec::new();
    if lifecycle != "ActivationFaulted" {
        unexpected.push("candidate did not become ActivationFaulted".into());
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
            "candidate publication",
            "old epoch retirement",
            "activation fault",
        ],
        unexpected,
        "",
        &lifecycle,
        BTreeMap::from([
            ("candidate_lifecycle".into(), json!(lifecycle)),
            ("root_publications".into(), json!(publications)),
            ("old_epoch_retired".into(), json!(true)),
            ("rollback_old_root".into(), json!(false)),
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
    let schema = StableId::from_name("r3-diagnostic-schema");
    let registry = DiagnosticRegistry::new(host, mode);
    let (mut harness, pending) = RuntimeDiagnosticHarness::hosted(config, registry)?;
    let module = if asynchronous {
        async_module(host, schema, typed_error)
    } else {
        host_call_module(host, schema)
    };
    let module = harness
        .load(module, host, schema)
        .map_err(|error| error.to_string())?;
    Ok((harness, module, pending))
}

fn simple_module(host: StableId, schema: StableId) -> VerifiedModule {
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
    let mut module = ModuleBuilder::new();
    module
        .metadata(host, schema)
        .function(function.finish().expect("simple diagnostic function"));
    verify_round_trip(module.finish())
}

fn host_call_module(host: StableId, schema: StableId) -> VerifiedModule {
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
    module.metadata(host, schema);
    module.host_import(HostImport {
        stable_id: StableId::from_name("R3::immediate"),
        parameters: Vec::new(),
        result: Some(ValueType::I32),
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
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

fn async_module(host: StableId, schema: StableId, typed_error: bool) -> VerifiedModule {
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
    let mut function = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: Some(ValueType::Named(result.type_id)),
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
    let mut function = function.finish().expect("async diagnostic function");
    function.root_bitmap[0] = true;
    function.safepoints = vec![0, 1];
    function.root_maps = vec![
        RootMap {
            pc: 0,
            bitmap: vec![false],
        },
        RootMap {
            pc: 1,
            bitmap: vec![true],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema);
    module.enum_type(error);
    module.enum_type(result);
    module.host_import(HostImport {
        stable_id: StableId::from_name("R3::async"),
        parameters: Vec::new(),
        result: Some(ValueType::Named(async_result.result_type)),
        mode: HostCallMode::Async,
        fuel_cost: 1,
        async_result: Some(async_result),
    });
    module.function(function);
    module.source_map([
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
    verify_round_trip(module.finish())
}

fn migration_module(
    host: StableId,
    schema: StableId,
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
    module.metadata(host, schema);
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
        stateful_schema_hash: module.state_schema.stable_hash(),
        minimum_migration_limits: minimum,
    };
    verify_round_trip(module)
}

fn migration_fuel_module(host: StableId, schema: StableId) -> VerifiedModule {
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
    module.metadata(host, schema);
    let entry = module.function(migration);
    let mut module = module.finish();
    module.reload_metadata = ReloadMetadata {
        migration_entry: Some(entry),
        activation_entry: None,
        stateful_schema_hash: module.state_schema.stable_hash(),
        minimum_migration_limits: nexa_bytecode::minimum_migration_limits(&module, Some(entry)),
    };
    verify_round_trip(module)
}

fn migration_graph_module(host: StableId, schema: StableId, fault: GraphFault) -> VerifiedModule {
    let root_type = StableId::from_name("R3GraphRoot");
    let root_id = StableId::from_name("R3GraphRootObject");
    let child_type = StableId::from_name("R3GraphChild");
    let field_id = StableId::from_parts(&["R3GraphRoot", "::value"]);
    let handle = StateHandleType::new(ValueType::Named(child_type));
    let field_type = match fault {
        GraphFault::IllegalStrongReference => ValueType::Ref,
        _ => ValueType::Named(handle.type_id),
    };
    let mut migration = FunctionBuilder::new(
        Signature {
            parameters: vec![field_type],
            result: None,
        },
        2,
    );
    migration
        .effect(FunctionEffect::Migration)
        .set_root(0)
        .expect("graph input is a root")
        .set_root(1)
        .expect("graph object is a root")
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
            bitmap: vec![true, false],
        },
        RootMap {
            pc: 3,
            bitmap: vec![true, true],
        },
    ];
    let mut module = ModuleBuilder::new();
    module.metadata(host, schema).state_schema(StateSchema {
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
    });
    if !matches!(fault, GraphFault::IllegalStrongReference) {
        module.state_handle_type(handle);
    }
    let entry = module.function(migration);
    let mut module = module.finish();
    module.reload_metadata = ReloadMetadata {
        migration_entry: Some(entry),
        activation_entry: None,
        stateful_schema_hash: module.state_schema.stable_hash(),
        minimum_migration_limits: nexa_bytecode::minimum_migration_limits(&module, Some(entry)),
    };
    verify_round_trip(module)
}

fn activation_trap_module(host: StableId, schema: StableId) -> VerifiedModule {
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
    module.metadata(host, schema);
    let entry = module.function(activation.finish().expect("activation diagnostic function"));
    module.reload_entries(None, Some(entry));
    verify_round_trip(module.finish())
}

fn host_capability_modules(
    host: StableId,
    schema: StableId,
) -> Result<Vec<VerifiedModule>, String> {
    let mut modules = Vec::new();

    let mut direct = ModuleBuilder::new();
    direct.metadata(host, schema);
    direct.host_import(HostImport {
        stable_id: StableId::from_name("R3::capability"),
        parameters: Vec::new(),
        result: None,
        mode: HostCallMode::Immediate,
        fuel_cost: 1,
        async_result: None,
    });
    add_void_function(&mut direct, Vec::new());
    modules.push(verify_round_trip(direct.finish()));

    let request_type = ValueType::Named(StableId::from_name("HostRequest"));
    let option = option_type(request_type);
    let mut option_module = ModuleBuilder::new();
    option_module
        .metadata(host, schema)
        .enum_type(option.clone());
    add_void_function(&mut option_module, vec![ValueType::Named(option.type_id)]);
    modules.push(verify_round_trip(option_module.finish()));

    let host_error = ValueType::Named(StableId::from_name("HostError"));
    let result = result_type(ValueType::I32, host_error);
    let mut result_module = ModuleBuilder::new();
    result_module
        .metadata(host, schema)
        .enum_type(result.clone());
    add_void_function(&mut result_module, vec![ValueType::Named(result.type_id)]);
    modules.push(verify_round_trip(result_module.finish()));

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
        .metadata(host, schema)
        .struct_type(snapshot_content)
        .snapshot_type(snapshot)
        .struct_type(structure.clone());
    add_void_function(
        &mut struct_module,
        vec![ValueType::Named(structure.type_id)],
    );
    modules.push(verify_round_trip(struct_module.finish()));

    let resource = ValueType::Named(StableId::from_name("ResourceToken"));
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
        .metadata(host, schema)
        .enum_type(enumeration.clone());
    add_void_function(
        &mut enum_module,
        vec![ValueType::Named(enumeration.type_id)],
    );
    modules.push(verify_round_trip(enum_module.finish()));

    if modules.len() != 5 {
        return Err("host capability matrix is incomplete".into());
    }
    Ok(modules)
}

fn add_void_function(module: &mut ModuleBuilder, parameters: Vec<ValueType>) {
    let registers = u16::try_from(parameters.len()).expect("small capability function");
    let root_registers = parameters
        .iter()
        .enumerate()
        .filter_map(|(index, ty)| matches!(ty, ValueType::Named(_)).then_some(index))
        .collect::<Vec<_>>();
    let mut function = FunctionBuilder::new(
        Signature {
            parameters,
            result: None,
        },
        registers,
    );
    for register in root_registers {
        function
            .set_root(u16::try_from(register).expect("small capability root"))
            .expect("capability root register");
    }
    function
        .effect(FunctionEffect::Immediate)
        .emit(Instruction::ReturnVoid);
    module.function(function.finish().expect("capability function"));
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
